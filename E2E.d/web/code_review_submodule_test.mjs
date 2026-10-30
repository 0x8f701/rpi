// Web code-review frozen-submodule lane (playwright half of code_review_submodule.sh).
// Exactly-one code_review_open (from=review-base, includeWorkingTree=true, no to) and
// zero code_review_comment frames are counted at journey end.
// Rows: ext/lib.txt expanded (no bare ext); sib-clean file-changing SHA bump kept
// as a modified gitlink (sib-clean/file.txt not expanded); marker on the single addition line.
// Every close/cleanup error fails with context; only the favicon console category is tolerated.
import { chromium } from 'playwright';
import fs from 'node:fs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const BASE_REF = process.env.RPI_BASE_REF || 'review-base';
const DIRTY_INNER = process.env.RPI_DIRTY_INNER || 'ext/lib.txt';
const CLEAN_SIB = process.env.RPI_CLEAN_SIB || 'sib-clean';
const CLEAN_SIB_INNER = process.env.RPI_CLEAN_SIB_INNER || `${CLEAN_SIB}/file.txt`;
const MARKER = process.env.RPI_MARKER || 'SUBMOD-P0-CHANGED';
const pageErrors = [];
const consoleErrors = [];
const sentFrames = [];
const sentParseFailures = [];
let closeError = null;

class LaneFailure extends Error {}

function fail(message) {
  throw new LaneFailure(message);
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs, polling: 25 });
  } catch {
    const dump = await page
      .evaluate(() => ({
        panel: !!document.getElementById('code-review-panel'),
        busy: document.getElementById('code-review-panel')?.getAttribute('aria-busy') || '',
        label: document.querySelector('.code-review__label')?.textContent || '',
        rows: Array.from(document.querySelectorAll('button.code-review__file')).map((button) => ({
          path: button.getAttribute('data-file-path'),
          label: button.getAttribute('aria-label') || '',
          selected: button.closest('[role="treeitem"]')?.getAttribute('aria-selected') || '',
        })),
        diffPath: document.querySelector('.code-review__diff-path')?.textContent || '',
        hunks: Array.from(document.querySelectorAll('button.code-review__hunk-header')).map((button) => ({
          disabled: button.hasAttribute('disabled'),
          text: button.textContent || '',
        })),
        errors: Array.from(document.querySelectorAll('#toasts .toast--error')).map((toast) => toast.textContent || ''),
        body: (document.body?.textContent || '').slice(0, 700),
      }))
      .catch(() => ({ panel: false, body: 'page-eval-failed' }));
    const sent = sentFrames.flatMap(raw => {
      try {
        const frame = JSON.parse(raw);
        return [{ type: frame.type || '', path: frame.path || '', cursor: frame.cursor ?? null }];
      } catch {
        return [];
      }
    });
    fail(`${label} (timeout after ${timeoutMs}ms); page=${JSON.stringify(dump)} sent=${JSON.stringify(sent)}`);
  }
}

function parseSentFrame(raw) {
  try {
    return JSON.parse(raw);
  } catch {
    sentParseFailures.push(raw);
    return null;
  }
}

function hunkHeaderSelector() {
  return 'button.code-review__hunk-header:not([disabled])';
}

function composerSelector() {
  return 'textarea.code-review__comment-input';
}

