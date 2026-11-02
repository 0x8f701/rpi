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
//   R1  boot: WS reaches `on`; the sidebar lists >=1 row with a non-empty
//       data-session-id (sessions scenario primary row)
//   R2  after the server kill, the client enters `reconnecting`; for the
//       whole outage window (>=12s, crossing the sidebar's 8s poll cadence)
//       no sidebar error surface exists: `.session-sidebar__error` count is
//       always zero and the body (minus the #toasts surface) never contains
//       any sidebar error string; the retained rows stay listed
//   R3  after the respawn on the same port, WS returns to `on` and the
//       sidebar eventually repopulates rows WITHOUT user action (gated poll
//       resolves via notifyOpen)
//   R4  the session panel (opened after reconnect via the New Session
//       footer entry) loads its current-session block; the error strings
//       are still absent after the panel reload
//   R6  switch-pending catalog coalescing: the lane holds the first
//       switch_session frame OFF the wire for >=12s (crossing the sidebar's
//       8s poll); during the whole window ZERO session_list frames go out,
//       `session switch pending` never renders anywhere (incl. #toasts),
//       rows stay; after the release the switch settles and exactly ONE
//       deferred session_list refreshes the catalog
//   R5  desktop + mobile screenshots of the reconnected sidebar

// The sidebar renders no error element at all (the error presentation is
// deleted as a concept), so the stable assertions are: zero
// `.session-sidebar__error` nodes anywhere, and none of the sidebar error
// strings (`load failed`, `session_list failed`, the RPC-limit text,
// `workflow_detail`, `session switch pending`) anywhere at all. A transient
// `not connected` (fail-fast active action) toast is expected and allowed.

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

/** Sidebar error strings that must never render anywhere — the deleted
 *  `load failed` sentinel, the business session_list failure, the global
 *  RPC limit text, workflow_detail, and the switch-pending transient that
 *  must stay fully internal (never rendered, never toasted). */
const FORBIDDEN_SIDEBAR_STRINGS = [
  'load failed',
  'session_list failed',
  'too many concurrent RPC commands',
  'workflow_detail',
  'session switch pending',
];

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

/** Error-concept scan: `.session-sidebar__error` must not exist at all, and
 *  no sidebar error string may appear in the body text with the #toasts
 *  surface (the only legitimate error-text carrier) stripped, or in the
 *  sidebar itself. Needles are an evaluate argument — a module-scope const
 *  is not serialized into the page function. */
function scanSentinel(needles) {
  const bodyClone = document.body.cloneNode(true);
  bodyClone.querySelector('#toasts')?.remove();
  const bodyText = bodyClone.textContent || '';
  const sidebarText = document.getElementById('session-sidebar')?.textContent || '';
  return {
    sidebarErrorCount: document.querySelectorAll('.session-sidebar__error').length,
    sidebarRows: document.querySelectorAll('.session-sidebar__switch[data-session-id]').length,
    bodyHit: needles.find((needle) => bodyText.includes(needle)) || null,
    sidebarHit: needles.find((needle) => sidebarText.includes(needle)) || null,
  };
}

