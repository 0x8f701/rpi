// Web composer command-button + code-review panel E2E lane (playwright half
// of E2E.d/web/commands_review.sh).
//
// Environment:
//   RPI_URL          http://127.0.0.1:<port>/web
//   RPI_TOKEN        token file content (served via rpi-auth.<token> subprotocol)
//   RPI_CHROME       executable path of the system Chrome (optional)
//   RPI_EVIDENCE     evidence dir for screenshots
//   RPI_DIRTY_FILE   tracked file modified in the working tree ("greet.txt")
//   RPI_ADDED_FILE   staged new tracked file ("added.txt")
//   RPI_BIG_FILE     oversize file whose diff the backend marks truncated
//   RPI_SKILL_NAME   fixture project skill name ("greet")
//   RPI_SKILL_DESC   fixture project skill description text
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
//     file renders its truncated token + the file-level truncation banner,
//     and its diff loads behind a 4000-line soft window: Load more reveals
//     the later lines and Load full reaches the end (8200 lines)
//   - the changed-file rail is a collapsible path tree: the nested/ dir row
//     is expanded by default, clicking it collapses/expands the subtree,
//     the filter keeps the matched file + its ancestor (forced expanded),
//     and tree keyboard navigation spans dirs + files (arrows rove, Home/End
//     jump, Enter toggles dirs/selects files, ArrowRight/Left expand/collapse)
//   - TUI/Web parity for the file tree: compact colored status glyphs
//     (modified -> M, added -> A) mirror the TUI letters; every level shows
//     only its basename (nested/deep.txt renders as "deep.txt") while the
//     FULL repo-relative path stays in data-file-path/title and the readable
//     full state in aria-label; selection + filter key on the full path;
//     stats never swallow the filename and the rail never overflows
//     (desktop + mobile viewport)
//   - comment bodies render markdown (strong, lists, ```rust fences with
//     hljs highlight) while hostile HTML stays literal text — no element is
//     created, no script runs, no dialog opens (user AND assistant comments)
//   - plain Enter submits a comment; Shift+Enter inserts a newline without
//     submitting and a synthetic IME-composing Enter never submits; while
//     the first reply streams the composer stays ENABLED with no streaming
//     warning note, a second comment submits immediately and renders as a
//     queued card, and Abort clears the active stream AND drops the queued
//     (not-yet-started) comment while leaving the partial assistant reply
//   - the >4000-line fixture keeps the file-level 'Large file — the diff
//     loads in bounded pages' banner/paging (Load more/Load full), the
//     removed panel-level 'Large diff — all changed files…' notice stays
//     ABSENT, and typing into the composer with the large diff rendered
//     stays responsive (stable textarea identity, bounded per-keystroke
//     DOM mutation, bounded edit time — never an inflated test timeout)
//   - the panel polls code_review_snapshot at the 1.5s contract while open
//     (≥2 frames ~1.5s apart) and polling stops after the panel closes
//   - a session switch closes the owning review workspace (code_review_close
//     stamped with the owning sessionId), and a bare /code-review in the
//     target session never reuses the previous session's revision args
//   - Escape with a draft shows the inline close confirm; "Keep editing"
//     keeps the workspace + draft; Escape with no draft closes the panel
//   - mobile (≤900px): Files→Diff→Thread tab transitions, composer visible
//     without scrolling, Back-to-diff, and button.code-review__close removes
//     #code-review-panel from the DOM; `.code-review__thread-resizer` hidden
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

import { chromium } from 'playwright';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const dirtyFile = process.env.RPI_DIRTY_FILE || 'greet.txt';
const addedFile = process.env.RPI_ADDED_FILE || 'added.txt';
const bigFile = process.env.RPI_BIG_FILE || 'big.txt';
const nestedFile = process.env.RPI_NESTED_FILE || 'nested/deep.txt';
const skillName = process.env.RPI_SKILL_NAME || 'greet';
const skillDesc = process.env.RPI_SKILL_DESC || 'Greet skill for E2E';