async function assertNoHorizontalOverflow(page, label) {
  const surface = await page.evaluate(() => {
    const selectors = [
      'html',
      '.code-review__head',
      '.code-review__tab-bar',
      '.code-review__composer',
      '.code-review__comments-head',
      '.code-review__composer-actions',
      '#toasts',
      '#toasts .toast',
      '#toasts .toast__message',
    ];
    const overflowing = selectors.flatMap((selector) =>
      [...document.querySelectorAll(selector)]
        .filter((element) => element.getClientRects().length > 0)
        .filter((element) => element.scrollWidth > element.clientWidth + 1)
        .map((element) => ({
          selector,
          scrollWidth: element.scrollWidth,
          clientWidth: element.clientWidth,
        })),
    );
    const errorToasts = [...document.querySelectorAll('#toasts .toast--error')]
      .filter((element) => element.getClientRects().length > 0)
      .map((element) => element.textContent || '');
    const diffErrors = [...document.querySelectorAll('.code-review__window-error')]
      .filter((element) => element.getClientRects().length > 0)
      .map((element) => element.textContent || '');
    return { overflowing, errorToasts, diffErrors };
  });
  if (surface.overflowing.length > 0) {
    fail(`${label}: horizontal overflow ${JSON.stringify(surface.overflowing)}`);
  }
  if (surface.errorToasts.length > 0) {
    fail(`${label}: visible error toasts ${JSON.stringify(surface.errorToasts)}`);
  }
  if (surface.diffErrors.length > 0) {
    const fileDiffFrames = sentFrames.flatMap(raw => {
      try {
        const frame = JSON.parse(raw);
        return frame.type === 'code_review_file_diff'
          ? [{ path: frame.path, cursor: frame.cursor, maxLines: frame.maxLines }]
          : [];
      } catch {
        return [];
      }
    });
    fail(`${label}: visible file diff errors ${JSON.stringify(surface.diffErrors)} frames=${JSON.stringify(fileDiffFrames)}`);
  }
}

async function scrollDiffPane(page, label) {
  const scrolled = await page.evaluate(() => {
    const el = document.querySelector('.code-review__diff:not(.is-hidden)');
    if (!el) return 'no-diff';
    el.scrollTop = 0;
    const overflows = el.scrollHeight > el.clientHeight;
    el.scrollTop = el.scrollHeight;
    return { overflows, top: el.scrollTop, height: el.scrollHeight, client: el.clientHeight };
  });
  if (scrolled === 'no-diff') fail(`${label}: diff viewport missing for scrolling`);
  if (!scrolled.overflows) {
    fail(`${label}: diff content does not overflow its viewport (scrollHeight=${scrolled.height} clientHeight=${scrolled.client})`);
  }
  if (scrolled.top <= 0) {
    fail(`${label}: diff pane did not scroll (scrollTop=${scrolled.top})`);
  }
}

async function clickTab(page, name, label) {
  const tab = page.locator('button.code-review__tab', { hasText: name });
  const count = await tab.count();
  if (count !== 1) {
    fail(`${label}: expected exactly one "${name}" tab (found ${count})`);
  }
  await tab.click();
  await waitFor(
    page,
    (want) => {
      const el = Array.from(document.querySelectorAll('button.code-review__tab')).find((t) =>
        t.textContent.trim() === want,
      );
      return !!el && (el.getAttribute('aria-pressed') === 'true'
        || el.classList.contains('is-active'));
    },
    `${label}: ${name} tab never became active`,
    10000,
    name,
  );
}

async function assertMobilePaneIsolation(page, label, visible) {
  const panes = await page.evaluate(() => ({
    files: [...document.querySelectorAll('.code-review__files')].some(
      (el) => el.getClientRects().length > 0,
    ),
    diff: [...document.querySelectorAll('.code-review__diff:not(.is-hidden)')].some(
      (el) => el.getClientRects().length > 0,
    ),
    comments: [...document.querySelectorAll('.code-review__comments:not(.is-hidden)')].some(
      (el) => el.getClientRects().length > 0,
    ),
    pressed: document.querySelectorAll('button.code-review__tab[aria-pressed="true"]').length,
  }));
  const want = {
    files: { files: true, diff: false, comments: false },
    diff: { files: false, diff: true, comments: false },
    thread: { files: false, diff: false, comments: true },
  }[visible];
  const got = { files: panes.files, diff: panes.diff, comments: panes.comments };
  if (panes.pressed !== 1 || JSON.stringify(got) !== JSON.stringify(want)) {
    fail(
      `${label}: mobile pane isolation violated (pressed=${panes.pressed}): ${JSON.stringify({ got, want })}`,
    );
  }
}

