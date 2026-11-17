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
//   - opening the command picker lists /compact, /skill, /review
//   - choosing /review inserts the draft WITHOUT auto-submitting
//   - the inline slash-autocomplete popup OWNS the selecting Enter and never
//     dispatches: the first Enter settles the token and dismisses the popup,
//     and the INDEPENDENT second Enter dispatches the command
//     (lib/slash-completion.mjs drives that contract at every slash site)
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
//     stamped with the owning sessionId), and a bare /review in the
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
//   - the outer shell (section.code-review-panel) matches the Files section:
//     left 0, right edge at the viewport, top at the measured header, bottom
//     at the viewport, padding 0, and .code-review flush with the section.
//     The session rail and New Session are not the top hit. Desktop 1440,
//     phone 390, iPad portrait 820, and landscape 1180 each fail a rail inset
//     or outer gutter by name. Closing restores the prior app layout.
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
//   - code-review error/close: /review HEAD~1 HEAD opens the panel with
//     the from/to revisions and closes via the × button; /review
//     <bad-ref> HEAD renders the real git revision error state with Retry
//     and closes cleanly; /review a b c is a client-side arity toast
//     that never opens the panel
//   - the literal single-ref user path `/review origin/master` (the
//     fixture seeds refs/remotes/origin/master LOCALLY — no remote URL, no
//     network) dispatches EXACTLY ONE code_review_open with
//     from=origin/master includeWorkingTree=true and no `to` key (the
//     parser contract), the comparison label names origin/master → working
//     tree over a non-empty diff (the dirty file row is present), the
//     header-to-footer shell contract holds on desktop 1360x900 AND mobile
//     390x844 (assertCrShellGeometry / assertCrMobileShellGeometry), the exact command result is
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
//     `/review HEAD~1 HEAD` then dispatches code_review_open stamped with
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
//   - Files max layout (files.fullscreen, after files.ask): desktop 1280×900
//     and mobile 390×844 open Files over the app body with the global header
//     visible and the whole composer footer hidden at 0px (footer-hide
//     contract). There is no expand control. Close then reopen
//     keeps the Ask draft and tab. A custom tree width survives that reopen,
//     a fresh session starts at 29, and a stored 29 is not treated as a
//     derived home step. Ask fills the remaining height with Source visible
//     on desktop; mobile reaches both.
//   - files.selection-cache-compaction: retained Ask lines 4–7 survive real
//   - Dependency map (files.map, after files.selection): the committed clean
//     MapRoot.sol opens its directory graph. Search then the tree selects
//     directory beta; the request path is beta and the nodes are only beta.
//     The relation list shows the fixture import arrow and source purpose.
//     A held alpha response after the beta or workspace switch paints nothing.
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
import { closeManageSession, createFreshSessionViaManage, openManageSession } from './lib/session-lifecycle.mjs';
import { clickFeature, openSessionsDrawer } from './lib/feature-menu.mjs';
import { selectCompletionThenSubmit } from './lib/slash-completion.mjs';

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
const parentCwd = process.env.RPI_PARENT_CWD || '';
const childCommit = process.env.RPI_CHILD_COMMIT || '';
const INJECT_FAIL = process.env.INJECT_FAIL === '1';

const FILES_IDS = [
  'files.solidity-preview',
  'files.nongit',
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
  'files.citations',
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
function isKnownConsoleNoise(message) {
  const text = typeof message === 'string' ? message : message.text();
  const url = typeof message === 'string' ? '' : message.location()?.url || '';
  let path = '';
  try { path = url ? new URL(url).pathname : ''; } catch { path = ''; }
  return text === 'Failed to load resource: the server responded with a status of 404 (Not Found)' && path === '/favicon.ico'
    || text === "Failed to read the 'localStorage' property from 'Window': The document is sandboxed and lacks the 'allow-same-origin' flag."
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
      footer: (() => {
        const f = document.querySelector('.app-main > footer');
        if (!f) return null;
        const fr = f.getBoundingClientRect();
        return {
          display: getComputedStyle(f).display,
          heightVar: Number.parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--app-footer-height')) || 0,
          rectH: fr ? fr.height : 0,
        };
      })(),
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
    const root = getComputedStyle(document.documentElement);
    const px = (name) => {
      const raw = root.getPropertyValue(name).trim();
      const value = Number.parseFloat(raw);
      return raw.endsWith('px') && Number.isFinite(value) ? value : null;
    };
    return {
      x: r.x, y: r.y, w: r.width, h: r.height, bottom: r.bottom,
      innerW: window.innerWidth, innerH: window.innerHeight,
      vvW: vv ? vv.width : window.innerWidth,
      vvH: vv ? vv.height : window.innerHeight,
      position: cs.position,
      headerHeight: px('--app-header-height'),
      footerHeight: px('--app-footer-height'),
      pad: `${cs.paddingTop} ${cs.paddingRight} ${cs.paddingBottom} ${cs.paddingLeft}`,
      covered: (() => {
        const hit = (node) => {
          const box = node?.getBoundingClientRect();
          if (!box || box.width < 1 || box.height < 1) return false;
          const el = document.elementFromPoint(box.x + box.width / 2, box.y + box.height / 2);
          return !!el && (el === node || node.contains(el));
        };
        return {
          sidebar: hit(document.querySelector('.session-sidebar')),
          newSession: hit(document.getElementById('sidebar-manage-btn')),
        };
      })(),
      head, body,
      composer: (() => {
        const c = p.querySelector('.code-review__composer textarea');
        const cr = c?.getBoundingClientRect() || null;
        return {
          rect: cr ? { h: Math.round(cr.height) } : null,
          hitInside: !!cr && (() => { const t = document.elementFromPoint(cr.x + cr.width / 2, cr.y + Math.min(cr.height / 2, Math.max(cr.height - 1, 1))); return !!t && (t === c || c.contains(t)); })(),
        };
      })(),
      docScrollW: document.documentElement.scrollWidth,
      docScrollH: document.documentElement.scrollHeight,
    };
  });
}

/** Files-section shell: left 0, right at the viewport, top at the measured
 *  header, bottom at the viewport while the main footer is hidden, padding
 *  0, and .code-review flush with the section. A rail inset, outer gutter,
 *  or rail/New Session hit fails here. */
