// Web code-review gap-expand regression (playwright half of
// code_review_expand.sh).
//
// A 50-line dirty file with a change around line 40 must expose a gap bar
// above the hunk. Clicking ▼ ("Expand toward hunk") loads snapshot context
// so the unique line-17 marker appears, the hunk header/edit stay the same,
// every code_review_file_context expand count is 1..20, and the journey is
// captured at desktop (≥1280) and mobile (390×844). Zero page/console errors.
import { chromium } from 'playwright';
import fs from 'node:fs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const DIRTY_FILE = process.env.RPI_DIRTY_FILE || 'wide.rs';
const EXPAND_MARKER = process.env.RPI_EXPAND_MARKER || 'EXPAND-ABOVE-17 unique-gap-marker';
const HUNK_MARKER = process.env.RPI_HUNK_MARKER || 'wide-line-40-EDIT';
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

    await page.fill('#prompt-input', '/code-review');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') !== null,
      'code-review panel did not open',
    );
    await waitFor(
      page,
      () => {
        const label = document.querySelector('.code-review__label')?.textContent || '';
        return label.includes('HEAD') && label.includes('working tree');
      },
      'comparison label missing',
      30000,
    );
    await waitFor(
      page,
      (dirty) => !!document.querySelector(`button.code-review__file[data-file-path="${dirty}"]`),
      'dirty file row missing',
      30000,
      DIRTY_FILE,
    );
    await page.click(`button.code-review__file[data-file-path="${DIRTY_FILE}"]`);
    await waitFor(
      page,
      () => !!document.querySelector('button.code-review__hunk-header:not([disabled])'),
      'hunk never became selectable',
      30000,
    );
    await waitFor(
      page,
      () => !!document.querySelector('button[aria-label="Expand toward hunk"]'),
      'gap bar Expand toward hunk missing',
      30000,
    );

    const before = await page.evaluate(() => {
      const remaining = document.querySelector(
        'button[aria-label="Expand remaining lines in this gap"]',
      );
      return {
        hunkHeader: document.querySelector('.code-review__hunk-header')?.textContent || '',
        remainingLabel: remaining?.textContent || '',
        towardHunk: document.querySelector('button[aria-label="Expand toward hunk"]')?.textContent || '',
        towardTop: document.querySelector('button[aria-label="Expand toward file top"]')?.textContent || '',
      };
    });
    if (before.towardHunk.trim() !== '▼') {
      fail(`▼ must mean Expand toward hunk, got ${JSON.stringify(before.towardHunk)}`);
    }
    if (before.towardTop.trim() !== '▲') {
      fail(`▲ must mean Expand toward file top, got ${JSON.stringify(before.towardTop)}`);
    }
    if (!before.hunkHeader.includes('@@')) {
      fail(`hunk header missing @@: ${JSON.stringify(before.hunkHeader)}`);
    }
    const remainingBefore = Number.parseInt(before.remainingLabel, 10);
    if (!Number.isFinite(remainingBefore) || remainingBefore <= 0) {
      fail(`gap remaining count missing: ${JSON.stringify(before.remainingLabel)}`);
    }
    const bodyBefore = await page.evaluate(() => document.body?.textContent || '');
    if (bodyBefore.includes(EXPAND_MARKER)) {
      fail('line-17 marker was already visible before expand (gap was not collapsed)');
    }
    if (!bodyBefore.includes(HUNK_MARKER)) {
      fail(`hunk edit marker missing before expand: ${HUNK_MARKER}`);
    }
    await page.screenshot({ path: `${evidence}/01-desktop-gap-before.png`, fullPage: true });

    const sentBefore = contextExpandFrames(await parseFrames(page, 'sent')).length;
    await page.click('button[aria-label="Expand toward hunk"]');
    await waitFor(
      page,
      (marker) => (document.body?.textContent || '').includes(marker),
      'expanded context never showed the line-17 marker',
      30000,
      EXPAND_MARKER,
    );

    const after = await page.evaluate((marker) => {
      const remaining = document.querySelector(
        'button[aria-label="Expand remaining lines in this gap"]',
      );
      const gapLines = [...document.querySelectorAll('.code-review__gap .code-review__line')]
        .map((el) => el.textContent || '');
      return {
        hunkHeader: document.querySelector('.code-review__hunk-header')?.textContent || '',
        remainingLabel: remaining?.textContent || '',
        markerInGap: gapLines.some((text) => text.includes(marker)),
        hunkStillPresent: (document.body?.textContent || '').includes(
          document.querySelector('.code-review__hunk-header')?.textContent || '@@',
        ),
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
    const remainingAfter = Number.parseInt(after.remainingLabel, 10);
    if (Number.isFinite(remainingAfter) && remainingAfter >= remainingBefore) {
      fail(
        `remaining count did not shrink after ▼ (${remainingBefore} -> ${remainingAfter})`,
      );
    }

    const sent = contextExpandFrames(await parseFrames(page, 'sent'));
    if (sent.length <= sentBefore) {
      fail('no code_review_file_context expand frame after ▼');
    }
    const oversize = sent.filter((frame) => Number(frame.count) > 20);
    if (oversize.length) {
      fail(`expand count exceeded 20: ${JSON.stringify(oversize)}`);
    }
    const lastExpand = sent[sent.length - 1];
    if (Number(lastExpand.count) < 1 || Number(lastExpand.count) > 20) {
      fail(`last expand count not in 1..20: ${JSON.stringify(lastExpand)}`);
    }
    await page.screenshot({ path: `${evidence}/02-desktop-gap-after.png`, fullPage: true });

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
          hunkHeader: after.hunkHeader,
          remainingBefore,
          remainingAfter: Number.isFinite(remainingAfter) ? remainingAfter : after.remainingLabel,
          lastExpandCount: lastExpand.count,
          expandFrames: sent.length,
          pageErrors,
          consoleErrors: clean,
        },
        null,
        2,
      ),
    );
    console.log(
      'web-code-review-expand: PASSED (gap bar; ▼ toward hunk revealed line-17 marker; hunk header frozen; count<=20; desktop+mobile; zero browser errors)',
    );
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-code-review-expand: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(3);
});
