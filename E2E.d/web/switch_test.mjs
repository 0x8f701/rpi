// Model + thinking-level switch web E2E lane (playwright half of
// E2E.d/web/switch.sh).
//
// Environment:
//   RPI_URL        http://127.0.0.1:<port>/web
//   RPI_TOKEN      token file content (served via rpi-auth.<token> subprotocol)
//   RPI_SLOW_TAIL  tail of the FIRST slow mock reply ("chunk-four-done")
//   RPI_CHROME     executable path of the system Chrome (optional)
//   RPI_EVIDENCE   evidence dir for screenshots
//
// Fixture: the mock model is `reasoning` and a SECOND model (mock-2) is
// registered, so both header selects have switchable options.
//
// Asserts (regression guard for App.tsx onModelChange/onThinkingChange):
//   1. #model-select lists both models and reflects the current model
//   2. switching the model (set_model) round-trips: get_state reflects the
//      new provider/id in the select
//   3. #thinking-select is enabled for the reasoning model and switchable
//      (set_thinking_level) — the select reflects the new level
//   4. the session still round-trips after the switches (request 1 = slow
//      stream completes)

import { chromium } from 'playwright';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const slowTail = process.env.RPI_SLOW_TAIL || 'chunk-four-done';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

function fail(message) {
  // Exit 2 (not 1): 1 means "playwright setup failure (node/chromium/npm)",
  // which must NOT be confused with an assertion failure (the mock's request
  // counter is stateful and a rerun would see shifted replies).
  console.error(`web-switch: FAIL: ${message}`);
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
      console.error(`web-switch: page error: ${err.message}`);
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(page, () => document.querySelector('#conn-state') !== null, 'conn-state missing');

    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'WS did not reach "connected"'
    );

    // 1. Both fixture models are listed (the offline catalog also carries a
    //    faux model, so assert presence, not an exact count).
    await waitFor(
      page,
      () => {
        const opts = Array.from(document.getElementById('model-select')?.options || []).map((o) => o.value);
        return opts.includes('user-steering/mock') && opts.includes('user-steering/mock-2');
      },
      'model select never listed both fixture models'
    );
    const initialModel = await page.evaluate(() => document.getElementById('model-select')?.value || '');
    if (initialModel !== 'user-steering/mock') {
      fail(`current model should be user-steering/mock, got "${initialModel}"`);
    }

    // 2. Model switch round-trips through set_model + get_state.
    await page.selectOption('#model-select', 'user-steering/mock-2');
    await waitFor(
      page,
      () => document.getElementById('model-select')?.value === 'user-steering/mock-2',
      'model switch never reflected in the select'
    );
    await page.screenshot({ path: `${evidence}/switch-model.png`, fullPage: true });

    // 3. The reasoning model exposes switchable thinking levels.
    await waitFor(
      page,
      () => {
        const sel = document.getElementById('thinking-select');
        return sel && !sel.disabled && sel.options.length > 1;
      },
      'thinking select never enabled with switchable levels for the reasoning model'
    );
    const levels = await page.evaluate(() =>
      Array.from(document.getElementById('thinking-select')?.options || []).map((o) => o.value)
    );
    if (!levels.includes('high')) fail(`thinking levels should include "high", got [${levels.join(', ')}]`);
    await page.selectOption('#thinking-select', 'high');
    await waitFor(
      page,
      () => document.getElementById('thinking-select')?.value === 'high',
      'thinking-level switch never reflected in the select'
    );
    await page.screenshot({ path: `${evidence}/switch-thinking.png`, fullPage: true });

    // 4. The session still round-trips after the switches.
    await page.fill('#prompt-input', 'hello after the switches');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (tail) => document.body.textContent.includes(tail),
      'post-switch prompt never round-tripped',
      30000,
      slowTail
    );

    console.log('web-switch: PASSED (model select lists + set_model round-trip; thinking level set_thinking_level round-trip; post-switch round-trip)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-switch: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
