// WebSocket auth web E2E lane (playwright half of E2E.d/web/auth.sh).
//
// Environment:
//   RPI_URL        http://127.0.0.1:<port>/web
//   RPI_TOKEN      the GOOD token (file content, rpi-auth.<token> subprotocol)
//   RPI_WRONG_TOKEN a token the server must reject
//   RPI_SLOW_TAIL  tail of the FIRST slow mock reply ("chunk-four-done")
//   RPI_CHROME     executable path of the system Chrome (optional)
//   RPI_EVIDENCE   evidence dir for screenshots
// Asserts the rpi-auth subprotocol contract against a TOKENED listener
// (auth_tokenless_test.mjs covers the tokenless listener):
//   1. NO token: the bounded /status preflight proves the listener is
//      token-gated and the boot HOLDS in the truthful `token_required` state —
//      the empty-hint carries actionable copy, the pairing card
//      auto-opens with the token input visible, and ZERO WebSocket creations
//      happen during the hold (no silent 401 retry churn)
//   2. WRONG token: saving it releases the hold with EXACTLY ONE connect
//      attempt; the refused handshake surfaces the "wrong or missing token"
//      ERROR toast and the socket never opens
//   3. GOOD token: exactly ONE post-save socket generation opens (with the
//      rpi-auth subprotocol), reaches the connected state (empty-hint flips
//      to "Send a prompt"), and a prompt round-trips
// The token value NEVER crosses into a log line: the in-page observer only
// reports counts and booleans to Node.

import { chromium } from 'playwright';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const wrongToken = process.env.RPI_WRONG_TOKEN || 'definitely-wrong-token';
const slowTail = process.env.RPI_SLOW_TAIL || 'chunk-four-done';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

// Longer than RECONNECT_INITIAL_DELAY (1000ms): if the hold still armed the
// silent retry loop, a second attempt would land inside this window.
const HOLD_SAMPLE_MS = 1600;
const NO_RETRY_COPY = 'No reconnect is attempted until the token is saved.';
const OLD_CONNECTING_COPY = 'Connecting to the control plane';

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

/** WebSocket creation observer, installed before any page script runs so the
 *  boot hold's no-socket contract is observable from the very first attempt.
 *  Counts constructor attempts and successful opens (socket generations) plus
 *  how many opens used the rpi-auth.<token> subprotocol; only the counters
 *  cross back to Node (the token itself is never printed). */
async function installWsObserver(page, authToken) {
  await page.addInitScript((good) => {
    const NativeWebSocket = window.WebSocket;
    const state = { attempts: 0, opens: 0, authOpens: 0 };
    class ObservedWebSocket extends NativeWebSocket {
      constructor(...args) {
        super(...args);
        state.attempts += 1;
        this.addEventListener('open', () => {
          state.opens += 1;
          if ((this.protocol || '') === `rpi-auth.${good}`) state.authOpens += 1;
        });
      }
    }
    for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
      Object.defineProperty(ObservedWebSocket, key, { value: NativeWebSocket[key] });
    }
    window.WebSocket = ObservedWebSocket;
    window.__rpiAuthWs = state;
  }, authToken);
}

const wsCounters = (page) => page.evaluate(() => ({ ...window.__rpiAuthWs }));

/** Settings remains the token-editing surface after initial pairing. */
async function ensureSettingsOpen(page, label) {
  const open = await page.evaluate(() => document.querySelector('#settings-token-input') !== null);
  if (!open) {
    await page.click('#settings-toggle-btn');
    await waitFor(
      page,
      () => document.querySelector('#settings-token-input') !== null,
      `${label}: settings token input missing after toggle`
    );
  }
}