function assertCrShellGeometry(g, label) {
  if (!g) fail(`${label}: #code-review-panel missing for shell geometry assertions`);
  if (Math.abs(g.x) > 1 || Math.abs(g.innerW - g.x - g.w) > 1) {
    fail(`${label}: review shell must match the Files section box (left 0, right at viewport ${g.innerW}; got x=${g.x} w=${g.w})`);
  }
  if (g.pad !== '0px 0px 0px 0px') fail(`${label}: section.code-review-panel padding must be 0, got ${g.pad}`);
  if (!g.body || Math.abs(g.body.x - g.x) > 1 || Math.abs(g.body.y - g.y) > 1 || Math.abs(g.body.width - g.w) > 1 || Math.abs(g.body.height - g.h) > 1) {
    fail(`${label}: .code-review must share the section bounds (no outer gutter) shell=${g.x},${g.y},${g.w}x${g.h} inner=${g.body ? `${g.body.x},${g.body.y},${g.body.width}x${g.body.height}` : 'missing'}`);
  }
  if (g.covered?.sidebar || g.covered?.newSession) {
    fail(`${label}: session rail or New Session is the top hit ${JSON.stringify(g.covered)}`);
  }
  const headerHeight = g.headerHeight;
  if (g.footerHeight !== 0) fail(`${label}: review open must hide the WHOLE mounted composer footer (--app-footer-height=${g.footerHeight}, expected 0px)`);
  const footerTop = g.innerH;
  if (headerHeight === null || headerHeight === undefined || Math.abs(g.y - headerHeight) > 1) {
    fail(`${label}: shell must start at the app header bottom (y=${g.y}, --app-header-height=${headerHeight}) — full-viewport cover or drawer-height regression`);
  }
  if (Math.abs(g.w - (g.innerW - g.x)) > 2) {
    fail(`${label}: shell width ${g.w} does not fill from x=${g.x} to viewport ${g.innerW} — width-rail/route-gutter regression ${JSON.stringify(g)}`);
  }
  const expectedH = footerTop === null ? null : footerTop - headerHeight;
  if (expectedH === null || Math.abs(g.h - expectedH) > 2 || Math.abs(g.bottom - footerTop) > 1) {
    fail(`${label}: shell height ${g.h} must stop at the viewport bottom while the composer footer is hidden (bottom=${g.bottom}, viewport ${g.innerH}, header ${headerHeight}) — 90vh/100dvh or footer-visible regression ${JSON.stringify(g)}`);
  }
  if (g.position !== 'fixed') fail(`${label}: shell position must be fixed (got "${g.position}")`);
  if (g.head && (g.head.top < g.y - 1 || g.head.bottom > g.y + g.h + 1)) {
    fail(`${label}: header escapes the shell rect (head ${g.head.top}..${g.head.bottom}, shell ${g.y}..${g.y + g.h})`);
  }
  if (g.body && (g.body.top < g.y - 1 || g.body.bottom > g.y + g.h + 1)) {
    fail(`${label}: body escapes the shell rect (body ${g.body.top}..${g.body.bottom}, shell ${g.y}..${g.y + g.h})`);
  }
  if (g.docScrollW > g.innerW + 1 || g.docScrollH > g.innerH + 1) {
    fail(`${label}: page scrolls behind the shell (doc ${g.docScrollW}x${g.docScrollH} vs viewport ${g.innerW}x${g.innerH})`);
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
  if (after.activeId !== 'prompt-input') {
    fail(`${label}: close must auto-focus the prompt draft before any manual focus (before "${before.activeId}", after "${after.activeId}")`);
  }
  if (!after.footer || after.footer.display === 'none' || after.footer.heightVar <= 0 || Math.abs(after.footer.heightVar - after.footer.rectH) > 1) {
    fail(`${label}: close must restore the whole composer footer (--app-footer-height>0 matching the live rect): ${JSON.stringify(after.footer)}`);
  }
}

/** Same Files-flush shell as assertCrShellGeometry, at the caller's viewport. */
async function assertCrMobileShellGeometry(page, label) {
  assertCrShellGeometry(await crShellGeometry(page), label);
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

/** Payload fingerprint for failure messages: length + djb2, never the text. */
const textHash = (text) => {
  let h = 5381;
  for (let i = 0; i < text.length; i += 1) h = ((h * 33) ^ text.charCodeAt(i)) >>> 0;
  return `${text.length}:${h.toString(16)}`;
};

async function assertCommandCopyButton(page, selector, {
  expectedClipboard,
  secret = 'supersecret',
  keyboard = false,
  frames = [],
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
  const button = await page.locator(selector).last().elementHandle();
  if (!button) fail(`command-copy ${selector}: no copy button handle to act on`);
  // Identifiers only: which transcript row owns this button, observed for replacement during the click.
  const rowBefore = await page.evaluate((btn) => {
    const row = btn?.closest('[data-item-id]') || null;
    const trace = window.__copyRowTrace = {
      row, removed: false, addedItemIds: [],
      toolCallId: row?.querySelector('[data-tool-call-id]')?.getAttribute('data-tool-call-id') || '',
    };
    trace.observer = new MutationObserver(() => {
      if (trace.row && !trace.row.isConnected) trace.removed = true;
      if (!trace.toolCallId) return;
      for (const candidate of document.querySelectorAll('[data-item-id]')) {
        const card = candidate.querySelector('[data-tool-call-id]');
        const itemId = candidate.getAttribute('data-item-id');
        if (card?.getAttribute('data-tool-call-id') === trace.toolCallId
          && candidate !== trace.row && !trace.addedItemIds.includes(itemId)) trace.addedItemIds.push(itemId);
      }
    });
    trace.observer.observe(document.getElementById('transcript') || document.body, { childList: true, subtree: true });
    return {
      itemId: trace.row?.getAttribute('data-item-id') ?? null,
      toolCallId: trace.toolCallId,
      status: trace.row?.querySelector('[data-tool-status]')?.getAttribute('data-tool-status') ?? null,
    };
  }, button);
  let primaryError = null;
  try {
    if (keyboard) {
      await button.focus();
      const focused = await page.evaluate((el) => document.activeElement === el, button);
      if (!focused) fail(`command-copy ${selector}: keyboard focus never landed on the copy button`);
      await page.keyboard.press('Enter');
    } else {
      await button.click();
    }
    // Wall-clock polling on the SAME pinned handle: the observation must not depend on rendering rAF.
    await page.waitForFunction(
      (btn) => !!btn && btn.isConnected
        && (btn.textContent || '').trim() === 'copied' && (btn.getAttribute('aria-label') || '') === 'Command copied',
      button,
      { timeout: 10000, polling: 25 }
    );
  } catch (error) {
    // Failure only: clipboard first, compared by boolean/length/hash — never the payload or the secret.
    const actual = await readClipboardText(page);
    const observed = await page.evaluate(({ sel, btn }) => {
      const live = document.querySelectorAll(sel);
      const trace = window.__copyRowTrace || {};
      const last = live[live.length - 1] || null;
      return {
        label: btn && btn.isConnected ? (btn.textContent || '').trim() : null,
        aria: btn && btn.isConnected ? btn.getAttribute('aria-label') : null,
        hasFocus: document.hasFocus(),
        visibilityState: document.visibilityState,
        buttonCount: live.length,
        handleConnected: !!(btn && btn.isConnected),
        handleIsLast: live.length > 0 && btn === last,
        rowRemoved: trace.removed === true,
        rowAddedItemIds: trace.addedItemIds || [],
        rowAfterItemId: last ? last.closest('[data-item-id]')?.getAttribute('data-item-id') ?? null : null,
      };
    }, { sel: selector, btn: button });
    const stateStamp = [...frames].reverse().map((raw) => {
      try { return JSON.parse(raw); } catch { return null; }
    }).find((frame) => frame && frame.type === 'response' && /get_(messages|state)/.test(String(frame.command || '')));
    error.message = `${error.message} (observed=${JSON.stringify({ rowBefore, ...observed,
      generationStamp: stateStamp ? { command: stateStamp.command, id: stateStamp.id, ts: stateStamp.timestamp ?? null } : null,
      clipboardMatchesExpected: actual === expectedClipboard, clipboardLen: actual.length, clipboardHash: textHash(actual),
      expectedLen: expectedClipboard.length, expectedHash: textHash(expectedClipboard), secretLeaked: actual.includes(secret) })})`;
    primaryError = error;
  }
  // Observer cleanup is part of the assertion: its failure is never swallowed.
  try {
    await page.evaluate(() => { window.__copyRowTrace?.observer?.disconnect(); });
  } catch (cleanupError) {
    const cleanupLabel = `command-copy ${selector}: observer cleanup failed`;
    throw primaryError
      ? new AggregateError([primaryError, cleanupError], `${cleanupLabel} (primary failure is errors[0])`)
      : new Error(cleanupLabel, { cause: cleanupError });
  }
  if (primaryError) throw primaryError;
  const copied = await readClipboardText(page);
  if (copied !== expectedClipboard) {
    fail(`command-copy ${selector}: clipboard was not the exact redacted multiline payload (got=${JSON.stringify(copied)})`);
  }
  if (copied.includes(secret)) {
    fail(`command-copy ${selector}: clipboard recovered the hidden secret`);
  }
  try {
    await page.waitForFunction(
      (btn) => !!btn && btn.isConnected
        && (btn.textContent || '').trim() === 'copy' && (btn.getAttribute('aria-label') || '') === 'Copy command',
      button,
      { timeout: 4000, polling: 25 }
    );
  } catch {
    fail(`command-copy ${selector}: success flash did not reset to idle Copy command`);
  }
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

async function revealSessionRail(page, label) {
  const graph = page.getByRole('button', { name: 'Close dependency graph' });
  if (await graph.count()) await graph.click();
  if (await page.locator(FILES_DRAWER_CLOSE).count()) await page.locator(FILES_DRAWER_CLOSE).click();
  if (!await page.evaluate(() => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true)) {
    await openSessionsDrawer(page);
  }
  await waitFor(page, () => {
    const button = document.getElementById('sidebar-manage-btn');
    const box = button?.getBoundingClientRect();
    return box && box.width > 0 && box.height > 0;
  }, `${label}: session manage control is not visible`, 20000);
}

async function openFileExplorer(page) {
  if ((await page.locator('section.file-explorer[aria-label="File explorer"]').count()) > 0) return;
  await clickFeature(page, '#file-explorer-toggle-btn');
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
  // Sample center, scrollTop, and hit identity in one layout turn. A later
  // boundingBox() is a different snapshot and can land on a sibling row.
  const placed = await row.evaluate((node) => {
    const tree = node.closest('.file-explorer__tree-viewport');
    const list = node.closest('.file-explorer__tree-list');
    if (!tree || !list) return { error: 'file row has no tree list' };
    const before = node.getBoundingClientRect();
    const bounds = list.getBoundingClientRect();
    list.scrollTop += before.top + before.height / 2 - (bounds.top + bounds.bottom) / 2;
    const after = node.getBoundingClientRect();
    const next = list.getBoundingClientRect();
    const round = (rect) => ({ x: Math.round(rect.x), y: Math.round(rect.y), w: Math.round(rect.width), h: Math.round(rect.height) });
    const point = { x: after.left + after.width / 2, y: after.top + after.height / 2 };
    const el = document.elementFromPoint(point.x, point.y);
    const hitRect = el?.getBoundingClientRect();
    const hitRow = el?.closest('button.file-explorer__row');
    return {
      before: round(before),
      after: round(after),
      tree: round(next),
      scrollTop: list.scrollTop,
      clientHeight: list.clientHeight,
      inside: after.width > 0 && after.height > 0 && after.top >= next.top && after.bottom <= next.bottom,
      point: { x: point.x, y: point.y },
      row: el === node || hitRow === node,
      tag: el?.tagName || '',
      className: typeof el?.className === 'string' ? el.className : '',
      testid: el?.getAttribute('data-testid') || '',
      rect: hitRect ? round(hitRect) : null,
      hitTitle: hitRow?.getAttribute('title') || '',
      targetTitle: node.getAttribute('title') || '',
    };
  });
  if (placed.error) fail(`${filePath}: ${placed.error}`);
  const rowBlocker = { ...placed, row: placed.after, targetIsHit: placed.row === true };
  if (!placed.inside) {
    const error = new Error(`File Explorer row ${filePath} is outside the tree scroller`);
    error.rowBlocker = rowBlocker;
    throw error;
  }
  if (!placed.row) {
    const error = new Error(`File Explorer row ${filePath} is not the top element at its center`);
    error.rowBlocker = rowBlocker;
    throw error;
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
    const explorer = document.querySelector('section.file-explorer')?.getBoundingClientRect();
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
      explorerLeft: explorer ? Math.round(explorer.left) : null,
      explorerRight: explorer ? Math.round(window.innerWidth - explorer.right) : null,
      outerResizer: document.getElementById('file-explorer-resizer') != null,
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
  if (chrome.outerResizer) fail('obsolete #file-explorer-resizer is still mounted');
  if (desktopResizer && (chrome.explorerLeft !== 0 || chrome.explorerRight !== 0)) {
    fail(`desktop Files is not the full viewport width (${JSON.stringify({ left: chrome.explorerLeft, right: chrome.explorerRight })})`);
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
  const dirtyDelta = diffFramesAfter - diffFramesBefore;
  const allDelta = allDiffAfter - allDiffBefore;
  if (dirtyDelta < 0 || dirtyDelta > 1 || allDelta !== dirtyDelta) {
    const requests = filesWsOf(sentFrames, 'file_explorer_diff').map((frame) => frame.path);
    fail(`${label}: status chip diff requests disagree with the already-loaded file cache (${JSON.stringify({ dirtyFile, before: diffFramesBefore, after: diffFramesAfter, allBefore: allDiffBefore, allAfter: allDiffAfter, dirtyDelta, allDelta, requests })})`);
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
  await page.focus('[data-testid="file-explorer-divider"]');
  const focused = await page.evaluate(() => document.activeElement?.getAttribute('data-testid') || document.activeElement?.tagName || '');
  if (focused !== 'file-explorer-divider') {
    fail(`divider keyboard target is ${focused}, not the ARIA separator`);
  }
  await page.keyboard.press('ArrowRight');
  await page.keyboard.press('ArrowRight');
  await waitFor(
    page,
    (start) => {
      const now = Number(document.querySelector('[data-testid="file-explorer-divider"]')?.getAttribute('aria-valuenow') || '0');
      const pct = getComputedStyle(document.querySelector('[data-testid="file-explorer-layout"]')).getPropertyValue('--file-explorer-tree-pct').trim();
      return now === start + 4 && pct === `${start + 4}%`;
    },
    'divider keyboard move did not advance the ARIA separator by two ArrowRight steps',
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
    (start) => {
      const tree = document.querySelector('[data-testid="file-explorer-tree"] .file-explorer__tree-list');
      const content = document.querySelector('.file-explorer__markdown, .file-explorer__code');
      const now = Number(document.querySelector('[data-testid="file-explorer-divider"]')?.getAttribute('aria-valuenow') || '0');
      const pct = getComputedStyle(document.querySelector('[data-testid="file-explorer-layout"]')).getPropertyValue('--file-explorer-tree-pct').trim();
      return tree != null && content != null
        && tree.scrollTop > 0
        && content.scrollTop > 0
        && now === start + 4
        && pct === `${start + 4}%`;
    },
    'S0 treePct/treeScroll/contentScroll did not keep the moved separator and nonzero scroll',
    25000,
    before
  );
}



async function assertSendReclaimed(page, label) {
  const proof = await page.evaluate(() => {
    const leftover = document.querySelectorAll('.file-explorer, #file-explorer-close-btn').length;
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
      formerIsFiles: former?.closest('.file-explorer, #file-explorer-close-btn') != null,
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
    const pressed = surface?.querySelector('[aria-pressed="true"]');
    const treeTop = tree?.getBoundingClientRect().top ?? -1;
    const contentTop = content?.getBoundingClientRect().top ?? -1;
    const surfaceBox = surface?.getBoundingClientRect();
    const contentBox = content?.getBoundingClientRect();
    return {
      surfaceInHead: !!head?.querySelector('.file-explorer__surface, [aria-label="File surface"]'),
      surfaceInContent: !!content?.contains(surface),
      surfaceAboveContentBody: !!(surfaceBox && contentBox && surfaceBox.top <= contentBox.top + 2),
      checked: (pressed?.textContent || '').replace(/\s+/g, ' ').trim(),
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

  const previewWasWide = await page.evaluate(() => window.innerWidth > 720);
  await page.setViewportSize({ width: 390, height: 844 });
  await waitFor(page, () => window.innerWidth === 390 && window.innerHeight === 844, 'mobile viewport did not become 390x844 for preview');
  if (previewWasWide) {
    await waitFor(page, () => window.matchMedia('(max-width: 720px)').matches && document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') !== true, 'preview mobile sessions drawer did not settle closed');
  }
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
  for (let step = 0; step < (30 - 22) / 2; step++) {
    await page.keyboard.press('ArrowRight');
  }
  await waitFor(
    page,
    () => Number(document.querySelector('[data-testid="file-explorer-divider"]')?.getAttribute('aria-valuenow')) === 30,
    'file tree split never reached the custom 30% width'
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
  if (!Number.isFinite(s0Pct) || s0Pct === 29 || s0.treePct === '29%' || s0.treePct === '') {
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
  await hideFileExplorer(page, 'citation prompt');
  const citationOwner = await displayedExplorerSession(page);
  await page.fill('#prompt-input', 'render workspace citations');
  await page.press('#prompt-input', 'Enter');
  await waitFor(page, () => [...document.querySelectorAll('#transcript code.md-code-path')].some((node) => node.getAttribute('data-md-path') === 'unique.rs'), 'citation reply did not render the unique file', 30000);
  const citationNames = await page.evaluate(() => [...document.querySelectorAll('#transcript code.md-code-path')].map((node) => node.getAttribute('data-md-path')));
  for (const name of ['unique.rs', 'deposit_logs.rs', 'daemon.rs', 'prove_bridge.rs']) {
    if (!citationNames.includes(name)) fail(`citation reply missing ${name}: ${JSON.stringify(citationNames)}`);
  }
  const proveUnique = async (label) => {
    await hideFileExplorer(page, `${label} before unique`);
    const before = sentFrames.length;
    await page.locator('#transcript code.md-code-path[data-md-path="unique.rs"]').last().click();
    await waitFor(page, () => (document.querySelector('[aria-label="Selected file"] pre.file-explorer__code')?.textContent || '').includes('CITE-UNIQUE'), `${label}: unique.rs content did not open`, 20000);
    const reads = sentFrames.slice(before).map(parseSentFrame).filter((frame) => frame?.type === 'file_explorer_read' && frame.path === 'unique.rs');
    const probe = reads.filter((frame) => frame.maxLines === 1);
    const response = receivedFrames.slice(before).map(parseSentFrame).find((frame) => frame?.type === 'response' && frame.id === probe[0]?.id);
    const searches = sentFrames.slice(before).map(parseSentFrame).filter((frame) => frame?.type === 'file_explorer_search');
    if (probe.length !== 1 || probe[0].sessionId !== citationOwner || response?.success !== true || response?.data?.path !== 'unique.rs' || searches.length !== 0) {
      fail(`${label}: unique open was not the direct unique.rs read (${JSON.stringify({ probe, response, searches })})`);
    }
  };
  const proveAmbiguous = async (name, label) => {
    const stem = name.slice(0, -3);
    const copyA = `psy-node/bridge/${name}`;
    const copyB = `psy-node-fix/bridge/${name}`;
    await hideFileExplorer(page, `${label} before ${name}`);
    const before = sentFrames.length;
    await page.locator(`#transcript code.md-code-path[data-md-path="${name}"]`).last().click();
    await waitFor(page, (paths) => {
      const shown = [...document.querySelectorAll('[data-testid="file-explorer-search-results"] .file-explorer__row .file-explorer__row-path')].map((node) => node.textContent || '');
      return shown.length === 2 && shown.includes(paths.copyA) && shown.includes(paths.copyB);
    }, `${label}: ${name} did not list exactly its two full paths`, 20000, { copyA, copyB });
    const reads = sentFrames.slice(before).map(parseSentFrame).filter((frame) => frame?.type === 'file_explorer_read' && frame.path === name);
    if (reads.length === 0 || reads.some((frame) => frame.sessionId !== citationOwner)) fail(`${label}: missing-file read was not owner scoped (${JSON.stringify(reads)})`);
    await page.locator('[data-testid="file-explorer-search-results"] .file-explorer__row', { hasText: copyA }).click();
    await waitFor(page, (marker) => (document.querySelector('pre.file-explorer__code')?.textContent || '').includes(marker), `${label}: ${copyA} did not open`, 20000, `COPY-A-${stem}`);
    await hideFileExplorer(page, `${label} after ${copyA}`);
    await page.locator(`#transcript code.md-code-path[data-md-path="${name}"]`).last().click();
    await page.locator('[data-testid="file-explorer-search-results"] .file-explorer__row', { hasText: copyB }).click();
    await waitFor(page, (marker) => (document.querySelector('pre.file-explorer__code')?.textContent || '').includes(marker), `${label}: ${copyB} did not open`, 20000, `COPY-B-${stem}`);
    await laneShot(page, `${label.startsWith('mobile') ? 'files-citations-mobile' : 'files-citations-desktop'}-${stem}.png`, { fullPage: false });
    await hideFileExplorer(page, `${label} after ${copyB}`);
  };
  // One directory citation per breakpoint, each against a DIFFERENT public
  // fixture directory. An accepted file open now auto-reveals (and CACHES)
  // its parent chain, so a directory citation that follows any file or
  // ambiguous citation finds its target already listed: the panel's reveal
  // correctly skips the cached dir, and the only later bridge list is the
  // unrelated serialized mount refresh — so the exact "validate then reveal"
  // wire pair could only be sampled by luck. Both breakpoint runs therefore
  // execute BEFORE every file/ambiguous citation and each opens a directory
  // that no citation has touched yet.
  //   target.href   transcript link + workspace-relative directory
  //   target.parent its expandable parent row (must be re-expanded by reveal)
  //   target.child  a real listing-backed child row (proves the dir settled)
  const proveDirectoryCitation = async (label, target) => {
    if (label.startsWith('mobile')) {
      await page.locator('#transcript code.md-code-path[data-md-path="unique.rs"]').last().click();
      await waitFor(page, () => document.querySelector('section.file-explorer') !== null, `${label}: unique.rs did not open Files`, 20000);
    } else {
      await openFileExplorer(page);
    }
    // The search box lives in the tree pane; on narrow viewports Files opens
    // tree-collapsed (content layout), so a REAL 'Expand file tree' toggle
    // click must expose it first — never a forced DOM set. Desktop (expanded)
    // has no Expand-labeled toggle and goes straight through.
    const expandTree = page.locator('button[aria-label="Expand file tree"]');
    if (await expandTree.count() && await expandTree.isVisible()) await expandTree.click();
    await waitFor(page, () => document.querySelector('[data-testid="file-explorer-search"]')?.offsetParent !== null, `${label}: file tree did not expand to expose the search box`, 10000);
    // The kept candidate list is the DOCUMENTED choice-mode convenience
    // (FEATURES: picking a row keeps the list; a search edit, a directory
    // choice, or an accepted markdown open clears it). Exit it with a REAL
    // user-visible search edit — type a changing query (proves the input is
    // editable in choices mode and the edit clears the candidates), then
    // clear the query so the plain tree owns the pane.
    await page.locator('[data-testid="file-explorer-search"]').fill('zz-no-match');
    await waitFor(
      page,
      () => document.querySelector('[data-testid="file-explorer-search"]')?.value === 'zz-no-match'
        && document.querySelector('[data-file-ambiguous-choice]') === null,
      `${label}: a search edit must be accepted in choices mode and clear the candidates`,
    );
    await page.locator('[data-testid="file-explorer-search"]').fill('');
    await waitFor(
      page,
      () => document.querySelector('[data-testid="file-explorer-search"]')?.value === ''
        && document.querySelector('[data-testid="file-explorer-search-results"]') === null,
      `${label}: clearing the query did not restore the tree before collapsing ${target.parent}`,
    );
    const parent = page.locator(`button.file-explorer__row--dir[title="${target.parent}"]`);
    if (await parent.count()) {
      if ((await parent.getAttribute('aria-expanded')) === 'true') await parent.click();
      await waitFor(
        page,
        (dir) => document.querySelector(`button.file-explorer__row--dir[title="${dir}"]`)?.getAttribute('aria-expanded') === 'false',
        `${label}: ${target.parent} did not collapse before the directory citation`,
        25000,
        target.parent,
      );
    }
    await hideFileExplorer(page, `${label} before directory citation`);
    const before = sentFrames.length;
    await page.locator(`#transcript a[href="${target.href}"]`).last().click();
    await waitFor(page, (want) => {
      const tree = document.querySelector('[data-testid="file-explorer-tree"]');
      const layout = document.querySelector('[data-testid="file-explorer-layout"]')?.getAttribute('data-layout') || '';
      const parentRow = document.querySelector(`button.file-explorer__row--dir[title="${want.parent}"]`);
      const targetRow = document.querySelector(`button.file-explorer__row--dir[title="${want.href}"]`);
      const child = [...document.querySelectorAll('button.file-explorer__row')].some((row) => (row.getAttribute('title') || '') === want.child);
      const treeBox = tree?.getBoundingClientRect();
      const mobile = window.matchMedia('(max-width: 720px)').matches;
      return tree != null
        && tree.hasAttribute('hidden') === false
        && treeBox != null
        && treeBox.width > 0
        && treeBox.height > 0
        && (!mobile || layout === 'tree')
        && parentRow?.getAttribute('aria-expanded') === 'true'
        && targetRow?.getAttribute('aria-expanded') === 'true'
        && child;
    }, `${label}: ${target.href} citation did not expand its parent and child`, 20000, target);
    const sent = sentFrames.slice(before).map(parseSentFrame);
    const read = sent.find((frame) => frame?.type === 'file_explorer_read' && frame.path === target.href && frame.maxLines === 1);
    const listed = sent.filter((frame) => frame?.type === 'file_explorer_list' && (frame.path === target.parent || frame.path === target.href));
    const response = receivedFrames.map(parseSentFrame).find((frame) => frame?.type === 'response' && frame.id === read?.id);
    const targetLists = listed.filter((frame) => frame.path === target.href && frame.sessionId === citationOwner);
    if (!read || read.sessionId !== citationOwner || response?.success !== false || response?.data?.code !== 'directory_unavailable' || targetLists.length < 2) {
      fail(`${label}: directory citation did not validate then reveal ${target.href} (${JSON.stringify({ read, response, listed })})`);
    }
    const parentListed = sentFrames.map(parseSentFrame).some((frame) => frame?.type === 'file_explorer_list' && frame.path === target.parent && frame.sessionId === citationOwner);
    if (!parentListed) {
      fail(`${label}: directory citation has no owner parent list (${JSON.stringify(listed)})`);
    }
    const directoryTarget = page.locator(`button.file-explorer__row--dir[title="${target.href}"]`);
    await directoryTarget.scrollIntoViewIfNeeded();
    const directoryBox = await directoryTarget.boundingBox();
    if (!directoryBox || directoryBox.width <= 0 || directoryBox.height <= 0) {
      fail(`${label}: ${target.href} is not visible in the tree viewport`);
    }
    await laneShot(page, `${label.startsWith('mobile') ? 'files-directory-citation-mobile' : 'files-directory-citation-desktop'}.png`, { fullPage: false });
    await hideFileExplorer(page, `${label} after directory citation`);
  };
  // Directory citations FIRST, at both breakpoints, before ANY file or
  // ambiguous citation: each target is then cold at its own turn (the
  // desktop target's chain is untouched, and the mobile target's directory
  // family — psy-node-fix — is untouched because the desktop run only
  // reveals psy-node/*). The exact-one-count rules, timeouts, owner scoping
  // and DOM waits above are unchanged.
  const directoryCitationTargets = {
    'desktop citation': { href: 'psy-node/bridge', parent: 'psy-node', child: 'psy-node/bridge/deposit_logs.rs' },
    'mobile citation': { href: 'psy-node-fix/bridge', parent: 'psy-node-fix', child: 'psy-node-fix/bridge/deposit_logs.rs' },
  };
  const citationViewport = page.viewportSize();
  for (const label of ['desktop citation', 'mobile citation']) {
    if (label.startsWith('mobile')) {
      await page.setViewportSize({ width: 390, height: 844 });
      await waitFor(page, () => window.innerWidth === 390, 'citation mobile viewport');
    }
    await proveDirectoryCitation(label, directoryCitationTargets[label]);
  }
  for (const label of ['desktop citation', 'mobile citation']) {
    if (label.startsWith('mobile')) {
      await page.setViewportSize({ width: 390, height: 844 });
      await waitFor(page, () => window.innerWidth === 390, 'citation mobile viewport');
    } else if (citationViewport != null) {
      await page.setViewportSize(citationViewport);
      await waitFor(page, (size) => window.innerWidth === size.width && window.innerHeight === size.height, 'citation desktop viewport restore', 10000, citationViewport);
    }
    await proveUnique(label);
    for (const name of ['deposit_logs.rs', 'daemon.rs', 'prove_bridge.rs']) await proveAmbiguous(name, label);
  }
  await page.setViewportSize({ width: 1360, height: 900 });
  recordFiles('files.citations');
  await openFileExplorer(page);
  await page.locator('[data-testid="file-explorer-search"]').fill('');
  await waitFor(page, () => document.querySelector('[data-testid="file-explorer-search"]')?.value === '' && document.querySelector('[data-testid="file-explorer-search-results"]') === null, 'citation search did not clear before source selection');
  await hideFileExplorer(page, 'citation search clear');

  await runFileSelectionJourney(page, sentFrames, await displayedExplorerSession(page));
  recordFiles('files.selection');
  await runFileMapJourney(page, sentFrames, receivedFrames);
  recordFiles('files.map');
  await runFilesFullscreenJourney(page);
  recordFiles('files.fullscreen');
  await hideFileExplorer(page, 'tablet hide before mobile');
  const mobileWasWide = await page.evaluate(() => window.innerWidth > 720);
  await page.setViewportSize({ width: 390, height: 844 });
  await waitFor(page, () => window.innerWidth === 390 && window.innerHeight === 844, 'mobile viewport did not become 390x844');
  if (mobileWasWide) {
    await waitFor(page, () => window.matchMedia('(max-width: 720px)').matches && document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') !== true, 'files mobile sessions drawer did not settle closed');
  }
  await openFileExplorer(page);
  await filesChromeContract(page, { desktopResizer: false });
  const closeBox = await page.locator(FILES_DRAWER_CLOSE).boundingBox();
  if (!closeBox || closeBox.width < 44 || closeBox.height < 44) {
    fail(`mobile drawer close is ${closeBox?.width ?? -1}x${closeBox?.height ?? -1}, need >=44x44`);
  }
  await clickFilesRow(page, markdownFile);
  await page.getByRole('button', { name: 'Preview', exact: true }).click();
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

function askBoundTo(path) {
  const dock = document.querySelector('section.file-conversation');
  const active = document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || '';
  return dock != null
    && dock.getAttribute('aria-label') === 'File conversation'
    && (dock.getAttribute('data-session-id') || '') !== ''
    && active === path;
}

async function clickVisibleAsk(page, label) {
  const ask = page.getByRole('textbox', { name: 'File question' });
  await waitFor(
    page,
    () => {
      const control = document.querySelector('textarea[aria-label="File question"]');
      if (!control) return false;
      const box = control.getBoundingClientRect();
      return box.width > 0 && box.height > 0 && getComputedStyle(control).visibility !== 'hidden';
    },
    `${label}: File question is not a visible control`
  );
  const box = await ask.boundingBox();
  if (!box || box.width < 1 || box.height < 1) fail(`${label}: File question has no visible box`);
  const hit = await page.evaluate(({ x, y }) => {
    const el = document.elementFromPoint(x, y);
    return el?.closest('textarea[aria-label="File question"]') != null;
  }, { x: box.x + box.width / 2, y: box.y + Math.min(box.height / 2, 12) });
  if (!hit) fail(`${label}: File question is covered at its visible center`);
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
    const sourceNode = document.querySelector('[data-testid="file-explorer-content"][aria-label="Selected file"] pre.file-explorer__code');
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
    const body = document.querySelector('.file-explorer__body')?.getBoundingClientRect();
    const inside = (box) => body != null && box != null && box.x >= body.x - 1 && box.x + box.width <= body.x + body.width + 1
      && box.y >= body.y - 1 && box.y + box.height <= body.y + body.height + 1;
    const sideBySide = sourceBox != null && dockBox != null && sourceBox.x + sourceBox.width <= dockBox.x + 1
      && sourceBox.y < dockBox.y + dockBox.height && dockBox.y < sourceBox.y + sourceBox.height;
    const stacked = sourceBox != null && dockBox != null && sourceBox.y + sourceBox.height <= dockBox.y + 1
      && sourceBox.height >= 120 && dockBox.height >= 120;
    return {
      pathText: document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || '',
      dockLabel: dock?.getAttribute('aria-label') || '',
      conversationId: dock?.getAttribute('data-conversation-id') ?? null,
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
      askHeight: document.querySelector('textarea[aria-label="File question"]')?.getBoundingClientRect().height ?? 0,
      drawerInside: section != null && section.getBoundingClientRect().width <= window.innerWidth + 1,
      bodyWidth: body ? Math.round(body.width) : 0,
      overflow: document.documentElement.scrollWidth <= document.documentElement.clientWidth + 1,
    };
  }, SOLIDITY_SOURCE_LINES);
  if (chrome.dockLabel !== 'File conversation' || chrome.pathText !== filePath) {
    fail(`${label}: Ask dock is not bound to ${filePath} (path=${JSON.stringify(chrome.pathText)} label=${JSON.stringify(chrome.dockLabel)} id=${JSON.stringify(chrome.conversationId)})`);
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
    const root = document.querySelector('[data-testid="file-explorer-content"][aria-label="Selected file"] pre.file-explorer__code');
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
    const box = (node) => {
      if (!node || node.getClientRects().length === 0) return null;
      const rect = node.getBoundingClientRect();
      const computed = getComputedStyle(node);
      return {
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
        left: rect.left,
        width: rect.width,
        height: rect.height,
        position: computed.position,
        topStyle: computed.top,
        leftStyle: computed.left,
        maxHeight: computed.maxHeight,
        heightStyle: computed.height,
        overflow: computed.overflow,
        visibility: computed.visibility,
      };
    };
    const paneBox = pane?.getBoundingClientRect();
    const lines = [];
    const lineBoxes = [];
    for (let line = start; line <= end; line += 1) {
      const node = document.getElementById(`file-explorer-line-${line}`);
      const rect = node?.getBoundingClientRect();
      if (!rect || rect.width <= 0 || rect.height <= 0) {
        lineBoxes.push({ line, missing: true });
        continue;
      }
      lines.push(rect);
      const onPane = paneBox != null
        && rect.bottom > paneBox.top && rect.top < paneBox.bottom
        && rect.right > paneBox.left && rect.left < paneBox.right;
      lineBoxes.push({
        line,
        top: rect.top,
        bottom: rect.bottom,
        left: rect.left,
        right: rect.right,
        height: rect.height,
        offPane: paneBox != null && !onPane,
      });
    }
    const barBox = bar?.getBoundingClientRect();
    const floatNode = document.querySelector('.file-explorer__selection-float');
    const popupNode = document.querySelector('[data-testid="file-selection-blame-list"]');
    const sourceNode = document.querySelector('.file-explorer__source');
    const selection = lines.length === 0 ? null : {
      top: Math.min(...lines.map((rect) => rect.top)),
      bottom: Math.max(...lines.map((rect) => rect.bottom)),
      left: Math.min(...lines.map((rect) => rect.left)),
      right: Math.max(...lines.map((rect) => rect.right)),
    };
    const style = bar ? getComputedStyle(bar) : null;
    const floatStyle = floatNode ? getComputedStyle(floatNode) : null;
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
    const sourceBox = sourceNode?.getBoundingClientRect();
    const usableTop = paneBox && sourceBox ? Math.max(paneBox.top, sourceBox.top) : null;
    const usableBottom = paneBox && sourceBox ? Math.min(paneBox.bottom, sourceBox.bottom) : null;
    const roomBelow = usableBottom != null && selection
      ? usableBottom - selection.bottom >= (barBox?.height ?? 0) + edge + edge
      : false;
    const roomAbove = usableTop != null && selection
      ? selection.top - usableTop >= (barBox?.height ?? 0) + edge + edge
      : false;
    const blame = bar?.querySelector('[data-testid="file-selection-blame"]');
    const close = bar?.querySelector('[data-testid="file-selection-close"]');
    const question = document.querySelector('textarea.file-conversation__input');
    const caption = document.querySelector('.file-conversation__context');
    const questionBox = question?.getBoundingClientRect();
    const captionBox = caption?.getBoundingClientRect();
    const target = (node) => {
      if (!node) return 0;
      const rect = node.getBoundingClientRect();
      return Math.min(rect.width, rect.height);
    };
    const codeBox = code?.getBoundingClientRect();
    return {
      outsideSource: bar != null && code != null && !code.contains(bar),
      position: style?.position || '',
      inside,
      overlaps: overlaps === true,
      room: roomBelow || roomAbove,
      blame: blame?.getAttribute('aria-label') || '',
      close: close?.getAttribute('aria-label') || '',
      targets: [target(blame), target(close)],
      narrow: window.matchMedia('(max-width: 720px)').matches,
      question: question?.getAttribute('aria-label') || '',
      questionVisible: questionBox != null && questionBox.width > 0 && questionBox.height > 0,
      caption: caption?.textContent || '',
      captionVisible: captionBox != null && captionBox.width > 0 && captionBox.height > 0,
      geometry: {
        viewport: { width: window.innerWidth, height: window.innerHeight },
        pane: box(pane),
        source: box(sourceNode),
        code: codeBox == null ? null : {
          top: codeBox.top,
          right: codeBox.right,
          bottom: codeBox.bottom,
          left: codeBox.left,
          width: codeBox.width,
          height: codeBox.height,
          scrollTop: code.scrollTop,
          scrollHeight: code.scrollHeight,
          clientHeight: code.clientHeight,
        },
        selection,
        lines: lineBoxes,
        linesOffPane: lineBoxes.filter((entry) => entry.offPane === true).map((entry) => entry.line),
        bar: box(bar),
        float: box(floatNode),
        popup: box(popupNode),
        anchor: floatNode == null ? null : {
          top: floatNode.style.top,
          left: floatNode.style.left,
          placement: floatNode.getAttribute('data-placement'),
          position: floatStyle?.position || '',
        },
      },
    };
  }, { start: startLine, end: endLine, edge: SELECTION_EDGE_PX });
  if (!proof.outsideSource || proof.position === 'static') {
    fail(`${label}: selection bar is not a floating overlay outside the source (${JSON.stringify(proof)})`);
  }
  if (!proof.inside) fail(`${label}: floating bar is not inside the content pane by ${SELECTION_EDGE_PX}px (${JSON.stringify(proof)})`);
  if (proof.room && proof.overlaps) fail(`${label}: floating bar overlaps the selection while space exists (${JSON.stringify(proof)})`);
  if (proof.blame !== 'Blame' || proof.close !== 'Close selection') {
    fail(`${label}: floating actions are not named Blame and Close selection (${JSON.stringify(proof)})`);
  }
  if (proof.question !== 'File question' || !proof.questionVisible) {
    fail(`${label}: Ask question is not the visible File question box (${JSON.stringify(proof)})`);
  }
  if (proof.caption !== `Selection lines ${startLine}–${endLine}` || !proof.captionVisible) {
    fail(`${label}: Ask dock caption is not the visible selected range (${JSON.stringify(proof)})`);
  }
  if (proof.narrow && proof.targets.some((size) => size < 44)) {
    fail(`${label}: narrow floating actions are under 44px (${JSON.stringify(proof.targets)})`);
  }
}

async function assertToolbarAlignment(page, label) {
  const aligned = await page.evaluate(() => {
    const search = document.querySelector('[data-testid="file-explorer-search"]');
    const surface = document.querySelector('.file-explorer__surface');
    const source = Array.from(surface?.querySelectorAll('button') || [])
      .find((node) => (node.textContent || '').trim() === 'Source');
    const treeHidden = document.querySelector('[data-testid="file-explorer-tree"]')?.hidden === true;
    const searchBox = search?.getClientRects().length ? search.getBoundingClientRect() : null;
    if (!surface) return { comparable: true, noSurface: true, treeHidden, search: searchBox != null };
    const sourceBox = source?.getClientRects().length ? source.getBoundingClientRect() : null;
    if (!searchBox || !sourceBox) return { comparable: false, treeHidden, search: searchBox != null, source: sourceBox != null };
    return {
      comparable: true,
      treeHidden,
      top: Math.abs(searchBox.top - sourceBox.top),
      bottom: Math.abs(searchBox.bottom - sourceBox.bottom),
    };
  });
  if (!aligned.comparable) {
    if (aligned.treeHidden) return;
    fail(`${label}: Search and Source toolbars are not both measurable (${JSON.stringify(aligned)})`);
  }
  if (aligned.noSurface) return;
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
  await page.getByRole('button', { name: 'Source', exact: true }).click();
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
    const sourceButton = page.getByRole('button', { name: 'Source', exact: true });
    if (await sourceButton.count()) await sourceButton.click();
    await waitFor(page, () => {
      const code = document.querySelector('pre.file-explorer__code');
      return code != null && code.getClientRects().length > 0;
    }, `files.selection ${leg.label}: source missing`);
    if (leg.contentWidth) {
      const split = await page.locator('[data-testid="file-explorer-layout"]').boundingBox();
      const divider = await page.locator('[data-testid="file-explorer-divider"]').boundingBox();
      if (!split || !divider) fail(`files.selection ${leg.label}: tree divider missing`);
      await page.mouse.move(divider.x + divider.width / 2, divider.y + divider.height / 2);
      await page.mouse.down();
      await page.mouse.move(split.x + split.width * 0.4, divider.y + divider.height / 2);
      await page.mouse.up();
      await twoFrames(page);
      const contentWidth = async () => page.evaluate(() => document.querySelector('[data-testid="file-explorer-content"]')?.getBoundingClientRect().width ?? null);
      let low = 721;
      let high = 1360;
      let width = null;
      let matched = false;
      for (let step = 0; step < 10; step += 1) {
        const candidate = Math.round((low + high) / 2);
        await page.setViewportSize({ width: candidate, height: 900 });
        await twoFrames(page);
        width = await contentWidth();
        if (width == null) fail(`files.selection ${leg.label}: content pane missing at viewport ${candidate}`);
        if (Math.abs(width - leg.contentWidth) <= 1) {
          matched = true;
          break;
        }
        if (width < leg.contentWidth) low = candidate + 1;
        else high = candidate - 1;
      }
      if (!matched) {
        const widths = await page.evaluate(() => ({
          viewport: window.innerWidth,
          layout: document.querySelector('[data-testid="file-explorer-layout"]')?.getBoundingClientRect().width,
          tree: document.querySelector('[data-testid="file-explorer-tree"]')?.getBoundingClientRect().width,
          content: document.querySelector('[data-testid="file-explorer-content"]')?.getBoundingClientRect().width,
          ask: document.querySelector('section.file-conversation')?.getBoundingClientRect().width,
        }));
        fail(`files.selection ${leg.label}: content pane is not ${leg.contentWidth}px ${JSON.stringify(widths)}`);
      }
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
      await page.getByRole('textbox', { name: 'File question' }).click();
      await waitFor(page, ({ startLine: start, endLine: end }) => document.activeElement === document.querySelector('textarea[aria-label="File question"]')
        && (document.querySelector('.file-conversation__context')?.textContent || '') === `Selection lines ${start}\u2013${end}`,
        `${leg.label}: File question did not focus with the selected range`, 25000, { startLine, endLine });
      const question = `files-selection-${leg.label}-${laneNonce || 'lane'}`;
      await sendFileQuestion(page, question, `${leg.label} selection ask`);
      await waitForFixtureReply(page, question, `${leg.label} selection ask`);
      const promptFrames = sentFrames.slice(frameMark).map(parseSentFrame).filter((frame) => frame?.type === 'file_conversation_prompt');
      const prompt = promptFrames.at(-1);
      if (!prompt || prompt.sessionId !== owner || prompt.selection?.startLine !== startLine || prompt.selection?.endLine !== endLine) {
        fail(`${leg.label}: selected prompt is not the exact owner range (${JSON.stringify(prompt)})`);
      }
      if (await sourceButton.count()) await sourceButton.click();
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
      await page.setViewportSize({ width: leg.width, height: leg.height });
      await waitFor(page, (size) => window.innerWidth === size.width && window.innerHeight === size.height, `files.selection ${leg.label}: viewport did not restore`, 10000, leg);
    }
  }
  await page.setViewportSize({ width: 1360, height: 900 });
  await waitFor(page, () => window.innerWidth === 1360 && window.innerHeight === 900, 'files.selection fullscreen viewport', 10000);
  if ((await page.locator('section.file-explorer').count()) === 0) await openFileExplorer(page);
  await clickFilesRow(page, solFile);
  const fullscreenSource = page.getByRole('button', { name: 'Source', exact: true });
  if (await fullscreenSource.count()) await fullscreenSource.click();
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

// files.map: MapRoot.sol is the open alpha file. Search, then the tree,
// selects directory beta. The request path is beta and painted nodes are
// only beta. The relation list shows the fixture import arrow and source
// purpose. A held alpha response after beta or a workspace switch paints nothing.
async function runFileMapJourney(page, sentFrames, receivedFrames) {
  await page.setViewportSize({ width: 1280, height: 900 });
  await waitFor(page, () => window.innerWidth === 1280 && window.innerHeight === 900, 'files.map desktop viewport', 10000);
  if ((await page.locator('section.file-explorer').count()) === 0) await openFileExplorer(page);
  const leftoverSelection = page.getByRole('button', { name: 'Close selection', exact: true });
  if (await leftoverSelection.count()) await leftoverSelection.click();
  await expandFilesTree(page);
  await clickFilesRow(page, solMapRootFile);
  const mapToggle = page.getByRole('button', { name: 'Graph' });
  await waitFor(
    page,
    () => {
      const control = document.querySelector('[data-testid="file-explorer-map-toggle"]');
      return control != null && control.getAttribute('aria-label') === 'Graph' && control.getBoundingClientRect().width > 0;
    },
    'files.map: Map surface control is not visible for the file tab',
    25000,
  );
  const rootMapMark = sentFrames.length;
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
    const text = svg?.textContent || '';
    return {
      svg: svg != null && svg.getBoundingClientRect().width > 0,
      root: text.includes(rootPath),
      dep: text.includes(depPath),
      caller: text.includes(callerPath),
      edge: text.includes('import'),
      interactiveNodes: document.querySelector('[data-map-node-path]') != null,
      svgNoScript: svg?.querySelector('script, foreignObject') == null,
    };
  }, { rootPath: solMapRootFile, depPath: solMapDepFile, callerPath: solMapCallerFile });
  if (!mapProof.svg || mapProof.svgNoScript === false || !mapProof.root || !mapProof.dep || !mapProof.caller || !mapProof.edge || mapProof.interactiveNodes) {
    fail(`files.map: rendered graph does not name the fixture files and import edges (${JSON.stringify(mapProof)})`);
  }
  await laneShot(page, 'files-map-root.png', { fullPage: true });
  await page.getByRole('button', { name: 'View', exact: true }).click();
  await waitFor(page, () => document.querySelector('.mermaid-viewer:not([hidden]) svg') != null, 'files.map: View did not open the diagram', 10000);
  const viewerSafe = await page.evaluate(() => document.querySelector('.mermaid-viewer svg')?.querySelector('script, foreignObject') == null);
  if (!viewerSafe) fail('files.map: viewer SVG carries script/foreignObject');
  const downloadPromise = page.waitForEvent('download');
  await page.locator('.mermaid-viewer').getByRole('button', { name: 'Download', exact: true }).click();
  const download = await downloadPromise;
  if (download.suggestedFilename() !== 'mermaid-diagram.svg') fail(`files.map: download name is ${download.suggestedFilename()}`);
  const saved = fs.readFileSync(await download.path(), 'utf8');
  if (!saved.includes('<svg') || /<script|<foreignObject/i.test(saved)) fail('files.map: downloaded SVG is not a script-free diagram');
  await page.locator('.mermaid-viewer').getByRole('button', { name: 'Close', exact: true }).click();
  const selectedMapOverlay = await page.evaluate(() => document.querySelector('[data-testid="file-selection-actions"]') != null);
  if (selectedMapOverlay) fail('files.map: the selected map shows a source selection overlay');
  await page.getByRole('button', { name: 'Close dependency graph' }).click();
  await clickFilesRow(page, solMapRootFile);
  await page.evaluate(() => window.__rpiWarmReconnect.holdNextFileExplorerMap());
  await mapToggle.click();
  await waitFor(page, () => window.__rpiWarmReconnect.heldMapCount() > 0, 'files.map: the stale leg never dispatched file_explorer_map', 15000);
  const heldRootId = await page.evaluate(() => window.__rpiWarmReconnect.heldMapIds()[0] || '');
  await page.getByRole('button', { name: 'Close dependency graph' }).click();
  await clickFilesRow(page, cleanFile);
  const releasedRoot = await page.evaluate(() => window.__rpiWarmReconnect.releaseHeldFileExplorerMap());
  await twoFrames(page);
  const rootLeak = await page.evaluate((path) => ({
    active: document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || '',
    map: document.querySelector('[data-testid="file-dependency-map"]') != null,
  }), cleanFile);
  if (rootLeak.active !== cleanFile || rootLeak.map || !releasedRoot.includes(heldRootId)) fail(`files.map: stale root map painted on the clean file (${JSON.stringify(rootLeak)})`);
  await clickFilesRow(page, solMapRootFile);
  const search = page.locator('[data-testid="file-explorer-search"]');
  await search.fill('beta');
  await waitFor(
    page,
    () => [...document.querySelectorAll('[data-testid="file-explorer-search-results"] button.file-explorer__row')].some((row) => (row.querySelector('.file-explorer__row-path')?.textContent || '') === 'beta'),
    'files.map: search did not list directory beta',
    15000,
  );
  await page.locator('[data-testid="file-explorer-search-results"] button.file-explorer__row').filter({ hasText: 'beta' }).first().click();
  await waitFor(page, () => document.querySelector('[data-testid="file-explorer-search-results"]') == null, 'files.map: selecting beta did not leave search', 10000);
  await clickFilesRow(page, 'beta');
  const mapMark = sentFrames.length;
  await mapToggle.click();
  try {
    await waitFor(
      page,
      () => document.querySelector('[data-testid="file-dependency-map"]')?.getAttribute('data-map-path') === 'beta',
      'files.map: beta graph did not paint',
      30000,
    );
  } catch (error) {
    const request = sentFrames.slice(mapMark).map(parseSentFrame).find((frame) => frame?.type === 'file_explorer_map') || null;
    const response = request ? receivedFrames.map(parseSentFrame).find((frame) => frame?.type === 'response' && frame.id === request.id) : null;
    const data = response?.data || {};
    error.mapRequest = request ? { id: request.id || '', path: request.path || '', sessionId: request.sessionId || '' } : null;
    error.mapResponse = response ? { id: response.id || '', success: response.success === true, path: data.path || '', nodeCount: Array.isArray(data.nodes) ? data.nodes.length : 0, error: data.error || response.error || '' } : null;
    error.mapHold = await page.evaluate(() => ({ armed: window.__rpiWarmReconnect?.state?.arm?.fileExplorerMap === true, held: window.__rpiWarmReconnect?.heldMapCount?.() ?? null })).catch(() => null);
    throw error;
  }
  const betaRequest = sentFrames.slice(mapMark).map(parseSentFrame).find((frame) => frame?.type === 'file_explorer_map');
  if (!betaRequest || betaRequest.path !== 'beta') fail(`files.map: request path is not beta (${JSON.stringify(betaRequest)})`);
  const betaProof = await page.evaluate(() => {
    const host = document.querySelector('[data-testid="file-dependency-map"]');
    const relations = document.querySelector('[data-testid="file-dependency-map-relations"]')?.textContent || '';
    const nodes = [...(host?.querySelectorAll('svg .node text, svg .nodeLabel') || [])].map((node) => (node.textContent || '').trim()).filter(Boolean);
    return { path: host?.getAttribute('data-map-path') || '', nodes, relations };
  });
  const labels = [...betaProof.nodes].sort();
  if (betaProof.path !== 'beta' || labels.join('|') !== 'beta.ts|lib.ts') {
    fail(`files.map: painted node labels are not exactly beta.ts and lib.ts (${JSON.stringify(betaProof.nodes)})`);
  }
  if (!betaProof.relations.includes('beta.ts \u2192 lib.ts via import') || !betaProof.relations.includes('beta.ts purpose') || !betaProof.relations.includes('Beta ledger owned by the directory graph.')) {
    fail(`files.map: relations omit the exact import or source purpose (${JSON.stringify(betaProof.relations)})`);
  }
  await laneShot(page, 'files-map-beta-desktop.png', { fullPage: false });
  const rootDirectory = sentFrames.slice(rootMapMark).map(parseSentFrame).find((frame) => frame?.type === 'file_explorer_map')?.path;
  if (typeof rootDirectory !== 'string' || rootDirectory === 'beta') fail(`files.map: root directory request was not captured (${JSON.stringify(rootDirectory)})`);
  await page.getByRole('button', { name: 'Close dependency graph' }).click();
  await clickFilesRow(page, solMapRootFile);
  await page.evaluate(() => window.__rpiWarmReconnect.holdNextFileExplorerMap());
  await mapToggle.click();
  await waitFor(page, () => window.__rpiWarmReconnect.heldMapCount() > 0, 'files.map: old root response was not held', 15000);
  const heldMapId = await page.evaluate(() => window.__rpiWarmReconnect.heldMapIds()[0] || '');
  await page.getByRole('button', { name: 'Close dependency graph' }).click();
  await page.locator('[data-testid="file-explorer-search"]').fill('beta');
  await page.locator('[data-testid="file-explorer-search-results"] button.file-explorer__row').filter({ hasText: 'beta' }).first().click();
  await mapToggle.click();
  await waitFor(page, () => {
    const host = document.querySelector('[data-testid="file-dependency-map"]');
    return host?.getAttribute('data-map-path') === 'beta' && host.querySelector('svg')?.getBoundingClientRect().width > 0;
  }, 'files.map: fresh beta graph did not settle before the old release', 30000);
  const before = await page.evaluate(() => document.querySelector('[data-testid="file-dependency-map"]')?.textContent || '');
  if (!before.includes('beta.ts') || !before.includes('lib.ts')) fail('files.map: settled beta graph is empty');
  const released = await page.evaluate(() => window.__rpiWarmReconnect.releaseHeldFileExplorerMap());
  await waitFor(page, (text) => (document.querySelector('[data-testid="file-dependency-map"]')?.textContent || '') === text, 'files.map: held root response changed the settled beta graph', 10000, before);
  if (!heldMapId || !released.includes(heldMapId)) fail(`files.map: held ${rootDirectory} response was not released`);
  await revealSessionRail(page, 'map workspace switch');
  const other = await createFreshSessionViaManage(page, waitFor, 'map workspace switch', { closePanel: true, timeoutMs: 20000 });
  await openFileExplorer(page);
  await clickFilesRow(page, 'beta');
  await mapToggle.click();
  await waitFor(page, () => document.querySelector('[data-testid="file-dependency-map"]')?.getAttribute('data-map-path') === 'beta', 'files.map: second workspace beta graph missing', 30000);
  await revealSessionRail(page, 'map original owner');
  await page.locator(`.session-sidebar__switch[data-session-id="${other.beforeSessionId}"]`).click();
  await waitFor(page, (id) => document.getElementById('session-lifecycle')?.dataset.displayedSessionId === id, 'files.map: original workspace did not return', 20000, other.beforeSessionId);
  const returned = await displayedExplorerSession(page);
  if (returned !== other.beforeSessionId) fail(`files.map: original owner did not return (${returned})`);
  const mapWasWide = await page.evaluate(() => window.innerWidth > 720);
  await page.setViewportSize({ width: 390, height: 844 });
  if (mapWasWide) {
    await waitFor(page, () => window.matchMedia('(max-width: 720px)').matches && document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') !== true, 'files.map mobile sessions drawer did not settle closed');
  }
  await openFileExplorer(page);
  await clickFilesRow(page, 'beta');
  await page.getByRole('button', { name: 'Graph' }).click();
  await waitFor(page, () => {
    const svg = document.querySelector('[data-testid="file-explorer-map-pane"] svg');
    const box = svg?.getBoundingClientRect();
    return box && box.width > 0 && box.right <= window.innerWidth + 1 && document.documentElement.scrollWidth <= document.documentElement.clientWidth + 1;
  }, 'files.map: mobile beta SVG is not visible inside the viewport', 30000);
  await waitFor(page, () => document.querySelector('[data-testid="file-dependency-map"]')?.getAttribute('data-map-path') === 'beta', 'files.map: mobile beta graph missing', 30000);
  await laneShot(page, 'files-map-beta-mobile.png', { fullPage: false });
  await page.getByRole('button', { name: 'Close dependency graph' }).click();
  await page.setViewportSize({ width: 1280, height: 900 });
  await clickFilesRow(page, cleanFile);
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
    path: document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || '',
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
    askBoundTo,
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
    await waitFor(page, askBoundTo, 'files.ask: Ask did not stay bound to the long source', 15000, markdownFile);
  }, async () => {
    await page.getByRole('button', { name: 'Source', exact: true }).click();
    await waitFor(page, (file) => {
      const dock = document.querySelector('section.file-conversation');
      const active = document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || '';
      return dock?.getAttribute('aria-label') === 'File conversation' && (dock.getAttribute('data-session-id') || '') !== '' && active === file && document.querySelector('pre.file-explorer__code') != null;
    }, 'files.ask: Source left the selected file conversation', 15000, markdownFile);
  });
  await clickFilesRow(page, solFile);
  await waitFor(page, askBoundTo, 'files.ask: Token conversation did not return before the selected range', 15000, solFile);

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
    (file) => {
      const dock = document.querySelector('section.file-conversation');
      const active = document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || '';
      return dock?.getAttribute('aria-label') === 'File conversation' && (dock.getAttribute('data-session-id') || '') !== '' && active === file && (dock.getAttribute('data-conversation-id') || '') === '' && document.querySelector('pre.file-explorer__code') != null;
    },
    `files.ask: switching to ${cleanFile} kept the first conversation`,
    15000,
    cleanFile
  );
  const secondMark = sentFrames.length;
  await clickVisibleAsk(page, 'second file');
  const secondQuestion = `files-second-${laneNonce || 'lane'}-clean`;
  await sendFileQuestion(page, secondQuestion, 'second file');
  await waitFor(
    page,
    askBoundTo,
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
    (want) => {
      const dock = document.querySelector('section.file-conversation');
      const active = document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || '';
      return dock?.getAttribute('aria-label') === 'File conversation' && active === want.path && Array.from(document.querySelectorAll('.side-chat__entry--user')).some((entry) => (entry.textContent || '').trimStart().startsWith(want.question)) && !Array.from(document.querySelectorAll('.side-chat__entry--user')).some((entry) => (entry.textContent || '').includes(want.second));
    },
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
    (want) => {
      const dock = document.querySelector('section.file-conversation');
      const active = document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || '';
      return dock?.getAttribute('aria-label') === 'File conversation' && active === want.path && dock?.getAttribute('data-conversation-id') === want.id && Array.from(document.querySelectorAll('.side-chat__entry--user')).some((entry) => (entry.textContent || '').trimStart().startsWith(want.question));
    },
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



async function assertParentChildBlame(page, sentFrames, owner) {
  if (!/^[0-9a-f]{40}$/.test(childCommit)) fail(`parent blame: child commit missing (${JSON.stringify(childCommit)})`);
  await page.setViewportSize({ width: 1280, height: 900 });
  if ((await page.locator('section.file-explorer').count()) === 0) await openFileExplorer(page);
  await clickFilesDir(page, 'psy-node');
  await clickFilesRow(page, 'psy-node/child.rs');
  const source = page.getByRole('button', { name: 'Source', exact: true });
  if (await source.count()) await source.click();
  await waitFor(
    page,
    (want) => document.querySelector('#file-explorer-line-1 .file-explorer__line-text')?.textContent === want,
    'parent blame: child.rs source line 1 did not render',
    25000,
    '/// Child ledger owned by the parent-cwd session.',
  );
  await selectSourceLines(page, 1, 1, 'parent child blame');
  const mark = sentFrames.length;
  await page.getByRole('button', { name: 'Blame', exact: true }).click();
  await waitFor(page, () => document.querySelector('[data-testid="file-selection-blame-list"] [data-line="1"]') != null, 'parent blame: child row missing', 15000);
  const frame = sentFrames.slice(mark).map(parseSentFrame).find((item) => item?.type === 'file_explorer_blame');
  if (!frame || frame.path !== 'psy-node/child.rs' || frame.sessionId !== owner) fail(`parent blame request is not the child path (${JSON.stringify(frame)})`);
  const row = await page.evaluate(() => {
    const line = document.querySelector('[data-testid="file-selection-blame-list"] [data-line="1"]');
    return {
      commit: line?.querySelector('span.file-selection-actions__commit')?.getAttribute('title') || '',
      author: line?.querySelector('span.file-selection-actions__author')?.textContent || '',
      source: line?.querySelector('span.file-selection-actions__text')?.textContent || '',
    };
  });
  if (row.commit !== childCommit || row.author !== 'Web E2E' || row.source !== '/// Child ledger owned by the parent-cwd session.') {
    fail(`parent blame row is not the synthetic child commit (${JSON.stringify(row)})`);
  }
  await laneShot(page, 'files-selection-parent-child-blame.png', { fullPage: false });
  await assertSelectionDismiss(page, 'parent child blame');
  await hideFileExplorer(page, 'parent child blame');
}
function keepNonGitShot(name) {
  const dir = process.env.RPI_FAILURE_EVIDENCE_DIR;
  if (!dir) throw new Error(`files.nongit: RPI_FAILURE_EVIDENCE_DIR is not set; cannot retain ${name}`);
  fs.mkdirSync(dir, { recursive: true });
  fs.copyFileSync(path.join(stagingDir, name), path.join(dir, name));
}

async function runNonGitExplorerJourney(page, owner) {
  const label = 'files.nongit';
  if (!owner) fail(`${label}: parent non-repo session id is empty`);
  const residue = page.locator('#toasts .toast--error');
  const residueTexts = await residue.allTextContents();
  if (residueTexts.some((text) => !text.includes('code review refresh targets a closed review'))) {
    fail(`${label}: unexpected error toast before the journey (${JSON.stringify(residueTexts)})`);
  }
  while (await residue.count()) {
    const before = await residue.count();
    await residue.first().click();
    await waitFor(page, (beforeArg) => document.querySelectorAll('#toasts .toast--error').length < beforeArg, `${label}: error toast refused to dismiss`, 5000, before);
  }
  await waitFor(page, () => document.querySelectorAll('#toasts .toast').length === 0, `${label}: toasts did not clear after dismissal`);
  await page.setViewportSize({ width: 1360, height: 900 });
  await waitFor(page, () => window.innerWidth === 1360 && window.innerHeight === 900, `${label}: desktop viewport`, 10000);
  if ((await page.locator('section.file-explorer').count()) === 0) await openFileExplorer(page);
  await expandFilesTree(page);
  await clickFilesDir(page, 'elsewhere');
  await clickFilesRow(page, 'elsewhere/rewards.rs');
  const source = page.getByRole('button', { name: 'Source', exact: true });
  if (await source.count()) await source.click();
  await waitFor(page, () => document.querySelector('pre.file-explorer__code')?.getClientRects().length > 0, `${label}: real source did not open`);
  const surface = await page.evaluate(() => ({
    body: document.querySelector('section.file-explorer')?.textContent || '',
    code: document.querySelector('pre.file-explorer__code')?.textContent || '',
    rows: document.querySelectorAll('button.file-explorer__row').length,
  }));
  if (!surface.code.includes('OUTSIDE-CHILD-REWARDS')) fail(`${label}: real non-git file body did not render (${JSON.stringify(surface.code.slice(0, 80))})`);
  if (surface.rows < 1) fail(`${label}: no tree rows rendered for the non-git workspace`);
  if (surface.body.includes('Not a git repository')) fail(`${label}: retired non-git prose rendered`);
  await laneShot(page, 'files-nongit-desktop.png', { fullPage: false });
  keepNonGitShot('files-nongit-desktop.png');
  await page.setViewportSize({ width: 390, height: 844 });
  await waitFor(page, () => window.innerWidth === 390 && window.innerHeight === 844, `${label}: mobile viewport`, 10000);
  await waitFor(page, () => {
    const files = document.querySelector('section.file-explorer[aria-label="File explorer"]');
    return files !== null && document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') !== true;
  }, `${label}: Files open must close the sessions drawer`, 10000);
  await expandFilesTree(page);
  const treeCoversSource = () => page.evaluate(() => {
    const code = document.querySelector('pre.file-explorer__code');
    return code == null || code.getClientRects().length === 0
      || (code.getBoundingClientRect().width < 40);
  });
  if (await treeCoversSource()) {
    await page.locator('#file-explorer-tree-toggle').click();
    await waitFor(page, () => {
      const code = document.querySelector('pre.file-explorer__code');
      return code != null && code.getClientRects().length > 0 && code.getBoundingClientRect().width >= 40;
    }, `${label}: source pane did not return after tree collapse`, 10000);
  }
  const mobile = await page.evaluate(() => {
    const code = document.querySelector('pre.file-explorer__code');
    const rect = code?.getBoundingClientRect();
    return {
      body: document.querySelector('section.file-explorer')?.textContent || '',
      sourceVisible: code != null && code.getClientRects().length > 0 && (rect?.width ?? 0) >= 40,
      sourceText: code?.textContent || '',
    };
  });
  if (mobile.body.includes('Not a git repository')) fail(`${label}: mobile retired non-git prose rendered`);
  if (!mobile.sourceVisible || !mobile.sourceText.includes('OUTSIDE-CHILD-REWARDS')) {
    fail(`${label}: mobile visible source missing the real file body (${JSON.stringify({ sourceVisible: mobile.sourceVisible, code: mobile.sourceText.slice(0, 80) })})`);
  }
  await laneShot(page, 'files-nongit-mobile.png', { fullPage: false });
  keepNonGitShot('files-nongit-mobile.png');
  await page.setViewportSize({ width: 1360, height: 900 });
  await waitFor(page, () => window.innerWidth === 1360 && window.innerHeight === 900, `${label}: desktop viewport restore`, 10000);
  await hideFileExplorer(page, label);
}


function treePctNow() {
  const raw = getComputedStyle(document.querySelector('[data-testid="file-explorer-layout"]')).getPropertyValue('--file-explorer-tree-pct');
  return Number.parseFloat(raw);
}

async function assertFilesMaxLayout(page, label) {
  const expand = await page.locator('#file-explorer-expand-btn, button.file-explorer__expand-btn').count();
  if (expand !== 0) fail(`${label}: expand control is still mounted`);
  const box = await page.evaluate(() => {
    const header = document.querySelector('header')?.getBoundingClientRect();
    const footerNode = document.querySelector('footer:has(#composer-row), footer:has(#prompt-input)');
    const footerStyle = footerNode ? getComputedStyle(footerNode) : null;
    const explorer = document.querySelector('section.file-explorer')?.getBoundingClientRect();
    const visible = (rect) => rect && rect.width > 0 && rect.height > 0;
    const round = (rect) => rect ? { x: Math.round(rect.x), y: Math.round(rect.y), w: Math.round(rect.width), h: Math.round(rect.height) } : null;
    const styleOf = (selector) => {
      const node = document.querySelector(selector);
      if (!node) return null;
      const style = getComputedStyle(node);
      return { position: style.position, top: style.top, bottom: style.bottom, height: style.height, maxHeight: style.maxHeight, margin: style.margin };
    };
    const root = getComputedStyle(document.documentElement);
    return {
      header: visible(header) && header.top >= -1 && header.bottom > 0,
      footer: !!footerStyle && footerStyle.display === 'none' && root.getPropertyValue('--app-footer-height').trim() === '0px',
      left: explorer ? Math.round(explorer.left) : null,
      right: explorer ? Math.round(window.innerWidth - explorer.right) : null,
      top: explorer && header ? Math.round(explorer.top - header.bottom) : null,
      bottom: explorer ? Math.round(window.innerHeight - explorer.bottom) : null,
      width: explorer ? Math.round(explorer.width) : 0,
      rects: { header: round(header), footer: round(footerNode?.getBoundingClientRect() ?? null), explorer: round(explorer) },
      styles: { header: styleOf('header'), footer: styleOf('footer:has(#composer-row), footer:has(#prompt-input)'), explorer: styleOf('section.file-explorer') },
      vars: { header: root.getPropertyValue('--app-header-height').trim(), footer: root.getPropertyValue('--app-footer-height').trim() },
    };
  });
  if (!box.header || !box.footer) fail(`${label}: header hidden or composer footer VISIBLE — Files-open must hide the whole footer at 0px (${JSON.stringify(box)})`);
  if (box.left !== 0 || box.right !== 0 || Math.abs(box.top) > 1 || Math.abs(box.bottom) > 1 || Math.abs(box.width - (await page.evaluate(() => window.innerWidth))) > 2) {
    fail(`${label}: explorer is not the full app body (${JSON.stringify(box)})`);
  }
}


async function runFilesFullscreenJourney(page) {
  const legs = [
    { width: 1280, height: 900, label: 'desktop' },
    { width: 390, height: 844, label: 'mobile' },
  ];
  for (const leg of legs) {
    await page.setViewportSize(leg);
    await waitFor(page, (size) => window.innerWidth === size.width && window.innerHeight === size.height, `${leg.label} layout viewport`, 10000, leg);
    if ((await page.locator('section.file-explorer').count()) === 0) await openFileExplorer(page);
    const draft = `layout-draft-${leg.label}`;
    await page.locator('textarea.file-conversation__input').fill(draft);
    const tab = await page.evaluate(() => document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || '');
    await waitFor(page, () => {
      const header = document.querySelector('header')?.getBoundingClientRect();
      const footer = document.querySelector('footer:has(#composer-row), footer:has(#prompt-input)')?.getBoundingClientRect();
      const root = getComputedStyle(document.documentElement);
      return header && footer && Math.abs(header.height - Number.parseFloat(root.getPropertyValue('--app-header-height'))) <= 1 && Math.abs(footer.height - Number.parseFloat(root.getPropertyValue('--app-footer-height'))) <= 1;
    }, `${leg.label}: app chrome variables did not match the live rectangles`, 1000);
    await assertFilesMaxLayout(page, leg.label);
    if (leg.label === 'desktop') {
      const owner = await displayedExplorerSession(page);
      await revealSessionRail(page, 'fresh tree default');
      const freshSession = await createFreshSessionViaManage(page, waitFor, 'fresh tree default', { closePanel: true, timeoutMs: 20000 });
      await openFileExplorer(page);
      if ((await page.evaluate(treePctNow)) !== 29) fail('fresh owner tree percent is not 29');
      await page.locator(FILES_DRAWER_CLOSE).click();
      await openFileExplorer(page);
      if ((await page.evaluate(treePctNow)) !== 29) fail('fresh owner lost the default 29 across reopen');
      await revealSessionRail(page, 'layout original owner');
      await page.locator(`.session-sidebar__switch[data-session-id="${owner}"]`).click();
      await waitFor(page, (id) => document.getElementById('session-lifecycle')?.dataset.displayedSessionId === id, 'layout owner did not return', 20000, owner);
      await openFileExplorer(page);
      const divider = page.locator('[data-testid="file-explorer-divider"]');
      await divider.focus();
      await page.keyboard.press('Home');
      for (let step = 0; step < 4; step += 1) await page.keyboard.press('ArrowRight');
      if ((await page.evaluate(treePctNow)) !== 30) fail('desktop: custom tree percent is not 30');
      if (freshSession.sessionId === owner) fail('fresh tree session did not change owner');
      await page.locator(FILES_DRAWER_CLOSE).click();
      await openFileExplorer(page);
      if ((await page.evaluate(treePctNow)) !== 30) fail('desktop: reopen lost custom tree percent 30');
    }
    await page.locator(FILES_DRAWER_CLOSE).click();
    await openFileExplorer(page);
    const restored = await page.evaluate((want) => ({
      tab: document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || '',
      draft: document.querySelector('textarea.file-conversation__input')?.value || '',
    }), tab);
    if (restored.tab !== tab || restored.draft !== draft) fail(`${leg.label}: close/reopen lost tab or draft (${JSON.stringify(restored)})`);
    const surfaces = await page.evaluate(() => {
      const source = document.querySelector('[data-testid="file-explorer-content"]');
      const ask = document.querySelector('section.file-conversation');
      const body = document.querySelector('[data-testid="file-explorer-layout"]');
      const sourceBox = source?.getBoundingClientRect();
      const askBox = ask?.getBoundingClientRect();
      const bodyBox = body?.getBoundingClientRect();
      const style = body ? getComputedStyle(body) : null;
      const pad = style ? Number.parseFloat(style.paddingTop) + Number.parseFloat(style.paddingBottom) + Number.parseFloat(style.borderTopWidth) + Number.parseFloat(style.borderBottomWidth) : 0;
      const contentTop = bodyBox ? bodyBox.top + Number.parseFloat(style.paddingTop) + Number.parseFloat(style.borderTopWidth) : 0;
      const contentBottom = bodyBox ? bodyBox.bottom - Number.parseFloat(style.paddingBottom) - Number.parseFloat(style.borderBottomWidth) : 0;
      const inside = (box) => box && box.width > 0 && box.height > 0 && box.left >= -1 && box.right <= window.innerWidth + 1 && box.bottom <= window.innerHeight + 1;
      const input = document.querySelector('textarea.file-conversation__input');
      const send = document.querySelector('section.file-conversation .file-conversation__action--send');
      const inputBox = input?.getBoundingClientRect();
      const sendBox = send?.getBoundingClientRect();
      const point = inputBox ? document.elementFromPoint(inputBox.left + inputBox.width / 2, inputBox.top + inputBox.height / 2) : null;
      return {
        source: inside(sourceBox) && source.querySelector('pre.file-explorer__code, .file-explorer__markdown') != null,
        ask: inside(askBox),
        askFills: askBox && Math.abs(askBox.top - contentTop) <= 1 && Math.abs(askBox.bottom - contentBottom) <= 1 && askBox.height >= bodyBox.height - pad - 1,
        askTarget: inputBox && inputBox.width >= 44 && inputBox.height >= 44 && point?.closest('textarea.file-conversation__input') != null && sendBox && sendBox.width >= 44 && sendBox.height >= 44,
        overflow: document.documentElement.scrollWidth <= document.documentElement.clientWidth + 1,
      };
    });
    if (leg.label === 'desktop' && (!surfaces.source || !surfaces.askFills)) {
      const error = new Error(`desktop: Ask does not fill the body beside a visible source (${JSON.stringify(surfaces)})`);
      error.layoutGeometry = await page.evaluate(() => {
        const nodes = {
          body: document.querySelector('[data-testid="file-explorer-layout"]'),
          source: document.querySelector('pre.file-explorer__code, .file-explorer__markdown'),
          ask: document.querySelector('section.file-conversation'),
        };
        const measure = (node) => {
          if (!node) return null;
          const rect = node.getBoundingClientRect();
          const style = getComputedStyle(node);
          const chain = [];
          for (let current = node; current && chain.length < 4; current = current.parentElement) {
            chain.push(`${current.tagName}.${String(current.className || '').split(' ').filter(Boolean).slice(0, 3).join('.')}`);
          }
          return {
            chain,
            rect: { x: Math.round(rect.x), y: Math.round(rect.y), w: Math.round(rect.width), h: Math.round(rect.height) },
            flex: style.flexBasis,
            height: style.height,
            minHeight: style.minHeight,
            maxHeight: style.maxHeight,
            padding: style.padding,
          };
        };
        return {
          body: measure(nodes.body),
          source: measure(nodes.source),
          ask: measure(nodes.ask),
          scrollWidth: document.documentElement.scrollWidth,
          clientWidth: document.documentElement.clientWidth,
          layout: nodes.body?.getAttribute('data-layout') || '',
          tree: nodes.body ? getComputedStyle(nodes.body).getPropertyValue('--file-explorer-tree-pct').trim() : '',
        };
      });
      throw error;
    }
    if (leg.label === 'mobile' && (!surfaces.source || !surfaces.ask || !surfaces.askTarget || !surfaces.overflow)) fail(`mobile: source and Ask are not both reachable (${JSON.stringify(surfaces)})`);
    await laneShot(page, `files-layout-${leg.label}.png`, { fullPage: false });
  }
  await page.setViewportSize({ width: 1360, height: 900 });
  await hideFileExplorer(page, 'layout restore before mobile');
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
          if (!socket) return [];
          const released = [];
          for (const frame of heldFileExplorerMap.values()) {
            if (!frame) continue;
            releasing.add(frame.id);
            socket.dispatchEvent(new MessageEvent('message', { data: JSON.stringify(frame) }));
            releasing.delete(frame.id);
            released.push(frame.id);
          }
          heldFileExplorerMap.clear();
          return released;
        },
        heldMapCount() {
          return Array.from(heldFileExplorerMap.values()).filter((frame) => frame !== null).length;
        },
        heldMapIds() {
          return Array.from(heldFileExplorerMap.keys());
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
      if (message.type() === 'error' && !isKnownConsoleNoise(message)) consoleErrors.push(message.text());
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

    // The picker lists /compact, /skill, /review (backend authority).
    await openPicker(page);
    await waitFor(
      page,
      () => {
        const names = Array.from(document.querySelectorAll('.command-picker__option .command-picker__name')).map((el) => el.textContent.trim());
        return names.includes('/compact') && names.includes('/skill') && names.includes('/review');
      },
      'command picker did not list /compact, /skill, /review'
    );
    const names = await pickerNames(page);
    for (const required of ['/compact', '/skill', '/review']) {
      if (!names.includes(required)) fail(`command picker missing ${required} (got ${names.join(', ')})`);
    }
    await laneShot(page, 'command-picker-open.png', { fullPage: true });

    // Choose /review -> draft lands in the composer, NO auto-submit.
    await chooseCommand(page, 'review');
    await waitFor(
      page,
      () => {
        const v = document.getElementById('prompt-input')?.value.trim() || '';
        return v.startsWith('/review');
      },
      'choosing /review did not insert the draft into the composer'
    );
    // No auto-submit: the panel must NOT have opened yet, and no user bubble
    // for the slash command appears in the transcript.
    const panelBefore = await page.evaluate(() => document.getElementById('code-review-panel') !== null);
    if (panelBefore) fail('/review auto-submitted — panel opened without Enter');
    await laneShot(page, 'code-review-draft-no-submit.png', { fullPage: true });

    // Enter dispatches code_review_open -> the real review panel renders.
    // Pre-open baseline for the close-restore contract asserted after the
    // geometry checks below (scroll, composer rect, focus = prompt-input).
    const crLayoutBeforeDesktop = await crAppLayout(page);
    // The inline completion popup owns the SELECTING Enter and never
    // dispatches: /review is settled by that Enter, and the helper presses
    // the INDEPENDENT dispatch Enter that actually opens the panel.
    await selectCompletionThenSubmit(page, {
      token: '/review',
      label: 'desktop /review open',
      sentFrames,
      dispatchType: 'code_review_open',
    });
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

    // Files-flush shell at 1440, 390, iPad portrait 820, and landscape 1180.
    // Restore 1360 before close so the close-restore rect stays comparable.
    for (const leg of [
      { width: 1440, height: 900, label: 'review-shell-1440' },
      { width: 390, height: 844, label: 'review-shell-390' },
      { width: 820, height: 1180, label: 'review-shell-820' },
      { width: 1180, height: 820, label: 'review-shell-1180' },
    ]) {
      await page.setViewportSize({ width: leg.width, height: leg.height });
      await waitFor(page, (size) => window.innerWidth === size.width && window.innerHeight === size.height, `${leg.label} viewport`, 10000, leg);
      if (leg.width <= 720) {
        await waitFor(page, () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') !== true, `${leg.label} sessions drawer did not settle closed`);
      }
      assertCrShellGeometry(await crShellGeometry(page), leg.label);
    }
    await page.setViewportSize({ width: 1360, height: 900 });
    await waitFor(page, () => window.innerWidth === 1360 && window.innerHeight === 900, 'desktop viewport restored after review shell');
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
    await chooseCommand(page, 'review');
    await selectCompletionThenSubmit(page, {
      token: '/review',
      label: 'desktop /review reopen after close-restore',
      sentFrames,
      dispatchType: 'code_review_open',
    });
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
    await chooseCommand(page, 'review');
    await waitFor(
      page,
      () => (document.getElementById('prompt-input')?.value.trim() || '').startsWith('/review'),
      'thread reload: /review draft missing'
    );
    await selectCompletionThenSubmit(page, {
      token: '/review',
      label: 'thread reload /review reopen',
      sentFrames,
      dispatchType: 'code_review_open',
    });
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
    const composerHit = await page.evaluate(() => {
      const c = document.querySelector('.code-review__composer textarea');
      const r = c?.getBoundingClientRect() || null;
      const hit = r && document.elementFromPoint(r.x + r.width / 2, r.y + Math.min(r.height / 2, Math.max(r.height - 1, 1)));
      return { h: r ? Math.round(r.height) : null, hitInside: !!hit && (hit === c || c.contains(hit)) };
    });
    if (!composerHit.h || composerHit.h < 44 || !composerHit.hitInside) fail(`selected-hunk composer must be its own >=44px hit target under the hidden footer: ${JSON.stringify(composerHit)}`);

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
    await page.fill('#prompt-input', '/review');
    await selectCompletionThenSubmit(page, {
      token: '/review',
      label: 'live-textarea draft gate /review reopen',
      sentFrames,
      dispatchType: 'code_review_open',
    });
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
    if (!parentCwd || !childCommit) fail('parent non-repo fixture env is missing');
    const parentSession = await createFreshSessionViaManage(page, waitFor, 'parent non-repo session', {
      closePanel: true,
      timeoutMs: 20000,
      explicitCwd: parentCwd,
    });
    const parentSid = parentSession.sessionId;
    await openManageSession(page, waitFor, 'parent non-repo cwd');
    await waitFor(
      page,
      (wanted) => document.querySelector('[data-testid="session-cwd-value"]')?.getAttribute('title') === wanted,
      'parent non-repo session cwd did not load',
      20000,
      parentCwd,
    );
    await closeManageSession(page, waitFor, 'parent non-repo cwd');
    await assertParentChildBlame(page, sentFrames, parentSid);
    await runNonGitExplorerJourney(page, parentSid);
    recordFiles('files.nongit');
    const parentOpenMark = sentFrames.length;
    await page.fill('#prompt-input', '/review psy-node');
    await page.press('#prompt-input', 'Enter');
    const parentOpenDeadline = Date.now() + 20000;
    let parentOpen = null;
    let parentOpenResponse = null;
    while (Date.now() < parentOpenDeadline) {
      parentOpen = sentFrames.slice(parentOpenMark).map(parseSentFrame)
        .find((frame) => frame?.type === 'code_review_open' && frame.sessionId === parentSid && frame.repo === 'psy-node') || null;
      parentOpenResponse = parentOpen
        ? receivedFrames.map((raw) => { try { return JSON.parse(raw); } catch { return null; } })
          .find((frame) => frame?.type === 'response' && frame.id === parentOpen.id) || null
        : null;
      if (parentOpen && parentOpenResponse) break;
      await page.waitForTimeout(50);
    }
    if (!parentOpen || !parentOpenResponse || parentOpenResponse.success !== true
      || parentOpenResponse.data?.error != null || parentOpenResponse.data?.repo !== 'psy-node') {
      fail(`parent /review psy-node Enter did not return a null child snapshot error (${JSON.stringify({ parentOpen, parentOpenResponse })})`);
    }
    await waitFor(page, () => document.getElementById('code-review-panel') !== null, 'parent review response did not commit the panel', 20000);
    await waitFor(page, () => [...document.querySelectorAll('.code-review__file')].some((row) => row.getAttribute('data-file-path') === 'changed.txt'), 'parent review did not list changed.txt', 20000);
    await page.evaluate(() => [...document.querySelectorAll('.code-review__file')].find((row) => row.getAttribute('data-file-path') === 'changed.txt')?.click());
    await waitFor(page, () => (document.querySelector('.code-review__diff-path')?.textContent || '').includes('changed.txt'), 'parent review did not render changed.txt', 20000);
    const parentDiff = await page.evaluate(() => document.querySelector('.code-review__diff')?.textContent || '');
    if (!parentDiff.includes('child changed again')) fail('parent review diff is not the owned child fixture');
    await laneShot(page, 'code-review-parent-child-desktop.png', { fullPage: false });
    const reviewCite = 'see `rewards.rs`';
    await page.locator('.code-review__hunk-header').first().click();
    await waitFor(page, () => document.querySelector('.code-review__comment-input') !== null, 'review citation composer did not open', 20000);
    await page.fill('.code-review__comment-input', reviewCite);
    await page.press('.code-review__comment-input', 'Enter');
    await waitFor(
      page,
      () => [...document.querySelectorAll('.code-review__comment--user[data-review-scope="child"][data-review-repo="psy-node"] code.md-code-path')]
        .some((node) => node.getAttribute('data-md-path') === 'rewards.rs' && (node.textContent || '') === 'rewards.rs'),
      'review user comment did not stamp the missing child basename',
      20000,
    );
    const reviewCiteBefore = sentFrames.length;
    await page.locator('.code-review__comment--user[data-review-scope="child"][data-review-repo="psy-node"] code.md-code-path[data-md-path="rewards.rs"]').last().click();
    await waitFor(page, () => [...document.querySelectorAll('.toast--error')].some((toast) => (toast.textContent || '').includes('File not found: rewards.rs')), 'missing review citation did not name rewards.rs', 20000);
    const reviewCiteSent = sentFrames.slice(reviewCiteBefore).map(parseSentFrame);
    const reviewReads = reviewCiteSent.filter((frame) => frame?.type === 'file_explorer_read');
    const reviewSearches = reviewCiteSent.filter((frame) => frame?.type === 'file_explorer_search');
    const reviewStates = reviewCiteSent.filter((frame) => frame?.type === 'get_state');
    const reviewSearchValue = await page.evaluate(() => document.querySelector('[data-testid="file-explorer-search"]')?.value || '');
    const reviewDrawerOpen = await page.evaluate(() => document.querySelector('.file-explorer') !== null);
    if (reviewReads.length !== 1
      || reviewReads[0]?.path !== 'psy-node/rewards.rs'
      || reviewReads[0]?.sessionId !== parentSid
      || reviewSearches.length !== 0
      || reviewStates.length !== 0
      || reviewSearchValue === 'rewards.rs'
      || reviewDrawerOpen) {
      fail(`missing review basename opened a workspace search (${JSON.stringify({ reviewReads, reviewSearches, reviewStates, reviewSearchValue, reviewDrawerOpen })})`);
    }
    await page.evaluate(() => document.querySelectorAll('#toasts .toast').forEach((toast) => toast.click()));
    const parentRefreshMark = sentFrames.length;
    await page.getByRole('button', { name: 'Refresh', exact: true }).click();
    const parentRefreshDeadline = Date.now() + 20000;
    let parentRefresh = null;
    let parentRefreshResponse = null;
    while (Date.now() < parentRefreshDeadline) {
      parentRefresh = sentFrames.slice(parentRefreshMark).map(parseSentFrame)
        .find((frame) => frame?.type === 'code_review_refresh' && frame.sessionId === parentSid) || null;
      parentRefreshResponse = parentRefresh
        ? receivedFrames.map((raw) => { try { return JSON.parse(raw); } catch { return null; } })
          .find((frame) => frame?.type === 'response' && frame.id === parentRefresh.id) || null
        : null;
      if (parentRefresh && parentRefreshResponse) break;
      await page.waitForTimeout(50);
    }
    if (!parentRefresh || !parentRefreshResponse || parentRefreshResponse.success !== true
      || parentRefreshResponse.data?.error != null || parentRefreshResponse.data?.repo !== 'psy-node') {
      fail(`parent review refresh did not return a null child snapshot error (${JSON.stringify({ parentRefresh, parentRefreshResponse })})`);
    }
    await waitFor(page, () => [...document.querySelectorAll('.code-review__file')].some((row) => row.getAttribute('data-file-path') === 'changed.txt'), 'parent refresh response did not keep changed.txt', 20000);
    await page.setViewportSize({ width: 390, height: 844 });
    await waitFor(page, () => window.innerWidth === 390 && window.innerHeight === 844, 'parent review mobile viewport');
    await waitFor(page, () => [...document.querySelectorAll('.code-review__file')].some((row) => row.getAttribute('data-file-path') === 'changed.txt'), 'parent review mobile file list lost changed.txt', 20000);
    await laneShot(page, 'code-review-parent-child-mobile.png', { fullPage: false });
    await page.setViewportSize({ width: 1360, height: 900 });
    await page.click('button.code-review__close');
    await waitFor(page, () => document.getElementById('code-review-panel') === null, 'parent review panel did not close');
    await page.evaluate(() => {
      document.querySelectorAll('#toasts .toast').forEach((toast) => toast.click());
    });
    await page.setViewportSize({ width: 1360, height: 900 });
    await waitFor(page, () => window.innerWidth === 1360 && window.innerHeight === 900, 'ancestor review desktop viewport');
    if (!await page.evaluate(() => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true)) {
      await openSessionsDrawer(page);
    }
    await waitFor(page, () => {
      const button = document.getElementById('sidebar-manage-btn');
      return button !== null && button.getClientRects().length > 0;
    }, 'ancestor review sidebar manage control did not become visible', 20000);
    const nestedSession = await createFreshSessionViaManage(page, waitFor, 'ancestor review session', {
      closePanel: true,
      timeoutMs: 20000,
      explicitCwd: `${parentCwd}/psy-node/session-nested`,
    });
    const nestedSid = nestedSession.sessionId;
    const ancestorOpenMark = sentFrames.length;
    await page.fill('#prompt-input', '/review');
    await selectCompletionThenSubmit(page, {
      token: '/review',
      label: 'ancestor review /review open',
      sentFrames,
      dispatchType: 'code_review_open',
    });
    const ancestorDeadline = Date.now() + 20000;
    let ancestorOpen = null;
    let ancestorResponse = null;
    while (Date.now() < ancestorDeadline) {
      ancestorOpen = sentFrames.slice(ancestorOpenMark).map(parseSentFrame)
        .find((frame) => frame?.type === 'code_review_open' && frame.sessionId === nestedSid && frame.repo === undefined) || null;
      ancestorResponse = ancestorOpen
        ? receivedFrames.map((raw) => { try { return JSON.parse(raw); } catch { return null; } })
          .find((frame) => frame?.type === 'response' && frame.id === ancestorOpen.id) || null
        : null;
      if (ancestorOpen && ancestorResponse) break;
      await page.waitForTimeout(50);
    }
    if (!ancestorResponse || ancestorResponse.success !== true
      || JSON.stringify(ancestorResponse.data?.repo) !== JSON.stringify({ unavailable: true })) {
      fail(`nested /review did not project an unavailable ancestor (${JSON.stringify(ancestorResponse?.data?.repo)})`);
    }
    await waitFor(page, () => [...document.querySelectorAll('.code-review__file')].some((row) => row.getAttribute('data-file-path') === 'changed.txt'), 'ancestor review lost changed.txt', 20000);
    await page.evaluate(() => [...document.querySelectorAll('.code-review__file')].find((row) => row.getAttribute('data-file-path') === 'changed.txt')?.click());
    await waitFor(page, () => (document.querySelector('.code-review__diff')?.textContent || '').includes('child changed again'), 'ancestor diff is not the child root', 20000);
    await page.locator('.code-review__hunk-header').first().click();
    await waitFor(page, () => document.querySelector('.code-review__comment-input') !== null, 'ancestor citation composer did not open', 20000);
    await page.fill('.code-review__comment-input', 'see `changed.txt`');
    await page.press('.code-review__comment-input', 'Enter');
    await waitFor(
      page,
      () => document.querySelector('.code-review__comment--user[data-review-scope="unavailable"] code.md-code-path[data-md-path="changed.txt"]') !== null,
      'ancestor user citation did not stamp unavailable',
      20000,
    );
    const ancestorCiteBefore = sentFrames.length;
    await page.locator('.code-review__comment--user[data-review-scope="unavailable"] code.md-code-path[data-md-path="changed.txt"]').click();
    await page.waitForTimeout(300);
    const ancestorSent = sentFrames.slice(ancestorCiteBefore).map(parseSentFrame);
    const ancestorFileRpc = ancestorSent.filter((frame) => frame?.type === 'file_explorer_read' || frame?.type === 'file_explorer_search' || frame?.type === 'get_state');
    const ancestorDrawer = await page.evaluate(() => document.querySelector('.file-explorer') !== null);
    if (ancestorFileRpc.length !== 0 || ancestorDrawer) {
      fail(`unavailable ancestor citation issued file RPC (${JSON.stringify({ ancestorFileRpc, ancestorDrawer })})`);
    }
    await laneShot(page, 'code-review-ancestor-unavailable-desktop.png', { fullPage: false });
    await page.setViewportSize({ width: 390, height: 844 });
    await page.locator('.code-review__tab', { hasText: 'Thread' }).click();
    await waitFor(page, () => {
      const citation = document.querySelector('.code-review__comment--user[data-review-scope="unavailable"] code.md-code-path[data-md-path="changed.txt"]');
      const box = citation?.getBoundingClientRect();
      return box != null && box.width > 0 && box.height > 0 && box.bottom > 0 && box.top < window.innerHeight;
    }, 'ancestor citation is not visible on the mobile thread', 20000);
    const ancestorMobileBefore = sentFrames.length;
    await page.locator('.code-review__comment--user[data-review-scope="unavailable"] code.md-code-path[data-md-path="changed.txt"]').click();
    await page.waitForTimeout(300);
    const ancestorMobileSent = sentFrames.slice(ancestorMobileBefore).map(parseSentFrame);
    const ancestorMobileRpc = ancestorMobileSent.filter((frame) => frame?.type === 'file_explorer_read' || frame?.type === 'file_explorer_search' || frame?.type === 'get_state');
    const ancestorMobileSurface = await page.evaluate(() => ({
      explorer: document.querySelector('.file-explorer') !== null,
      path: window.location.pathname,
    }));
    if (ancestorMobileRpc.length !== 0 || ancestorMobileSurface.explorer || ancestorMobileSurface.path !== '/web') {
      fail(`mobile unavailable ancestor citation issued file RPC (${JSON.stringify({ ancestorMobileRpc, ancestorMobileSurface })})`);
    }
    await laneShot(page, 'code-review-ancestor-unavailable-mobile.png', { fullPage: false });
    await page.setViewportSize({ width: 1360, height: 900 });
    await page.click('button.code-review__close');
    await page.locator('.code-review__confirm-actions button', { hasText: 'Close panel' }).click();
    await waitFor(page, () => document.getElementById('code-review-panel') === null, 'ancestor review panel did not close');
    await revealSessionRail(page, 'return to parent review session');
    // Selected-only sidebar: the closed parent row is hidden from the
    // default view but search restores catalog matches — query the exact
    // session id (matchSessionSearch matches ids), switch, then clear.
    await page.fill('#session-sidebar-search', parentSid);
    await waitFor(
      page,
      (want) => {
        const row = Array.from(document.querySelectorAll('.session-sidebar__switch')).find(
          (button) => button.dataset.sessionId === want,
        );
        return row !== undefined && row.disabled !== true;
      },
      'return to parent review: sidebar row never became clickable',
      20000,
      parentSid,
    );
    const clicked = await page.evaluate((want) => {
      const row = Array.from(document.querySelectorAll('.session-sidebar__switch')).find(
        (button) => button.dataset.sessionId === want,
      );
      if (!row) return false;
      row.click();
      return true;
    }, parentSid);
    if (!clicked) fail('return to parent review: sidebar row missing');
    await waitFor(page, (target) => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId === target, 'did not return to the parent review session', 20000, parentSid);
    await page.fill('#session-sidebar-search', '');
    await waitFor(page, () => document.querySelectorAll('#toasts .toast').length === 0, 'queued toasts did not clear before the bad commit');
    const badCommit = 'dddddddddddddddddddddddddddddddddddddddd';
    await page.evaluate(() => {
      window.__reviewErrorToasts = [];
      const record = (node) => {
        if (!(node instanceof Element)) return;
        for (const toast of [node, ...node.querySelectorAll('.toast--error')]) {
          if (!toast.matches?.('.toast--error')) continue;
          const message = toast.querySelector('.toast__message')?.textContent || toast.textContent || '';
          window.__reviewErrorToasts.push(message);
        }
      };
      new MutationObserver((records) => {
        for (const recordSet of records) for (const node of recordSet.addedNodes) record(node);
      }).observe(document.body, { childList: true, subtree: true });
    });
    await page.fill('#prompt-input', `/show ${badCommit}`);
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (token) => (window.__reviewErrorToasts || []).filter((message) => message.includes(token)).length >= 1,
      'parent /show bad commit produced no error toast',
      20000,
      badCommit,
    );
    await page.waitForTimeout(800);
    const badCommitToasts = await page.evaluate((token) => [...new Set((window.__reviewErrorToasts || []).filter((message) => message.includes(token)))], badCommit);
    if (badCommitToasts.length !== 1) {
      fail(`parent /show ${badCommit} must surface exactly one error toast (got ${JSON.stringify(badCommitToasts)})`);
    }
    const panelAfterBadCommit = await page.evaluate(() => document.getElementById('code-review-panel') !== null);
    if (panelAfterBadCommit) fail(`parent /show ${badCommit} mounted the code-review panel`);
    await page.fill('#prompt-input', '');

    const parentShowMark = sentFrames.length;
    await page.fill('#prompt-input', `/show ${childCommit}`);
    await page.press('#prompt-input', 'Enter');
    const parentShowDeadline = Date.now() + 20000;
    let childShow = null;
    let childShowResponse = null;
    while (Date.now() < parentShowDeadline) {
      childShow = sentFrames.slice(parentShowMark).map(parseSentFrame)
        .find((frame) => frame?.type === 'code_review_open' && frame.sessionId === parentSid && frame.commit === childCommit) || null;
      childShowResponse = childShow
        ? receivedFrames.map((raw) => { try { return JSON.parse(raw); } catch { return null; } })
          .find((frame) => frame?.type === 'response' && frame.id === childShow.id) || null
        : null;
      if (childShow && childShowResponse) break;
      await page.waitForTimeout(50);
    }
    if (!childShow || !childShowResponse || childShowResponse.success !== true
      || childShowResponse.data?.error != null || childShowResponse.data?.repo !== 'psy-node') {
      fail(`parent /show did not return a null child snapshot error (${JSON.stringify({ childShow, childShowResponse })})`);
    }
    await waitFor(page, () => document.getElementById('code-review-panel') !== null
      && [...document.querySelectorAll('.code-review__file')].some((row) => row.getAttribute('data-file-path') === 'base.txt'),
      'parent /show response did not commit the child base.txt panel', 20000);
    await page.click('button.code-review__close');
    await waitFor(page, () => document.getElementById('code-review-panel') === null, 'parent /show panel did not close');
    await revealSessionRail(page, 'parent non-repo: return to the repo session');
    await page.fill('#session-sidebar-search', parentSession.beforeSessionId);
    await waitFor(
      page,
      (want) => {
        const row = Array.from(document.querySelectorAll('.session-sidebar__switch')).find(
          (button) => button.dataset.sessionId === want,
        );
        return row !== undefined && row.disabled !== true;
      },
      'parent non-repo: repo sidebar row never became clickable',
      20000,
      parentSession.beforeSessionId,
    );
    const clickedBack = await page.evaluate((want) => {
      const row = Array.from(document.querySelectorAll('.session-sidebar__switch')).find(
        (button) => button.dataset.sessionId === want,
      );
      if (!row) return false;
      row.click();
      return true;
    }, parentSession.beforeSessionId);
    if (!clickedBack) fail('parent non-repo: repo sidebar row missing');
    await waitFor(page, (target) => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId === target, 'parent non-repo: did not return to the repo session', 20000, parentSession.beforeSessionId);
    await page.fill('#session-sidebar-search', '');
    await page.setViewportSize({ width: 1360, height: 900 });
    await runFileExplorerAcceptance(page, sentFrames, receivedFrames);

    // ---- Session switch closes the owning review workspace ----
    // Create target session B while code review is closed, then switch back
    // to A. The actual A -> B cutover below clicks B's persistent sidebar row
    // while the review panel is still mounted, so only switch_session can
    // trigger the stamped code_review_close cleanup.
    await revealSessionRail(page, 'code-review target setup: reopen Sessions rail');
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
    await chooseCommand(page, 'review');
    await waitFor(
      page,
      () => (document.getElementById('prompt-input')?.value.trim() || '').startsWith('/review'),
      'reopen draft missing before the session-switch gate'
    );
    await page.fill('#prompt-input', '/review HEAD~1 HEAD');
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
    // Target session B: a bare /review must NOT reuse A's revision args.
    await openPicker(page);
    await chooseCommand(page, 'review');
    await waitFor(
      page,
      () => (document.getElementById('prompt-input')?.value.trim() || '').startsWith('/review'),
      'choosing /review in the target session did not insert the draft'
    );
    await selectCompletionThenSubmit(page, {
      token: '/review',
      label: 'target-session /review open',
      sentFrames,
      dispatchType: 'code_review_open',
    });
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
      'bare /review in the target session did not open the working-tree scope',
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

    // Bare /review on A, then switch to B before A's code_review_commits
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
    await page.fill('#prompt-input', '/show');
    // Bare /show is staged through the inline popup too: the selecting Enter
    // settles the token, the dispatch Enter opens the chooser and requests
    // the commit list.
    await selectCompletionThenSubmit(page, {
      token: '/show',
      label: 'commit picker: bare /show on A',
      sentFrames,
      dispatchType: 'code_review_commits',
    });
    await waitFor(
      page,
      () => document.querySelector('[aria-label="Recent commits"]') !== null,
      'commit picker: bare /show did not open the recent-commit chooser on A',
      20000,
    );
    const lateCommitRequest = sentFrames.slice(lateCommitMark).map(parseSentFrame)
      .find((frame) => frame?.type === 'code_review_commits' && frame.sessionId === sidA);
    if (!lateCommitRequest) fail(`commit picker: bare /show did not request code_review_commits for ${sidA}`);
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
    await page.fill('#prompt-input', '/show');
    await selectCompletionThenSubmit(page, {
      token: '/show',
      label: 'commit picker: current-session /show',
      sentFrames,
      dispatchType: 'code_review_commits',
    });
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
    await chooseCommand(page, 'review');
    await waitFor(
      page,
      () => (document.getElementById('prompt-input')?.value.trim() || '').startsWith('/review'),
      'review ownership: choosing /review in the sibling session did not insert the draft',
    );
    await page.fill('#prompt-input', '/review HEAD~1 HEAD');
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
      fail(`review ownership: the sibling session's /review must dispatch exactly one code_review_open stamped with ${sidB} (${JSON.stringify(siblingOpens)})`);
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
    // The mobile page's own outgoing frames feed the same wire proof (page-scoped observer).
    const mobileSentFrames = [];
    mobilePage.on('websocket', (socket) => {
      socket.on('framesent', (frame) => { if (typeof frame.payload === 'string') mobileSentFrames.push(frame.payload); });
    });
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
      if (message.type() === 'error' && !isKnownConsoleNoise(message)) mobileConsoleErrors.push(message.text());
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
    await chooseCommand(mobilePage, 'review');
    await waitFor(
      mobilePage,
      () => (document.getElementById('prompt-input')?.value.trim() || '').startsWith('/review'),
      'choosing /review on mobile did not insert the draft'
    );
    // The mobile page's own socket is not in the desktop frame log, so the
    // selection step is proven by the product surface: the panel must stay
    // unmounted and the draft must settle before the dispatch Enter.
    await selectCompletionThenSubmit(mobilePage, {
      token: '/review',
      label: 'mobile /review open',
      sentFrames: mobileSentFrames,
      dispatchType: 'code_review_open',
    });
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
    // ---- Outer shell header-to-footer geometry (mobile 390x844) ----
    // Same header/footer contract as desktop: the shared drawer base's
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
    await selectCompletionThenSubmit(page, {
      token: `/skill ${skillName}`,
      option: skillName,
      label: `/skill ${skillName} summary dispatch`,
      sentFrames,
      dispatchType: 'skill',
    });
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
    await selectCompletionThenSubmit(page, {
      token: '/compact',
      label: '/compact dispatch',
      sentFrames,
      dispatchType: 'compact',
    });
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
    await selectCompletionThenSubmit(page, {
      token: '/compact',
      label: 'bare /compact dispatch',
      sentFrames,
      dispatchType: 'compact',
    });
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
    // Bare /skill lists every skill candidate, so the inline popup would own
    // the Enter and settle a candidate instead of submitting the bare token.
    // Dismiss it explicitly with the popup's own Escape (closeInline), then
    // the single Enter dispatches the bare command and surfaces the usage
    // error exactly as before.
    await waitFor(
      page,
      () => document.querySelector('.command-picker__popover') !== null,
      'bare /skill: the inline completion popup did not open',
      25000
    );
    await page.press('#prompt-input', 'Escape');
    await waitFor(
      page,
      () => document.querySelector('.command-picker__popover') === null
        && (document.getElementById('prompt-input')?.value || '') === '/skill',
      'bare /skill: Escape did not dismiss the inline completion popup with the draft intact',
      15000
    );
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
    // unified action back to Send) so the following /review Enter is
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

    // /review HEAD~1 HEAD: two revisions reach code_review_open and the
    // comparison label renders "HEAD~1 → HEAD"; the × close button removes
    // the panel.
    await page.fill('#prompt-input', '/review HEAD~1 HEAD');
    const revBefore = sentFrames.length;
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, () => document.getElementById('code-review-panel') !== null, 'code-review panel did not open for /review HEAD~1 HEAD');
    // The revision-args open uses the same outer shell: header-to-footer,
    // never a drawer-sized right rail and never a full-viewport cover.
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
      fail(`/review HEAD~1 HEAD must send code_review_open with from=HEAD~1 to=HEAD (got ${JSON.stringify(revFrame)})`);
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

    // /review <bad-ref> HEAD: git resolution fails before the panel
    // opens. Toast the invalid-revision message and keep the composer draft.
    await page.fill('#prompt-input', '/review invalid-ref-xyz HEAD');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => Array.from(document.querySelectorAll('.toast--error')).some((t) =>
        (t.textContent || '').includes('invalid source revision; expected a commit, branch, or tag')
      ),
      '/review invalid-ref-xyz HEAD did not toast the invalid revision before opening a panel',
      15000
    );
    const panelAfterBadRef = await page.evaluate(() => document.getElementById('code-review-panel') !== null);
    if (panelAfterBadRef) fail('/review invalid-ref-xyz HEAD opened the panel — invalid revisions must toast first');
    const badRefValue = await page.evaluate(() => document.getElementById('prompt-input')?.value || '');
    if (badRefValue !== '/review invalid-ref-xyz HEAD') {
      fail(`/review invalid-ref-xyz HEAD must preserve the draft after the invalid revision toast (value=${JSON.stringify(badRefValue)})`);
    }
    await laneShot(page, 'code-review-error.png', { fullPage: true });

    // /review a b c: the client-side arity guard toasts the usage error
    // and NEVER opens the panel (zero, one, or two revisions only).
    await page.fill('#prompt-input', '/review a b c');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => Array.from(document.querySelectorAll('.toast--error')).some((t) =>
        (t.textContent || '').includes('usage: /review [<repo>] [<base> | <from> <to>]')
      ),
      '/review a b c did not surface the arity usage error toast',
      15000
    );
    await page.waitForTimeout(800);
    const panelAfterArity = await page.evaluate(() => document.getElementById('code-review-panel') !== null);
    if (panelAfterArity) fail('/review a b c opened the panel — the arity guard must reject before any RPC');
    const arityValue = await page.evaluate(() => document.getElementById('prompt-input')?.value || '');
    if (arityValue !== '/review a b c') {
      fail(`/review a b c must preserve the draft after the usage error (value=${JSON.stringify(arityValue)})`);
    }
    await laneShot(page, 'code-review-arity-error.png', { fullPage: true });
    // The two expected error toasts above are the evidence in their own
    // shots; DISMISS them with real clicks (an action-less toast dismisses
    // on click, App ToastList) so the clean origin-master geometry captures
    // are not covered by stale errors. A toast that refuses to dismiss is a
    // typed failure — never a sleep and never a skipped assertion.
    const errorToasts = page.locator('#toasts .toast--error');
    while (await errorToasts.count()) {
      const before = await errorToasts.count();
      await errorToasts.first().click();
      await waitFor(page, (beforeArg) => document.querySelectorAll('#toasts .toast--error').length < beforeArg, 'error toast refused to dismiss', 5000, before);
    }

    // ------------------------------------------------------------------
    // /review origin/master: the EXACT literal single-ref user path.
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
    await page.fill('#prompt-input', '/review origin/master');
    const originBefore = sentFrames.length;
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, () => document.getElementById('code-review-panel') !== null, 'code-review panel did not open for /review origin/master');
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
      fail(`/review origin/master must dispatch exactly one code_review_open with from=origin/master includeWorkingTree=true and no to key (count=${originOpenCount}, got ${JSON.stringify(originOpen)})`);
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

    // Clipboard writeText needs a FOCUSED document and the closed mobile context was a separate window.
    await page.bringToFront();
    await waitFor(page, () => document.hasFocus(), 'command-copy: desktop page did not take document focus before the clipboard journeys', 10000);

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
      frames: receivedFrames,
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
        frames: receivedFrames,
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
    await chooseCommand(page, 'review');
    await waitFor(
      page,
      () => (document.getElementById('prompt-input')?.value.trim() || '').startsWith('/review'),
      'review ownership: choosing /review in the third session did not insert the draft',
    );
    await selectCompletionThenSubmit(page, {
      token: '/review',
      label: 'review ownership: third session /review open',
      sentFrames,
      dispatchType: 'code_review_open',
    });
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
    // stamp taken from that re-bound ref sends /review into the primary's
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
    await chooseCommand(page, 'review');
    await waitFor(
      page,
      () => (document.getElementById('prompt-input')?.value.trim() || '').startsWith('/review'),
      'warm reconnect: choosing /review did not insert the draft',
    );
    await page.fill('#prompt-input', '/review HEAD~1 HEAD');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') !== null,
      'warm reconnect: /review never opened the review panel',
      30000,
    );
    const warmSlashOpens = sentFrames
      .slice(warmSlashMark)
      .map(parseSentFrame)
      .filter((frame) => frame?.type === 'code_review_open');
    if (warmSlashOpens.length === 0) fail('warm reconnect: the slash dispatched no code_review_open frame');
    const slashOwner = warmSlashOpens[0].sessionId;
    if (slashOwner !== displayedBeforeReconnect) {
      fail(`warm reconnect: /review was stamped for ${JSON.stringify(slashOwner)} instead of the displayed session ${displayedBeforeReconnect} (listener primary ${listenerPrimaryId}) — git resolves the revision in the primary's workspace`);
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
      fail(`warm reconnect: the displayed session ${displayedBeforeReconnect} holds no open review after /review (${error?.message || error}) sent=${JSON.stringify(warmSlashOpens[0])} response=${JSON.stringify(openResponse)}`);
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
      fail(`warm reconnect: /review resolved its revisions in the wrong workspace (${wrongWorkspaceToasts} invalid-revision toast(s))`);
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
    // Close the restored panel so the deferred-probe phase starts from a
    // closed panel and a reachable composer.
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
    await chooseCommand(page, 'review');
    await waitFor(
      page,
      () => (document.getElementById('prompt-input')?.value.trim() || '').startsWith('/review'),
      'deferred probe: choosing /review did not insert the draft',
    );
    await page.fill('#prompt-input', '/review HEAD~1 HEAD');
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
    await chooseCommand(page, 'review');
    await waitFor(
      page,
      () => (document.getElementById('prompt-input')?.value.trim() || '').startsWith('/review'),
      'pending unmount: choosing /review did not insert the draft',
    );
    await page.fill('#prompt-input', '/review');
    const unmountDraft = await page.evaluate(() => document.getElementById('prompt-input')?.value || '');
    if (unmountDraft.trim() !== '/review') {
      fail(`pending unmount: the composer draft was lost before submit (value=${JSON.stringify(unmountDraft)})`);
    }
    await selectCompletionThenSubmit(page, {
      token: '/review',
      label: 'pending unmount /review open',
      sentFrames,
      dispatchType: 'code_review_open',
    });
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
        const context = { message, stack, laneNonce, mapRequest: err?.mapRequest || null, mapResponse: err?.mapResponse || null, mapHold: err?.mapHold || null, rowBlocker: err?.rowBlocker || null, layoutGeometry: err?.layoutGeometry || null, pages: [] };
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
                featureMenuBtn: probe('#feature-menu-btn'),
                railReopen: probe('#rail-reopen-btn'),
                fileExplorerSection: probe('section.file-explorer'),
                map: {
                  hostPath: document.querySelector('[data-testid="file-dependency-map"]')?.getAttribute('data-map-path') || '',
                  svgCount: document.querySelectorAll('[data-testid="file-explorer-map-pane"] svg').length,
                  nodeCount: document.querySelectorAll('[data-testid="file-dependency-map"] svg .node, [data-testid="file-dependency-map"] svg .nodeLabel').length,
                  foreignObject: document.querySelector('[data-testid="file-explorer-map-pane"] svg foreignObject') != null,
                  dialog: document.querySelector('[data-testid="file-explorer-map-dialog"]') != null,
                  active: document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || '',
                },
                filesOpenClass: document.querySelector('.app-layout')?.classList.contains('app-layout--files-open') === true,
                footer: (() => {
                  const f = document.querySelector('footer:has(#composer-row), footer:has(#prompt-input)');
                  if (!f) return null;
                  const rect = f.getBoundingClientRect();
                  return { x: Math.round(rect.x), y: Math.round(rect.y), w: Math.round(rect.width), h: Math.round(rect.height), display: getComputedStyle(f).display };
                })(),
                transcriptHit: (() => {
                  const transcript = document.getElementById('transcript');
                  const tbox = transcript?.getBoundingClientRect();
                  const el = tbox && tbox.width > 0 ? document.elementFromPoint(tbox.right - 10, tbox.top + Math.min(120, tbox.height / 2)) : null;
                  return el ? { tag: el.tagName, id: el.id || '', classes: typeof el.className === 'string' ? el.className : '' } : null;
                })(),
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