function assertScanClean(hit, label) {
  if (hit.sidebarErrorCount !== 0) fail(`${label}: .session-sidebar__error rendered (count ${hit.sidebarErrorCount})`);
  if (hit.bodyHit) fail(`${label}: forbidden sidebar error string "${hit.bodyHit}" in body text`);
  if (hit.sidebarHit) fail(`${label}: forbidden sidebar error string "${hit.sidebarHit}" in the sidebar`);
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
      console.error(`web-readygate-spawn: page error: ${err.message}`);
    });

    // Observability-only WebSocket subclass (same capture pattern as the
    // projects lane): records every outgoing `session_list` and
    // `switch_session` frame. When the lane arms `__rpiHoldSwitch`, the
    // FIRST switch_session frame is held OFF the wire and its release
    // function parked in `__rpiReleaseSwitch` — a deterministic injected
    // switch delay the lane controls, guaranteed to cross the sidebar's 8s
    // poll. Frames are never mutated.
    await page.addInitScript(() => {
      window.__rpiCatalogFrames = [];
      window.__rpiSwitchFrames = [];
      window.__rpiHoldSwitch = false;
      window.__rpiReleaseSwitch = null;
      const NativeWebSocket = window.WebSocket;
      class SwitchPendingProbeWebSocket extends NativeWebSocket {
        send(data) {
          if (typeof data === 'string') {
            try {
              const frame = JSON.parse(data);
              if (frame?.type === 'session_list') window.__rpiCatalogFrames.push(frame);
              if (frame?.type === 'switch_session' && typeof window.__rpiReleaseSwitch !== 'function') {
                window.__rpiSwitchFrames.push(frame);
                window.__rpiReleaseSwitch = () => super.send(data);
                return undefined; // held: not on the wire until released
              }
            } catch {
              // Non-JSON frames pass through unchanged.
            }
          }
          return super.send(data);
        }
      }
      for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
        Object.defineProperty(SwitchPendingProbeWebSocket, key, { value: NativeWebSocket[key] });
      }
      window.WebSocket = SwitchPendingProbeWebSocket;
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'R1: page title missing');
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
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

    // R2: kill the server; the transport outage toast must appear.
    fs.writeFileSync(KILL_MARKER, 'go');
    await waitFor(
      page,
      () => document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length === 1,
      'R2: client never raised the transport outage toast after the server kill',
      20000
    );
    // Hold the outage >=12s so the sidebar's 8s poll fires at least once
    // while the WS is CONNECTING; every 300ms sample must be error-free and
    // the retained rows must stay listed (the catalog is never cleared).
    let samples = 0;
    let outageStart = Date.now();
    while (Date.now() - outageStart < 12000) {
      const hit = await page.evaluate(scanSentinel, FORBIDDEN_SIDEBAR_STRINGS);
      samples += 1;
      assertScanClean(hit, `R2 sample ${samples} (outage)`);
      if (hit.sidebarRows < bootRows) {
        fail(`R2: sidebar lost retained rows during the outage (boot ${bootRows}, sample ${samples} rows ${hit.sidebarRows})`);
      }
      await page.waitForTimeout(300);
    }
    // The sidebar keeps its retained rows during the outage — the empty
    // state must NOT appear; the error strings are what must never render.
    await page.screenshot({ path: `${evidence}/readygate-spawn-outage.png`, fullPage: true });

    // R3: respawn handshake; the transport toast clears and the sidebar
    // repopulates WITHOUT user action (the gated 8s poll resolves on
    // notifyOpen).
    waitForFile(UP_MARKER, 'R3: server respawn');
    await waitFor(
      page,
      () => document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length === 0,
      'R3: WS did not return to connected after the respawn (transport toast did not clear)',
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
    const hit = await page.evaluate(scanSentinel, FORBIDDEN_SIDEBAR_STRINGS);
    assertScanClean(hit, 'R3 (after reconnect)');
    await page.screenshot({ path: `${evidence}/readygate-spawn-reconnected.png`, fullPage: true });

    // R4: the session panel opens and loads its current-session block; the
    // sentinel stays absent after the panel's own load.
    const newEntryLabel = await page.evaluate(() => {
      const button = document.getElementById('sidebar-manage-btn');
      return button ? (button.textContent || '').trim() : null;
    });
    if (newEntryLabel !== 'New Session') fail(`R4: sidebar footer entry must read "New Session" (got ${JSON.stringify(newEntryLabel)})`);
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
    // QA G1/G2: the New Session entry must land on the empty cwd input —
    // it exists, is empty, and owns document focus after mount (the App
    // onOpenManage bounded focus retry; a few rAFs are allowed to land).
    const cwdState = await page.evaluate(() => {
      const input = document.getElementById('session-new-cwd-input');
      return {
        exists: input !== null,
        value: input ? String(input.value) : null,
        focused: input !== null && document.activeElement === input,
      };
    });
    if (!cwdState.exists) fail('R4: #session-new-cwd-input missing after the New Session entry');
    if (cwdState.value !== '') fail(`R4: #session-new-cwd-input must start empty (got ${JSON.stringify(cwdState.value)})`);
    if (!cwdState.focused) fail('R4: #session-new-cwd-input must own document focus after the New Session entry');
    const panelHit = await page.evaluate(scanSentinel, FORBIDDEN_SIDEBAR_STRINGS);
    assertScanClean(panelHit, 'R4 (after panel reload)');
    await page.screenshot({ path: `${evidence}/readygate-spawn-panel.png`, fullPage: true });

    // R6: switch-pending catalog coalescing. The first switch_session frame
    // was held OFF the wire by the probe; the lane releases it after a
    // >=12s pending window that crosses the sidebar's 8s poll. During the
    // whole window: ZERO session_list frames on the wire (every poll/
    // revision refresh intent coalesces into one deferred flush), zero
    // `session switch pending` text anywhere (body incl. #toasts and the
    // sidebar), and the rows stay. After the release the switch settles and
    // exactly ONE deferred session_list refreshes the catalog.
    const activeBefore = await page.evaluate(() =>
      document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
    );
    if (!activeBefore) fail('R6: no active session id before the switch');
    const switchTarget = await page.evaluate(() => {
      const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '';
      return [...document.querySelectorAll('.session-sidebar__switch[data-session-id]')]
        .map((r) => r.dataset.sessionId || '')
        .find((id) => id !== '' && id !== active) || '';
    });
    if (!switchTarget) fail('R6: no second sidebar row to switch to (seed row missing)');
    const catalogBaseline = await page.evaluate(() => window.__rpiCatalogFrames.length);
    await page.click(`.session-sidebar__switch[data-session-id="${switchTarget}"]`);
    await waitFor(
      page,
      () => window.__rpiSwitchFrames.length === 1 && typeof window.__rpiReleaseSwitch === 'function',
      'R6: switch_session frame never captured',
      15000
    );
    let pendingSamples = 0;
    const pendingStart = Date.now();
    while (Date.now() - pendingStart < 12000) {
      const sample = await page.evaluate((before) => ({
        catalogSent: window.__rpiCatalogFrames.length - before,
        rows: document.querySelectorAll('.session-sidebar__switch[data-session-id]').length,
        sidebarErrorCount: document.querySelectorAll('.session-sidebar__error').length,
        bodyText: document.body.textContent || '',
      }), catalogBaseline);
      if (sample.catalogSent !== 0) {
        fail(`R6: ${sample.catalogSent} session_list frame(s) sent during the pending window (must be 0)`);
      }
      if (sample.sidebarErrorCount !== 0) fail(`R6: .session-sidebar__error rendered during pending (count ${sample.sidebarErrorCount})`);
      if (sample.bodyText.includes('session switch pending')) fail('R6: "session switch pending" rendered in the body (incl. #toasts)');
      if (sample.rows < 2) fail(`R6: sidebar lost rows during the pending window (${sample.rows})`);
      await page.waitForTimeout(300);
    }
    await page.screenshot({ path: `${evidence}/readygate-spawn-switch-pending.png`, fullPage: true });
    const release = await page.evaluate(() => {
      const release = window.__rpiReleaseSwitch;
      window.__rpiReleaseSwitch = null;
      if (typeof release === 'function') release();
      return window.__rpiCatalogFrames.length;
    });
    const switchDeadline = Date.now() + 30000;
    for (;;) {
      const active = await page.evaluate(() =>
        document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
      );
      if (active === switchTarget) break;
      if (Date.now() > switchDeadline) fail(`R6: switch never settled (target ${switchTarget} never became active)`);
      await page.waitForTimeout(120);
    }
    const postSettleCatalog = await page.evaluate((before) => window.__rpiCatalogFrames.length - before, catalogBaseline);
    if (postSettleCatalog !== 1) {
      fail(`R6: after the settled switch exactly ONE deferred session_list is expected (got ${postSettleCatalog})`);
    }
    const postSwitchHit = await page.evaluate(scanSentinel, FORBIDDEN_SIDEBAR_STRINGS);
    assertScanClean(postSwitchHit, 'R6 (after switch settle)');
    await page.screenshot({ path: `${evidence}/readygate-spawn-switched.png`, fullPage: true });

    // R5: desktop + mobile screenshots of the reconnected, error-free
    // sidebar (the coordinator's inspected-frame evidence).
    await page.setViewportSize({ width: 1280, height: 800 });
    await page.waitForTimeout(400);
    await page.screenshot({ path: `${evidence}/readygate-spawn-desktop.png`, fullPage: false });
    await page.setViewportSize({ width: 375, height: 667 });
    await page.waitForTimeout(400);
    await page.screenshot({ path: `${evidence}/readygate-spawn-mobile.png`, fullPage: false });

    console.log('web-readygate-spawn: PASSED (boot sidebar row; zero .session-sidebar__error and no sidebar error strings across the 12s outage window crossing the 8s poll, after same-port respawn, and after the session panel reload with retained rows; sidebar repopulated without user action; held switch_session crossing the 8s poll sent ZERO session_list and rendered zero "session switch pending", settled switch sent exactly one deferred session_list; desktop+mobile screenshots captured)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-readygate-spawn: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
