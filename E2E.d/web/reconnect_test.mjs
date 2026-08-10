// Auto-reconnect web E2E lane (playwright half of E2E.d/web/reconnect.sh).
//
// Environment:
//   RPI_URL         http://127.0.0.1:<port>/web
//   RPI_TOKEN       token file content (served via rpi-auth.<token> subprotocol)
//   RPI_FIRST_TAIL  tail of the FIRST slow mock reply ("chunk-four-done")
//   RPI_FAST_REPLY  instant mock reply text ("steering-followup-reply")
//   RPI_WORK        shared dir for the kill/restart handshake with the lane:
//                     test writes kill-server.marker, lane respawns the
//                     listener on the SAME port and writes server-up.marker
//   RPI_CHROME      executable path of the system Chrome (optional)
//   RPI_EVIDENCE    evidence dir for screenshots
//
// Asserts (regression guard for App.tsx reconnect bootstrap):
//   1. a same-process WebSocket outage during a slow turn lets the backend
//      finish while the browser is disconnected; automatic reconnect restores
//      the full authoritative reply with no stale streaming item
//   2. the existing real server restart still enters `reconnecting`, returns
//      automatically to `on`, preserves the transcript, and round-trips again

import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const firstTail = process.env.RPI_FIRST_TAIL || 'chunk-four-done';
const fastReply = process.env.RPI_FAST_REPLY || 'steering-followup-reply';
const work = process.env.RPI_WORK || '.';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

const KILL_MARKER = path.join(work, 'kill-server.marker');
const UP_MARKER = path.join(work, 'server-up.marker');

const slowHead = 'steer-1-';
const thirdTail = 'steer-3-ing stream chunk-four-done';
const firstReply = `steer-1-ing stream ${firstTail}`;

function fail(message) {
  // Exit 2 (not 1): 1 means "playwright setup failure (node/chromium/npm)",
  // which must NOT be confused with an assertion failure (the mock's request
  // counter is stateful and a rerun would see shifted replies).
  console.error(`web-reconnect: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

function waitForFile(file, label, timeoutMs = 60000) {
  // Promise-based polling (setTimeout), NOT Atomics.wait: a blocking sleep
  // would freeze the event loop and starve the conn-state pill watcher.
  return new Promise((resolve, reject) => {
    const deadline = Date.now() + timeoutMs;
    const check = () => {
      if (fs.existsSync(file)) return resolve();
      if (Date.now() > deadline) return reject(new Error(`${label} (timeout ${timeoutMs}ms)`));
      setTimeout(check, 100);
    };
    check();
  });
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
    // Install a transparent WebSocket shim before the app loads. The test can
    // close the live socket and temporarily reject replacements while the rpi
    // process stays alive and finishes the slow provider turn.
    await page.addInitScript(() => {
      const NativeWebSocket = window.WebSocket;
      let current = null;
      let blocked = false;
      class TestWebSocket extends NativeWebSocket {
        constructor(...args) {
          if (blocked) throw new Error('web-reconnect test outage');
          super(...args);
          current = this;
        }
      }
      for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
        Object.defineProperty(TestWebSocket, key, { value: NativeWebSocket[key] });
      }
      window.WebSocket = TestWebSocket;
      window.__rpiReconnectTest = {
        disconnect() {
          blocked = true;
          current?.close(4000, 'test outage');
        },
        reconnect() {
          blocked = false;
        },
      };
    });
    page.on('pageerror', (err) => {
      console.error(`web-reconnect: page error: ${err.message}`);
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(page, () => document.querySelector('#conn-state') !== null, 'conn-state missing');

    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'WS did not reach "connected"'
    );

    // Request 1 is the slow stream. Drop only the browser socket after its
    // first delta, keep replacements blocked long enough for the backend to
    // record the complete reply, then allow App.tsx to reconnect.
    await page.fill('#prompt-input', 'finish this turn while the browser socket is down');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (head) => document.body.textContent.includes(head),
      'slow turn never started streaming',
      30000,
      slowHead
    );
    await page.evaluate(() => window.__rpiReconnectTest.disconnect());
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'reconnecting',
      'same-process outage never entered reconnecting'
    );
    await page.waitForTimeout(1200);
    await page.evaluate(() => window.__rpiReconnectTest.reconnect());
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'same-process outage never auto-reconnected',
      30000
    );
    await waitFor(
      page,
      (reply) => document.body.textContent.includes(reply),
      'full authoritative completed reply was not restored after reconnect',
      30000,
      firstReply
    );
    const staleStreamingItem = await page.locator('#transcript .assistant-toolcall').count() > 0;
    if (staleStreamingItem) fail('stale streaming assistant remained after authoritative restore');
    await page.screenshot({ path: `${evidence}/reconnect-inflight-restored.png`, fullPage: true });

    // Request 2 is instant; retain the existing server-restart assertions
    // after the same-process outage phase.
    await page.fill('#prompt-input', 'complete before the server restart');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (reply) => document.body.textContent.includes(reply),
      'pre-restart reply did not round-trip',
      30000,
      fastReply
    );
    // Watch the pill during the outage; the lane reacts to our marker by
    // killing and respawning the listener on the same port.
    let sawReconnecting = false;
    const observedStates = [];
    const pillWatch = setInterval(async () => {
      try {
        const state = await page.evaluate(() => document.getElementById('conn-state')?.dataset.state || '');
        observedStates.push(state);
        if (state === 'reconnecting') sawReconnecting = true;
      } catch {
        /* page mid-navigation / closed */
      }
    }, 100);

    fs.writeFileSync(KILL_MARKER, 'kill now\n');
    await waitForFile(UP_MARKER, 'lane never respawned the listener');
    clearInterval(pillWatch);

    // 1. The pill showed `reconnecting` on its own after the server died.
    if (!sawReconnecting) fail(`conn-state never entered "reconnecting" after the server was killed (observed: ${observedStates.slice(-25).join(',')})`);
    await page.screenshot({ path: `${evidence}/reconnect-pill.png`, fullPage: true });

    // 2. Auto-reconnect: no user action, back to `on`.
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'client never auto-reconnected after the listener came back'
    );
    await page.screenshot({ path: `${evidence}/reconnect-on.png`, fullPage: true });

    // 3. The pre-crash transcript survived the reconnect.
    const survives = await page.evaluate((tail) => document.body.textContent.includes(tail), firstTail);
    if (!survives) fail('pre-crash assistant reply vanished after the reconnect');

    // 4. The composer still round-trips (request 3 = slow reply).
    await page.fill('#prompt-input', 'still here after the restart');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (reply) => document.body.textContent.includes(reply),
      'post-reconnect prompt did not round-trip',
      30000,
      thirdTail
    );

    console.log('web-reconnect: PASSED (in-flight authoritative restore + no stale stream + server restart + round-trip)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-reconnect: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