async function closeSettingsIfOpen(page) {
  const open = await page.evaluate(() => document.querySelector('#settings-token-input') !== null);
  if (open) await page.click('#settings-close-btn');
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
    await installWsObserver(page, token);

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');

    // 1. NO token: the /status preflight holds the boot in `token_required`.
    //    The empty-hint carries the exact actionable copy (the old silent
    //    "Connecting…" state is gone) and the pairing card auto-opens.
    await waitFor(
      page,
      () => (document.querySelector('.empty-hint')?.textContent || '').includes('This listener requires an access token'),
      'token-required boot hold never surfaced the actionable empty hint',
      15000
    );
    const holdHint = await page.evaluate(() => document.querySelector('.empty-hint')?.textContent || '');
    if (!holdHint.includes(NO_RETRY_COPY)) {
      fail('token-required empty hint lost the no-retry explanation copy');
    }
    if (holdHint.includes(OLD_CONNECTING_COPY)) {
      fail('token_required state still shows the old silent "Connecting…" copy');
    }
    await waitFor(
      page,
      () => document.querySelector('#token-pairing-input') !== null,
      'token_required hold did not open the pairing card'
    );
    await page.screenshot({ path: `${evidence}/auth-hold.png`, fullPage: true });

    // The hold owns recovery: ZERO sockets during the hold, and the state is
    // persistent — within the first reconnect-backoff window the observer
    // must record no new attempts (no silent 401 retry churn) and no opens.
    const hold = await wsCounters(page);
    if (hold.attempts !== 0 || hold.opens !== 0) {
      fail(`token_required hold opened a WebSocket before any token was saved (attempts=${hold.attempts} opens=${hold.opens})`);
    }
    const noTokenToast = await page.evaluate(() =>
      Array.from(document.querySelectorAll('#toasts .toast')).some((t) => t.textContent.includes('connection failed'))
    );
    if (noTokenToast) fail('token_required hold surfaced a transport error toast; the hold must stay silent until a token is saved');
    await page.waitForTimeout(HOLD_SAMPLE_MS);
    const heldStill = await wsCounters(page);
    if (heldStill.attempts !== hold.attempts || heldStill.opens !== 0) {
      fail(`token_required hold showed retry churn (attempts ${hold.attempts} -> ${heldStill.attempts}, opens=${heldStill.opens})`);
    }

    // 2. WRONG token: saving it releases the hold with EXACTLY ONE connect
    //    attempt; the refused handshake surfaces the error toast and the
    //    socket never opens.
    await page.fill('#token-pairing-input', wrongToken);
    const beforeWrong = await wsCounters(page);
    await page.click('.token-pairing__connect');
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('#toasts .toast--error')).some((t) =>
          t.textContent.includes('wrong or missing token')
        ),
      'wrong-token error toast never appeared'
    );
    const afterWrong = await wsCounters(page);
    if (afterWrong.attempts !== beforeWrong.attempts + 1) {
      fail(`wrong-token save must release the hold with exactly one connect attempt (attempts ${beforeWrong.attempts} -> ${afterWrong.attempts})`);
    }
    if (afterWrong.opens !== beforeWrong.opens) {
      fail('wrong-token handshake reached OPEN; the listener must refuse it');
    }
    await page.screenshot({ path: `${evidence}/auth-wrong.png`, fullPage: true });
    await closeSettingsIfOpen(page);

    // 3. GOOD token: the save supersedes any wrong-token retry timer and opens
    //    EXACTLY ONE socket generation (with the rpi-auth subprotocol), which
    //    reaches "connected" and round-trips a prompt.
    await ensureSettingsOpen(page, 'good-token');
    await page.fill('#settings-token-input', token);
    const beforeGood = await wsCounters(page);
    await page.click('#settings-token-save-btn');
    await waitFor(
      page,
      () => (document.querySelector('.empty-hint')?.textContent || '').includes('Send a prompt to start the session.'),
      'WS never reached "connected" with the good token'
    );
    const afterGood = await wsCounters(page);
    if (afterGood.opens !== beforeGood.opens + 1) {
      fail(`good-token save must open exactly one socket generation (opens ${beforeGood.opens} -> ${afterGood.opens})`);
    }
    if (afterGood.authOpens !== beforeGood.authOpens + 1) {
      fail('the post-save connection did not authenticate with the rpi-auth.<token> subprotocol');
    }
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

    console.log('web-auth: PASSED (token_required hold with zero sockets + wrong-token refusal + good-token single-connect round-trip)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-auth: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});