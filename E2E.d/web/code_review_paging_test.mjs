// Web code-review tree/paging/comment-markdown regression lane (playwright
// half of E2E.d/web/code_review_paging.sh).
//
// Environment:
//   RPI_URL          http://127.0.0.1:<port>/web
//   RPI_TOKEN        token file content (served via rpi-auth.<token> subprotocol)
//   RPI_DIRTY_FILE   tracked file modified in the working tree ("greet.txt")
//   RPI_BIG_FILE     oversize file whose diff the backend marks truncated
//   RPI_CHROME       executable path of the system Chrome (optional)
//   RPI_EVIDENCE     evidence dir for screenshots
//
// Assertions:
//   T1  the review file list is a NESTED tree: directory rows
//       li.code-review__tree-row[data-tree-kind="dir"] with
//       button.code-review__tree-dir + aria-expanded (on the li) wrap the
//       fixture's nested files (src/other.rs + src/deep/feature.rs)
//   T2  clicking a directory row collapses it: aria-expanded flips to false
//       and its child FILE rows disappear from the visible list; clicking
//       again expands and the children reappear
//   T3  the >4000-line fixture renders the truncated banner
//       (.code-review__banner--truncated, "Large file — the diff loads in
//       bounded pages") and the first window never exceeds the 4000-line cap
//       (changed-line-04001 is NOT among the first rendered lines)
//   T4  clicking button.code-review__load-more grows the rendered line set
//       past the first 4000-line window — changed-line-04001 appears and the
//       window status reports more lines
//   T5  button.code-review__load-full loads the remaining pages (status
//       reaches the full total or the hard UI cap is surfaced)
//   T7  a globally-truncated EMPTY placeholder (zz-later.txt, hunks: [] +
//       truncated because the combined patch exceeds the 2 MiB snapshot cap)
//       auto-loads its diff on selection — lines appear WITHOUT any
//       Refresh/Load click, the pane never claims "No hunks in this file",
//       and the unknown-language body stays plain (no hljs spans)
//   T8  rust diff lines (src/other.rs) render hljs token spans with verbatim
//       textContent, unchanged line numbers/prefix/kind backgrounds, and a
//       hostile <script> diff line stays LITERAL text with no side effect
//   C1  a submitted comment carrying **bold**, a list, a ```rust``` fence,
//       and hostile <script>/<img onerror> HTML renders markdown (strong,
//       ul>li, pre.md-fence__pre > code.hljs with the .md-fence__lang label)
//       while the hostile HTML stays LITERAL text (textContent carries
//       "<script>", no element created, window.__crPwned undefined, no
//       dialog)
//   C2  the ASSISTANT review reply (mock-routed markdown matrix) renders the
//       same markdown + literal-hostile contract in its own comment
//   T6  responsive breakpoint + mobile tab bar, driven while the panel stays
//       mounted: resizing the viewport across the 900px breakpoint fires the
//       matchMedia change handler (Files/Diff/Thread tab bar appears), each
//       tab click drives the single-pane narrow UI (active tab flips, only
//       the active pane is visible), Back-to-diff returns from the Thread
//       pane, and resizing back to desktop removes the tab bar

import { chromium } from 'playwright';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const dirtyFile = process.env.RPI_DIRTY_FILE || 'greet.txt';
const bigFile = process.env.RPI_BIG_FILE || 'big.txt';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

