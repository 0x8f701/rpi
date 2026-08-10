// Goal panel E2E (playwright lane of E2E.d/web/goal.sh).
//
// Environment:
//   RPI_URL         http://127.0.0.1:<port>/web
//   RPI_TOKEN       token file content (served via rpi-auth.<token> subprotocol)
//   RPI_CHROME      executable path of the system Chrome (optional)
//   RPI_EVIDENCE    evidence dir for screenshots
//
// Asserts:
//   1. page loads and WS connects (subprotocol)
//   2. Goal panel opens; empty state shows before any goal exists
//   3. creating a goal through the panel form updates the panel (objective,
//      lifecycle, budget + usage) and appends a `created` journal entry
//   4. pinning through the panel form shows the pin and a `pins_updated` entry
//   5. a goal_pause issued by a SECOND WS client (raw `ws`, live event
//      stream) flips the panel to `paused` without any page interaction —
//      proving goal events refresh the panel live
//   6. Resume via the panel button returns to `active`; journal replays the
//      full history (created → pins_updated → paused → resumed)

import { chromium } from 'playwright';
import WebSocket from 'ws';
import fs from 'node:fs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

function fail(message) {
  // Exit 2 (not 1): 1 means "playwright setup failure (node/chromium/npm)",
  // which must NOT be confused with an assertion failure (the mock's request
  // counter is stateful and a rerun would see shifted replies — a fake pass).
  console.error(`web-goal: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

/** Raw RPC client over the SAME listener (second WS session). */
function rpcClient(wsUrl) {
  const ws = new WebSocket(wsUrl, token ? [`rpi-auth.${token}`] : []);
  const pending = new Map();
  let seq = 0;
  ws.on('message', (raw) => {
    let frame;
    try {
      frame = JSON.parse(String(raw));
    } catch {
      return;
    }
    if (frame && frame.type === 'response' && frame.id && pending.has(frame.id)) {
      const { resolve, reject } = pending.get(frame.id);
      pending.delete(frame.id);
      if (frame.success) resolve(frame.data || {});
      else reject(new Error(frame.error || 'rpc failed'));
    }
  });
  const ready = new Promise((resolve, reject) => {
    ws.on('open', resolve);
    ws.on('error', reject);
  });
  return {
    ready,
    async call(command) {
      await ready;
      const id = `e2e-${++seq}`;
      return new Promise((resolve, reject) => {
        pending.set(id, { resolve, reject });
        ws.send(JSON.stringify({ ...command, id }));
        setTimeout(() => {
          if (pending.delete(id)) reject(new Error(`rpc timed out: ${command.type}`));
        }, 15000);
      });
    },
    close() {
      try {
        ws.close();
      } catch {
        /* already closed */
      }
    },
  };
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const wsUrl = url.replace(/^http/, 'ws').replace(/\/web\/?$/, '/ws');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage();
    page.on('pageerror', (err) => {
      console.error(`web-goal: page error: ${err.message}`);
    });

    // 1. Page loads; WS connects via the rpi-auth.<token> subprotocol (Settings panel).
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    if (token) {
      await page.click('#settings-toggle-btn');
      await waitFor(page, () => document.querySelector('#settings-token-input') !== null, 'settings token input missing');
      await page.fill('#settings-token-input', token);
      await page.click('#settings-token-save-btn');
      await page.click('#settings-close-btn');
    }
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'WS did not reach "connected"'
    );

    // 2. Open the Goal panel; empty state must render before any goal exists.
    await page.click('#goal-panel-btn');
    await waitFor(page, () => document.getElementById('goal-panel') !== null, 'goal panel missing');
    await waitFor(
      page,
      () => document.getElementById('goal-panel').dataset.hasGoal === 'false',
      'goal panel should start with no goal'
    );

    // 3. Create a goal through the panel form.
    const objective = `ship the web goal e2e ${Date.now()}`;
    await page.fill('#goal-objective-input', objective);
    await page.fill('#goal-budget-input', '100');
    await page.click('#goal-create-btn');
    await waitFor(
      page,
      (obj) =>
        document.getElementById('goal-panel').dataset.hasGoal === 'true' &&
        document.getElementById('goal-panel').dataset.lifecycle === 'active' &&
        (document.getElementById('goal-objective') || {}).textContent === obj,
      'created goal never appeared in the panel',
      30000,
      objective
    );
    const budgetText = await page.textContent('#goal-budget');
    if (!budgetText.includes('100 token budget')) {
      fail(`budget line wrong: ${budgetText}`);
    }
    const usageText = await page.textContent('#goal-usage');
    if (!usageText.includes('0/100 tokens')) {
      fail(`usage line wrong: ${usageText}`);
    }
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('#goal-journal li')).some(
          (li) => li.dataset.kind === 'created'
        ),
      'journal never recorded the created event'
    );

    // 4. Pin through the panel form.
    const pinText = 'stay focused';
    await page.fill('#goal-pin-input', pinText);
    await page.click('#goal-pin-btn');
    await waitFor(
      page,
      (pin) =>
        Array.from(document.querySelectorAll('#goal-pins li .goal-pin__text')).some(
          (el) => el.textContent === pin
        ),
      'pinned text never appeared in the panel',
      30000,
      pinText
    );
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('#goal-journal li')).some(
          (li) => li.dataset.kind === 'pins_updated'
        ),
      'journal never recorded the pins_updated event'
    );
    await page.screenshot({ path: `${evidence}/goal-created-pinned.png`, fullPage: true });

    // 5. Live update: a SECOND WS client pauses the goal. The panel must
    //    reflect the paused state purely from the pushed goal_updated event.
    const rpc = rpcClient(wsUrl);
    const paused = await rpc.call({ type: 'goal_pause' });
    if (!paused || paused.lifecycle !== 'paused') {
      fail(`raw goal_pause returned ${JSON.stringify(paused)}`);
    }
    await waitFor(
      page,
      () => document.getElementById('goal-panel').dataset.lifecycle === 'paused',
      'panel never flipped to paused from the live event'
    );
    const statusText = await page.textContent('#goal-status');
    if (!statusText.includes('paused')) {
      fail(`status line wrong: ${statusText}`);
    }
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('#goal-journal li')).some(
          (li) => li.dataset.kind === 'paused'
        ),
      'journal never recorded the paused event'
    );
    await page.screenshot({ path: `${evidence}/goal-paused-live.png`, fullPage: true });

    // 6. Resume via the panel's own action button.
    await page.click('#goal-resume-btn');
    await waitFor(
      page,
      () => document.getElementById('goal-panel').dataset.lifecycle === 'active',
      'panel never returned to active after resume'
    );
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('#goal-journal li')).some(
          (li) => li.dataset.kind === 'resumed'
        ),
      'journal never recorded the resumed event'
    );

    // Journal replay order: created → pins_updated → paused → resumed.
    const kinds = await page.$$eval('#goal-journal li', (lis) => lis.map((li) => li.dataset.kind));
    const expected = ['created', 'pins_updated', 'paused', 'resumed'];
    for (const kind of expected) {
      if (!kinds.includes(kind)) {
        fail(`journal replay missing ${kind}; got ${JSON.stringify(kinds)}`);
      }
    }
    const firstCreated = kinds.indexOf('created');
    const firstPinned = kinds.indexOf('pins_updated');
    const firstPaused = kinds.indexOf('paused');
    const firstResumed = kinds.indexOf('resumed');
    if (!(firstCreated < firstPinned && firstPinned < firstPaused && firstPaused < firstResumed)) {
      fail(`journal replay out of order: ${JSON.stringify(kinds)}`);
    }
    await page.screenshot({ path: `${evidence}/goal-resumed-journal.png`, fullPage: true });

    rpc.close();
    console.log('web-goal: playwright PASSED (empty state, create, pin, live pause event, resume, journal replay)');
  } finally {
    await browser.close();
  }
}

main().catch((err) => {
  console.error(`web-goal: FATAL: ${err.stack || err.message}`);
  process.exit(2);
});
