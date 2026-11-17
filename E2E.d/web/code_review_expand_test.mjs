// Web code-review gap-expand regression (playwright half of
// code_review_expand.sh).
//
// A 50-line dirty file with a change around line 40 must expose a gap bar
// above the hunk (the fixture renders a trailing band below it too; every
// interaction here targets the FIRST .code-review__gap band, the leading
// gap). Clicking ▼ ("Expand toward hunk") loads snapshot context so the
// unique line-17 marker appears, the hunk header/edit stay the same, and
// every code_review_file_context expand count is 1..20. ▼ loads the
// 17..36 window, leaving exactly 16 gap lines — and EITHER remaining
// button (▲ "Expand toward file top" or "Expand remaining lines in this
// gap") consumes those 16 lines in one count-16 wire frame and clears the
// gap bar, so the two buttons can never be clicked in sequence on one
// gap. The lane therefore runs two independent reset journeys: ▼ then ▲,
// and, after a full page reset, ▼ then Expand remaining. Every context
// page carries matching old/new line numbers. Desktop (≥1280) and mobile
// (390×844). Zero page/console errors.
import { chromium } from 'playwright';
import fs from 'node:fs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const DIRTY_FILE = process.env.RPI_DIRTY_FILE || 'wide.rs';
const EXPAND_MARKER = process.env.RPI_EXPAND_MARKER || 'EXPAND-ABOVE-17 unique-gap-marker';
const HUNK_MARKER = process.env.RPI_HUNK_MARKER || 'wide-line-40-EDIT';
const TOWARD_HUNK = 'Expand toward hunk';
const TOWARD_TOP = 'Expand toward file top';
const REMAINING = 'Expand remaining lines in this gap';
const pageErrors = [];
const consoleErrors = [];

