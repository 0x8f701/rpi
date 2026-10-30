// Failed-turn transcript lane — real Chromium assertions that a turn which
// fails at the model transport layer (provider HTTP 500 before any delta)
// surfaces its persisted errorMessage in the transcript, live and after a
// reload refill, instead of rendering an empty hole. Mirrors the reported
// incident: repeated prompts whose turns settled stopReason:"error" showed
// NOTHING (no row, no toast) — the user experienced silence.
//
// Drives the REAL `rpi --listen` fixture with the loopback steering mock;
// the failure seed is the EXACT prompt marker 'web turn error'
// (E2E.d/lib/user_mock_server.py, additive route).
//
// Environment (same contract as presentation_test.mjs):
//   RPI_URL     http://127.0.0.1:<port>/web
//   RPI_TOKEN   token file content (rpi-auth.<token> subprotocol)
//   RPI_CHROME  system Chrome executable (optional)
//   RPI_EVIDENCE evidence dir (turn-error-assertions.json + screenshot)
//
// Exit: 0 = every assertion held + evidence written; 2 = assertion failure;
//       1 = setup failure (playwright unusable).

import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

const pageErrors = [];
const consoleErrors = [];
const executed = new Map();

function record(id) {
  executed.set(id, (executed.get(id) || 0) + 1);
  console.log(`[web-turn-error:assert] ${id}`);
}
function fail(message) {
  console.error(`web-turn-error: FAIL: ${message}`);
  process.exit(2);
}
async function waitFor(page, fn, label, timeoutMs = 30000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

function observeBrowserErrors(page) {
  page.on('pageerror', (err) => pageErrors.push(err.message));
  page.on('console', (msg) => {
    if (msg.type() === 'error') consoleErrors.push(msg.text());
  });
}

async function connectPage(page) {
  if (token) {
    await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
  }
  await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
  await waitFor(page, () => document.title === 'rpi web', 'page title missing');
  await waitFor(page, () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true, 'WS did not reach "connected"');
}

async function sendPrompt(page, text) {
  await page.fill('#prompt-input', text);
  await page.press('#prompt-input', 'Enter');
}

async function assertErrorRow(page, id, label) {
  await waitFor(
    page,
    () => [...document.querySelectorAll('.msg--assistant__error')]
      .some((row) => (row.textContent || '').includes('mock upstream failed')),
    `${label}: failed turn must render .msg--assistant__error with the persisted reason`,
  );
  record(id);
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    await fs.promises.mkdir(evidence, { recursive: true });
    // ============ live: failure paints the row, toast fires, composer idles ====
    const page = await browser.newPage();
    observeBrowserErrors(page);
    await connectPage(page);
    await sendPrompt(page, 'web turn error');
    await assertErrorRow(page, 'turn-error.live-row', 'live transcript');
    await waitFor(page, () => document.querySelector('.toast--error') !== null, 'failed turn raised no error toast');
    record('turn-error.failed-toast');
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'composer did not return to idle after the failed turn',
    );
    record('turn-error.composer-idle');
    await page.screenshot({ path: path.join(evidence, 'turn-error-desktop.png'), fullPage: false });

    // ======== restored: reload refills from the persisted session =========
    await page.reload({ waitUntil: 'domcontentloaded' });
    await waitFor(page, () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true, 'WS did not reconnect after reload');
    await assertErrorRow(page, 'turn-error.restored-row', 'restored transcript');

    // ============ mobile 390x844: the row must not overflow ==============
    const mobile = await browser.newPage({ viewport: { width: 390, height: 844 } });
    observeBrowserErrors(mobile);
    await connectPage(mobile);
    await assertErrorRow(mobile, 'turn-error.mobile-restored-row', 'mobile restored transcript');
    const overflow = await mobile.evaluate(() => {
      const row = [...document.querySelectorAll('.msg--assistant__error')].at(-1);
      if (!row) return null;
      const rect = row.getBoundingClientRect();
      return { right: rect.right, innerWidth: window.innerWidth };
    });
    if (overflow === null) fail('mobile: error row missing');
    if (overflow.right > overflow.innerWidth + 1) fail(`mobile error row overflows the viewport: ${JSON.stringify(overflow)}`);
    record('turn-error.mobile-no-overflow');
    await mobile.close();

    if (pageErrors.length > 0 || consoleErrors.length > 0) {
      fail(`browser errors observed: ${JSON.stringify({ pageErrors, consoleErrors })}`);
    }
    fs.writeFileSync(
      path.join(evidence, 'turn-error-assertions.json'),
      JSON.stringify({ executed: [...executed.keys()], counts: Object.fromEntries(executed), pageErrors, consoleErrors }, null, 2),
    );
    console.log(`web-turn-error: PASS — ${executed.size} assertions`);
  } finally {
    await browser.close();
  }
}

main().catch((err) => {
  console.error(`web-turn-error: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
