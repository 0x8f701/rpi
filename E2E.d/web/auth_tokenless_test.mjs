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
//   1. EMPTY token boot connects without an error toast.
//   2. Obsolete persisted active-host state cannot override the serving page
//      authority after reload, including the actual WebSocket endpoint.
//   3. A prompt round-trips over the tokenless connection.

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
    const pageErrors = [];
    const consoleWebSocketErrors = [];
    let webSocketUrls = [];
    page.on('pageerror', (err) => pageErrors.push(err.message));
    page.on('console', (message) => {
      if (message.type() === 'error' && /WebSocket/i.test(message.text())) {
        consoleWebSocketErrors.push(message.text());
      }
    });
    page.on('websocket', (socket) => webSocketUrls.push(socket.url()));

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');

    // 1. Empty token: the boot auto-connect must reach "connected" with no toast.
    //    The token field is left untouched (the boot probe sends no subprotocol).
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'tokenless boot auto-connect never reached "connected" with an empty token',
      20000
    );
    const anyErrorToast = await page.evaluate(() =>
      Array.from(document.querySelectorAll('#toasts .toast--error')).length > 0
    );
    if (anyErrorToast) fail('an error toast appeared during the tokenless boot auto-connect');
    await page.screenshot({ path: `${evidence}/auth-tokenless-connected.png`, fullPage: true });

    // 2. A stale host saved by an older client must not override this page's
    //    serving authority after reload.
    const pageAuthority = new URL(url).host;
    await page.evaluate(() => window.localStorage.setItem('rpi-web-active-host', '192.0.2.87:8765'));
    webSocketUrls = [];
    await page.reload({ waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'reload with stale active host never reconnected to the page authority',
      20000
    );
    const pageUrl = new URL(url);
    const expectedWebSocketUrl = `${pageUrl.protocol === 'https:' ? 'wss:' : 'ws:'}//${pageAuthority}/ws`;
    const controlWebSockets = webSocketUrls.filter((value) => new URL(value).pathname === '/ws');
    if (controlWebSockets.length === 0) fail('reload opened no control-plane WebSocket');
    if (controlWebSockets.some((value) => value !== expectedWebSocketUrl)) {
      fail(`reload opened a WebSocket outside the page authority: ${JSON.stringify(controlWebSockets)}`);
    }
    if (pageErrors.length > 0) fail(`page errors during stale-host reload: ${JSON.stringify(pageErrors)}`);
    if (consoleWebSocketErrors.length > 0) {
      fail(`console WebSocket errors during stale-host reload: ${JSON.stringify(consoleWebSocketErrors)}`);
    }
    await page.screenshot({ path: `${evidence}/auth-tokenless-stale-host-reload.png`, fullPage: true });

    // 3. A prompt round-trips over the tokenless connection after the reload.
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

    console.log('web-auth-tokenless: PASSED (empty-token boot + stale-host reload + round-trip)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-auth-tokenless: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});