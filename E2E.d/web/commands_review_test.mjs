// Web composer command-button + code-review panel E2E lane (playwright half
// of E2E.d/web/commands_review.sh).
//
// Environment:
//   RPI_URL          http://127.0.0.1:<port>/web
//   RPI_TOKEN        token file content (served via rpi-auth.<token> subprotocol)
//   RPI_CHROME       executable path of the system Chrome (optional)
//   RPI_STAGING      the run-scoped staging dir handed over by the shell
//   RPI_LANE_NONCE   this invocation's nonce (must match the shell's)
//   RPI_DIRTY_FILE   tracked file modified in the working tree ("greet.txt")
//   RPI_ADDED_FILE   staged new tracked file ("added.txt")
//   RPI_BIG_FILE     oversize (>4000 diff lines) file with a COMPLETE body
//                    (well under every backend cap; the panel eager-pages it)
//   RPI_NESTED_FILE  nested dirty file ("nested/deep.txt")
//   RPI_DELETED_FILE committed-then-deleted virtual path ("gone.txt")
//   RPI_RENAMED_FILE nested renamed path ("nested/renamed.txt")
//   RPI_RENAMED_OLD  rename old path ("nested/old.txt")
//   RPI_MARKDOWN_FILE long markdown paging fixture ("notes.md")
//   RPI_SELECTION_WINDOW_FILE 40400-line, 80800-byte source compaction fixture
//   RPI_HTML_FILE    committed HTML preview fixture ("page.html")
//   RPI_PREVIEW_MD_FILE committed clean markdown preview fixture ("readme.md")
//   RPI_CLEAN_FILE   committed-never-modified clean root sibling ("clean.txt")
//   RPI_SOL_FILE     committed clean Solidity Source-lexing preview fixture
//                    ("Token.sol"; NEVER modified — a dirty row routes the
//                    explorer tree click to the diff surface)
//   RPI_SOL_REVIEW_FILE
//                    dirty Solidity review fixture ("src/ReviewToken.sol"):
//                    committed baseline differs from the working tree, so
//                    the review rail renders a real .sol changed diff
//   RPI_SEPOLIA_DELETED_FILE
//                    committed-then-deleted nested path whose directories
//                    exist only in the change catalog
//                    ("deployments/sepolia/deploy.json")
//   RPI_SKILL_NAME   fixture project skill name ("greet")
//   RPI_SKILL_DESC   fixture project skill description text
//   INJECT_FAIL      self-test flag: '1' injects a real uncaught page error
//                    plus a real console.error on the MOBILE code-review page
//                    so its browser-error gate MUST fail (exit nonzero); the
//                    shell's post-mortem then proves the prior pointer and
//                    published evidence survived byte-identical; default 0
//
// Run-scoped STAGING ONLY: every artifact this run creates (screenshots, the
// coverage assertions JSON, the assertions manifest) is written into the ONE
// staging dir the shell handed over via RPI_STAGING and registered only
// after its write succeeded. This script NEVER renames the staging set,
// NEVER writes a run directory, and NEVER touches the current pointer:
// commands_review.sh runs the HTTP sanity and listener-close gates, validates
// the exact staged set/ledger, and performs the single stage->run rename +
// pointer commit — the lane's sole PASS line is printed by the shell after
// the commit. On failure ONLY the staging dir is removed; published evidence
// and the prior pointer are unreachable from here.
//
// Asserts the Web composer command surface + the real code-review
// workspace against the seeded TEMP git repo + fixture skill:
//   - #command-btn is LEFT of #prompt-input in the composer row
//   - opening the command picker lists /compact, /skill, /code-review
//   - choosing /code-review inserts the draft WITHOUT auto-submitting
//   - Enter opens the real review panel: HEAD→working-tree comparison label,
//     the dirty file row, and the changed diff lines (deletion + addition)
//   - the dirty file renders TWO separated hunks; the SECOND hunk's comment
//     lands only in that hunk's identity/thread (never the first hunk's)
//   - per-hunk drafts A/B survive hunk switches; submitting A leaves B
//   - switching files clears the composer + hunk selection
//   - hostile diff/comment text stays literal (no element, no script, no
//     dialog side effect)
//   - hunks are NEVER auto-selected: after open/file switch the comment
//     composer is absent until the user explicitly selects a hunk header
//   - the file filter narrows/restores the file list; a >4000-line fixture
//     file has a complete body (no truncated token, banner, or bounded
//     status): selecting it eager-fetches the FULL backend page chain
//     (cursor 0→1000→…→8000, maxLines 1000, terminal hasMore=false) with
//     no Load control, and real near-bottom scroll grows the visible
//     window 4000 → 8000 → 8200
//   - the changed-file rail is a collapsible path tree: the nested/ dir row
//     is expanded by default, clicking it collapses/expands the subtree,
//     the filter keeps the matched file + its ancestor (forced expanded),
//     and tree keyboard navigation spans dirs + files (arrows rove, Home/End
//     jump, Enter toggles dirs/selects files, ArrowRight/Left expand/collapse)
//   - TUI/Web parity for the file tree: compact colored status glyphs
//     (modified -> M, added -> A) mirror the TUI letters; every level shows
//     only its basename (nested/deep.txt renders as "deep.txt") while the
//     FULL repo-relative path stays in data-file-path/title and the readable
//     full state in aria-label; selection + filter key on the full path.
//     The trailing status column is proven against a TRUE clean sibling (a
//     committed, never-modified root file with NO status chip): the changed
//     filename x equals the clean sibling's x, the filename text right edge
//     never overlaps the chip, filename/chip centers hit-test to their own
//     buttons, and clicking the chip opens EXACTLY that file's diff
//     (desktop + mobile; mobile chip is a 44x44 touch target)
//   - comment bodies render markdown (strong, lists, ```rust fences with
//     hljs highlight) while hostile HTML stays literal text — no element is
//     created, no script runs, no dialog opens (user AND assistant comments)
//   - plain Enter submits a comment; Shift+Enter inserts a newline without
//     submitting and a synthetic IME-composing Enter never submits; while
//     the first reply streams the composer stays ENABLED with no streaming
//     warning note, a second comment submits immediately and renders as a
//     queued card, and Abort clears the active stream AND drops the queued
//     (not-yet-started) comment while leaving the partial assistant reply
//   - the >4000-line fixture pages WITHOUT manual affordances (no
//     .code-review__load-more/.code-review__load-full, no 'Large file'
//     banner), the removed panel-level 'Large diff — all changed files…'
//     notice stays ABSENT, and typing into the composer with the large
//     diff rendered stays responsive (stable textarea identity, bounded
//     per-keystroke DOM mutation, bounded edit time — never an inflated
//     test timeout)
//   - the panel polls code_review_snapshot at the 1.5s contract while open
//     (≥2 frames ~1.5s apart) and polling stops after the panel closes
//     (≥3.6s with zero snapshot frames and no duplicate close)
//   - a session switch closes the owning review workspace (code_review_close
//     stamped with the owning sessionId), and a bare /code-review in the
//     target session never reuses the previous session's revision args
//   - Escape with a draft shows the inline close confirm; "Keep editing"
//     keeps the workspace + draft; Escape with no draft closes the panel
//   - typing into `.code-review__comment-input` then immediately dispatching
//     click on `button.code-review__close` from the page (no Playwright
//     pointer click, no blur/hunk switch) shows `.code-review__confirm`
//     with "1 unsent draft"; Keep editing (`button:nth-child(2)`) restores
//     the exact whitespace+newline bytes and focuses the textarea. Same for
//     the header Refresh button (`title="Reload the diff snapshot"`).
//     Refresh primary (`.code-review__confirm-actions button:nth-child(1)`)
//     then dispatches exactly one `code_review_refresh`. Close primary keeps
//     the panel until click, then emits exactly one additional
//     `code_review_close` stamped with the owning sessionId (zero/new-close
//     before the primary click, no duplicate close and no snapshot frames
//     over a ≥3.6s post-close window). DOM unmount alone is not enough.
//   - mobile (≤900px): Files→Diff→Thread tab transitions, composer visible
//     without scrolling, Back-to-diff, and button.code-review__close removes
//     #code-review-panel from the DOM; `.code-review__thread-resizer` hidden
//   - the outer shell (#code-review-panel) is a FULL-viewport fixed overlay:
//     rect ≈ (0,0), width == innerWidth/visualViewport width, height ==
//     innerHeight/visualViewport height on desktop 1360x900 and mobile
//     390x844 — a 400px width rail, 94vw route gutter, bottom-drawer top
//     offset, top-header inset, or 90vh/drawer-height height each FAIL here
//     by name; the header/body stay inside the shell and the page behind it
//     does not scroll. Closing unmounts the panel AND restores the prior app
//     layout (scroll position, composer rect, focus).
//   - desktop thread column: `.code-review__thread-resizer` ARIA separator,
//     `--code-review-thread-width` (default 280, bounds 240–480), pointer +
//     keyboard + localStorage `rpi-code-review-thread-width` reload persistence
//   - selecting /skill drills the picker into skills mode; the REAL on-disk
//     fixture skill candidate (greet) renders with name + description;
//     selecting it inserts `/skill greet` (no auto-submit) and Enter renders
//     the loaded skill's frontmatter summary bubble
//   - /compact dispatches the compact RPC — observed on the outgoing WS frame
//     (deterministic; the provider round-trip is never required for this lane)
//   - slash-command variants: /compact --snap dispatches snapcompact and
//     renders the REAL deterministic A→B token report bubble; /compact
//     <instructions> and bare /compact dispatch compact (with/without
//     customInstructions) and surface the truthful "session too small"
//     error bubble + toast; /skill greet extra uses only the first token
//     as the skill name; /skill bare is a usage-error toast with no RPC;
//     /unknowncmd falls through to a normal prompt (user bubble + prompt RPC)
//   - code-review error/close: /code-review HEAD~1 HEAD opens the panel with
//     the from/to revisions and closes via the × button; /code-review
//     <bad-ref> HEAD renders the real git revision error state with Retry
//     and closes cleanly; /code-review a b c is a client-side arity toast
//     that never opens the panel
//   - the literal single-ref user path `/code-review origin/master` (the
//     fixture seeds refs/remotes/origin/master LOCALLY — no remote URL, no
//     network) dispatches EXACTLY ONE code_review_open with
//     from=origin/master includeWorkingTree=true and no `to` key (the
//     parser contract), the comparison label names origin/master → working
//     tree over a non-empty diff (the dirty file row is present), the
//     fullscreen shell contract holds on desktop 1360x900 AND mobile
//     390x844 (assertCrShellGeometry both), the exact command result is
//     screenshotted, and the × close unmounts the panel and restores the
//     prior app layout (scroll, composer rect, focus)
//   - Command copy: real-browser click, keyboard Enter, and mobile ≥40×40
//     touch target on `.tool-card__command-copy` / `.bash-copy`; clipboard
//     carries the exact multiline redacted command (never the hidden secret);
//     success flashes `copied` / `Command copied` then resets; clipboard
//     rejection flashes `copy failed` / `Copy failed` then resets
//   - review ownership across a cross-session switch (A → B → A): once the
//     DISPLAY authority has settled on the owning session (displayed id AND
//     command target — the sidebar row follows the active session state), the
//     restored view slot must re-open the panel for that session and dispatch a
//     bare owner-owned code_review_open that SUCCEEDS with its working-tree
//     snapshot; the revisions captured for the previous session are dropped and
//     no live code_review_* frame names any other session
//   - review ownership when the active session ref has diverged from the
//     display (a reconnect that still displays the sibling): the panel stays
//     mounted across a reconnect that keeps its owner (no close frame), and
//     leaving the sibling for a THIRD session — whose own slot remembers its
//     review panel — closes the sibling's review on the sibling
//     (probe-verified no-open), opens the third session's own bare scope
//     successfully, and carries none of the sibling's revisions. (The sidebar
//     paints the row the REBOUND ref names as active and ignores clicks on it,
//     so "switch to the primary while the ref already names it" is not a
//     reachable user action.)
//   - owner-targeted cleanup inside a pending switch: the sibling's reconnect
//     catch-up cut AND its negotiated switch cut are held (so the switch cannot
//     take the fast path and its pending window stays open), the handoff paints
//     the sibling as displayed, and the deferred probe then resolves: the review
//     is closed exactly once on the session being left, that session is
//     probe-verified open-free, and no live frame after the handoff names it
//   - the MOUNTED panel's owner close across a pending switch: with a review
//     open and applied on the displayed session, a negotiated handoff (its
//     authoritative cut held) unmounts the panel — its owner-stamped
//     code_review_close must land on the wire EXACTLY once, the session it owns
//     is probe-verified open-free, and no close-failure toast appears (an
//     ungated close is refused before it reaches the wire)
//   - a RESTORED panel recovers its own bootstrap: the target's slot keeps the
//     review panel (it is left with the panel open), the switch back restores it
//     inside the still-pending window, and after the settle it must dispatch a
//     SUCCESSFUL bare target-owned code_review_open whose response carries the
//     working-tree label, apply that snapshot, recover its model catalog
//     (get_available_models for the target) and raise no switch-pending toast —
//     an ungated bootstrap is refused and never retried
//   - warm reconnect slash ownership: with the displayed session a sibling of
//     the listener primary, a forced socket close + auto-reconnect leaves
//     display authority (and its command target) on the displayed session;
//     `/code-review HEAD~1 HEAD` then dispatches code_review_open stamped with
//     the DISPLAYED session (never the re-bound primary) — and no
//     code_review_open frame rides any other session, so the mounted panel
//     cannot reopen the review in the primary — while the backend probe on the
//     displayed session reports the open review, the panel renders the
//     requested revisions, and no invalid-revision toast appears (git resolved
//     the revision in the displayed session's workspace)
//   - File Explorer (1360/900/390): ordinary Files open, integrated Git
//     M/A/D/R tree + dir aggregate, code/markdown infinite scroll without
//     Load more, right-click delete of a tracked row (exactly one
//     file_explorer_delete, no download — the tree download affordance was
//     removed; git-only rows are inert), drawer close reclaiming Send,
//     and global Files reopen of exact selection/expansion/split/scroll/paging,
//     and the true-clean-sibling status-column contract (alignment, no
//     filename/chip overlap, own hit targets, chip opens the exact file
//     diff — desktop + mobile 44px) with zero page/console errors (coverage
//     IDs files.*)
//   - Ask about the committed clean Token.sol (files.ask): the visible Ask
//     control opens the source dock beside the intact source; a unique
//     question dispatches file_conversation_open for the displayed session
//     and the exact owner path plus a UUID, then file_conversation_prompt
//     reuses that id with no selection; a real text selection then prompts
//     the same id with that range; the assistant reply is the fixture
//     model's actual stream; Stop aborts that exact id; switching to a
//     second file keeps the first history and opens its own conversation;
//     Back and the drawer close keep the owner and send no close for
//     the wrong path (desktop ≥1280 and mobile 390×844)
//   - Files fullscreen (files.fullscreen, after files.ask): desktop 1360×900
//     and mobile 390×844 go normal → expanded → restored, then close from
//     fullscreen. Expanded hides the App global header, pins the explorer
//     top at 0, and keeps Restore/Close reachable. Restore and close recover
//     the original header geometry before the next paint. Ask state from
//     the preceding journey stays. Chat has no fullscreen control.
//   - files.selection-cache-compaction: retained Ask lines 4–7 survive real
//     paging through cursor 40000 and match the prompt and accepted context.
//   - Dependency map (files.map, after files.selection): the committed clean
//     MapRoot.sol opens its Map surface on the owner: an actual rendered SVG
//     diagram with the selected root, the resolved ./MapDep.sol neighbor, the
//     external forge-std/Test.sol import, the unresolved ./MissingMap.sol
//     import, and the reverse MapCaller.sol dependent; node-list buttons open
//     the exact neighbor through the owner; switching to another file while
//     the map request is still in flight paints no map for the wrong file.
//   - committed-then-deleted nested deployments/sepolia/deploy.json keeps its
//     changed-file catalog row visible (virtual git-only D) while the
//     unavailable virtual directories are hidden: no expandable dir row for
//     deployments/deployments/sepolia, no 'directory is unavailable'
//     error/Retry, and zero file_explorer_list frames for either dir
//     (files.virtual-dirs-hidden)
//   - Solidity lexing in the preview journey at both breakpoints: seeded
//     Token.sol renders specific .hljs-keyword tokens (contract, function)
//     over the intact line-for-line source — never a plain unhighlighted
//     render (files.solidity-preview); the review panel's own
//     src/ReviewToken.sol changed diff (committed baseline ≠ working tree;
//     the clean Token.sol preview fixture stays separate) lexes
//     contract/function/uint256 tokens over exact preserved line text at
//     both breakpoints too, and every leg returns to the dirty file

import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';
import WebSocket from 'ws';
import { createFreshSessionViaManage } from './lib/session-lifecycle.mjs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const dirtyFile = process.env.RPI_DIRTY_FILE || 'greet.txt';
const addedFile = process.env.RPI_ADDED_FILE || 'added.txt';
const bigFile = process.env.RPI_BIG_FILE || 'big.txt';
const nestedFile = process.env.RPI_NESTED_FILE || 'nested/deep.txt';
const deletedFile = process.env.RPI_DELETED_FILE || 'gone.txt';
const renamedFile = process.env.RPI_RENAMED_FILE || 'nested/renamed.txt';
const renamedOld = process.env.RPI_RENAMED_OLD || 'nested/old.txt';
const markdownFile = process.env.RPI_MARKDOWN_FILE || 'notes.md';
const selectionWindowFile = process.env.RPI_SELECTION_WINDOW_FILE || 'selection-window.txt';
const htmlFile = process.env.RPI_HTML_FILE || 'page.html';
const previewMarkdownFile = process.env.RPI_PREVIEW_MD_FILE || 'readme.md';
const cleanFile = process.env.RPI_CLEAN_FILE || 'clean.txt';
const solFile = process.env.RPI_SOL_FILE || 'Token.sol';
const solCommit = process.env.RPI_SOL_COMMIT || '';
const solCommitTime = Number(process.env.RPI_SOL_COMMIT_TIME || '0');
const solReviewFile = process.env.RPI_SOL_REVIEW_FILE || 'src/ReviewToken.sol';
const sepoliaDeletedFile = process.env.RPI_SEPOLIA_DELETED_FILE || 'deployments/sepolia/deploy.json';
const skillName = process.env.RPI_SKILL_NAME || 'greet';
const solMapRootFile = process.env.RPI_SOL_MAP_ROOT_FILE || 'MapRoot.sol';
const solMapDepFile = process.env.RPI_SOL_MAP_DEP_FILE || 'MapDep.sol';
const solMapCallerFile = process.env.RPI_SOL_MAP_CALLER_FILE || 'MapCaller.sol';
const skillDesc = process.env.RPI_SKILL_DESC || 'Greet skill for E2E';
const INJECT_FAIL = process.env.INJECT_FAIL === '1';

const FILES_IDS = [
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

// ONE documented console policy: every pageerror and every console.error
// fails the lane EXCEPT favicon fetch failures (browsers request
// /favicon.ico automatically; the fixture server has none) and the
// sandboxed HTML preview iframe reading localStorage (sandbox="" has no
// allow-same-origin; that throw is the isolation contract).
function isKnownConsoleNoise(text) {
  return text.includes('favicon')
    || text.includes("The document is sandboxed and lacks the 'allow-same-origin' flag")
    || text === "Blocked script execution in 'about:srcdoc' because the document's frame is sandboxed and the 'allow-scripts' permission is not set.";
}

// Run-scoped STAGING ONLY: every artifact this run creates (screenshots, the
// coverage assertions JSON, the assertions manifest) is written into the ONE
// staging dir the shell handed over via RPI_STAGING and registered only
// after its write succeeded. Existing published evidence is NEVER touched
// during the run or on failure — a failed run removes ONLY its staging dir;
// the stage->run rename and the current pointer belong to the shell.
const stagingDir = process.env.RPI_STAGING
  || fail('RPI_STAGING is required (the shell hands over the run-scoped staging dir)');
const laneNonce = process.env.RPI_LANE_NONCE || '';
const laneArtifacts = []; // staged artifact names, AFTER their write succeeded

function stageWrite(name, data) {
  fs.mkdirSync(stagingDir, { recursive: true });
  fs.writeFileSync(path.join(stagingDir, name), data);
  laneArtifacts.push(name);
  return path.join(stagingDir, name);
}

async function laneShot(page, name, opts = {}) {
  fs.mkdirSync(stagingDir, { recursive: true });
  const staged = path.join(stagingDir, name);
  await page.screenshot({ path: staged, ...opts });
  laneArtifacts.push(name);
  const keep = process.env.RPI_FAILURE_EVIDENCE_DIR || process.env.RPI_EVIDENCE;
  if (keep && name.startsWith('files-selection')) {
    fs.mkdirSync(keep, { recursive: true });
    fs.copyFileSync(staged, path.join(keep, name));
  }
  return staged;
}

function cleanupStaging() {
  fs.rmSync(stagingDir, { recursive: true, force: true });
}

// Stage the final assertions manifest: the run nonce and the EXACT
// registered artifact list (screenshots + the coverage ledger written by
// this run) so the shell's commit gate can validate the staged set precisely
// before any run-dir rename. The manifest file itself is never in its own
// artifact list; the shell adds it to the declared set.
function stageManifest() {
  fs.mkdirSync(stagingDir, { recursive: true });
  fs.writeFileSync(path.join(stagingDir, 'playwright-assertions.json'), JSON.stringify({
    lane: 'web-commands-review',
    nonce: laneNonce,
    artifacts: laneArtifacts,
  }, null, 2));
}

function fail(message) {
  const error = new Error(message);
  error.laneAssertion = true;
  throw error;
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

/** Open the command picker and return the option element handles shown. */
async function openPicker(page) {
  await page.click('#command-btn');
  await waitFor(page, () => document.querySelector('.command-picker__popover') !== null, 'command picker popover did not open');
}

/** Names (with leading '/') currently shown in the picker list. */
async function pickerNames(page) {
  return page.evaluate(() =>
    Array.from(document.querySelectorAll('.command-picker__option .command-picker__name')).map((el) => el.textContent.trim())
  );
}

/** Wait until the picker renders the option `/${name}`, then click it. The
 *  option list is populated asynchronously (get_commands resolves only after
 *  the popover opens and the loading state clears), so the lookup retries —
 *  mirroring the first-open contract instead of racing the catalog fetch. */
async function chooseCommand(page, name) {
  await waitFor(
    page,
    (want) => Array.from(document.querySelectorAll('.command-picker__option')).some((li) =>
      li.querySelector('.command-picker__name')?.textContent.trim() === `/${want}`
    ),
    `command picker option /${name} did not appear`,
    25000,
    name
  );
  const clicked = await page.evaluate((want) => {
    const opt = Array.from(document.querySelectorAll('.command-picker__option')).find((li) =>
      li.querySelector('.command-picker__name')?.textContent.trim() === `/${want}`
    );
    if (!opt) return false;
    // The option chooses on mousedown (preventDefault keeps search focus);
    // a real click dispatches mousedown then click, so .click() works.
    opt.click();
    return true;
  }, name);
  if (!clicked) fail(`command picker option /${name} not found`);
}
/** Wait until the picker's skills mode renders the candidate for `skillName`,
 *  then click it. Selecting `/skill` drills the picker into skills mode, where
 *  each candidate row carries `data-skill-name="<bare>"` and shows the bare
 *  name + frontmatter description. The candidate list is populated from the
 *  real `get_commands` catalog (loaded skills from disk), so this proves the
 *  picker surfaces the on-disk fixture skill, not a hardcoded list. */
async function chooseSkillCandidate(page, skillName) {
  await waitFor(
    page,
    (want) => Array.from(document.querySelectorAll('.command-picker__option[data-skill-name]')).some(
      (li) => li.getAttribute('data-skill-name') === want
    ),
    `skill candidate ${skillName} did not appear in picker skills mode`,
    25000,
    skillName
  );
  const clicked = await page.evaluate((want) => {
    const opt = Array.from(document.querySelectorAll('.command-picker__option[data-skill-name]')).find(
      (li) => li.getAttribute('data-skill-name') === want
    );
    if (!opt) return false;
    opt.click();
    return true;
  }, skillName);
  if (!clicked) fail(`skill candidate ${skillName} not found in picker`);
}

/** Layout baseline of the app beneath the modal code-review shell (scroll
 *  position, composer rect, focused element id) captured BEFORE the panel
 *  opens; compared after it closes for the restore contract. */
async function crAppLayout(page) {
  return page.evaluate(() => {
    const el = document.getElementById('prompt-input');
    const r = el?.getBoundingClientRect();
    return {
      scrollY: (document.scrollingElement || document.documentElement).scrollTop,
      scrollHeight: (document.scrollingElement || document.documentElement).scrollHeight,
      composer: r ? { x: r.x, y: r.y, w: r.width, h: r.height } : null,
      activeId: document.activeElement?.id || '',
    };
  });
}

/** Outer-shell geometry probe for #code-review-panel: viewport-relative rect
 *  + computed position + header/body rects + document scroll extent. */
async function crShellGeometry(page) {
  return page.evaluate(() => {
    const p = document.getElementById('code-review-panel');
    if (!p) return null;
    const r = p.getBoundingClientRect();
    const cs = getComputedStyle(p);
    const head = p.querySelector('.code-review__head')?.getBoundingClientRect() || null;
    const body = p.querySelector('.code-review')?.getBoundingClientRect() || null;
    const vv = window.visualViewport;
    return {
      x: r.x, y: r.y, w: r.width, h: r.height,
      innerW: window.innerWidth, innerH: window.innerHeight,
      vvW: vv ? vv.width : window.innerWidth,
      vvH: vv ? vv.height : window.innerHeight,
      position: cs.position,
      head, body,
      docScrollW: document.documentElement.scrollWidth,
      docScrollH: document.documentElement.scrollHeight,
    };
  });
}

/** Full-viewport shell contract. Every failure names the regression class it
 *  proves, so a geometry regression is never a generic size mismatch:
 *  width-rail (shared drawer 400px base), route gutter (94vw max-width),
 *  bottom drawer / top-header inset (y>0), 90vh/drawer height (h mismatch),
 *  header/body escape, page scroll behind the shell. */
function assertCrShellGeometry(g, label) {
  if (!g) fail(`${label}: #code-review-panel missing for shell geometry assertions`);
  if (Math.abs(g.x) > 1) fail(`${label}: shell not flush left (x=${g.x}) — route-gutter/width-rail regression ${JSON.stringify(g)}`);
  if (Math.abs(g.y) > 1) fail(`${label}: shell not flush top (y=${g.y}, position=${g.position}, head=${JSON.stringify(g.head)}, body=${JSON.stringify(g.body)}) — bottom-drawer/top-header-inset regression`);
  if (Math.abs(g.w - g.innerW) > 1 || Math.abs(g.w - g.vvW) > 1) {
    fail(`${label}: shell width ${g.w} != viewport ${g.innerW} (visualViewport ${g.vvW}) — width-rail regression ${JSON.stringify(g)}`);
  }
  if (Math.abs(g.h - g.innerH) > 2 || Math.abs(g.h - g.vvH) > 2) {
    fail(`${label}: shell height ${g.h} != viewport ${g.innerH} (visualViewport ${g.vvH}) — 90vh/drawer-height regression ${JSON.stringify(g)}`);
  }
  if (g.position !== 'fixed') fail(`${label}: shell position must be fixed (got "${g.position}")`);
  if (g.head && (g.head.top < g.y - 1 || g.head.bottom > g.y + g.h + 1)) {
    fail(`${label}: header escapes the shell rect (head ${g.head.top}..${g.head.bottom}, shell ${g.y}..${g.y + g.h})`);
  }
  if (g.body && (g.body.top < g.y - 1 || g.body.bottom > g.y + g.h + 1)) {
    fail(`${label}: body escapes the shell rect (body ${g.body.top}..${g.body.bottom}, shell ${g.y}..${g.y + g.h})`);
  }
  if (g.docScrollW > g.innerW + 1 || g.docScrollH > g.innerH + 1) {
    fail(`${label}: page scrolls behind the fullscreen shell (doc ${g.docScrollW}x${g.docScrollH} vs viewport ${g.innerW}x${g.innerH})`);
  }
}

/** Close-restore contract: after the panel unmounts the underlying app must
 *  be exactly where it was pre-open (scroll, composer rect, focus). */
function assertCrLayoutRestored(before, after, label) {
  if (!before.composer || !after.composer) {
    fail(`${label}: composer rect missing for close-restore comparison (before=${JSON.stringify(before.composer)}, after=${JSON.stringify(after.composer)})`);
  }
  const close = (a, b) => Math.abs(a - b) <= 1;
  if (!close(after.composer.x, before.composer.x) || !close(after.composer.y, before.composer.y)
    || !close(after.composer.w, before.composer.w) || !close(after.composer.h, before.composer.h)) {
    fail(`${label}: composer rect changed across panel close (before ${JSON.stringify(before.composer)}, after ${JSON.stringify(after.composer)})`);
  }
  if (after.scrollY !== before.scrollY || after.scrollHeight !== before.scrollHeight) {
    fail(`${label}: page scroll changed across panel close (before ${before.scrollY}/${before.scrollHeight}, after ${after.scrollY}/${after.scrollHeight})`);
  }
  if (after.activeId !== before.activeId) {
    fail(`${label}: focus not restored after panel close (before "${before.activeId}", after "${after.activeId}")`);
  }
}

/** Mobile (≤768px) shell contract — the code review workspace is a fixed,
 *  full-WIDTH overlay that sits between the app header and the app footer so
 *  the composer stays reachable (styles.css `.code-review-panel`
 *  @media (max-width: 768px): `inset: var(--app-header-height) 0
 *  var(--app-footer-height, 0px) 0`; the ≥769px rule is the full-viewport
 *  contract asserted by assertCrShellGeometry). Both insets are the SAME
 *  measured integers App writes to <html>, so this is the product's own
 *  geometry — and every drawer regression still fails here by name: a 400px
 *  rail, the 94vw route gutter, a right-aligned/bottom drawer, a 90vh height,
 *  a shell covering the composer, or a page that scrolls behind it. */
async function assertCrMobileShellGeometry(page, label) {
  const probe = await page.evaluate(() => {
    const p = document.getElementById('code-review-panel');
    if (!p) return null;
    const r = p.getBoundingClientRect();
    const cs = getComputedStyle(p);
    const root = getComputedStyle(document.documentElement);
    const px = (name) => {
      const raw = root.getPropertyValue(name).trim();
      const value = Number.parseFloat(raw);
      return raw.endsWith('px') && Number.isFinite(value) ? value : null;
    };
    return {
      x: r.x, y: r.y, w: r.width, bottom: r.bottom,
      innerW: window.innerWidth, innerH: window.innerHeight,
      position: cs.position,
      headerHeight: px('--app-header-height'),
      footerHeight: px('--app-footer-height'),
      head: p.querySelector('.code-review__head')?.getBoundingClientRect() || null,
      body: p.querySelector('.code-review')?.getBoundingClientRect() || null,
      docScrollW: document.documentElement.scrollWidth,
      docScrollH: document.documentElement.scrollHeight,
    };
  });
  if (!probe) fail(`${label}: #code-review-panel missing for the mobile shell geometry assertions`);
  if (probe.position !== 'fixed') fail(`${label}: mobile shell position must be fixed (got "${probe.position}")`);
  if (Math.abs(probe.x) > 1 || Math.abs(probe.w - probe.innerW) > 1) {
    fail(`${label}: mobile shell must span the viewport width (x=${probe.x}, w=${probe.w}, viewport=${probe.innerW}) — width-rail/route-gutter regression`);
  }
  if (probe.headerHeight === null || Math.abs(probe.y - probe.headerHeight) > 1) {
    fail(`${label}: mobile shell must start at the app header's bottom (y=${probe.y}, --app-header-height=${probe.headerHeight}) — top-header inset regression`);
  }
  const footerTop = probe.footerHeight === null ? null : probe.innerH - probe.footerHeight;
  if (footerTop === null || Math.abs(probe.bottom - footerTop) > 1) {
    fail(`${label}: mobile shell must end at the app footer's top (bottom=${probe.bottom}, expected ${footerTop}, --app-footer-height=${probe.footerHeight}) — the composer must stay reachable`);
  }
  if (probe.head && (probe.head.top < probe.y - 1 || probe.head.bottom > probe.bottom + 1)) {
    fail(`${label}: header escapes the mobile shell rect (head ${probe.head.top}..${probe.head.bottom}, shell ${probe.y}..${probe.bottom})`);
  }
  if (probe.body && (probe.body.top < probe.y - 1 || probe.body.bottom > probe.bottom + 1)) {
    fail(`${label}: body escapes the mobile shell rect (body ${probe.body.top}..${probe.body.bottom}, shell ${probe.y}..${probe.bottom})`);
  }
  if (probe.docScrollW > probe.innerW + 1 || probe.docScrollH > probe.innerH + 1) {
    fail(`${label}: page scrolls behind the mobile shell (doc ${probe.docScrollW}x${probe.docScrollH} vs viewport ${probe.innerW}x${probe.innerH})`);
  }
}

/** True when the picker has drilled into skills mode (the `Skills` header is
 *  present). Used to assert the `/skill` parent switches the picker surface. */
async function pickerInSkillsMode(page) {
  return page.evaluate(() => {
    const title = document.querySelector('.command-picker__title')?.textContent?.trim() || '';
    return title === 'Skills';
  });
}

const filesExecuted = new Map(); // id -> exact execution count (per-ID ledger, no Set erasure)
function recordFiles(id) {
  filesExecuted.set(id, (filesExecuted.get(id) || 0) + 1);
}

function parseSentFrame(raw) {
  try {
    const parsed = JSON.parse(raw);
    return parsed && typeof parsed.type === 'string' ? parsed : null;
  } catch {
    return null;
  }
}

function codeReviewCloseFrames(sentFrames) {
  return sentFrames.map(parseSentFrame).filter((frame) => frame?.type === 'code_review_close');
}

/** Sent code_review_file_diff commands for one path (raw payloads → frames). */
function codeReviewFileDiffCommands(sentFrames, path) {
  return sentFrames
    .map(parseSentFrame)
    .filter((frame) => frame?.type === 'code_review_file_diff' && frame.path === path);
}

/** Received code_review_file_diff response payloads keyed by request id. */
function codeReviewFileDiffResponses(receivedFrames, path) {
  const responses = new Map();
  for (const raw of receivedFrames) {
    let frame;
    try {
      frame = JSON.parse(raw);
    } catch {
      continue;
    }
    if (frame?.type === 'response' && frame.data?.path === path && Array.isArray(frame.data.hunks)) {
      responses.set(frame.id, frame.data);
    }
  }
  return responses;
}

/**
 * Assert a file's backend paging wire chain is EXACTLY the expected cursor
 * progression (same contract as code_review_paging_test's assertPageChain):
 * every page requests maxLines=1000 at the previous response's nextCursor,
 * each response matches its request cursor plus the authoritative
 * totalLines, and the terminal page reports hasMore=false with no
 * nextCursor. Run only after the eager fetch has fully settled (the last
 * window row is rendered).
 */
function assertCodeReviewPageChain(sentFrames, receivedFrames, path, totalLines, pageSize = 1000) {
  const cmds = codeReviewFileDiffCommands(sentFrames, path);
  const expectedCount = Math.ceil(totalLines / pageSize);
  if (cmds.length !== expectedCount) {
    fail(`paging chain: expected ${expectedCount} code_review_file_diff pages for ${path}, got ${cmds.length}`);
  }
  const responses = codeReviewFileDiffResponses(receivedFrames, path);
  for (let i = 0; i < cmds.length; i++) {
    const cmd = cmds[i];
    const wantCursor = i * pageSize;
    if ((cmd.cursor ?? 0) !== wantCursor) {
      fail(`paging chain: ${path} page ${i} requested cursor ${cmd.cursor}, expected ${wantCursor}`);
    }
    if (cmd.maxLines !== pageSize) {
      fail(`paging chain: ${path} page ${i} requested maxLines ${cmd.maxLines}, expected ${pageSize}`);
    }
    const page = responses.get(cmd.id);
    if (!page) fail(`paging chain: no response for ${path} page ${i} (cursor ${wantCursor})`);
    if (page.cursor !== wantCursor) {
      fail(`paging chain: ${path} page ${i} response cursor ${page.cursor} != request ${wantCursor}`);
    }
    if (page.totalLines !== totalLines) {
      fail(`paging chain: ${path} page ${i} totalLines ${page.totalLines} != ${totalLines}`);
    }
    const terminal = i === cmds.length - 1;
    if (terminal) {
      if (page.hasMore !== false) {
        fail(`paging chain: ${path} terminal page must report hasMore=false (got ${page.hasMore})`);
      }
      if (page.nextCursor !== undefined) {
        fail(`paging chain: ${path} terminal page must not carry nextCursor (got ${page.nextCursor})`);
      }
    } else if (page.hasMore !== true || page.nextCursor !== wantCursor + pageSize) {
      fail(`paging chain: ${path} page ${i} must continue at ${wantCursor + pageSize} (got hasMore=${page.hasMore}, nextCursor=${page.nextCursor})`);
    }
  }
}

function wsUrlFrom(pageUrl) {
  return pageUrl.replace(/^http/, 'ws').replace(/\/web$/, '/ws');
}

function rpcClient(wsUrl) {
  const ws = new WebSocket(wsUrl, token ? [`rpi-auth.${token}`] : []);
  const pending = new Map();
  // The listener's per-runtime command ledger keys on the client-supplied id
  // and rejects a REUSED id whose request digest differs ("command id was
  // already used for a different request"), so every instance mints its own
  // namespace: two probes of the same session with a shared `cmd-copy-1` would
  // otherwise collide.
  const namespace = `cmd-${Math.random().toString(36).slice(2, 10)}`;
  let seq = 0;
  ws.on('message', (raw) => {
    let frame;
    try {
      frame = JSON.parse(String(raw));
    } catch {
      return;
    }
    if (frame && frame.type === 'response' && frame.id && pending.has(frame.id)) {
      const { resolve, reject } = pending.get(frame.id);
      pending.delete(frame.id);
      if (frame.success) resolve(frame.data || {});
      else reject(new Error(frame.error || 'rpc failed'));
    }
  });
  const ready = new Promise((resolve, reject) => {
    ws.on('open', resolve);
    ws.on('error', reject);
  });
  return {
    ready,
    async call(command) {
      await ready;
      const id = `${namespace}-${++seq}`;
      return new Promise((resolve, reject) => {
        pending.set(id, { resolve, reject });
        ws.send(JSON.stringify({ ...command, id }));
        setTimeout(() => {
          if (pending.delete(id)) reject(new Error(`rpc timed out: ${command.type}`));
        }, 20000);
      });
    },
    close() {
      try { ws.close(); } catch { /* already closed */ }
    },
  };
}

async function readClipboardText(page) {
  return page.evaluate(async () => navigator.clipboard.readText());
}

async function assertCommandCopyButton(page, selector, {
  expectedClipboard,
  secret = 'supersecret',
  keyboard = false,
}) {
  await waitFor(
    page,
    (sel) => {
      const btn = [...document.querySelectorAll(sel)].at(-1);
      return !!btn && (btn.getAttribute('aria-label') || '') === 'Copy command' && (btn.textContent || '').trim() === 'copy';
    },
    `command-copy ${selector} never rendered idle Copy command`,
    20000,
    selector
  );
  const button = page.locator(selector).last();
  if (keyboard) {
    await button.focus();
    const focused = await page.evaluate((sel) => {
      const btn = [...document.querySelectorAll(sel)].at(-1);
      return !!btn && document.activeElement === btn;
    }, selector);
    if (!focused) fail(`command-copy ${selector}: keyboard focus never landed on the copy button`);
    await page.keyboard.press('Enter');
  } else {
    await button.click();
  }
  await waitFor(
    page,
    (sel) => {
      const btn = [...document.querySelectorAll(sel)].at(-1);
      return !!btn && (btn.textContent || '').trim() === 'copied' && (btn.getAttribute('aria-label') || '') === 'Command copied';
    },
    `command-copy ${selector}: ${keyboard ? 'keyboard' : 'click'} did not flash copied / Command copied`,
    10000,
    selector
  );
  const copied = await readClipboardText(page);
  if (copied !== expectedClipboard) {
    fail(`command-copy ${selector}: clipboard was not the exact redacted multiline payload (got=${JSON.stringify(copied)})`);
  }
  if (copied.includes(secret)) {
    fail(`command-copy ${selector}: clipboard recovered the hidden secret`);
  }
  await waitFor(
    page,
    (sel) => {
      const btn = [...document.querySelectorAll(sel)].at(-1);
      return !!btn && (btn.textContent || '').trim() === 'copy' && (btn.getAttribute('aria-label') || '') === 'Copy command';
    },
    `command-copy ${selector}: success flash did not reset to idle Copy command`,
    4000,
    selector
  );
}


function filesWsOf(sentFrames, type, filePath) {
  return sentFrames.map(parseSentFrame).filter((frame) => {
    if (!frame || frame.type !== type) return false;
    if (!filePath) return true;
    return frame.path === filePath;
  });
}

function filesPageRequests(sentFrames, filePath) {
  return sentFrames.map(parseSentFrame).filter((frame) => (
    frame
    && (frame.type === 'file_explorer_read' || frame.type === 'file_explorer_diff')
    && frame.path === filePath
  ));
}

function filesBasename(filePath) {
  const slash = filePath.lastIndexOf('/');
  return slash >= 0 ? filePath.slice(slash + 1) : filePath;
}

async function hitTestSelector(page, selector, label) {
  const deadline = Date.now() + 10000;
  let top = '';
  while (Date.now() < deadline) {
    const hit = await page.evaluate((sel) => {
      const node = document.querySelector(sel);
      if (!node) return { ok: false, top: 'missing' };
      const box = node.getBoundingClientRect();
      if (box.width < 1 || box.height < 1) return { ok: false, top: 'empty-box' };
      const el = document.elementFromPoint(box.x + box.width / 2, box.y + box.height / 2);
      return {
        ok: el?.closest(sel) === node,
        top: el ? `${el.id || el.tagName}.${el.className || ''}` : 'none',
      };
    }, selector);
    if (hit.ok) return;
    top = hit.top;
    await page.waitForTimeout(50);
  }
  fail(`${label}: ${selector} is not the top element at its center (top=${top})`);
}

async function ordinaryClick(page, selector, label) {
  await hitTestSelector(page, selector, label);
  await page.locator(selector).click();
}

async function ensureFilesToggleHittable(page) {
  const toggle = page.locator('#file-explorer-toggle-btn');
  if (await toggle.isVisible()) return;
  const hamburger = page.locator('#sidebar-toggle-btn');
  if (await hamburger.count()) {
    const pressed = await hamburger.getAttribute('aria-pressed');
    if (pressed !== 'true') {
      await hamburger.click({ force: true });
    }
  }
  await waitFor(
    page,
    () => {
      const btn = document.getElementById('file-explorer-toggle-btn');
      if (!btn) return false;
      const style = getComputedStyle(btn);
      const box = btn.getBoundingClientRect();
      return style.display !== 'none'
        && style.visibility !== 'hidden'
        && style.opacity !== '0'
        && box.width > 0
        && box.height > 0
        && box.bottom > 0
        && box.top < window.innerHeight;
    },
    '#file-explorer-toggle-btn never became hittable'
  );
}

async function openFileExplorer(page) {
  if ((await page.locator('section.file-explorer[aria-label="File explorer"]').count()) > 0) return;
  await ensureFilesToggleHittable(page);
  await page.locator('#file-explorer-toggle-btn').click({ force: true });
  await waitFor(
    page,
    () => document.querySelector('section.file-explorer[aria-label="File explorer"]') !== null,
    'File explorer section did not open'
  );
}

async function filesRowLocator(page, filePath) {
  const titleExact = page.locator(`button.file-explorer__row[title="${filePath}"]`);
  if (await titleExact.count()) return titleExact.first();
  const name = filesBasename(filePath);
  return page.locator('button.file-explorer__row').filter({ hasText: name }).first();
}

/** Tap the product's own tree toggle when the Files body is tree-collapsed.
 *  Narrow (≤720px) viewports show ONE pane: with an active file the tree
 *  starts collapsed and only the content pane renders. The collapsed tree
 *  keeps one compact reopen at the tree edge (#file-explorer-tree-toggle,
 *  "Expand file tree"); the lane takes that same control. */
async function expandFilesTree(page) {
  const state = await page.evaluate(() => {
    const body = document.querySelector('.file-explorer__body');
    const toggle = document.getElementById('file-explorer-tree-toggle');
    return {
      collapsed: body?.classList.contains('file-explorer__body--tree-collapsed') === true,
      pressed: toggle?.getAttribute('aria-pressed') === 'true',
      hasToggle: toggle != null,
      narrow: typeof window.matchMedia === 'function'
        && window.matchMedia('(max-width: 720px)').matches,
    };
  });
  if (!state.collapsed) {
    if (state.hasToggle && state.pressed) {
      fail('file tree toggle is pressed (aria-pressed=true) but the body is not tree-collapsed');
    }
    return;
  }
  if (!state.hasToggle) fail('file tree body is collapsed but #file-explorer-tree-toggle is missing');
  if (!state.pressed) fail('file tree body is collapsed but the tree toggle is not aria-pressed');
  await page.locator('#file-explorer-tree-toggle').click();
  await waitFor(
    page,
    () => {
      const body = document.querySelector('.file-explorer__body');
      const tree = document.querySelector('.file-explorer__tree-viewport');
      if (!body || body.classList.contains('file-explorer__body--tree-collapsed')) return false;
      if (!tree) return false;
      const box = tree.getBoundingClientRect();
      return getComputedStyle(tree).display !== 'none' && box.width > 0 && box.height > 0;
    },
    'file tree did not expand after tapping the tree toggle'
  );
}

async function clickFilesRow(page, filePath, options = {}) {
  await waitFor(
    page,
    (want) => Array.from(document.querySelectorAll('button.file-explorer__row')).some((btn) => {
      const title = btn.getAttribute('title') || '';
      const name = (btn.querySelector('.file-explorer__row-path')?.textContent || '').trim();
      return title === want || title.endsWith(`/${want}`) || name === want.split('/').pop();
    }),
    `File Explorer row ${filePath} did not appear`,
    25000,
    filePath
  );
  const row = await filesRowLocator(page, filePath);
  // ≤720px viewports are exclusive-pane (styles.css): with an active file the
  // tree starts collapsed and the content pane owns the body. A user taps the
  // header "Expand file tree" toggle to reach the rows — the lane must too.
  await expandFilesTree(page);
  await row.scrollIntoViewIfNeeded();
  await row.evaluate((node) => {
    const tree = node.closest('.file-explorer__tree-viewport');
    const list = tree?.querySelector('.file-explorer__tree-list');
    const search = tree?.querySelector('[data-testid="file-explorer-search"]')
      ?? document.querySelector('[data-testid="file-explorer-search"]');
    if (!tree || !list) throw new Error('file row has no tree list');
    const bounds = tree.getBoundingClientRect();
    const top = search?.getBoundingClientRect().bottom ?? bounds.top;
    const rect = node.getBoundingClientRect();
    list.scrollTop += rect.top + rect.height / 2 - (top + bounds.bottom) / 2;
  });
  const box = await row.boundingBox();
  if (!box) fail(`File Explorer row ${filePath} has no box`);
  const hit = await page.evaluate(({ x, y }) => {
    const el = document.elementFromPoint(x, y);
    return el?.closest('button.file-explorer__row') != null;
  }, { x: box.x + box.width / 2, y: box.y + box.height / 2 });
  if (!hit) {
    fail(`File Explorer row ${filePath} is not the top element at its center`);
  }
  await row.click(options.button ? { button: options.button } : undefined);
}

async function clickFilesDir(page, dirTitle) {
  const selector = `button.file-explorer__row--dir[title="${dirTitle}"]`;
  await waitFor(
    page,
    (want) => document.querySelector(`button.file-explorer__row--dir[title="${want}"]`) !== null,
    `File Explorer dir ${dirTitle} missing`,
    25000,
    dirTitle
  );
  await ordinaryClick(page, selector, `expand ${dirTitle}`);
}

const FILES_DRAWER_CLOSE = '#file-explorer-drawer-close-btn';

async function filesChromeContract(page, { desktopResizer }) {
  const chrome = await page.evaluate(() => {
    const header = document.querySelector('[data-testid="file-explorer-header"]');
    const title = header?.querySelector('h2.file-explorer__title');
    const actions = header?.querySelector('.file-explorer__actions');
    const refresh = document.getElementById('file-explorer-refresh-btn');
    const tree = document.getElementById('file-explorer-tree-toggle');
    const treeRegion = document.querySelector('[data-testid="file-explorer-tree"]');
    const treeColumn = treeRegion?.parentElement;
    const search = treeRegion?.querySelector('[data-testid="file-explorer-search"]')
      ?? document.querySelector('[data-testid="file-explorer-search"]');
    const layout = document.querySelector('.app-layout');
    const resizer = document.getElementById('file-explorer-resizer');
    const box = (node) => {
      if (!node) return null;
      const rect = node.getBoundingClientRect();
      return { x: Math.round(rect.x), y: Math.round(rect.y), width: Math.round(rect.width), height: Math.round(rect.height) };
    };
    const visibleBox = (node) => {
      if (!node || node.closest('[hidden]') != null || node.getClientRects().length === 0) return null;
      return box(node);
    };
    const searchBox = visibleBox(search);
    const treeBox = box(tree);
    const searchInTree = treeRegion?.contains(search) === true;
    const treeHidden = treeRegion?.hasAttribute('hidden') === true || treeRegion?.getClientRects().length === 0;
    return {
      hideGone: document.getElementById('file-explorer-hide-btn') == null && document.querySelector('.file-explorer__title-button') == null,
      titleTag: title?.tagName || '',
      titleText: (title?.textContent || '').trim(),
      actionsWrapsRefresh: actions?.contains(refresh) === true,
      treeInHeader: header?.contains(tree) === true,
      treeCount: document.querySelectorAll('#file-explorer-tree-toggle').length,
      treeRightOfSearch: !!(searchBox && treeBox && treeBox.x >= searchBox.x + searchBox.width - 1),
      searchMounted: search != null,
      searchVisible: searchBox != null,
      searchInTree,
      treeHidden,
      drawerEdgeGone: document.querySelector('.file-explorer__tree-edge') == null,
      treePressed: tree?.getAttribute('aria-pressed') === 'true',
      treeLabel: tree?.getAttribute('aria-label') || '',
      treeSvg: tree?.querySelector('svg') != null,
      treeText: (tree?.textContent || '').replace(/\s+/g, ' ').trim(),
      narrow: window.matchMedia('(max-width: 720px)').matches,
      treeInContent: document.querySelector('[data-testid="file-explorer-content"]')?.contains(tree) === true,
      treeInToolbar: document.querySelector('.file-explorer__content-toolbar')?.contains(tree) === true,
      surfacePresent: document.querySelector('.file-explorer__surface') != null,
      treeLeftOfSurface: (() => {
        const surface = document.querySelector('.file-explorer__surface');
        if (!surface || !treeBox) return null;
        return treeBox.x + treeBox.width <= surface.getBoundingClientRect().x + 1;
      })(),
      refreshLabel: refresh?.getAttribute('aria-label') || '',
      refreshTitle: refresh?.getAttribute('title') || '',
      refreshVisibleText: (refresh?.textContent || '').replace(refresh?.querySelector('.file-explorer__refresh-glyph')?.textContent || '', '').trim(),
      refreshGlyph: refresh?.querySelector('.file-explorer__refresh-glyph') != null,
      closeBtn: document.getElementById('file-explorer-close-btn') !== null,
      closeButtons: Array.from(document.querySelectorAll('button.file-explorer__close')).map((btn) => ({
        id: btn.id,
        title: btn.getAttribute('title') || '',
        label: btn.getAttribute('aria-label') || '',
        box: box(btn),
      })),
      separateHide: document.querySelector('.file-explorer__hide') !== null,
      hintCopy: (document.body.textContent || '').includes('Open a file from markdown or the workspace tree'),
      downloadBtn: document.querySelector('button.file-explorer__download') !== null,
      loadMore: Array.from(document.querySelectorAll('section.file-explorer button')).some((btn) => (btn.textContent || '').trim() === 'Load more'),
      orphans: document.querySelector('[data-orphans="true"]') !== null,
      filesOpen: layout?.classList.contains('app-layout--files-open') === true,
      togglePressed: document.getElementById('file-explorer-toggle-btn')?.getAttribute('aria-pressed') || '',
      resizerPresent: resizer !== null,
      resizerDisplay: resizer ? getComputedStyle(resizer).display : 'missing',
    };
  });
  if (!chrome.hideGone) fail('obsolete Files title close (hide-btn / title-button) is still mounted');
  if (chrome.titleTag !== 'H2' || chrome.titleText !== 'Files' || !chrome.actionsWrapsRefresh) {
    fail(`Files title must be a static h2 and Refresh must sit in actions (${JSON.stringify({ tag: chrome.titleTag, text: chrome.titleText, actions: chrome.actionsWrapsRefresh })})`);
  }
  const treeShared = !chrome.treeInHeader && chrome.treeCount === 1 && chrome.treeSvg;
  const treeExpanded = chrome.treeLabel === 'Collapse file tree' && chrome.treePressed === false && chrome.searchMounted && chrome.searchInTree && chrome.drawerEdgeGone && chrome.treeRightOfSearch;
  const treeCollapsed = chrome.treeLabel === 'Expand file tree' && chrome.treePressed === true && chrome.treeHidden && !chrome.searchVisible && chrome.drawerEdgeGone && chrome.treeInToolbar && (chrome.surfacePresent ? chrome.treeLeftOfSurface === true : true) && (chrome.narrow ? chrome.treeText.includes('Files') : !chrome.treeText.includes('Files'));
  if (!treeShared || (!treeExpanded && !treeCollapsed)) {
    fail(`tree toggle must be the one SVG control: right of Search in the tree toolbar while expanded, inside the content toolbar left of the surface while collapsed (${JSON.stringify({ count: chrome.treeCount, inHeader: chrome.treeInHeader, right: chrome.treeRightOfSearch, svg: chrome.treeSvg, label: chrome.treeLabel, pressed: chrome.treePressed, search: chrome.searchMounted, searchVisible: chrome.searchVisible, treeHidden: chrome.treeHidden, inTree: chrome.searchInTree, edgeGone: chrome.drawerEdgeGone, toolbar: chrome.treeInToolbar, left: chrome.treeLeftOfSurface, text: chrome.treeText, narrow: chrome.narrow })})`);
  }
  if (chrome.refreshLabel !== 'Refresh' || chrome.refreshTitle !== 'Refresh' || chrome.refreshVisibleText !== '' || !chrome.refreshGlyph) {
    fail(`Refresh must stay icon-only with aria/title Refresh (got ${JSON.stringify({ label: chrome.refreshLabel, title: chrome.refreshTitle, text: chrome.refreshVisibleText, glyph: chrome.refreshGlyph })})`);
  }
  if (chrome.separateHide) fail('old separate hide control (.file-explorer__hide) must be absent');
  if (chrome.hintCopy) fail('removed hint copy "Open a file from markdown…" must be absent');
  if (chrome.closeBtn) fail('#file-explorer-close-btn must be absent');
  if (chrome.closeButtons.length !== 1) {
    fail(`expected exactly one drawer close (button.file-explorer__close), got ${chrome.closeButtons.length}`);
  }
  const drawerX = chrome.closeButtons[0];
  if (drawerX.id !== 'file-explorer-drawer-close-btn'
    || drawerX.title !== 'Close file explorer' || drawerX.label !== 'Close file explorer') {
    fail(`drawer close is not the registered control: ${JSON.stringify(drawerX)}`);
  }
  if (chrome.loadMore) fail('File Explorer must not render a Load more button');
  if (chrome.orphans) fail('deleted/renamed paths must not render as data-orphans');
  if (!chrome.filesOpen) fail('.app-layout--files-open missing while Files is open');
  if (chrome.togglePressed !== 'true') fail('Files toggle aria-pressed is not true while open');
  await expandFilesTree(page);
  const query = `files-search-${Date.now()}`;
  const search = page.locator('[data-testid="file-explorer-search"]');
  await search.fill(query);
  const activeBefore = await page.evaluate(() => document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || '');
  const askBefore = await page.evaluate(() => document.querySelector('section.file-conversation')?.getAttribute('data-conversation-id') || '');
  await page.locator('#file-explorer-tree-toggle').click();
  await waitFor(
    page,
    () => document.getElementById('file-explorer-tree-toggle')?.getAttribute('aria-label') === 'Expand file tree'
      && document.querySelectorAll('#file-explorer-tree-toggle').length === 1
      && document.querySelector('.file-explorer__tree-edge') == null
      && document.querySelector('.file-explorer__content-toolbar')?.contains(document.getElementById('file-explorer-tree-toggle')) === true,
    'collapsing the tree did not leave exactly one Expand control in the content toolbar',
  );
  const collapsed = await page.evaluate(() => {
    const toggle = document.getElementById('file-explorer-tree-toggle');
    const toolbar = document.querySelector('.file-explorer__content-toolbar');
    const surface = toolbar?.querySelector('.file-explorer__surface');
    const box = toggle?.getBoundingClientRect();
    const narrow = window.matchMedia('(max-width: 720px)').matches;
    const search = document.querySelector('[data-testid="file-explorer-search"]');
    const treeRegion = document.querySelector('[data-testid="file-explorer-tree"]');
    const searchBox = search?.getClientRects().length ? search.getBoundingClientRect() : null;

    return {
      pressed: toggle?.getAttribute('aria-pressed') || '',
      reachable: !!box && box.width >= 24 && box.height >= 24,
      count: document.querySelectorAll('#file-explorer-tree-toggle').length,
      edgeGone: document.querySelector('.file-explorer__tree-edge') == null,
      searchNotVisible: search == null || search.closest('[hidden]') != null || searchBox == null || searchBox.width < 1 || searchBox.height < 1,
      treeHidden: treeRegion?.hasAttribute('hidden') === true || treeRegion?.getClientRects().length === 0,
      inToolbar: toolbar?.contains(toggle) === true,
      leftOfSurface: surface && surface.getClientRects().length > 0 ? !!(box && box.x + box.width <= surface.getBoundingClientRect().x + 1) : null,
      filesLabel: narrow ? (toggle?.textContent || '').includes('Files') : !(toggle?.textContent || '').includes('Files'),
    };
  });
  if (collapsed.pressed !== 'true' || !collapsed.reachable || collapsed.count !== 1 || !collapsed.edgeGone || !collapsed.searchNotVisible || !collapsed.treeHidden || !collapsed.inToolbar || collapsed.leftOfSurface === false || !collapsed.filesLabel) {
    fail(`collapsed tree reopen is not the one content-toolbar control: ${JSON.stringify(collapsed)}`);
  }
  await page.locator('#file-explorer-tree-toggle').click();
  await waitFor(
    page,
    () => document.getElementById('file-explorer-tree-toggle')?.getAttribute('aria-label') === 'Collapse file tree',
    'reopening the tree did not restore Collapse file tree',
  );
  const restored = await page.evaluate(() => ({
    query: document.querySelector('[data-testid="file-explorer-search"]')?.value || '',
    active: document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || '',
    ask: document.querySelector('section.file-conversation')?.getAttribute('data-conversation-id') || '',
    count: document.querySelectorAll('#file-explorer-tree-toggle').length,
  }));
  if (restored.query !== query || restored.active !== activeBefore || restored.ask !== askBefore || restored.count !== 1) {
    fail(`tree collapse/reopen changed search/file/Ask (${JSON.stringify({ query, activeBefore, askBefore, restored })})`);
  }
  await search.fill('');
  if (desktopResizer) {
    if (!chrome.resizerPresent) fail('#file-explorer-resizer missing on desktop');
  } else if (chrome.resizerDisplay !== 'none' && chrome.resizerDisplay !== 'missing') {
    fail(`#file-explorer-resizer must be hidden in overlay (display=${chrome.resizerDisplay})`);
  }
}

async function filesTreeStatus(page, filePath) {
  return page.evaluate((want) => {
    const rows = Array.from(document.querySelectorAll('button.file-explorer__row'));
    const row = rows.find((btn) => {
      const title = btn.getAttribute('title') || '';
      const name = (btn.querySelector('.file-explorer__row-path')?.textContent || '').trim();
      return title === want || title.endsWith(`/${want}`) || name === want.split('/').pop();
    });
    if (!row) return null;
    const item = row.closest('.file-explorer__item') || row.parentElement;
    const status = item?.querySelector('button.file-explorer__status');
    return {
      title: row.getAttribute('title') || '',
      name: (row.querySelector('.file-explorer__row-path')?.textContent || '').trim(),
      gitOnly: row.getAttribute('data-git-only') === 'true',
      status: status?.getAttribute('data-status') || '',
      letter: (status?.textContent || '').trim(),
      statusTitle: status?.getAttribute('title') || '',
      active: row.classList.contains('file-explorer__row--active'),
    };
  }, filePath);
}

/** Trailing status column contract (P1 false-pass guard): a changed row's
 *  FILENAME x-position must equal a TRUE clean sibling's at the same depth —
 *  a committed, tracked, never-modified root file that carries NO status chip
 *  at all (comparing two chip-carrying rows cannot detect chip-induced
 *  indent/overlap, which is exactly how the old notes.md comparison
 *  false-passed). Additionally: the filename TEXT right edge must not overlap
 *  the trailing M/A/D/R chip; elementFromPoint at the filename center
 *  resolves the row button and at the chip center resolves the status
 *  button; and clicking the chip opens EXACTLY that file's diff (exactly one
 *  file_explorer_diff frame with the exact path, rendered diff carries this
 *  file's changed lines and no sibling's). `chip44` additionally asserts the
 *  mobile 44x44 touch target. */
async function assertFilesStatusColumn(page, sentFrames, label, { chip44 = false } = {}) {
  // Narrow viewports collapse the tree once a file is open — expand it so the
  // dirty/clean rows and the status chip are real, hittable geometry again.
  await expandFilesTree(page);
  const diffFramesBefore = filesWsOf(sentFrames, 'file_explorer_diff', dirtyFile).length;
  const allDiffBefore = filesWsOf(sentFrames, 'file_explorer_diff').length;
  const geo = await page.evaluate(({ dirty, clean }) => {
    const findRow = (want) => Array.from(document.querySelectorAll('button.file-explorer__row')).find((btn) => {
      const title = btn.getAttribute('title') || '';
      const name = (btn.querySelector('.file-explorer__row-path')?.textContent || '').trim();
      return title === want || title.endsWith(`/${want}`) || name === want.split('/').pop();
    });
    const dirtyRow = findRow(dirty);
    const cleanRow = findRow(clean);
    if (!dirtyRow || !cleanRow) return { missing: { dirty: !dirtyRow, clean: !cleanRow } };
    const status = dirtyRow.closest('.file-explorer__item')?.querySelector('button.file-explorer__status');
    if (!status) return { missingChip: true };
    // True-clean guard: the sibling must carry NO status chip — it must be a
    // committed, tracked, never-modified file, not another dirty row.
    const cleanStatus = cleanRow.closest('.file-explorer__item')?.querySelector('button.file-explorer__status');
    const inView = (box) => box.width > 0 && box.height > 0
      && box.top >= 0 && box.left >= 0
      && box.bottom <= window.innerHeight && box.right <= window.innerWidth;
    dirtyRow.scrollIntoView({ block: 'center' });
    if (!inView(cleanRow.getBoundingClientRect())) cleanRow.scrollIntoView({ block: 'center' });
    if (!inView(dirtyRow.getBoundingClientRect()) || !inView(cleanRow.getBoundingClientRect())) {
      return { offscreen: true };
    }
    // Filename TEXT bounds (not the stretching span) via a Range so the
    // overlap assertion measures the rendered filename, not the flex filler.
    const textRect = (row) => {
      const el = row.querySelector('.file-explorer__row-path');
      if (!el) return null;
      const range = document.createRange();
      range.selectNodeContents(el);
      return range.getBoundingClientRect();
    };
    const dirtyText = textRect(dirtyRow);
    const cleanPath = cleanRow.querySelector('.file-explorer__row-path')?.getBoundingClientRect();
    if (!dirtyText || !cleanPath) return { missingPath: true };
    const statusBox = status.getBoundingClientRect();
    const hitAt = (box) => document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
    const nameHit = hitAt(dirtyText);
    const chipHit = hitAt(statusBox);
    return {
      dirtyX: dirtyText.left,
      cleanX: cleanPath.left,
      dirtyNameRight: dirtyText.right,
      statusLeft: statusBox.left,
      statusRight: statusBox.right,
      statusW: statusBox.width,
      statusH: statusBox.height,
      rowRight: dirtyRow.getBoundingClientRect().right,
      cleanStatusPresent: cleanStatus != null,
      chipStatus: status.getAttribute('data-status') || '',
      depthDirty: dirtyRow.closest('ul')?.getAttribute('data-depth') || '',
      depthClean: cleanRow.closest('ul')?.getAttribute('data-depth') || '',
      nameHitRow: nameHit?.closest('button.file-explorer__row') === dirtyRow,
      chipHitStatus: chipHit?.closest('button.file-explorer__status') === status,
    };
  }, { dirty: dirtyFile, clean: cleanFile });
  if (geo.missing) {
    fail(`${label}: status-column geometry rows missing (dirty=${dirtyFile} clean=${cleanFile} missing=${JSON.stringify(geo.missing)})`);
  }
  if (geo.offscreen) fail(`${label}: dirty/clean rows could not be scrolled into view for hit testing`);
  if (geo.missingChip) fail(`${label}: modified row ${dirtyFile} has no status chip`);
  if (geo.missingPath) fail(`${label}: compared rows are missing their .file-explorer__row-path`);
  if (geo.cleanStatusPresent) {
    fail(`${label}: clean sibling ${cleanFile} carries a status chip — the fixture sibling must be committed, tracked and never modified (false-pass guard)`);
  }
  if (geo.chipStatus !== 'modified') fail(`${label}: dirty row chip is not modified (data-status=${JSON.stringify(geo.chipStatus)})`);
  if (geo.depthDirty !== geo.depthClean) fail(`${label}: compared rows are not at the same tree depth (${geo.depthDirty} vs ${geo.depthClean})`);
  if (Math.abs(geo.dirtyX - geo.cleanX) > 1) {
    fail(`${label}: modified filename x ${geo.dirtyX.toFixed(1)} != TRUE clean sibling x ${geo.cleanX.toFixed(1)} — status must not indent rows`);
  }
  if (geo.dirtyNameRight > geo.statusLeft) {
    fail(`${label}: filename right edge ${geo.dirtyNameRight.toFixed(1)} overlaps status chip left ${geo.statusLeft.toFixed(1)} — chip must own a reserved trailing column`);
  }
  if (Math.abs(geo.statusRight - geo.rowRight) > 8) {
    fail(`${label}: status chip is not in the trailing column (chip right ${geo.statusRight.toFixed(1)} vs row right ${geo.rowRight.toFixed(1)})`);
  }
  if (!geo.nameHitRow) {
    fail(`${label}: elementFromPoint at the filename center does not resolve the row button — the chip (or another layer) covers the filename`);
  }
  if (!geo.chipHitStatus) {
    fail(`${label}: elementFromPoint at the chip center does not resolve the status button`);
  }
  if (chip44 && (geo.statusW < 44 || geo.statusH < 44)) {
    fail(`${label}: mobile status chip is ${geo.statusW.toFixed(0)}x${geo.statusH.toFixed(0)}, need >=44x44`);
  }
  // Clicking the chip opens EXACTLY this file's diff: exactly one new
  // file_explorer_diff frame with the exact path (and no diff frame for any
  // other path), and the rendered diff carries this file's changed lines and
  // no sibling file's content.
  await page.locator('li.file-explorer__item')
    .filter({ has: page.locator(`button.file-explorer__row[title="${dirtyFile}"]`) })
    .locator('button.file-explorer__status')
    .click();
  await waitFor(
    page,
    () => document.querySelector('.file-explorer__code[data-view="diff"]') !== null,
    `status chip click did not open the ${dirtyFile} diff view`,
  );
  const diffText = await page.evaluate(() => {
    const pane = document.querySelector('.file-explorer__code[data-view="diff"]');
    return pane ? (pane.textContent || '') : null;
  });
  if (diffText == null) fail(`${label}: diff view did not render after status chip click`);
  const gitHeaders = diffText.split('\n').filter((line) => line.startsWith('diff --git'));
  if (gitHeaders.length > 0 && gitHeaders.some((line) => !line.includes(dirtyFile))) {
    fail(`${label}: rendered diff header names another file (${JSON.stringify(gitHeaders)})`);
  }
  if (!diffText.includes('-beta') || !diffText.includes('+BETA')) {
    fail(`${label}: rendered diff does not carry ${dirtyFile}'s changed lines`);
  }
  if (diffText.includes('+DEEP-TWO') || diffText.includes('changed-line-000001')) {
    fail(`${label}: rendered diff is not EXACTLY ${dirtyFile} — sibling diff content leaked into the view`);
  }
  const diffFramesAfter = filesWsOf(sentFrames, 'file_explorer_diff', dirtyFile).length;
  const allDiffAfter = filesWsOf(sentFrames, 'file_explorer_diff').length;
  const expectedRequests = diffFramesBefore === 0 ? 1 : 0;
  if (diffFramesAfter !== diffFramesBefore + expectedRequests || allDiffAfter !== allDiffBefore + expectedRequests) {
    fail(`${label}: status chip diff requests disagree with the already-loaded file cache`);
  }
}

async function filesSnapshot(page) {
  return page.evaluate(() => {
    const active = document.querySelector('button.file-explorer__row--active');
    const dirs = {};
    for (const dir of document.querySelectorAll('button.file-explorer__row--dir')) {
      dirs[dir.getAttribute('title') || ''] = dir.getAttribute('aria-expanded');
    }
    const divider = document.querySelector('[data-testid="file-explorer-divider"]');
    const tree = document.querySelector('[data-testid="file-explorer-tree"] .file-explorer__tree-list');
    const code = document.querySelector('.file-explorer__code');
    const markdown = document.querySelector('.file-explorer__markdown');
    const content = code || markdown;
    const layout = document.querySelector('[data-testid="file-explorer-layout"]');
    return {
      activeTitle: active?.getAttribute('title') || null,
      activeText: (active?.textContent || '').trim() || null,
      dirs,
      dividerNow: divider?.getAttribute('aria-valuenow') || null,
      treePct: layout ? getComputedStyle(layout).getPropertyValue('--file-explorer-tree-pct').trim() : '',
      treeScroll: tree?.scrollTop ?? null,
      contentScroll: content?.scrollTop ?? null,
      view: content?.getAttribute('data-view') || null,
    };
  });
}

async function mutateFilesGeometry(page) {
  const divider = page.locator('[data-testid="file-explorer-divider"]');
  await waitFor(
    page,
    () => {
      const el = document.querySelector('[data-testid="file-explorer-divider"]');
      if (!el) return false;
      const box = el.getBoundingClientRect();
      return box.width > 0 && box.height > 0 && el.getAttribute('aria-orientation') === 'vertical';
    },
    'File Explorer divider is not a hittable vertical separator'
  );
  await hitTestSelector(page, '[data-testid="file-explorer-divider"]', 'tree/content divider');
  const before = await page.evaluate(() => {
    const el = document.querySelector('[data-testid="file-explorer-divider"]');
    return Number(el?.getAttribute('aria-valuenow') || '0');
  });
  await divider.click();
  await page.keyboard.press('ArrowRight');
  await page.keyboard.press('ArrowRight');
  await waitFor(
    page,
    (start) => {
      const now = Number(document.querySelector('[data-testid="file-explorer-divider"]')?.getAttribute('aria-valuenow') || '0');
      const pct = getComputedStyle(document.querySelector('[data-testid="file-explorer-layout"]')).getPropertyValue('--file-explorer-tree-pct').trim();
      return Number.isFinite(now) && now !== start && now !== 34 && pct !== '34%' && pct !== '';
    },
    'divider keyboard move did not leave the default 34% tree split',
    25000,
    before
  );

  await waitFor(
    page,
    () => {
      const tree = document.querySelector('[data-testid="file-explorer-tree"] .file-explorer__tree-list');
      const content = document.querySelector('.file-explorer__markdown, .file-explorer__code');
      return tree != null && content != null && tree.scrollHeight > tree.clientHeight + 8 && content.scrollHeight > content.clientHeight + 8;
    },
    'tree or content viewport is not overflow-scrollable for a nonzero S0'
  );
  await page.locator('[data-testid="file-explorer-tree"] .file-explorer__tree-list').evaluate((el) => {
    const max = el.scrollHeight - el.clientHeight;
    el.scrollTop = Math.min(80, Math.max(24, max));
    el.dispatchEvent(new Event('scroll', { bubbles: true }));
  });
  await page.locator('.file-explorer__markdown, .file-explorer__code').first().evaluate((el) => {
    const max = el.scrollHeight - el.clientHeight;
    el.scrollTop = Math.min(160, Math.max(48, max));
    el.dispatchEvent(new Event('scroll', { bubbles: true }));
  });
  await waitFor(
    page,
    () => {
      const tree = document.querySelector('[data-testid="file-explorer-tree"] .file-explorer__tree-list');
      const content = document.querySelector('.file-explorer__markdown, .file-explorer__code');
      const now = Number(document.querySelector('[data-testid="file-explorer-divider"]')?.getAttribute('aria-valuenow') || '0');
      const pct = getComputedStyle(document.querySelector('[data-testid="file-explorer-layout"]')).getPropertyValue('--file-explorer-tree-pct').trim();
      return tree != null && content != null
        && tree.scrollTop > 0
        && content.scrollTop > 0
        && Number.isFinite(now) && now !== 34
        && pct !== '34%' && pct !== '';
    },
    'S0 treePct/treeScroll/contentScroll did not become non-default/nonzero'
  );
}



async function assertSendReclaimed(page, label) {
  const proof = await page.evaluate(() => {
    const leftover = document.querySelectorAll('.file-explorer, #file-explorer-resizer, #file-explorer-close-btn').length;
    const send = document.getElementById('send-btn');
    const layout = document.querySelector('.app-layout');
    if (!send) return { leftover, filesOpen: layout?.classList.contains('app-layout--files-open') === true, sendHit: false, covered: true };
    const box = send.getBoundingClientRect();
    const top = document.elementFromPoint(box.x + box.width / 2, box.y + box.height / 2);
    const former = document.elementFromPoint(window.innerWidth - 220, 120);
    const transcript = document.getElementById('transcript');
    const tbox = transcript?.getBoundingClientRect();
    const probe = tbox && tbox.width > 0
      ? document.elementFromPoint(tbox.right - 10, tbox.top + Math.min(120, tbox.height / 2))
      : null;
    const main = document.querySelector('.app-main');
    return {
      leftover,
      filesOpen: layout?.classList.contains('app-layout--files-open') === true,
      sendHit: top?.closest('#send-btn') != null,
      formerIsFiles: former?.closest('.file-explorer, #file-explorer-resizer, #file-explorer-close-btn') != null,
      togglePressed: document.getElementById('file-explorer-toggle-btn')?.getAttribute('aria-pressed') || '',
      toggleLabel: document.getElementById('file-explorer-toggle-btn')?.getAttribute('aria-label') || '',
      transcriptWidth: tbox ? Math.round(tbox.width) : null,
      transcriptHit: probe?.closest('#transcript') != null,
      // Desktop reservation (.app-layout--files-open/collapsed .app-main) must
      // be gone once the drawer fully closes; mobile/overlay pads 0 anyway.
      mainPaddingRight: main && window.innerWidth >= 936 ? getComputedStyle(main).paddingRight : 'n/a',
    };
  });
  if (proof.leftover !== 0) fail(`${label}: leftover Files nodes remain (${proof.leftover})`);
  if (proof.filesOpen) fail(`${label}: .app-layout--files-open still present`);
  if (proof.togglePressed !== 'false') fail(`${label}: Files toggle still pressed`);
  if (proof.toggleLabel !== 'Open file explorer') fail(`${label}: Files toggle aria-label is not Open file explorer`);
  if (!proof.sendHit) fail(`${label}: Send is not the top element at its center`);
  if (proof.formerIsFiles) fail(`${label}: former drawer center still hits a Files node`);
  if (proof.transcriptWidth == null) fail(`${label}: #transcript missing after Files close`);
  if (!proof.transcriptHit) fail(`${label}: transcript hitbox not restored after drawer close`);
  if (proof.mainPaddingRight !== 'n/a' && proof.mainPaddingRight !== '0px') {
    fail(`${label}: .app-main still reserves drawer width after Files close (padding-right=${proof.mainPaddingRight})`);
  }
  await ordinaryClick(page, '#send-btn', `${label}: Send after hide`);
}

async function hideFileExplorer(page, label) {
  if ((await page.locator('section.file-explorer').count()) === 0) return;
  await ordinaryClick(page, FILES_DRAWER_CLOSE, `${label}: Close file explorer`);
  await waitFor(
    page,
    () => document.querySelector('section.file-explorer') === null,
    `${label}: File explorer section did not detach`
  );
  await assertSendReclaimed(page, label);
}

async function scrollFilesContent(page, kind) {
  const selector = kind === 'markdown' ? '.file-explorer__markdown' : '.file-explorer__code';
  await page.locator(selector).evaluate((el) => {
    el.scrollTop = el.scrollHeight;
  });
}


function assertPageCursors(frames, filePath, label) {
  const reads = filesPageRequests(frames, filePath);
  const pages = reads.filter((frame) => frame.maxLines === 400);
  const probes = reads.filter((frame) => frame.maxLines === 1);
  if (pages.length < 1) fail(`${label}: no 400-line file page for ${filePath}`);
  const first = pages[0];
  if (first.cursor != null && first.cursor !== 0) {
    fail(`${label}: first page cursor must be omitted or 0 (got ${first.cursor})`);
  }
  const seen = new Set();
  let lastCursor = -1;
  for (const frame of pages) {
    const cursor = frame.cursor ?? 0;
    const key = `${frame.type}:${frame.path}:${cursor}`;
    if (seen.has(key)) fail(`${label}: duplicate in-flight page ${key}`);
    seen.add(key);
    if (cursor < lastCursor) fail(`${label}: page cursors must not go backwards`);
    lastCursor = cursor;
  }
  for (const probe of probes) {
    const cursor = probe.cursor ?? 0;
    if (!Number.isInteger(cursor) || cursor < 0) {
      fail(`${label}: line probe cursor is not a window start (${JSON.stringify(probe)})`);
    }
  }
  return pages;
}

async function pageLongFile(page, sentFrames, filePath, kind, expectedTotal) {
  // Scope every wire measurement to THIS open-file journey: sentFrames spans
  // the whole page lifetime, and an earlier settled lifetime (e.g. the readme
  // preview deep link into notes.md:40) already spent cursor 0, so re-opening
  // the row legitimately requests cursor 0 again. Only a repeated cursor
  // inside one journey is a real duplicate in-flight page.
  const journeyMark = sentFrames.length;
  await clickFilesRow(page, filePath);
  const pane = kind === 'markdown' ? '.file-explorer__markdown' : '.file-explorer__code';
  await waitFor(
    page,
    (sel) => document.querySelector(sel) !== null,
    `${filePath} content pane did not render`,
    25000,
    pane
  );
  if (kind === 'code') {
    const lineOne = await page.evaluate(() => document.getElementById('file-explorer-line-1') !== null);
    if (!lineOne) fail(`${filePath} missing #file-explorer-line-1`);
  } else {
    const ghost = await page.evaluate(() => document.querySelectorAll('.file-explorer__line-no').length);
    if (ghost !== 0) fail(`${filePath} markdown wrap created ghost line numbers (${ghost})`);
  }
  const loadMore = await page.evaluate(() =>
    Array.from(document.querySelectorAll('section.file-explorer button')).some((btn) => (btn.textContent || '').trim() === 'Load more')
  );
  if (loadMore) fail(`${filePath} still shows Load more after infinite-scroll cutover`);
  if (kind === 'code') {
    const firstWindow = await page.evaluate(() => document.querySelectorAll('.file-explorer__code .file-explorer__line').length);
    if (firstWindow > 400) fail(`${filePath} first window loaded ${firstWindow} lines (cap 400)`);
  }
  const active = await page.evaluate((want) => {
    const rows = Array.from(document.querySelectorAll('button.file-explorer__row--active'));
    return rows.length === 1 && ((rows[0].getAttribute('title') || '') === want || (rows[0].getAttribute('title') || '').endsWith(`/${want}`));
  }, filePath);
  if (!active) fail(`${filePath} is not the only active row`);
  // The loaded/total footer is gone: paging progress is the loaded DOM
  // window (code lines / markdown text length) plus the page requests
  // observed on the wire.
  const target = kind === 'code' ? Math.min(800, expectedTotal ?? 800) : null;
  const loadedNow = () => page.evaluate((k) => {
    if (k === 'code') return document.querySelectorAll('.file-explorer__code .file-explorer__line').length;
    return (document.querySelector('.file-explorer__markdown')?.textContent || '').length;
  }, kind);
  const readCount = () => filesPageRequests(sentFrames.slice(journeyMark), filePath).length;
  if (readCount() < 1) fail(`${filePath}: no initial page request on the wire`);
  let loaded = await loadedNow();
  let pages = 0;
  while (kind === 'code' ? loaded < target : pages < 1) {
    const prevReads = readCount();
    const prevLoaded = loaded;
    await scrollFilesContent(page, kind);
    const deadline = Date.now() + 25000;
    for (;;) {
      loaded = await loadedNow();
      if (loaded > prevLoaded && readCount() > prevReads) break;
      if (Date.now() > deadline) fail(`${filePath} infinite scroll did not load the next page`);
      await page.waitForTimeout(150);
    }
    pages += 1;
    if (kind === 'code') {
      const nextLine = await page.evaluate((n) => document.getElementById(`file-explorer-line-${n}`) !== null, prevLoaded + 1);
      if (!nextLine) fail(`${filePath} missing #file-explorer-line-${prevLoaded + 1} after page ${pages}`);
    }
    if (pages > 8) fail(`${filePath} infinite scroll did not reach ${target} after 8 pages`);
  }
  const reads = assertPageCursors(sentFrames.slice(journeyMark), filePath, filePath);
  if (reads.length < 2) fail(`${filePath}: expected at least two settled pages, got ${reads.length}`);
  return { reads };
}

async function filesSurfaceProof(page) {
  return page.evaluate(() => {
    const head = document.querySelector('[data-testid="file-explorer-header"]');
    const content = document.querySelector('[data-testid="file-explorer-content"]');
    const tree = document.querySelector('[data-testid="file-explorer-tree"]');
    const surface = document.querySelector('.file-explorer__surface');
    const checked = surface?.querySelector('[aria-checked="true"]');
    const treeTop = tree?.getBoundingClientRect().top ?? -1;
    const contentTop = content?.getBoundingClientRect().top ?? -1;
    const surfaceBox = surface?.getBoundingClientRect();
    const contentBox = content?.getBoundingClientRect();
    return {
      surfaceInHead: !!head?.querySelector('.file-explorer__surface, [aria-label="File surface"]'),
      surfaceInContent: !!content?.contains(surface),
      surfaceAboveContentBody: !!(surfaceBox && contentBox && surfaceBox.top <= contentBox.top + 2),
      checked: (checked?.textContent || '').replace(/\s+/g, ' ').trim(),
      label: surface?.getAttribute('aria-label') || '',
      markdown: !!document.querySelector('.file-explorer__markdown'),
      code: !!document.querySelector('pre.file-explorer__code'),
      htmlFrame: !!document.querySelector('iframe.file-explorer__html-preview-frame'),
      htmlSandbox: document.querySelector('iframe.file-explorer__html-preview-frame')?.getAttribute('sandbox') ?? null,
      treeContentTopDelta: Math.abs(treeTop - contentTop),
      pwned: window.__fePwned ?? null,
    };
  });
}

async function assertMarkdownPreviewToggle(page) {
  const preview = await filesSurfaceProof(page);
  if (preview.surfaceInHead) fail('Source/Preview must not live in the Files header');
  if (!preview.surfaceInContent || preview.label !== 'File surface') {
    fail(`Source/Preview must be the content toolbar (${JSON.stringify(preview)})`);
  }
  if (preview.treeContentTopDelta > 2) {
    fail(`tree/content top edges desynced by ${preview.treeContentTopDelta}px`);
  }
  if (preview.checked !== 'Preview') fail(`markdown default surface must be Preview, got ${JSON.stringify(preview.checked)}`);
  if (!preview.markdown || preview.code) fail('markdown Preview must render MarkdownBody without line numbers');
  await page.locator('.file-explorer__surface-btn', { hasText: 'Source' }).click();
  await waitFor(
    page,
    () => document.querySelector('pre.file-explorer__code') !== null
      && document.querySelector('.file-explorer__markdown') === null,
    'Source did not show line-numbered markdown'
  );
  const source = await filesSurfaceProof(page);
  if (source.checked !== 'Source') fail(`Source radio did not activate, got ${JSON.stringify(source.checked)}`);
  if (source.surfaceInHead || !source.surfaceInContent) fail('Source toggle left the content toolbar');
  if (!source.code || source.markdown) fail('Source must show line-numbered code, not MarkdownBody');
  await page.locator('.file-explorer__surface-btn', { hasText: 'Preview' }).click();
  await waitFor(
    page,
    () => document.querySelector('.file-explorer__markdown') !== null
      && document.querySelector('pre.file-explorer__code') === null,
    'Preview did not restore MarkdownBody'
  );
}

async function assertHtmlPreview(page) {
  await clickFilesRow(page, htmlFile);
  await waitFor(
    page,
    () => document.querySelector('iframe.file-explorer__html-preview-frame') !== null,
    `${htmlFile} HTML preview iframe did not render`,
    25000
  );
  const html = await filesSurfaceProof(page);
  if (html.surfaceInHead) fail('HTML Source/Preview must not live in the Files header');
  if (!html.surfaceInContent) fail('HTML Source/Preview must sit on the content toolbar');
  if (html.checked !== 'Preview') fail(`HTML default surface must be Preview, got ${JSON.stringify(html.checked)}`);
  if (html.htmlSandbox !== '') fail(`HTML preview iframe sandbox must be empty, got ${JSON.stringify(html.htmlSandbox)}`);
  if (html.pwned != null) fail(`HTML preview executed script (__fePwned=${html.pwned})`);
  if (html.treeContentTopDelta > 2) {
    fail(`HTML preview tree/content top edges desynced by ${html.treeContentTopDelta}px`);
  }
}

// Committed clean Solidity fixture (commands_review.sh $SOL_FILE; kept in
// lockstep). The Source surface must lex it through a registered Solidity
// grammar: specific keyword tokens must render as .hljs-keyword spans and
// the rendered line texts must equal the seeded source line-for-line — a
// plain unhighlighted render (no .sol mapping/grammar) keeps the text intact
// but fails the keyword tokens.
const SOLIDITY_SOURCE_LINES = [
  '// SPDX-License-Identifier: MIT',
  'pragma solidity ^0.8.20;',
  '',
  'contract TokenStore {',
  '    uint256 private totalSupply;',
  '',
  '    function mint(uint256 amount) public {',
  '        totalSupply += amount;',
  '    }',
  '}',
];

async function assertSolidityPreview(page, label) {
  await clickFilesRow(page, solFile);
  await waitFor(
    page,
    () => document.querySelector('pre.file-explorer__code') !== null
      && document.querySelector('.file-explorer__markdown') === null,
    `${label}: ${solFile} Source code pane did not render`,
    25000
  );
  await waitFor(
    page,
    () => document.getElementById('file-explorer-line-1') !== null,
    `${label}: ${solFile} first source line missing`,
    25000
  );
  const proof = await page.evaluate(() => {
    const pre = document.querySelector('pre.file-explorer__code');
    return {
      lines: Array.from(pre.querySelectorAll('.file-explorer__line-text')).map((el) => el.textContent || ''),
      spanCount: pre.querySelectorAll('.file-explorer__line-text span[class^="hljs-"]').length,
      keywords: Array.from(new Set(Array.from(pre.querySelectorAll('.file-explorer__line-text span.hljs-keyword'))
        .map((el) => (el.textContent || '').trim()))),
    };
  });
  if (JSON.stringify(proof.lines) !== JSON.stringify(SOLIDITY_SOURCE_LINES)) {
    fail(`${label}: ${solFile} rendered source is not intact line-for-line (got ${JSON.stringify(proof.lines)})`);
  }
  for (const token of ['contract', 'function']) {
    if (!proof.keywords.includes(token)) {
      fail(`${label}: ${solFile} missing hljs-keyword token "${token}" (keywords=${JSON.stringify(proof.keywords)}, hljs spans=${proof.spanCount})`);
    }
  }
  const row = page.locator('button.file-explorer__row').filter({ hasText: solFile.split('/').pop() }).first();
  if (!(await row.isVisible())) await expandFilesTree(page);
  await row.hover();
  const tree = await page.evaluate((want) => {
    const file = Array.from(document.querySelectorAll('button.file-explorer__row')).find((btn) => (btn.getAttribute('title') || '') === want);
    const dir = document.querySelector('button.file-explorer__row--dir');
    const probe = document.createElement('span');
    probe.style.color = 'var(--accent)';
    document.body.appendChild(probe);
    const accent = getComputedStyle(probe).color;
    probe.remove();
    const fileStyle = file ? getComputedStyle(file) : null;
    const dirStyle = dir ? getComputedStyle(dir) : null;
    const hovered = document.querySelector('button.file-explorer__row:hover');
    const hoverStyle = hovered ? getComputedStyle(hovered) : null;
    const iconBox = (node) => {
      const icon = node?.querySelector('svg[data-icon]');
      if (!icon) return null;
      const box = icon.getBoundingClientRect();
      const paths = Array.from(icon.querySelectorAll('path')).map((path) => path.getAttribute('d') || '');
      return {
        icon: icon.getAttribute('data-icon') || '',
        text: (icon.textContent || '').trim(),
        width: Math.round(box.width),
        height: Math.round(box.height),
        paths,
      };
    };
    const fileIcon = iconBox(file);
    const dirIcon = iconBox(dir);
    return {
      found: file != null,
      fileIcon,
      solidity: file?.classList.contains('file-explorer__row--solidity') === true,
      active: file?.classList.contains('file-explorer__row--active') === true,
      accentMatch: fileStyle != null && accent !== '' && fileStyle.color === accent,
      hoverIsFile: hovered === file,
      hoverKeepsAccent: hoverStyle != null && hoverStyle.color === accent,
      hoverBackground: hoverStyle?.backgroundColor || '',
      dirDistinct: dirStyle != null && fileStyle != null && dirStyle.color !== fileStyle.color
        && dir.classList.contains('file-explorer__row--dir') && !dir.classList.contains('file-explorer__row--solidity'),
      dirIcon,
    };
  }, solFile);
  const sized = (icon) => icon && icon.text === '' && icon.width > 0 && icon.height > 0 && icon.paths.length > 0 && icon.paths.every((d) => d.length > 0);
  const sameSlot = tree.fileIcon && tree.dirIcon && tree.fileIcon.width === tree.dirIcon.width && tree.fileIcon.height === tree.dirIcon.height;
  const pathsDiffer = tree.fileIcon && tree.dirIcon && JSON.stringify(tree.fileIcon.paths) !== JSON.stringify(tree.dirIcon.paths);
  const fileVisible = sized(tree.fileIcon) && tree.fileIcon.icon === 'solidity';
  const dirVisible = sized(tree.dirIcon) && tree.dirIcon.icon === 'dir' && sameSlot && pathsDiffer;
  if (!tree.found || !fileVisible || !tree.solidity || !tree.active || !tree.accentMatch
    || !tree.hoverIsFile || !tree.hoverKeepsAccent || tree.hoverBackground === '') {
    fail(`${label}: ${solFile} tree row lost its accent solidity SVG under selection/hover (${JSON.stringify(tree)})`);
  }
  if (!tree.dirDistinct || !dirVisible) {
    fail(`${label}: a directory row is not a distinct dir SVG from the solidity file (${JSON.stringify(tree)})`);
  }
  if (!(await page.locator('pre.file-explorer__code').isVisible())) await clickFilesRow(page, solFile);
}

// src/ReviewToken.sol REVIEW-diff expectation (commands_review.sh
// $SOL_REVIEW_FILE seeding, kept in lockstep): the committed baseline below
// differs from the working tree — the review rail's dirty .sol row.
// The shared Solidity grammar must lex BOTH sides — keyword/type tokens over
// exact preserved line text; the highlighted renderer must never mangle a
// line. The Explorer's clean Token.sol preview fixture (SOLIDITY_SOURCE_LINES
// is its committed AND working-tree content) is separate and stays untouched.
const SOLIDITY_BASELINE_LINES = [
  '// SPDX-License-Identifier: MIT',
  'pragma solidity ^0.8.19;',
  '',
  'contract OldToken {',
  '    uint256 private supply;',
  '',
  '    function burn(uint256 amount) public {',
  '        supply -= amount;',
  '    }',
  '}',
];
const SOLIDITY_REVIEW_DELETED_LINES = SOLIDITY_BASELINE_LINES.filter(
  (line) => !SOLIDITY_SOURCE_LINES.includes(line)
);
const SOLIDITY_REVIEW_ADDED_LINES = SOLIDITY_SOURCE_LINES.filter(
  (line) => !SOLIDITY_BASELINE_LINES.includes(line)
);

/** Diff-surface proof for the SELECTED review file: rendered line kinds +
 *  exact texts plus the Solidity token sets the shared grammar must emit. */
async function reviewSolidityProof(page) {
  return page.evaluate(() => {
    const lines = Array.from(document.querySelectorAll('.code-review__line')).map((line) => ({
      kind: (Array.from(line.classList).find((c) => c.startsWith('code-review__line--')) || '')
        .slice('code-review__line--'.length),
      text: line.querySelector('.code-review__line-text')?.textContent || '',
    }));
    const spansIn = (kind) => document
      .querySelectorAll(`.code-review__line--${kind} span[class^="hljs-"]`).length;
    const tokensIn = (selector) => Array.from(new Set(
      Array.from(document.querySelectorAll(selector)).map((el) => (el.textContent || '').trim())
    ));
    return {
      lines,
      keywords: tokensIn('.code-review__line .hljs-keyword'),
      types: tokensIn('.code-review__line .hljs-type'),
      deletionSpans: spansIn('deletion'),
      additionSpans: spansIn('addition'),
      spanCount: document.querySelectorAll('.code-review__line span[class^="hljs-"]').length,
    };
  });
}

/** Review .sol diff contract: exact baseline/worktree line text on the right
 *  sides, no invented text, and contract/function/type tokens lexed on both
 *  changed sides (never a plain unhighlighted render). */
function assertSolidityReviewDiff(proof, label) {
  if (proof.lines.length === 0) {
    fail(`${label}: ${solReviewFile} review diff rendered no lines`);
  }
  const invented = proof.lines
    .map((line) => line.text)
    .filter((text) => !SOLIDITY_BASELINE_LINES.includes(text) && !SOLIDITY_SOURCE_LINES.includes(text));
  if (invented.length) {
    fail(`${label}: ${solReviewFile} review diff rendered text outside the seeded baseline/worktree sources (${JSON.stringify(invented)})`);
  }
  const deletions = proof.lines.filter((line) => line.kind === 'deletion').map((line) => line.text);
  const additions = proof.lines.filter((line) => line.kind === 'addition').map((line) => line.text);
  for (const want of SOLIDITY_REVIEW_DELETED_LINES) {
    if (!deletions.includes(want)) {
      fail(`${label}: ${solReviewFile} review diff missing the exact baseline deletion line ${JSON.stringify(want)} (deletions=${JSON.stringify(deletions)})`);
    }
  }
  for (const want of SOLIDITY_REVIEW_ADDED_LINES) {
    if (!additions.includes(want)) {
      fail(`${label}: ${solReviewFile} review diff missing the exact working-tree addition line ${JSON.stringify(want)} (additions=${JSON.stringify(additions)})`);
    }
  }
  for (const token of ['contract', 'function']) {
    if (!proof.keywords.includes(token)) {
      fail(`${label}: ${solReviewFile} review diff missing hljs-keyword token "${token}" (keywords=${JSON.stringify(proof.keywords)}, hljs spans=${proof.spanCount})`);
    }
  }
  if (!proof.types.includes('uint256')) {
    fail(`${label}: ${solReviewFile} review diff missing the hljs-type token "uint256" (types=${JSON.stringify(proof.types)}, hljs spans=${proof.spanCount})`);
  }
  if (proof.deletionSpans === 0 || proof.additionSpans === 0) {
    fail(`${label}: ${solReviewFile} review diff did not lex BOTH changed sides (deletion spans=${proof.deletionSpans}, addition spans=${proof.additionSpans})`);
  }
}

async function runFileExplorerPreviewJourney(page) {
  await page.setViewportSize({ width: 1360, height: 900 });
  await waitFor(page, () => window.innerWidth === 1360 && window.innerHeight === 900, 'desktop viewport did not become 1360x900 for preview');
  await openFileExplorer(page);
  await clickFilesRow(page, previewMarkdownFile);
  await waitFor(
    page,
    () => document.querySelector('.file-explorer__markdown') !== null,
    'markdown Preview did not render',
    25000
  );
  await assertMarkdownPreviewToggle(page);
  await assertNotesDeepLinkStays(page, 'readme preview');
  await clickFilesRow(page, previewMarkdownFile);
  await waitFor(
    page,
    (want) => (document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || '') === want
      && document.querySelector('.file-explorer__markdown') !== null,
    'deep link did not restore the readme preview',
    25000,
    previewMarkdownFile,
  );
  await laneShot(page, 'files-desktop-preview-journey.png', { fullPage: true });
  await assertHtmlPreview(page);
  await laneShot(page, 'files-desktop-html-preview-journey.png', { fullPage: true });
  // Solidity must be LEXED in the Source surface: specific .hljs-keyword
  // tokens over the intact seeded source — never a plain unhighlighted render.
  await clickFilesRow(page, cleanFile);
  await expandFilesTree(page);
  await laneShot(page, 'files-desktop-tree-icons.png', { fullPage: true });
  await assertSolidityPreview(page, 'desktop solidity preview');
  await laneShot(page, 'files-desktop-sol-preview.png', { fullPage: true });
  await laneShot(page, 'files-desktop-tree-icons-selected.png', { fullPage: true });
  await hideFileExplorer(page, 'preview journey hide');

  await page.setViewportSize({ width: 390, height: 844 });
  await waitFor(page, () => window.innerWidth === 390 && window.innerHeight === 844, 'mobile viewport did not become 390x844 for preview');
  await openFileExplorer(page);
  await clickFilesRow(page, previewMarkdownFile);
  await waitFor(
    page,
    () => document.querySelector('.file-explorer__markdown') !== null,
    'mobile markdown Preview did not render',
    25000
  );
  const mobileSurface = await filesSurfaceProof(page);
  if (mobileSurface.surfaceInHead || !mobileSurface.surfaceInContent) {
    fail(`mobile Source/Preview must stay on the content toolbar: ${JSON.stringify(mobileSurface)}`);
  }
  if (mobileSurface.checked !== 'Preview' || !mobileSurface.markdown) {
    fail(`mobile markdown default must be Preview: ${JSON.stringify(mobileSurface)}`);
  }
  const surfaceBox = await page.locator('.file-explorer__surface-btn', { hasText: 'Preview' }).boundingBox();
  if (!surfaceBox || surfaceBox.height < 44) {
    fail(`mobile Preview control is ${surfaceBox?.width ?? -1}x${surfaceBox?.height ?? -1}, need height >=44`);
  }
  await laneShot(page, 'files-mobile-preview-journey.png', { fullPage: true });
  await clickFilesRow(page, cleanFile);
  await expandFilesTree(page);
  await laneShot(page, 'files-mobile-tree-icons.png', { fullPage: true });
  await assertSolidityPreview(page, 'mobile solidity preview');
  await laneShot(page, 'files-mobile-sol-preview.png', { fullPage: true });
  await laneShot(page, 'files-mobile-tree-icons-selected.png', { fullPage: true });
  await hideFileExplorer(page, 'preview journey mobile hide');
  // Both breakpoints lexed Solidity: register the preview journey's id.
  recordFiles('files.solidity-preview');
  await page.setViewportSize({ width: 1360, height: 900 });
  await waitFor(page, () => window.innerWidth === 1360 && window.innerHeight === 900, 'desktop viewport did not restore after preview');
}

async function runFileExplorerAcceptance(page, sentFrames, receivedFrames) {
  await page.setViewportSize({ width: 1360, height: 900 });
  await waitFor(page, () => window.innerWidth === 1360 && window.innerHeight === 900, 'desktop viewport did not become 1360x900');
  await waitFor(page, () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true, 'WS not connected before Files');
  recordFiles('files.boot');

  await openFileExplorer(page);
  await filesChromeContract(page, { desktopResizer: true });
  await laneShot(page, 'files-desktop-open.png', { fullPage: true });
  recordFiles('files.desktop-open');

  await clickFilesDir(page, 'nested');
  await waitFor(
    page,
    (want) => Array.from(document.querySelectorAll('button.file-explorer__row')).some((btn) => (btn.getAttribute('title') || '') === want),
    `nested child ${nestedFile} missing after expanding nested/`,
    25000,
    nestedFile
  );
  const greet = await filesTreeStatus(page, dirtyFile);
  const added = await filesTreeStatus(page, addedFile);
  const gone = await filesTreeStatus(page, deletedFile);
  const renamed = await filesTreeStatus(page, renamedFile);
  if (!greet || greet.letter !== 'M' || greet.status !== 'modified') fail(`greet.txt status is not M (${JSON.stringify(greet)})`);
  if (!added || added.letter !== 'A' || added.status !== 'added') fail(`added.txt status is not A (${JSON.stringify(added)})`);
  if (!gone || gone.letter !== 'D' || gone.status !== 'deleted' || !gone.gitOnly) {
    fail(`gone.txt must be a virtual deleted git-only row (${JSON.stringify(gone)})`);
  }
  if (!renamed || renamed.letter !== 'R' || renamed.status !== 'renamed') {
    fail(`nested/renamed.txt status is not R (${JSON.stringify(renamed)})`);
  }
  if (!renamed.statusTitle.includes(`${renamedOld} → ${renamedFile}`) && !renamed.statusTitle.includes('→')) {
    fail(`renamed status title missing old → new (${renamed.statusTitle})`);
  }
  await waitFor(
    page,
    () => {
      const dir = document.querySelector('button.file-explorer__row--dir[title="nested"]');
      const badge = dir?.closest('.file-explorer__item')?.querySelector('.file-explorer__status');
      return dir?.getAttribute('aria-expanded') === 'true'
        && dir?.classList.contains('file-explorer__row--has-status')
        && badge?.getAttribute('data-status') === 'renamed'
        && (badge?.textContent || '').trim() === 'R';
    },
    'nested/ dir aggregate letter R did not appear',
    10000
  );
  const nestedDir = await page.evaluate(() => {
    const dir = document.querySelector('button.file-explorer__row--dir[title="nested"]');
    if (!dir) return null;
    const badge = dir.closest('.file-explorer__item')?.querySelector('.file-explorer__status');
    return {
      expanded: dir.getAttribute('aria-expanded'),
      hasStatus: dir.classList.contains('file-explorer__row--has-status'),
      badgeStatus: badge?.getAttribute('data-status') || '',
      badgeText: (badge?.textContent || '').trim(),
    };
  });
  if (!nestedDir || nestedDir.expanded !== 'true') fail('nested/ dir is not expanded after ordinary click');
  if (!nestedDir.hasStatus || nestedDir.badgeStatus !== 'renamed' || nestedDir.badgeText !== 'R') {
    fail(`nested/ dir aggregate must be data-status=renamed letter R (got ${JSON.stringify(nestedDir)})`);
  }
  const orphans = await page.evaluate(() => document.querySelector('[data-orphans="true"]') !== null);
  if (orphans) fail('integrated Git tree still renders a data-orphans list');
  recordFiles('files.git-tree');
  await assertFilesStatusColumn(page, sentFrames, 'desktop');
  await laneShot(page, 'files-status-column-desktop.png', { fullPage: true });
  // files.virtual-dirs-hidden: deployments/sepolia/deploy.json is committed
  // then deleted from the working tree, so deployments/ and
  // deployments/sepolia/ exist ONLY in the change catalog — no disk listing
  // can ever succeed. The changed-file catalog row itself must stay visible
  // (virtual git-only D), while the unavailable directories must NOT render
  // as available expandable dir rows, must never surface a
  // "directory is unavailable" error/Retry, and must never be listed on the
  // wire — hiding unavailable directories must not delete catalog visibility.
  const sepoliaDeleted = await filesTreeStatus(page, sepoliaDeletedFile);
  if (!sepoliaDeleted || sepoliaDeleted.letter !== 'D' || sepoliaDeleted.status !== 'deleted' || !sepoliaDeleted.gitOnly) {
    fail(`${sepoliaDeletedFile} changed-file catalog row must stay visible as a virtual git-only D row (got ${JSON.stringify(sepoliaDeleted)})`);
  }
  const virtualDirs = await page.evaluate(() => Array.from(document.querySelectorAll('button.file-explorer__row--dir'))
    .map((btn) => btn.getAttribute('title') || '')
    .filter((title) => title === 'deployments' || title.startsWith('deployments/')));
  if (virtualDirs.length !== 0) {
    fail(`unavailable virtual directories must not render as expandable dir rows (got ${JSON.stringify(virtualDirs)})`);
  }
  const unavailableSurface = await page.evaluate(() => {
    const section = document.querySelector('section.file-explorer');
    return {
      unavailableText: section ? (section.textContent || '').includes('directory is unavailable') : null,
      errors: document.querySelectorAll('.file-explorer__error').length,
      retries: document.querySelectorAll('.file-explorer__retry').length,
    };
  });
  if (unavailableSurface.unavailableText !== false || unavailableSurface.errors !== 0 || unavailableSurface.retries !== 0) {
    fail(`unavailable directories must never surface an error/Retry surface (got ${JSON.stringify(unavailableSurface)})`);
  }
  const sepoliaListings = filesWsOf(sentFrames, 'file_explorer_list', 'deployments').length
    + filesWsOf(sentFrames, 'file_explorer_list', 'deployments/sepolia').length;
  if (sepoliaListings !== 0) {
    fail(`unavailable virtual directories must never be listed on the wire (${sepoliaListings} file_explorer_list frames)`);
  }
  await laneShot(page, 'files-desktop-virtual-dirs.png', { fullPage: true });
  recordFiles('files.virtual-dirs-hidden');

  // files.tree-split: the desktop tree/content divider is a real slider —
  // keyboard steps move --file-explorer-tree-pct by the documented step with
  // aria-valuenow tracking, Home clamps to the documented minimum, and the
  // shipped default split is restored so later desktop phases keep the
  // shipped layout.
  const splitBefore = await page.evaluate(() => {
    const layout = document.querySelector('[data-testid="file-explorer-layout"]');
    const divider = document.querySelector('[data-testid="file-explorer-divider"]');
    return {
      layout: layout?.getAttribute('data-layout') || '',
      pct: Number.parseFloat(layout?.style.getPropertyValue('--file-explorer-tree-pct') || ''),
      now: Number(divider?.getAttribute('aria-valuenow')),
      min: Number(divider?.getAttribute('aria-valuemin')),
      max: Number(divider?.getAttribute('aria-valuemax')),
    };
  });
  if (splitBefore.layout !== 'split') fail(`file explorer layout is not a desktop split: ${JSON.stringify(splitBefore)}`);
  if (!Number.isFinite(splitBefore.now) || splitBefore.now < splitBefore.min || splitBefore.now > splitBefore.max) {
    fail(`file explorer split percent out of range: ${JSON.stringify(splitBefore)}`);
  }
  if (splitBefore.pct !== splitBefore.now) {
    fail(`file explorer split var/aria disagree: ${JSON.stringify(splitBefore)}`);
  }
  await page.focus('[data-testid="file-explorer-divider"]');
  await page.keyboard.press('ArrowRight');
  await waitFor(
    page,
    (want) => Number(document.querySelector('[data-testid="file-explorer-divider"]')?.getAttribute('aria-valuenow')) === want,
    'file tree split ArrowRight never stepped +2',
    10000,
    splitBefore.now + 2
  );
  await page.keyboard.press('Home');
  await waitFor(
    page,
    () => {
      const divider = document.querySelector('[data-testid="file-explorer-divider"]');
      const layout = document.querySelector('[data-testid="file-explorer-layout"]');
      return Number(divider?.getAttribute('aria-valuenow')) === 22
        && (layout?.style.getPropertyValue('--file-explorer-tree-pct') || '') === '22%';
    },
    'file tree split Home did not clamp to the 22% minimum'
  );
  for (let step = 0; step < (34 - 22) / 2; step++) {
    await page.keyboard.press('ArrowRight');
  }
  await waitFor(
    page,
    () => Number(document.querySelector('[data-testid="file-explorer-divider"]')?.getAttribute('aria-valuenow')) === 34,
    'file tree split never returned to the shipped 34% default'
  );
  recordFiles('files.tree-split');


  await pageLongFile(page, sentFrames, bigFile, 'code', 4100);
  await laneShot(page, 'files-desktop-paged.png', { fullPage: true });
  recordFiles('files.code-scroll');

  const mdReadsBefore = filesPageRequests(sentFrames, markdownFile).length;
  await pageLongFile(page, sentFrames, markdownFile, 'markdown');
  if (filesPageRequests(sentFrames, markdownFile).length <= mdReadsBefore) {
    fail('markdown infinite scroll did not issue a file_explorer_read');
  }
  recordFiles('files.markdown-scroll');

  await assertMarkdownPreviewToggle(page);
  await laneShot(page, 'files-desktop-preview.png', { fullPage: true });
  await assertHtmlPreview(page);
  await laneShot(page, 'files-desktop-html-preview.png', { fullPage: true });
  recordFiles('files.preview-toggle');

  await clickFilesRow(page, dirtyFile);
  await waitFor(
    page,
    (want) => {
      const active = document.querySelector('button.file-explorer__row--active');
      return (active?.getAttribute('title') || '') === want;
    },
    `${dirtyFile} did not become the active Files row`,
    25000,
    dirtyFile
  );
  // Right-click contract (current product): a tracked tree row's contextmenu
  // is DELETE — exactly one file_explorer_delete, no stage_download, no
  // download event (the tree download affordance was removed upstream);
  // git-only rows are a full no-op. renamed.txt is only asserted by the
  // earlier git-tree phase, so it is the disposable delete target.
  const downloads = [];
  const onDownload = (download) => downloads.push(download);
  page.on('download', onDownload);
  const stageBefore = filesWsOf(sentFrames, 'file_explorer_stage_download').length;
  const deleteBefore = filesWsOf(sentFrames, 'file_explorer_delete', renamedFile).length;
  await clickFilesRow(page, renamedFile, { button: 'right' });
  if (filesWsOf(sentFrames, 'file_explorer_delete', renamedFile).length !== deleteBefore + 1) {
    fail(`right-click ${renamedFile} did not send exactly one file_explorer_delete`);
  }
  if (filesWsOf(sentFrames, 'file_explorer_stage_download').length !== stageBefore) {
    fail('tracked row right-click sent file_explorer_stage_download');
  }
  if (downloads.length !== 0) fail(`tracked row right-click produced ${downloads.length} Playwright download(s)`);
  await waitFor(
    page,
    (want) => !Array.from(document.querySelectorAll('button.file-explorer__row'))
      .some((btn) => (btn.getAttribute('title') || '') === want),
    `${renamedFile} row survived the right-click delete refresh`,
    25000,
    renamedFile
  );
  const goneDownloadsBefore = downloads.length;
  const goneStageBefore = filesWsOf(sentFrames, 'file_explorer_stage_download', deletedFile).length;
  const goneDeleteBefore = filesWsOf(sentFrames, 'file_explorer_delete', deletedFile).length;
  const goneDownload = page.waitForEvent('download', { timeout: 2000 }).then(() => 'download').catch(() => 'none');
  await clickFilesRow(page, deletedFile, { button: 'right' });
  if (await goneDownload === 'download') fail('deleted row right-click produced a download');
  if (filesWsOf(sentFrames, 'file_explorer_stage_download', deletedFile).length !== goneStageBefore) {
    fail('deleted row right-click sent file_explorer_stage_download');
  }
  if (filesWsOf(sentFrames, 'file_explorer_delete', deletedFile).length !== goneDeleteBefore) {
    fail('deleted (git-only) row right-click sent file_explorer_delete');
  }
  if (downloads.length !== goneDownloadsBefore) fail('deleted row produced an extra Playwright download');
  page.off('download', onDownload);
  await laneShot(page, 'files-desktop-download.png', { fullPage: true });
  recordFiles('files.download');

  const s0Reads = filesPageRequests(sentFrames, markdownFile).length;
  await clickFilesRow(page, markdownFile);
  await waitFor(
    page,
    (want) => (document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || '') === want,
    'markdown row did not reactivate before snapshot',
    25000,
    markdownFile
  );
  await mutateFilesGeometry(page);
  await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  const s0 = await filesSnapshot(page);
  if (!s0.activeTitle || s0.dividerNow == null || s0.view == null) {
    fail(`S0 fields missing (${JSON.stringify(s0)})`);
  }
  const s0Pct = Number.parseFloat(String(s0.dividerNow));
  if (!Number.isFinite(s0Pct) || s0Pct === 34 || s0.treePct === '34%' || s0.treePct === '') {
    fail(`S0 treePct must be non-default (got divider=${s0.dividerNow} pct=${s0.treePct})`);
  }
  if (!(s0.treeScroll > 0) || !(s0.contentScroll > 0)) {
    fail(`S0 tree/content scroll must be nonzero (tree=${s0.treeScroll} content=${s0.contentScroll})`);
  }
  // Reopen must restore the latest state committed before close.
  const closeState = await filesSnapshot(page);


  await hideFileExplorer(page, 'desktop hide');
  await laneShot(page, 'files-desktop-closed-send.png', { fullPage: true });
  recordFiles('files.hide-reclaim');

  await openFileExplorer(page);
  await waitFor(
    page,
    (want) => (document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || '') === want,
    'reopen did not restore the active Files row',
    25000,
    closeState.activeTitle
  );
  await waitFor(
    page,
    (expected) => {
      const tree = document.querySelector('[data-testid="file-explorer-tree"] .file-explorer__tree-list');
      const loading = (document.querySelector('[data-testid="file-explorer-tree"]')?.textContent || '').includes('Loading workspace\u2026');
      return tree != null && !loading && tree.scrollTop === expected;
    },
    'reopen did not restore the saved tree scroll after directory reload settled',
    25000,
    closeState.treeScroll,
  );
  const s1 = await filesSnapshot(page);
  if (s1.activeTitle !== closeState.activeTitle) fail(`reopen active row ${s1.activeTitle} != ${closeState.activeTitle}`);
  if (JSON.stringify(s1.dirs) !== JSON.stringify(closeState.dirs)) fail(`reopen expanded dirs ${JSON.stringify(s1.dirs)} != ${JSON.stringify(closeState.dirs)}`);
  if (s1.dividerNow !== closeState.dividerNow || s1.treePct !== closeState.treePct) {
    fail(`reopen split ${s1.dividerNow}/${s1.treePct} != ${closeState.dividerNow}/${closeState.treePct}`);
  }
  if (s1.treeScroll !== closeState.treeScroll) fail(`reopen tree scroll ${s1.treeScroll} != ${closeState.treeScroll}`);
  if (s1.contentScroll !== closeState.contentScroll) fail(`reopen content scroll ${s1.contentScroll} != ${closeState.contentScroll}`);
  if (s1.view !== closeState.view) fail(`reopen data-view ${s1.view} != ${closeState.view}`);
  const s1Reads = filesPageRequests(sentFrames, markdownFile).length;
  if (s1Reads !== s0Reads) fail(`reopen re-fetched cached markdown pages (${s0Reads} -> ${s1Reads})`);
  await laneShot(page, 'files-desktop-reopen.png', { fullPage: true });
  recordFiles('files.reopen');

  await page.setViewportSize({ width: 900, height: 1000 });
  await waitFor(page, () => window.innerWidth === 900 && window.innerHeight === 1000, 'tablet viewport did not become 900x1000');
  await waitFor(
    page,
    () => {
      const layout = document.querySelector('.app-layout');
      const drawer = document.querySelector('section.file-explorer');
      const overlay = layout?.classList.contains('app-layout--files-overlay') === true;
      const full = drawer != null && Math.round(drawer.getBoundingClientRect().width) >= window.innerWidth - 1;
      return overlay || full;
    },
    'tablet Files drawer is not overlay/100vw'
  );
  await filesChromeContract(page, { desktopResizer: false });
  await laneShot(page, 'files-tablet-open.png', { fullPage: true });
  await hideFileExplorer(page, 'tablet hide');
  await laneShot(page, 'files-tablet-closed.png', { fullPage: true });
  await openFileExplorer(page);
  const tabletRestored = await filesSnapshot(page);
  if (tabletRestored.activeTitle !== s0.activeTitle) fail('tablet reopen lost the active Files row');
  recordFiles('files.tablet');

  await runFileAskJourney(page, sentFrames);
  recordFiles('files.ask');

  await runFileSelectionJourney(page, sentFrames, await displayedExplorerSession(page));
  recordFiles('files.selection');
  await runFileMapJourney(page, sentFrames);
  recordFiles('files.map');
  await runFilesFullscreenJourney(page);
  recordFiles('files.fullscreen');

  await hideFileExplorer(page, 'tablet hide before mobile');
  await page.setViewportSize({ width: 390, height: 844 });
  await waitFor(page, () => window.innerWidth === 390 && window.innerHeight === 844, 'mobile viewport did not become 390x844');
  await openFileExplorer(page);
  await filesChromeContract(page, { desktopResizer: false });
  const closeBox = await page.locator(FILES_DRAWER_CLOSE).boundingBox();
  if (!closeBox || closeBox.width < 44 || closeBox.height < 44) {
    fail(`mobile drawer close is ${closeBox?.width ?? -1}x${closeBox?.height ?? -1}, need >=44x44`);
  }
  await clickFilesRow(page, markdownFile);
  await page.getByRole('radio', { name: 'Preview', exact: true }).click();
  await waitFor(
    page,
    () => document.querySelector('.file-explorer__markdown') !== null,
    'mobile markdown Preview did not render',
    25000
  );
  const mobileSurface = await filesSurfaceProof(page);
  if (mobileSurface.surfaceInHead || !mobileSurface.surfaceInContent) {
    fail(`mobile Source/Preview must stay on the content toolbar: ${JSON.stringify(mobileSurface)}`);
  }
  if (mobileSurface.checked !== 'Preview' || !mobileSurface.markdown) {
    fail(`mobile markdown Preview selection did not render: ${JSON.stringify(mobileSurface)}`);
  }
  const surfaceBox = await page.locator('.file-explorer__surface-btn', { hasText: 'Preview' }).boundingBox();
  if (!surfaceBox || surfaceBox.height < 44) {
    fail(`mobile Preview control is ${surfaceBox?.width ?? -1}x${surfaceBox?.height ?? -1}, need height >=44`);
  }
  await laneShot(page, 'files-mobile-preview.png', { fullPage: true });
  const overflowOpen = await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth);
  if (!overflowOpen) fail('mobile Files open overflows horizontally');
  await laneShot(page, 'files-mobile-open.png', { fullPage: true });
  await laneShot(page, 'files-mobile-header-hide.png', { fullPage: true });
  await assertFilesStatusColumn(page, sentFrames, 'mobile', { chip44: true });
  await laneShot(page, 'files-status-column-mobile.png', { fullPage: true });
  await hideFileExplorer(page, 'mobile hide');
  const overflowClosed = await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth);
  if (!overflowClosed) fail('mobile Files closed overflows horizontally');
  await laneShot(page, 'files-mobile-closed.png', { fullPage: true });
  recordFiles('files.mobile');
  await runSelectionCacheCompactionJourney(page, sentFrames, receivedFrames);
  recordFiles('files.selection-cache-compaction');
  await hideFileExplorer(page, 'selection cache compaction hide');

}

const FILE_CONVERSATION_ID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

function fileConversationFrames(sentFrames, type) {
  return sentFrames.map(parseSentFrame).filter((frame) => frame?.type === type);
}

function isFixtureModelReply(text) {
  return /steer-\d+-ing stream chunk-four-done/.test(text) || text.includes('steering-followup-reply');
}

async function displayedExplorerSession(page) {
  return page.evaluate(() => document.getElementById('session-lifecycle')?.dataset.displayedSessionId || '');
}

async function clickVisibleAsk(page, label) {
  const ask = page.getByRole('radio', { name: 'Ask about this file' });
  await waitFor(
    page,
    () => {
      const control = document.querySelector('[data-testid="file-explorer-ask-toggle"]');
      if (!control || control.getAttribute('aria-label') !== 'Ask about this file') return false;
      const box = control.getBoundingClientRect();
      return box.width > 0 && box.height > 0 && getComputedStyle(control).visibility !== 'hidden';
    },
    `${label}: Ask about this file is not a visible control`
  );
  const box = await ask.boundingBox();
  if (!box || box.width < 1 || box.height < 1) fail(`${label}: Ask control has no visible box`);
  const hit = await page.evaluate(({ x, y }) => {
    const el = document.elementFromPoint(x, y);
    return el?.closest('[data-testid="file-explorer-ask-toggle"]') != null;
  }, { x: box.x + box.width / 2, y: box.y + Math.min(box.height / 2, 12) });
  if (!hit) fail(`${label}: Ask control is covered at its visible center`);
  await ask.click();
}

async function assertAskChrome(page, filePath, label, { touch = false, source = true } = {}) {
  await waitFor(
    page,
    (expectSource) => {
      const split = document.querySelector('[data-testid="file-explorer-ask-split"]');
      const dock = document.querySelector('section.file-conversation');
      const code = document.querySelector('pre.file-explorer__code');
      if (!split || !dock) return false;
      return expectSource ? code !== null : true;
    },
    `${label}: Ask did not open the source dock`,
    25000,
    source
  );
  const chrome = await page.evaluate((expectedLines) => {
    const section = document.querySelector('section.file-explorer');
    const header = document.querySelector('.file-explorer__head');
    const dock = document.querySelector('section.file-conversation');
    const sourceNode = document.querySelector('[data-testid="file-explorer-ask-split"] pre.file-explorer__code');
    const footer = dock?.querySelector('.file-conversation__actions');
    const boxOf = (node) => {
      if (!node) return null;
      const box = node.getBoundingClientRect();
      const style = getComputedStyle(node);
      return {
        x: box.x, y: box.y, width: box.width, height: box.height,
        hidden: style.display === 'none' || style.visibility === 'hidden',
      };
    };
    const headerBox = boxOf(header);
    const footerBox = boxOf(footer);
    const sourceBox = boxOf(sourceNode);
    const dockBox = boxOf(dock);
    const visible = (box) => box != null && !box.hidden && box.width > 0 && box.height > 0
      && box.y < window.innerHeight && box.y + box.height > 0;
    const lines = Array.from(sourceNode?.querySelectorAll('.file-explorer__line-text') || []).map((el) => el.textContent || '');
    const split = document.querySelector('[data-testid="file-explorer-ask-split"]')?.getBoundingClientRect();
    const inside = (box) => split != null && box != null && box.x >= split.x - 1 && box.x + box.width <= split.x + split.width + 1
      && box.y >= split.y - 1 && box.y + box.height <= split.y + split.height + 1;
    const sideBySide = sourceBox != null && dockBox != null && sourceBox.x + sourceBox.width <= dockBox.x + 1
      && sourceBox.y < dockBox.y + dockBox.height && dockBox.y < sourceBox.y + sourceBox.height;
    const stacked = sourceBox != null && dockBox != null && sourceBox.y + sourceBox.height <= dockBox.y + 1
      && sourceBox.height >= 120 && dockBox.height >= 120;
    return {
      pathText: dock?.querySelector('.file-conversation__path')?.textContent || '',
      sourceIntact: JSON.stringify(lines) === JSON.stringify(expectedLines),
      lines,
      headerVisible: visible(headerBox) && headerBox.y >= -1,
      footerVisible: visible(footerBox) && footerBox.y + footerBox.height <= window.innerHeight + 1,
      sourceVisible: visible(sourceBox),
      sourceWidth: sourceBox?.width ?? 0,
      dockVisible: visible(dockBox),
      dockWidth: dockBox?.width ?? 0,
      windowWidth: window.innerWidth,
      integrated: visible(sourceBox) && visible(dockBox) && inside(sourceBox) && inside(dockBox) && (sideBySide || stacked),
      askHeight: document.querySelector('[data-testid="file-explorer-ask-toggle"]')?.getBoundingClientRect().height ?? 0,
      drawerInside: section != null && section.getBoundingClientRect().width <= window.innerWidth + 1,
      overflow: document.documentElement.scrollWidth <= document.documentElement.clientWidth + 1,
    };
  }, SOLIDITY_SOURCE_LINES);
  if (chrome.pathText !== 'No file' && chrome.pathText !== filePath) {
    fail(`${label}: Ask dock path is ${JSON.stringify(chrome.pathText)}, not ${filePath} or the pre-open placeholder`);
  }
  if (source) {
    if (!chrome.sourceIntact) {
      fail(`${label}: Ask source pane is not the intact ${filePath} source (${JSON.stringify(chrome.lines)})`);
    }
    if (!chrome.sourceVisible || !chrome.integrated) {
      fail(`${label}: source and dialogue are not integrated side by side or stacked (${JSON.stringify(chrome)})`);
    }
    if (chrome.windowWidth >= 1280 && (chrome.sourceWidth < 300 || chrome.dockWidth < 280)) {
      fail(`${label}: desktop Ask panes are unreadable (source ${chrome.sourceWidth}px, dock ${chrome.dockWidth}px)`);
    }
  }
  if (!chrome.dockVisible || !chrome.headerVisible || !chrome.footerVisible) {
    fail(`${label}: dock, header, or footer is not visible (${JSON.stringify(chrome)})`);
  }
  if (!chrome.drawerInside || !chrome.overflow) fail(`${label}: Ask layout overflows the viewport`);
  if (touch && chrome.askHeight < 44) fail(`${label}: Ask control height ${chrome.askHeight} is under 44`);
}

async function selectSourceRange(page, startLine, endLine, label) {
  const selected = await page.evaluate(({ startLine: start, endLine: end }) => {
    const startNode = document.querySelector(`#file-explorer-line-${start} .file-explorer__line-text`);
    const endNode = document.querySelector(`#file-explorer-line-${end} .file-explorer__line-text`);
    const root = document.querySelector('[data-testid="file-explorer-ask-split"] pre.file-explorer__code');
    if (!startNode || !endNode || !root?.contains(startNode) || !root.contains(endNode)) return false;
    const range = document.createRange();
    range.setStart(startNode, 0);
    range.setEnd(endNode, endNode.childNodes.length);
    const selection = window.getSelection();
    if (!selection) return false;
    selection.removeAllRanges();
    selection.addRange(range);
    root.dispatchEvent(new MouseEvent('mouseup', { bubbles: true, cancelable: true, view: window }));
    return !selection.isCollapsed && root.contains(selection.anchorNode) && root.contains(selection.focusNode);
  }, { startLine, endLine });
  if (!selected) fail(`${label}: source lines ${startLine}-${endLine} could not be selected`);
  await waitFor(
    page,
    ({ start, end }) => (document.querySelector('.file-conversation__context')?.textContent || '')
      === `Selection lines ${start}–${end}`,
    `${label}: Ask dock did not disclose the selected source range`,
    10000,
    { start: startLine, end: endLine }
  );
}

async function selectSourceLines(page, startLine, endLine, label) {
  const selected = await page.evaluate(({ startLine: start, endLine: end }) => {
    const startNode = document.querySelector(`#file-explorer-line-${start} .file-explorer__line-text`);
    const endNode = document.querySelector(`#file-explorer-line-${end} .file-explorer__line-text`);
    const root = document.querySelector('pre.file-explorer__code');
    if (!startNode || !endNode || !root?.contains(startNode) || !root.contains(endNode)) return false;
    const range = document.createRange();
    range.setStart(startNode, 0);
    range.setEnd(endNode, endNode.childNodes.length);
    const selection = window.getSelection();
    if (!selection) return false;
    selection.removeAllRanges();
    selection.addRange(range);
    root.dispatchEvent(new MouseEvent('mouseup', { bubbles: true, cancelable: true, view: window }));
    return !selection.isCollapsed
      && startNode.contains(selection.anchorNode)
      && endNode.contains(selection.focusNode);
  }, { startLine, endLine });
  if (!selected) fail(`${label}: source lines ${startLine}-${endLine} could not be selected`);
  await waitFor(page, ({ start, end }) => {
    const toolbar = document.querySelector('[data-testid="file-selection-actions"]');
    const root = document.querySelector('pre.file-explorer__code');
    const selection = window.getSelection();
    const anchorLine = selection?.anchorNode?.parentElement?.closest('[id^="file-explorer-line-"]')?.id;
    const focusLine = selection?.focusNode?.parentElement?.closest('[id^="file-explorer-line-"]')?.id;
    return toolbar != null && root != null && !root.contains(toolbar)
      && toolbar.getAttribute('role') === 'toolbar'
      && toolbar.getAttribute('aria-label') === 'Selected lines'
      && anchorLine === `file-explorer-line-${start}`
      && focusLine === `file-explorer-line-${end}`;
  }, `${label}: selection toolbar did not render outside the source for lines ${startLine}-${endLine}`, 10000, { start: startLine, end: endLine });
}
function fileSelectionBlameDate(time) {
  const date = new Date(time * 1000);
  if (!Number.isFinite(time) || Number.isNaN(date.getTime())) return '';
  const month = String(date.getMonth() + 1).padStart(2, '0');
  const day = String(date.getDate()).padStart(2, '0');
  return `${date.getFullYear()}-${month}-${day}`;
}

const SELECTION_EDGE_PX = 8;
const SELECTION_STABLE_PX = 0.5;

async function sourceGeometry(page) {
  return page.evaluate(() => {
    const code = document.querySelector('pre.file-explorer__code');
    const first = document.querySelector('#file-explorer-line-1');
    const pane = document.querySelector('[data-testid="file-explorer-content"]');
    const search = document.querySelector('[data-testid="file-explorer-search"]');
    const sourceBtn = Array.from(document.querySelectorAll('.file-explorer__surface [role="radio"]'))
      .find((node) => (node.textContent || '').trim() === 'Source');
    const box = (node) => {
      if (!node || node.getClientRects().length === 0) return null;
      const rect = node.getBoundingClientRect();
      return { x: rect.x, y: rect.y, width: rect.width, height: rect.height, top: rect.top, bottom: rect.bottom, left: rect.left, right: rect.right };
    };
    return {
      code: box(code),
      clientHeight: code ? code.clientHeight : null,
      scrollTop: code ? code.scrollTop : null,
      first: box(first),
      firstText: (first?.querySelector('.file-explorer__line-text')?.textContent ?? '').slice(0, 80),
      pane: box(pane),
      search: box(search),
      source: box(sourceBtn ?? null),
      phoneHidden: window.matchMedia('(max-width: 720px)').matches
        && document.querySelector('[data-testid="file-explorer-tree"]')?.hidden === true,
    };
  });
}

function geometryShift(before, after, label) {
  if (!before?.code || !after?.code || !before.first || !after.first) {
    fail(`${label}: source geometry missing (${JSON.stringify({ before: !!before?.code, after: !!after?.code })})`);
  }
  const delta = (left, right) => Math.abs(left - right);
  const moved = delta(before.code.x, after.code.x) > SELECTION_STABLE_PX
    || delta(before.code.y, after.code.y) > SELECTION_STABLE_PX
    || delta(before.code.width, after.code.width) > SELECTION_STABLE_PX
    || delta(before.code.height, after.code.height) > SELECTION_STABLE_PX
    || delta(before.clientHeight, after.clientHeight) > SELECTION_STABLE_PX
    || delta(before.scrollTop, after.scrollTop) > SELECTION_STABLE_PX
    || delta(before.first.y, after.first.y) > SELECTION_STABLE_PX
    || before.firstText !== after.firstText;
  if (moved) fail(`${label}: source rect, clientHeight, scrollTop, or first line moved (${JSON.stringify({ before, after })})`);
}

async function assertSelectionFloat(page, startLine, endLine, label) {
  const proof = await page.evaluate(({ start, end, edge }) => {
    const pane = document.querySelector('[data-testid="file-explorer-content"]');
    const bar = document.querySelector('[data-testid="file-selection-actions"]');
    const code = document.querySelector('pre.file-explorer__code');
    const lines = [];
    for (let line = start; line <= end; line += 1) {
      const node = document.getElementById(`file-explorer-line-${line}`);
      if (node) lines.push(node.getBoundingClientRect());
    }
    const paneBox = pane?.getBoundingClientRect();
    const barBox = bar?.getBoundingClientRect();
    const selection = lines.length === 0 ? null : {
      top: Math.min(...lines.map((rect) => rect.top)),
      bottom: Math.max(...lines.map((rect) => rect.bottom)),
      left: Math.min(...lines.map((rect) => rect.left)),
      right: Math.max(...lines.map((rect) => rect.right)),
    };
    const style = bar ? getComputedStyle(bar) : null;
    const inside = paneBox && barBox
      && barBox.left >= paneBox.left + edge - 0.5
      && barBox.right <= paneBox.right - edge + 0.5
      && barBox.top >= paneBox.top + edge - 0.5
      && barBox.bottom <= paneBox.bottom - edge + 0.5;
    const overlaps = selection && barBox
      && barBox.top < selection.bottom - 0.5
      && barBox.bottom > selection.top + 0.5
      && barBox.left < selection.right - 0.5
      && barBox.right > selection.left + 0.5;
    const roomBelow = paneBox && selection ? paneBox.bottom - selection.bottom >= (barBox?.height ?? 0) + edge : false;
    const roomAbove = paneBox && selection ? selection.top - paneBox.top >= (barBox?.height ?? 0) + edge : false;
    const ask = bar?.querySelector('[data-testid="file-selection-ask"]');
    const blame = bar?.querySelector('[data-testid="file-selection-blame"]');
    const close = bar?.querySelector('[data-testid="file-selection-close"]');
    const target = (node) => {
      if (!node) return 0;
      const rect = node.getBoundingClientRect();
      return Math.min(rect.width, rect.height);
    };
    return {
      outsideSource: bar != null && code != null && !code.contains(bar),
      position: style?.position || '',
      inside,
      overlaps: overlaps === true,
      room: roomBelow || roomAbove,
      ask: ask?.getAttribute('aria-label') || '',
      blame: blame?.getAttribute('aria-label') || '',
      close: close?.getAttribute('aria-label') || '',
      targets: [target(ask), target(blame), target(close)],
      narrow: window.matchMedia('(max-width: 720px)').matches,
    };
  }, { start: startLine, end: endLine, edge: SELECTION_EDGE_PX });
  if (!proof.outsideSource || proof.position === 'static') {
    fail(`${label}: selection bar is not a floating overlay outside the source (${JSON.stringify(proof)})`);
  }
  if (!proof.inside) fail(`${label}: floating bar is not inside the content pane by ${SELECTION_EDGE_PX}px (${JSON.stringify(proof)})`);
  if (proof.room && proof.overlaps) fail(`${label}: floating bar overlaps the selection while space exists (${JSON.stringify(proof)})`);
  if (proof.ask !== 'Ask' || proof.blame !== 'Blame' || proof.close !== 'Close selection') {
    fail(`${label}: floating actions are not named Ask, Blame, and Close selection (${JSON.stringify(proof)})`);
  }
  if (proof.narrow && proof.targets.some((size) => size < 44)) {
    fail(`${label}: narrow floating actions are under 44px (${JSON.stringify(proof.targets)})`);
  }
}

async function assertToolbarAlignment(page, label) {
  const aligned = await page.evaluate(() => {
    const search = document.querySelector('[data-testid="file-explorer-search"]');
    const source = Array.from(document.querySelectorAll('.file-explorer__surface [role="radio"]'))
      .find((node) => (node.textContent || '').trim() === 'Source');
    const treeHidden = document.querySelector('[data-testid="file-explorer-tree"]')?.hidden === true;
    if (!search || !source || search.getClientRects().length === 0 || source.getClientRects().length === 0) {
      return { comparable: false, treeHidden };
    }
    const searchBox = search.getBoundingClientRect();
    const sourceBox = source.getBoundingClientRect();
    return {
      comparable: true,
      treeHidden,
      top: Math.abs(searchBox.top - sourceBox.top),
      bottom: Math.abs(searchBox.bottom - sourceBox.bottom),
    };
  });
  if (!aligned.comparable) {
    if (aligned.treeHidden) return;
    fail(`${label}: Search and Source toolbars are not both measurable`);
  }
  if (aligned.top > SELECTION_STABLE_PX || aligned.bottom > SELECTION_STABLE_PX) {
    fail(`${label}: Search and Source toolbar edges differ (${JSON.stringify(aligned)})`);
  }
}
async function twoFrames(page) {
  await page.evaluate(() => new Promise((resolve) => {
    requestAnimationFrame(() => requestAnimationFrame(resolve));
  }));
}

async function sourceScrollTop(page) {
  return page.evaluate(() => document.querySelector('pre.file-explorer__code')?.scrollTop ?? null);
}

async function assertSourceScrollStays(page, expected, label) {
  const code = page.locator('pre.file-explorer__code');
  await code.evaluate((node) => {
    node.dispatchEvent(new Event('scroll', { bubbles: true }));
    if (typeof ResizeObserver === 'function') {
      const probe = new ResizeObserver(() => {});
      probe.observe(node);
      probe.disconnect();
    }
  });
  await twoFrames(page);
  const after = await sourceScrollTop(page);
  if (after == null || Math.abs(after - expected) > 0.5) {
    fail(`${label}: source scroll moved (${expected} -> ${after})`);
  }
}

async function openDeepLinkedSource(page, filePath, line, label) {
  const link = page.locator(`[data-md-path="${filePath}"][data-md-line="${line}"]`).first();
  if (await link.count() === 0) {
    fail(`${label}: no rendered markdown link opens ${filePath}:${line}`);
  }
  await link.click();
  await waitFor(
    page,
    (want) => {
      const row = document.getElementById(`file-explorer-line-${want}`);
      const code = document.querySelector('pre.file-explorer__code');
      if (!row || !code || row.getClientRects().length === 0) return false;
      const rowBox = row.getBoundingClientRect();
      const codeBox = code.getBoundingClientRect();
      return rowBox.top >= codeBox.top - 1 && rowBox.bottom <= codeBox.bottom + 1;
    },
    `${label}: deep link did not bring line ${line} into the source viewport`,
    25000,
    line,
  );
}
async function assertLongSourceRoundTrip(page, label, leave, back) {
  const active = await page.evaluate(() => document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || '');
  await clickFilesRow(page, markdownFile);
  const source = page.getByRole('radio', { name: 'Source', exact: true });
  if (await source.count()) await source.click();
  await waitFor(page, () => {
    const code = document.querySelector('pre.file-explorer__code');
    return code != null && code.scrollHeight > code.clientHeight + 8;
  }, `${label}: ${markdownFile} source is not scrollable`, 15000);
  const mark = await page.locator('pre.file-explorer__code').evaluate((node) => {
    const max = node.scrollHeight - node.clientHeight;
    node.scrollTop = Math.min(160, Math.max(48, max));
    node.dispatchEvent(new Event('scroll', { bubbles: true }));
    return node.scrollTop;
  });
  if (!(mark > 0)) fail(`${label}: ${markdownFile} source scroll did not leave zero`);
  await leave();
  await back();
  await assertSourceScrollStays(page, mark, label);
  if (active && active !== markdownFile) await clickFilesRow(page, active);
}

async function assertNotesDeepLinkStays(page, label) {
  const line = 40;
  const link = page.locator(`[data-md-path="${markdownFile}"][data-md-line="${line}"]`).first();
  if (await link.count() === 0) fail(`${label}: seeded ${markdownFile}:${line} link is not rendered`);
  await openDeepLinkedSource(page, markdownFile, line, label);
  const away = await page.locator('pre.file-explorer__code').evaluate((node) => {
    const max = node.scrollHeight - node.clientHeight;
    if (max <= 8) return null;
    node.scrollTop = max;
    node.dispatchEvent(new Event('scroll', { bubbles: true }));
    return node.scrollTop;
  });
  if (away == null || away <= 0) fail(`${label}: ${markdownFile} cannot scroll away from the deep-linked line`);
  await assertSourceScrollStays(page, away, `${label} deep link stays away`);
}

async function assertSelectionDismiss(page, label) {
  const drawer = await page.locator('section.file-explorer').count();
  await page.keyboard.press('Escape');
  await waitFor(
    page,
    () => document.querySelector('[data-testid="file-selection-actions"]') == null
      && document.querySelector('[data-testid="file-selection-blame-list"]') == null
      && document.querySelector('section.file-explorer') != null,
    `${label}: Escape did not dismiss the selection before the drawer`,
    10000,
  );
  if ((await page.locator('section.file-explorer').count()) !== drawer) {
    fail(`${label}: Escape closed the drawer while dismissing the selection`);
  }
  const range = await page.evaluate(() => {
    const selection = window.getSelection();
    return selection == null || selection.isCollapsed;
  });
  if (!range) fail(`${label}: dismissed selection still has a live range`);
}
async function runFileSelectionJourney(page, sentFrames, owner) {
  if (!owner) fail('files.selection: displayed session id is empty');
  if (!/^[0-9a-f]{40}$/.test(solCommit)) fail(`files.selection: fixture commit was not published (${JSON.stringify(solCommit)})`);
  const startLine = 4;
  const endLine = 7;
  const expected = [
    'contract TokenStore {',
    '    uint256 private totalSupply;',
    '',
    '    function mint(uint256 amount) public {',
  ];
  const blameDate = fileSelectionBlameDate(solCommitTime);
  const legs = [
    { width: 1360, height: 900, label: 'desktop', wire: true },
    { width: 1360, height: 900, label: 'narrow360', wire: false, contentWidth: 360 },
    { width: 390, height: 844, label: 'mobile', wire: true },
  ];
  for (const leg of legs) {
    await page.setViewportSize({ width: leg.width, height: leg.height });
    await waitFor(page, (size) => window.innerWidth === size.width && window.innerHeight === size.height, `files.selection ${leg.label} viewport`, 10000, leg);
    if ((await page.locator('section.file-explorer').count()) === 0) await openFileExplorer(page);
    await expandFilesTree(page);
    await clickFilesRow(page, solFile);
    const sourceRadio = page.getByRole('radio', { name: 'Source' });
    if (await sourceRadio.count()) await sourceRadio.click();
    await waitFor(page, () => {
      const code = document.querySelector('pre.file-explorer__code');
      return code != null && code.getClientRects().length > 0;
    }, `files.selection ${leg.label}: source missing`);
    let narrowLayout;
    if (leg.contentWidth) {
      narrowLayout = await page.evaluate(() => ({
        drawerWidth: Number(document.getElementById('file-explorer-resizer')?.getAttribute('aria-valuenow')),
        treePct: Number(document.querySelector('[data-testid="file-explorer-divider"]')?.getAttribute('aria-valuenow')),
      }));
      if (!(narrowLayout.drawerWidth > 0 && narrowLayout.treePct > 0)) {
        fail(`files.selection ${leg.label}: resize controls missing (${JSON.stringify(narrowLayout)})`);
      }
      // Content width is the flex remainder after tree + divider, not its inline width.
      await page.locator('#file-explorer-resizer').press('Home');
      await twoFrames(page);
      const split = await page.locator('[data-testid="file-explorer-layout"]').boundingBox();
      const divider = await page.locator('[data-testid="file-explorer-divider"]').boundingBox();
      if (!split || !divider) fail(`files.selection ${leg.label}: split geometry missing`);
      await page.mouse.move(divider.x + divider.width / 2, divider.y + divider.height / 2);
      await page.mouse.down();
      const treePct = Math.floor((split.width - leg.contentWidth - divider.width) / split.width * 100);
      await page.mouse.move(split.x + split.width * treePct / 100, divider.y + divider.height / 2);
      await page.mouse.up();
      await twoFrames(page);
      const pane = await page.locator('[data-testid="file-explorer-content"]').boundingBox();
      const resizer = await page.locator('#file-explorer-resizer').boundingBox();
      if (!pane || !resizer) fail(`files.selection ${leg.label}: content resize geometry missing`);
      // Tree percentages round to integers; the outer pixel resizer supplies the remainder.
      const drawerDelta = (leg.contentWidth - pane.width) / (1 - treePct / 100);
      await page.mouse.move(resizer.x + resizer.width / 2, resizer.y + resizer.height / 2);
      await page.mouse.down();
      await page.mouse.move(resizer.x + resizer.width / 2 - drawerDelta, resizer.y + resizer.height / 2);
      await page.mouse.up();
      await waitFor(page, (width) => {
        const pane = document.querySelector('[data-testid="file-explorer-content"]');
        return pane != null && Math.abs(pane.getBoundingClientRect().width - width) <= 1;
      }, `files.selection ${leg.label}: content pane is not ${leg.contentWidth}px`, 10000, leg.contentWidth);
    }
    await assertToolbarAlignment(page, `${leg.label} toolbars`);
    const before = await sourceGeometry(page);
    await laneShot(page, `files-selection-${leg.label}-before.png`);
    const frameMark = sentFrames.length;
    await selectSourceLines(page, startLine, endLine, `${leg.label} selection`);
    geometryShift(before, await sourceGeometry(page), `${leg.label} selection`);
    await assertSelectionFloat(page, startLine, endLine, `${leg.label} selection`);
    const kept = await page.evaluate(({ start, end }) => {
      const selection = window.getSelection();
      const anchor = selection?.anchorNode?.parentElement?.closest('[id^="file-explorer-line-"]')?.id;
      const focus = selection?.focusNode?.parentElement?.closest('[id^="file-explorer-line-"]')?.id;
      const bar = document.querySelector('[data-testid="file-selection-actions"]');
      const point = bar?.getBoundingClientRect();
      const hit = point ? document.elementFromPoint(point.x + 8, point.y + Math.min(12, point.height / 2)) : null;
      return anchor === `file-explorer-line-${start}` && focus === `file-explorer-line-${end}` && hit?.closest('[data-testid="file-selection-actions"]') != null;
    }, { start: startLine, end: endLine });
    if (!kept) fail(`${leg.label}: pointer on the floating bar did not preserve the selected range`);
    await laneShot(page, `files-selection-${leg.label}-selected.png`);
    if (leg.wire) {
      await page.getByRole('button', { name: 'Ask', exact: true }).click();
      await waitFor(page, ({ startLine: start, endLine: end }) => document.activeElement === document.querySelector('textarea.file-conversation__input')
        && (document.querySelector('.file-conversation__context')?.textContent || '') === `Selection lines ${start}\u2013${end}`,
        `${leg.label}: Ask did not focus the dock with the selected range`, 25000, { startLine, endLine });
      const question = `files-selection-${leg.label}-${laneNonce || 'lane'}`;
      await sendFileQuestion(page, question, `${leg.label} selection ask`);
      const prompt = sentFrames.slice(frameMark).map(parseSentFrame).filter((frame) => frame?.type === 'file_conversation_prompt').at(-1);
      if (!prompt || prompt.sessionId !== owner || prompt.selection?.startLine !== startLine || prompt.selection?.endLine !== endLine) {
        fail(`${leg.label}: selected prompt is not the exact owner range (${JSON.stringify(prompt)})`);
      }
      await waitForFixtureReply(page, question, `${leg.label} selection ask`);
      if (await sourceRadio.count()) await sourceRadio.click();
      await waitFor(page, () => {
        const code = document.querySelector('pre.file-explorer__code');
        return code != null && code.getClientRects().length > 0;
      }, `${leg.label}: source hidden before Blame`);
    }
    const beforeBlame = await sourceGeometry(page);
    await selectSourceLines(page, startLine, endLine, `${leg.label} blame`);
    geometryShift(beforeBlame, await sourceGeometry(page), `${leg.label} reselection`);
    const blameMark = sentFrames.length;
    await page.getByRole('button', { name: 'Blame', exact: true }).click();
    await waitFor(page, (count) => document.querySelectorAll('[data-testid="file-selection-blame-list"] [data-line]').length === count, `${leg.label}: blame rows missing`, 15000, expected.length);
    geometryShift(beforeBlame, await sourceGeometry(page), `${leg.label} blame open`);
    await assertSelectionFloat(page, startLine, endLine, `${leg.label} blame`);
    if (leg.wire) {
      const blame = sentFrames.slice(blameMark).map(parseSentFrame).find((frame) => frame?.type === 'file_explorer_blame');
      if (!blame || blame.path !== solFile || blame.startLine !== startLine || blame.endLine !== endLine || blame.sessionId !== owner) {
        fail(`${leg.label}: blame request is not the exact range (${JSON.stringify(blame)})`);
      }
      const rows = await page.evaluate(() => Array.from(document.querySelectorAll('[data-testid="file-selection-blame-list"] [data-line]')).map((row) => {
        const commit = row.querySelector('span.file-selection-actions__commit');
        return {
          line: Number(row.getAttribute('data-line')),
          commitTitle: commit?.getAttribute('title') || '',
          commitLabel: commit?.textContent || '',
          author: row.querySelector('span.file-selection-actions__author')?.textContent || '',
          date: row.querySelector('span.file-selection-actions__date')?.textContent || '',
          source: row.querySelector('span.file-selection-actions__text')?.textContent ?? null,
        };
      }));
      expected.forEach((source, index) => {
        const row = rows.find((item) => item.line === startLine + index);
        if (!row || row.commitTitle !== solCommit || row.commitLabel !== solCommit.slice(0, 9) || row.author !== 'Web E2E' || row.date !== blameDate || row.source !== source) {
          fail(`${leg.label}: blame line ${startLine + index} is not the committed source (${JSON.stringify(row)})`);
        }
      });
    }
    const scrollProof = await page.evaluate((line) => {
      const list = document.querySelector('[data-testid="file-selection-blame-list"]');
      const source = document.querySelector(`[data-testid="file-selection-blame-list"] [data-line="${line}"] .file-selection-actions__text`);
      const header = document.querySelector('[data-testid="file-explorer-header"]');
      const composer = document.querySelector('textarea.file-conversation__input');
      const box = (node) => {
        if (!node) return null;
        const rect = node.getBoundingClientRect();
        return { x: Math.round(rect.x), y: Math.round(rect.y), right: Math.round(rect.right), width: Math.round(rect.width), height: Math.round(rect.height) };
      };
      const beforeScroll = { header: box(header), composer: box(composer) };
      const max = list ? list.scrollWidth - list.clientWidth : 0;
      if (list && max > 0) list.scrollLeft = max;
      const sourceBox = box(source);
      const afterScroll = { header: box(header), composer: box(composer) };
      return {
        max,
        scrollLeft: list ? list.scrollLeft : 0,
        sourceRight: sourceBox ? sourceBox.right : null,
        viewport: window.innerWidth,
        before: beforeScroll,
        after: afterScroll,
      };
    }, endLine);
    if (scrollProof.max > 0 && scrollProof.scrollLeft <= 0) fail(`${leg.label}: blame list did not scroll (${JSON.stringify(scrollProof)})`);
    if (scrollProof.sourceRight == null || scrollProof.sourceRight > scrollProof.viewport) {
      fail(`${leg.label}: last blame source remains outside the viewport (${JSON.stringify(scrollProof)})`);
    }
    if (JSON.stringify(scrollProof.before) !== JSON.stringify(scrollProof.after)) {
      fail(`${leg.label}: blame scroll moved the header or composer (${JSON.stringify(scrollProof)})`);
    }
    await laneShot(page, `files-selection-${leg.label}-blame.png`);
    await laneShot(page, `files-selection-${leg.label}-scrolled.png`, { fullPage: true });
    await laneShot(page, `files-selection-${leg.label}.png`, { fullPage: true });
    await assertSelectionDismiss(page, leg.label);
    geometryShift(beforeBlame, await sourceGeometry(page), `${leg.label} dismiss`);
    await laneShot(page, `files-selection-${leg.label}-closed.png`);
    if (leg.contentWidth) {
      const split = await page.locator('[data-testid="file-explorer-layout"]').boundingBox();
      const divider = await page.locator('[data-testid="file-explorer-divider"]').boundingBox();
      const resizer = await page.locator('#file-explorer-resizer').boundingBox();
      const drawer = await page.locator('section.file-explorer').boundingBox();
      if (!split || !divider || !resizer || !drawer) fail(`files.selection ${leg.label}: restore geometry missing`);
      await page.mouse.move(divider.x + divider.width / 2, divider.y + divider.height / 2);
      await page.mouse.down();
      await page.mouse.move(split.x + split.width * narrowLayout.treePct / 100, divider.y + divider.height / 2);
      await page.mouse.up();
      await page.mouse.move(resizer.x + resizer.width / 2, resizer.y + resizer.height / 2);
      await page.mouse.down();
      await page.mouse.move(resizer.x + resizer.width / 2 + drawer.width - narrowLayout.drawerWidth, resizer.y + resizer.height / 2);
      await page.mouse.up();
    }
  }
  await page.setViewportSize({ width: 1360, height: 900 });
  await waitFor(page, () => window.innerWidth === 1360 && window.innerHeight === 900, 'files.selection fullscreen viewport', 10000);
  if ((await page.locator('section.file-explorer').count()) === 0) await openFileExplorer(page);
  const expand = page.locator('#file-explorer-expand-btn');
  if (await expand.count()) {
    await expand.click();
    await waitFor(page, () => document.querySelector('.app-layout')?.classList.contains('app-layout--files-expanded') === true, 'files.selection fullscreen did not expand', 10000);
  }
  await clickFilesRow(page, solFile);
  const fullscreenRadio = page.getByRole('radio', { name: 'Source' });
  if (await fullscreenRadio.count()) await fullscreenRadio.click();
  await waitFor(page, () => document.querySelector('pre.file-explorer__code')?.getClientRects().length > 0, 'files.selection fullscreen source missing');
  await assertToolbarAlignment(page, 'fullscreen toolbars');
  const fullscreenBefore = await sourceGeometry(page);
  await laneShot(page, 'files-selection-fullscreen-before.png');
  await selectSourceLines(page, startLine, endLine, 'fullscreen selection');
  geometryShift(fullscreenBefore, await sourceGeometry(page), 'fullscreen selection');
  await assertSelectionFloat(page, startLine, endLine, 'fullscreen selection');
  await laneShot(page, 'files-selection-fullscreen-selected.png');
  await page.getByRole('button', { name: 'Blame', exact: true }).click();
  await waitFor(page, (count) => document.querySelectorAll('[data-testid="file-selection-blame-list"] [data-line]').length === count, 'fullscreen: blame rows missing', 15000, expected.length);
  geometryShift(fullscreenBefore, await sourceGeometry(page), 'fullscreen blame');
  await laneShot(page, 'files-selection-fullscreen-blame.png');
  await assertSelectionDismiss(page, 'fullscreen');
  geometryShift(fullscreenBefore, await sourceGeometry(page), 'fullscreen dismiss');
  await laneShot(page, 'files-selection-fullscreen-closed.png');
  const restore = page.getByRole('button', { name: 'Restore file explorer' });
  if (await restore.count()) await restore.click();
  await expandFilesTree(page);
  await clickFilesRow(page, cleanFile);
  await waitFor(page, (want) => (document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || '') === want, 'files.selection: clean file did not become active', 25000, cleanFile);
  const stale = await page.evaluate(() => (document.querySelector('[data-testid="file-selection-blame-list"]')?.textContent || '').includes('TokenStore'));
  if (stale) fail('files.selection: Token.sol blame remained after switching to the clean file');
  await page.setViewportSize({ width: 1360, height: 900 });
}

async function paginateSelectionWindow(page, sentFrames, receivedFrames, owner, frameMark) {
  const deadline = Date.now() + 90000;
  const wheelPoint = await page.evaluate(() => {
    const source = document.querySelector('pre.file-explorer__code');
    const rect = source?.getBoundingClientRect();
    if (!rect || rect.width <= 16 || rect.height <= 16) return null;
    const point = { x: rect.left + 8, y: rect.top + 8 };
    return source.contains(document.elementFromPoint(point.x, point.y)) ? point : null;
  });
  if (!wheelPoint) fail('files.selection-cache-compaction: source top-left wheel target is obscured');
  await page.mouse.move(wheelPoint.x, wheelPoint.y);
  const wheelReady = await page.evaluate(({ x, y }) =>
    document.querySelector('pre.file-explorer__code')?.contains(document.elementFromPoint(x, y))
      && document.activeElement === document.querySelector('textarea.file-conversation__input'), wheelPoint);
  if (!wheelReady) fail('files.selection-cache-compaction: wheel target or composer focus changed');
  for (let cursor = 400; cursor <= 40000; cursor += 400) {
    const remaining = deadline - Date.now();
    if (remaining <= 0) fail(`files.selection-cache-compaction: pagination deadline at cursor ${cursor}`);
    await page.mouse.wheel(0, 10000000);
    await waitFor(page, (lastLine) => document.getElementById(`file-explorer-line-${lastLine}`) != null,
      `files.selection-cache-compaction: page at cursor ${cursor} did not render`, remaining, cursor + 400);
    const requests = filesWsOf(sentFrames.slice(frameMark), 'file_explorer_read', selectionWindowFile)
      .filter((frame) => frame.cursor === cursor);
    const request = requests[0];
    if (requests.length !== 1 || request.sessionId !== owner) {
      fail(`files.selection-cache-compaction: cursor ${cursor} must have one owner request (${JSON.stringify(requests)})`);
    }
    const response = receivedFrames.map(parseSentFrame).find((frame) => frame?.type === 'response' && frame.id === request.id);
    if (response?.success !== true || response.data?.path !== selectionWindowFile
      || response.data?.nextCursor !== (cursor < 40000 ? cursor + 400 : undefined)
      || response.data?.totalLines !== 40400 || response.data?.sizeBytes !== 80800
      || response.data?.lines?.length !== 400 || response.data.lines.some((line) => line !== 'x')) {
      fail(`files.selection-cache-compaction: cursor ${cursor} response mismatch (${JSON.stringify(response)})`);
    }
  }
  const windowState = await page.evaluate(() => {
    const rows = document.querySelectorAll('pre.file-explorer__code [id^="file-explorer-line-"]');
    return {
      first: rows[0]?.id, last: rows[rows.length - 1]?.id, count: rows.length,
      oldRows: [4, 5, 6, 7].some((line) => document.getElementById(`file-explorer-line-${line}`) != null),
      composerFocused: document.activeElement === document.querySelector('textarea.file-conversation__input'),
      disclosure: document.querySelector('.file-conversation__context')?.textContent,
    };
  });
  if (windowState.first !== 'file-explorer-line-40001' || windowState.last !== 'file-explorer-line-40400'
    || windowState.count !== 400 || windowState.oldRows || !windowState.composerFocused
    || windowState.disclosure !== 'Selection lines 4–7') {
    fail(`files.selection-cache-compaction: retained selection/window mismatch (${JSON.stringify(windowState)})`);
  }
  return windowState;
}

async function runSelectionCacheCompactionJourney(page, sentFrames, receivedFrames) {
  const label = 'files.selection-cache-compaction';
  const owner = await displayedExplorerSession(page);
  if (!owner) fail(`${label}: displayed session is empty`);
  await page.setViewportSize({ width: 1360, height: 900 });
  await openFileExplorer(page);
  await expandFilesTree(page);
  const frameMark = sentFrames.length;
  await clickFilesRow(page, selectionWindowFile);
  await waitFor(page, () => document.getElementById('file-explorer-line-400') != null,
    `${label}: first source page missing`);
  await clickVisibleAsk(page, label);
  await selectSourceRange(page, 4, 7, label);
  const question = `${label}-${laneNonce}`;
  const input = page.locator('textarea.file-conversation__input');
  await input.click();
  await input.fill(question);
  const disclosure = await page.locator('.file-conversation__context').textContent();
  if (disclosure !== 'Selection lines 4–7') fail(`${label}: composer focus lost selection (${disclosure})`);
  await laneShot(page, 'files-selection-cache-compaction-before.png');
  const windowState = await paginateSelectionWindow(page, sentFrames, receivedFrames, owner, frameMark);
  await laneShot(page, 'files-selection-cache-compaction-before-send.png');
  await page.locator('section.file-conversation .file-conversation__action--send').click();
  await waitFor(page, () => document.querySelector('textarea.file-conversation__input')?.value === '',
    `${label}: prompt was not accepted`);
  const prompts = fileConversationFrames(sentFrames.slice(frameMark), 'file_conversation_prompt')
    .filter((frame) => frame.message === question);
  const prompt = prompts[0];
  const opened = filesWsOf(sentFrames.slice(frameMark), 'file_conversation_open', selectionWindowFile);
  const response = receivedFrames.map(parseSentFrame).find((frame) => frame?.type === 'response' && frame.id === prompt?.id);
  const afterDisclosure = await page.locator('.file-conversation__context').textContent();
  await laneShot(page, 'files-selection-cache-compaction-after-response.png');
  const evidence = { windowState, disclosure, afterDisclosure, opened, prompts, response,
    pages: filesWsOf(sentFrames.slice(frameMark), 'file_explorer_read', selectionWindowFile) };
  const name = 'files-selection-cache-compaction-wire.json';
  const staged = stageWrite(name, `${JSON.stringify(evidence, null, 2)}\n`);
  const keep = process.env.RPI_FAILURE_EVIDENCE_DIR || process.env.RPI_EVIDENCE;
  if (keep) fs.copyFileSync(staged, path.join(keep, name));
  if (prompts.length !== 1 || opened.length !== 1 || opened[0].sessionId !== owner
    || prompt.sessionId !== owner || prompt.conversationId !== opened[0].conversationId
    || prompt.selection?.startLine !== 4 || prompt.selection?.endLine !== 7
    || afterDisclosure !== disclosure) {
    fail(`${label}: disclosed lines 4–7 do not match the owner prompt (${JSON.stringify(evidence)})`);
  }
  const context = response?.data?.context;
  if (response?.success !== true || response.data?.accepted !== true
    || response.data?.conversationId !== prompt.conversationId || response.data?.path !== selectionWindowFile
    || context?.requestedRange?.startLine !== 4 || context?.requestedRange?.endLine !== 7
    || context?.includedRange?.startLine !== 4 || context?.includedRange?.endLine !== 7) {
    fail(`${label}: accepted context is not the disclosed lines 4–7 (${JSON.stringify(response)})`);
  }
  await waitForFixtureReply(page, question, label);
}

// files.map: the current-file dependency map on the owner. The committed
// clean MapRoot.sol is a real extractor fixture: ./MapDep.sol resolves (the
// forward neighbor), forge-std/Test.sol is external, ./MissingMap.sol is
// unresolved (no supported relative candidate in the bounded scan), and
// MapCaller.sol imports the root back (the reverse dependent). Purpose is
// the fixture's verbatim natspec, never a generated description. The map
// renders through MarkdownBody's shared Mermaid surface (actual SVG);
// file navigation is the node list's exact open through the existing
// explorer owner. Stale request proof: the response that lands after the
// user switched files never paints a map for the wrong file.
async function runFileMapJourney(page, sentFrames) {
  await page.setViewportSize({ width: 1360, height: 900 });
  await waitFor(page, () => window.innerWidth === 1360 && window.innerHeight === 900, 'files.map desktop viewport', 10000);
  if ((await page.locator('section.file-explorer').count()) === 0) await openFileExplorer(page);
  await expandFilesTree(page);
  await clickFilesRow(page, solMapRootFile);
  const mapToggle = page.getByRole('radio', { name: 'Dependency map for this file' });
  await waitFor(
    page,
    () => {
      const control = document.querySelector('[data-testid="file-explorer-map-toggle"]');
      return control != null
        && control.getBoundingClientRect().width > 0
        && control.getBoundingClientRect().height > 0;
    },
    'files.map: Map surface control is not visible for the file tab',
    25000,
  );
  await mapToggle.click();
  await waitFor(
    page,
    () => document.querySelector('[data-testid="file-dependency-map"]') != null
      && document.querySelector('[data-testid="file-explorer-map-pane"] svg') != null,
    'files.map: the map pane never rendered an actual SVG diagram',
    30000,
  );
  const mapProof = await page.evaluate(({ rootPath, depPath, callerPath }) => {
    const pane = document.querySelector('[data-testid="file-explorer-map-pane"]');
    const svg = pane?.querySelector('svg');
    const nodes = Array.from(document.querySelectorAll('[data-testid="file-dependency-map-nodes"] button.file-explorer__map-node[data-map-node-path]'));
    const outgoing = document.querySelector('[data-testid="file-dependency-map-outgoing"]')?.textContent || '';
    const incoming = document.querySelector('[data-testid="file-dependency-map-incoming"]')?.textContent || '';
    const coverage = document.querySelector('[data-testid="file-dependency-map-coverage"]')?.textContent || '';
    return {
      svg: svg != null && svg.getBoundingClientRect().width > 0,
      selected: nodes.find((node) => node.getAttribute('aria-current') === 'true')?.getAttribute('data-map-node-path') || '',
      nodePaths: nodes.map((node) => node.getAttribute('data-map-node-path') || ''),
      outgoingHasDep: outgoing.includes(depPath) && nodes.some((node) => node.getAttribute('data-map-node-path') === depPath && (node.textContent || '').includes('MapDepVault')),
      outgoingExternal: outgoing.includes('unresolved forge-std/Test.sol'),
      outgoingUnresolved: outgoing.includes('unresolved ./MissingMap.sol'),
      incomingHasCaller: incoming.includes(callerPath),
      purposeRoot: nodes.some((node) => node.getAttribute('data-map-node-path') === rootPath && (node.textContent || '').includes('Root ledger contract for the map fixture')),
      purposeCaller: nodes.some((node) => node.getAttribute('data-map-node-path') === callerPath && (node.textContent || '').includes('Caller contract for the map fixture')),
      coverageHonest: coverage.includes('Visible workspace entries only') && coverage.includes('Coverage is incomplete.'),
      svgNoScript: svg?.querySelector('script, foreignObject') == null,
    };
  }, { rootPath: solMapRootFile, depPath: solMapDepFile, callerPath: solMapCallerFile });
  if (!mapProof.svg) fail('files.map: the rendered diagram SVG has no visible box');
  if (mapProof.svgNoScript === false) fail('files.map: the rendered SVG carries script/foreignObject');
  if (mapProof.selected !== solMapRootFile) fail(`files.map: the selected node is ${JSON.stringify(mapProof.selected)}, not ${solMapRootFile}`);
  if (mapProof.nodePaths.length !== 3
    || !mapProof.nodePaths.includes(solMapRootFile)
    || !mapProof.nodePaths.includes(solMapDepFile)
    || !mapProof.nodePaths.includes(solMapCallerFile)) {
    fail(`files.map: node list is ${JSON.stringify(mapProof.nodePaths)}, expected the root plus both file neighbors`);
  }
  if (!mapProof.outgoingHasDep) fail('files.map: outgoing does not name the resolved MapDep.sol neighbor with its declaration');
  if (!mapProof.outgoingExternal) fail('files.map: outgoing does not disclose the unresolved package import');
  if (!mapProof.outgoingUnresolved) fail('files.map: outgoing does not disclose the unresolved ./MissingMap.sol import');
  if (!mapProof.incomingHasCaller) fail('files.map: incoming does not name the reverse MapCaller.sol dependent');
  if (!mapProof.purposeRoot || !mapProof.purposeCaller) fail(`files.map: purpose lines are not the fixtures' verbatim natspec (${JSON.stringify(mapProof)})`);
  if (!mapProof.coverageHonest) fail(`files.map: coverage does not disclose visible-entries-only/incomplete (${JSON.stringify(mapProof)})`);
  await laneShot(page, 'files-map-root.png', { fullPage: true });
  const mapScroller = page.locator('[data-testid="file-dependency-map"]');
  const mapReach = await mapScroller.evaluate((node) => {
    const max = node.scrollHeight - node.clientHeight;
    const nodes = node.querySelector('[data-testid="file-dependency-map-nodes"]');
    const coverage = node.querySelector('[data-testid="file-dependency-map-coverage"]');
    const box = node.getBoundingClientRect();
    const inside = (target) => {
      if (!target || target.getClientRects().length === 0) return false;
      const rect = target.getBoundingClientRect();
      return rect.bottom <= box.bottom + 1 && rect.top < box.bottom;
    };
    if (max > 8) {
      node.scrollTop = max;
      node.dispatchEvent(new Event('scroll', { bubbles: true }));
    }
    return { scrollable: max > 8, scrollTop: node.scrollTop, nodes: inside(nodes), coverage: inside(coverage) };
  });
  if (!mapReach.nodes || !mapReach.coverage || (mapReach.scrollable && mapReach.scrollTop <= 0)) {
    fail(`files.map: bottom nodes and coverage are not reachable (${JSON.stringify(mapReach)})`);
  }
  const selectedMapOverlay = await page.evaluate(() => (
    document.querySelector('[data-testid="file-selection-actions"]') != null
    || document.querySelector('.file-explorer__selection-float:not(.is-hidden)') != null
  ));
  if (selectedMapOverlay) fail('files.map: the selected map shows a source selection overlay');

  // Node-list navigation: the resolved neighbor opens through the owner,
  // exactly like a tree row, and the new file's tab becomes active.
  const depButton = page.locator(`[data-testid="file-dependency-map-nodes"] button.file-explorer__map-node[data-map-node-path="${solMapDepFile}"]`);
  await depButton.click();
  await waitFor(
    page,
    (path) => (document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || '') === path
      && document.querySelector('[data-testid="file-explorer-map-pane"]') == null,
    'files.map: clicking the MapDep node did not open the exact file through the owner',
    25000,
    solMapDepFile,
  );
  await waitFor(
    page,
    () => document.querySelector('pre.file-explorer__code') != null || document.querySelector('.file-explorer__markdown') != null,
    'files.map: the opened neighbor shows neither source nor preview content',
    25000,
  );
  await laneShot(page, 'files-map-neighbor-open.png');

  // Stale-response contract: re-open the root, arm the response hold, click
  // Map, then switch files BEFORE the response lands. The held response
  // releases onto a DIFFERENT active file, and must paint nothing for it.
  await clickFilesRow(page, solMapRootFile);
  await page.evaluate(() => window.__rpiWarmReconnect.holdNextFileExplorerMap());
  const staleArmMark = sentFrames.length;
  await mapToggle.click();
  await waitFor(
    page,
    () => window.__rpiWarmReconnect.heldMapCount() > 0,
    'files.map: the stale leg never dispatched file_explorer_map',
    15000,
    staleArmMark,
  );
  await clickFilesRow(page, cleanFile);
  await page.evaluate(() => window.__rpiWarmReconnect.releaseHeldFileExplorerMap());
  await waitFor(
    page,
    (path) => (document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || '') === path
      && document.querySelector('[data-testid="file-dependency-map"]') == null,
    'files.map: the clean file became active but the stale map still painted',
    15000,
    cleanFile,
  );
  const staleProof = await page.evaluate((rootName) => {
    const content = document.querySelector('[data-testid="file-explorer-content"]')?.textContent || '';
    return {
      mapPaneGone: document.querySelector('[data-testid="file-explorer-map-pane"]') == null,
      noRootLeak: !content.includes('Root ledger contract for the map fixture') && !content.includes('MapDepVault'),
      activeRow: (document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || ''),
    };
  }, solMapRootFile);
  if (staleProof.activeRow !== cleanFile) fail(`files.map: the active row after the stale release is ${JSON.stringify(staleProof.activeRow)}, not ${cleanFile}`);
  if (!staleProof.mapPaneGone || !staleProof.noRootLeak) fail(`files.map: the stale response leaked map content onto ${cleanFile} (${JSON.stringify(staleProof)})`);
  await laneShot(page, 'files-map-stale-switch.png');

  // Mobile 390x844 evidence: the map surface and its SVG stay usable.
  await page.setViewportSize({ width: 390, height: 844 });
  await waitFor(page, () => window.innerWidth === 390 && window.innerHeight === 844, 'files.map mobile viewport', 10000);
  await clickFilesRow(page, solMapRootFile);
  await mapToggle.click();
  await waitFor(
    page,
    () => document.querySelector('[data-testid="file-dependency-map"]') != null
      && document.querySelector('[data-testid="file-explorer-map-pane"] svg') != null,
    'files.map: mobile never rendered the map SVG',
    30000,
  );
  const overflowMobile = await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth);
  if (!overflowMobile) fail('files.map: mobile map pane overflows horizontally');
  await laneShot(page, 'files-map-mobile.png', { fullPage: true });
  await page.setViewportSize({ width: 1360, height: 900 });
  await waitFor(page, () => window.innerWidth === 1360 && window.innerHeight === 900, 'files.map restore viewport', 10000);
  // Restore the exact state the fullscreen journey expects: the clean file
  // active on its Source surface, and the Ask conversation untouched.
  await clickFilesRow(page, cleanFile);
  await waitFor(
    page,
    (path) => (document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || '') === path
      && document.querySelector('[data-testid="file-explorer-map-pane"]') == null,
    'files.map: clean-file restore before fullscreen failed',
    25000,
    cleanFile,
  );
}
async function sendFileQuestion(page, question, label) {
  const input = page.locator('textarea.file-conversation__input');
  await input.click();
  await input.fill(question);
  const send = page.locator('section.file-conversation .file-conversation__action--send');
  const box = await send.boundingBox();
  if (!box || box.width < 1 || box.height < 1) fail(`${label}: Send is not visible`);
  await send.click();
}

function settledFileFixtureReply(want) {
  const dock = document.querySelector('section.file-conversation');
  if (!dock || typeof want !== 'string' || want === '') return null;
  const conversationId = dock.getAttribute('data-conversation-id') || '';
  if (!conversationId) return null;
  const entries = Array.from(dock.querySelectorAll('.file-conversation__transcript .side-chat__entry'));
  let userIndex = -1;
  for (let index = 0; index < entries.length; index += 1) {
    const entry = entries[index];
    if (!entry.classList.contains('side-chat__entry--user')) continue;
    const text = (entry.querySelector('.side-chat__text')?.textContent || '').trim();
    if (text === want) userIndex = index;
  }
  if (userIndex < 0) return null;
  let end = entries.length;
  for (let index = userIndex + 1; index < entries.length; index += 1) {
    if (entries[index].classList.contains('side-chat__entry--user')) {
      end = index;
      break;
    }
  }
  const streaming = dock.querySelector('.file-conversation__action--stop') !== null
    || dock.querySelector('.side-chat__typing') !== null;
  let assistant = null;
  for (let index = end - 1; index > userIndex; index -= 1) {
    const entry = entries[index];
    if (!entry.classList.contains('side-chat__entry--assistant')) continue;
    if (!entry.querySelector('.side-chat__text')) continue;
    assistant = entry;
    break;
  }
  if (!assistant || streaming) return null;
  if (assistant.classList.contains('side-chat__entry--error') || assistant.classList.contains('side-chat__entry--partial')) return null;
  const assistantText = assistant.querySelector('.side-chat__text')?.textContent || '';
  const fixture = /steer-\d+-ing stream chunk-four-done/.test(assistantText) || assistantText.includes('steering-followup-reply');
  if (!fixture) return null;
  return {
    user: true,
    assistant: assistantText,
    fixture: true,
    path: dock.querySelector('.file-conversation__path')?.textContent || '',
    streaming: false,
    conversationId,
  };
}

async function waitForFixtureReply(page, question, label) {
  await waitFor(
    page,
    settledFileFixtureReply,
    `${label}: fixture model reply for ${JSON.stringify(question)} did not settle on the current file conversation`,
    30000,
    question,
  );
  const reply = await page.evaluate(settledFileFixtureReply, question);
  if (!reply?.user || !reply.fixture || reply.streaming || !reply.conversationId) {
    fail(`${label}: fixture reply for ${JSON.stringify(question)} did not settle on the current file conversation (${JSON.stringify(reply)})`);
  }
  return reply;
}

async function runFileAskJourney(page, sentFrames) {
  const owner = await displayedExplorerSession(page);
  if (!owner) fail('files.ask: displayed session id is empty before Ask');
  await page.setViewportSize({ width: 1360, height: 900 });
  await waitFor(page, () => window.innerWidth === 1360 && window.innerHeight === 900, 'files.ask desktop viewport did not become 1360x900');
  if ((await page.locator('section.file-explorer').count()) === 0) await openFileExplorer(page);
  await clickFilesRow(page, solFile);
  await waitFor(
    page,
    () => document.querySelector('pre.file-explorer__code') !== null,
    `files.ask: ${solFile} source did not render`
  );
  const opensBefore = fileConversationFrames(sentFrames, 'file_conversation_open').length;
  const promptsBefore = fileConversationFrames(sentFrames, 'file_conversation_prompt').length;
  const closesBefore = fileConversationFrames(sentFrames, 'file_conversation_close').length;
  await clickVisibleAsk(page, 'desktop');
  await assertAskChrome(page, solFile, 'desktop ask open');
  const question = `files-ask-${laneNonce || 'lane'}-token`;
  const promptMark = sentFrames.length;
  await sendFileQuestion(page, question, 'desktop plain');
  await waitFor(
    page,
    (want) => document.querySelector('section.file-conversation .file-conversation__path')?.textContent === want,
    `files.ask: ${solFile} dock did not bind after send`,
    25000,
    solFile
  );
  let plainFrames = [];
  const plainDeadline = Date.now() + 15000;
  while (Date.now() < plainDeadline) {
    plainFrames = sentFrames.slice(promptMark).map(parseSentFrame).filter(Boolean);
    if (plainFrames.some((frame) => frame.type === 'file_conversation_open')
      && plainFrames.some((frame) => frame.type === 'file_conversation_prompt')) break;
    await page.waitForTimeout(50);
  }
  const opens = plainFrames.filter((frame) => frame.type === 'file_conversation_open');
  const prompts = plainFrames.filter((frame) => frame.type === 'file_conversation_prompt');
  if (opens.length !== 1) fail(`files.ask: expected exactly one file_conversation_open, got ${JSON.stringify(opens)}`);
  const conversationId = opens[0].conversationId;
  if (!FILE_CONVERSATION_ID_RE.test(conversationId)) {
    fail(`files.ask: conversation id is not a UUID (${JSON.stringify(conversationId)})`);
  }
  if (opens[0].path !== solFile || opens[0].sessionId !== owner) {
    fail(`files.ask: open owner/path ${JSON.stringify(opens[0])} != ${owner} ${solFile}`);
  }
  if (prompts.length !== 1 || prompts[0].conversationId !== conversationId || prompts[0].message !== question
    || prompts[0].sessionId !== owner || 'selection' in prompts[0]) {
    fail(`files.ask: plain prompt is not the same id without a selection (${JSON.stringify(prompts)})`);
  }
  const plainReply = await waitForFixtureReply(page, question, 'desktop plain');
  if (!plainReply.user || !isFixtureModelReply(plainReply.assistant) || plainReply.path !== solFile || plainReply.streaming) {
    fail(`files.ask: fixture reply did not settle on ${solFile} (${JSON.stringify(plainReply)})`);
  }
  await laneShot(page, 'files-ask-desktop.png', { fullPage: true });
  await assertLongSourceRoundTrip(page, 'files.ask Ask to Source', async () => {
    await clickVisibleAsk(page, 'notes source scroll');
    await waitFor(page, () => document.querySelector('section.file-conversation') != null, 'files.ask: Ask did not open on the long source', 15000);
  }, async () => {
    await page.getByRole('radio', { name: 'Source', exact: true }).click();
    await waitFor(page, () => document.querySelector('pre.file-explorer__code') != null && document.querySelector('section.file-conversation') == null, 'files.ask: Source did not replace Ask', 15000);
  });
  await clickFilesRow(page, solFile);
  await waitFor(page, () => document.querySelector('section.file-conversation') != null, 'files.ask: Token Ask did not return before the selected range', 15000);

  const rangeStart = 4;
  const rangeEnd = 7;
  await selectSourceRange(page, rangeStart, rangeEnd, 'desktop selection');
  const selectedQuestion = `${question}-lines-${rangeStart}-${rangeEnd}`;
  const selectedMark = sentFrames.length;
  await sendFileQuestion(page, selectedQuestion, 'desktop selection');
  await waitFor(
    page,
    (want) => document.querySelector('.file-conversation__action--stop') !== null
      || Array.from(document.querySelectorAll('.side-chat__entry--user')).some((entry) => (entry.textContent || '').includes(want)),
    'files.ask: selected-range prompt did not start',
    25000,
    selectedQuestion
  );
  const selectedPrompts = sentFrames.slice(selectedMark).map(parseSentFrame)
    .filter((frame) => frame?.type === 'file_conversation_prompt');
  const selectedOpens = sentFrames.slice(selectedMark).map(parseSentFrame)
    .filter((frame) => frame?.type === 'file_conversation_open');
  if (selectedOpens.length !== 0) fail(`files.ask: selection reused the open instead of the same id (${JSON.stringify(selectedOpens)})`);
  if (selectedPrompts.length !== 1 || selectedPrompts[0].conversationId !== conversationId
    || selectedPrompts[0].sessionId !== owner || selectedPrompts[0].message !== selectedQuestion
    || selectedPrompts[0].selection?.startLine !== rangeStart
    || selectedPrompts[0].selection?.endLine !== rangeEnd) {
    fail(`files.ask: selected prompt is not exact id ${conversationId} lines ${rangeStart}-${rangeEnd} (${JSON.stringify(selectedPrompts)})`);
  }
  await waitFor(
    page,
    () => document.querySelector('.file-conversation__action--stop') !== null,
    'files.ask: selected prompt did not reach a visible Stop control',
    20000
  );
  const abortMark = sentFrames.length;
  const stop = page.getByRole('button', { name: 'Stop generating' });
  const stopBox = await stop.boundingBox();
  if (!stopBox || stopBox.width < 1 || stopBox.height < 1) fail('files.ask: Stop is not visible');
  await stop.click();
  await waitFor(
    page,
    () => document.querySelector('.file-conversation__action--stop') === null
      && document.querySelector('.file-conversation__action--send') !== null,
    'files.ask: Stop did not return the dock to Send'
  );
  const aborts = sentFrames.slice(abortMark).map(parseSentFrame).filter((frame) => frame?.type === 'file_conversation_abort');
  if (aborts.length !== 1 || aborts[0].conversationId !== conversationId || aborts[0].sessionId !== owner) {
    fail(`files.ask: Stop did not abort the exact conversation id (${JSON.stringify(aborts)})`);
  }
  const retained = await page.evaluate((want) => Array.from(document.querySelectorAll('.side-chat__entry--user'))
    .some((entry) => (entry.textContent || '').includes(want)), question);
  if (!retained) fail('files.ask: Stop dropped the first file question');

  await page.setViewportSize({ width: 390, height: 844 });
  await waitFor(page, () => window.innerWidth === 390 && window.innerHeight === 844, 'files.ask switch viewport did not become 390x844');
  await expandFilesTree(page);
  await clickFilesRow(page, cleanFile);
  await waitFor(
    page,
    () => document.querySelector('section.file-conversation') === null
      && document.querySelector('pre.file-explorer__code') !== null,
    `files.ask: switching to ${cleanFile} did not leave the first conversation dock`
  );
  const secondMark = sentFrames.length;
  await clickVisibleAsk(page, 'second file');
  const secondQuestion = `files-second-${laneNonce || 'lane'}-clean`;
  await sendFileQuestion(page, secondQuestion, 'second file');
  await waitFor(
    page,
    (want) => document.querySelector('section.file-conversation .file-conversation__path')?.textContent === want,
    `files.ask: second dock did not bind ${cleanFile}`,
    25000,
    cleanFile
  );
  let secondFrames = [];
  const secondDeadline = Date.now() + 15000;
  while (Date.now() < secondDeadline) {
    secondFrames = sentFrames.slice(secondMark).map(parseSentFrame).filter(Boolean);
    if (secondFrames.some((frame) => frame.type === 'file_conversation_open')
      && secondFrames.some((frame) => frame.type === 'file_conversation_prompt')) break;
    await page.waitForTimeout(50);
  }
  const secondOpens = secondFrames.filter((frame) => frame.type === 'file_conversation_open');
  const secondPrompts = secondFrames.filter((frame) => frame.type === 'file_conversation_prompt');
  if (secondOpens.length !== 1 || secondOpens[0].path !== cleanFile || secondOpens[0].sessionId !== owner
    || secondOpens[0].conversationId === conversationId || !FILE_CONVERSATION_ID_RE.test(secondOpens[0].conversationId)) {
    fail(`files.ask: second file did not open its own owner-scoped conversation (${JSON.stringify(secondOpens)})`);
  }
  if (secondPrompts.length !== 1 || secondPrompts[0].conversationId !== secondOpens[0].conversationId
    || secondPrompts[0].message !== secondQuestion || 'selection' in secondPrompts[0]) {
    fail(`files.ask: second prompt is not its own id (${JSON.stringify(secondPrompts)})`);
  }
  const secondReply = await waitForFixtureReply(page, secondQuestion, 'second file');
  if (secondReply.path !== cleanFile || !isFixtureModelReply(secondReply.assistant)) {
    fail(`files.ask: second file reply did not settle (${JSON.stringify(secondReply)})`);
  }
  const secondShowsFirst = await page.locator('.side-chat__entry--user').evaluateAll(
    (entries, marker) => entries.some((entry) => (entry.textContent || '').trimStart().startsWith(marker)),
    question,
  );
  if (secondShowsFirst) fail('files.ask: second file rendered the first file history');
  await laneShot(page, 'files-ask-second-mobile.png', { fullPage: true });

  await expandFilesTree(page);

  await clickFilesRow(page, solFile);
  await waitFor(
    page,
    (want) => document.querySelector('section.file-conversation .file-conversation__path')?.textContent === want.path
      && Array.from(document.querySelectorAll('.side-chat__entry--user')).some((entry) => (entry.textContent || '').trimStart().startsWith(want.question))
      && !Array.from(document.querySelectorAll('.side-chat__entry--user')).some((entry) => (entry.textContent || '').includes(want.second)),
    'files.ask: returning to the first file lost its retained history',
    20000,
    { path: solFile, question, second: secondQuestion }
  );
  const returnOpens = sentFrames.slice(secondMark).map(parseSentFrame)
    .filter((frame) => frame?.type === 'file_conversation_open' && frame.path === solFile);
  if (returnOpens.length !== 0) fail(`files.ask: returning reopened the retained conversation (${JSON.stringify(returnOpens)})`);
  const retainedId = await page.getAttribute('section.file-conversation', 'data-conversation-id');
  if (retainedId !== conversationId) fail(`files.ask: retained dock id ${retainedId} != ${conversationId}`);

  await page.getByRole('button', { name: 'Expand file tree' }).click();
  await waitFor(
    page,
    () => {
      const content = document.querySelector('[data-testid="file-explorer-content"]');
      const tree = document.querySelector('[data-testid="file-explorer-tree"]');
      return content?.hidden === true && tree?.hidden !== true;
    },
    'files.ask: Back did not return to the file tree'
  );
  const backCloses = sentFrames.slice(abortMark).map(parseSentFrame).filter((frame) => frame?.type === 'file_conversation_close');
  if (backCloses.length !== 0) fail(`files.ask: Back closed a conversation (${JSON.stringify(backCloses)})`);
  await clickFilesRow(page, solFile);
  await waitFor(
    page,
    (want) => document.querySelector('section.file-conversation')?.getAttribute('data-conversation-id') === want,
    'files.ask: Back lost the first file conversation',
    20000,
    conversationId
  );

  await page.locator(FILES_DRAWER_CLOSE).click();
  await waitFor(
    page,
    () => document.querySelector('section.file-explorer') === null,
    'files.ask: drawer close did not unmount the explorer'
  );
  const hideCloses = sentFrames.slice(abortMark).map(parseSentFrame).filter((frame) => frame?.type === 'file_conversation_close');
  if (hideCloses.length !== 0) fail(`files.ask: Files close closed a conversation (${JSON.stringify(hideCloses)})`);
  await openFileExplorer(page);
  await waitFor(
    page,
    (want) => document.querySelector('section.file-conversation')?.getAttribute('data-conversation-id') === want.id
      && document.querySelector('.file-conversation__path')?.textContent === want.path
      && Array.from(document.querySelectorAll('.side-chat__entry--user')).some((entry) => (entry.textContent || '').trimStart().startsWith(want.question)),
    'files.ask: reopening Files restored the wrong owner conversation',
    20000,
    { id: conversationId, path: solFile, question }
  );
  const closesAtEnd = fileConversationFrames(sentFrames, 'file_conversation_close');
  if (closesAtEnd.length !== closesBefore) {
    fail(`files.ask: journey emitted a file_conversation_close (${closesBefore} -> ${closesAtEnd.length})`);
  }
  if (fileConversationFrames(sentFrames, 'file_conversation_open').length !== opensBefore + 2) {
    fail('files.ask: open count is not exactly the two owner-scoped conversations');
  }
  if (fileConversationFrames(sentFrames, 'file_conversation_prompt').length !== promptsBefore + 3) {
    fail('files.ask: prompt count is not exactly the plain, selected, and second-file questions');
  }
  const foreign = sentFrames.slice(promptMark).map(parseSentFrame).filter((frame) => (
    typeof frame?.type === 'string' && frame.type.startsWith('file_conversation_') && frame.sessionId !== owner
  ));
  if (foreign.length !== 0) fail(`files.ask: a file conversation frame named another owner (${JSON.stringify(foreign)})`);

  await assertAskChrome(page, solFile, 'mobile ask', { touch: true, source: false });
  const mobileHistory = await page.evaluate((want) => Array.from(document.querySelectorAll('.side-chat__entry--user'))
    .some((entry) => (entry.textContent || '').trimStart().startsWith(want)), question);
  if (!mobileHistory) fail('files.ask: mobile dock lost the first file history');
  await laneShot(page, 'files-ask-mobile.png', { fullPage: true });
  await page.setViewportSize({ width: 1360, height: 900 });
  await waitFor(page, () => window.innerWidth === 1360 && window.innerHeight === 900, 'desktop viewport did not restore after files.ask');
  await hideFileExplorer(page, 'files.ask restore before mobile');
}


function sameHeaderGeometry(before, after) {
  if (!before?.header || !after?.header) return false;
  return before.header.x === after.header.x
    && before.header.y === after.header.y
    && before.header.width === after.header.width
    && before.header.height === after.header.height
    && before.header.display === after.header.display
    && before.headerHeight === after.headerHeight;
}

async function assertFilesFullscreenLeg(page, label, { askId }) {
  await waitFor(page, () => {
    const sample = window.__filesFullscreenSample();
    const header = sample.header;
    return header != null && header.display !== 'none' && header.height > 0
      && sample.explorerTop === header.height;
  }, `${label}: ordinary explorer top did not settle on the live header bottom`);
  const before = await page.evaluate(() => window.__filesFullscreenSample());
  await page.evaluate((sample) => { window.__filesFullscreenBefore = sample; }, before);
  if (before.expandedClass) fail(`${label}: Files started already expanded`);
  if (!before.header || before.header.display === 'none' || before.header.height < 1) {
    fail(`${label}: global header was not visible before expand (${JSON.stringify(before.header)})`);
  }
  if (before.explorerTop !== before.header.height) {
    fail(`${label}: ordinary explorer top ${before.explorerTop} is not the header bottom ${before.header?.height}`);
  }
  const expand = page.getByRole('button', { name: 'Expand file explorer to the full app area' });
  await expand.click();
  await waitFor(
    page,
    () => {
      const sample = window.__filesFullscreenSample();
      const headerHidden = sample.header == null || sample.header.display === 'none' || sample.header.height === 0;
      return sample.expandedClass && headerHidden && sample.explorerTop === 0
        && sample.headerHeight === '0px'
        && sample.expandLabel === 'Restore file explorer'
        && sample.expandReachable && sample.closeReachable;
    },
    `${label}: expanded Files did not hide the global header at top 0 with Restore and Close reachable`,
  );
  await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  const expanded = await page.evaluate(() => window.__filesFullscreenSample());
  if (expanded.explorerTop !== 0 || expanded.headerHeight !== '0px') {
    fail(`${label}: fullscreen geometry drifted after paint (${JSON.stringify(expanded)})`);
  }
  if (askId && expanded.askId !== askId) fail(`${label}: fullscreen dropped Ask conversation ${askId} (got ${expanded.askId})`);
  await laneShot(page, `files-fullscreen-${label.replace(/\s+/g, '-')}.png`, { fullPage: true });
  await page.getByRole('button', { name: 'Restore file explorer' }).click();
  await waitFor(
    page,
    () => {
      const sample = window.__filesFullscreenSample();
      return !sample.expandedClass
        && sample.expandLabel === 'Expand file explorer to the full app area'
        && sample.header != null
        && sample.header.display !== 'none'
        && sample.header.height > 0
        && sample.explorerTop === sample.header.height;
    },
    `${label}: restore did not bring the global header back before the explorer`,
  );
  const restored = await page.evaluate(() => window.__filesFullscreenSample());
  if (!sameHeaderGeometry(before, restored)) {
    fail(`${label}: restored header geometry ${JSON.stringify(restored.header)}/${restored.headerHeight} != ${JSON.stringify(before.header)}/${before.headerHeight}`);
  }
  if (askId && restored.askId !== askId) fail(`${label}: restore dropped Ask conversation ${askId}`);
  await page.getByRole('button', { name: 'Expand file explorer to the full app area' }).click();
  await waitFor(
    page,
    () => window.__filesFullscreenSample().explorerTop === 0 && window.__filesFullscreenSample().headerHeight === '0px',
    `${label}: second expand did not return the explorer to top 0`,
  );
  await page.locator('#file-explorer-drawer-close-btn').click();
  await waitFor(
    page,
    () => {
      const header = document.querySelector('header');
      const box = header?.getBoundingClientRect();
      return document.querySelector('section.file-explorer') == null
        && !document.querySelector('.app-layout')?.classList.contains('app-layout--files-expanded')
        && header != null
        && getComputedStyle(header).display !== 'none'
        && box != null
        && Math.round(box.height) === window.__filesFullscreenBefore.header.height
        && getComputedStyle(document.documentElement).getPropertyValue('--app-header-height').trim() === window.__filesFullscreenBefore.headerHeight;
    },
    `${label}: fullscreen close did not restore the original global header`,
  );
}

async function runFilesFullscreenJourney(page) {
  await page.evaluate(() => {
    window.__filesFullscreenSample = () => {
      const header = document.querySelector('header');
      const explorer = document.querySelector('section.file-explorer');
      const expand = document.getElementById('file-explorer-expand-btn');
      const close = document.getElementById('file-explorer-drawer-close-btn');
      const rect = (node) => {
        if (!node || node.getClientRects().length === 0) return null;
        const box = node.getBoundingClientRect();
        const style = getComputedStyle(node);
        if (style.display === 'none' || style.visibility === 'hidden') return { x: Math.round(box.x), y: Math.round(box.y), width: Math.round(box.width), height: 0, display: style.display };
        return { x: Math.round(box.x), y: Math.round(box.y), width: Math.round(box.width), height: Math.round(box.height), display: style.display };
      };
      const explorerBox = explorer ? explorer.getBoundingClientRect() : null;
      return {
        expandedClass: document.querySelector('.app-layout')?.classList.contains('app-layout--files-expanded') === true,
        headerHeight: getComputedStyle(document.documentElement).getPropertyValue('--app-header-height').trim(),
        header: rect(header),
        explorerTop: explorerBox && explorer.getClientRects().length > 0 ? Math.round(explorerBox.top) : null,
        expandLabel: expand?.getAttribute('aria-label') || '',
        expandReachable: !!(expand && expand.getClientRects().length > 0 && expand.getBoundingClientRect().width >= 1),
        closeReachable: !!(close && close.getClientRects().length > 0 && close.getBoundingClientRect().width >= 1),
        askId: document.querySelector('section.file-conversation')?.getAttribute('data-conversation-id') || '',
      };
    };
  });
  const legs = [
    { width: 1360, height: 900, label: 'desktop' },
    { width: 390, height: 844, label: 'mobile' },
  ];
  for (const leg of legs) {
    await page.setViewportSize({ width: leg.width, height: leg.height });
    await waitFor(
      page,
      (size) => window.innerWidth === size.width && window.innerHeight === size.height,
      `${leg.label} fullscreen viewport did not become ${leg.width}x${leg.height}`,
      10000,
      leg,
    );
    if ((await page.locator('section.file-explorer').count()) === 0) await openFileExplorer(page);
    await waitFor(
      page,
      () => document.getElementById('file-explorer-expand-btn') != null,
      `${leg.label}: App Files expand control is missing`,
    );
    const askId = await page.evaluate(() => document.querySelector('section.file-conversation')?.getAttribute('data-conversation-id') || '');
    await assertFilesFullscreenLeg(page, leg.label, { askId });
    await openFileExplorer(page);
  }
  const chatHasExpand = await page.evaluate(() => !!document.querySelector('[id$="expand-btn"][aria-label="Expand file explorer to the full app area"]') && !!document.querySelector('#chat-file-explorer-toggle-btn'));
  if (chatHasExpand) fail('Chat embed must not expose Files fullscreen');
  await page.setViewportSize({ width: 1360, height: 900 });
  await waitFor(page, () => window.innerWidth === 1360 && window.innerHeight === 900, 'desktop viewport did not restore after files.fullscreen');
  await hideFileExplorer(page, 'fullscreen restore before mobile');
}



/** Force the page's live socket closed and let the product's OWN reconnect
 *  loop recover. The outage surface asserted here is the product's current one
 *  — the reconnect overlay (#reconnect-overlay) plus the disabled composer;
 *  App.toastTransportOutage only flags the outage, it no longer raises the
 *  transport toast this lane's older phases assumed. The page socket must see
 *  exactly one forced close. */
async function forceWarmReconnect(page, label) {
  const closesBefore = await page.evaluate(() => window.__rpiWarmReconnect.state.closeCodes.length);
  await page.evaluate(() => window.__rpiWarmReconnect.disconnect());
  await waitFor(
    page,
    () => document.getElementById('reconnect-overlay') !== null
      && document.getElementById('send-btn')?.hasAttribute('disabled') === true,
    `${label}: the forced socket close did not surface the reconnect overlay with a disabled composer`,
    30000,
  );
  await page.evaluate(() => window.__rpiWarmReconnect.reconnect());
  await waitFor(
    page,
    () => document.getElementById('reconnect-overlay') === null
      && document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
    `${label}: the product never recovered after the forced close`,
    30000,
  );
  const closesAfter = await page.evaluate(() => window.__rpiWarmReconnect.state.closeCodes.length);
  if (closesAfter !== closesBefore + 1) {
    fail(`${label}: the forced close did not reach the page socket exactly once (${closesBefore} -> ${closesAfter})`);
  }
}

/** Owner-targeted review restore diagnostics: the DOM/authority state plus the
 *  frames sent since a mark, so a restore-phase failure names the cause
 *  (panel absent, switch still pending, rejected boot probe) instead of only
 *  the symptom. */
async function reviewRestoreDiagnostics(page, sentFrames, mark) {
  const dom = await page.evaluate(() => ({
    panel: document.getElementById('code-review-panel') !== null,
    displayed: document.getElementById('session-lifecycle')?.dataset.displayedSessionId || '',
    commandTarget: document.getElementById('session-lifecycle')?.dataset.commandSessionId || '',
    pendingGeneration: document.getElementById('session-lifecycle')?.dataset.pendingGeneration || '',
    authoritativeReady: document.getElementById('session-lifecycle')?.dataset.authoritativeReady || '',
    toasts: [...document.querySelectorAll('#toasts .toast')].map((toast) => toast.textContent || ''),
  }));
  const frames = sentFrames.slice(mark).map(parseSentFrame)
    .filter((frame) => frame !== null)
    .map((frame) => `${frame.type}${frame.sessionId ? `@${frame.sessionId}` : ''}${frame.from ? `(from=${frame.from})` : ''}${frame.to ? `(to=${frame.to})` : ''}`);
  return { ...dom, frameCount: frames.length, frames: frames.slice(0, 40) };
}


async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  // Failure-diagnostic page handles: captured by the catch block below so a
  // crashed run leaves its screenshot/viewport/stack OUTSIDE the rollback
  // staging (which fail() removes) — next to playwright.out instead.
  let diagMainPage = null;
  let diagMobilePage = null;
  const ownershipTrace = { startSentIndex: null, events: [], dropped: 0 };
  const ownershipTraceRequests = new Map();
  const captureOwnershipFrame = (direction, payload) => {
    if (ownershipTrace.startSentIndex === null) return;
    const frame = parseSentFrame(payload);
    const commands = ['code_review_open', 'code_review_close', 'code_review_refresh', 'code_review_snapshot', 'code_review_commits', 'switch_session'];
    const command = direction === 'sent' ? frame?.type : ownershipTraceRequests.get(frame?.id);
    if (!commands.includes(command) || (direction === 'received' && frame?.type !== 'response')) return;
    if (ownershipTrace.events.length >= 256) {
      ownershipTrace.dropped += 1;
      return;
    }
    if (direction === 'sent') ownershipTraceRequests.set(frame.id, command);
    const fields = (record, keys) => Object.fromEntries(keys
      .filter((key) => ['string', 'number', 'boolean'].includes(typeof record?.[key]))
      .map((key) => [key, typeof record[key] === 'string'
        ? (token ? record[key].split(token).join('[REDACTED]') : record[key]).slice(0, 2048)
        : record[key]]));
    ownershipTrace.events.push({
      sequence: ownershipTrace.events.length, at: Date.now(), direction, command,
      ...fields(frame, ['id', 'sessionId', 'commit', 'from', 'to', 'includeWorkingTree', 'reviewId', 'snapshotId', 'success', 'error', 'errorCode', 'code']),
      ...(direction === 'received' ? { data: {
        ...fields(frame.data, ['reviewId', 'snapshotId', 'comparisonLabel', 'closed', 'staleReview', 'code', 'errorCode']),
        ...(Array.isArray(frame.data?.files) ? { fileCount: frame.data.files.length } : {}),
      } } : {}),
    });
  };
  try {
    const page = await browser.newPage({
      viewport: { width: 1360, height: 900 },
      acceptDownloads: true,
      permissions: ['clipboard-read', 'clipboard-write'],
    });
    diagMainPage = page;
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    // Warm-reconnect seam for the composer slash phases at the end of the run:
    // a transparent WebSocket observer that can close the live socket and then
    // fail the product's reconnect attempts synchronously, so Chromium never
    // opens a TCP connection against a listener that is still serving the same
    // port (no manufactured ERR_CONNECTION_REFUSED). The product keeps its own
    // connect / scheduleReconnect / ReadyGate loop — this mirrors the proven
    // shim in E2E.d/web/reconnect_test.mjs.
    //
    // The same observer can also HOLD named command responses (a code_review_open
    // probe, a switch_session negotiation) and release them on demand, so a
    // deferred probe can be resolved inside an exactly-defined pending-switch
    // window without any timing race.
    await page.addInitScript(() => {
      const NativeWebSocket = window.WebSocket;
      const state = { attempts: 0, closeCodes: [], held: [] };
      let blocked = false;
      let current = null;
      const arm = { reviewOpen: false, switchSession: false, commitList: false };
      const heldReviewOpen = new Map();
      const heldSwitchSession = new Map();
      const heldCuts = new Map();
      const heldFileExplorerMap = new Map();
      const heldCommitList = new Map();
      let cutHoldSessionId = null;
      // Ids currently being re-dispatched by releaseHeld: the interceptor must
      // not swallow its own synthetic event (the app's onmessage is the listener
      // that must receive it).
      const releasing = new Set();
      const heldBucket = (id) => {
        if (heldReviewOpen.has(id)) return heldReviewOpen;
        if (heldSwitchSession.has(id)) return heldSwitchSession;
        if (heldFileExplorerMap.has(id)) return heldFileExplorerMap;
        if (heldCommitList.has(id)) return heldCommitList;
        return null;
      };
      class ObservedWebSocket extends NativeWebSocket {
        constructor(...args) {
          state.attempts += 1;
          if (blocked) throw new Error('commands-review warm reconnect outage');
          super(...args);
          current = this;
          this.addEventListener('close', (event) => state.closeCodes.push(event.code));
          // Registered BEFORE the app assigns onmessage, so a held response (or
          // a held subscription cut) is swallowed here and re-dispatched verbatim
          // on release. A held cut is how a session is kept OUT of the reconnect
          // generation's ready cache, which is what forces its next switch onto
          // the negotiated path deterministically.
          this.addEventListener('message', (event) => {
            let frame;
            try {
              frame = JSON.parse(event.data);
            } catch {
              return;
            }
            if (!frame || typeof frame.type !== 'string') return;
            if (frame.type === 'response' && typeof frame.id === 'string') {
              if (releasing.has(frame.id)) return; // our own release: let the app see it
              const bucket = heldBucket(frame.id);
              if (!bucket) return;
              event.stopImmediatePropagation();
              bucket.set(frame.id, frame);
              state.held.push(`${frame.command || 'response'}:${frame.id}`);
              return;
            }
            if (cutHoldSessionId !== null
              && (frame.type === 'subscription_snapshot' || frame.type === 'session_resync')
              && frame.sessionId === cutHoldSessionId) {
              event.stopImmediatePropagation();
              heldCuts.set(frame.sessionId, frame);
              state.held.push(`cut:${frame.sessionId}`);
              cutHoldSessionId = null;
            }
          });
        }
        send(data) {
          if (typeof data === 'string') {
            try {
              const frame = JSON.parse(data);
              if (frame && typeof frame.id === 'string') {
                if (arm.reviewOpen && frame.type === 'code_review_open') {
                  arm.reviewOpen = false;
                  heldReviewOpen.set(frame.id, null);
                }
                if (arm.switchSession && frame.type === 'switch_session') {
                  arm.switchSession = false;
                  heldSwitchSession.set(frame.id, null);
                }
                if (arm.fileExplorerMap && frame.type === 'file_explorer_map') {
                  arm.fileExplorerMap = false;
                  heldFileExplorerMap.set(frame.id, null);
                }
                if (arm.commitList && frame.type === 'code_review_commits') {
                  arm.commitList = false;
                  heldCommitList.set(frame.id, null);
                }
              }
            } catch {
              // not a product control-plane frame
            }
          }
          super.send(data);
        }
      }
      for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
        Object.defineProperty(ObservedWebSocket, key, { value: NativeWebSocket[key] });
      }
      window.WebSocket = ObservedWebSocket;
      window.__rpiWarmReconnect = {
        state,
        disconnect() {
          blocked = true;
          const socket = current;
          state.disconnects = (state.disconnects || 0) + 1;
          state.disconnectReadyState = socket ? socket.readyState : null;
          try {
            socket?.close(4000, 'warm reconnect outage');
          } catch {
            // already closing
          }
        },
        reconnect() {
          blocked = false;
        },
        holdNextReviewOpen() {
          arm.reviewOpen = true;
        },
        holdNextSwitchSession() {
          arm.switchSession = true;
        },
        holdNextCutFor(sessionId) {
          cutHoldSessionId = sessionId;
        },
        holdNextFileExplorerMap() {
          arm.fileExplorerMap = true;
        },
        holdNextCommitList() {
          arm.commitList = true;
        },
        releaseHeldFileExplorerMap() {
          const socket = current;
          if (!socket) return 0;
          let released = 0;
          for (const frame of heldFileExplorerMap.values()) {
            if (!frame) continue;
            releasing.add(frame.id);
            socket.dispatchEvent(new MessageEvent('message', { data: JSON.stringify(frame) }));
            releasing.delete(frame.id);
            released += 1;
          }
          heldFileExplorerMap.clear();
          return released;
        },
        heldMapCount() {
          return Array.from(heldFileExplorerMap.values()).filter((frame) => frame !== null).length;
        },
        heldCount() {
          return [
            ...heldReviewOpen.values(),
            ...heldSwitchSession.values(),
            ...heldCuts.values(),
            ...heldCommitList.values(),
          ].filter((frame) => frame !== null).length;
        },
        releaseHeld(order = ['reviewOpen', 'switchSession', 'cuts']) {
          const socket = current;
          if (!socket) return 0;
          const buckets = { reviewOpen: heldReviewOpen, switchSession: heldSwitchSession, cuts: heldCuts, commitList: heldCommitList };
          let released = 0;
          // Deterministic order per call: the caller decides which held frame
          // resolves when (the deferred-probe phase needs the negotiation's
          // handoff to paint the new display BEFORE the probe resolves).
          for (const name of order) {
            const bucket = buckets[name];
            if (!bucket) continue;
            for (const frame of bucket.values()) {
              if (!frame) continue;
              releasing.add(frame.id);
              socket.dispatchEvent(new MessageEvent('message', { data: JSON.stringify(frame) }));
              releasing.delete(frame.id);
              released += 1;
            }
            bucket.clear();
          }
          return released;
        },
      };
    });
    const pageErrors = [];
    const consoleErrors = [];
    page.on('pageerror', (err) => {
      const message = String(err && err.message ? err.message : err);
      if (isKnownConsoleNoise(message)) return;
      pageErrors.push(message);
      console.error(`web-commands-review: page error: ${message}`);
    });
    page.on('console', (message) => {
      if (message.type() === 'error' && !isKnownConsoleNoise(message.text())) consoleErrors.push(message.text());
    });

    // Capture outgoing WS frames so the /compact dispatch can be observed
    // without requiring the provider round-trip to succeed (deterministic).
    // code_review_snapshot frames are timestamped separately: the panel polls
    // at a fixed 1.5s cadence while mounted, so ≥2 frames ~1.5s apart must be
    // observable, and polling must stop once the panel closes.
    const sentFrames = [];
    const snapshotFrames = [];
    const receivedFrames = [];
    page.on('websocket', (ws) => {
      ws.on('framesent', (frame) => {
        const payload = typeof frame.payload === 'string' ? frame.payload : '';
        if (!payload) return;
        sentFrames.push(payload);
        captureOwnershipFrame('sent', payload);
        try {
          const parsed = JSON.parse(payload);
          if (parsed && parsed.type === 'code_review_snapshot') {
            snapshotFrames.push({ t: Date.now(), payload });
          }
        } catch {
          // not a JSON frame — ignore
        }
      });
      // The code-review page chain asserts RESPONSE identity (cursor/
      // totalLines/hasMore per request id), so incoming frames are captured
      // alongside the sent tap.
      ws.on('framereceived', (frame) => {
        const payload = typeof frame.payload === 'string' ? frame.payload : '';
        if (!payload) return;
        receivedFrames.push(payload);
        captureOwnershipFrame('received', payload);
      });
    });
    // Hostile diff/comment text must never open a browser dialog.
    let dialogs = 0;
    page.on('dialog', (dialog) => {
      dialogs += 1;
      dialog.dismiss().catch(() => {});
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await page.context().grantPermissions(['clipboard-read', 'clipboard-write'], { origin: new URL(url).origin });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'WS did not reach "connected"'
    );

    // #command-btn sits LEFT of #prompt-input in #composer-row.
    const geometry = await page.evaluate(() => {
      const btn = document.getElementById('command-btn');
      const input = document.getElementById('prompt-input');
      if (!btn || !input) return null;
      const b = btn.getBoundingClientRect();
      const i = input.getBoundingClientRect();
      return { btnRight: b.right, inputLeft: i.left, btnTop: b.top, inputTop: i.top };
    });
    if (!geometry) fail('command-btn or prompt-input missing from the composer row');
    if (geometry.btnRight > geometry.inputLeft) {
      fail(`command-btn is not left of prompt-input (btnRight=${geometry.btnRight} > inputLeft=${geometry.inputLeft})`);
    }
    await laneShot(page, 'command-button-left.png', { fullPage: true });
    await runFileExplorerPreviewJourney(page);

    // The picker lists /compact, /skill, /code-review (backend authority).
    await openPicker(page);
    await waitFor(
      page,
      () => {
        const names = Array.from(document.querySelectorAll('.command-picker__option .command-picker__name')).map((el) => el.textContent.trim());
        return names.includes('/compact') && names.includes('/skill') && names.includes('/code-review');
      },
      'command picker did not list /compact, /skill, /code-review'
    );
    const names = await pickerNames(page);
    for (const required of ['/compact', '/skill', '/code-review']) {
      if (!names.includes(required)) fail(`command picker missing ${required} (got ${names.join(', ')})`);
    }
    await laneShot(page, 'command-picker-open.png', { fullPage: true });

    // Choose /code-review -> draft lands in the composer, NO auto-submit.
    await chooseCommand(page, 'code-review');
    await waitFor(
      page,
      () => {
        const v = document.getElementById('prompt-input')?.value.trim() || '';
        return v.startsWith('/code-review');
      },
      'choosing /code-review did not insert the draft into the composer'
    );
    // No auto-submit: the panel must NOT have opened yet, and no user bubble
    // for the slash command appears in the transcript.
    const panelBefore = await page.evaluate(() => document.getElementById('code-review-panel') !== null);
    if (panelBefore) fail('/code-review auto-submitted — panel opened without Enter');
    await laneShot(page, 'code-review-draft-no-submit.png', { fullPage: true });

    // Enter dispatches code_review_open -> the real review panel renders.
    // Pre-open baseline for the close-restore contract asserted after the
    // geometry checks below (scroll, composer rect, focus = prompt-input).
    const crLayoutBeforeDesktop = await crAppLayout(page);
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, () => document.getElementById('code-review-panel') !== null, 'code-review panel did not open on Enter');
    // Comparison label: backend comparisonLabel "HEAD → working tree".
    await waitFor(
      page,
      () => {
        const label = document.querySelector('.code-review__label')?.textContent || '';
        return label.includes('HEAD') && label.includes('working tree');
      },
      'code-review panel did not render the HEAD→working-tree comparison label'
    );
    const label = await page.evaluate(() => document.querySelector('.code-review__label')?.textContent || '');
    if (!label.includes('HEAD') || !label.includes('working tree')) {
      fail(`code-review comparison label wrong: ${label}`);
    }
    await laneShot(page, 'code-review-panel-open.png', { fullPage: true });

    // The dirty file appears in the file list with a modified status. Click
    // it so its diff (addition + deletion lines) renders.
    await waitFor(
      page,
      (want) => Array.from(document.querySelectorAll('.code-review__file')).some((b) =>
        b.getAttribute('data-file-path') === want
      ),
      `code-review file list never rendered the dirty file ${dirtyFile}`,
      20000,
      dirtyFile
    );
    const dirtyClicked = await page.evaluate((want) => {
      const btn = Array.from(document.querySelectorAll('.code-review__file')).find((b) =>
        b.getAttribute('data-file-path') === want
      );
      if (!btn) return false;
      btn.click();
      return true;
    }, dirtyFile);
    if (!dirtyClicked) fail(`could not click the dirty file row ${dirtyFile}`);
    await waitFor(
      page,
      () => document.querySelector('.code-review__line--deletion') !== null,
      'code-review panel never rendered a deletion diff line for the dirty file'
    );
    await waitFor(
      page,
      () => document.querySelector('.code-review__line--addition') !== null,
      'code-review panel never rendered an addition diff line for the dirty file'
    );
    // Concrete values from the fixture: the deleted line is "beta", an added
    // line is "delta" (and "BETA"). Assert the rendered diff text carries them.
    const diffLines = await page.evaluate(() =>
      Array.from(document.querySelectorAll('.code-review__line')).map((line) => ({
        kind: Array.from(line.classList).find((c) => c.startsWith('code-review__line--')) || '',
        text: line.querySelector('.code-review__line-text')?.textContent || '',
      }))
    );
    const deletionTexts = diffLines.filter((d) => d.kind.endsWith('deletion')).map((d) => d.text);
    const additionTexts = diffLines.filter((d) => d.kind.endsWith('addition')).map((d) => d.text);
    if (!deletionTexts.some((t) => t.includes('beta'))) {
      fail(`code-review deletion line did not render the removed "beta" line (deletions=${JSON.stringify(deletionTexts)})`);
    }

    // ---- Outer shell full-viewport geometry (desktop 1360x900) ----
    // The shared drawer base styles .code-review-panel like an ordinary
    // bottom drawer (width:400px, max-width:94vw, 90vh height); the shell
    // override must fully reset sizing so inset:0 stretches it across the
    // viewport. A 400px rail, 94vw gutter, bottom-drawer top offset,
    // top-header inset, or 90vh height each fail by name inside
    // assertCrShellGeometry.
    const crGeomDesktop = await crShellGeometry(page);
    assertCrShellGeometry(crGeomDesktop, 'desktop');
    await laneShot(page, 'code-review-panel-geometry-desktop.png');

    // Close unmounts the panel AND restores the prior app layout
    // (scroll, composer rect, focus), then the workspace reopens so the
    // remaining desktop assertions continue on a live panel.
    await page.click('button.code-review__close');
    await waitFor(page, () => document.getElementById('code-review-panel') === null, 'desktop close: panel did not unmount');
    const crLayoutAfterDesktop = await crAppLayout(page);
    assertCrLayoutRestored(crLayoutBeforeDesktop, crLayoutAfterDesktop, 'desktop close-restore');
    await laneShot(page, 'code-review-panel-close-restore-desktop.png');
    await openPicker(page);
    await chooseCommand(page, 'code-review');
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, () => document.getElementById('code-review-panel') !== null, 'desktop reopen after the geometry close-restore cycle failed');
    await waitFor(
      page,
      (want) => Array.from(document.querySelectorAll('.code-review__file')).some((b) =>
        b.getAttribute('data-file-path') === want
      ),
      'desktop reopen: file list never rendered',
      20000,
      dirtyFile
    );
    if (!additionTexts.some((t) => t.includes('delta'))) {
      fail(`code-review addition line did not render the added "delta" line (additions=${JSON.stringify(additionTexts)})`);
    }
    // The staged new file is also listed.
    const hasAdded = await page.evaluate((want) =>
      Array.from(document.querySelectorAll('.code-review__file')).some((b) =>
        b.getAttribute('data-file-path') === want
      )
    , addedFile);
    if (!hasAdded) fail(`code-review file list missing the staged new file ${addedFile}`);

    // ---- Thread column width: stable empty column + desktop resizer ----
    // Contract: `.code-review__thread-resizer` (ARIA separator) between Diff
    // and Thread, CSS var `--code-review-thread-width` on `.code-review`,
    // localStorage `rpi-code-review-thread-width`, bounds 240–480 (default 280).
    // Review React owns the DOM mount; this lane asserts the full desktop
    // pointer/keyboard/bounds/reload journey once it is present.
    await waitFor(
      page,
      () => document.querySelector('.code-review__thread-resizer') !== null,
      'code-review thread resizer (.code-review__thread-resizer) never mounted on desktop'
    );
    const threadInitial = await page.evaluate(() => {
      const root = document.querySelector('.code-review');
      const resizer = document.querySelector('.code-review__thread-resizer');
      const comments = document.querySelector('.code-review__comments');
      if (!root || !resizer || !comments) return null;
      const raw = getComputedStyle(root).getPropertyValue('--code-review-thread-width').trim();
      const px = Number.parseFloat(raw);
      return {
        raw,
        px,
        role: resizer.getAttribute('role'),
        tabIndex: resizer.tabIndex,
        commentsW: comments.getBoundingClientRect().width,
        stored: window.localStorage.getItem('rpi-code-review-thread-width'),
      };
    });
    if (!threadInitial) fail('code-review thread width probes missing (.code-review / resizer / comments)');
    if (threadInitial.role !== 'separator') {
      fail(`thread resizer role must be separator (got "${threadInitial.role}")`);
    }
    if (threadInitial.tabIndex < 0) fail('thread resizer must be keyboard-focusable');
    if (!(threadInitial.px >= 240 && threadInitial.px <= 480)) {
      fail(`--code-review-thread-width must be within 240–480 (got "${threadInitial.raw}")`);
    }
    // Empty thread column stays stable (width tracks the CSS var, not content).
    if (Math.abs(threadInitial.commentsW - threadInitial.px) > 12) {
      fail(`empty thread column width ${threadInitial.commentsW}px must track --code-review-thread-width ${threadInitial.raw}`);
    }

    // Keyboard: Home → max 480, End → min 240 (or ArrowRight/Left steps).
    await page.focus('.code-review__thread-resizer');
    await page.keyboard.press('Home');
    const afterThreadHome = await page.evaluate(() => {
      const root = document.querySelector('.code-review');
      const raw = root ? getComputedStyle(root).getPropertyValue('--code-review-thread-width').trim() : '';
      const stored = window.localStorage.getItem('rpi-code-review-thread-width');
      return { raw, px: Number.parseFloat(raw), stored };
    });
    if (afterThreadHome.px !== 480 && afterThreadHome.stored !== '480') {
      // Some implementations only step via arrows; fall back to many Right presses.
      for (let i = 0; i < 20; i++) await page.keyboard.press('ArrowRight');
    }
    const afterMax = await page.evaluate(() => {
      const root = document.querySelector('.code-review');
      const raw = root ? getComputedStyle(root).getPropertyValue('--code-review-thread-width').trim() : '';
      const stored = window.localStorage.getItem('rpi-code-review-thread-width');
      const commentsW = document.querySelector('.code-review__comments')?.getBoundingClientRect().width || 0;
      return { raw, px: Number.parseFloat(raw), stored, commentsW };
    });
    if (afterMax.px !== 480) {
      fail(`thread resizer must clamp to max 480px (got "${afterMax.raw}", stored="${afterMax.stored}")`);
    }
    if (afterMax.stored !== '480') {
      fail(`thread max must persist rpi-code-review-thread-width=480 (got "${afterMax.stored}")`);
    }
    await page.keyboard.press('End');
    let afterMin = await page.evaluate(() => {
      const root = document.querySelector('.code-review');
      const raw = root ? getComputedStyle(root).getPropertyValue('--code-review-thread-width').trim() : '';
      const stored = window.localStorage.getItem('rpi-code-review-thread-width');
      return { raw, px: Number.parseFloat(raw), stored };
    });
    if (afterMin.px !== 240 && afterMin.stored !== '240') {
      for (let i = 0; i < 20; i++) await page.keyboard.press('ArrowLeft');
      afterMin = await page.evaluate(() => {
        const root = document.querySelector('.code-review');
        const raw = root ? getComputedStyle(root).getPropertyValue('--code-review-thread-width').trim() : '';
        const stored = window.localStorage.getItem('rpi-code-review-thread-width');
        return { raw, px: Number.parseFloat(raw), stored };
      });
    }
    if (afterMin.px !== 240) {
      fail(`thread resizer must clamp to min 240px (got "${afterMin.raw}", stored="${afterMin.stored}")`);
    }
    if (afterMin.stored !== '240') {
      fail(`thread min must persist rpi-code-review-thread-width=240 (got "${afterMin.stored}")`);
    }

    // Pointer drag: pull resizer left → thread column grows.
    const threadBox = await page.locator('.code-review__thread-resizer').boundingBox();
    if (!threadBox) fail('thread resizer has no bounding box');
    const tx = threadBox.x + threadBox.width / 2;
    const ty = threadBox.y + threadBox.height / 2;
    await page.mouse.move(tx, ty);
    await page.mouse.down();
    await page.mouse.move(tx - 80, ty, { steps: 8 });
    await page.mouse.up();
    const afterThreadDrag = await page.evaluate(() => {
      const root = document.querySelector('.code-review');
      const raw = root ? getComputedStyle(root).getPropertyValue('--code-review-thread-width').trim() : '';
      const px = Number.parseFloat(raw);
      const stored = window.localStorage.getItem('rpi-code-review-thread-width');
      const commentsW = document.querySelector('.code-review__comments')?.getBoundingClientRect().width || 0;
      return { raw, px, stored, commentsW };
    });
    if (!(afterThreadDrag.px > 240 && afterThreadDrag.px <= 480)) {
      fail(`thread pointer drag must grow width above 240 within bounds (got "${afterThreadDrag.raw}")`);
    }
    {
      const storedN = Number(afterThreadDrag.stored);
      if (!(Number.isFinite(storedN) && Math.abs(storedN - afterThreadDrag.px) < 1.5)) {
        fail(`thread drag must persist rpi-code-review-thread-width≈${afterThreadDrag.px} (got "${afterThreadDrag.stored}")`);
      }
    }
    if (Math.abs(afterThreadDrag.commentsW - afterThreadDrag.px) > 12) {
      fail(`thread column width ${afterThreadDrag.commentsW}px must track var ${afterThreadDrag.raw}`);
    }
    const persistedThreadW = afterThreadDrag.px;
    await laneShot(page, 'code-review-thread-resize.png', { fullPage: true });

    // Reload persistence for the thread width.
    await page.reload({ waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'thread reload: WS did not reach connected'
    );
    // Re-open the review panel via the command surface.
    await openPicker(page);
    await chooseCommand(page, 'code-review');
    await waitFor(
      page,
      () => (document.getElementById('prompt-input')?.value.trim() || '').startsWith('/code-review'),
      'thread reload: /code-review draft missing'
    );
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, () => document.getElementById('code-review-panel') !== null, 'thread reload: panel did not reopen');
    await waitFor(
      page,
      () => document.querySelector('.code-review__thread-resizer') !== null,
      'thread reload: resizer missing after reopen'
    );
    const afterThreadReload = await page.evaluate((want) => {
      const root = document.querySelector('.code-review');
      const raw = root ? getComputedStyle(root).getPropertyValue('--code-review-thread-width').trim() : '';
      const px = Number.parseFloat(raw);
      const stored = window.localStorage.getItem('rpi-code-review-thread-width');
      return { raw, px, stored, want };
    }, persistedThreadW);
    if (!(Number.isFinite(afterThreadReload.px) && Math.abs(afterThreadReload.px - persistedThreadW) < 1.5)) {
      fail(`thread reload must restore --code-review-thread-width≈${persistedThreadW} (got "${afterThreadReload.raw}", stored="${afterThreadReload.stored}")`);
    }

    // Re-select the dirty file so later hunk/thread journeys still have a stage.
    await waitFor(
      page,
      (want) => Array.from(document.querySelectorAll('.code-review__file')).some((b) =>
        b.getAttribute('data-file-path') === want
      ),
      `thread reload: dirty file ${dirtyFile} missing after reopen`,
      20000,
      dirtyFile
    );
    await page.evaluate((want) => {
      const btn = Array.from(document.querySelectorAll('.code-review__file')).find((b) =>
        b.getAttribute('data-file-path') === want
      );
      if (btn) btn.click();
    }, dirtyFile);
    await waitFor(
      page,
      () => document.querySelector('.code-review__line--deletion') !== null,
      'thread reload: dirty file diff did not re-render'
    );

    // ---- TUI/Web parity: compact status glyphs + basename tree rows ----
    // The changed-file rail mirrors the TUI file tree: one colored compact
    // letter per status (A/D/M/R/C/B/?), every level shows only its
    // basename (the hierarchy carries the directories), and the FULL
    // repo-relative path + readable status stay available via
    // data-file-path / title / aria-label. Selection and filtering still
    // key on the full path.
    const rowMeta = await page.evaluate(() => {
      const rows = {};
      for (const btn of Array.from(document.querySelectorAll('button.code-review__file'))) {
        const path = btn.getAttribute('data-file-path') || '';
        rows[path] = {
          glyph: btn.querySelector('.code-review__file-status')?.textContent?.trim() || '',
          name: btn.querySelector('.code-review__file-path')?.textContent?.trim() || '',
          title: btn.getAttribute('title') || '',
          aria: btn.getAttribute('aria-label') || '',
        };
      }
      return rows;
    });
    const dirtyMeta = rowMeta[dirtyFile];
    if (!dirtyMeta) fail(`no file row for the dirty file ${dirtyFile}`);
    if (dirtyMeta.glyph !== 'M') {
      fail(`modified file must show the compact M glyph (got "${dirtyMeta.glyph}")`);
    }
    const addedMeta = rowMeta[addedFile];
    if (!addedMeta) fail(`no file row for the added file ${addedFile}`);
    if (addedMeta.glyph !== 'A') {
      fail(`added file must show the compact A glyph (got "${addedMeta.glyph}")`);
    }
    if (dirtyMeta.name !== dirtyFile) {
      fail(`modified row must display the full filename ${dirtyFile} (got "${dirtyMeta.name}")`);
    }
    if (addedMeta.name !== addedFile) {
      fail(`added row must display the full filename ${addedFile} (got "${addedMeta.name}")`);
    }
    if (!dirtyMeta.aria.includes('modified') || !dirtyMeta.aria.includes(dirtyFile)) {
      fail(`modified row aria-label must carry the readable full state (got "${dirtyMeta.aria}")`);
    }
    if (!addedMeta.aria.includes('added') || !addedMeta.aria.includes(addedFile)) {
      fail(`added row aria-label must carry the readable full state (got "${addedMeta.aria}")`);
    }
    if (dirtyMeta.title !== dirtyFile || addedMeta.title !== addedFile) {
      fail('file rows must carry the full repo-relative path in title');
    }
    const nestedMeta = rowMeta[nestedFile];
    if (!nestedMeta) fail(`no file row for the nested file ${nestedFile}`);
    const nestedBase = nestedFile.split('/').pop();
    if (nestedMeta.name !== nestedBase) {
      fail(`nested row must show only its basename "${nestedBase}" (got "${nestedMeta.name}")`);
    }
    if (nestedMeta.name.includes('/')) {
      fail(`nested basename must not repeat the full path (got "${nestedMeta.name}")`);
    }
    if (nestedMeta.title !== nestedFile || !nestedMeta.aria.includes(nestedFile)) {
      fail(`nested row must carry the full repo-relative path in title/aria (title="${nestedMeta.title}", aria="${nestedMeta.aria}")`);
    }
    // Stats must not swallow the filename: every visible file row's name
    // column fits without ellipsis, and the rail itself never overflows.
    const rowFit = await page.evaluate(() =>
      Array.from(document.querySelectorAll('button.code-review__file')).every((btn) => {
        const span = btn.querySelector('.code-review__file-path');
        if (!span) return true;
        return span.scrollWidth <= span.clientWidth + 1;
      })
    );
    if (!rowFit) fail('a file row filename is visually truncated (stats swallowed the name)');
    const railOverflow = await page.evaluate(() => {
      const rail = document.querySelector('.code-review__files');
      return rail && rail.scrollWidth > rail.clientWidth + 1;
    });
    if (railOverflow) fail('the file rail overflows horizontally on desktop');

    // ---- Fixture produces TWO separated hunks ----
    // The dirty file is seeded with changes >6 untouched lines apart, so the
    // backend snapshot must carry exactly two distinct hunks (@@ -1,6 +1,7
    // and @@ -13,6 +14,7). One merged hunk fails the gate: both regions must
    // be exposed as separate selectable comment targets.
    const hunkHeaders = await page.evaluate(() =>
      Array.from(document.querySelectorAll('.code-review__hunk-header')).map((b) => b.textContent.trim())
    );
    if (hunkHeaders.length !== 2) {
      fail(`dirty file must render exactly two separated hunks (got ${hunkHeaders.length}: ${hunkHeaders.join(' | ')})`);
    }
    if (!hunkHeaders[0].includes('@@ -1,6 +1,7')) {
      fail(`first hunk header wrong (got "${hunkHeaders[0]}")`);
    }
    if (!hunkHeaders[1].includes('@@ -13,6 +14,7')) {
      fail(`second hunk header wrong (got "${hunkHeaders[1]}")`);
    }
    // The hostile fixture line (a second-hunk addition) must render LITERALLY
    // as text: no <img>/[onerror] element is created and no script runs.
    const hostileLine = '<script>window.__crPwned=1</script><img src=x onerror=window.__crPwned=2>';
    const hostileRendered = await page.evaluate((want) =>
      Array.from(document.querySelectorAll('.code-review__line--addition .code-review__line-text'))
        .some((el) => (el.textContent || '').includes(want))
    , hostileLine);
    if (!hostileRendered) {
      fail('hostile diff line did not render as literal text in the panel');
    }
    const hostileDiffEffects = await page.evaluate(() => ({
      img: document.querySelector('.code-review img[src="x"]') !== null,
      onerror: document.querySelector('.code-review [onerror]') !== null,
      pwned: typeof window.__crPwned !== 'undefined',
    }));
    if (hostileDiffEffects.img || hostileDiffEffects.onerror || hostileDiffEffects.pwned) {
      fail(`hostile diff line created DOM/script side effects: ${JSON.stringify(hostileDiffEffects)}`);
    }
    await laneShot(page, 'code-review-diff-lines.png', { fullPage: true });

    // ---- Explicit hunk selection (never auto-selected) ----
    // After opening and switching files there is NO comment target until the
    // user picks a hunk: the composer must not be rendered, and the comments
    // pane must prompt for an explicit selection.
    const autoComposer = await page.evaluate(
      () => document.querySelector('.code-review__comment-input') !== null
    );
    if (autoComposer) {
      fail('hunk auto-selected on file switch — composer rendered without explicit hunk selection');
    }
    const selectPrompt = await page.evaluate(() =>
      Array.from(document.querySelectorAll('.code-review__comments .code-review__empty')).some(
        (el) => (el.textContent || '').includes('Select a hunk')
      )
    );
    if (!selectPrompt) fail('comments pane did not prompt for explicit hunk selection');

    // ---- Selecting a different file clears the composer/hunk selection ----
    // A selected hunk + its composer belong to one file's diff; switching
    // files must clear both, and returning must NOT auto-reselect anything.
    await page.click('.code-review__hunk-header');
    await waitFor(
      page,
      () => document.querySelector('.code-review__comment-input') !== null,
      'composer did not appear before the file-switch gate'
    );
    await page.evaluate((want) => {
      const btn = Array.from(document.querySelectorAll('.code-review__file')).find((b) =>
        b.getAttribute('data-file-path') === want
      );
      if (btn) btn.click();
    }, addedFile);
    await waitFor(
      page,
      (want) => (document.querySelector('.code-review__diff-path')?.textContent || '').includes(want),
      `switching to ${addedFile} did not render its diff`,
      10000,
      addedFile
    );
    const clearedOnSwitch = await page.evaluate(() => ({
      composer: document.querySelector('.code-review__comment-input') !== null,
      selected: document.querySelector('.code-review__hunk.is-selected') !== null,
    }));
    if (clearedOnSwitch.composer || clearedOnSwitch.selected) {
      fail(`file switch did not clear hunk selection (composer=${clearedOnSwitch.composer}, selected=${clearedOnSwitch.selected})`);
    }
    // Back to the dirty file: the composer must stay cleared.
    await page.evaluate((want) => {
      const btn = Array.from(document.querySelectorAll('.code-review__file')).find((b) =>
        b.getAttribute('data-file-path') === want
      );
      if (btn) btn.click();
    }, dirtyFile);
    await waitFor(
      page,
      () => document.querySelector('.code-review__line--deletion') !== null,
      'dirty file diff did not re-render after the file-switch gate'
    );
    const stillCleared = await page.evaluate(() => ({
      composer: document.querySelector('.code-review__comment-input') !== null,
      selected: document.querySelector('.code-review__hunk.is-selected') !== null,
    }));
    if (stillCleared.composer || stillCleared.selected) {
      fail('hunk selection re-appeared after switching back to the dirty file');
    }

    // ---- File filter ----
    const beforeFilter = await page.evaluate(() => document.querySelectorAll('.code-review__file').length);
    await page.fill('.code-review__file-filter', 'added');
    await waitFor(
      page,
      () => document.querySelectorAll('.code-review__file').length === 1,
      'file filter did not narrow the list to one row'
    );
    await page.fill('.code-review__file-filter', '');
    await waitFor(
      page,
      (expected) => document.querySelectorAll('.code-review__file').length === expected,
      'clearing the file filter did not restore the full list',
      25000,
      beforeFilter
    );

    // ---- Nested path tree: default expanded, click collapse/expand ----
    // The changed-file rail is a collapsible tree: nested/ renders as a
    // directory row (dirs sort before files), its file row is visible by
    // default, clicking the dir toggles the subtree, and a filter keeps the
    // matched file PLUS its ancestor dir (forced expanded).
    const nestedDir = await page.evaluate(() => {
      const button = document.querySelector('.code-review__tree-dir[data-dir-path="nested"]');
      const li = button?.closest('li.code-review__tree-row[data-tree-kind="dir"]');
      if (!li || !button) return null;
      return {
        name: (button.querySelector('.code-review__file-path')?.textContent || '').trim(),
        expanded: li.getAttribute('aria-expanded'),
      };
    });
    if (!nestedDir) fail('nested directory row missing from the file tree');
    if (nestedDir.name !== 'nested') fail(`nested directory row name wrong (got "${nestedDir.name}")`);
    if (nestedDir.expanded !== 'true') fail('nested directory is not expanded by default');
    const deepVisible = await page.evaluate((want) =>
      Array.from(document.querySelectorAll('.code-review__file')).some((b) =>
        b.getAttribute('data-file-path') === want
      )
    , nestedFile);
    if (!deepVisible) fail(`nested file ${nestedFile} not visible under the default-expanded dir`);
    const nestedChildCount = await page.evaluate(() =>
      document.querySelectorAll('.code-review__file-list .code-review__file[data-file-path^="nested/"]').length
    );
    if (nestedChildCount < 1) fail('nested/ has no child file rows to collapse');
    const rowsBeforeCollapse = await page.evaluate(() =>
      document.querySelectorAll('.code-review__file-list [role="treeitem"]').length
    );
    await page.click('.code-review__tree-dir[data-dir-path="nested"]');
    await waitFor(
      page,
      ({ before, children }) => document.querySelectorAll('.code-review__file-list [role="treeitem"]').length === before - children,
      'clicking the dir row did not collapse the subtree',
      10000,
      { before: rowsBeforeCollapse, children: nestedChildCount }
    );
    const deepHidden = await page.evaluate((want) =>
      Array.from(document.querySelectorAll('.code-review__file')).some((b) =>
        b.getAttribute('data-file-path') === want
      )
    , nestedFile);
    if (deepHidden) fail(`nested file ${nestedFile} still visible after collapsing its dir`);
    await page.click('.code-review__tree-dir[data-dir-path="nested"]');
    await waitFor(
      page,
      (n) => document.querySelectorAll('.code-review__file-list [role="treeitem"]').length === n,
      'clicking the collapsed dir row did not re-expand the subtree',
      10000,
      rowsBeforeCollapse
    );
    // Filter keeps the matched file AND its ancestor dir (forced expanded).
    await page.fill('.code-review__file-filter', 'deep');
    await waitFor(
      page,
      (want) => {
        const li = document.querySelector('.code-review__tree-dir[data-dir-path="nested"]')
          ?.closest('li.code-review__tree-row[data-tree-kind="dir"]');
        const file = Array.from(document.querySelectorAll('.code-review__file')).some((b) =>
          b.getAttribute('data-file-path') === want
        );
        return li !== null && file && li?.getAttribute('aria-expanded') === 'true';
      },
      'filter did not keep the ancestor dir + matched file (forced expanded)',
      10000,
      nestedFile
    );
    await page.fill('.code-review__file-filter', '');
    await waitFor(
      page,
      (want) => Array.from(document.querySelectorAll('.code-review__file')).some((b) =>
        b.getAttribute('data-file-path') === want
      ),
      `file list did not restore ${dirtyFile}`,
      10000,
      dirtyFile
    );

    // ---- Oversize fixture file: eager per-file paging, no truncation ----
    // The >4000-line fixture's diff body is COMPLETE (well under every
    // backend cap): the file row carries no truncated token, no truncation
    // banner or bounded status may exist, and the Load more/full controls
    // are gone — the selection eagerly fetches the file's full backend page
    // chain and real near-bottom scroll grows the visible window
    // 4000 → 8000 → 8200.
    await page.fill('.code-review__file-filter', bigFile);
    await waitFor(
      page,
      (want) => Array.from(document.querySelectorAll('.code-review__file')).some((b) =>
        b.getAttribute('data-file-path') === want
      ),
      `oversize fixture file ${bigFile} not listed`,
      10000,
      bigFile
    );
    const bigStats = await page.evaluate((want) => {
      const btn = Array.from(document.querySelectorAll('.code-review__file')).find((b) =>
        b.getAttribute('data-file-path') === want
      );
      return btn ? (btn.querySelector('.code-review__file-stats')?.textContent || '') : '';
    }, bigFile);
    if (/truncated/i.test(bigStats)) {
      fail(`fully-parsed oversize fixture row must not carry a truncated token (stats=${bigStats})`);
    }
    await page.evaluate((want) => {
      const btn = Array.from(document.querySelectorAll('.code-review__file')).find((b) =>
        b.getAttribute('data-file-path') === want
      );
      if (btn) btn.click();
    }, bigFile);
    // First window: exactly the 4000-line soft cap, deepest changed line
    // still beyond it, and NO truncation surface of any kind.
    await waitFor(
      page,
      (n) => document.querySelectorAll('.code-review__line').length === n,
      'oversize file did not settle on the 4000-line soft window',
      10000,
      4000
    );
    const changedEarly = await page.evaluate(() =>
      Array.from(document.querySelectorAll('.code-review__line-text')).some((el) =>
        (el.textContent || '').includes('changed-line-00001')
      )
    );
    if (changedEarly) fail('changed lines rendered before any window growth');
    const oversizeState = await page.evaluate(() => ({
      banner: Array.from(document.querySelectorAll('.code-review__banner, .code-review__banner--truncated')).some(
        (el) => /truncat|Large file/i.test(el.textContent || '')
      ),
      status: document.querySelector('.code-review__page-status')?.textContent || null,
      loadMore: document.querySelector('.code-review__load-more') !== null,
      loadFull: document.querySelector('.code-review__load-full') !== null,
    }));
    if (oversizeState.banner) fail('oversize fixture must not render a truncated/Large-file banner');
    if (oversizeState.status) {
      fail(`fully-parsed oversize paging must render no bounded status (got ${JSON.stringify(oversizeState.status)})`);
    }
    if (oversizeState.loadMore || oversizeState.loadFull) {
      fail('manual .code-review__load-more/.code-review__load-full buttons must be gone (scroll owns paging)');
    }
    // Real near-bottom scroll grows the window one soft-cap chunk per event:
    // 4000 → 8000 (later base-lines AND the first changed lines) → 8200
    // (the deepest changed line).
    const scrollDiffToBottom = () => page.evaluate(() => {
      const el = document.querySelector('.code-review__diff:not(.is-hidden)');
      if (!el) throw new Error('diff viewport not found');
      el.scrollTop = el.scrollHeight;
    });
    await scrollDiffToBottom();
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.code-review__line-text')).some((el) =>
          (el.textContent || '').includes('base-line-04001')
        ) &&
        Array.from(document.querySelectorAll('.code-review__line-text')).some((el) =>
          (el.textContent || '').includes('changed-line-00001')
        ),
      'near-bottom scroll did not grow the window past the 4000-line soft cap',
      10000
    );
    await scrollDiffToBottom();
    await waitFor(
      page,
      () => document.querySelectorAll('.code-review__line').length === 8200
        && Array.from(document.querySelectorAll('.code-review__line-text')).some((el) =>
          (el.textContent || '').includes('changed-line-04100')
        ),
      'scroll growth did not reveal the final changed line of the oversize fixture',
      10000
    );
    // Eager-fetch contract (aligned with code_review_paging_test T4): the
    // selection eagerly consumed the FULL backend page chain for this
    // ordinary complete body — exactly 9 pages (cursors 0→1000→…→8000,
    // maxLines 1000, each response matching its request cursor and the
    // authoritative totalLines 8200, terminal hasMore=false with no
    // nextCursor). The chain is asserted here because the final 8200-line
    // window only renders once the terminal page has merged.
    assertCodeReviewPageChain(sentFrames, receivedFrames, bigFile, 8200);
    const grownState = await page.evaluate(() => ({
      banner: Array.from(document.querySelectorAll('.code-review__banner, .code-review__banner--truncated')).some(
        (el) => /truncat/i.test(el.textContent || '')
      ),
      status: document.querySelector('.code-review__page-status')?.textContent || null,
      loadMore: document.querySelector('.code-review__load-more') !== null,
      loadFull: document.querySelector('.code-review__load-full') !== null,
    }));
    if (grownState.banner || grownState.status || grownState.loadMore || grownState.loadFull) {
      fail(`fully-paged oversize file must render no banner/status/buttons (got ${JSON.stringify(grownState)})`);
    }
    // The panel-level "Large diff — all changed files are listed; file
    // bodies load in bounded pages on demand" notice was REMOVED from the
    // panel: with the oversize file open, NO banner anywhere may carry that
    // text — ordinary paging renders no truncation surface at all.
    const panelLevelNotice = await page.evaluate(() =>
      Array.from(document.querySelectorAll('.code-review__banner, .code-review__banner--truncated, [role="status"]')).some((el) =>
        (el.textContent || '').includes('Large diff — all changed files')
      )
    );
    if (panelLevelNotice) {
      fail('the removed panel-level "Large diff — all changed files…" notice still renders');
    }

    // ---- Large-diff input responsiveness ----
    // With the full ~8200-line diff rendered (the heaviest DOM this panel
    // produces), typing into the comment composer must stay responsive: the
    // textarea NODE is never remounted per keystroke (stable identity), the
    // whole edit causes bounded DOM mutation (the rAF-coalesced auto-resize,
    // never a full subtree re-render per keystroke), and the edit completes
    // within a strict wall-clock bound. This is a real-browser measurement
    // of the composed edit — not an inflated test timeout.
    const bigHunkClicked = await page.evaluate(() => {
      const header = document.querySelector('button.code-review__hunk-header');
      if (!header) return false;
      header.click();
      return true;
    });
    if (!bigHunkClicked) fail('large-diff responsiveness: no hunk header in the big file diff');
    await waitFor(
      page,
      () => document.querySelector('.code-review__comment-input') !== null,
      'large-diff responsiveness: composer did not open for the big file hunk'
    );
    const responsiveness = await page.evaluate(() => {
      const input = document.querySelector('.code-review__comment-input');
      if (!input) return null;
      input.dataset.responsivenessProbe = '1';
      const stats = { mutations: 0 };
      const observer = new MutationObserver((records) => {
        stats.mutations += records.length;
      });
      observer.observe(input.parentElement, {
        subtree: true,
        childList: true,
        attributes: true,
        characterData: true,
      });
      const text = 'responsiveness probe line one\nline two\nline three ' + 'x'.repeat(40);
      const t0 = performance.now();
      for (let i = 0; i < text.length; i++) {
        input.value = text.slice(0, i + 1);
        input.dispatchEvent(
          new InputEvent('input', { bubbles: true, inputType: 'insertText', data: text[i] })
        );
      }
      const elapsed = performance.now() - t0;
      observer.disconnect();
      const live = document.querySelector('.code-review__comment-input');
      const stillSameNode = live !== null && live.dataset.responsivenessProbe === '1';
      delete input.dataset.responsivenessProbe;
      return { elapsed, mutations: stats.mutations, chars: text.length, stillSameNode, value: input.value };
    });
    if (!responsiveness) fail('large-diff responsiveness probe could not bind the composer');
    if (!responsiveness.stillSameNode) {
      fail('large-diff responsiveness: the composer textarea was REMOUNTED during typing (identity changed)');
    }
    if (responsiveness.value.length !== responsiveness.chars) {
      fail(`large-diff responsiveness: keystrokes lost (value ${responsiveness.value.length}/${responsiveness.chars})`);
    }
    // Bounded DOM churn: the auto-resize coalesces per-frame work, so the
    // whole edit must not rewrite the composer subtree per keystroke.
    if (responsiveness.mutations > responsiveness.chars * 6) {
      fail(`large-diff responsiveness: per-keystroke DOM churn too high (${responsiveness.mutations} mutations for ${responsiveness.chars} keystrokes)`);
    }
    // Bounded main-thread time for the full edit with ~8200 diff lines in
    // the DOM (a per-keystroke O(n) re-render would blow this bound).
    if (responsiveness.elapsed > 1000) {
      fail(`large-diff responsiveness: edit took ${responsiveness.elapsed.toFixed(1)}ms (bound 1000ms)`);
    }
    // Clear the probe draft so the close guards never see an unsent draft.
    await page.fill('.code-review__comment-input', '');
    await laneShot(page, 'large-diff-responsive.png', { fullPage: true });
    // Back to the full list + the dirty file for the remaining phases.
    await page.fill('.code-review__file-filter', '');
    await waitFor(
      page,
      (want) => Array.from(document.querySelectorAll('.code-review__file')).some((b) =>
        b.getAttribute('data-file-path') === want
      ),
      `file list did not restore ${dirtyFile}`,
      10000,
      dirtyFile
    );

    // ---- File list tree keyboard navigation ----
    // The changed-file rail is one roving-focus row list spanning dirs and
    // files: ArrowUp/ArrowDown move across all rows, Home/End jump to the
    // ends, Enter toggles a focused dir and selects a focused file, and
    // ArrowRight/ArrowLeft expand/collapse a focused dir. Nested/ holds
    // more than one child once File Explorer rename fixtures land.
    const treeRowTexts = await page.evaluate(() =>
      Array.from(document.querySelectorAll('.code-review__file-list [role="treeitem"]'))
        .map((li) => (li.querySelector('button')?.textContent || '').trim())
    );
    if (treeRowTexts.length < 3) {
      fail(`file tree has too few rows for keyboard nav (${treeRowTexts.join(' | ')})`);
    }
    const greetRow = treeRowTexts.findIndex((t) => t.includes(dirtyFile));
    if (greetRow < 0) fail(`dirty file ${dirtyFile} not in the rendered file tree`);
    await page.evaluate((idx) => {
      document.querySelectorAll('.code-review__file-list [role="treeitem"] button')[idx].focus();
    }, greetRow);
    // Arrow toward the adjacent row (direction depends on the dirty file's
    // position) and assert the roving focus moved there.
    const moveKey = greetRow + 1 < treeRowTexts.length ? 'ArrowDown' : 'ArrowUp';
    const adjacentText = treeRowTexts[greetRow + (moveKey === 'ArrowDown' ? 1 : -1)];
    await page.keyboard.press(moveKey);
    const afterMove = await page.evaluate(() => document.activeElement?.textContent?.trim() || '');
    if (!afterMove.includes(adjacentText)) {
      fail(`${moveKey} did not move the roving focus to ${adjacentText} (active=${afterMove})`);
    }
    // Home -> the current physical first row (project fixtures may add .pi/).
    await page.keyboard.press('Home');
    const afterHome = await page.evaluate(() => document.activeElement?.textContent?.trim() || '');
    if (!afterHome.includes(treeRowTexts[0])) {
      fail(`Home did not jump the focus to the first tree row ${treeRowTexts[0]} (active=${afterHome})`);
    }
    // Focus nested explicitly before exercising directory Enter/arrow toggles.
    await page.evaluate(() => {
      document.querySelector('.code-review__tree-dir[data-dir-path="nested"]')?.focus();
    });
    const nestedKeyboardChildren = await page.evaluate(() =>
      document.querySelectorAll('.code-review__file-list .code-review__file[data-file-path^="nested/"]').length
    );
    if (nestedKeyboardChildren < 1) fail('keyboard nav: nested/ has no child file rows');
    await page.keyboard.press('Enter');
    await waitFor(
      page,
      ({ before, children }) => document.querySelectorAll('.code-review__file-list [role="treeitem"]').length === before - children,
      'Enter on the dir row did not collapse the tree (child row still visible)',
      10000,
      { before: treeRowTexts.length, children: nestedKeyboardChildren }
    );
    await page.keyboard.press('Enter');
    await waitFor(
      page,
      (n) => document.querySelectorAll('.code-review__file-list [role="treeitem"]').length === n,
      'Enter on the collapsed dir row did not re-expand the tree',
      10000,
      treeRowTexts.length
    );
    // ArrowRight expands a collapsed dir; ArrowLeft collapses an expanded
    // one. Leave the tree expanded (the default state) afterwards.
    await page.keyboard.press('Enter'); // collapse nested
    await waitFor(
      page,
      ({ before, children }) => document.querySelectorAll('.code-review__file-list [role="treeitem"]').length === before - children,
      'tree did not collapse before the ArrowRight gate',
      10000,
      { before: treeRowTexts.length, children: nestedKeyboardChildren }
    );
    await page.keyboard.press('ArrowRight');
    await waitFor(
      page,
      (n) => document.querySelectorAll('.code-review__file-list [role="treeitem"]').length === n,
      'ArrowRight did not expand the collapsed dir row',
      10000,
      treeRowTexts.length
    );
    await page.keyboard.press('ArrowLeft');
    await waitFor(
      page,
      ({ before, children }) => document.querySelectorAll('.code-review__file-list [role="treeitem"]').length === before - children,
      'ArrowLeft did not collapse the expanded dir row',
      10000,
      { before: treeRowTexts.length, children: nestedKeyboardChildren }
    );
    await page.keyboard.press('ArrowRight');
    await waitFor(
      page,
      (n) => document.querySelectorAll('.code-review__file-list [role="treeitem"]').length === n,
      'ArrowRight did not re-expand the collapsed dir row',
      10000,
      treeRowTexts.length
    );
    // End -> last row (a file in this fixture); Enter selects it.
    const lastTreeFile = await page.evaluate(() => {
      const items = Array.from(document.querySelectorAll('.code-review__file-list [role="treeitem"]'));
      for (let i = items.length - 1; i >= 0; i--) {
        const path = items[i].querySelector('button.code-review__file')?.getAttribute('data-file-path');
        if (path) return path;
      }
      return '';
    });
    if (!lastTreeFile) fail('file tree has no file row for End/Enter');
    await page.keyboard.press('End');
    const afterEnd = await page.evaluate(() => document.activeElement?.textContent?.trim() || '');
    const lastName = lastTreeFile.split('/').pop();
    if (!afterEnd.includes(lastName)) {
      fail(`End did not jump the focus to the last tree row ${lastTreeFile} (active=${afterEnd})`);
    }
    await page.keyboard.press('Enter');
    await waitFor(
      page,
      (want) => (document.querySelector('.code-review__diff-path')?.textContent || '').includes(want),
      'Enter did not select the focused file row',
      10000,
      lastName
    );

    // ---- Explicit hunk selection opens the thread + composer ----
    await page.evaluate((want) => {
      const btn = Array.from(document.querySelectorAll('.code-review__file')).find((b) =>
        b.getAttribute('data-file-path') === want
      );
      if (btn) btn.click();
    }, dirtyFile);
    await waitFor(
      page,
      () => document.querySelector('.code-review__line--deletion') !== null,
      'dirty file diff did not re-render after keyboard navigation'
    );
    // ---- Solidity lexing in the REVIEW diff (desktop) ----
    // src/ReviewToken.sol is dirty in this fixture (the committed baseline
    // differs from the working tree; clean Token.sol is the Explorer's), so
    // the changed-file rail carries a real M row whose diff is Solidity
    // source. The shared grammar must lex it — keyword/type tokens on BOTH
    // sides — while every rendered line keeps its exact source bytes (the
    // review counterpart of the Explorer Source preview contract).
    await page.evaluate((want) => {
      const btn = Array.from(document.querySelectorAll('.code-review__file')).find((b) =>
        b.getAttribute('data-file-path') === want
      );
      if (btn) btn.click();
    }, solReviewFile);
    await waitFor(
      page,
      (want) => (document.querySelector('.code-review__diff-path')?.textContent || '').includes(want),
      `selecting ${solReviewFile} in the review rail did not render its diff`,
      20000,
      solReviewFile
    );
    await waitFor(
      page,
      () => document.querySelector('.code-review__line--deletion') !== null
        && document.querySelector('.code-review__line--addition') !== null
        && Array.from(document.querySelectorAll('.code-review__line-text'))
          .some((el) => (el.textContent || '').includes('TokenStore')),
      `${solReviewFile} review diff never rendered its changed Solidity lines`
    );
    assertSolidityReviewDiff(await reviewSolidityProof(page), 'desktop solidity review');
    // Evidence hygiene before the capture (test-side only): the keyboard
    // navigation's End press left the rail scrolled to its LAST row and the
    // roving focus on it, and the stationary pointer from earlier real rail
    // clicks :hovers whatever row now sits under it — so the SELECTED row
    // (the one the diff header names) can be scrolled out of view. Scroll it
    // into view and park the pointer off the rail so the screenshot shows the
    // row the header names without a stale hover highlight. The focus ring is
    // genuine roving-focus state from the exercised keyboard phase and stays.
    await page.evaluate((want) => {
      const btn = Array.from(document.querySelectorAll('.code-review__file')).find((b) =>
        b.getAttribute('data-file-path') === want
      );
      btn?.scrollIntoView({ block: 'center' });
    }, solReviewFile);
    await page.mouse.move(0, 0);
    await laneShot(page, 'code-review-sol-diff-desktop.png', { fullPage: true });
    // Return to the original review file/selection before the remaining
    // assertions: the dirty file with NO hunk selected (the composer stays
    // cleared until the explicit hunk click below).
    await page.evaluate((want) => {
      const btn = Array.from(document.querySelectorAll('.code-review__file')).find((b) =>
        b.getAttribute('data-file-path') === want
      );
      if (btn) btn.click();
    }, dirtyFile);
    await waitFor(
      page,
      () => document.querySelector('.code-review__line--deletion') !== null,
      'dirty file diff did not re-render after the Solidity review leg'
    );
    const solReturnCleared = await page.evaluate(() => ({
      composer: document.querySelector('.code-review__comment-input') !== null,
      selected: document.querySelector('.code-review__hunk.is-selected') !== null,
    }));
    if (solReturnCleared.composer || solReturnCleared.selected) {
      fail(`returning from the Solidity review leg did not restore the cleared dirty-file selection (composer=${solReturnCleared.composer}, selected=${solReturnCleared.selected})`);
    }
    const composerBefore = await page.evaluate(
      () => document.querySelector('.code-review__comment-input') !== null
    );
    if (composerBefore) fail('composer rendered before explicit hunk selection');
    await page.click('.code-review__hunk-header');
    await waitFor(
      page,
      () => document.querySelector('.code-review__comment-input') !== null,
      'composer did not appear after explicit hunk selection'
    );

    // ---- Enter submits; Shift+Enter and IME-composing Enter do not ----
    // The composer keybinding is "Enter to submit · Shift+Enter for a newline"
    // (the composer hint). Shift+Enter must only insert a newline — never
    // dispatch a comment. A synthetic composing Enter (the keydown that
    // confirms an IME composition, isComposing=true) must likewise never
    // submit. Plain Enter is the real submit path.
    const commentInputSel = '.code-review__comment-input';
    const commentText = 'explicit hunk comment from the e2e lane';
    const userCommentCount = () =>
      page.evaluate(() => document.querySelectorAll('.code-review__comment--user').length);
    // (a) Shift+Enter: the textarea value grows by a newline and no comment
    // is submitted (no user card, no streaming state).
    await page.fill(commentInputSel, 'first line');
    await page.press(commentInputSel, 'Shift+Enter');
    const afterShiftEnter = await page.evaluate(
      () => document.querySelector('.code-review__comment-input')?.value || ''
    );
    if (!afterShiftEnter.includes('\n')) {
      fail(`Shift+Enter must insert a newline without submitting (value=${JSON.stringify(afterShiftEnter)})`);
    }
    const commentsBeforeShift = await userCommentCount();
    await page.waitForTimeout(700);
    const commentsAfterShift = await userCommentCount();
    if (commentsAfterShift !== commentsBeforeShift) {
      fail(`Shift+Enter submitted a comment (user cards ${commentsBeforeShift} -> ${commentsAfterShift}) — it must only insert a newline`);
    }
    if (await page.evaluate(() => document.querySelector('.code-review__streaming') !== null)) {
      fail('Shift+Enter started a review reply — it must only insert a newline');
    }
    // (b) Synthetic composing Enter (IME confirm): must not submit and must
    // not be preventDefault-ed (the composing Enter keeps its native role).
    await page.fill(commentInputSel, 'ime draft');
    const composingEnter = await page.evaluate(() => {
      const el = document.querySelector('.code-review__comment-input');
      const ev = new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true });
      Object.defineProperty(ev, 'isComposing', { value: true });
      const prevented = !el.dispatchEvent(ev);
      return { prevented };
    });
    if (composingEnter.prevented) {
      fail('a composing Enter was preventDefault-ed — the IME-confirm Enter must never submit');
    }
    const commentsBeforeComposing = await userCommentCount();
    await page.waitForTimeout(700);
    const commentsAfterComposing = await userCommentCount();
    if (commentsAfterComposing !== commentsBeforeComposing) {
      fail('a composing Enter submitted the draft — the IME-confirm Enter must never submit');
    }
    // (c) Plain Enter submits.
    await page.fill(commentInputSel, commentText);
    await page.press(commentInputSel, 'Enter');
    await waitFor(
      page,
      (want) =>
        Array.from(document.querySelectorAll('.code-review__comment--user')).some((el) =>
          (el.querySelector('.code-review__comment-text')?.textContent || '').includes(want)
        ),
      'Enter did not submit the comment (user comment missing from the thread)',
      20000,
      commentText
    );

    // ---- Streaming: composer enabled, queued comment, Abort ----
    // The steering mock streams the review reply slowly (odd request), so
    // the snapshot reports the streaming state until it completes. A
    // streaming reply must NOT block the composer (no disabled textarea, no
    // streaming-warning note): a second comment submits immediately and
    // renders as a queued card behind the first reply. Abort clears the
    // active stream AND drops the queued (not-yet-started) comment with the
    // queue (the backend FIFO contract) while leaving the partial assistant
    // reply.
    await waitFor(
      page,
      () => document.querySelector('.code-review__streaming') !== null,
      'streaming state did not appear after the comment (steering mock slow stream)',
      10000
    );
    const streamComposerState = await page.evaluate(() => {
      const el = document.querySelector('.code-review__comment-input');
      return {
        present: el !== null,
        disabled: el ? el.disabled : true,
        warningNote: document.querySelector('.code-review__streaming-note') !== null,
        hint: document.querySelector('.code-review__composer-hint')?.textContent?.trim() || '',
      };
    });
    if (!streamComposerState.present || streamComposerState.disabled) {
      fail(`composer must stay ENABLED while the reply streams (present=${streamComposerState.present} disabled=${streamComposerState.disabled})`);
    }
    if (streamComposerState.warningNote) {
      fail('the streaming-warning note must be ABSENT while the reply streams (.code-review__streaming-note present)');
    }
    if (!streamComposerState.hint.includes('Shift+Enter')) {
      fail(`composer hint must document Enter/Shift+Enter (hint=${JSON.stringify(streamComposerState.hint)})`);
    }
    if (!streamComposerState.hint.includes('Esc to stop') || streamComposerState.hint.includes('abort')) {
      fail(`composer hint must use Stop language (hint=${JSON.stringify(streamComposerState.hint)})`);
    }
    const stopCopy = await page.evaluate(() => {
      const btn = document.querySelector('button.code-review__action--stop');
      return {
        text: (btn?.textContent || '').replace(/\s+/g, ' ').trim(),
        aria: btn?.getAttribute('aria-label') || '',
      };
    });
    if (!stopCopy.text.includes('Stop') || stopCopy.text.includes('Abort')
      || !stopCopy.aria.includes('Stop')) {
      fail(`streaming interrupt control is not Stop: ${JSON.stringify(stopCopy)}`);
    }
    // Second comment while the first reply streams: submits immediately and
    // renders as a queued user card (the backend FIFO, not a rejection).
    const queuedText = 'queued-second-comment';
    await page.fill(commentInputSel, queuedText);
    await page.press(commentInputSel, 'Enter');
    await waitFor(
      page,
      (want) =>
        Array.from(document.querySelectorAll('.code-review__comment--user')).some((el) =>
          (el.querySelector('.code-review__comment-text')?.textContent || '').includes(want)
        ),
      'second comment did not submit + render while the first reply streams',
      20000,
      queuedText
    );
    if (!(await page.evaluate(() => document.querySelector('.code-review__streaming') !== null))) {
      fail('the second comment should sit queued behind the still-streaming first reply');
    }
    // Abort: clears the stream and drops the queued (not-yet-started)
    // comment; the aborted in-flight reply stays as a partial comment.
    await page.click('button.code-review__action--stop');
    await waitFor(
      page,
      () => document.querySelector('.code-review__streaming') === null,
      'abort did not clear the streaming state',
      15000
    );
    const queuedGone = await page.evaluate(
      (want) =>
        !Array.from(document.querySelectorAll('.code-review__comment--user')).some((el) =>
          (el.querySelector('.code-review__comment-text')?.textContent || '').includes(want)
        ),
      queuedText
    );
    if (!queuedGone) {
      fail('abort did not drop the queued (not-yet-started) comment from the thread');
    }
    const partialText = await page.evaluate(() => {
      const el = document.querySelector('.code-review__comment--assistant.is-partial');
      return el ? (el.querySelector('.code-review__comment-text')?.textContent || '') : '';
    });
    if (!partialText.trim()) {
      fail('abort did not leave a partial assistant comment in the thread');
    }

    // ---- Second-hunk comment belongs ONLY to its own thread ----
    // Select the SECOND hunk explicitly; the comment must land in that
    // hunk's thread and never leak into the first hunk's thread.
    await page.evaluate(() => {
      document.querySelectorAll('.code-review__hunk-header')[1].click();
    });
    await waitFor(
      page,
      () => document.querySelector('.code-review__comment-input') !== null,
      'composer did not open for the second hunk'
    );
    const hunk2Id = await page.evaluate(() =>
      (document.querySelector('.code-review__hunk-id')?.textContent || '').replace(/\s+/g, ' ').trim()
    );
    if (!hunk2Id.includes('@@ -13,6 +14,7')) {
      fail(`second hunk identity wrong (hunk-id="${hunk2Id}")`);
    }
    const secondHunkComment = 'hunk-two-exclusive-comment';
    await page.fill('.code-review__comment-input', secondHunkComment);
    await page.press('.code-review__comment-input', 'Enter');
    await waitFor(
      page,
      (want) =>
        Array.from(document.querySelectorAll('.code-review__comment--user')).some((el) =>
          (el.querySelector('.code-review__comment-text')?.textContent || '').includes(want)
        ),
      'second-hunk comment did not submit',
      20000,
      secondHunkComment
    );
    // The reply is the instant even-numbered mock turn. While it lands the DOM
    // can atomically show more cards than the badge counts (cards render from
    // the live-merged thread, the badge from the snapshot count), so any bare
    // badge/entries read races. Wait for the SETTLED reply in this hunk's open
    // thread, then read badge + thread shape in ONE evaluate and pin the
    // terminal state exactly: 1 user comment, 1 settled reply, badge 2
    // (countThreadComments = comments.length, title "N comments").
    await waitFor(
      page,
      (want) => {
        const thread = document.querySelector('.code-review__thread');
        const users = thread ? Array.from(thread.querySelectorAll('.code-review__comment--user')) : [];
        return users.length === 1
          && (users[0]?.querySelector('.code-review__comment-text')?.textContent || '').includes(want)
          && Array.from(thread.querySelectorAll('.code-review__comment--assistant'))
            .some((el) => !el.classList.contains('is-partial'));
      },
      'second-hunk reply did not settle before the badge read',
      20000,
      secondHunkComment
    );
    const hunk2Thread = await page.evaluate(() => {
      const thread = document.querySelector('.code-review__thread');
      const assistants = Array.from(thread?.querySelectorAll('.code-review__comment--assistant') ?? []);
      return {
        badge: document.querySelectorAll('.code-review__hunk')[1]?.querySelector('.code-review__hunk-badge')?.textContent?.trim() || '',
        userCount: thread?.querySelectorAll('.code-review__comment--user').length ?? 0,
        settled: assistants.filter((el) => !el.classList.contains('is-partial')).length,
        partial: assistants.filter((el) => el.classList.contains('is-partial')).length,
      };
    });
    if (hunk2Thread.userCount !== 1 || hunk2Thread.settled !== 1 || hunk2Thread.partial !== 0) {
      fail(`second hunk thread must be exactly one comment + one settled reply (got ${JSON.stringify(hunk2Thread)})`);
    }
    if (hunk2Thread.badge !== '2') {
      fail(`second hunk badge must count both thread entries after the settled reply (got ${JSON.stringify(hunk2Thread.badge)})`);
    }
    // Switch to the FIRST hunk: its thread must NOT carry the second hunk's
    // comment, and its own earlier comment must still be there.
    await page.evaluate(() => {
      document.querySelectorAll('.code-review__hunk-header')[0].click();
    });
    const hunk1ThreadText = await page.evaluate(() =>
      document.querySelector('.code-review__thread')?.textContent || ''
    );
    if (hunk1ThreadText.includes(secondHunkComment)) {
      fail('second-hunk comment leaked into the first hunk thread');
    }
    if (!hunk1ThreadText.includes('explicit hunk comment from the e2e lane')) {
      fail('first hunk lost its own comment after the second-hunk comment');
    }
    const hunk1Id = await page.evaluate(() =>
      (document.querySelector('.code-review__hunk-id')?.textContent || '').replace(/\s+/g, ' ').trim()
    );
    if (!hunk1Id.includes('@@ -1,6 +1,7')) {
      fail(`first hunk identity wrong (hunk-id="${hunk1Id}")`);
    }
    // Back to the second hunk: the comment + thread survive (identity-keyed).
    await page.evaluate(() => {
      document.querySelectorAll('.code-review__hunk-header')[1].click();
    });
    await waitFor(
      page,
      (want) =>
        Array.from(document.querySelectorAll('.code-review__comment--user')).some((el) =>
          (el.querySelector('.code-review__comment-text')?.textContent || '').includes(want)
        ),
      'second-hunk comment did not persist across hunk switches',
      10000,
      secondHunkComment
    );
    // Let the (instant, even-numbered) assistant reply land before the next
    // comment so the next submit starts a fresh run on this hunk.
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.code-review__comment--assistant')).some((el) =>
          !el.classList.contains('is-partial')
        ),
      'assistant reply to the second-hunk comment never landed',
      20000
    );

    // ---- Per-hunk drafts: A/B survive switches; submitting A leaves B ----
    // Drafts are keyed per hunk identity: switching hunks must preserve each
    // hunk's unsent draft, and submitting one hunk's draft must not touch
    // the other hunk's draft.
    await page.evaluate(() => {
      document.querySelectorAll('.code-review__hunk-header')[0].click();
    });
    await waitFor(
      page,
      () => document.querySelector('.code-review__comment-input') !== null,
      'composer did not open for the draft gate'
    );
    await page.fill('.code-review__comment-input', 'draft-A-alpha');
    await page.evaluate(() => {
      document.querySelectorAll('.code-review__hunk-header')[1].click();
    });
    await waitFor(
      page,
      () => document.querySelector('.code-review__comment-input') !== null,
      'composer did not open on the second hunk for the draft gate'
    );
    const draftB0 = await page.evaluate(
      () => document.querySelector('.code-review__comment-input')?.value || ''
    );
    if (draftB0 !== '') {
      fail(`second hunk inherited the first hunk's draft (value=${JSON.stringify(draftB0)})`);
    }
    await page.fill('.code-review__comment-input', 'draft-B-beta');
    await page.evaluate(() => {
      document.querySelectorAll('.code-review__hunk-header')[0].click();
    });
    await waitFor(
      page,
      () => document.querySelector('.code-review__comment-input')?.value === 'draft-A-alpha',
      'draft A did not survive the hunk switch',
      10000
    );
    // Submit A (the odd-numbered slow mock request: streaming state must
    // appear, then clear; the reply completes on its own).
    await page.press('.code-review__comment-input', 'Enter');
    await waitFor(
      page,
      (want) =>
        Array.from(document.querySelectorAll('.code-review__comment--user')).some((el) =>
          (el.querySelector('.code-review__comment-text')?.textContent || '').includes(want)
        ),
      'draft A did not submit',
      20000,
      'draft-A-alpha'
    );
    await waitFor(
      page,
      () => document.querySelector('.code-review__streaming') !== null,
      'streaming state did not appear after submitting draft A (slow mock request)',
      15000
    );
    await waitFor(
      page,
      () => document.querySelector('.code-review__streaming') === null,
      'streaming state did not clear after submitting draft A',
      25000
    );
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.code-review__comment--assistant')).some((el) =>
          !el.classList.contains('is-partial')
        ),
      'assistant reply to draft A never landed',
      20000
    );
    // B survives A's submit.
    await page.evaluate(() => {
      document.querySelectorAll('.code-review__hunk-header')[1].click();
    });
    await waitFor(
      page,
      () => document.querySelector('.code-review__comment-input')?.value === 'draft-B-beta',
      'draft B did not survive submitting draft A',
      10000
    );

    // ---- Hostile comment text stays literal (no dialog/script side effect) ----
    // Overwrite draft B with hostile markup and submit: the panel must render
    // it as inert text and must never execute it or open a dialog.
    const hostileComment = '<script>window.__crPwned=3</script><img src=x onerror="window.__crPwned=4">';
    const assistantCountBefore = await page.evaluate(
      () => document.querySelectorAll('.code-review__comment--assistant:not(.is-partial)').length
    );
    await page.fill('.code-review__comment-input', hostileComment);
    await page.press('.code-review__comment-input', 'Enter');
    await waitFor(
      page,
      (want) =>
        Array.from(document.querySelectorAll('.code-review__comment--user')).some((el) =>
          (el.querySelector('.code-review__comment-text')?.textContent || '').includes(want)
        ),
      'hostile comment did not submit',
      20000,
      '<script>window.__crPwned=3</script>'
    );
    const hostileCommentEffects = await page.evaluate(() => ({
      img: document.querySelector('.code-review img[src="x"]') !== null,
      onerror: document.querySelector('.code-review [onerror]') !== null,
      pwned: typeof window.__crPwned !== 'undefined',
    }));
    if (hostileCommentEffects.img || hostileCommentEffects.onerror || hostileCommentEffects.pwned) {
      fail(`hostile comment created DOM/script side effects: ${JSON.stringify(hostileCommentEffects)}`);
    }
    if (dialogs > 0) {
      fail(`hostile comment opened ${dialogs} browser dialog(s)`);
    }
    // Let the instant reply land (the thread's committed assistant count must
    // grow by one) so the close guard sees no streaming.
    await waitFor(
      page,
      (base) =>
        document.querySelectorAll('.code-review__comment--assistant:not(.is-partial)').length > base,
      'assistant reply to the hostile comment never landed',
      20000,
      assistantCountBefore
    );
    await waitFor(
      page,
      () => document.querySelector('.code-review__streaming') === null,
      'streaming state did not clear after the hostile comment',
      15000
    );

    // ---- Comment markdown renders (strong / list / rust fence) ----
    // Comment bodies flow through the shared markdown renderer: **strong**,
    // lists, and ```rust fences render as structured elements, while hostile
    // HTML in the SAME comment stays literal text and never executes. This is
    // the 5th review comment (odd), so the steering mock streams the reply:
    // the streaming state must appear, then clear, before the DOM asserts.
    const markdownComment =
      '**bold**\n\n- list item one\n- list item two\n\n' +
      '```rust\nfn main() {\n    let x = 1;\n}\n```\n\n' +
      '<script>window.__crPwned=5</script>';
    await page.fill('.code-review__comment-input', markdownComment);
    await page.press('.code-review__comment-input', 'Enter');
    await waitFor(
      page,
      (want) =>
        Array.from(document.querySelectorAll('.code-review__comment--user')).some((el) =>
          (el.querySelector('.code-review__comment-text')?.textContent || '').includes(want)
        ),
      'markdown comment did not submit',
      20000,
      'fn main()'
    );
    await waitFor(
      page,
      () => document.querySelector('.code-review__streaming') !== null,
      'streaming state did not appear after the markdown comment (slow mock request)',
      15000
    );
    await waitFor(
      page,
      () => document.querySelector('.code-review__streaming') === null,
      'streaming state did not clear after the markdown comment',
      25000
    );
    const md = await page.evaluate(() => {
      const el = Array.from(document.querySelectorAll('.code-review__comment--user .code-review__comment-text')).find(
        (t) => (t.textContent || '').includes('fn main()')
      );
      if (!el) return null;
      return {
        strong: Array.from(el.querySelectorAll('strong')).map((n) => n.textContent).join(','),
        items: Array.from(el.querySelectorAll('ul li')).map((n) => n.textContent.trim()).join(','),
        fence: el.querySelector('pre code.hljs')?.textContent || '',
        fenceLangs: Array.from(el.querySelectorAll('.md-fence__lang')).map((n) => n.textContent.trim()).join(','),
        keywords: Array.from(el.querySelectorAll('code.hljs .hljs-keyword')).map((n) => n.textContent).join(','),
        hasImg: el.querySelector('img[src="x"]') !== null,
        hasOnerror: el.querySelector('[onerror]') !== null,
        scriptLiteral: (el.textContent || '').includes('<script>window.__crPwned=5</script>'),
      };
    });
    if (!md) fail('markdown comment element missing from the thread');
    if (!md.strong.includes('bold')) fail(`comment **strong** did not render (strong="${md.strong}")`);
    if (!md.items.includes('list item one') || !md.items.includes('list item two')) {
      fail(`comment list did not render (items="${md.items}")`);
    }
    if (!md.fence.includes('fn main()')) fail('comment rust fence did not render its source');
    if (!md.fenceLangs.includes('rust')) fail(`rust fence language label missing (langs="${md.fenceLangs}")`);
    if (!md.keywords.includes('fn')) fail(`rust fence not highlighted (keywords="${md.keywords}")`);
    if (md.hasImg || md.hasOnerror) fail('hostile HTML inside the markdown comment created elements');
    if (!md.scriptLiteral) fail('hostile script tag did not stay literal text in the markdown comment');
    const mdPwned = await page.evaluate(() => typeof window.__crPwned !== 'undefined');
    if (mdPwned) fail('hostile script inside the markdown comment executed');
    if (dialogs > 0) fail(`markdown comment opened ${dialogs} browser dialog(s)`);

    // ---- Snapshot polling: ~1.5s cadence while open, stops after close ----
    // The panel polls code_review_snapshot every 1500ms while mounted. In a
    // quiet window (no busy transitions) consecutive polls must land ~1.5s
    // apart; after the panel closes no further snapshot frames may appear.
    const pollStart = snapshotFrames.length;
    await page.waitForTimeout(3300);
    const pollWindow = snapshotFrames.slice(pollStart);
    if (pollWindow.length < 2) {
      fail(`expected at least 2 snapshot polls in 3.3s (got ${pollWindow.length})`);
    }
    for (let i = 1; i < pollWindow.length; i++) {
      const gap = pollWindow[i].t - pollWindow[i - 1].t;
      if (gap < 1200 || gap > 4000) {
        fail(`snapshot poll cadence violated (gap ${gap}ms; expected ~1500ms)`);
      }
    }

    // ---- Inline close confirm (draft guard), then Escape close ----
    await page.fill('.code-review__comment-input', 'keep-this-draft');
    await page.click('.code-review__title'); // move focus out of the composer
    await page.keyboard.press('Escape');
    await waitFor(
      page,
      () => document.querySelector('.code-review__confirm') !== null,
      'Escape with a draft did not show the inline close confirm'
    );
    const keptOpen = await page.evaluate(
      () => document.getElementById('code-review-panel') !== null
    );
    if (!keptOpen) fail('close confirm was dismissed but the panel closed anyway');
    await page.click('.code-review__confirm-actions button:nth-child(2)'); // Keep editing
    await waitFor(
      page,
      () => document.querySelector('.code-review__confirm') === null,
      'close confirm did not dismiss'
    );
    const draftKept = await page.evaluate(
      () => document.querySelector('.code-review__comment-input')?.value || ''
    );
    if (draftKept !== 'keep-this-draft') {
      fail(`draft was not preserved across the confirm (value=${draftKept})`);
    }
    await page.fill('.code-review__comment-input', '');
    await page.click('.code-review__title');
    await page.keyboard.press('Escape');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') === null,
      'Escape did not close the panel with an empty draft'
    );
    // Reopen a fresh panel so the live-textarea Close/Refresh gate starts
    // with no leftover draft and later lane steps are not polluted.
    await page.fill('#prompt-input', '/code-review');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') !== null,
      'code-review panel did not reopen for the live-textarea draft gate'
    );
    await waitFor(
      page,
      () => document.querySelector('.code-review__hunk-header') !== null,
      'reopened review panel never rendered a hunk header'
    );
    await page.click('.code-review__hunk-header');
    await waitFor(
      page,
      () => document.querySelector('.code-review__comment-input') !== null,
      'composer did not appear after reopening for the live-textarea draft gate'
    );

    // ---- Immediate Close/Refresh while the textarea still holds focus ----
    // The composer is uncontrolled: React draftCount is stale until blur.
    // Close and Refresh must read composerRef.current.value synchronously.
    // Do NOT click the title, switch hunks, or otherwise blur first.
    // Exact live bytes include leading/trailing whitespace and a newline so
    // Keep editing cannot pass on a trimmed substitute.
    const closeOwnerSid = await page.evaluate(
      () => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
    );
    if (!closeOwnerSid) fail('live Close gate: owning session id missing before the focused-textarea Close');
    const closeFramesBeforeLive = codeReviewCloseFrames(sentFrames);
    const liveCloseDraft = '  live-close-no-blur  \n';
    await page.fill('.code-review__comment-input', liveCloseDraft);
    const closeFocus = await page.evaluate(
      () => document.activeElement?.classList.contains('code-review__comment-input') === true
    );
    if (!closeFocus) fail('live Close gate: composer lost focus before the × click (cannot rely on blur)');
    // Playwright page.click() blurs the textarea first (onBlur commits).
    // Dispatch the button click from the page so the live value is still
    // only in composerRef.current.value when requestClose runs.
    const closeClicked = await page.evaluate(() => {
      const input = document.querySelector('.code-review__comment-input');
      const btn = document.querySelector('button.code-review__close');
      if (!input || !btn) return false;
      if (document.activeElement !== input) return false;
      btn.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }));
      return true;
    });
    if (!closeClicked) fail('live Close gate: could not dispatch × click while the textarea held focus');
    await waitFor(
      page,
      () => document.querySelector('.code-review__confirm') !== null,
      'immediate Close with a focused live textarea did not show the inline confirm'
    );
    const closeConfirm = await page.evaluate(() => ({
      panel: document.getElementById('code-review-panel') !== null,
      message: document.querySelector('.code-review__confirm p')?.textContent || '',
      keep: document.querySelector('.code-review__confirm-actions button:nth-child(2)')?.textContent?.trim() || '',
      primary: document.querySelector('.code-review__confirm-actions button:nth-child(1)')?.textContent?.trim() || '',
    }));
    if (!closeConfirm.panel) fail('immediate Close discarded the panel instead of confirming');
    if (!closeConfirm.message.includes('1 unsent draft')) {
      fail(`immediate Close confirm did not count the live textarea (message=${JSON.stringify(closeConfirm.message)})`);
    }
    if (closeConfirm.keep !== 'Keep editing') {
      fail(`immediate Close confirm missing Keep editing (label=${JSON.stringify(closeConfirm.keep)})`);
    }
    if (closeConfirm.primary !== 'Close panel') {
      fail(`immediate Close confirm missing Close panel (label=${JSON.stringify(closeConfirm.primary)})`);
    }
    await page.click('.code-review__confirm-actions button:nth-child(2)');
    await waitFor(
      page,
      () => document.querySelector('.code-review__confirm') === null,
      'immediate Close Keep editing did not dismiss the confirm'
    );
    const closeKept = await page.evaluate(() => {
      const input = document.querySelector('.code-review__comment-input');
      return {
        value: input?.value ?? null,
        focused: document.activeElement === input,
      };
    });
    if (closeKept.value !== liveCloseDraft) {
      fail(`Keep editing after immediate Close lost exact bytes (value=${JSON.stringify(closeKept.value)})`);
    }
    if (!closeKept.focused) fail('Keep editing after immediate Close did not restore composer focus');

    const liveRefreshDraft = '  live-refresh-no-blur  \n';
    await page.fill('.code-review__comment-input', liveRefreshDraft);
    const refreshFocus = await page.evaluate(
      () => document.activeElement?.classList.contains('code-review__comment-input') === true
    );
    if (!refreshFocus) fail('live Refresh gate: composer lost focus before the Refresh click (cannot rely on blur)');
    const refreshBeforeKeep = sentFrames.filter((p) => {
      try { return JSON.parse(p).type === 'code_review_refresh'; } catch { return false; }
    }).length;
    const refreshClicked = await page.evaluate(() => {
      const input = document.querySelector('.code-review__comment-input');
      const btn = document.querySelector('button.code-review__action[title="Reload the diff snapshot"]');
      if (!input || !btn) return false;
      if (document.activeElement !== input) return false;
      btn.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }));
      return true;
    });
    if (!refreshClicked) fail('live Refresh gate: could not dispatch Refresh click while the textarea held focus');
    await waitFor(
      page,
      () => document.querySelector('.code-review__confirm') !== null,
      'immediate Refresh with a focused live textarea did not show the inline confirm'
    );
    const refreshConfirm = await page.evaluate(() => ({
      panel: document.getElementById('code-review-panel') !== null,
      message: document.querySelector('.code-review__confirm p')?.textContent || '',
      keep: document.querySelector('.code-review__confirm-actions button:nth-child(2)')?.textContent?.trim() || '',
      primary: document.querySelector('.code-review__confirm-actions button:nth-child(1)')?.textContent?.trim() || '',
      busy: document.querySelector('.code-review__busy') !== null,
    }));
    if (!refreshConfirm.panel) fail('immediate Refresh closed the panel instead of confirming');
    if (refreshConfirm.busy) fail('immediate Refresh started refreshing before confirm');
    if (!refreshConfirm.message.includes('1 draft') || !refreshConfirm.message.includes('will be kept')) {
      fail(`immediate Refresh confirm did not count the live textarea (message=${JSON.stringify(refreshConfirm.message)})`);
    }
    if (refreshConfirm.keep !== 'Keep editing') {
      fail(`immediate Refresh confirm missing Keep editing (label=${JSON.stringify(refreshConfirm.keep)})`);
    }
    if (refreshConfirm.primary !== 'Refresh now') {
      fail(`immediate Refresh confirm missing Refresh now (label=${JSON.stringify(refreshConfirm.primary)})`);
    }
    await page.click('.code-review__confirm-actions button:nth-child(2)');
    await waitFor(
      page,
      () => document.querySelector('.code-review__confirm') === null,
      'immediate Refresh Keep editing did not dismiss the confirm'
    );
    const refreshKept = await page.evaluate(() => {
      const input = document.querySelector('.code-review__comment-input');
      return {
        value: input?.value ?? null,
        focused: document.activeElement === input,
      };
    });
    if (refreshKept.value !== liveRefreshDraft) {
      fail(`Keep editing after immediate Refresh lost exact bytes (value=${JSON.stringify(refreshKept.value)})`);
    }
    if (!refreshKept.focused) fail('Keep editing after immediate Refresh did not restore composer focus');
    const refreshAfterKeep = sentFrames.filter((p) => {
      try { return JSON.parse(p).type === 'code_review_refresh'; } catch { return false; }
    }).length;
    if (refreshAfterKeep !== refreshBeforeKeep) {
      fail(`Keep editing after immediate Refresh dispatched code_review_refresh (${refreshBeforeKeep} -> ${refreshAfterKeep})`);
    }

    // Primary Refresh: confirm then dispatch exactly one code_review_refresh.
    const refreshBeforePrimary = sentFrames.filter((p) => {
      try { return JSON.parse(p).type === 'code_review_refresh'; } catch { return false; }
    }).length;
    const refreshPrimaryClicked = await page.evaluate(() => {
      const input = document.querySelector('.code-review__comment-input');
      const btn = document.querySelector('button.code-review__action[title="Reload the diff snapshot"]');
      if (!input || !btn) return false;
      if (document.activeElement !== input) return false;
      btn.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }));
      return true;
    });
    if (!refreshPrimaryClicked) fail('Refresh primary: could not dispatch Refresh while the textarea held focus');
    await waitFor(
      page,
      () => document.querySelector('.code-review__confirm') !== null,
      'Refresh primary: live textarea did not show the inline confirm'
    );
    const refreshPrimaryBeforeClick = sentFrames.filter((p) => {
      try { return JSON.parse(p).type === 'code_review_refresh'; } catch { return false; }
    }).length;
    if (refreshPrimaryBeforeClick !== refreshBeforePrimary) {
      fail(`Refresh primary dispatched code_review_refresh before confirm (${refreshBeforePrimary} -> ${refreshPrimaryBeforeClick})`);
    }
    await page.click('.code-review__confirm-actions button:nth-child(1)'); // Refresh now
    await waitFor(
      page,
      () => document.querySelector('.code-review__confirm') === null,
      'Refresh primary: confirm did not dismiss after Refresh now'
    );
    const refreshPrimaryDeadline = Date.now() + 15000;
    let refreshPrimaryCount = refreshPrimaryBeforeClick;
    while (Date.now() < refreshPrimaryDeadline) {
      refreshPrimaryCount = sentFrames.filter((p) => {
        try { return JSON.parse(p).type === 'code_review_refresh'; } catch { return false; }
      }).length;
      if (refreshPrimaryCount >= refreshBeforePrimary + 1) break;
      await page.waitForTimeout(50);
    }
    if (refreshPrimaryCount !== refreshBeforePrimary + 1) {
      fail(`Refresh primary must dispatch exactly one code_review_refresh after confirm (before=${refreshBeforePrimary} after=${refreshPrimaryCount})`);
    }
    await waitFor(
      page,
      () => document.querySelector('.code-review__busy') === null,
      'Refresh primary: refresh busy state did not clear'
    );
    await waitFor(
      page,
      (want) => document.querySelector('.code-review__comment-input')?.value === want,
      'Refresh primary: live draft did not survive the confirmed refresh',
      15000,
      liveRefreshDraft
    );

    // Primary Close: confirm keeps the panel until the primary click, then
    // closes so later lane steps reopen a fresh panel with no leftover draft.
    // Immediate Close cannot pass on DOM unmount alone: capture the owning
    // session and the close-frame baseline before the synthetic focused
    // × click, assert zero new close before "Close panel", then exactly one
    // additional close stamped with the owner, plus ≥3.6s with no further
    // snapshot frames and no duplicate close.
    const closePrimaryDraft = '  live-close-primary  \n';
    await page.fill('.code-review__comment-input', closePrimaryDraft);
    const closePrimaryFocus = await page.evaluate(
      () => document.activeElement?.classList.contains('code-review__comment-input') === true
    );
    if (!closePrimaryFocus) fail('Close primary: composer lost focus before the × click');
    const closePrimaryClicked = await page.evaluate(() => {
      const input = document.querySelector('.code-review__comment-input');
      const btn = document.querySelector('button.code-review__close');
      if (!input || !btn) return false;
      if (document.activeElement !== input) return false;
      btn.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }));
      return true;
    });
    if (!closePrimaryClicked) fail('Close primary: could not dispatch × click while the textarea held focus');
    await waitFor(
      page,
      () => document.querySelector('.code-review__confirm') !== null,
      'Close primary: live textarea did not show the inline confirm'
    );
    const closePrimaryHeld = await page.evaluate(() => ({
      panel: document.getElementById('code-review-panel') !== null,
      confirm: document.querySelector('.code-review__confirm') !== null,
      value: document.querySelector('.code-review__comment-input')?.value ?? null,
    }));
    if (!closePrimaryHeld.panel || !closePrimaryHeld.confirm) {
      fail('Close primary closed the panel before the primary click');
    }
    if (closePrimaryHeld.value !== closePrimaryDraft) {
      fail(`Close primary lost exact bytes before confirm (${JSON.stringify(closePrimaryHeld.value)})`);
    }
    const closeFramesBeforePrimary = codeReviewCloseFrames(sentFrames);
    if (closeFramesBeforePrimary.length !== closeFramesBeforeLive.length) {
      fail(`Close primary dispatched code_review_close before Close panel (${closeFramesBeforeLive.length} -> ${closeFramesBeforePrimary.length})`);
    }
    await page.click('.code-review__confirm-actions button:nth-child(1)'); // Close panel
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') === null,
      'Close primary: panel did not close after Close panel'
    );
    const closePrimaryDeadline = Date.now() + 5000;
    let closeFramesAfterPrimary = closeFramesBeforePrimary;
    while (Date.now() < closePrimaryDeadline) {
      closeFramesAfterPrimary = codeReviewCloseFrames(sentFrames);
      if (closeFramesAfterPrimary.length >= closeFramesBeforeLive.length + 1) break;
      await page.waitForTimeout(50);
    }
    if (closeFramesAfterPrimary.length !== closeFramesBeforeLive.length + 1) {
      fail(`Close primary must dispatch exactly one code_review_close after Close panel (before=${closeFramesBeforeLive.length} after=${closeFramesAfterPrimary.length})`);
    }
    const newClose = closeFramesAfterPrimary[closeFramesAfterPrimary.length - 1];
    if (!newClose || newClose.sessionId !== closeOwnerSid) {
      fail(`Close primary close was not stamped with the owning session (got=${JSON.stringify(newClose?.sessionId)} want=${JSON.stringify(closeOwnerSid)})`);
    }
    const snapshotsAtClose = snapshotFrames.length;
    await page.waitForTimeout(3600);
    if (snapshotFrames.length !== snapshotsAtClose) {
      fail(`Close primary emitted snapshot frames over the ≥3.6s post-close window (before=${snapshotsAtClose} after=${snapshotFrames.length})`);
    }
    const closeFramesAfterWindow = codeReviewCloseFrames(sentFrames);
    if (closeFramesAfterWindow.length !== closeFramesBeforeLive.length + 1) {
      fail(`Close primary duplicated code_review_close over the ≥3.6s observation window (before=${closeFramesBeforeLive.length} after=${closeFramesAfterWindow.length})`);
    }
    await laneShot(page, 'code-review-closed.png', { fullPage: true });
    await runFileExplorerAcceptance(page, sentFrames, receivedFrames);
    await page.setViewportSize({ width: 1360, height: 900 });
    if (await page.locator('#rail-reopen-btn').isVisible()) await page.click('#rail-reopen-btn');
    await waitFor(page, () => document.getElementById('sidebar-manage-btn')?.getClientRects().length > 0, 'desktop sidebar did not restore after Files mobile journey');

    // ---- Session switch closes the owning review workspace ----
    // Create target session B while code review is closed, then switch back
    // to A. The actual A -> B cutover below clicks B's persistent sidebar row
    // while the review panel is still mounted, so only switch_session can
    // trigger the stamped code_review_close cleanup.
    const { beforeSessionId: sidA, sessionId: sidB } = await createFreshSessionViaManage(
      page,
      waitFor,
      'code-review target setup',
      { closePanel: true, timeoutMs: 20000 },
    );
    const switchedBack = await page.evaluate((targetId) => {
      const row = Array.from(document.querySelectorAll('.session-sidebar__switch')).find(
        (button) => button.dataset.sessionId === targetId,
      );
      if (!row) return false;
      row.click();
      return true;
    }, sidA);
    if (!switchedBack) fail('owning session row missing before the code-review switch gate');
    await waitFor(
      page,
      (targetId) => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId === targetId,
      'sidebar did not switch back to the owning session before code review opened',
      20000,
      sidA,
    );

    // Reopen the panel in A with TWO revisions so revision args are captured.
    // The later bare open in B must not reuse them.
    await openPicker(page);
    await chooseCommand(page, 'code-review');
    await waitFor(
      page,
      () => (document.getElementById('prompt-input')?.value.trim() || '').startsWith('/code-review'),
      'reopen draft missing before the session-switch gate'
    );
    await page.fill('#prompt-input', '/code-review HEAD~1 HEAD');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') !== null,
      'two-revision code-review panel did not open'
    );
    await waitFor(
      page,
      () => {
        const label = document.querySelector('.code-review__label')?.textContent || '';
        return label.includes('HEAD~1') && label.includes('HEAD');
      },
      'two-revision comparison label missing',
      20000
    );
    const labelRev = await page.evaluate(() => document.querySelector('.code-review__label')?.textContent || '');
    if (labelRev.includes('working tree')) {
      fail(`two-revision open showed the working-tree label instead (label="${labelRev}")`);
    }

    const switchMark = sentFrames.length;
    const switchClickedWhileMounted = await page.evaluate((targetId) => {
      const panel = document.getElementById('code-review-panel');
      const row = Array.from(document.querySelectorAll('.session-sidebar__switch')).find(
        (button) => button.dataset.sessionId === targetId,
      );
      if (!panel || !row) return false;
      row.click();
      return true;
    }, sidB);
    if (!switchClickedWhileMounted) {
      fail('target sidebar row was not clicked while the code-review panel remained mounted');
    }
    await waitFor(
      page,
      (targetId) =>
        document.getElementById('code-review-panel') === null &&
        document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId === targetId,
      'code-review panel did not close on the sidebar switch_session cutover',
      20000,
      sidB,
    );

    const closeFrames = sentFrames.slice(switchMark).map((payload) => JSON.parse(payload))
      .filter((frame) => frame.type === 'code_review_close');
    if (closeFrames.length !== 1 || closeFrames[0].sessionId !== sidA) {
      fail('loaded-session view switch must close exactly the source-owned review workspace');
    }
    // Target session B: a bare /code-review must NOT reuse A's revision args.
    await openPicker(page);
    await chooseCommand(page, 'code-review');
    await waitFor(
      page,
      () => (document.getElementById('prompt-input')?.value.trim() || '').startsWith('/code-review'),
      'choosing /code-review in the target session did not insert the draft'
    );
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') !== null,
      'code-review panel did not open in the target session'
    );
    await waitFor(
      page,
      () => {
        const label = document.querySelector('.code-review__label')?.textContent || '';
        return label.includes('working tree');
      },
      'bare /code-review in the target session did not open the working-tree scope',
      20000
    );
    const labelB = await page.evaluate(() => document.querySelector('.code-review__label')?.textContent || '');
    if (labelB.includes('HEAD~1')) {
      fail(`target session reused stale revision args from session A (label="${labelB}")`);
    }
    // Close the target-session panel so the remaining flows start clean.
    await page.click('.code-review__title');
    await page.keyboard.press('Escape');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') === null,
      'Escape did not close the target-session panel'
    );


    // ---- Review ownership across a cross-session switch (A -> B -> A) ----
    // A review belongs to the session that opened it. Switching away closes
    // that workspace (the panel's unmount close is stamped with the owner), the
    // restored view slot re-opens the panel under the session that is
    // DISPLAYED — never under the session the user left — and the revisions
    // captured for the previous session are dropped, so the restored panel
    // opens the new session's working tree instead of reusing the old
    // session's revisions. Every live code_review_* command the displayed
    // panel issues must therefore name the displayed session.
    // A reconnect leaves the sidebar's catalog refresh busy, and every switch
    // button is `disabled={busy}` then: a click during that window is swallowed
    // (the session never switches). Wait for the row to accept the click first,
    // like the suite's other switch helpers do.
    const clickSessionRow = async (targetId, label) => {
      await waitFor(
        page,
        (want) => {
          const row = Array.from(document.querySelectorAll('.session-sidebar__switch')).find(
            (button) => button.dataset.sessionId === want,
          );
          return row !== undefined && row.disabled !== true;
        },
        `${label}: sidebar row ${targetId} never became clickable`,
        20000,
        targetId,
      );
      const clicked = await page.evaluate((want) => {
        const row = Array.from(document.querySelectorAll('.session-sidebar__switch')).find(
          (button) => button.dataset.sessionId === want,
        );
        if (!row) return false;
        row.click();
        return true;
      }, targetId);
      if (!clicked) fail(`${label}: sidebar row ${targetId} missing`);
    };
    const switchToSessionRow = async (targetId, label) => {
      await clickSessionRow(targetId, label);
      await waitFor(
        page,
        (want) => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId === want,
        `${label}: sidebar never switched to ${targetId}`,
        20000,
        targetId,
      );
    };

    // Bare /commit on A, then switch to B before A's code_review_commits
    // response is released. The late list must not paint A's row or open a
    // review on B. A current row click while A is displayed must.
    await switchToSessionRow(sidA, 'commit picker: return to the owning session');
    await page.click('button.code-review__close');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') === null,
      'commit picker: restored review did not close before returning to Chat',
    );
    await page.evaluate(() => window.__rpiWarmReconnect.holdNextCommitList());
    const lateCommitMark = sentFrames.length;
    await page.fill('#prompt-input', '/commit');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.querySelector('[aria-label="Recent commits"]') !== null,
      'commit picker: bare /commit did not open the recent-commit chooser on A',
      20000,
    );
    const lateCommitRequest = sentFrames.slice(lateCommitMark).map(parseSentFrame)
      .find((frame) => frame?.type === 'code_review_commits' && frame.sessionId === sidA);
    if (!lateCommitRequest) fail(`commit picker: bare /commit did not request code_review_commits for ${sidA}`);
    await waitFor(
      page,
      (id) => window.__rpiWarmReconnect.state.held.includes(`code_review_commits:${id}`),
      `commit picker: response for A's code_review_commits request ${lateCommitRequest.id} was not intercepted`,
      20000,
      lateCommitRequest.id,
    );
    await switchToSessionRow(sidB, 'commit picker: switch away before the commit list returns');
    const releasedCommitList = await page.evaluate(() => window.__rpiWarmReconnect.releaseHeld(['commitList']));
    if (releasedCommitList !== 1) fail(`commit picker: expected exactly one held commit list, released ${releasedCommitList}`);
    await page.waitForTimeout(500);
    const lateCommitDom = await page.evaluate((hash) => ({
      picker: document.querySelector('[aria-label="Recent commits"]') !== null,
      row: document.querySelector(`[role="option"][data-commit="${hash}"]`) !== null,
      panel: document.getElementById('code-review-panel') !== null,
      displayed: document.getElementById('session-lifecycle')?.dataset.displayedSessionId || '',
      command: document.getElementById('session-lifecycle')?.dataset.commandSessionId || '',
    }), solCommit);
    if (lateCommitDom.picker || lateCommitDom.row || lateCommitDom.panel
      || lateCommitDom.displayed !== sidB || lateCommitDom.command !== sidB) {
      fail(`commit picker: late A commit list painted or opened on B (${JSON.stringify(lateCommitDom)})`);
    }
    const lateOpens = sentFrames.slice(lateCommitMark).map(parseSentFrame)
      .filter((frame) => frame?.type === 'code_review_open');
    if (lateOpens.length !== 0) {
      fail(`commit picker: late A commit list dispatched code_review_open (${JSON.stringify(lateOpens)})`);
    }
    await switchToSessionRow(sidA, 'commit picker: return for the current-session row');
    const currentCommitMark = sentFrames.length;
    ownershipTrace.startSentIndex = currentCommitMark;
    await page.fill('#prompt-input', '/commit');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (hash) => document.querySelector(`[role="option"][data-commit="${hash}"]`) !== null,
      'commit picker: the current session did not paint its own commit row',
      20000,
      solCommit,
    );
    await page.locator(`[role="option"][data-commit="${solCommit}"]`).click();
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') !== null,
      'commit picker: the current session row did not open the review panel',
      20000,
    );
    const currentOpens = sentFrames.slice(currentCommitMark).map(parseSentFrame)
      .filter((frame) => frame?.type === 'code_review_open');
    if (currentOpens.length !== 1 || currentOpens[0].sessionId !== sidA || currentOpens[0].commit !== solCommit) {
      fail(`commit picker: current row did not open exactly ${solCommit} on ${sidA} (${JSON.stringify(currentOpens)})`);
    }
    // Leave A's commit review open while switching away so returning to A
    // exercises a real view-slot restore rather than clicking the active row.
    await switchToSessionRow(sidB, 'review ownership: leave the owning session before restore');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') === null,
      'review ownership: sibling session retained the owning review panel',
    );
    const restoreMark = sentFrames.length;
    await switchToSessionRow(sidA, 'review ownership: return to the owning session');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') !== null,
      'review ownership: the restored view slot did not re-open the review panel',
      20000,
    );
    await page.waitForTimeout(800);
    // The DISPLAY authority is the contract surface (the sidebar row follows the
    // active session state, which a reconnect can diverge from): wait until both
    // the displayed id and its command target name the owning session.
    await waitFor(
      page,
      (want) => {
        const data = document.getElementById('session-lifecycle')?.dataset || {};
        return data.displayedSessionId === want && data.commandSessionId === want;
      },
      'review ownership: the display authority never settled on the owning session',
      20000,
      sidA,
    );
    // The restored panel boots with a bare open and then polls its OWN session
    // (CODE_REVIEW_SNAPSHOT_POLL_MS cadence): wait for that poll so the whole
    // phase window — open, poll and any wrong-owner frame — is on the wire.
    const restoredSnapshotDeadline = Date.now() + 6000;
    let restoredOwnedSnapshots = [];
    while (Date.now() < restoredSnapshotDeadline) {
      restoredOwnedSnapshots = sentFrames.slice(restoreMark).map(parseSentFrame)
        .filter((frame) => frame?.type === 'code_review_snapshot' && frame.sessionId === sidA);
      if (restoredOwnedSnapshots.length > 0) break;
      await page.waitForTimeout(200);
    }
    const restoredFrames = sentFrames.slice(restoreMark).map(parseSentFrame)
      .filter((frame) => typeof frame?.type === 'string' && frame.type.startsWith('code_review_'));
    // The restored panel must OPEN the owning session's own scope: a bare
    // code_review_open (no from/to) that SUCCEEDS, plus its working-tree
    // snapshot — a lone close or an unrelated frame is not proof.
    const restoredOwnedOpen = restoredFrames.find((frame) => frame.type === 'code_review_open'
      && frame.sessionId === sidA
      && frame.from === undefined
      && frame.to === undefined);
    if (!restoredOwnedOpen) {
      fail(`review ownership: the restored panel never dispatched a bare owner-owned code_review_open (${JSON.stringify(restoredFrames.map((frame) => `${frame.type}@${frame.sessionId}${frame.from ? `(from=${frame.from})` : ''}`))})`);
    }
    const restoredOpenResponse = receivedFrames
      .map((raw) => { try { return JSON.parse(raw); } catch { return null; } })
      .find((frame) => frame?.type === 'response' && frame.id === restoredOwnedOpen.id) || null;
    if (!restoredOpenResponse || restoredOpenResponse.success !== true) {
      fail(`review ownership: the restored panel's owner-owned open did not succeed (${JSON.stringify(restoredOpenResponse)})`);
    }
    const restoredOpenLabel = restoredOpenResponse.data?.comparisonLabel || '';
    if (!restoredOpenLabel.includes('working tree')) {
      fail(`review ownership: the restored panel's own open did not resolve the working-tree scope (label="${restoredOpenLabel}")`);
    }
    if (restoredOwnedSnapshots.length === 0) {
      fail(`review ownership: the restored panel never probed its own session (${JSON.stringify(restoredFrames.map((frame) => `${frame.type}@${frame.sessionId}`))})`);
    }
    const restoredForeign = restoredFrames.filter((frame) => frame.type !== 'code_review_close' && frame.sessionId !== sidA);
    if (restoredForeign.length !== 0) {
      fail(`review ownership: the restored panel addressed ${JSON.stringify([...new Set(restoredForeign.map((frame) => frame.sessionId))])} while ${sidA} is displayed`);
    }
    const restoredRevisions = restoredFrames.filter((frame) => frame.from === 'HEAD~1' || frame.to === 'HEAD');
    if (restoredRevisions.length !== 0) {
      fail(`review ownership: the restored panel reused the previous session's revisions (${JSON.stringify(restoredRevisions)})`);
    }
    await laneShot(page, 'review-ownership-restored-a.png', { fullPage: true });

    // Open a review in B, then switch back to A: the restored A panel must not
    // issue a single B-owned command and must not carry B's revisions.
    await switchToSessionRow(sidB, 'review ownership: switch to the sibling session');
    const siblingMark = sentFrames.length;
    await openPicker(page);
    await chooseCommand(page, 'code-review');
    await waitFor(
      page,
      () => (document.getElementById('prompt-input')?.value.trim() || '').startsWith('/code-review'),
      'review ownership: choosing /code-review in the sibling session did not insert the draft',
    );
    await page.fill('#prompt-input', '/code-review HEAD~1 HEAD');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') !== null,
      'review ownership: the sibling session review panel did not open',
      30000,
    );
    await waitFor(
      page,
      () => {
        const label = document.querySelector('.code-review__label')?.textContent || '';
        return label.includes('HEAD~1') && label.includes('HEAD');
      },
      'review ownership: the sibling session review did not render its revisions',
      20000,
    );
    // Settle window so a wrong-owner reopen would be on the wire before the
    // exactly-one assertion below reads the captured frames.
    await page.waitForTimeout(800);
    const siblingOpens = sentFrames.slice(siblingMark).map(parseSentFrame)
      .filter((frame) => frame?.type === 'code_review_open');
    if (siblingOpens.length !== 1 || siblingOpens[0].sessionId !== sidB) {
      fail(`review ownership: the sibling session's /code-review must dispatch exactly one code_review_open stamped with ${sidB} (${JSON.stringify(siblingOpens)})`);
    }
    const returnMark = sentFrames.length;
    await switchToSessionRow(sidA, 'review ownership: return to the owning session again');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') !== null,
      'review ownership: the second return did not re-open the review panel',
      20000,
    );
    await page.waitForTimeout(800);
    const returnedFrames = sentFrames.slice(returnMark).map(parseSentFrame)
      .filter((frame) => typeof frame?.type === 'string' && frame.type.startsWith('code_review_'));
    // Same contract on the second return: the DISPLAY authority must settle on
    // the owning session, and the restored panel must open THAT session's own
    // bare scope successfully — never a lone frame of any kind.
    await waitFor(
      page,
      (want) => {
        const data = document.getElementById('session-lifecycle')?.dataset || {};
        return data.displayedSessionId === want && data.commandSessionId === want;
      },
      'review ownership: the display authority never settled on the owning session again',
      20000,
      sidA,
    );
    const returnedSnapshotDeadline = Date.now() + 6000;
    let returnedOwnedSnapshots = [];
    while (Date.now() < returnedSnapshotDeadline) {
      returnedOwnedSnapshots = sentFrames.slice(returnMark).map(parseSentFrame)
        .filter((frame) => frame?.type === 'code_review_snapshot' && frame.sessionId === sidA);
      if (returnedOwnedSnapshots.length > 0) break;
      await page.waitForTimeout(200);
    }
    const returnedOwnedOpen = returnedFrames.find((frame) => frame.type === 'code_review_open'
      && frame.sessionId === sidA
      && frame.from === undefined
      && frame.to === undefined);
    if (!returnedOwnedOpen) {
      fail(`review ownership: the re-restored panel never dispatched a bare owner-owned code_review_open (${JSON.stringify(returnedFrames.map((frame) => `${frame.type}@${frame.sessionId}${frame.from ? `(from=${frame.from})` : ''}`))})`);
    }
    const returnedOpenResponse = receivedFrames
      .map((raw) => { try { return JSON.parse(raw); } catch { return null; } })
      .find((frame) => frame?.type === 'response' && frame.id === returnedOwnedOpen.id) || null;
    if (!returnedOpenResponse || returnedOpenResponse.success !== true) {
      fail(`review ownership: the re-restored panel's owner-owned open did not succeed (${JSON.stringify(returnedOpenResponse)})`);
    }
    const returnedOpenLabel = returnedOpenResponse.data?.comparisonLabel || '';
    if (!returnedOpenLabel.includes('working tree')) {
      fail(`review ownership: the re-restored panel's own open did not resolve the working-tree scope (label="${returnedOpenLabel}")`);
    }
    if (returnedOwnedSnapshots.length === 0) {
      fail(`review ownership: the re-restored panel never probed its own session (${JSON.stringify(returnedFrames.map((frame) => `${frame.type}@${frame.sessionId}`))})`);
    }
    const returnedForeign = returnedFrames.filter((frame) => frame.type !== 'code_review_close' && frame.sessionId !== sidA);
    if (returnedForeign.length !== 0) {
      fail(`review ownership: the displayed panel issued ${JSON.stringify([...new Set(returnedForeign.map((frame) => frame.sessionId))])}-owned commands (${returnedForeign.map((frame) => frame.type).join(', ')}) while ${sidA} is displayed`);
    }
    const returnedRevisions = returnedFrames.filter((frame) => frame.from === 'HEAD~1' || frame.to === 'HEAD');
    if (returnedRevisions.length !== 0) {
      fail(`review ownership: the restored panel in ${sidA} carried the sibling session's revisions (${JSON.stringify(returnedRevisions)})`);
    }
    await waitFor(
      page,
      () => (document.querySelector('.code-review__label')?.textContent || '').includes('working tree'),
      'review ownership: the restored panel did not open the displayed session\'s working-tree scope',
      20000,
    );
    const ownershipLabel = await page.evaluate(() => document.querySelector('.code-review__label')?.textContent || '');
    if (ownershipLabel.includes('HEAD~1')) {
      fail(`review ownership: the restored panel in ${sidA} reused the sibling session's revisions (label="${ownershipLabel}")`);
    }
    await laneShot(page, 'review-ownership-displayed-session.png', { fullPage: true });
    // Leave the run on the sibling session (a non-primary displayed session,
    // the warm-reconnect phase's precondition) with the review closed.
    await page.click('.code-review__title');
    await page.keyboard.press('Escape');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') === null,
      'review ownership: Escape did not close the restored panel',
    );
    await switchToSessionRow(sidB, 'review ownership: leave the sibling session displayed');
    // The sibling slot remembers its OWN review panel: close whatever it
    // re-opened so the remaining flows start from a closed panel.
    if (await page.evaluate(() => document.getElementById('code-review-panel') !== null)) {
      await page.click('.code-review__title');
      await page.keyboard.press('Escape');
      await waitFor(
        page,
        () => document.getElementById('code-review-panel') === null,
        'review ownership: Escape did not close the sibling review panel',
      );
    }

    // ---- Mobile (≤900px): tabbed single-pane workspace ----
    // Files -> selecting a file opens Diff -> selecting a hunk opens Thread;
    // the composer is reachable without scrolling; Back-to-diff returns; the
    // close button (required E2E selector) removes the panel.
    const mobileContext = await browser.newContext({ viewport: { width: 390, height: 844 } });
    const mobilePage = await mobileContext.newPage();
    diagMobilePage = mobilePage;
    // The MOBILE page owns its browser-error evidence: its own pageerror +
    // console.error handlers, its own arrays, consumed by the lane-final
    // union gate — an empty desktop-only array can never stand in for mobile.
    const mobilePageErrors = [];
    const mobileConsoleErrors = [];
    mobilePage.on('pageerror', (err) => {
      const message = String(err && err.message ? err.message : err);
      if (isKnownConsoleNoise(message)) return;
      mobilePageErrors.push(message);
      console.error(`web-commands-review: mobile page error: ${message}`);
    });
    mobilePage.on('console', (message) => {
      if (message.type() === 'error' && !isKnownConsoleNoise(message.text())) mobileConsoleErrors.push(message.text());
    });
    if (token) {
      await mobilePage.addInitScript(
        (t) => { window.localStorage.setItem('rpi-web-token', t); },
        token
      );
    }
    await mobilePage.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(mobilePage, () => document.title === 'rpi web', 'mobile page title missing');
    await waitFor(
      mobilePage,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'mobile WS did not reach "connected"'
    );
    if (INJECT_FAIL) {
      // Deterministic self-test: a REAL uncaught page error and a REAL
      // console.error on the MOBILE page, observed through the mobile
      // handlers above; the lane-final union browser-error gate (after every
      // page/context closed) must exit nonzero.
      await mobilePage.evaluate(() => { setTimeout(() => { throw new Error('self-test injected mobile pageerror'); }, 0); });
      await mobilePage.evaluate(() => console.error('self-test injected mobile console.error'));
      await mobilePage.waitForTimeout(250);
    }
    await mobilePage.fill('#prompt-input', '');
    await mobilePage.locator('#prompt-input').focus();
    await mobilePage.locator('#prompt-input').press('x');
    await mobilePage.locator('#prompt-input').press('Backspace');
    await mobilePage.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    const crLayoutBeforeMobile = await crAppLayout(mobilePage);
    await openPicker(mobilePage);
    await chooseCommand(mobilePage, 'code-review');
    await waitFor(
      mobilePage,
      () => (document.getElementById('prompt-input')?.value.trim() || '').startsWith('/code-review'),
      'choosing /code-review on mobile did not insert the draft'
    );
    await mobilePage.press('#prompt-input', 'Enter');
    await waitFor(
      mobilePage,
      () => document.getElementById('code-review-panel') !== null,
      'mobile code-review panel did not open'
    );
    // Mobile ≤900px: the thread resizer is CSS-hidden (no desktop column drag).
    const mobileResizerHidden = await mobilePage.evaluate(() => {
      const r = document.querySelector('.code-review__thread-resizer');
      if (!r) return true; // absent is fine on mobile
      return getComputedStyle(r).display === 'none';
    });
    if (!mobileResizerHidden) fail('mobile must hide .code-review__thread-resizer');
    // Files tab is active on open; the Diff pane is hidden.
    const mobileTabs = await mobilePage.evaluate(() =>
      Array.from(document.querySelectorAll('.code-review__tab')).map((t) => t.textContent.trim())
    );
    if (mobileTabs.join(',') !== 'Files,Diff,Thread') {
      fail(`mobile tab bar missing Files/Diff/Thread (got ${mobileTabs.join(',')})`);
    }
    const mobileActiveTab = await mobilePage.evaluate(
      () => document.querySelector('.code-review__tab.is-active')?.textContent.trim() || ''
    );
    if (mobileActiveTab !== 'Files') {
      fail(`mobile opens on the ${mobileActiveTab} tab instead of Files`);
    }
    const diffVisibleOnFiles = await mobilePage.evaluate(() => {
      const el = document.querySelector('.code-review__diff');
      if (!el) return false;
      const rect = el.getBoundingClientRect();
      return rect.width > 0 && rect.height > 0;
    });
    if (diffVisibleOnFiles) fail('diff pane is visible while the Files tab is active');
    // Mobile no-overflow contract for the file rail: the full-viewport rail
    // must not overflow the viewport or itself, and the dirty filename
    // renders in full (the stats column never swallows the name).
    const mobileOverflow = await mobilePage.evaluate(() => {
      const rail = document.querySelector('.code-review__files');
      if (!rail) return null;
      return {
        pageOverflow: document.documentElement.scrollWidth > window.innerWidth + 1,
        railOverflow: rail.scrollWidth > rail.clientWidth + 1,
      };
    });
    if (!mobileOverflow) fail('mobile file rail missing while the Files tab is active');
    if (mobileOverflow.pageOverflow || mobileOverflow.railOverflow) {
      fail(`mobile file rail overflows (page=${mobileOverflow.pageOverflow}, rail=${mobileOverflow.railOverflow})`);
    }
    // Selecting a file switches to the Diff tab and renders its diff.
    await waitFor(
      mobilePage,
      (want) => Array.from(document.querySelectorAll('.code-review__file')).some((b) =>
        b.getAttribute('data-file-path') === want
      ),
      `mobile file list never rendered ${dirtyFile}`,
      20000,
      dirtyFile
    );
    const mobileName = await mobilePage.evaluate((want) => {
      const btn = Array.from(document.querySelectorAll('button.code-review__file')).find((b) =>
        b.getAttribute('data-file-path') === want
      );
      const span = btn && btn.querySelector('.code-review__file-path');
      if (!span) return null;
      return {
        text: (span.textContent || '').trim(),
        fits: span.scrollWidth <= span.clientWidth + 1,
      };
    }, dirtyFile);
    if (!mobileName) fail(`mobile file row for ${dirtyFile} missing`);
    if (mobileName.text !== dirtyFile) {
      fail(`mobile file row must display the full filename ${dirtyFile} (got "${mobileName.text}")`);
    }

    if (!mobileName.fits) {
      fail(`mobile filename ${dirtyFile} is truncated by the stats column`);
    }
    // ---- Solidity lexing in the REVIEW diff (mobile) ----
    // Selecting the dirty .sol row from the Files rail switches to the Diff
    // tab with its changed diff lexed through the shared Solidity grammar —
    // the mobile counterpart of the desktop review assertion. The next block
    // re-selects the dirty file, restoring the original review selection.
    await mobilePage.evaluate((want) => {
      const btn = Array.from(document.querySelectorAll('.code-review__file')).find((b) =>
        b.getAttribute('data-file-path') === want
      );
      if (btn) btn.click();
    }, solReviewFile);
    await waitFor(
      mobilePage,
      () => document.querySelector('.code-review__tab.is-active')?.textContent.trim() === 'Diff'
        && document.querySelector('.code-review__line--deletion') !== null
        && document.querySelector('.code-review__line--addition') !== null
        && Array.from(document.querySelectorAll('.code-review__line-text'))
          .some((el) => (el.textContent || '').includes('TokenStore')),
      `mobile ${solReviewFile} review diff never rendered its changed Solidity lines`
    );
    assertSolidityReviewDiff(await reviewSolidityProof(mobilePage), 'mobile solidity review');
    await laneShot(mobilePage, 'code-review-sol-diff-mobile.png');
    // ---- Outer shell full-viewport geometry (mobile 390x844) ----
    // Same full-viewport contract as desktop: the shared drawer base's
    // max-width/height caps must never inset or shrink the shell here.
    await assertCrMobileShellGeometry(mobilePage, 'mobile');
    await laneShot(mobilePage, 'code-review-panel-geometry-mobile.png');
    await mobilePage.evaluate((want) => {
      const btn = Array.from(document.querySelectorAll('.code-review__file')).find((b) =>
        b.getAttribute('data-file-path') === want
      );
      if (btn) btn.click();
    }, dirtyFile);
    await waitFor(
      mobilePage,
      () => document.querySelector('.code-review__tab.is-active')?.textContent.trim() === 'Diff',
      'selecting a file did not switch to the Diff tab'
    );
    await waitFor(
      mobilePage,
      () => document.querySelector('.code-review__line--deletion') !== null,
      'mobile diff did not render the dirty file deletion line'
    );
    // Selecting a hunk switches to the Thread tab; the composer is visible in
    // the viewport without scrolling the pane.
    await mobilePage.click('.code-review__hunk-header');
    await waitFor(
      mobilePage,
      () => document.querySelector('.code-review__tab.is-active')?.textContent.trim() === 'Thread',
      'selecting a hunk did not switch to the Thread tab'
    );
    const composerVisible = await mobilePage.evaluate(() => {
      const el = document.querySelector('.code-review__comment-input');
      if (!el) return false;
      const rect = el.getBoundingClientRect();
      return rect.width > 0 && rect.height > 0 && rect.top >= 0 && rect.bottom <= window.innerHeight;
    });
    if (!composerVisible) fail('mobile thread composer is not visible without scrolling');
    // Back to diff returns to the Diff tab.
    await mobilePage.click('.code-review__back');
    await waitFor(
      mobilePage,
      () => document.querySelector('.code-review__tab.is-active')?.textContent.trim() === 'Diff',
      'Back to diff did not return to the Diff tab'
    );
    // The close button removes the panel (required E2E selector).
    await mobilePage.click('button.code-review__close');
    await waitFor(
      mobilePage,
      () => document.getElementById('code-review-panel') === null,
      'mobile close button did not remove the panel'
    );
    // Close-restore on mobile: the prior app layout (scroll, composer rect,
    // focus) must survive the panel unmount untouched.
    const crLayoutAfterMobile = await crAppLayout(mobilePage);
    assertCrLayoutRestored(crLayoutBeforeMobile, crLayoutAfterMobile, 'mobile close-restore');
    await laneShot(mobilePage, 'code-review-panel-close-restore-mobile.png');
    // Close the MOBILE page/context while its listeners remain effective, so
    // close-time pageerror/console.error events still land in the mobile
    // arrays; the lane-final union gate consumes them after EVERYTHING has
    // closed (no mid-run gate — all actions execute first).
    await mobileContext.close();
    await laneShot(page, 'code-review-mobile-tabs.png', { fullPage: true });

    // /skill <fixture> renders the loaded skill's frontmatter summary. The
    // picker surfaces the REAL on-disk fixture skill as a candidate: selecting
    // the `/skill` parent drills the picker into skills mode, the greet
    // candidate (loaded from `.rpi/skills/greet/SKILL.md` via get_commands)
    // appears with its name + description, and selecting it inserts
    // `/skill greet` WITHOUT auto-submitting. Enter then dispatches the typed
    // `skill` RPC and the frontmatter summary bubble renders.
    await openPicker(page);
    await chooseCommand(page, 'skill');
    // Selecting /skill switches the picker into skills mode (no composer
    // draft yet) — assert the drill-in surface before picking a candidate.
    await waitFor(
      page,
      () => document.querySelector('.command-picker__title')?.textContent?.trim() === 'Skills',
      'selecting /skill did not drill the picker into skills mode'
    );
    // The fixture skill candidate is rendered with its bare name + frontmatter
    // description (discoverability — the user can see what greet does before
    // selecting it).
    await waitFor(
      page,
      (want) => {
        const opt = Array.from(document.querySelectorAll('.command-picker__option[data-skill-name]')).find(
          (li) => li.getAttribute('data-skill-name') === want
        );
        if (!opt) return false;
        const name = opt.querySelector('.command-picker__name')?.textContent?.trim() || '';
        const desc = opt.querySelector('.command-picker__desc')?.textContent?.trim() || '';
        return name === want && desc.length > 0;
      },
      `skill candidate ${skillName} did not render with name + description`,
      25000,
      skillName
    );
    await laneShot(page, 'skill-candidates.png', { fullPage: true });
    await chooseSkillCandidate(page, skillName);
    // Selecting the candidate inserts `/skill <name>` (no auto-submit).
    await waitFor(
      page,
      (want) => (document.getElementById('prompt-input')?.value.trim() || '') === `/skill ${want}`,
      `selecting skill candidate ${skillName} did not insert /skill ${skillName} into the composer`,
      25000,
      skillName
    );
    await page.press('#prompt-input', 'Enter');
    // The /skill response renders as a dedicated transcript summary bubble
    // (div.msg.msg--summary, label "skill", text in .msg--summary__text).
    // Assert THAT bubble carries the frontmatter values — not just any page
    // text — so a dropped summary text fails the lane even if the label paints.
    await waitFor(
      page,
      (args) => {
        const bubbles = Array.from(document.querySelectorAll('.msg--summary'));
        return bubbles.some((b) => {
          const label = b.querySelector('.msg--summary__label')?.textContent?.trim() || '';
          const text = b.querySelector('.msg--summary__text')?.textContent || '';
          return label === 'skill' && text.includes(`name: ${args.skillName}`) && text.includes(args.desc);
        });
      },
      `/skill ${skillName} did not render the skill summary bubble (.msg--summary label "skill") with the frontmatter`,
      20000,
      { skillName, desc: skillDesc }
    );
    await laneShot(page, 'skill-summary.png', { fullPage: true });

    // /compact dispatches the compact RPC — observe the outgoing WS frame.
    // Provider success is NOT required; only that the composer dispatch
    // sends the compact command (deterministic, empty session is fine).
    await openPicker(page);
    await chooseCommand(page, 'compact');
    await waitFor(
      page,
      () => (document.getElementById('prompt-input')?.value.trim() || '') === '/compact',
      'choosing /compact did not insert /compact into the composer'
    );
    const compactBefore = sentFrames.length;
    await page.press('#prompt-input', 'Enter');
    let sawCompact = false;
    const deadline = Date.now() + 15000;
    while (Date.now() < deadline) {
      for (let i = compactBefore; i < sentFrames.length; i++) {
        let frame;
        try {
          frame = JSON.parse(sentFrames[i]);
        } catch {
          continue;
        }
        if (frame && frame.type === 'compact') { sawCompact = true; break; }
      }
      if (sawCompact) break;
      await page.waitForTimeout(200);
    }
    if (!sawCompact) {
      fail(`/compact did not dispatch a compact RPC on the WS (sent ${sentFrames.length - compactBefore} frames after submit; last=${sentFrames.slice(-3).join(' | ') || 'none'})`);
    }
    await laneShot(page, 'compact-dispatch.png', { fullPage: true });

    // ------------------------------------------------------------------
    // Slash-command variants (coverage journeys). The session here is still
    // tiny (no provider-turn accumulation), so EVERY compact variant hits
    // the same deterministic boundary: the RPC goes out with the right wire
    // shape and the backend answers truthfully with "Nothing to compact
    // (session too small)" — surfaced as a summary bubble + error toast.
    // No staged-prompt accumulation is involved (the archive's turn-count
    // precondition is not staged here), so the journeys stay fast and
    // deterministic.
    // ------------------------------------------------------------------

    // /compact --snap: dispatches the snapcompact RPC (the deterministic
    // offline archive wire) and surfaces the truthful error boundary.
    await page.fill('#prompt-input', '/compact --snap');
    const snapBefore = sentFrames.length;
    await page.press('#prompt-input', 'Enter');
    let snapFrame = null;
    const snapDeadline = Date.now() + 15000;
    while (Date.now() < snapDeadline) {
      for (let f = snapBefore; f < sentFrames.length; f++) {
        let frame;
        try {
          frame = JSON.parse(sentFrames[f]);
        } catch {
          continue;
        }
        if (frame && frame.type === 'snapcompact') {
          snapFrame = frame;
          break;
        }
      }
      if (snapFrame) break;
      await page.waitForTimeout(100);
    }
    if (!snapFrame) {
      fail(`/compact --snap did not dispatch a snapcompact RPC (last frames: ${sentFrames.slice(-3).join(' | ')})`);
    }
    await waitFor(
      page,
      () => {
        const bubbles = Array.from(document.querySelectorAll('.msg--summary'));
        const hasBubble = bubbles.some((b) => {
          const label = b.querySelector('.msg--summary__label')?.textContent?.trim() || '';
          const text = b.querySelector('.msg--summary__text')?.textContent || '';
          return label === 'snapcompact' && text.includes('Nothing to compact');
        });
        const hasToast = Array.from(document.querySelectorAll('.toast--error')).some((t) =>
          (t.textContent || '').includes('Snapcompact failed: Nothing to compact')
        );
        return hasBubble && hasToast;
      },
      '/compact --snap did not surface the truthful "session too small" error bubble + toast',
      20000
    );
    await laneShot(page, 'snapcompact-error.png', { fullPage: true });

    // /compact <instructions>: the LLM path carries the custom instructions
    // on the compact RPC and, with a sub-threshold session, surfaces the
    // truthful "Nothing to compact (session too small)" error bubble + toast.
    await page.fill('#prompt-input', '/compact keep the key decisions');
    const llmBefore = sentFrames.length;
    await page.press('#prompt-input', 'Enter');
    let llmFrame = null;
    const llmDeadline = Date.now() + 15000;
    while (Date.now() < llmDeadline) {
      for (let f = llmBefore; f < sentFrames.length; f++) {
        let frame;
        try {
          frame = JSON.parse(sentFrames[f]);
        } catch {
          continue;
        }
        if (frame && frame.type === 'compact') {
          llmFrame = frame;
          break;
        }
      }
      if (llmFrame) break;
      await page.waitForTimeout(100);
    }
    if (!llmFrame) {
      fail(`/compact <instructions> did not dispatch a compact RPC (last frames: ${sentFrames.slice(-3).join(' | ')})`);
    }
    if (llmFrame.customInstructions !== 'keep the key decisions') {
      fail(`/compact <instructions> RPC must carry customInstructions (got ${JSON.stringify(llmFrame.customInstructions)})`);
    }
    await waitFor(
      page,
      () => {
        const bubbles = Array.from(document.querySelectorAll('.msg--summary'));
        const hasBubble = bubbles.some((b) => {
          const label = b.querySelector('.msg--summary__label')?.textContent?.trim() || '';
          const text = b.querySelector('.msg--summary__text')?.textContent || '';
          return label === 'compact' && text.includes('Nothing to compact');
        });
        const hasToast = Array.from(document.querySelectorAll('.toast--error')).some((t) =>
          (t.textContent || '').includes('Compact failed: Nothing to compact')
        );
        return hasBubble && hasToast;
      },
      '/compact <instructions> did not surface the truthful "session too small" error bubble + toast',
      20000
    );
    await laneShot(page, 'compact-instructions-error.png', { fullPage: true });

    // /compact bare: the RPC carries NO customInstructions key and the same
    // deterministic error boundary surfaces (bubble + toast).
    await page.fill('#prompt-input', '/compact');
    const bareBefore = sentFrames.length;
    await page.press('#prompt-input', 'Enter');
    let bareFrame = null;
    const bareDeadline = Date.now() + 15000;
    while (Date.now() < bareDeadline) {
      for (let f = bareBefore; f < sentFrames.length; f++) {
        let frame;
        try {
          frame = JSON.parse(sentFrames[f]);
        } catch {
          continue;
        }
        if (frame && frame.type === 'compact') {
          bareFrame = frame;
          break;
        }
      }
      if (bareFrame) break;
      await page.waitForTimeout(100);
    }
    if (!bareFrame) {
      fail(`bare /compact did not dispatch a compact RPC (last frames: ${sentFrames.slice(-3).join(' | ')})`);
    }
    if ('customInstructions' in bareFrame) {
      fail(`bare /compact RPC must NOT carry customInstructions (got ${JSON.stringify(bareFrame.customInstructions)})`);
    }
    await waitFor(
      page,
      () => {
        const bubbles = Array.from(document.querySelectorAll('.msg--summary'));
        return bubbles.some((b) => {
          const label = b.querySelector('.msg--summary__label')?.textContent?.trim() || '';
          const text = b.querySelector('.msg--summary__text')?.textContent || '';
          return label === 'compact' && text.includes('Nothing to compact');
        });
      },
      'bare /compact did not surface the "session too small" error bubble',
      20000
    );
    await laneShot(page, 'compact-bare-error.png', { fullPage: true });

    // /skill greet extra words: only the FIRST token is the skill name (a
    // pasted description tail must never poison the RPC); the skill RPC goes
    // out with name=greet and the summary bubble renders the frontmatter.
    await page.fill('#prompt-input', `/skill ${skillName} extra words after the name`);
    const skillBefore = sentFrames.length;
    await page.press('#prompt-input', 'Enter');
    let skillFrame = null;
    const skillDeadline = Date.now() + 15000;
    while (Date.now() < skillDeadline) {
      for (let f = skillBefore; f < sentFrames.length; f++) {
        let frame;
        try {
          frame = JSON.parse(sentFrames[f]);
        } catch {
          continue;
        }
        if (frame && frame.type === 'skill') {
          skillFrame = frame;
          break;
        }
      }
      if (skillFrame) break;
      await page.waitForTimeout(100);
    }
    if (!skillFrame) {
      fail(`/skill ${skillName} extra words did not dispatch a skill RPC (last frames: ${sentFrames.slice(-3).join(' | ')})`);
    }
    if (skillFrame.name !== skillName) {
      fail(`/skill with a trailing tail must use only the first token as the skill name (got ${JSON.stringify(skillFrame.name)})`);
    }
    await waitFor(
      page,
      (args) => {
        const bubbles = Array.from(document.querySelectorAll('.msg--summary'));
        return bubbles.some((b) => {
          const label = b.querySelector('.msg--summary__label')?.textContent?.trim() || '';
          const text = b.querySelector('.msg--summary__text')?.textContent || '';
          return label === 'skill' && text.includes(`name: ${args.skillName}`);
        });
      },
      `/skill ${skillName} extra words did not render the skill summary bubble`,
      20000,
      { skillName }
    );
    await laneShot(page, 'skill-first-token.png', { fullPage: true });

    // /skill bare: a client-side usage error toast, NO skill RPC ever goes
    // out, and the draft is PRESERVED (TUI parity: usage errors keep the
    // composer so the user can correct the command).
    const skillRpcBefore = sentFrames.filter((p) => { try { return JSON.parse(p).type === 'skill'; } catch { return false; } }).length;
    await page.fill('#prompt-input', '/skill');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => Array.from(document.querySelectorAll('.toast--error')).some((t) => (t.textContent || '').includes('usage: /skill <name>')),
      '/skill bare did not surface the usage: /skill <name> error toast',
      15000
    );
    await page.waitForTimeout(1200); // window long enough for a wrongly-dispatched skill RPC to appear
    const skillRpcAfter = sentFrames.filter((p) => { try { return JSON.parse(p).type === 'skill'; } catch { return false; } }).length;
    if (skillRpcAfter !== skillRpcBefore) {
      fail(`/skill bare must not dispatch a skill RPC (frames ${skillRpcBefore} -> ${skillRpcAfter})`);
    }
    const skillBareValue = await page.evaluate(() => document.getElementById('prompt-input')?.value || '');
    if (skillBareValue !== '/skill') {
      fail(`/skill bare must preserve the draft after the usage error (value=${JSON.stringify(skillBareValue)})`);
    }
    await laneShot(page, 'skill-bare-error.png', { fullPage: true });

    // /unknowncmd: NOT a Web-supported slash — it falls through to a normal
    // prompt: an optimistic user bubble renders and a prompt RPC carries the
    // exact text (the fallback is the user's message, not an error).
    const userBubblesBefore = await page.evaluate(() => document.querySelectorAll('.msg--user').length);
    await page.fill('#prompt-input', '/unknowncmd');
    const unknownBefore = sentFrames.length;
    await page.press('#prompt-input', 'Enter');
    let unknownFrame = null;
    const unknownDeadline = Date.now() + 15000;
    while (Date.now() < unknownDeadline) {
      for (let f = unknownBefore; f < sentFrames.length; f++) {
        let frame;
        try {
          frame = JSON.parse(sentFrames[f]);
        } catch {
          continue;
        }
        if (frame && frame.type === 'prompt' && frame.message === '/unknowncmd') {
          unknownFrame = frame;
          break;
        }
      }
      if (unknownFrame) break;
      await page.waitForTimeout(100);
    }
    if (!unknownFrame) {
      fail(`/unknowncmd did not fall through to a prompt RPC (last frames: ${sentFrames.slice(-3).join(' | ')})`);
    }
    await waitFor(
      page,
      (args) => document.querySelectorAll('.msg--user').length > args,
      '/unknowncmd fallthrough never rendered an optimistic user bubble',
      15000,
      userBubblesBefore
    );
    // Wait for the fallthrough's own turn to settle (confirmed bubble +
    // unified action back to Send) so the following /code-review Enter is
    // never raced into a steer.
    await waitFor(
      page,
      () => {
        // Production idle gate: unified action must read Send (missing
        // element fails) before the confirmed-bubble check.
        if (document.getElementById('send-btn')?.getAttribute('aria-label') !== 'Send message') return false;
        return !Array.from(document.querySelectorAll('.msg--user')).some(
          (b) => (b.textContent || '').includes('/unknowncmd') && b.classList.contains('optimistic')
        );
      },
      'slash variants: /unknowncmd turn never settled (Send action + confirmed bubble)',
      30000
    );
    await laneShot(page, 'unknowncmd-fallthrough.png', { fullPage: true });

    // ------------------------------------------------------------------
    // Code Review error/close (coverage journeys). The panel is closed here;
    // each variant opens it fresh, exercises the boundary, and closes it.
    // ------------------------------------------------------------------

    // /code-review HEAD~1 HEAD: two revisions reach code_review_open and the
    // comparison label renders "HEAD~1 → HEAD"; the × close button removes
    // the panel.
    await page.fill('#prompt-input', '/code-review HEAD~1 HEAD');
    const revBefore = sentFrames.length;
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, () => document.getElementById('code-review-panel') !== null, 'code-review panel did not open for /code-review HEAD~1 HEAD');
    // The revision-args open uses the same outer shell: it must still be a
    // full-viewport overlay, never a drawer-sized right rail.
    assertCrShellGeometry(await crShellGeometry(page), 'two-revision');
    let revFrame = null;
    const revDeadline = Date.now() + 15000;
    while (Date.now() < revDeadline) {
      for (let f = revBefore; f < sentFrames.length; f++) {
        let frame;
        try {
          frame = JSON.parse(sentFrames[f]);
        } catch {
          continue;
        }
        if (frame && frame.type === 'code_review_open') {
          revFrame = frame;
          break;
        }
      }
      if (revFrame) break;
      await page.waitForTimeout(100);
    }
    if (!revFrame || revFrame.from !== 'HEAD~1' || revFrame.to !== 'HEAD') {
      fail(`/code-review HEAD~1 HEAD must send code_review_open with from=HEAD~1 to=HEAD (got ${JSON.stringify(revFrame)})`);
    }
    await waitFor(
      page,
      () => {
        const label = document.querySelector('.code-review__label')?.textContent || '';
        return label.includes('HEAD~1') && label.includes('HEAD');
      },
      'code-review panel did not render the HEAD~1 → HEAD comparison label'
    );
    await waitFor(
      page,
      () => document.querySelectorAll('.code-review__file').length > 0,
      'code-review HEAD~1 HEAD panel never rendered the changed-file list'
    );
    await laneShot(page, 'code-review-revisions.png', { fullPage: true });
    await page.click('button.code-review__close');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') === null,
      'code-review panel did not close via the × close button'
    );
    await laneShot(page, 'code-review-revisions-closed.png', { fullPage: true });

    // /code-review <bad-ref> HEAD: git resolution fails before the panel
    // opens. Toast the invalid-revision message and keep the composer draft.
    await page.fill('#prompt-input', '/code-review invalid-ref-xyz HEAD');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => Array.from(document.querySelectorAll('.toast--error')).some((t) =>
        (t.textContent || '').includes('invalid source revision; expected a commit, branch, or tag')
      ),
      '/code-review invalid-ref-xyz HEAD did not toast the invalid revision before opening a panel',
      15000
    );
    await page.waitForTimeout(800);
    const panelAfterBadRef = await page.evaluate(() => document.getElementById('code-review-panel') !== null);
    if (panelAfterBadRef) fail('/code-review invalid-ref-xyz HEAD opened the panel — invalid revisions must toast first');
    const badRefValue = await page.evaluate(() => document.getElementById('prompt-input')?.value || '');
    if (badRefValue !== '/code-review invalid-ref-xyz HEAD') {
      fail(`/code-review invalid-ref-xyz HEAD must preserve the draft after the invalid revision toast (value=${JSON.stringify(badRefValue)})`);
    }
    await laneShot(page, 'code-review-error.png', { fullPage: true });

    // /code-review a b c: the client-side arity guard toasts the usage error
    // and NEVER opens the panel (zero, one, or two revisions only).
    await page.fill('#prompt-input', '/code-review a b c');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => Array.from(document.querySelectorAll('.toast--error')).some((t) =>
        (t.textContent || '').includes('usage: /code-review [<base> | <from> <to>]')
      ),
      '/code-review a b c did not surface the arity usage error toast',
      15000
    );
    await page.waitForTimeout(800);
    const panelAfterArity = await page.evaluate(() => document.getElementById('code-review-panel') !== null);
    if (panelAfterArity) fail('/code-review a b c opened the panel — the arity guard must reject before any RPC');
    const arityValue = await page.evaluate(() => document.getElementById('prompt-input')?.value || '');
    if (arityValue !== '/code-review a b c') {
      fail(`/code-review a b c must preserve the draft after the usage error (value=${JSON.stringify(arityValue)})`);
    }
    await laneShot(page, 'code-review-arity-error.png', { fullPage: true });

    // ------------------------------------------------------------------
    // /code-review origin/master: the EXACT literal single-ref user path.
    // The fixture seeded refs/remotes/origin/master LOCALLY (git update-ref
    // — no remote URL, no network). A single <base> token must dispatch
    // EXACTLY ONE code_review_open with from=origin/master,
    // includeWorkingTree=true, and NO `to` key (the rpc.rs parser contract:
    // a lone `from` without includeWorkingTree is rejected;
    // includeWorkingTree requires `from` alone), the panel label names
    // origin/master → working tree over a non-empty diff, the fullscreen
    // shell holds on desktop AND mobile, and close restores the app.
    // ------------------------------------------------------------------
    await page.setViewportSize({ width: 1360, height: 900 });
    await waitFor(page, () => window.innerWidth === 1360 && window.innerHeight === 900, 'desktop viewport did not become 1360x900 before the origin/master phase');
    // The collapse control hands focus to the collapsed rail's reopen button on
    // the next frame (SessionSidebar's `requestAnimationFrame(...focus())`), so
    // wait for that hand-off to land BEFORE focusing the composer: otherwise it
    // can steal focus between this focus and the panel's mount, and the panel's
    // close restore would return the reopen button instead of the composer.
    const collapsedForOrigin = await page.evaluate(() => {
      const button = document.getElementById('sidebar-collapse-btn');
      if (!button || button.getClientRects().length === 0) return false;
      button.click();
      return true;
    });
    if (collapsedForOrigin) {
      await waitFor(
        page,
        () => document.activeElement?.id === 'rail-reopen-btn',
        'collapse did not hand focus to the collapsed rail reopen button',
        10000,
      );
    }
    await page.locator('#prompt-input').focus();
    const crLayoutBeforeOrigin = await crAppLayout(page);
    await page.fill('#prompt-input', '/code-review origin/master');
    const originBefore = sentFrames.length;
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, () => document.getElementById('code-review-panel') !== null, 'code-review panel did not open for /code-review origin/master');
    // Settle window so a wrongly-duplicated open would be on the wire before
    // the exact-single assertion below scans the captured frames.
    await page.waitForTimeout(800);
    let originOpen = null;
    let originOpenCount = 0;
    for (let f = originBefore; f < sentFrames.length; f++) {
      let frame;
      try {
        frame = JSON.parse(sentFrames[f]);
      } catch {
        continue;
      }
      if (frame && frame.type === 'code_review_open') {
        if (!originOpen) originOpen = frame;
        originOpenCount++;
      }
    }
    if (!originOpen
      || originOpen.from !== 'origin/master'
      || originOpen.includeWorkingTree !== true
      || Object.hasOwn(originOpen, 'to')
      || originOpenCount !== 1) {
      fail(`/code-review origin/master must dispatch exactly one code_review_open with from=origin/master includeWorkingTree=true and no to key (count=${originOpenCount}, got ${JSON.stringify(originOpen)})`);
    }
    // Comparison label: backend comparisonLabel "origin/master → working
    // tree" for the BaseToWorkingTree scope (a missing ref would instead
    // render the real git revision error, so this proves resolution).
    await waitFor(
      page,
      () => {
        const label = document.querySelector('.code-review__label')?.textContent || '';
        return label.includes('origin/master') && label.includes('working tree');
      },
      'code-review panel did not render the origin/master → working tree comparison label'
    );
    // Non-empty diff: the dirty tracked file is in the changed-file tree
    // under its full repo-relative path.
    await waitFor(
      page,
      (want) => Array.from(document.querySelectorAll('.code-review__file')).some((b) =>
        b.getAttribute('data-file-path') === want
      ),
      `origin/master panel never rendered the dirty file ${dirtyFile}`,
      20000,
      dirtyFile
    );
    assertCrShellGeometry(await crShellGeometry(page), 'origin-master desktop');
    await laneShot(page, 'code-review-origin-master.png', { fullPage: true });

    // Mobile fullscreen contract for the same single-ref open.
    await page.setViewportSize({ width: 390, height: 844 });
    await waitFor(page, () => window.innerWidth === 390 && window.innerHeight === 844, 'mobile viewport did not become 390x844 for the origin/master phase');
    await assertCrMobileShellGeometry(page, 'origin-master mobile');
    await laneShot(page, 'code-review-origin-master-mobile.png', { fullPage: true });
    await page.setViewportSize({ width: 1360, height: 900 });
    await waitFor(page, () => window.innerWidth === 1360 && window.innerHeight === 900, 'desktop viewport did not restore after the origin/master phase');

    // Close restores the prior app layout (scroll, composer rect, focus).
    await page.click('button.code-review__close');
    await waitFor(page, () => document.getElementById('code-review-panel') === null, 'origin/master panel did not close via the × close button');
    const crLayoutAfterOrigin = await crAppLayout(page);
    assertCrLayoutRestored(crLayoutBeforeOrigin, crLayoutAfterOrigin, 'origin/master close-restore');
    await laneShot(page, 'code-review-origin-master-closed.png', { fullPage: true });

    // ---- Command copy: click, keyboard, mobile touch, exact multiline/redacted payload, success/failure ----
    // This lane previously had zero real-browser command-copy assertions.
    // Drive a real bash tool card (steering mock marker) plus a durable
    // bashExecution card, then prove both copy controls against the live
    // clipboard. Restore 1360×900 afterwards so leftover viewport from Files
    // cannot leak into the remaining assertions.
    await page.setViewportSize({ width: 1360, height: 900 });
    await waitFor(page, () => window.innerWidth === 1360 && window.innerHeight === 900, 'desktop viewport did not restore 1360x900 before command-copy');
    await waitFor(page, () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true, 'WS not connected before command-copy');

    const COPY_SECRET = 'supersecret';
    const COPY_MULTILINE = `# line one\nexport token = ${COPY_SECRET}\nprintf 'command-copy-ok'\n`;
    const COPY_REDACTED = `# line one\nexport token=[REDACTED]\nprintf 'command-copy-ok'\n`;
    const TOOL_COPY_PAYLOAD = "# line one\n# line two\n# line three\n# line four\n# line five\nprintf 'command-ok\\n'";

    await waitFor(
      page,
      // Production idle gate (App.tsx): the unified action reads Send only
      // when no run is in flight; a missing element fails the gate.
      () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message',
      'command-copy: session was still streaming before the bash tool card',
      30000
    );

    const copyCardsBefore = await page.evaluate(() => document.querySelectorAll('.tool-card--command').length);
    await page.fill('#prompt-input', 'presentation bash success');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (before) => document.querySelectorAll('.tool-card--command').length > before
        && [...document.querySelectorAll('.tool-card--command')].at(-1)?.querySelector('.tool-card__command-copy') != null,
      'command-copy: bash tool card never rendered .tool-card__command-copy',
      30000,
      copyCardsBefore
    );
    await waitFor(
      page,
      () => {
        const card = [...document.querySelectorAll('.tool-card--command')].at(-1);
        return card?.getAttribute('data-tool-status') === 'done';
      },
      'command-copy: bash tool card never settled',
      30000
    );
    await assertCommandCopyButton(page, '.tool-card__command-copy', {
      expectedClipboard: TOOL_COPY_PAYLOAD,
      secret: COPY_SECRET,
    });

    const activeSid = await page.evaluate(() => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '');
    if (!activeSid) fail('command-copy: no active session id for the durable bash RPC');
    const rpc = rpcClient(wsUrlFrom(url));
    try {
      await rpc.call({ type: 'bash', sessionId: activeSid, command: COPY_MULTILINE, excludeFromContext: false });
      await waitFor(
        page,
        () => {
          const cards = [...document.querySelectorAll('.msg--bash')];
          const card = cards[cards.length - 1];
          if (!card) return false;
          if (card.getAttribute('data-bash-status') !== 'done') return false;
          const btn = card.querySelector('.bash-copy');
          return !!btn && (btn.getAttribute('aria-label') || '') === 'Copy command';
        },
        'command-copy: durable bash card never rendered .bash-copy',
        30000
      );
      await assertCommandCopyButton(page, '.bash-copy', {
        expectedClipboard: COPY_REDACTED,
        secret: COPY_SECRET,
        keyboard: true,
      });

      const failBtn = page.locator('.bash-copy').last();
      await failBtn.evaluate((el) => {
        const proto = Object.getPrototypeOf(navigator.clipboard);
        Object.defineProperty(navigator, 'clipboard', {
          configurable: true,
          value: {
            writeText: () => Promise.reject(new Error('denied')),
            readText: proto.readText ? proto.readText.bind(navigator.clipboard) : undefined,
          },
        });
      });
      await failBtn.click();
      await waitFor(
        page,
        () => {
          const btn = [...document.querySelectorAll('.bash-copy')].at(-1);
          return !!btn && (btn.textContent || '').trim() === 'copy failed' && (btn.getAttribute('aria-label') || '') === 'Copy failed';
        },
        'command-copy: clipboard rejection did not flash copy failed / Copy failed',
        10000
      );
      await waitFor(
        page,
        () => {
          const btn = [...document.querySelectorAll('.bash-copy')].at(-1);
          return !!btn && (btn.textContent || '').trim() === 'copy' && (btn.getAttribute('aria-label') || '') === 'Copy command';
        },
        'command-copy: failure flash did not reset to idle Copy command',
        4000
      );
    } finally {
      rpc.close();
    }

    await page.setViewportSize({ width: 390, height: 844 });
    await waitFor(page, () => window.innerWidth === 390 && window.innerHeight === 844, 'command-copy mobile viewport did not become 390x844');
    const touch = await page.evaluate(() => {
      const measure = (sel) => {
        const btn = [...document.querySelectorAll(sel)].at(-1);
        if (!btn) return null;
        const r = btn.getBoundingClientRect();
        return { w: r.width, h: r.height };
      };
      return {
        tool: measure('.tool-card__command-copy'),
        bash: measure('.bash-copy'),
      };
    });
    if (!touch.tool || touch.tool.w < 40 || touch.tool.h < 40) {
      fail(`command-copy mobile .tool-card__command-copy is below 40×40 (got=${JSON.stringify(touch.tool)})`);
    }
    if (!touch.bash || touch.bash.w < 40 || touch.bash.h < 40) {
      fail(`command-copy mobile .bash-copy is below 40×40 (got=${JSON.stringify(touch.bash)})`);
    }
    await page.setViewportSize({ width: 1360, height: 900 });
    await waitFor(page, () => window.innerWidth === 1360 && window.innerHeight === 900, 'desktop viewport did not restore after command-copy');
    // Designated final proof screenshot + pass marker JSON are STAGED only —
    // never published before the gates pass.
    await laneShot(page, 'command-copy.png', { fullPage: true });

    // ---- Third session for the cross-session ownership phase ----
    // A review workspace must never carry another session's revisions across a
    // switch, and with the active ref diverged from the display (below) the
    // sidebar ignores a click on the row the REF already names — so the
    // reachable cross-session move is to a third session. C is created here,
    // opens its OWN review panel (so its view slot remembers the panel), and is
    // then left for the sibling B: the ownership phase after the reconnect
    // switches back to C and asserts that C's restored panel opens C's own
    // scope, never B's revisions.
    // The origin/master phase collapses the desktop rail, and the Manage control
    // lives in the expanded sidebar: reopen it and wait for the control to be
    // hit-testable before driving the Manage flow.
    if (await page.locator('#rail-reopen-btn').isVisible()) {
      await page.click('#rail-reopen-btn');
    }
    await waitFor(
      page,
      () => {
        const button = document.getElementById('sidebar-manage-btn');
        return button !== null && button.getClientRects().length > 0;
      },
      'review ownership: the sidebar manage control never became visible for the third session setup',
      20000,
    );
    const thirdSession = await createFreshSessionViaManage(
      page,
      waitFor,
      'review ownership: third session setup',
      { closePanel: true, timeoutMs: 20000 },
    );
    const sidC = thirdSession.sessionId;
    if (!sidC || sidC === sidA || sidC === sidB) {
      fail(`review ownership: third session identity is not distinct (${sidC})`);
    }
    await openPicker(page);
    await chooseCommand(page, 'code-review');
    await waitFor(
      page,
      () => (document.getElementById('prompt-input')?.value.trim() || '').startsWith('/code-review'),
      'review ownership: choosing /code-review in the third session did not insert the draft',
    );
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') !== null,
      'review ownership: the third session review panel did not open',
      20000,
    );
    await waitFor(
      page,
      () => (document.querySelector('.code-review__label')?.textContent || '').includes('working tree'),
      'review ownership: the third session review did not open its working-tree scope',
      20000,
    );
    // Leave C for the sibling: C's slot keeps its panel, its review closes.
    await switchToSessionRow(sidB, 'review ownership: leave the third session for the sibling');
    await waitFor(
      page,
      (want) => document.getElementById('session-lifecycle')?.dataset.displayedSessionId === want,
      'review ownership: the sibling session never became the displayed session',
      20000,
      sidB,
    );

    // ---- Warm reconnect keeps slash dispatch on the DISPLAYED session ----
    // The listener's primary is the page's first session while the user is
    // looking at a sibling. A warm reconnect re-binds the App's ACTIVE session
    // ref to that primary (the bootstrap's id-less get_state + applyState)
    // while display authority — and its command target — stays on the
    // displayed sibling: the saved per-authority preference already names the
    // displayed session, so the bootstrap's saved-restore leg is redundant and
    // nothing re-activates it. Slash dispatch stamps an EXPLICIT
    // `command.sessionId`, which WINS over the displayed command target, so a
    // stamp taken from that re-bound ref sends /code-review into the primary's
    // workspace — where git answers `fatal: Needed a single revision` and the
    // Web toasts `invalid source revision; expected a commit, branch, or tag`.
    // A typed prompt carries no sessionId, which is why only the slash failed.
    const primaryProbe = rpcClient(wsUrlFrom(url));
    let listenerPrimaryId = '';
    try {
      const primaryState = await primaryProbe.call({ type: 'get_state' });
      listenerPrimaryId = typeof primaryState?.sessionId === 'string' ? primaryState.sessionId : '';
    } finally {
      primaryProbe.close();
    }
    if (!listenerPrimaryId) fail('warm reconnect: the owner-less get_state did not resolve the listener primary');
    const displayedBeforeReconnect = await page.evaluate(
      () => document.getElementById('session-lifecycle')?.dataset.displayedSessionId || '',
    );
    if (!displayedBeforeReconnect) fail('warm reconnect: no displayed session authority before the forced reconnect');
    if (displayedBeforeReconnect === listenerPrimaryId) {
      fail(`warm reconnect: the displayed session ${displayedBeforeReconnect} is the listener primary — the scenario needs a displayed sibling session`);
    }
    await forceWarmReconnect(page, 'warm reconnect');
    const afterReconnect = await page.evaluate(() => ({
      displayed: document.getElementById('session-lifecycle')?.dataset.displayedSessionId || '',
      commandTarget: document.getElementById('session-lifecycle')?.dataset.commandSessionId || '',
      activeRow: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
      socketAttempts: window.__rpiWarmReconnect.state.attempts,
      closeCodes: window.__rpiWarmReconnect.state.closeCodes,
    }));
    if (afterReconnect.socketAttempts < 2) {
      fail(`warm reconnect: no second socket generation was opened (${JSON.stringify(afterReconnect)})`);
    }
    if (afterReconnect.displayed !== displayedBeforeReconnect
      || afterReconnect.commandTarget !== displayedBeforeReconnect) {
      fail(`warm reconnect: display authority moved off the displayed session ${displayedBeforeReconnect} (${JSON.stringify(afterReconnect)})`);
    }
    await laneShot(page, 'reconnect-displayed-session.png', { fullPage: true });

    const warmSlashMark = sentFrames.length;
    await openPicker(page);
    await chooseCommand(page, 'code-review');
    await waitFor(
      page,
      () => (document.getElementById('prompt-input')?.value.trim() || '').startsWith('/code-review'),
      'warm reconnect: choosing /code-review did not insert the draft',
    );
    await page.fill('#prompt-input', '/code-review HEAD~1 HEAD');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') !== null,
      'warm reconnect: /code-review never opened the review panel',
      30000,
    );
    const warmSlashOpens = sentFrames
      .slice(warmSlashMark)
      .map(parseSentFrame)
      .filter((frame) => frame?.type === 'code_review_open');
    if (warmSlashOpens.length === 0) fail('warm reconnect: the slash dispatched no code_review_open frame');
    const slashOwner = warmSlashOpens[0].sessionId;
    if (slashOwner !== displayedBeforeReconnect) {
      fail(`warm reconnect: /code-review was stamped for ${JSON.stringify(slashOwner)} instead of the displayed session ${displayedBeforeReconnect} (listener primary ${listenerPrimaryId}) — git resolves the revision in the primary's workspace`);
    }
    if (warmSlashOpens[0].from !== 'HEAD~1' || warmSlashOpens[0].to !== 'HEAD') {
      fail(`warm reconnect: code_review_open carried the wrong revisions (${JSON.stringify(warmSlashOpens[0])})`);
    }
    // Settle window: a wrong-owner panel reopen would be on the wire here.
    await page.waitForTimeout(800);
    const foreignOpens = sentFrames
      .slice(warmSlashMark)
      .map(parseSentFrame)
      .filter((frame) => frame?.type === 'code_review_open' && frame.sessionId !== displayedBeforeReconnect);
    if (foreignOpens.length !== 0) {
      fail(`warm reconnect: ${foreignOpens.length} code_review_open frame(s) targeted ${JSON.stringify(foreignOpens.map((frame) => frame.sessionId))} instead of the displayed session ${displayedBeforeReconnect} — the review must belong to the session the user is looking at`);
    }
    // Backend ownership proof, independent of the App's own refs: the review
    // the slash opened must live in the DISPLAYED session.
    const ownerProbe = rpcClient(wsUrlFrom(url));
    let ownerSnapshot = null;
    try {
      ownerSnapshot = await ownerProbe.call({ type: 'code_review_snapshot', sessionId: displayedBeforeReconnect });
    } catch (error) {
      const openId = warmSlashOpens[0].id;
      const openResponse = receivedFrames
        .map((raw) => { try { return JSON.parse(raw); } catch { return null; } })
        .find((frame) => frame?.type === 'response' && frame.id === openId) || null;
      fail(`warm reconnect: the displayed session ${displayedBeforeReconnect} holds no open review after /code-review (${error?.message || error}) sent=${JSON.stringify(warmSlashOpens[0])} response=${JSON.stringify(openResponse)}`);
    } finally {
      ownerProbe.close();
    }
    if (!ownerSnapshot || typeof ownerSnapshot.reviewId !== 'string' || ownerSnapshot.reviewId === '') {
      fail(`warm reconnect: the displayed session's review snapshot is empty (${JSON.stringify(ownerSnapshot)})`);
    }
    await waitFor(
      page,
      () => {
        const label = document.querySelector('.code-review__label')?.textContent || '';
        return label.includes('HEAD~1') && label.includes('HEAD');
      },
      'warm reconnect: the review panel never rendered the requested revisions',
      20000,
    );
    const wrongWorkspaceToasts = await page.evaluate(() =>
      [...document.querySelectorAll('#toasts .toast--error')]
        .filter((toast) => (toast.textContent || '').includes('invalid source revision')).length
    );
    if (wrongWorkspaceToasts !== 0) {
      fail(`warm reconnect: /code-review resolved its revisions in the wrong workspace (${wrongWorkspaceToasts} invalid-revision toast(s))`);
    }
    await laneShot(page, 'reconnect-review-displayed-session.png', { fullPage: true });

    // ---- A reconnect that keeps the displayed owner keeps the review ----
    // The panel is mounted under the DISPLAY authority, so a socket reconnect
    // that still displays its owner must leave it mounted (its own epoch probe
    // reconciles the controller) — only a bootstrap that moves the display off
    // that session may close it, and the decision must never be taken from the
    // active ref, which the bootstrap re-binds to the listener primary.
    const keptMark = sentFrames.length;
    await forceWarmReconnect(page, 'kept review');
    await page.waitForTimeout(800);
    const keptState = await page.evaluate(() => ({
      panelOpen: document.getElementById('code-review-panel') !== null,
      displayed: document.getElementById('session-lifecycle')?.dataset.displayedSessionId || '',
    }));
    if (keptState.displayed !== displayedBeforeReconnect) {
      fail(`kept review: the displayed session moved (${displayedBeforeReconnect} -> ${keptState.displayed})`);
    }
    if (!keptState.panelOpen) {
      fail(`kept review: a reconnect that still displays ${displayedBeforeReconnect} closed its review panel`);
    }
    const keptFrames = sentFrames.slice(keptMark).map(parseSentFrame)
      .filter((frame) => typeof frame?.type === 'string' && frame.type.startsWith('code_review_'));
    const keptForeign = keptFrames.filter((frame) => frame.type !== 'code_review_close' && frame.sessionId !== displayedBeforeReconnect);
    if (keptForeign.length !== 0) {
      fail(`kept review: the kept panel addressed ${JSON.stringify([...new Set(keptForeign.map((frame) => frame.sessionId))])} while ${displayedBeforeReconnect} is displayed`);
    }
    const keptCloses = keptFrames.filter((frame) => frame.type === 'code_review_close');
    if (keptCloses.length !== 0) {
      fail(`kept review: the reconnect closed the displayed owner's review (${JSON.stringify(keptCloses)})`);
    }
    await laneShot(page, 'reconnect-review-kept-open.png', { fullPage: true });

    // ---- The active-ref divergence must not smuggle revisions across ----
    // The active ref names the listener primary while the sibling B is
    // displayed, so the sidebar paints the PRIMARY's row active and ignores a
    // click on it (`SessionSidebar`: `onClick: if (isActive) return;`, with the
    // active row driven by the App's session-id state) — the reachable
    // cross-session move is the third session C, whose own view slot remembers
    // its review panel. Leaving B for C must close B's review ON B and must not
    // hand B's revisions to C's restored panel: the panel opens C's own bare
    // scope, and no live frame in the window names B.
    const leaveMark = sentFrames.length;
    await switchToSessionRow(sidC, 'review ownership: leave the sibling for the third session');
    await waitFor(
      page,
      (want) => {
        const data = document.getElementById('session-lifecycle')?.dataset || {};
        return data.displayedSessionId === want && data.commandSessionId === want;
      },
      'review ownership: the display authority never settled on the third session',
      20000,
      sidC,
    );
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') !== null,
      'review ownership: the third session restored no review panel',
      20000,
    );
    // Bounded wait for the restored panel's bare open AND its response: the
    // panel boots asynchronously and the open is one round trip.
    const leaveOpenDeadline = Date.now() + 8000;
    let leaveOwnedOpen = null;
    let leaveOpenResponse = null;
    while (Date.now() < leaveOpenDeadline) {
      leaveOwnedOpen = sentFrames.slice(leaveMark).map(parseSentFrame)
        .find((frame) => frame?.type === 'code_review_open'
          && frame.sessionId === sidC
          && frame.from === undefined
          && frame.to === undefined) || null;
      if (leaveOwnedOpen) {
        leaveOpenResponse = receivedFrames
          .map((raw) => { try { return JSON.parse(raw); } catch { return null; } })
          .find((frame) => frame?.type === 'response' && frame.id === leaveOwnedOpen.id) || null;
        if (leaveOpenResponse) break;
      }
      await page.waitForTimeout(200);
    }
    const leaveFrames = sentFrames.slice(leaveMark).map(parseSentFrame)
      .filter((frame) => typeof frame?.type === 'string' && frame.type.startsWith('code_review_'));
    if (!leaveOwnedOpen) {
      fail(`review ownership: the third session's restored panel never dispatched a bare owner-owned code_review_open (${JSON.stringify(leaveFrames.map((frame) => `${frame.type}@${frame.sessionId}${frame.from ? `(from=${frame.from})` : ''}`))})`);
    }
    if (!leaveOpenResponse || leaveOpenResponse.success !== true) {
      fail(`review ownership: the third session's owner-owned open did not succeed (${JSON.stringify(leaveOpenResponse)})`);
    }
    const leaveOpenLabel = leaveOpenResponse.data?.comparisonLabel || '';
    if (!leaveOpenLabel.includes('working tree')) {
      fail(`review ownership: the third session's own open did not resolve the working-tree scope (label="${leaveOpenLabel}")`);
    }
    const leaveForeign = leaveFrames.filter((frame) => frame.type !== 'code_review_close' && frame.sessionId !== sidC);
    if (leaveForeign.length !== 0) {
      fail(`review ownership: the third session's panel addressed ${JSON.stringify([...new Set(leaveForeign.map((frame) => frame.sessionId))])} (${leaveForeign.map((frame) => frame.type).join(', ')})`);
    }
    const leaveRevisions = leaveFrames.filter((frame) => frame.from === 'HEAD~1' || frame.to === 'HEAD');
    if (leaveRevisions.length !== 0) {
      fail(`review ownership: the third session's panel carried the sibling session's revisions (${JSON.stringify(leaveRevisions)})`);
    }
    const leaveCloses = leaveFrames.filter((frame) => frame.type === 'code_review_close');
    if (leaveCloses.length !== 1 || leaveCloses[0].sessionId !== sidB) {
      fail(`review ownership: leaving the sibling must close exactly that session's review (${JSON.stringify(leaveCloses)})`);
    }
    const closedOwnerProbe = rpcClient(wsUrlFrom(url));
    try {
      await closedOwnerProbe.call({ type: 'code_review_snapshot', sessionId: sidB });
      fail(`review ownership: ${sidB} still holds an open review after switching away`);
    } catch (error) {
      if (!(error?.message || '').includes('no open code review')) {
        fail(`review ownership: ${sidB} review probe failed unexpectedly: ${error?.message || error}`);
      }
    } finally {
      closedOwnerProbe.close();
    }
    await laneShot(page, 'review-ownership-third-session.png', { fullPage: true });
    // The desktop shell is a full-viewport overlay: close the restored panel so
    // the deferred-probe phase starts from a closed panel (and a reachable
    // composer).
    await page.click('.code-review__title');
    await page.keyboard.press('Escape');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') === null,
      'review ownership: Escape did not close the third session panel',
    );

    // ---- Owner-targeted cleanup must survive a pending switch ----
    // The probe resolves AFTER a negotiated warm switch painted the next
    // session but BEFORE its authoritative cut. The review belongs to the
    // session being left, so it is closed there: the close must be allowed
    // while that switch is pending (an ungated send is rejected by
    // commandRejectedBySwitchPending) and stamped with the review's own
    // session, and no panel may mount over the session the user moved to.
    //
    // The switch is forced onto the negotiated path deterministically: the
    // sibling session's reconnect catch-up cut is held, so it never re-enters
    // this socket generation's ready cache and its next switch cannot take the
    // frontend-only fast path. The switch_session response is held too, which
    // keeps the pending window open until the release below.
    await page.evaluate((target) => window.__rpiWarmReconnect.holdNextCutFor(target), sidB);
    await forceWarmReconnect(page, 'deferred probe');
    await waitFor(
      page,
      () => window.__rpiWarmReconnect.state.held.some((entry) => entry === `cut:${window.__rpiWarmReconnect.state.cutHoldSessionId || ''}`)
        || window.__rpiWarmReconnect.state.held.some((entry) => entry.startsWith('cut:')),
      'deferred probe: the reconnect never re-subscribed the sibling session (no cut to hold)',
      20000,
    );
    // Hold the SWITCH's authoritative cut too, so the pending window stays open
    // across the releases below.
    await page.evaluate((target) => window.__rpiWarmReconnect.holdNextCutFor(target), sidB);
    await page.evaluate(() => window.__rpiWarmReconnect.holdNextReviewOpen());
    await page.evaluate(() => window.__rpiWarmReconnect.holdNextSwitchSession());
    const deferredMark = sentFrames.length;
    await openPicker(page);
    await chooseCommand(page, 'code-review');
    await waitFor(
      page,
      () => (document.getElementById('prompt-input')?.value.trim() || '').startsWith('/code-review'),
      'deferred probe: choosing /code-review did not insert the draft',
    );
    await page.fill('#prompt-input', '/code-review HEAD~1 HEAD');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => window.__rpiWarmReconnect.state.held.some((entry) => entry.startsWith('code_review_open:')),
      'deferred probe: the code_review_open response was not held',
      20000,
    );
    await clickSessionRow(sidB, 'deferred probe: switch to the sibling session inside the pending window');
    await waitFor(
      page,
      () => window.__rpiWarmReconnect.state.held.some((entry) => entry.startsWith('switch_session:')),
      'deferred probe: the switch did not negotiate on the wire (no switch_session was held)',
      20000,
    );
    // Release ONLY the negotiation: its handoff paints the sibling as the
    // displayed session while the switch stays pending (its authoritative cut is
    // held), which is exactly the window the mismatched-owner cleanup runs in.
    await page.evaluate(() => window.__rpiWarmReconnect.releaseHeld(['switchSession']));
    const handoffDeadline = Date.now() + 20000;
    let handoffDisplayed = '';
    while (Date.now() < handoffDeadline) {
      handoffDisplayed = await page.evaluate(() => document.getElementById('session-lifecycle')?.dataset.displayedSessionId || '');
      if (handoffDisplayed === sidB) break;
      await page.waitForTimeout(100);
    }
    if (handoffDisplayed !== sidB) {
      const handoffState = await page.evaluate(() => ({
        held: window.__rpiWarmReconnect.state.held,
        lifecycle: { ...(document.getElementById('session-lifecycle')?.dataset || {}) },
      }));
      fail(`deferred probe: the handoff never painted the sibling session (displayed="${handoffDisplayed}") ${JSON.stringify(handoffState)} ${JSON.stringify(await reviewRestoreDiagnostics(page, sentFrames, deferredMark))}`);
    }
    await waitFor(
      page,
      () => window.__rpiWarmReconnect.state.held.filter((entry) => entry.startsWith('cut:')).length >= 2,
      'deferred probe: the switch authoritative cut was never held',
      20000,
    );
    // Now resolve the probe: the review belongs to the session being left, so it
    // is closed there — and that close must survive the pending switch. The mark
    // is taken AFTER the handoff so the slash's own (legitimate, pre-handoff)
    // open for the then-displayed session is not counted against the new owner.
    const postHandoffMark = sentFrames.length;
    await page.evaluate(() => window.__rpiWarmReconnect.releaseHeld(['reviewOpen']));
    const cleanupDeadline = Date.now() + 20000;
    let deferredCloses = [];
    while (Date.now() < cleanupDeadline) {
      deferredCloses = sentFrames.slice(postHandoffMark).map(parseSentFrame)
        .filter((frame) => frame?.type === 'code_review_close');
      if (deferredCloses.length > 0) break;
      await page.waitForTimeout(100);
    }
    if (deferredCloses.length !== 1 || deferredCloses[0].sessionId !== sidC) {
      fail(`deferred probe: the mismatched-owner cleanup must close exactly the review's own session (${JSON.stringify(deferredCloses)})`);
    }
    const leftOwnerProbe = rpcClient(wsUrlFrom(url));
    try {
      await leftOwnerProbe.call({ type: 'code_review_snapshot', sessionId: sidC });
      fail(`deferred probe: ${sidC} still holds an open review after the mismatched-owner cleanup`);
    } catch (error) {
      if (!(error?.message || '').includes('no open code review')) {
        fail(`deferred probe: ${sidC} review probe failed unexpectedly: ${error?.message || error}`);
      }
    } finally {
      leftOwnerProbe.close();
    }
    // The handoff painted the sibling as the displayed session, so the sibling's
    // OWN slot panel may legitimately restore here (the documented contract):
    // what must never happen is a live review command naming the session the
    // user just left.
    const cleanupFrames = sentFrames.slice(postHandoffMark).map(parseSentFrame)
      .filter((frame) => typeof frame?.type === 'string' && frame.type.startsWith('code_review_'));
    const cleanupForeign = cleanupFrames.filter((frame) => frame.type !== 'code_review_close' && frame.sessionId !== sidB);
    if (cleanupForeign.length !== 0) {
      fail(`deferred probe: after the cleanup the mounted surface addressed ${JSON.stringify([...new Set(cleanupForeign.map((frame) => frame.sessionId))])} (${cleanupForeign.map((frame) => frame.type).join(', ')}) while ${sidB} is displayed`);
    }
    await laneShot(page, 'reconnect-deferred-probe-cleanup.png', { fullPage: true });
    const settleMark = sentFrames.length;
    await page.evaluate(() => window.__rpiWarmReconnect.releaseHeld(['cuts']));
    await waitFor(
      page,
      (target) => document.getElementById('session-lifecycle')?.dataset.displayedSessionId === target
        && document.getElementById('session-lifecycle')?.dataset.pendingGeneration === '',
      'deferred probe: the negotiated switch never settled',
      30000,
      sidB,
    );
    // The sibling's own view slot may re-open ITS review panel here (it was
    // displaying one when the user left it): whatever it issues must be the
    // sibling's, never the session the user left.
    await page.waitForTimeout(800);
    const settledFrames = sentFrames.slice(settleMark).map(parseSentFrame)
      .filter((frame) => typeof frame?.type === 'string' && frame.type.startsWith('code_review_') && frame.type !== 'code_review_close');
    const settledForeign = settledFrames.filter((frame) => frame.sessionId !== sidB);
    if (settledForeign.length !== 0) {
      fail(`deferred probe: the settled panel addressed ${JSON.stringify([...new Set(settledForeign.map((frame) => frame.sessionId))])} while ${sidB} is displayed`);
    }

    // ---- The mounted panel's owner close must survive a pending switch ----
    // A warm display activation unmounts the review panel while its switch is
    // still pending (the App clears activePanel on the display transition), so
    // the panel's own owner-stamped close must be allowed during that window:
    // an ungated send is rejected by the pending-switch gate and the review
    // stays open on the session that owns it. The switch is forced onto the
    // negotiated path (the sibling's catch-up cut is held, keeping it out of
    // this generation's ready cache) and its authoritative cut is held too, so
    // the pending window covers the unmount.
    // The sibling is displayed WITH its own review panel open (restored from its
    // slot). Leave it that way: the programmatic row click works over the shell,
    // and leaving a session with its panel open is what keeps `code-review` in
    // that session's view slot — so the switch back below restores a panel for
    // the TARGET, which is the panel whose bootstrap this phase asserts.
    await clickSessionRow(sidC, 'pending unmount: move to the third session with the sibling panel open');
    await waitFor(
      page,
      (want) => {
        const data = document.getElementById('session-lifecycle')?.dataset || {};
        return data.displayedSessionId === want && data.commandSessionId === want;
      },
      'pending unmount: the display authority never settled on the third session',
      20000,
      sidC,
    );
    // The third session's own restored panel (if its slot kept one) would cover
    // the composer: close it so the slash below is reachable.
    if (await page.evaluate(() => document.getElementById('code-review-panel') !== null)) {
      await page.click('.code-review__title');
      await page.keyboard.press('Escape');
      await waitFor(
        page,
        () => document.getElementById('code-review-panel') === null,
        'pending unmount: Escape did not close the third session panel',
      );
    }
    const panelCloseMark = sentFrames.length;
    await openPicker(page);
    await chooseCommand(page, 'code-review');
    await waitFor(
      page,
      () => (document.getElementById('prompt-input')?.value.trim() || '').startsWith('/code-review'),
      'pending unmount: choosing /code-review did not insert the draft',
    );
    await page.fill('#prompt-input', '/code-review');
    const unmountDraft = await page.evaluate(() => document.getElementById('prompt-input')?.value || '');
    if (unmountDraft.trim() !== '/code-review') {
      fail(`pending unmount: the composer draft was lost before submit (value=${JSON.stringify(unmountDraft)})`);
    }
    await page.press('#prompt-input', 'Enter');
    const unmountPanelDeadline = Date.now() + 20000;
    let unmountPanelOpen = false;
    while (Date.now() < unmountPanelDeadline) {
      unmountPanelOpen = await page.evaluate(() => document.getElementById('code-review-panel') !== null);
      if (unmountPanelOpen) break;
      await page.waitForTimeout(100);
    }
    if (!unmountPanelOpen) {
      const unmountOpenState = await page.evaluate(() => ({
        toasts: [...document.querySelectorAll('#toasts .toast')].map((toast) => toast.textContent || ''),
        lifecycle: { ...(document.getElementById('session-lifecycle')?.dataset || {}) },
      }));
      fail(`pending unmount: the review panel did not open ${JSON.stringify(unmountOpenState)} ${JSON.stringify(await reviewRestoreDiagnostics(page, sentFrames, panelCloseMark))}`);
    }
    await waitFor(
      page,
      () => (document.querySelector('.code-review__label')?.textContent || '').includes('working tree'),
      'pending unmount: the review did not apply its working-tree snapshot',
      20000,
    );
    const unmountOwner = await page.evaluate(
      () => document.getElementById('session-lifecycle')?.dataset.displayedSessionId || '',
    );
    if (!unmountOwner || unmountOwner === sidB) {
      fail(`pending unmount: the review is not owned by a non-target session (owner=${unmountOwner})`);
    }
    await page.evaluate((target) => window.__rpiWarmReconnect.holdNextCutFor(target), sidB);
    await forceWarmReconnect(page, 'pending unmount');
    await waitFor(
      page,
      () => window.__rpiWarmReconnect.state.held.some((entry) => entry.startsWith('cut:')),
      'pending unmount: the reconnect never re-subscribed the sibling session',
      20000,
    );
    await page.evaluate((target) => window.__rpiWarmReconnect.holdNextCutFor(target), sidB);
    await page.evaluate(() => window.__rpiWarmReconnect.holdNextSwitchSession());
    await clickSessionRow(sidB, 'pending unmount: switch to the sibling inside the pending window');
    await waitFor(
      page,
      () => window.__rpiWarmReconnect.state.held.some((entry) => entry.startsWith('switch_session:')),
      'pending unmount: the switch did not negotiate on the wire',
      20000,
    );
    // The handoff paints the sibling while the switch stays pending (its
    // authoritative cut is held) — the unmount happens right here.
    await page.evaluate(() => window.__rpiWarmReconnect.releaseHeld(['switchSession']));
    await waitFor(
      page,
      (target) => document.getElementById('session-lifecycle')?.dataset.displayedSessionId === target,
      'pending unmount: the handoff never painted the sibling session',
      20000,
      sidB,
    );
    await waitFor(
      page,
      () => window.__rpiWarmReconnect.state.held.filter((entry) => entry.startsWith('cut:')).length >= 2,
      'pending unmount: the switch authoritative cut was never held',
      20000,
    );
    const unmountCloseDeadline = Date.now() + 20000;
    let unmountCloses = [];
    while (Date.now() < unmountCloseDeadline) {
      unmountCloses = sentFrames.slice(panelCloseMark).map(parseSentFrame)
        .filter((frame) => frame?.type === 'code_review_close' && frame.sessionId === unmountOwner);
      if (unmountCloses.length > 0) break;
      await page.waitForTimeout(100);
    }
    if (unmountCloses.length !== 1) {
      const pendingToasts = await page.evaluate(() =>
        [...document.querySelectorAll('#toasts .toast')].map((toast) => toast.textContent || ''));
      fail(`pending unmount: the panel's owner close must land exactly once while the switch is pending (closes=${JSON.stringify(unmountCloses)}, toasts=${JSON.stringify(pendingToasts)})`);
    }
    const unmountOwnerProbe = rpcClient(wsUrlFrom(url));
    try {
      await unmountOwnerProbe.call({ type: 'code_review_snapshot', sessionId: unmountOwner });
      fail(`pending unmount: ${unmountOwner} still holds an open review after the panel unmounted`);
    } catch (error) {
      if (!(error?.message || '').includes('no open code review')) {
        fail(`pending unmount: ${unmountOwner} review probe failed unexpectedly: ${error?.message || error}`);
      }
    } finally {
      unmountOwnerProbe.close();
    }
    // The landed close frame is the proof the close was NOT rejected (an ungated
    // send is refused before it ever reaches the wire), so this checks the
    // remaining toast surface for a CLOSE failure specifically: the
    // switch-pending toasts a restored sibling panel can raise in this same
    // window come from ITS boot/probe sends, which are ungated — a separate
    // panel-side gap, not this close.
    const closeFailureToasts = await page.evaluate(() =>
      [...document.querySelectorAll('#toasts .toast')]
        .filter((toast) => (toast.textContent || '').includes('close failed')).length);
    if (closeFailureToasts !== 0) {
      fail(`pending unmount: the owner close reported a failure (${closeFailureToasts} toast(s))`);
    }
    await laneShot(page, 'reconnect-panel-unmount-pending.png', { fullPage: true });
    await page.evaluate(() => window.__rpiWarmReconnect.releaseHeld(['cuts']));
    await waitFor(
      page,
      (target) => document.getElementById('session-lifecycle')?.dataset.displayedSessionId === target
        && document.getElementById('session-lifecycle')?.dataset.pendingGeneration === '',
      'pending unmount: the negotiated switch never settled',
      30000,
      sidB,
    );

    // ---- A restored panel must recover its OWN bootstrap after the settle ----
    // The mount bootstrap (bare owner open + model catalog) can land inside the
    // pending window. The panel must still reach a SUCCESSFUL target-owned bare
    // code_review_open with an applied working-tree snapshot and a recovered
    // model catalog afterwards — and surface no switch-pending toast.
    const bootstrapDeadline = Date.now() + 20000;
    let bootstrapOwnedOpen = null;
    let bootstrapOpenResponse = null;
    let bootstrapModelFrames = 0;
    while (Date.now() < bootstrapDeadline) {
      const bootstrapFrames = sentFrames.slice(panelCloseMark).map(parseSentFrame)
        .filter((frame) => frame !== null);
      bootstrapOwnedOpen = bootstrapFrames.find((frame) => frame.type === 'code_review_open'
        && frame.sessionId === sidB
        && frame.from === undefined
        && frame.to === undefined) || null;
      bootstrapModelFrames = bootstrapFrames
        .filter((frame) => frame.type === 'get_available_models' && frame.sessionId === sidB).length;
      if (bootstrapOwnedOpen) {
        bootstrapOpenResponse = receivedFrames
          .map((raw) => { try { return JSON.parse(raw); } catch { return null; } })
          .find((frame) => frame?.type === 'response' && frame.id === bootstrapOwnedOpen.id) || null;
        if (bootstrapOpenResponse && bootstrapModelFrames > 0) break;
      }
      await page.waitForTimeout(200);
    }
    if (!bootstrapOwnedOpen) {
      const bootstrapFrames = sentFrames.slice(panelCloseMark).map(parseSentFrame)
        .filter((frame) => typeof frame?.type === 'string' && frame.type.startsWith('code_review_'))
        .map((frame) => `${frame.type}@${frame.sessionId}${frame.from ? `(from=${frame.from})` : ''}${frame.to ? `(to=${frame.to})` : ''}`);
      fail(`pending unmount: the restored panel never dispatched a bare target-owned code_review_open after the settle (sidB=${sidB}, frames=${JSON.stringify(bootstrapFrames)}, models=${bootstrapModelFrames})`);
    }
    if (!bootstrapOpenResponse || bootstrapOpenResponse.success !== true) {
      // (the frame exists; the response is missing or a failure)
      fail(`pending unmount: the restored panel's target-owned open did not succeed (${JSON.stringify(bootstrapOpenResponse)})`);
    }
    const bootstrapOpenLabel = bootstrapOpenResponse.data?.comparisonLabel || '';
    if (!bootstrapOpenLabel.includes('working tree')) {
      fail(`pending unmount: the restored panel's own open did not resolve the working-tree scope (label="${bootstrapOpenLabel}")`);
    }
    if (bootstrapModelFrames === 0) {
      fail('pending unmount: the restored panel never recovered its model catalog');
    }
    const bootstrapPanelState = await page.evaluate(() => ({
      panel: document.getElementById('code-review-panel') !== null,
      label: document.querySelector('.code-review__label')?.textContent || '',
      toasts: [...document.querySelectorAll('#toasts .toast')].map((toast) => toast.textContent || ''),
    }));
    if (!bootstrapPanelState.panel) {
      fail('pending unmount: the restored panel is not mounted after the settle');
    }
    if (!bootstrapPanelState.label.includes('working tree')) {
      fail(`pending unmount: the restored panel did not apply its own snapshot (label="${bootstrapPanelState.label}")`);
    }
    const bootstrapPendingToasts = bootstrapPanelState.toasts.filter((text) => text.includes('session switch pending')).length;
    if (bootstrapPendingToasts !== 0) {
      fail(`pending unmount: the restored panel surfaced ${bootstrapPendingToasts} switch-pending toast(s) (${JSON.stringify(bootstrapPanelState.toasts)})`);
    }
    await laneShot(page, 'reconnect-restored-panel-bootstrap.png', { fullPage: true });

    // All actions executed: close every remaining page/browser while the
    // handlers are still effective, so close-time errors land in the same
    // per-page arrays (the mobile context already closed mid-run).
    await browser.close().catch(() => {});

    // Final union gate AFTER everything is closed: every page/context's
    // errors (desktop page + mobile code-review page) under ONE console
    // policy — no desktop-only array can stand in for mobile.
    const allPageErrors = [...pageErrors, ...mobilePageErrors];
    const allConsoleErrors = [...consoleErrors, ...mobileConsoleErrors];
    if (allPageErrors.length || allConsoleErrors.length) {
      fail(`browser errors (desktop + mobile union): ${JSON.stringify({ pageErrors: allPageErrors, consoleErrors: allConsoleErrors })}`);
    }
    // files.zero-errors records at the FINAL ACTUAL browser-error union:
    // the union gate above just evaluated the complete desktop+mobile error
    // arrays after every page/context closed and passed, so the zero-error
    // contract is truly proven — the record never precedes the gate it
    // certifies (an early record would let a failing union stand behind a
    // recorded zero-errors claim).
    recordFiles('files.zero-errors');
    // Exact per-ID ledger gate BEFORE staging, over the REAL filesExecuted
    // Map: its ACTUAL insertion order (execution order) must equal FILES_IDS
    // elementwise — every documented File Explorer id executed EXACTLY once,
    // nothing undocumented, no reordering (never a sorted/set comparison;
    // the length check subsumes missing and undocumented ids).
    const filesOrder = [...filesExecuted.keys()];
    if (filesOrder.length !== FILES_IDS.length
      || filesOrder.some((id, index) => id !== FILES_IDS[index])) {
      fail(`File Explorer ledger mismatch — executed ${JSON.stringify(filesOrder)} != documented ${JSON.stringify([...FILES_IDS])}`);
    }
    const repeatedFiles = [...filesExecuted].filter(([, count]) => count !== 1).map(([id]) => id);
    if (repeatedFiles.length) fail(`File Explorer exact-once violation: ${repeatedFiles.join(', ')}`);
    // Stage the REAL Map (actual insertion order + real per-ID counts) —
    // never a reconstruction of FILES_IDS — plus the manifest, then report
    // the staged path. This script NEVER prints PASS: the lane PASS line is
    // printed by commands_review.sh only after the shell's HTTP sanity +
    // listener-close gates and its exact staged-set/ledger validation and
    // the single run-dir rename + pointer commit succeed.
    stageWrite('coverage-assertions.json', JSON.stringify({
      documented: [...FILES_IDS],
      executed: filesOrder,
      counts: Object.fromEntries(filesExecuted),
    }, null, 2));
    stageManifest();
    console.log(`web-commands-review: STAGED ${stagingDir}`);
  } catch (err) {
    // Failure diagnostics OUTSIDE the rollback staging: fail() removes the
    // staging dir, so anything that must survive the failure is written to
    // the shell-provided evidence dir (next to playwright.out) BEFORE the
    // staging teardown. Best-effort: a diagnostics fault never masks the
    // original error.
    const message = err && err.message ? err.message : String(err);
    const stack = err && err.stack ? err.stack : '';
    const failureDir = process.env.RPI_FAILURE_EVIDENCE_DIR || '';
    if (failureDir) {
      try {
        fs.mkdirSync(failureDir, { recursive: true });
        if (ownershipTrace.startSentIndex !== null) {
          fs.writeFileSync(path.join(failureDir, 'failure-review-ownership-wire.json'), JSON.stringify(ownershipTrace, null, 2));
        }
        const context = { message, stack, laneNonce, pages: [] };
        for (const [label, p] of [['desktop', diagMainPage], ['mobile', diagMobilePage]]) {
          if (!p) continue;
          const entry = { label };
          try { entry.viewport = p.viewportSize(); } catch (e) { entry.viewportError = String((e && e.message) || e); }
          try { entry.url = p.url(); } catch (e) { entry.urlError = String((e && e.message) || e); }
          try {
            entry.dom = await p.evaluate(() => {
              const probe = (sel) => {
                const el = document.querySelector(sel);
                if (!el) return { present: false };
                const rect = el.getBoundingClientRect();
                const cs = getComputedStyle(el);
                return {
                  present: true,
                  rect: { x: rect.x, y: rect.y, w: rect.width, h: rect.height },
                  display: cs.display,
                  visibility: cs.visibility,
                  ariaPressed: el.getAttribute('aria-pressed'),
                  clientRects: el.getClientRects().length,
                };
              };
              return {
                innerWidth: window.innerWidth,
                innerHeight: window.innerHeight,
                activeElement: document.activeElement ? (document.activeElement.id || document.activeElement.tagName) : null,
                fileExplorerToggle: probe('#file-explorer-toggle-btn'),
                sidebarToggle: probe('#sidebar-toggle-btn'),
                railReopen: probe('#rail-reopen-btn'),
                fileExplorerSection: probe('section.file-explorer'),
              };
            });
          } catch (e) { entry.domError = String((e && e.message) || e); }
          for (const [shot, opts] of [['full', { fullPage: true }], ['viewport', {}]]) {
            try {
              await p.screenshot({ path: path.join(failureDir, `failure-${label}-${shot}.png`), ...opts });
              entry[`${shot}Shot`] = `failure-${label}-${shot}.png`;
            } catch (e) { entry[`${shot}ShotError`] = String((e && e.message) || e); }
          }
          context.pages.push(entry);
        }
        fs.writeFileSync(path.join(failureDir, 'failure-context.json'), JSON.stringify(context, null, 2));
        console.error(`web-commands-review: failure diagnostics written to ${failureDir} (failure-context.json, failure-*-*.png)`);
      } catch (diagErr) {
        console.error(`web-commands-review: failure diagnostics capture failed: ${(diagErr && diagErr.message) || diagErr}`);
      }
    } else {
      console.error(`web-commands-review: failure stack: ${stack || message}`);
    }
    await browser.close().catch(() => {});
    console.error(`web-commands-review: FAIL: ${message}`);
    cleanupStaging();
    if (INJECT_FAIL && fs.existsSync(stagingDir)) {
      console.error(`web-commands-review: FAIL: staging dir survived failure cleanup: ${stagingDir}`);
      process.exit(3);
    }
    process.exit(2);
  }
}

main().catch((err) => {
  // A crash outside the browser scope is also a FAILED run: remove only the
  // staging dir; published evidence is never touched.
  cleanupStaging();
  console.error(`web-commands-review: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});