async function main() {
  let browser = null;
  try {
    if (!url) fail('RPI_URL is required');
    const launchOptions = chromePath ? { executablePath: chromePath } : {};
    browser = await chromium.launch(launchOptions);
    const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
    if (token) {
      await page.addInitScript((t) => {
        window.localStorage.setItem('rpi-web-token', t);
        window.localStorage.setItem(`rpi-web-token:${window.location.host}`, t);
      }, token);
    }
    page.on('pageerror', (err) => pageErrors.push(err.message));
    page.on('console', (message) => {
      if (message.type() === 'error') consoleErrors.push(message.text());
    });
    page.on('websocket', (socket) => {
      socket.on('framesent', (frame) => {
        if (typeof frame.payload === 'string') sentFrames.push(frame.payload);
      });
    });
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'WS did not reach connected',
    );

    await page.fill('#prompt-input', `/code-review ${BASE_REF}`);
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') !== null,
      'code-review panel did not open',
      30000,
    );
    await waitFor(
      page,
      (base) => {
        const label = document.querySelector('.code-review__label')?.textContent || '';
        return label.includes(base) && label.includes('working tree');
      },
      'comparison label missing',
      30000,
      BASE_REF,
    );

    await waitFor(
      page,
      (inner) => !!document.querySelector(`button.code-review__file[data-file-path="${inner}"]`),
      'expanded dirty submodule file row missing',
      30000,
      DIRTY_INNER,
    );
    await waitFor(
      page,
      (sib) => {
        const row = document.querySelector(`button.code-review__file[data-file-path="${sib}"]`);
        if (!row) return false;
        const label = row.getAttribute('aria-label') || '';
        return /^modified:/i.test(label.trim()) && label.includes(sib);
      },
      'clean bumped submodule row missing or not modified',
      30000,
      CLEAN_SIB,
    );
    const rows = await page.evaluate(() =>
      Array.from(document.querySelectorAll('button.code-review__file')).map((b) => ({
        path: b.getAttribute('data-file-path'),
        label: b.getAttribute('aria-label') || '',
        status: b.querySelector('.code-review__file-status')?.textContent || '',
      })),
    );
    const paths = rows.map((row) => row.path);
    if (!paths.includes(DIRTY_INNER)) fail(`ext/lib.txt row missing: ${JSON.stringify(rows)}`);
    if (!paths.includes(CLEAN_SIB)) fail(`sib-clean gitlink row missing: ${JSON.stringify(rows)}`);
    if (paths.includes('ext')) {
      fail(`bare gitlink path ext must not be its own changed row (expanded into ext/lib.txt): ${JSON.stringify(paths)}`);
    }
    if (paths.some((path) => path !== CLEAN_SIB && path.startsWith(`${CLEAN_SIB}/`))) {
      fail(`clean SHA-bump child path ${CLEAN_SIB_INNER} must not be expanded (gitlink ${CLEAN_SIB} retained): ${JSON.stringify(rows)}`);
    }
    if (rows.length !== 2) {
      fail(`changed rows must be exactly ext/lib.txt + sib-clean gitlink (got ${JSON.stringify(rows)})`);
    }
    const sibRow = rows.find((row) => row.path === CLEAN_SIB);
    if (!/^modified:/i.test((sibRow.label || '').trim()) && !/^modified$/i.test((sibRow.status || '').trim().split(/\s+/)[0])) {
      fail(`clean bumped submodule must read as modified gitlink: ${JSON.stringify(sibRow)}`);
    }

    await page.click(`button.code-review__file[data-file-path="${DIRTY_INNER}"]`);
    await waitFor(
      page,
      (selector) => !!document.querySelector(selector),
      'hunk never became selectable',
      30000,
      hunkHeaderSelector(),
    );
    await waitFor(
      page,
      (marker) => (document.body?.textContent || '').includes(marker),
      'marker not yet rendered in the diff',
      30000,
      MARKER,
    );
    const markerLines = await page.evaluate(
      (marker) =>
        Array.from(document.querySelectorAll('.code-review__line--addition')).filter((el) =>
          (el.textContent || '').includes(marker),
        ).length,
      MARKER,
    );
    if (markerLines !== 1) {
      fail(`marker must appear on exactly one addition line (got ${markerLines})`);
    }
    const additionLines = await page.evaluate(() =>
      document.querySelectorAll('.code-review__line--addition').length,
    );
    if (additionLines !== 1) {
      fail(`expanded hunk must carry exactly one addition line (got ${additionLines})`);
    }

    const measure = () =>
      page.evaluate(
        (marker) => {
          const lineEl = Array.from(
            document.querySelectorAll('.code-review__line--addition'),
          ).find((el) => (el.textContent || '').includes(marker));
          if (!lineEl) return null;
          const card = lineEl.closest('.code-review__hunk');
          const lineStyle = window.getComputedStyle(lineEl);
          const cardStyle = card ? window.getComputedStyle(card) : null;
          return {
            lineBg: lineStyle.backgroundColor,
            cardBg: cardStyle ? cardStyle.backgroundColor : '',
          };
        },
        MARKER,
      );
    const tints = [];
    const setTheme = async (theme) => {
      await page.evaluate((want) => {
        if (want === 'light') document.documentElement.setAttribute('data-theme', 'light');
        else document.documentElement.removeAttribute('data-theme');
      }, theme);
      await waitFor(
        page,
        (want) =>
          document.documentElement.getAttribute('data-theme') ===
          (want === 'light' ? 'light' : null),
        `theme ${theme} did not apply`,
        10000,
        theme,
      );
      await waitFor(
        page,
        (marker) => {
          const el = Array.from(
            document.querySelectorAll('.code-review__line--addition'),
          ).find((line) => (line.textContent || '').includes(marker));
          return (
            !!el && window.getComputedStyle(el).backgroundColor !== 'rgba(0, 0, 0, 0)'
          );
        },
        `theme ${theme} addition tint never repainted`,
        10000,
        MARKER,
      );
    };
    for (const theme of ['dark', 'light']) {
      await setTheme(theme);
      const m = await measure();
      if (!m) fail(`no addition line to measure (theme=${theme})`);
      tints.push({ theme, ...m });
      const visibleTint =
        (m.lineBg || '').length > 0 &&
        m.lineBg !== 'rgba(0, 0, 0, 0)' &&
        m.lineBg !== 'transparent' &&
        m.lineBg !== m.cardBg;
      if (!visibleTint) {
        fail(`addition tint not visible (theme=${theme}): ${JSON.stringify(m)}`);
      }
    }
    await setTheme('dark');

    await page.click(hunkHeaderSelector());
    await waitFor(
      page,
      (selector) => !!document.querySelector(selector),
      'comment composer never became enabled',
      30000,
      `${composerSelector()}:not([disabled])`,
    );
    const draft = 'submodule e2e draft (never submitted)';
    await page.fill(composerSelector(), draft);
    const typed = await page.inputValue(composerSelector());
    if (typed !== draft) {
      fail(`composer keystrokes did not land (typed ${JSON.stringify(typed)})`);
    }
    const enabled = await page.evaluate((sel) => {
      const el = document.querySelector(sel);
      return {
        disabled: el ? el.disabled : true,
        value: el ? el.value : '',
      };
    }, composerSelector());
    if (enabled.disabled || enabled.value !== draft) {
      fail(`composer not editable after selection: ${JSON.stringify(enabled)}`);
    }
    const desktopPanes = await page.evaluate(() => {
      const rectOf = (selector) => {
        const el = document.querySelector(selector);
        if (!el) return null;
        const rect = el.getBoundingClientRect();
        return { left: rect.left, right: rect.right, width: rect.width, height: rect.height };
      };
      return {
        tabs: document.querySelectorAll('button.code-review__tab').length,
        files: rectOf('.code-review__files'),
        diff: rectOf('.code-review__diff:not(.is-hidden)'),
        comments: rectOf('.code-review__comments:not(.is-hidden)'),
      };
    });
    if (desktopPanes.tabs !== 0) {
      fail(`desktop must not render mobile tab buttons (found ${desktopPanes.tabs})`);
    }
    for (const name of ['files', 'diff', 'comments']) {
      const pane = desktopPanes[name];
      if (!pane || !(pane.width > 0) || !(pane.height > 0)) {
        fail(`desktop pane ${name} missing or not visible: ${JSON.stringify(desktopPanes)}`);
      }
    }
    if (
      desktopPanes.files.right > desktopPanes.diff.left + 1 ||
      desktopPanes.diff.right > desktopPanes.comments.left + 1
    ) {
      fail(
        `desktop panes must be distinct and non-overlapping (files.right<=diff.left<=comments.left, 1px tolerance): ${JSON.stringify(desktopPanes)}`,
      );
    }

    await scrollDiffPane(page, 'desktop');
    await assertNoHorizontalOverflow(page, 'desktop');
    await page.screenshot({ path: `${evidence}/01-desktop-review-base.png`, fullPage: true });

    await page.setViewportSize({ width: 390, height: 844 });
    await waitFor(
      page,
      () => document.querySelectorAll('button.code-review__tab').length === 3,
      'mobile tab bar missing at 390px',
      10000,
    );
    await clickTab(page, 'Files', 'mobile Files');
    const filesVisible = await page.evaluate(() => {
      const el = document.querySelector('.code-review__files');
      if (!el) return false;
      const rect = el.getBoundingClientRect();
      return rect.width > 0 && rect.height > 0;
    });
    if (!filesVisible) fail('Files pane not visible on the mobile Files tab');
    await assertMobilePaneIsolation(page, 'mobile Files', 'files');
    await page.screenshot({ path: `${evidence}/02-mobile-files-tab.png`, fullPage: true });
    await assertNoHorizontalOverflow(page, 'mobile files');

    await clickTab(page, 'Diff', 'mobile Diff');
    const diffVisible = await page.evaluate(() => {
      const el = document.querySelector('.code-review__diff:not(.is-hidden)');
      if (!el) return false;
      const rect = el.getBoundingClientRect();
      return rect.width > 0 && rect.height > 0;
    });
    if (!diffVisible) fail('Diff pane not visible on the mobile Diff tab');
    await assertMobilePaneIsolation(page, 'mobile Diff', 'diff');
    await waitFor(
      page,
      (marker) => (document.body?.textContent || '').includes(marker),
      'marker missing on the mobile Diff tab',
      10000,
      MARKER,
    );
    await scrollDiffPane(page, 'mobile diff');
    const diffScroll = await page.evaluate(() => {
      const el = document.querySelector('.code-review__diff:not(.is-hidden)');
      return el ? { top: el.scrollTop, height: el.scrollHeight, client: el.clientHeight } : null;
    });
    if (!diffScroll || diffScroll.top <= 0) {
      fail(`mobile Diff pane did not stay scrolled: ${JSON.stringify(diffScroll)}`);
    }
    await assertNoHorizontalOverflow(page, 'mobile diff');
    await page.screenshot({ path: `${evidence}/03-mobile-diff-tab.png`, fullPage: true });

    await clickTab(page, 'Thread', 'mobile Thread');
    const threadVisible = await page.evaluate(() => {
      const el = document.querySelector('.code-review__comments:not(.is-hidden)');
      if (!el) return false;
      const rect = el.getBoundingClientRect();
      return rect.width > 0 && rect.height > 0;
    });
    if (!threadVisible) fail('Thread pane not visible on the mobile Thread tab');
    await assertMobilePaneIsolation(page, 'mobile Thread', 'thread');
    const backVisible = await page.evaluate(() => {
      const el = document.querySelector('.code-review__back');
      if (!el) return false;
      const rect = el.getBoundingClientRect();
      return rect.width > 0 && rect.height > 0;
    });
    if (!backVisible) fail('Back-to-diff control missing on the mobile Thread tab');
    const composerMobile = await page.evaluate((sel) => {
      const el = document.querySelector(sel);
      return el ? { disabled: el.disabled, value: el.value } : null;
    }, composerSelector());
    if (!composerMobile || composerMobile.disabled || !composerMobile.value) {
      fail(`composer lost its enabled draft on the Thread tab: ${JSON.stringify(composerMobile)}`);
    }
    await assertNoHorizontalOverflow(page, 'mobile thread');
    await page.screenshot({ path: `${evidence}/04-mobile-thread-tab.png`, fullPage: true });

    const draftAfter = await page.inputValue(composerSelector());
    if (draftAfter !== draft) {
      fail(`composer draft changed across the tab journey (${JSON.stringify(draftAfter)})`);
    }

    const openFrames = sentFrames
      .map(parseSentFrame)
      .filter((frame) => frame && frame.type === 'code_review_open');
    if (openFrames.length !== 1) {
      fail(`exactly one code_review_open frame across the whole journey (got ${openFrames.length}); non-JSON frames: ${JSON.stringify(sentParseFailures)}`);
    }
    const open = openFrames[0];
    if (open.from !== BASE_REF || open.includeWorkingTree !== true || Object.hasOwn(open, 'to')) {
      fail(`code_review_open frame mismatch: ${JSON.stringify(open)}`);
    }

    const commentFrames = sentFrames
      .map(parseSentFrame)
      .filter((frame) => frame && frame.type === 'code_review_comment');
    if (commentFrames.length !== 0) {
      fail(`zero comment submits required across the whole journey (got ${commentFrames.length}); non-JSON frames: ${JSON.stringify(sentParseFailures)}`);
    }

    if (pageErrors.length) fail(`page errors: ${JSON.stringify(pageErrors)}`);
    const clean = consoleErrors.filter((t) => !t.includes('favicon'));
    if (clean.length) fail(`console errors: ${JSON.stringify(clean)}`);
    fs.writeFileSync(
      `${evidence}/code-review-submodule-report.json`,
      JSON.stringify(
        {
          openFrame: open,
          openFrameCount: openFrames.length,
          commentFrameCount: commentFrames.length,
          sentParseFailures,
          rows,
          markerLines,
          additionLines,
          tints,
          composerDraftPreserved: draftAfter === draft,
          screenshots: [
            '01-desktop-review-base.png',
            '02-mobile-files-tab.png',
            '03-mobile-diff-tab.png',
            '04-mobile-thread-tab.png',
          ],
          pageErrors,
          consoleErrors: clean,
        },
        null,
        2,
      ),
    );
    console.log(
      'web-code-review-submodule: PASSED (exactly one open frame from=review-base includeWorkingTree=true no to; zero comment submits; expanded ext/lib.txt with the SUBMOD marker as the single tinted addition; no bare ext row; clean file-changing SHA bump of sib-clean kept as modified gitlink; sib-clean/file.txt not expanded; enabled composer with preserved draft, never submitted; desktop 1280x800 three-pane Files/Diff/Hunk comments with no tab bar + mobile 390x844 Files/Diff/Thread tabs, real scroll, no overflow; 4 screenshots; zero browser errors)',
    );
  } finally {
    if (browser) {
      try {
        await browser.close();
      } catch (err) {
        closeError = err && err.message ? err.message : String(err);
        console.error(`web-code-review-submodule: FAIL: browser close error: ${closeError}`);
        process.exitCode = 3;
      }
    }
  }
}

main().catch((err) => {
  const detail = closeError ? `; browser close error: ${closeError}` : '';
  if (err instanceof LaneFailure) {
    console.error(`web-code-review-submodule: FAIL: ${err.message}${detail}`);
    process.exitCode = 2;
  } else {
    console.error(
      `web-code-review-submodule: FAIL: ${err && err.message ? err.message : err}${detail}`,
    );
    process.exitCode = 3;
  }
});