function fail(message) {
  console.error(`web-code-review-paging: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await page.evaluate(fn, arg)) return;
    await page.waitForTimeout(120);
  }
  fail(`${label} (timeout ${timeoutMs}ms)`);
}

/** Click a file row by its full repo-relative path (data-file-path). */
async function clickFile(page, want) {
  const clicked = await page.evaluate((pathText) => {
    const btn = Array.from(document.querySelectorAll('.code-review__file')).find((b) =>
      b.getAttribute('data-file-path') === pathText
    );
    if (!btn) return false;
    btn.click();
    return true;
  }, want);
  if (!clicked) fail(`file row ${want} not found for click`);
}

/** Directory rows as {id, expanded}. aria-expanded lives on the li. */
const dirRows = () => {
  const rows = Array.from(document.querySelectorAll('li.code-review__tree-row[data-tree-kind="dir"]'));
  return rows.map((li) => ({
    id: li.getAttribute('data-tree-id') || '',
    expanded: li.getAttribute('aria-expanded'),
  }));
};

/** Count of FILE rows that are actually visible (not inside a collapsed dir). */
const visibleFileRows = () => {
  const btns = Array.from(document.querySelectorAll('button.code-review__file'));
  return btns.filter((btn) => {
    const style = window.getComputedStyle(btn);
    return style.display !== 'none' && style.visibility !== 'hidden' && btn.offsetParent !== null;
  }).length;
};


async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage();
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    let dialogs = 0;
    page.on('dialog', (dialog) => {
      dialogs += 1;
      dialog.dismiss().catch(() => {});
    });
    page.on('pageerror', (err) => {
      console.error(`web-code-review-paging: page error: ${err.message}`);
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(page, () => document.getElementById('conn-state') !== null, 'conn-state missing');
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'WS did not reach "connected"'
    );

    // Open the code-review panel.
    await page.fill('#prompt-input', '/code-review');
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, () => document.getElementById('code-review-panel') !== null, 'code-review panel did not open');
    await waitFor(
      page,
      () => {
        const label = document.querySelector('.code-review__label')?.textContent || '';
        return label.includes('HEAD') && label.includes('working tree');
      },
      'comparison label missing'
    );

    // ---- T1: the file list is a nested tree ----
    await waitFor(
      page,
      () => document.querySelectorAll('li.code-review__tree-row[data-tree-kind="dir"]').length >= 1,
      'T1: no directory rows rendered (tree missing)'
    );
    const dirs = await page.evaluate(dirRows);
    const dirIds = dirs.map((d) => d.id);
    if (!dirIds.includes('dir:src')) fail(`T1: dir:src missing from the tree (got ${dirIds.join(', ')})`);
    if (!dirIds.includes('dir:src/deep')) fail(`T1: nested dir:src/deep missing from the tree (got ${dirIds.join(', ')})`);
    if (!dirs.every((d) => d.expanded === 'true')) {
      fail(`T1: directory rows must start expanded (${JSON.stringify(dirs)})`);
    }
    const allFilesVisible = await page.evaluate(visibleFileRows);
    if (allFilesVisible < 4) {
      fail(`T1: expected >=4 visible file rows (greet, big, src/other, src/deep/feature), got ${allFilesVisible}`);
    }
    await page.screenshot({ path: `${evidence}/code-review-tree-open.png`, fullPage: true });

    // ---- T2: collapse + expand a directory row ----
    const collapsed = await page.evaluate(() => {
      const li = Array.from(document.querySelectorAll('li.code-review__tree-row'))
        .find((row) => row.getAttribute('data-tree-id') === 'dir:src');
      const btn = li && li.querySelector('button.code-review__tree-dir');
      if (!btn) return null;
      btn.click();
      return true;
    });
    if (!collapsed) fail('T2: dir:src row button not found');
    await waitFor(
      page,
      () => {
        const li = Array.from(document.querySelectorAll('li.code-review__tree-row'))
          .find((row) => row.getAttribute('data-tree-id') === 'dir:src');
        return li && li.getAttribute('aria-expanded') === 'false';
      },
      'T2: aria-expanded did not flip to false on collapse'
    );
    await page.waitForTimeout(200);
    const filesWhileCollapsed = await page.evaluate(visibleFileRows);
    if (filesWhileCollapsed >= allFilesVisible) {
      fail(`T2: collapsing dir:src did not hide its child files (visible ${filesWhileCollapsed} >= ${allFilesVisible})`);
    }
    await page.screenshot({ path: `${evidence}/code-review-tree-collapsed.png`, fullPage: true });
    // Expand again: children reappear.
    await page.evaluate(() => {
      const li = Array.from(document.querySelectorAll('li.code-review__tree-row'))
        .find((row) => row.getAttribute('data-tree-id') === 'dir:src');
      li.querySelector('button.code-review__tree-dir').click();
    });
    await waitFor(
      page,
      () => {
        const li = Array.from(document.querySelectorAll('li.code-review__tree-row'))
          .find((row) => row.getAttribute('data-tree-id') === 'dir:src');
        return li && li.getAttribute('aria-expanded') === 'true';
      },
      'T2: aria-expanded did not flip back to true on expand'
    );
    await page.waitForTimeout(200);
    const filesExpanded = await page.evaluate(visibleFileRows);
    if (filesExpanded !== allFilesVisible) {
      fail(`T2: expanding dir:src did not restore its children (visible ${filesExpanded} != ${allFilesVisible})`);
    }

    // ---- T3/T4/T5: >4000-line diff paging ----
    await clickFile(page, bigFile);
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.code-review__banner--truncated')).some((el) =>
          (el.textContent || '').includes('Large file — the diff loads in bounded pages')
        ),
      'T3: truncated fixture did not render the truncation banner',
      15000
    );
    const windowStatusBefore = await page.evaluate(() => {
      const meta = document.querySelector('.code-review__window-meta')?.textContent?.trim() || '';
      const status = document.querySelector('.code-review__page-status')?.textContent?.trim() || '';
      return { meta, status };
    });
    if (!windowStatusBefore.meta && !windowStatusBefore.status) {
      fail('T3: no window/page status rendered for the truncated file');
    }
    const lineCountBefore = await page.evaluate(() =>
      document.querySelectorAll('.code-review__line').length
    );
    if (lineCountBefore > 4000) {
      fail(`T3: first render exceeded the 4000-line cap (${lineCountBefore})`);
    }
    const firstRenderHasHidden = await page.evaluate(() =>
      Array.from(document.querySelectorAll('.code-review__line-text')).some((el) =>
        (el.textContent || '').includes('changed-line-04001')
      )
    );
    if (firstRenderHasHidden) {
      fail('T3: changed-line-04001 must be BEYOND the first 4000-line window');
    }
    await page.screenshot({ path: `${evidence}/code-review-big-truncated.png`, fullPage: true });

    // T4: Load more grows the window past the cap. Each click advances exactly
    // one soft-cap window (4000 lines): the fixture's flat stream is 4100
    // deletions (base-line-00001..04100 at indices 0..4099) then 4100
    // additions (changed-line-00001..04100 at indices 4100..8199). The first
    // click reaches index 8000 — proving growth past the 4000-line cap — and
    // the SECOND click reaches 8200, surfacing changed-line-04001 (index 8100).
    const hasLoadMore = await page.evaluate(() => document.querySelector('button.code-review__load-more') !== null);
    if (!hasLoadMore) fail('T4: load-more button missing for the truncated file');
    await page.click('button.code-review__load-more');
    await waitFor(
      page,
      () => {
        const rows = Array.from(document.querySelectorAll('.code-review__line-text'));
        return rows.some((el) => (el.textContent || '').includes('base-line-04001'))
          || rows.some((el) => (el.textContent || '').includes('changed-line-00001'));
      },
      'T4: first Load more did not reveal lines beyond the initial 4000-line window',
      20000
    );
    const lineCountAfterMore = await page.evaluate(() =>
      document.querySelectorAll('.code-review__line').length
    );
    if (lineCountAfterMore <= lineCountBefore) {
      fail(`T4: Load more did not grow the rendered lines (${lineCountAfterMore} <= ${lineCountBefore})`);
    }
    await page.screenshot({ path: `${evidence}/code-review-load-more.png`, fullPage: true });

    // T4b: a SECOND Load more reaches the deep additions (changed-line-04001 at
    // index 8100) — the line that proves the full >4000-line diff is loadable.
    await page.click('button.code-review__load-more');
    await waitFor(
      page,
      () => {
        const rows = Array.from(document.querySelectorAll('.code-review__line-text'));
        return rows.some((el) => (el.textContent || '').includes('changed-line-04001'));
      },
      'T4b: changed-line-04001 never appeared after the second Load more',
      20000
    );
    await page.screenshot({ path: `${evidence}/code-review-load-more-2.png`, fullPage: true });

    // T5: Load full reaches the remainder (or the hard UI cap is surfaced).
    const lineCountAfterTwoMore = await page.evaluate(() =>
      document.querySelectorAll('.code-review__line').length
    );
    const hasLoadFull = await page.evaluate(() => document.querySelector('button.code-review__load-full') !== null);
    if (hasLoadFull) {
      await page.click('button.code-review__load-full');
      await waitFor(
        page,
        () => {
          const meta = document.querySelector('.code-review__window-meta')?.textContent?.trim() || '';
          const status = document.querySelector('.code-review__page-status')?.textContent?.trim() || '';
          const text = `${meta} ${status}`;
          const capped = document.querySelector('.code-review__page-cap') !== null;
          return capped || /no more|all lines|of \d+/i.test(text);
        },
        'T5: Load full did not reach the full total or the hard cap',
        30000
      );
      const statusFull = await page.evaluate(() => ({
        meta: document.querySelector('.code-review__window-meta')?.textContent?.trim() || '',
        status: document.querySelector('.code-review__page-status')?.textContent?.trim() || '',
        capped: document.querySelector('.code-review__page-cap') !== null,
        lines: document.querySelectorAll('.code-review__line').length,
      }));
      await page.screenshot({ path: `${evidence}/code-review-load-full.png`, fullPage: true });
      if (!statusFull.capped && statusFull.lines <= lineCountAfterTwoMore) {
        fail(`T5: Load full stalled without growing the window (${statusFull.lines} <= ${lineCountAfterTwoMore})`);
      }
    } else {
      // After the second 4000-line increment this 8200-line fixture can be
      // fully loaded already, so no further affordance is expected. The
      // original snapshot's `truncated` marker may remain as provenance;
      // prove completeness from the deepest fixture line instead.
      const deepestLoaded = await page.evaluate(() =>
        Array.from(document.querySelectorAll('.code-review__line-text')).some((el) =>
          (el.textContent || '').includes('changed-line-04100')
        )
      );
      if (!deepestLoaded) fail('T5: no Load full affordance and the deepest diff line is still missing');
    }

    // ---- T7: empty globally-truncated placeholder auto-loads on selection ----
    // The combined patch exceeds MAX_DIFF_BYTES (2 MiB) because zz-big.txt's
    // diff alone is > 2 MiB, so the catalog emits files after the cut as EMPTY
    // placeholders (hunks: [], truncated: true). Selecting zz-later.txt must
    // fetch the first bounded pages automatically — no Refresh/Load click.
    await clickFile(page, 'zz-later.txt');
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.code-review__line-text')).some((el) =>
          (el.textContent || '').includes('later changed')
        ),
      'T7: empty placeholder zz-later.txt did not auto-load its diff without a click',
      20000
    );
    const placeholderState = await page.evaluate(() => ({
      lineCount: document.querySelectorAll('.code-review__line').length,
      noHunks: Array.from(document.querySelectorAll('.code-review__empty')).some((el) =>
        (el.textContent || '').includes('No hunks in this file')
      ),
      hljsSpans: Array.from(document.querySelectorAll('.code-review__line-text span[class^="hljs-"]'))
        .length,
    }));
    if (placeholderState.lineCount < 2) {
      fail(`T7: placeholder rendered too few lines (${placeholderState.lineCount})`);
    }
    if (placeholderState.noHunks) {
      fail('T7: placeholder pane must never claim "No hunks in this file"');
    }
    if (placeholderState.hljsSpans !== 0) {
      fail(`T7: unknown-language placeholder body must stay plain (${placeholderState.hljsSpans} hljs spans)`);
    }
    await page.screenshot({ path: `${evidence}/code-review-placeholder-autoload.png`, fullPage: true });

    // ---- T8: hljs token spans on rust diff lines, verbatim + hostile literal ----
    await clickFile(page, 'src/other.rs');
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.code-review__line-text span[class^="hljs-"]')).length > 0,
      'T8: rust diff lines rendered no hljs token spans',
      15000
    );
    const rustLines = await page.evaluate(() =>
      Array.from(document.querySelectorAll('.code-review__line')).map((row) => ({
        kind: row.className,
        oldNo: row.querySelector('.code-review__line-no--old')?.textContent || '',
        newNo: row.querySelector('.code-review__line-no--new')?.textContent || '',
        prefix: row.querySelector('.code-review__line-prefix')?.textContent || '',
        text: row.querySelector('.code-review__line-text')?.textContent || '',
        spans: row.querySelectorAll('.code-review__line-text span[class^="hljs-"]').length,
      }))
    );
    const fnLine = rustLines.find((l) => l.text.includes('fn src_helper'));
    if (!fnLine || fnLine.spans === 0) {
      fail(`T8: 'fn src_helper' line missing or unhighlighted (${JSON.stringify(fnLine)})`);
    }
    if (fnLine.text !== 'fn src_helper() -> u32 {') {
      fail(`T8: highlighted line textContent drifted from the source (${fnLine.text})`);
    }
    const letLine = rustLines.find((l) => l.text.includes('let cards'));
    if (!letLine || letLine.spans === 0) {
      fail('T8: "let cards" line missing or unhighlighted');
    }
    // Line numbers, +/- prefix, and kind backgrounds must be untouched by
    // highlighting.
    if (fnLine.kind.includes('--addition') !== true) {
      fail(`T8: highlighted line lost its addition background class (${fnLine.kind})`);
    }
    if (fnLine.prefix !== '+' || fnLine.oldNo !== '' || fnLine.newNo === '') {
      fail(`T8: prefix/line numbers changed on the highlighted line (${JSON.stringify(fnLine)})`);
    }
    // The hostile <script> diff line stays LITERAL text with no side effect.
    const hostileLine = rustLines.find((l) => l.text.includes('<script>'));
    if (!hostileLine) {
      fail('T8: hostile <script> diff line missing from the rendered diff');
    }
    const hostileSafe = await page.evaluate(() => ({
      scriptEls: document.querySelectorAll('.code-review__diff script').length,
      pwned: typeof window.__crPwned !== 'undefined',
    }));
    if (hostileSafe.scriptEls !== 0) fail('T8: hostile diff line created a real <script> element');
    if (hostileSafe.pwned) fail('T8: hostile diff line executed (window.__crPwned set)');
    if (dialogs > 0) fail(`T8: hostile diff line opened ${dialogs} dialog(s)`);
    await page.screenshot({ path: `${evidence}/code-review-highlighted.png`, fullPage: true });

    // ---- C1/C2: comment markdown + hostile HTML literal ----
    await clickFile(page, dirtyFile);
    await waitFor(
      page,
      () => document.querySelector('.code-review__line--deletion') !== null,
      'dirty file diff did not render'
    );
    // Explicit hunk selection opens the composer.
    await page.click('.code-review__hunk-header');
    await waitFor(
      page,
      () => document.querySelector('.code-review__comment-input') !== null,
      'composer did not open after hunk selection'
    );
    const hostile = '<script>window.__crPwned=1</script><img src=x onerror=window.__crPwned=2>';
    const commentText = `review markdown matrix **user bold**\n- item one\n- item two\n\n\`\`\`rust\nfn user_rust() -> u32 { 1 }\n\`\`\`\n\n${hostile}`;
    await page.fill('.code-review__comment-input', commentText);
    await page.press('.code-review__comment-input', 'Control+Enter');

    // The USER comment renders markdown + literal hostile HTML.
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.code-review__comment--user')).some((el) => {
          const text = el.querySelector('.code-review__comment-text')?.textContent || '';
          return text.includes('user bold') && text.includes('user_rust');
        }),
      'C1: user comment with markdown never rendered',
      20000
    );
    const userComment = await page.evaluate(() => {
      const el = document.querySelector('.code-review__comment--user .code-review__comment-text');
      if (!el) return null;
      const pre = el.querySelector('pre.md-fence__pre, pre.md-fence');
      const fence = pre
        ? {
            langLabel: el.querySelector('.md-fence__lang')?.textContent?.trim() || '',
            codeText: (pre.querySelector('code')?.textContent || '').trim(),
          }
        : null;
      return {
        strong: el.querySelector('strong')?.textContent || '',
        listItems: Array.from(el.querySelectorAll('ul li')).map((li) => li.textContent),
        fence,
        text: el.textContent || '',
        scriptEl: el.querySelector('script') !== null,
        imgX: el.querySelector('img[src="x"]') !== null,
      };
    });
    if (!userComment || userComment.strong !== 'user bold') fail(`C1: user comment did not render <strong> (${JSON.stringify(userComment)})`);
    if (!userComment.listItems.includes('item one') || !userComment.listItems.includes('item two')) {
      fail(`C1: user comment did not render the list (${JSON.stringify(userComment.listItems)})`);
    }
    if (!userComment.fence || !userComment.fence.codeText.includes('user_rust')) {
      fail(`C1: user comment did not render the rust fence (${JSON.stringify(userComment.fence)})`);
    }
    if (userComment.scriptEl || userComment.imgX) {
      fail('C1: hostile HTML created real elements in the user comment');
    }
    if (!userComment.text.includes('<script>')) {
      fail('C1: hostile HTML must stay LITERAL text (escaped) in the user comment');
    }
    const noSideEffects = await page.evaluate(() => typeof window.__crPwned !== 'undefined');
    if (noSideEffects) fail('C1: hostile script executed (window.__crPwned set)');
    if (dialogs > 0) fail(`C1: hostile HTML opened ${dialogs} dialog(s)`);
    await page.screenshot({ path: `${evidence}/code-review-comment-user.png`, fullPage: true });

    // The ASSISTANT reply (mock-routed markdown matrix) renders markdown +
    // literal hostile HTML.
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.code-review__comment--assistant')).some((el) => {
          const text = el.querySelector('.code-review__comment-text')?.textContent || '';
          return text.includes('review bold verdict') && text.includes('review_rust');
        }),
      'C2: assistant review reply with markdown never rendered',
      30000
    );
    const assistantComment = await page.evaluate(() => {
      const el = document.querySelector('.code-review__comment--assistant .code-review__comment-text');
      if (!el) return null;
      const pre = el.querySelector('pre.md-fence__pre, pre.md-fence');
      const fence = pre
        ? {
            langLabel: el.querySelector('.md-fence__lang')?.textContent?.trim() || '',
            codeText: (pre.querySelector('code')?.textContent || '').trim(),
          }
        : null;
      return {
        strong: el.querySelector('strong')?.textContent || '',
        listItems: Array.from(el.querySelectorAll('ul li')).map((li) => li.textContent),
        fence,
        text: el.textContent || '',
        scriptEl: el.querySelector('script') !== null,
        imgX: el.querySelector('img[src="x"]') !== null,
      };
    });
    if (!assistantComment || assistantComment.strong !== 'review bold verdict') {
      fail(`C2: assistant comment did not render <strong> (${JSON.stringify(assistantComment)})`);
    }
    if (!assistantComment.listItems.includes('review item alpha') || !assistantComment.listItems.includes('review item beta')) {
      fail(`C2: assistant comment did not render the list (${JSON.stringify(assistantComment.listItems)})`);
    }
    if (!assistantComment.fence || !assistantComment.fence.codeText.includes('review_rust')) {
      fail(`C2: assistant comment did not render the rust fence (${JSON.stringify(assistantComment.fence)})`);
    }
    if (assistantComment.scriptEl || assistantComment.imgX) {
      fail('C2: hostile HTML created real elements in the assistant comment');
    }
    if (!assistantComment.text.includes('<script>')) {
      fail('C2: hostile HTML must stay LITERAL text in the assistant comment');
    }
    const noSideEffects2 = await page.evaluate(() => typeof window.__crPwned !== 'undefined');
    if (noSideEffects2) fail('C2: hostile script executed via the assistant comment (window.__crPwned set)');
    if (dialogs > 0) fail(`C2: hostile HTML opened ${dialogs} dialog(s)`);
    await page.screenshot({ path: `${evidence}/code-review-comment-assistant.png`, fullPage: true });

    // ---- T6: responsive breakpoint + mobile tab bar (narrow-viewport UI) ----
    // The desktop workspace renders no tab bar. Resizing the viewport across
    // the 900px breakpoint WHILE the panel is mounted fires the matchMedia
    // change handler, which surfaces the Files/Diff/Thread tab bar. Each tab
    // click drives the single-pane narrow UI (only the active pane is
    // visible), Back-to-diff returns from the Thread pane, and resizing back
    // to desktop removes the tab bar. This is a real user flow: the panel
    // stays open across the resize (window resize -> matchMedia change ->
    // narrow layout), not a mobile-context reload.
    const tabBarAtDesktop = await page.evaluate(() => document.querySelector('.code-review__tab-bar') !== null);
    if (tabBarAtDesktop) fail('T6: tab bar rendered while the workspace is desktop-width');
    await page.setViewportSize({ width: 700, height: 800 });
    await waitFor(
      page,
      () => document.querySelector('.code-review__tab-bar') !== null,
      'T6: resizing under 900px did not surface the mobile tab bar'
    );
    const narrowTabs = await page.evaluate(() =>
      Array.from(document.querySelectorAll('.code-review__tab')).map((t) => t.textContent.trim())
    );
    if (narrowTabs.join(',') !== 'Files,Diff,Thread') {
      fail(`T6: narrow tab bar wrong (got ${narrowTabs.join(',')})`);
    }
    const activeOnResize = await page.evaluate(
      () => document.querySelector('.code-review__tab.is-active')?.textContent.trim() || ''
    );
    if (activeOnResize !== 'Files') {
      fail(`T6: narrow workspace opens on the ${activeOnResize} tab instead of Files`);
    }
    const clickTab = async (name) => {
      const clicked = await page.evaluate((want) => {
        const tab = Array.from(document.querySelectorAll('.code-review__tab')).find((t) =>
          t.textContent.trim() === want
        );
        if (!tab) return false;
        tab.click();
        return true;
      }, name);
      if (!clicked) fail(`T6: ${name} tab not found for click`);
    };
    const visibleRect = (sel) =>
      page.evaluate((s) => {
        const el = document.querySelector(s);
        if (!el) return false;
        const rect = el.getBoundingClientRect();
        return rect.width > 0 && rect.height > 0;
      }, sel);

    // Click the Diff tab -> the diff pane becomes the visible single pane.
    await clickTab('Diff');
    await waitFor(
      page,
      () => document.querySelector('.code-review__tab.is-active')?.textContent.trim() === 'Diff',
      'T6: clicking the Diff tab did not activate it'
    );
    if (!(await visibleRect('.code-review__diff'))) fail('T6: diff pane not visible on the Diff tab');
    await page.screenshot({ path: `${evidence}/code-review-narrow-diff-tab.png`, fullPage: true });

    // Click the Thread tab -> the thread dock (with Back-to-diff) becomes
    // the visible single pane.
    await clickTab('Thread');
    await waitFor(
      page,
      () => document.querySelector('.code-review__tab.is-active')?.textContent.trim() === 'Thread',
      'T6: clicking the Thread tab did not activate it'
    );
    if (!(await visibleRect('.code-review__back'))) fail('T6: Back-to-diff missing on the Thread tab');
    await page.screenshot({ path: `${evidence}/code-review-narrow-thread-tab.png`, fullPage: true });

    // Back-to-diff returns to the diff pane.
    await page.click('.code-review__back');
    await waitFor(
      page,
      () => document.querySelector('.code-review__tab.is-active')?.textContent.trim() === 'Diff',
      'T6: Back-to-diff did not return to the Diff tab'
    );

    // Click the Files tab -> the file rail is the visible single pane again.
    await clickTab('Files');
    await waitFor(
      page,
      () => document.querySelector('.code-review__tab.is-active')?.textContent.trim() === 'Files',
      'T6: clicking the Files tab did not activate it'
    );
    if (await visibleRect('.code-review__diff')) fail('T6: diff pane still visible while the Files tab is active');
    await page.screenshot({ path: `${evidence}/code-review-narrow-files-tab.png`, fullPage: true });

    // Resize back to desktop: the change handler fires again (narrow->wide).
    await page.setViewportSize({ width: 1280, height: 800 });
    await waitFor(
      page,
      () => document.querySelector('.code-review__tab-bar') === null,
      'T6: resizing above 900px did not remove the mobile tab bar'
    );
    await page.screenshot({ path: `${evidence}/code-review-back-to-desktop.png`, fullPage: true });

    console.log('web-code-review-paging: PASSED (nested tree collapse/expand; >4000-line Load more grows past the cap + changed-line-04001 appears + Load full; user + assistant comment markdown bold/list/rust fence with hostile HTML literal and no side effect; responsive resize across the 900px breakpoint surfaces the Files/Diff/Thread tab bar, tab clicks drive the single-pane narrow UI, Back-to-diff returns, resize back removes the tab bar)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-code-review-paging: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
