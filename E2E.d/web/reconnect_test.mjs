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
// Asserts (regression guard for App.tsx scheduleReconnect/connect):
//   1. after a successful prompt round-trip the server is killed by the lane;
//      the pill must enter the `reconnecting` state on its own
//   2. once the lane respawns the listener on the same port, the client
//      reconnects AUTOMATICALLY (no user action) back to `on`
//   3. the pre-crash transcript survives the reconnect (text still rendered)
//   4. a prompt after the reconnect round-trips (request 2, instant reply)

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
    page.on('pageerror', (err) => {
      console.error(`web-reconnect: page error: ${err.message}`);
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(page, () => document.querySelector('#conn-state') !== null, 'conn-state missing');

    await page.fill('#token-input', token);
    await page.click('#connect-btn');
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'WS did not reach "connected"'
    );

    // Request 1 in the steering mock is the ~3.6s slow stream; wait for the tail.
    await page.fill('#prompt-input', 'hello, will you survive a restart?');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (tail) => document.body.textContent.includes(tail),
      'first reply never streamed into the DOM',
      30000,
      firstTail
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

    // 4. The composer still round-trips (request 2 = instant reply).
    await page.fill('#prompt-input', 'still here after the restart');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (reply) => document.body.textContent.includes(reply),
      'post-reconnect prompt did not round-trip',
      30000,
      fastReply
    );

    console.log('web-reconnect: PASSED (reconnecting pill + auto-reconnect + transcript survives + round-trip)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-reconnect: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