function fail(message) {
  console.error(`web-code-review-expand: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs, polling: 25 });
  } catch {
    const dump = await page
      .evaluate(() => ({
        panel: !!document.getElementById('code-review-panel'),
        gap: !!document.querySelector('.code-review__gap-bar'),
        hunk: document.querySelector('.code-review__hunk-header')?.textContent || '',
        body: (document.body?.textContent || '').slice(0, 400),
      }))
      .catch(() => ({ panel: false, body: 'page-eval-failed' }));
    fail(`${label} (timeout after ${timeoutMs}ms); page=${JSON.stringify(dump)}`);
  }
}

function parseFrames(page, kind) {
  return page.evaluate(
    (frameKind) =>
      window.__expandFrames[frameKind]
        .map((raw) => {
          try {
            return JSON.parse(raw);
          } catch {
            return null;
          }
        })
        .filter((frame) => frame !== null),
    kind,
  );
}

async function installSocketHarness(page) {
  await page.addInitScript(() => {
    const NativeWebSocket = window.WebSocket;
    const frames = { sent: [], received: [] };
    class TracedWebSocket extends NativeWebSocket {
      constructor(...args) {
        super(...args);
        this.addEventListener('message', (event) => {
          if (typeof event.data === 'string') frames.received.push(event.data);
        });
      }
      send(data) {
        if (typeof data === 'string') frames.sent.push(data);
        return super.send(data);
      }
    }
    for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
      Object.defineProperty(TracedWebSocket, key, { value: NativeWebSocket[key] });
    }
    window.WebSocket = TracedWebSocket;
    window.__expandFrames = frames;
  });
}

function contextExpandFrames(sent) {
  return sent.filter(
    (frame) => frame && frame.type === 'code_review_file_context' && Number(frame.count) > 0,
  );
}

function linesLoaded(frames) {
  return frames.reduce((sum, frame) => sum + Number(frame.count), 0);
}

async function contextPageFor(page, request) {
  // The correlated reply may still be in flight when the caller only
  // waited for a DOM signal (or none at all, like the count-0 probe), so
  // wait for THE response carrying this request's id before the strict
  // find below — never a sleep, never a weaker gate.
  await waitFor(
    page,
    (id) => {
      try {
        return window.__expandFrames.received.some((raw) => {
          const frame = JSON.parse(raw);
          return frame.type === 'response' && frame.id === id;
        });
      } catch {
        return false;
      }
    },
    `no response for code_review_file_context ${request.id}`,
    30000,
    request.id,
  );
  const received = await parseFrames(page, 'received');
  const reply = received.find(
    (frame) => frame && frame.type === 'response' && frame.id === request.id,
  );
  if (reply.success !== true || reply.command !== 'code_review_file_context') {
    fail(`file context response rejected: ${JSON.stringify(reply)}`);
  }
  const data = reply.data;
  if (
    !data
    || data.path !== request.path
    || data.binary !== false
    || !Array.isArray(data.lines)
    || data.lines.length !== Number(request.count)
  ) {
    fail(
      `file context page does not match request ${JSON.stringify(request)}: ${JSON.stringify(data)}`,
    );
  }
  return data;
}

function assertExpandWindow(label, before, after) {
  const added = after.slice(before.length);
  if (added.length !== 1) {
    fail(`${label}: expected exactly one new code_review_file_context, got ${added.length}`);
  }
  const frame = added[0];
  const count = Number(frame.count);
  if (!Number.isInteger(count) || count < 1 || count > 20) {
    fail(`${label}: expand count not in 1..20: ${JSON.stringify(frame)}`);
  }
  if (!Number.isInteger(Number(frame.oldStart)) || !Number.isInteger(Number(frame.newStart))) {
    fail(`${label}: expand window missing integer starts: ${JSON.stringify(frame)}`);
  }
  const prior = before.filter((entry) => entry.path === frame.path);
  const overlap = prior.some((entry) => {
    const start = Number(entry.newStart);
    const end = start + Number(entry.count);
    const next = Number(frame.newStart);
    return next < end && next + count > start;
  });
  if (overlap) {
    fail(`${label}: expand window overlaps a prior request: ${JSON.stringify(frame)}`);
  }
  return frame;
}

/** The leading gap band above the hunk; the fixture also renders a trailing one. */
function leadingGapButton(page, ariaLabel) {
  return page
    .locator('.code-review__gap')
    .first()
    .locator(`button[aria-label="${ariaLabel}"]`);
}

/** Identity + remaining label of the leading gap band before any expand. */
async function gapSnapshot(page, journeyLabel) {
  await waitFor(
    page,
    () => !!document.querySelector('button[aria-label="Expand toward hunk"]'),
    `${journeyLabel}: gap bar Expand toward hunk missing`,
    30000,
  );
  return page.evaluate(() => {
    const gap = document.querySelector('.code-review__gap');
    const button = (name) => gap?.querySelector(`button[aria-label="${name}"]`) || null;
    return {
      hunkHeader: document.querySelector('.code-review__hunk-header')?.textContent || '',
      remainingLabel: button('Expand remaining lines in this gap')?.textContent || '',
      remainingPresent: !!button('Expand remaining lines in this gap'),
      towardHunk: button('Expand toward hunk')?.textContent || '',
      towardTop: button('Expand toward file top')?.textContent || '',
      gapBarPresent: !!gap?.querySelector('.code-review__gap-bar'),
    };
  });
}

/**
 * The panel probes the full file size with a count=0 context request before
 * any expand; the fixture must be the 50-line wide.rs for the window math.
 */
async function gapTotalFor(page, journeyLabel) {
  await waitFor(
    page,
    () => {
      try {
        return window.__expandFrames.sent.some((raw) => {
          const frame = JSON.parse(raw);
          return frame.type === 'code_review_file_context' && Number(frame.count) === 0;
        });
      } catch {
        return false;
      }
    },
    `${journeyLabel}: count-0 line-count probe missing`,
    30000,
  );
  const sent = await parseFrames(page, 'sent');
  const probe = sent.find(
    (frame) => frame && frame.type === 'code_review_file_context' && Number(frame.count) === 0,
  );
  const data = await contextPageFor(page, probe);
  const total = Number(data.newLineCount);
  if (total !== 50) {
    fail(`${journeyLabel}: fixture newLineCount is not 50: ${JSON.stringify(data)}`);
  }
  return total;
}

/**
 * Assert the reply is exactly the start..end context run with matching
 * old/new line numbers (the fixture lines above the edit are unchanged).
 */
async function assertContextWindow(page, label, request, start, end) {
  const data = await contextPageFor(page, request);
  const lines = data.lines;
  if (lines.length !== end - start + 1) {
    fail(
      `${label}: expected the ${start}..${end} run (${end - start + 1} rows), got ${lines.length}: ${JSON.stringify(lines)}`,
    );
  }
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const oldNo = Number(line.oldNo);
    const newNo = Number(line.newNo);
    if (!Number.isInteger(oldNo) || !Number.isInteger(newNo)) {
      fail(`${label}: context line missing integer old/new numbers: ${JSON.stringify(line)}`);
    }
    if (oldNo !== newNo) {
      fail(`${label}: unchanged context line must keep old == new numbering: ${JSON.stringify(line)}`);
    }
    if (newNo !== start + i) {
      fail(`${label}: context window is not the ${start}..${end} run: ${JSON.stringify(line)}`);
    }
  }
  return data;
}

/** Open /review, select the fixture file, and wait for the leading gap bar. */
async function openReviewGap(page, journeyLabel) {
  await page.fill('#prompt-input', '/review');
  await page.press('#prompt-input', 'Enter');
  await waitFor(
    page,
    () => document.getElementById('code-review-panel') !== null,
    `${journeyLabel}: code-review panel did not open`,
    30000,
  );
  await waitFor(
    page,
    () => {
      const label = document.querySelector('.code-review__label')?.textContent || '';
      return label.includes('HEAD') && label.includes('working tree');
    },
    `${journeyLabel}: comparison label missing`,
    30000,
  );
  await waitFor(
    page,
    (dirty) => !!document.querySelector(`button.code-review__file[data-file-path="${dirty}"]`),
    `${journeyLabel}: dirty file row missing`,
    30000,
    DIRTY_FILE,
  );
  await page.click(`button.code-review__file[data-file-path="${DIRTY_FILE}"]`);
  await waitFor(
    page,
    () => !!document.querySelector('button.code-review__hunk-header:not([disabled])'),
    `${journeyLabel}: hunk never became selectable`,
    30000,
  );
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
    await installSocketHarness(page);
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
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'WS did not reach connected',
    );

    // ---- Journey A: ▼ toward hunk, then ▲ toward file top ----
    await openReviewGap(page, 'journey A');
    const before = await gapSnapshot(page, 'journey A');
    if (before.towardHunk.trim() !== '\u25bc') {
      fail(`\u25bc must mean Expand toward hunk, got ${JSON.stringify(before.towardHunk)}`);
    }
    if (before.towardTop.trim() !== '▲') {
      fail(`▲ must mean Expand toward file top, got ${JSON.stringify(before.towardTop)}`);
    }
    if (!before.hunkHeader.includes('@@')) {
      fail(`hunk header missing @@: ${JSON.stringify(before.hunkHeader)}`);
    }
    if (!before.remainingPresent) {
      fail('gap remaining control missing before expand');
    }
    const remainingBefore = Number.parseInt(before.remainingLabel, 10);
    if (!Number.isFinite(remainingBefore) || remainingBefore <= 0) {
      fail(`gap remaining count missing: ${JSON.stringify(before.remainingLabel)}`);
    }
    const gapTotal = await gapTotalFor(page, 'journey A');
    const bodyBefore = await page.evaluate(() => document.body?.textContent || '');
    if (bodyBefore.includes(EXPAND_MARKER)) {
      fail('line-17 marker was already visible before expand (gap was not collapsed)');
    }
    if (!bodyBefore.includes(HUNK_MARKER)) {
      fail(`hunk edit marker missing before expand: ${HUNK_MARKER}`);
    }
    await page.screenshot({ path: `${evidence}/01-desktop-gap-before.png`, fullPage: true });

    const sentBefore = contextExpandFrames(await parseFrames(page, 'sent')).length;
    await leadingGapButton(page, TOWARD_HUNK).click();
    await waitFor(
      page,
      (marker) => (document.body?.textContent || '').includes(marker),
      'expanded context never showed the line-17 marker',
      30000,
      EXPAND_MARKER,
    );

    const after = await page.evaluate((marker) => {
      const gap = document.querySelector('.code-review__gap');
      const gapLines = [...(gap?.querySelectorAll('.code-review__line') || [])].map(
        (el) => el.textContent || '',
      );
      return {
        hunkHeader: document.querySelector('.code-review__hunk-header')?.textContent || '',
        remainingLabel: gap?.querySelector('button[aria-label="Expand remaining lines in this gap"]')
          ?.textContent || '',
        remainingPresent: !!gap?.querySelector(
          'button[aria-label="Expand remaining lines in this gap"]',
        ),
        markerInGap: gapLines.some((text) => text.includes(marker)),
      };
    }, EXPAND_MARKER);
    if (after.hunkHeader !== before.hunkHeader) {
      fail(
        `hunk identity changed after expand: before=${JSON.stringify(before.hunkHeader)} after=${JSON.stringify(after.hunkHeader)}`,
      );
    }
    if (!after.markerInGap) {
      fail('line-17 marker rendered outside the gap band');
    }
    const bodyAfter = await page.evaluate(() => document.body?.textContent || '');
    if (!bodyAfter.includes(HUNK_MARKER)) {
      fail('hunk edit marker disappeared after expand');
    }

    const sent = contextExpandFrames(await parseFrames(page, 'sent'));
    if (sent.length <= sentBefore) {
      fail('no code_review_file_context expand frame after ▼');
    }
    const oversize = sent.filter((frame) => Number(frame.count) > 20);
    if (oversize.length) {
      fail(`expand count exceeded 20: ${JSON.stringify(oversize)}`);
    }
    const lastExpand = assertExpandWindow('toward hunk', sent.slice(0, sentBefore), sent);
    if (
      Number(lastExpand.count) !== 20
      || Number(lastExpand.newStart) !== 17
      || Number(lastExpand.oldStart) !== 17
    ) {
      fail(`▼ window is not the 20-line 17..36 run: ${JSON.stringify(lastExpand)}`);
    }
    const hunkPage = await assertContextWindow(page, '▼ toward hunk', lastExpand, 17, 36);
    const markerLine = hunkPage.lines.find((line) =>
      String(line.text || '').includes(EXPAND_MARKER),
    );
    if (!markerLine || Number(markerLine.newNo) !== 17) {
      fail(`line-17 marker not on new line 17 of the ▼ page: ${JSON.stringify(hunkPage.lines)}`);
    }
    const loadedAfterHunk = linesLoaded(sent.slice(sentBefore));
    const remainingAfter = remainingBefore - loadedAfterHunk;
    if (!after.remainingPresent || remainingAfter !== 16) {
      fail(`▼ must leave exactly 16 remaining lines (loaded ${loadedAfterHunk} of ${remainingBefore}, label ${JSON.stringify(after.remainingLabel)})`);
    }
    const parsedRemaining = Number.parseInt(after.remainingLabel, 10);
    if (parsedRemaining !== remainingAfter) {
      fail(
        `remaining label ${JSON.stringify(after.remainingLabel)} does not match the wire remainder ${remainingAfter}`,
      );
    }
    if (remainingAfter >= remainingBefore) {
      fail(`remaining count did not shrink after ▼ (${remainingBefore} -> ${remainingAfter})`);
    }
    await page.screenshot({ path: `${evidence}/02-desktop-gap-after.png`, fullPage: true });

    const gapLinesBeforeTop = await page.evaluate(
      () => document.querySelector('.code-review__gap')?.querySelectorAll('.code-review__line').length || 0,
    );
    await leadingGapButton(page, TOWARD_TOP).click();
    await waitFor(
      page,
      () => {
        const gap = document.querySelector('.code-review__gap');
        return gap !== null && gap.querySelector('.code-review__gap-bar') === null;
      },
      '▲ never cleared the leading gap bar',
      30000,
    );
    const sentAfterTop = contextExpandFrames(await parseFrames(page, 'sent'));
    const topExpand = assertExpandWindow('toward file top', sent, sentAfterTop);
    if (
      Number(topExpand.count) !== 16
      || Number(topExpand.newStart) !== 1
      || Number(topExpand.oldStart) !== 1
    ) {
      fail(`▲ window is not the remaining 16-line 1..16 run: ${JSON.stringify(topExpand)}`);
    }
    await assertContextWindow(page, '▲ toward file top', topExpand, 1, 16);
    const afterTop = await page.evaluate((marker) => {
      const gap = document.querySelector('.code-review__gap');
      const gapLines = [...(gap?.querySelectorAll('.code-review__line') || [])].map(
        (el) => el.textContent || '',
      );
      return {
        hunkHeader: document.querySelector('.code-review__hunk-header')?.textContent || '',
        gapBarPresent: !!gap?.querySelector('.code-review__gap-bar'),
        gapLines: gapLines.length,
        markerInGap: gapLines.some((text) => text.includes(marker)),
      };
    }, EXPAND_MARKER);
    if (afterTop.hunkHeader !== before.hunkHeader) {
      fail(
        `hunk header changed after ▲: before=${JSON.stringify(before.hunkHeader)} after=${JSON.stringify(afterTop.hunkHeader)}`,
      );
    }
    if (afterTop.gapBarPresent) {
      fail('gap bar still present after ▲');
    }
    if (afterTop.gapLines !== gapLinesBeforeTop + 16) {
      fail(
        `▲ did not add the remaining 16 lines (${gapLinesBeforeTop} -> ${afterTop.gapLines})`,
      );
    }
    if (!afterTop.markerInGap) {
      fail('line-17 marker left the gap band after ▲');
    }
    const bodyAfterTop = await page.evaluate(() => document.body?.textContent || '');
    if (!bodyAfterTop.includes(HUNK_MARKER)) {
      fail('hunk edit marker disappeared after ▲');
    }
    await page.screenshot({ path: `${evidence}/02b-desktop-gap-after-top.png`, fullPage: true });

    // ---- Journey B: full page reset, ▼ then Expand remaining ----
    await page.reload({ waitUntil: 'domcontentloaded' });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing after reset');
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'WS did not reach connected after reset',
    );
    await openReviewGap(page, 'journey B');
    const beforeB = await gapSnapshot(page, 'journey B');
    if (beforeB.hunkHeader !== before.hunkHeader) {
      fail(
        `reset produced a different hunk header: ${JSON.stringify({ before: before.hunkHeader, after: beforeB.hunkHeader })}`,
      );
    }
    if (!beforeB.remainingPresent) {
      fail('gap remaining control missing after reset');
    }
    const remainingBeforeB = Number.parseInt(beforeB.remainingLabel, 10);
    if (!Number.isFinite(remainingBeforeB) || remainingBeforeB !== remainingBefore) {
      fail(
        `reset did not restore the leading gap: label ${JSON.stringify(beforeB.remainingLabel)}`,
      );
    }
    const gapTotalB = await gapTotalFor(page, 'journey B');
    if (gapTotalB !== gapTotal) {
      fail(`reset changed the fixture size: ${gapTotal} -> ${gapTotalB}`);
    }
    const bodyBeforeB = await page.evaluate(() => document.body?.textContent || '');
    if (bodyBeforeB.includes(EXPAND_MARKER)) {
      fail('line-17 marker visible before the journey-B expand (reset did not collapse the gap)');
    }
    if (!bodyBeforeB.includes(HUNK_MARKER)) {
      fail(`hunk edit marker missing after reset: ${HUNK_MARKER}`);
    }

    const sentBeforeB = contextExpandFrames(await parseFrames(page, 'sent')).length;
    await leadingGapButton(page, TOWARD_HUNK).click();
    await waitFor(
      page,
      (marker) => (document.body?.textContent || '').includes(marker),
      'journey B ▼ never showed the line-17 marker',
      30000,
      EXPAND_MARKER,
    );
    const sentB = contextExpandFrames(await parseFrames(page, 'sent'));
    if (sentB.length <= sentBeforeB) {
      fail('no code_review_file_context expand frame after journey-B ▼');
    }
    const hunkExpandB = assertExpandWindow('journey B toward hunk', sentB.slice(0, sentBeforeB), sentB);
    if (Number(hunkExpandB.count) !== 20 || Number(hunkExpandB.newStart) !== 17) {
      fail(`journey-B ▼ window is not the 20-line 17..36 run: ${JSON.stringify(hunkExpandB)}`);
    }
    await assertContextWindow(page, 'journey B ▼', hunkExpandB, 17, 36);
    const remainingAfterB = remainingBeforeB - linesLoaded(sentB.slice(sentBeforeB));
    if (remainingAfterB !== 16) {
      fail(
        `journey-B ▼ must leave exactly 16 remaining lines (loaded ${linesLoaded(sentB.slice(sentBeforeB))} of ${remainingBeforeB})`,
      );
    }

    await leadingGapButton(page, REMAINING).click();
    await waitFor(
      page,
      () => {
        const gap = document.querySelector('.code-review__gap');
        return gap !== null && gap.querySelector('.code-review__gap-bar') === null;
      },
      'Expand remaining never cleared the leading gap bar',
      30000,
    );
    const sentAfterRemaining = contextExpandFrames(await parseFrames(page, 'sent'));
    const remainingExpand = assertExpandWindow('Expand remaining', sentB, sentAfterRemaining);
    if (
      Number(remainingExpand.count) !== 16
      || Number(remainingExpand.newStart) !== 1
      || Number(remainingExpand.oldStart) !== 1
    ) {
      fail(
        `Expand remaining window is not the typed 16-line 1..16 remainder: ${JSON.stringify(remainingExpand)}`,
      );
    }
    await assertContextWindow(page, 'Expand remaining', remainingExpand, 1, 16);
    const afterRemaining = await page.evaluate((marker) => {
      const gap = document.querySelector('.code-review__gap');
      const gapLines = [...(gap?.querySelectorAll('.code-review__line') || [])].map(
        (el) => el.textContent || '',
      );
      return {
        hunkHeader: document.querySelector('.code-review__hunk-header')?.textContent || '',
        gapBarPresent: !!gap?.querySelector('.code-review__gap-bar'),
        markerInGap: gapLines.some((text) => text.includes(marker)),
      };
    }, EXPAND_MARKER);
    if (afterRemaining.hunkHeader !== beforeB.hunkHeader) {
      fail(
        `hunk header changed after Expand remaining: before=${JSON.stringify(beforeB.hunkHeader)} after=${JSON.stringify(afterRemaining.hunkHeader)}`,
      );
    }
    if (afterRemaining.gapBarPresent) {
      fail('gap bar still present after Expand remaining');
    }
    if (!afterRemaining.markerInGap) {
      fail('line-17 marker left the gap band after Expand remaining');
    }
    const bodyAfterRemaining = await page.evaluate(() => document.body?.textContent || '');
    if (!bodyAfterRemaining.includes(HUNK_MARKER)) {
      fail('hunk edit marker disappeared after Expand remaining');
    }
    await page.screenshot({ path: `${evidence}/02c-desktop-gap-after-remaining.png`, fullPage: true });

    // ---- Mobile / dual-pane regression on the fully expanded diff ----
    await page.setViewportSize({ width: 390, height: 844 });
    await waitFor(
      page,
      () => document.querySelector('.code-review__tab-bar') !== null,
      'mobile tab bar missing at 390px',
    );
    await page.locator('.code-review__tab', { hasText: 'Diff' }).click();
    await waitFor(
      page,
      () => document.querySelector('.code-review__tab.is-active')?.textContent?.trim() === 'Diff',
      'mobile Diff tab did not activate',
    );
    await waitFor(
      page,
      (marker) => {
        const diff = document.querySelector('.code-review__diff');
        if (!diff) return false;
        const box = diff.getBoundingClientRect();
        return box.height > 0 && (diff.textContent || '').includes(marker);
      },
      'mobile Diff pane never showed the expanded line-17 marker',
      15000,
      EXPAND_MARKER,
    );
    const mobileOverflow = await page.evaluate(() => ({
      scrollWidth: document.documentElement.scrollWidth,
      clientWidth: document.documentElement.clientWidth,
    }));
    if (mobileOverflow.scrollWidth > mobileOverflow.clientWidth + 1) {
      fail(`mobile horizontal overflow: ${JSON.stringify(mobileOverflow)}`);
    }
    await page.screenshot({ path: `${evidence}/03-mobile-gap-after.png`, fullPage: true });

    if (pageErrors.length) fail(`page errors: ${JSON.stringify(pageErrors)}`);
    const clean = consoleErrors.filter((t) => !t.includes('favicon'));
    if (clean.length) fail(`console errors: ${JSON.stringify(clean)}`);

    fs.writeFileSync(
      `${evidence}/code-review-expand-report.json`,
      JSON.stringify(
        {
          hunkHeader: afterRemaining.hunkHeader,
          gapTotal,
          remainingBefore,
          remainingAfterHunk: remainingAfter,
          topWindow: {
            count: Number(topExpand.count),
            oldStart: Number(topExpand.oldStart),
            newStart: Number(topExpand.newStart),
          },
          remainingWindow: {
            count: Number(remainingExpand.count),
            oldStart: Number(remainingExpand.oldStart),
            newStart: Number(remainingExpand.newStart),
          },
          journeyAWireFrames: sentAfterTop.length,
          journeyBWireFrames: sentAfterRemaining.length,
          remainingLoaded: linesLoaded(sentAfterRemaining.slice(sentB.length)),
          pageErrors,
          consoleErrors: clean,
        },
        null,
        2,
      ),
    );
    console.log(
      'web-code-review-expand: PASSED (gap bar; ▼ toward hunk revealed line-17 marker; ▲ toward file top and Expand remaining each consumed the 16-line remainder in one count-16 frame and cleared the gap bar across two reset journeys; hunk header frozen; count 1..20; old==new line numbers; desktop+mobile; zero browser errors)',
    );
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-code-review-expand: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(3);
});
