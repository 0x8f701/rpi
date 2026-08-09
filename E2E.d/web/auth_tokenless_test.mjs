// WebSocket tokenless-listener web E2E lane (playwright half of
// E2E.d/web/auth_tokenless.sh).
//
// Environment:
//   RPI_URL        http://127.0.0.1:<port>/web  (listener started WITHOUT a
//                  token file — the tokenless policy: loopback accepts
//                  browser connections directly)
//   RPI_SLOW_TAIL  tail of the FIRST slow mock reply ("chunk-four-done")
//   RPI_CHROME     executable path of the system Chrome (optional)
//   RPI_EVIDENCE   evidence dir for screenshots
//
// Asserts the tokenless-listener contract:
//   1. EMPTY token: the boot auto-connect probe (no user interaction) reaches
//      `connected` and no error toast appears — the page never requires a
//      token on a tokenless listener.
//   2. A prompt round-trips over the tokenless connection.

import { chromium } from 'playwright';

const url = process.env.RPI_URL;
const slowTail = process.env.RPI_SLOW_TAIL || 'chunk-four-done';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

function fail(message) {
  // Exit 2 (not 1): 1 means "playwright setup failure (node/chromium/npm)",
  // which must NOT be confused with an assertion failure.
  console.error(`web-auth-tokenless: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage();
    page.on('pageerror', (err) => {
      console.error(`web-auth-tokenless: page error: ${err.message}`);
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(page, () => document.querySelector('#conn-state') !== null, 'conn-state missing');

    // 1. Empty token: the boot auto-connect must reach "connected" with no toast.
    //    The token field is left untouched (the boot probe sends no subprotocol).
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'tokenless boot auto-connect never reached "connected" with an empty token',
      20000
    );
    const anyErrorToast = await page.evaluate(() =>
      Array.from(document.querySelectorAll('#toasts .toast--error')).length > 0
    );
    if (anyErrorToast) fail('an error toast appeared during the tokenless boot auto-connect');
    await page.screenshot({ path: `${evidence}/auth-tokenless-connected.png`, fullPage: true });

    // 2. A prompt round-trips over the tokenless connection.
    await page.fill('#prompt-input', 'tokenless round-trip');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (tail) => document.body.textContent.includes(tail),
      'prompt never round-tripped over the tokenless connection',
      30000,
      slowTail
    );
    await page.screenshot({ path: `${evidence}/auth-tokenless-roundtrip.png`, fullPage: true });

    console.log('web-auth-tokenless: PASSED (empty-token boot connect + round-trip)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-auth-tokenless: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});