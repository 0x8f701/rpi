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
//   2. switching during a slow response changes only the next-turn model:
//      the active turn keeps its captured old model and completes normally,
//      while the header select shows the newly chosen model
//   3. the next prompt runs after the model cutover
//   4. #thinking-select remains enabled and switchable on the new model

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
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
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

    // 2. Start request 1 on the original model, then switch while the response
    // is visibly streaming. The backend captures the turn model at admission;
    // App.tsx must project mock-2 as a next-turn setting without retargeting
    // the in-flight response.
    await page.fill('#prompt-input', 'switch models while this response streams');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.body.textContent.includes('steer-1-'),
      'slow response never rendered its first streamed chunk',
      30000
    );
    await page.selectOption('#model-select', 'user-steering/mock-2');
    await waitFor(
      page,
      () => document.getElementById('model-select')?.value === 'user-steering/mock-2',
      'mid-stream model switch never updated the header select to the chosen model'
    );
    await page.screenshot({ path: `${evidence}/switch-model-mid-stream.png`, fullPage: true });
    await waitFor(
      page,
      (tail) => document.body.textContent.includes(tail)
        && document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message',
      'old-model in-flight response did not complete after the switch',
      30000,
      slowTail
    );
    const selectedAfterSettle = await page.evaluate(() => document.getElementById('model-select')?.value);
    if (selectedAfterSettle !== 'user-steering/mock-2') fail(`header select left the next-turn model after settle: ${selectedAfterSettle}`);

    // 3. Request 2 is admitted after the model cutover and must still
    // round-trip on the now-current mock-2 runtime.
    await page.fill('#prompt-input', 'hello on the next-turn model');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.body.textContent.includes('steering-followup-reply'),
      'next-turn prompt never round-tripped on the selected model',
      30000
    );

    // 4. The reasoning model exposes switchable thinking levels after the
    // mid-stream model cutover.
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

    console.log('web-switch: PASSED (mid-stream model cutover keeps current turn on old model, next turn uses selected model, thinking level round-trips)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-switch: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
