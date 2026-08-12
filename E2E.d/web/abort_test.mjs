// Abort semantics web E2E lane (playwright half of E2E.d/web/abort.sh).
//
// Environment:
//   RPI_URL          http://127.0.0.1:<port>/web
//   RPI_TOKEN        token file content (served via rpi-auth.<token> subprotocol)
//   RPI_SLOW_PREFIX  first chunk of the slow mock stream ("steer-1-")
//   RPI_ABORTED_TAIL final chunk that must NEVER render after abort ("-done")
//   RPI_FAST_REPLY   instant mock reply text ("steering-followup-reply")
//   RPI_CHROME       executable path of the system Chrome (optional)
//   RPI_EVIDENCE     evidence dir for screenshots
//
// Regression guards (B1/B2 web semantics):
//   1. B1 — an EARLY abort (immediately after the first delta lands) must
//      PRESERVE the streamed text in the transcript (the partial assistant
//      message stays) while the final chunks never render.
//   2. B2 — aborting surfaces a NEUTRAL toast ("run aborted"), never an
//      error toast (the abort is user-initiated, not a failure).
//   3. the composer recovers: the next prompt round-trips normally.

import { chromium } from 'playwright';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const slowPrefix = process.env.RPI_SLOW_PREFIX || 'steer-1-';
const abortedTail = process.env.RPI_ABORTED_TAIL || '-done';
const fastReply = process.env.RPI_FAST_REPLY || 'steering-followup-reply';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

function fail(message) {
  // Exit 2 (not 1): 1 means "playwright setup failure (node/chromium/npm)",
  // which must NOT be confused with an assertion failure (the mock's request
  // counter is stateful and a rerun would see shifted replies).
  console.error(`web-abort: FAIL: ${message}`);
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
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    page.on('pageerror', (err) => {
      console.error(`web-abort: page error: ${err.message}`);
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(page, () => document.querySelector('#conn-state') !== null, 'conn-state missing');

    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'WS did not reach "connected"'
    );

    // Request 1 in the steering mock is the ~3.6s slow stream.
    await page.fill('#prompt-input', 'write a very long answer');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (prefix) => document.body.textContent.includes(prefix),
      'slow stream never started (first delta missing)',
      30000,
      slowPrefix
    );

    // EARLY abort: the unified composer action changes to Stop as soon as the
    // first delta is visible. The next chunks arrive 0.6s apart.
    await waitFor(page, () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Stop generating', 'unified action never switched to Stop');
    await page.click('#send-btn');
    await waitFor(
      page,
      () => document.getElementById('stream-badge').hidden === true,
      'streaming badge did not clear after abort'
    );

    // B1: the streamed text survived the abort; the final chunks did not.
    const preserved = await page.evaluate(() => {
      const nodes = document.querySelectorAll('.msg--assistant .assistant-text');
      return nodes.length ? nodes[nodes.length - 1].textContent : '';
    });
    if (!preserved.includes(slowPrefix)) {
      fail(`B1: aborted message lost the streamed text: "${preserved}"`);
    }
    if (preserved.includes(abortedTail)) {
      fail(`B1: aborted message rendered chunks past the abort: "${preserved}"`);
    }
    await page.screenshot({ path: `${evidence}/abort-early.png`, fullPage: true });

    // B2: the abort produced a NEUTRAL toast — "run aborted", no error class.
    await waitFor(
      page,
      () => Array.from(document.querySelectorAll('#toasts .toast')).some((t) => t.textContent.includes('run aborted')),
      'neutral "run aborted" toast never appeared'
    );
    const toastState = await page.evaluate(() => {
      const toasts = Array.from(document.querySelectorAll('#toasts .toast'));
      return {
        neutral: toasts.some((t) => t.textContent.includes('run aborted')),
        neutralIsError: toasts.some((t) => t.textContent.includes('run aborted') && t.classList.contains('toast--error')),
        anyError: toasts.some((t) => t.classList.contains('toast--error')),
      };
    });
    if (!toastState.neutral) fail('B2: "run aborted" toast missing');
    if (toastState.neutralIsError) fail('B2: abort toast rendered as an ERROR toast');
    if (toastState.anyError) fail('B2: an unexpected error toast appeared alongside the abort');
    await page.screenshot({ path: `${evidence}/abort-toast.png`, fullPage: true });

    // Recovery: request 2 in the mock is instant again.
    await page.fill('#prompt-input', 'recovery prompt');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (reply) => document.body.textContent.includes(reply),
      'post-abort prompt did not round-trip',
      30000,
      fastReply
    );

    console.log('web-abort: PASSED (early-abort preserves text + neutral toast + recovery)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-abort: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
