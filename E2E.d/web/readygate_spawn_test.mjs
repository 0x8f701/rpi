// Web WS delayed-open/reconnect sidebar+panel regression lane (playwright
// half of E2E.d/web/readygate_spawn.sh).
//
// Environment:
//   RPI_URL         http://127.0.0.1:<port>/web
//   RPI_TOKEN       token file content (served via rpi-auth.<token> subprotocol)
//   RPI_WORK        shared dir for the kill/restart handshake with the lane:
//                     test writes kill-server.marker, the lane respawns the
//                     listener on the SAME port and writes server-up.marker
//   RPI_CHROME      executable path of the system Chrome (optional)
//   RPI_EVIDENCE    evidence dir for screenshots
//
// Asserts (ReadyGate regression, real listener kill + respawn):
//   R1  boot: WS reaches `on`; the sidebar lists >=1 row with a non-empty
//       data-session-id (sessions scenario primary row)
//   R2  after the server kill, the client enters `reconnecting`; for the
//       whole outage window (>=12s, crossing the sidebar's 8s poll cadence)
//       the sentinel `load failed: not connected` never appears in
//       document.body text, .session-sidebar__error, or .panel__status
//   R3  after the respawn on the same port, WS returns to `on` and the
//       sidebar eventually repopulates rows WITHOUT user action (gated poll
//       resolves via notifyOpen)
//   R4  the session panel (opened after reconnect) loads its current-session
//       block; the sentinel is still absent after the panel reload
//
// The sentinel is built at runtime as `load failed: ${safeText(err.message)}`,
// so scanning for the `load failed:` prefix is the stable assertion. A
// transient `not connected` (fail-fast active action) is expected and is NOT
// the sentinel.

import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const work = process.env.RPI_WORK || '.';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

const KILL_MARKER = path.join(work, 'kill-server.marker');
const UP_MARKER = path.join(work, 'server-up.marker');
const SENTINEL = 'load failed:';

function fail(message) {
  console.error(`web-readygate-spawn: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 30000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await page.evaluate(fn)) return;
    await page.waitForTimeout(120);
  }
  fail(`${label} (timeout ${timeoutMs}ms)`);
}

function waitForFile(file, label, timeoutMs = 90000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (fs.existsSync(file)) return;
    const ms = Math.min(200, deadline - Date.now());
    if (ms <= 0) break;
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
  }
  fail(`${label}: marker ${file} never appeared`);
}

/** Sentinel scan: the `load failed:` prefix in the body text, the sidebar
 *  error slot, and the session panel status slot. */
const scanSentinel = () => ({
  body: (document.body ? document.body.textContent : '').includes('load failed:'),
  sidebar: (document.querySelector('.session-sidebar__error')?.textContent || '').includes('load failed:'),
  panel: (document.querySelector('.panel__status')?.textContent || '').includes('load failed:'),
});

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
      console.error(`web-readygate-spawn: page error: ${err.message}`);
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'R1: page title missing');
    await waitFor(page, () => document.getElementById('conn-state') !== null, 'R1: conn-state missing');
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'R1: WS did not reach "connected"'
    );

    // R1: the sidebar eventually lists the sessions-scenario primary row.
    await waitFor(
      page,
      () => document.querySelectorAll('.session-sidebar__switch[data-session-id]:not([data-session-id=""])').length >= 1,
      'R1: sidebar never listed a primary session row after boot',
      30000
    );
    const bootRows = await page.evaluate(() =>
      document.querySelectorAll('.session-sidebar__switch[data-session-id]').length
    );
    await page.screenshot({ path: `${evidence}/readygate-spawn-boot.png`, fullPage: true });

    // R2: kill the server; the client must enter `reconnecting`.
    fs.writeFileSync(KILL_MARKER, 'go');
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'reconnecting',
      'R2: client never entered reconnecting after the server kill',
      20000
    );
    // Hold the outage >=12s so the sidebar's 8s poll fires at least once
    // while the WS is CONNECTING; every 300ms sample must be sentinel-free.
    let samples = 0;
    let outageStart = Date.now();
    while (Date.now() - outageStart < 12000) {
      const hit = await page.evaluate(scanSentinel);
      samples += 1;
      if (hit.body || hit.sidebar || hit.panel) {
        fail(`R2: 'load failed:' appeared during the outage (body=${hit.body} sidebar=${hit.sidebar} panel=${hit.panel}, sample ${samples})`);
      }
      await page.waitForTimeout(300);
    }
    // The sidebar empty state may show during the outage — that is fine; the
    // sentinel is what must never render.
    await page.screenshot({ path: `${evidence}/readygate-spawn-outage.png`, fullPage: true });

    // R3: respawn handshake; WS returns to `on`; sidebar repopulates WITHOUT
    // user action (the gated 8s poll resolves on notifyOpen).
    waitForFile(UP_MARKER, 'R3: server respawn');
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'R3: WS did not return to connected after the respawn',
      30000
    );
    await waitFor(
      page,
      () => document.querySelectorAll('.session-sidebar__switch[data-session-id]:not([data-session-id=""])').length >= 1,
      'R3: sidebar never repopulated after the reconnect (gated poll did not resolve)',
      30000
    );
    const postRows = await page.evaluate(() =>
      document.querySelectorAll('.session-sidebar__switch[data-session-id]').length
    );
    if (postRows < bootRows && bootRows > 0) {
      fail(`R3: sidebar lost rows after reconnect (boot ${bootRows}, post ${postRows})`);
    }
    let hit = await page.evaluate(scanSentinel);
    if (hit.body || hit.sidebar || hit.panel) {
      fail(`R3: 'load failed:' appeared after the reconnect (body=${hit.body} sidebar=${hit.sidebar} panel=${hit.panel})`);
    }
    await page.screenshot({ path: `${evidence}/readygate-spawn-reconnected.png`, fullPage: true });

    // R4: the session panel opens and loads its current-session block; the
    // sentinel stays absent after the panel's own load.
    await waitFor(page, () => document.getElementById('sidebar-manage-btn') !== null, 'R4: manage button missing');
    await page.click('#sidebar-manage-btn');
    await waitFor(page, () => document.getElementById('session-panel') !== null, 'R4: session panel did not open');
    await waitFor(
      page,
      () => {
        const value = document.querySelector('[data-testid="session-name-value"]');
        return value !== null && (value.textContent || '').trim() !== '';
      },
      'R4: session panel never loaded its current-session block after the reconnect',
      30000
    );
    hit = await page.evaluate(scanSentinel);
    if (hit.body || hit.sidebar || hit.panel) {
      fail(`R4: 'load failed:' appeared after the panel reload (body=${hit.body} sidebar=${hit.sidebar} panel=${hit.panel})`);
    }
    await page.screenshot({ path: `${evidence}/readygate-spawn-panel.png`, fullPage: true });

    console.log('web-readygate-spawn: PASSED (boot sidebar row; no "load failed:" across the 12s outage window crossing the 8s poll, after same-port respawn, or after the session panel reload; sidebar repopulated without user action)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-readygate-spawn: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
