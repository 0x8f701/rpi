// WebSocket auth web E2E lane (playwright half of E2E.d/web/auth.sh).
//
// Environment:
//   RPI_URL        http://127.0.0.1:<port>/web
//   RPI_TOKEN      the GOOD token (file content, rpi-auth.<token> subprotocol)
//   RPI_WRONG_TOKEN a token the server must reject
//   RPI_SLOW_TAIL  tail of the FIRST slow mock reply ("chunk-four-done")
//   RPI_CHROME     executable path of the system Chrome (optional)
//   RPI_EVIDENCE   evidence dir for screenshots
//
// Asserts the rpi-auth subprotocol contract:
//   1. NO token: the boot auto-connect probe fails SILENTLY (no error toast,
//      by design — the empty-hint explains the requirement) and the pill
//      never reaches `connected`; it settles into `reconnecting`
//   2. WRONG token: Connect surfaces the "wrong or missing token" ERROR toast
//      and never reaches `connected`
//   3. GOOD token: Connect reaches `connected` and a prompt round-trips

import { chromium } from 'playwright';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const wrongToken = process.env.RPI_WRONG_TOKEN || 'definitely-wrong-token';
const slowTail = process.env.RPI_SLOW_TAIL || 'chunk-four-done';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

function fail(message) {
  // Exit 2 (not 1): 1 means "playwright setup failure (node/chromium/npm)",
  // which must NOT be confused with an assertion failure (the mock's request
  // counter is stateful and a rerun would see shifted replies).
  console.error(`web-auth: FAIL: ${message}`);
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
      console.error(`web-auth: page error: ${err.message}`);
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(page, () => document.querySelector('#conn-state') !== null, 'conn-state missing');

    // 1. No token: the boot probe must fail silently and never connect.
    let everOn = false;
    const stateWatch = setInterval(async () => {
      try {
        const state = await page.evaluate(() => document.getElementById('conn-state')?.dataset.state || '');
        if (state === 'on') everOn = true;
      } catch {
        /* page closed */
      }
    }, 50);
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'reconnecting',
      'pill never entered "reconnecting" after the silent no-token probe',
      15000
    );
    const noTokenToast = await page.evaluate(() =>
      Array.from(document.querySelectorAll('#toasts .toast')).some((t) => t.textContent.includes('connection failed'))
    );
    if (noTokenToast) fail('no-token boot probe should be silent, but an error toast appeared');
    if (everOn) fail('WS reached "connected" without a token');

    // 2. Wrong token: explicit Connect must surface the error toast and fail.
    await page.fill('#token-input', wrongToken);
    await page.click('#connect-btn');
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('#toasts .toast--error')).some((t) =>
          t.textContent.includes('wrong or missing token')
        ),
      'wrong-token error toast never appeared'
    );
    await page.screenshot({ path: `${evidence}/auth-wrong.png`, fullPage: true });
    if (everOn) fail('WS reached "connected" with a wrong token');

    // 3. Good token: Connect reaches "connected" and the session works.
    await page.fill('#token-input', token);
    await page.click('#connect-btn');
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'WS never reached "connected" with the good token'
    );
    clearInterval(stateWatch);
    await page.fill('#prompt-input', 'auth round-trip');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (tail) => document.body.textContent.includes(tail),
      'prompt with the good token never round-tripped',
      30000,
      slowTail
    );
    await page.screenshot({ path: `${evidence}/auth-good.png`, fullPage: true });

    console.log('web-auth: PASSED (no-token silent probe + wrong-token error toast + good-token connect/round-trip)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-auth: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