function fail(message) {
  // Exit 2 (not 1): 1 means "playwright setup failure (node/chromium/npm)";
  // 2+ is an assertion failure (the lane reports it distinctly).
  console.error(`web-commands-review: FAIL: ${message}`);
  process.exit(2);
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

/** True when the picker has drilled into skills mode (the `Skills` header is
 *  present). Used to assert the `/skill` parent switches the picker surface. */
async function pickerInSkillsMode(page) {
  return page.evaluate(() => {
    const title = document.querySelector('.command-picker__title')?.textContent?.trim() || '';
    return title === 'Skills';
  });
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage();
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    page.on('pageerror', (err) => {
      console.error(`web-commands-review: page error: ${err.message}`);
    });

    // Capture outgoing WS frames so the /compact dispatch can be observed
    // without requiring the provider round-trip to succeed (deterministic).
    // code_review_snapshot frames are timestamped separately: the panel polls
    // at a fixed 1.5s cadence while mounted, so ≥2 frames ~1.5s apart must be
    // observable, and polling must stop once the panel closes.
    const sentFrames = [];
    const snapshotFrames = [];
    page.on('websocket', (ws) => {
      ws.on('framesent', (frame) => {
        const payload = typeof frame.payload === 'string' ? frame.payload : '';
        if (!payload) return;
        sentFrames.push(payload);
        try {
          const parsed = JSON.parse(payload);
          if (parsed && parsed.type === 'code_review_snapshot') {
            snapshotFrames.push({ t: Date.now(), payload });
          }
        } catch {
          // not a JSON frame — ignore
        }
      });
    });
    // Hostile diff/comment text must never open a browser dialog.
    let dialogs = 0;
    page.on('dialog', (dialog) => {
      dialogs += 1;
      dialog.dismiss().catch(() => {});
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(page, () => document.getElementById('conn-state') !== null, 'conn-state missing');
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
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
    await page.screenshot({ path: `${evidence}/command-button-left.png`, fullPage: true });

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
    await page.screenshot({ path: `${evidence}/command-picker-open.png`, fullPage: true });

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
    await page.screenshot({ path: `${evidence}/code-review-draft-no-submit.png`, fullPage: true });

    // Enter dispatches code_review_open -> the real review panel renders.
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
    await page.screenshot({ path: `${evidence}/code-review-panel-open.png`, fullPage: true });

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
    await page.screenshot({ path: `${evidence}/code-review-thread-resize.png`, fullPage: true });

    // Reload persistence for the thread width.
    await page.reload({ waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.getElementById('conn-state') !== null, 'thread reload: conn-state missing');
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
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
    await page.screenshot({ path: `${evidence}/code-review-diff-lines.png`, fullPage: true });

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
      const li = document.querySelector('.code-review__file-list li.code-review__tree-row[data-tree-kind="dir"]');
      if (!li) return null;
      return {
        name: (li.querySelector('.code-review__file-path')?.textContent || '').trim(),
        expanded: li.getAttribute('aria-expanded'),
      };
    });
    if (!nestedDir) fail('nested directory row missing from the file tree');
    if (nestedDir.name !== 'nested') fail(`directory row name wrong (got "${nestedDir.name}")`);
    if (nestedDir.expanded !== 'true') fail('nested directory is not expanded by default');
    const deepVisible = await page.evaluate((want) =>
      Array.from(document.querySelectorAll('.code-review__file')).some((b) =>
        b.getAttribute('data-file-path') === want
      )
    , nestedFile);
    if (!deepVisible) fail(`nested file ${nestedFile} not visible under the default-expanded dir`);
    const rowsBeforeCollapse = await page.evaluate(() =>
      document.querySelectorAll('.code-review__file-list [role="treeitem"]').length
    );
    await page.click('.code-review__file-list li.code-review__tree-row[data-tree-kind="dir"] button');
    await waitFor(
      page,
      (n) => document.querySelectorAll('.code-review__file-list [role="treeitem"]').length === n - 1,
      'clicking the dir row did not collapse the subtree',
      10000,
      rowsBeforeCollapse
    );
    const deepHidden = await page.evaluate((want) =>
      Array.from(document.querySelectorAll('.code-review__file')).some((b) =>
        b.getAttribute('data-file-path') === want
      )
    , nestedFile);
    if (deepHidden) fail(`nested file ${nestedFile} still visible after collapsing its dir`);
    await page.click('.code-review__file-list li.code-review__tree-row[data-tree-kind="dir"] button');
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
        const li = document.querySelector('.code-review__file-list li.code-review__tree-row[data-tree-kind="dir"]');
        const file = Array.from(document.querySelectorAll('.code-review__file')).some((b) =>
          b.getAttribute('data-file-path') === want
        );
        return li !== null && file && li.getAttribute('aria-expanded') === 'true';
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

    // ---- Truncated fixture file ----
    // The >4000-line fixture file is backend-marked truncated: the row carries
    // the truncated token and the diff stage surfaces a truncation banner.
    await page.fill('.code-review__file-filter', bigFile);
    await waitFor(
      page,
      (want) => Array.from(document.querySelectorAll('.code-review__file')).some((b) =>
        b.getAttribute('data-file-path') === want
      ),
      `truncated fixture file ${bigFile} not listed`,
      10000,
      bigFile
    );
    const truncatedStats = await page.evaluate((want) => {
      const btn = Array.from(document.querySelectorAll('.code-review__file')).find((b) =>
        b.getAttribute('data-file-path') === want
      );
      return btn ? (btn.querySelector('.code-review__file-stats')?.textContent || '') : '';
    }, bigFile);
    if (!truncatedStats.includes('truncated')) {
      fail(`truncated fixture file row missing the truncated token (stats=${truncatedStats})`);
    }
    await page.evaluate((want) => {
      const btn = Array.from(document.querySelectorAll('.code-review__file')).find((b) =>
        b.getAttribute('data-file-path') === want
      );
      if (btn) btn.click();
    }, bigFile);
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.code-review__banner--truncated')).some((el) =>
          (el.textContent || '').includes('Large file — the diff loads in bounded pages')
        ),
      'truncated fixture file did not render the file-level truncation banner',
      10000
    );
    // ---- Soft window: >4000-line file loads in bounded chunks ----
    // The big fixture file's diff starts behind the 4000-line soft window:
    // only the first 4000 lines render, "Load more" reveals the next chunk
    // (later base-lines AND the first changed lines), "Load full" jumps to
    // the end and the load controls disappear once everything is shown.
    await waitFor(
      page,
      (n) => document.querySelectorAll('.code-review__line').length === n,
      'big file did not settle on the 4000-line soft window',
      10000,
      4000
    );
    const changedEarly = await page.evaluate(() =>
      Array.from(document.querySelectorAll('.code-review__line-text')).some((el) =>
        (el.textContent || '').includes('changed-line-00001')
      )
    );
    if (changedEarly) fail('changed lines rendered before any Load more click');
    await page.click('.code-review__load-more');
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.code-review__line-text')).some((el) =>
          (el.textContent || '').includes('base-line-04001')
        ) &&
        Array.from(document.querySelectorAll('.code-review__line-text')).some((el) =>
          (el.textContent || '').includes('changed-line-00001')
        ),
      'Load more did not reveal the lines beyond the 4000-line window',
      10000
    );
    await page.click('.code-review__load-full');
    await waitFor(
      page,
      () => document.querySelector('.code-review__load-more') === null,
      'Load full did not exhaust the remaining lines (Load more still present)',
      10000
    );
    const finalLineVisible = await page.evaluate(() =>
      Array.from(document.querySelectorAll('.code-review__line-text')).some((el) =>
        (el.textContent || '').includes('changed-line-04100')
      )
    );
    if (!finalLineVisible) fail('Load full did not reveal the final changed line');
    // The panel-level "Large diff — all changed files are listed; file
    // bodies load in bounded pages on demand" notice was REMOVED from the
    // panel: with the truncated file open, NO banner anywhere may carry that
    // text — the file-level 'Large file — the diff loads in bounded pages'
    // banner (asserted above) is the only truncation surface now.
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
    await page.screenshot({ path: `${evidence}/large-diff-responsive.png`, fullPage: true });
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
    // ArrowRight/ArrowLeft expand/collapse a focused dir. The fixture tree
    // sorts as: nested(d), nested/deep.txt, added.txt, big.txt, greet.txt.
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
    // Home -> first row (the nested dir); Enter toggles it (collapse/expand).
    await page.keyboard.press('Home');
    const afterHome = await page.evaluate(() => document.activeElement?.textContent?.trim() || '');
    if (!afterHome.includes('nested')) {
      fail(`Home did not jump the focus to the first tree row (active=${afterHome})`);
    }
    await page.keyboard.press('Enter');
    await waitFor(
      page,
      (n) => document.querySelectorAll('.code-review__file-list [role="treeitem"]').length === n - 1,
      'Enter on the dir row did not collapse the tree (child row still visible)',
      10000,
      treeRowTexts.length
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
      (n) => document.querySelectorAll('.code-review__file-list [role="treeitem"]').length === n - 1,
      'tree did not collapse before the ArrowRight gate',
      10000,
      treeRowTexts.length
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
      (n) => document.querySelectorAll('.code-review__file-list [role="treeitem"]').length === n - 1,
      'ArrowLeft did not collapse the expanded dir row',
      10000,
      treeRowTexts.length
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
    await page.keyboard.press('End');
    const afterEnd = await page.evaluate(() => document.activeElement?.textContent?.trim() || '');
    if (!afterEnd.includes(dirtyFile)) {
      fail(`End did not jump the focus to the last tree row (active=${afterEnd})`);
    }
    await page.keyboard.press('Enter');
    await waitFor(
      page,
      (want) => (document.querySelector('.code-review__diff-path')?.textContent || '').includes(want),
      'Enter did not select the focused file row',
      10000,
      dirtyFile
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
    await page.click('button.code-review__action--warn');
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
    // The second hunk's row badge counts exactly this one comment.
    const hunk2Badge = await page.evaluate(() =>
      document.querySelectorAll('.code-review__hunk')[1]?.querySelector('.code-review__hunk-badge')?.textContent?.trim() || ''
    );
    if (hunk2Badge !== '1') {
      fail(`second hunk badge should be 1 after one comment (got "${hunk2Badge}")`);
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
    const assistantCountBefore = await page.evaluate(
      () => document.querySelectorAll('.code-review__comment--assistant:not(.is-partial)').length
    );
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
    await page.screenshot({ path: `${evidence}/code-review-closed.png`, fullPage: true });

    // Polling must STOP once the panel is closed: no code_review_snapshot
    // frame may be sent in the 3.6s window after unmount.
    const framesAtClose = snapshotFrames.length;
    await page.waitForTimeout(3600);
    const framesAfterClose = snapshotFrames.length;
    if (framesAfterClose !== framesAtClose) {
      fail(`snapshot polling continued after the panel closed (+${framesAfterClose - framesAtClose} frames in 3.6s)`);
    }

    // ---- Session switch closes the owning review workspace ----
    // Reopen the panel with TWO revisions so revision args are captured.
    // Creating a target session must close A's controller via a stamped
    // code_review_close (sessionId A — never the newly active session), and
    // a bare /code-review in the target session must not reuse A's args.
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
    const sidA = await page.evaluate(() => {
      const rows = Array.from(document.querySelectorAll('.session-sidebar__switch'));
      return rows.length ? (rows[0].dataset.sessionId || '') : '';
    });
    if (!sidA) fail('could not read the owning session id from the sidebar');
    // Switch: create a target session. The panel must close and the unmount
    // must emit code_review_close stamped with the OWNING session (A). The
    // fullscreen panel covers the sidebar, so dispatch the click directly.
    const switchMark = sentFrames.length;
    await page.evaluate(() => {
      const btn = document.getElementById('sidebar-new-session-btn');
      if (btn) btn.click();
    });
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') === null,
      'code-review panel did not close on session switch',
      20000
    );
    // The unmount cleanup emits the stamped close; retry briefly so the
    // frame cannot race the DOM observation.
    let stampedClose = null;
    const stampDeadline = Date.now() + 5000;
    while (!stampedClose && Date.now() < stampDeadline) {
      for (let i = switchMark; i < sentFrames.length; i++) {
        try {
          const frame = JSON.parse(sentFrames[i]);
          if (frame && frame.type === 'code_review_close' && frame.sessionId === sidA) {
            stampedClose = frame;
            break;
          }
        } catch {
          // not JSON — skip
        }
      }
      if (!stampedClose) await page.waitForTimeout(200);
    }
    if (!stampedClose) {
      fail('session switch did not emit a code_review_close stamped with the owning session');
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

    // ---- Mobile (≤900px): tabbed single-pane workspace ----
    // Files -> selecting a file opens Diff -> selecting a hunk opens Thread;
    // the composer is reachable without scrolling; Back-to-diff returns; the
    // close button (required E2E selector) removes the panel.
    const mobileContext = await browser.newContext({ viewport: { width: 480, height: 760 } });
    const mobilePage = await mobileContext.newPage();
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
      () => document.getElementById('conn-state')?.dataset.state === 'on',
      'mobile WS did not reach "connected"'
    );
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
    await mobileContext.close();
    await page.screenshot({ path: `${evidence}/code-review-mobile-tabs.png`, fullPage: true });

    // /skill <fixture> renders the loaded skill's frontmatter summary. The
    // picker surfaces the REAL on-disk fixture skill as a candidate: selecting
    // the `/skill` parent drills the picker into skills mode, the greet
    // candidate (loaded from `.pi/skills/greet/SKILL.md` via get_commands)
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
    await page.screenshot({ path: `${evidence}/skill-candidates.png`, fullPage: true });
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
    await page.screenshot({ path: `${evidence}/skill-summary.png`, fullPage: true });

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
    await page.screenshot({ path: `${evidence}/compact-dispatch.png`, fullPage: true });

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
    await page.screenshot({ path: `${evidence}/snapcompact-error.png`, fullPage: true });

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
    await page.screenshot({ path: `${evidence}/compact-instructions-error.png`, fullPage: true });

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
    await page.screenshot({ path: `${evidence}/compact-bare-error.png`, fullPage: true });

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
    await page.screenshot({ path: `${evidence}/skill-first-token.png`, fullPage: true });

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
    await page.screenshot({ path: `${evidence}/skill-bare-error.png`, fullPage: true });

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
    // badge hidden) so the following /code-review Enter is never raced into
    // a steer.
    await waitFor(
      page,
      () => {
        const badge = document.getElementById('stream-badge');
        if (badge && badge.hidden !== true) return false;
        return !Array.from(document.querySelectorAll('.msg--user')).some(
          (b) => (b.textContent || '').includes('/unknowncmd') && b.classList.contains('optimistic')
        );
      },
      'slash variants: /unknowncmd turn never settled (badge hidden + confirmed bubble)',
      30000
    );
    await page.screenshot({ path: `${evidence}/unknowncmd-fallthrough.png`, fullPage: true });

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
    await page.screenshot({ path: `${evidence}/code-review-revisions.png`, fullPage: true });
    await page.click('button.code-review__close');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') === null,
      'code-review panel did not close via the × close button'
    );
    await page.screenshot({ path: `${evidence}/code-review-revisions-closed.png`, fullPage: true });

    // /code-review <bad-ref> HEAD: the REAL git revision resolution error
    // renders in the panel (.code-review__error with "invalid source
    // revision"), Retry re-runs the open (same error), and the × close
    // button closes the error state cleanly.
    await page.fill('#prompt-input', '/code-review invalid-ref-xyz HEAD');
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, () => document.getElementById('code-review-panel') !== null, 'code-review panel did not open for the bad-ref variant');
    await waitFor(
      page,
      () => {
        const label = document.querySelector('.code-review__label')?.textContent || '';
        return label.includes('invalid-ref-xyz') && label.includes('HEAD');
      },
      'code-review bad-ref panel did not render the invalid-ref-xyz → HEAD comparison label'
    );
    await waitFor(
      page,
      () => {
        const error = document.querySelector('.code-review__error');
        return error !== null && (error.textContent || '').includes('invalid source revision');
      },
      'code-review bad-ref panel never rendered the real git revision error (.code-review__error with invalid source revision)'
    );
    const retryVisible = await page.evaluate(() => {
      const buttons = Array.from(document.querySelectorAll('.code-review__error-actions button.code-review__link'));
      return buttons.some((b) => (b.textContent || '').trim() === 'Retry');
    });
    if (!retryVisible) fail('code-review error state must offer a Retry action');
    await page.screenshot({ path: `${evidence}/code-review-error.png`, fullPage: true });
    await page.click('.code-review__error-actions button.code-review__link');
    // Retry re-runs the open against the same workspace; the same real error
    // must render again (the boundary is stable, never a transient blank).
    await waitFor(
      page,
      () => {
        const error = document.querySelector('.code-review__error');
        return error !== null && (error.textContent || '').includes('invalid source revision');
      },
      'code-review Retry did not re-surface the real git revision error'
    );
    await page.click('button.code-review__close');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') === null,
      'code-review error panel did not close via the × close button'
    );
    await page.screenshot({ path: `${evidence}/code-review-error-closed.png`, fullPage: true });

    // /code-review a b c: the client-side arity guard toasts the usage error
    // and NEVER opens the panel (zero or two revisions only).
    await page.fill('#prompt-input', '/code-review a b c');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => Array.from(document.querySelectorAll('.toast--error')).some((t) =>
        (t.textContent || '').includes('usage: /code-review [from to] — pass zero or two revisions')
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
    await page.screenshot({ path: `${evidence}/code-review-arity-error.png`, fullPage: true });

    console.log('web-commands-review: PASSED (command button left of textarea; picker lists /compact /skill /code-review; /code-review draft + no auto-submit; Enter opens real review panel with HEAD→working tree + dirty file + changed lines; two separated hunks; second-hunk thread ownership; per-hunk drafts A/B survive switches + submit A leaves B; file switch clears composer/hunk selection; hostile diff/comment literal with no dialog/script side effect; explicit hunk selection; file filter; collapsible path tree + tree keyboard nav; 4000-line soft window with Load more/Load full; TUI parity: M/A compact glyphs + basename-only rows with full path/state in data/title/aria + no rail overflow (desktop + mobile); comment markdown (strong/list/rust hljs) with hostile HTML literal; Ctrl+Enter comment + streaming/abort; 1.5s snapshot polling cadence observed + stops after close; session switch stamped code_review_close + no stale rev args; inline close confirm; Escape close; mobile Files/Diff/Thread tabs + close button; /skill visible summary; /compact WS dispatch)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-commands-review: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});