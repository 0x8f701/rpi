// Web ready-gate E2E lane (playwright half of E2E.d/web/readygate.sh).
//
// Reproduces the mount-before-WebSocket-OPEN regression and verifies the
// generation-safe bounded ready gate:
//   1. A WebSocket shim HOLDS every socket's native open (and shadows
//      readyState to CONNECTING) so the sidebar/session-panel mount BEFORE
//      the App's onOpen runs — the exact window where `sendCommand` used to
//      reject with `not connected` and surface a persistent
//      `load failed: not connected`.
//   2. The test releases the current generation's held open through
//      `__rpiReadyGateTest.releaseCurrentOpen()`; the App's onOpen fires, the
//      gate resolves, and the sidebar finally loads the primary session row
//      with NO `load failed: not connected` anywhere on the page.
//   3. An active New-session click DURING the hold fails FAST through Manage
//      Session; the panel shows `new session failed: not connected`, never a
//      15s hang and never `load failed: not connected`.
//   4. Simulated reconnect: close the socket, the replacement's open is held
//      again, the gate re-arms, and the sidebar reloads with the session row
//      still present and still no `load failed: not connected`.
//
// Timing model: an explicit hold/release handshake replaces the old fixed
// `setTimeout` open delay — a socket stays CONNECTING until the test calls
// `releaseCurrentOpen()`, so no assertion can race the open. (The old 700ms
// delay flaked under coverage instrumentation, where page work can straddle
// the timer: the fail-fast click or the outage sample could land after the
// delayed open had already fired.) Generation-safe: every constructed socket
// bumps `gen` and becomes `current`; `releaseCurrentOpen()` releases only the
// CURRENT generation's held open, and a native open that arrives before OR
// after the release triggers the App's onopen exactly once.
//
// Environment:
//   RPI_URL         http://127.0.0.1:<port>/web
//   RPI_TOKEN       token file content (served via rpi-auth.<token> subprotocol)
//   RPI_CHROME      executable path of the system Chrome (optional)
//   RPI_EVIDENCE    evidence dir for screenshots
//   RPI_OPEN_DELAY  optional settle ms applied ONLY after a release (default 0)

import { chromium } from 'playwright';
import {
  assertSidebarNewActionAbsent,
  clickNewSessionExpectingFailureViaManage,
} from './lib/session-lifecycle.mjs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const settleAfterRelease = Number(process.env.RPI_OPEN_DELAY || 0);

const SENTINEL = 'load failed: not connected';

function fail(message) {
  console.error(`web-readygate: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 30000, arg) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      if (await page.evaluate(fn, arg)) return;
    } catch {
      /* page mid-update */
    }
    await page.waitForTimeout(80);
  }
  fail(`${label} (within ${timeoutMs}ms)`);
}

// Sample the whole document text + the sidebar error element for the sentinel.
// Returns true the moment the sentinel is NOT present (used to assert the
// clean state holds across the held window and after open/reconnect).
async function assertNoLoadFailed(page, label) {
  const present = await page.evaluate((sentinel) => {
    const body = document.body ? document.body.textContent || '' : '';
    const sidebarErr = document.querySelector('.session-sidebar__error');
    const panelStatus = document.querySelector('.panel__status');
    const parts = [body, sidebarErr ? sidebarErr.textContent || '' : '', panelStatus ? panelStatus.textContent || '' : ''];
    return parts.some((t) => t.includes(sentinel));
  }, SENTINEL);
  if (present) fail(`${label}: page contains "${SENTINEL}"`);
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  const browserErrors = [];
  try {
    const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }

    // --- WebSocket shim: HOLD every socket's native open until the test
    //     releases it, shadowing readyState to CONNECTING meanwhile so
    //     sendCommand's readyState guard rejects (reproducing the
    //     mount-before-OPEN race). The App's onopen handler is dispatched
    //     only by `releaseCurrentOpen()`; onmessage/onclose/onerror stay
    //     native. Each constructed socket bumps `gen` and becomes `current`,
    //     so a release always targets the newest generation and a superseded
    //     socket's held open can never fire into the App. ---
    await page.addInitScript(() => {
      const NativeWebSocket = window.WebSocket;
      let gen = 0;
      let current = null;

      // Dispatch a held native open to the App's onopen exactly once: flip
      // the shadowed readyState to OPEN FIRST, then call the handler, so gate
      // waiters (and sendCommand's readyState guard) observe OPEN.
      function dispatchOpen(socket, state) {
        if (state.fired || state.detached || state.dead || state.held === null) return;
        state.fired = true;
        const event = state.held;
        state.held = null;
        state.ready = NativeWebSocket.OPEN;
        const fn = socket.__realOnOpen;
        if (typeof fn === 'function') fn.call(socket, event);
      }

      class TestWebSocket extends NativeWebSocket {
        constructor(...args) {
          super(...args);
          const state = {
            gen: ++gen,
            ready: NativeWebSocket.CONNECTING,
            held: null,      // native open event waiting for release
            released: false, // releaseCurrentOpen() called -> release on arrival
            fired: false,    // the App's onopen already ran for this generation
            detached: false, // App nulled onopen (replace/unmount): never fire late
            dead: false,     // socket closed before release
          };
          this.__state = state;
          this.__realOnOpen = null;
          // Shadow readyState so the App sees CONNECTING until the held open
          // is released — the exact condition under which sendCommand rejects
          // with `not connected`.
          Object.defineProperty(this, 'readyState', {
            get: () => state.ready,
            configurable: true,
          });
          current = this;
        }
      }

      // Override onopen on the prototype: register the App's handler through
      // the NATIVE addEventListener so the real 'open' event is CAPTURED and
      // held instead of delivered. The listener attaches once (the App assigns
      // onopen synchronously after construction, before the loopback handshake
      // completes) and dispatches the LATEST handler on release.
      Object.defineProperty(TestWebSocket.prototype, 'onopen', {
        get: function () { return this.__realOnOpen || null; },
        set: function (fn) {
          this.__realOnOpen = fn;
          const state = this.__state;
          if (fn === null) {
            // App detach (detachTransportHandlers on replace/unmount): a held
            // open from this generation must never fire into the App.
            state.detached = true;
            return;
          }
          if (!state.listening) {
            state.listening = true;
            const socket = this;
            NativeWebSocket.prototype.addEventListener.call(socket, 'open', (event) => {
              if (state.detached || state.fired || state.dead) return;
              state.held = event;
              if (state.released) dispatchOpen(socket, state);
            });
            // A socket that closes while still held must never dispatch later.
            NativeWebSocket.prototype.addEventListener.call(socket, 'close', () => {
              state.held = null;
              state.dead = true;
            });
          }
        },
        configurable: true,
      });
      for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
        Object.defineProperty(TestWebSocket, key, { value: NativeWebSocket[key] });
      }
      window.WebSocket = TestWebSocket;
      window.__rpiReadyGateTest = {
        /** Closes the current socket (simulated outage); the App's onclose
         *  schedules the reconnect, whose replacement becomes the new current
         *  generation. */
        closeCurrent() { try { current && current.close(4000, 'ready-gate test outage'); } catch { /* */ } },
        failCurrentOpen() {
          const socket = current;
          const state = socket && socket.__state;
          if (!socket || !state || state.dead) return;
          state.released = true;
          const fail = () => {
            if (state.dead || state.detached) return;
            state.held = null;
            state.ready = NativeWebSocket.CLOSED;
            state.dead = true;
            const close = socket.onclose;
            if (typeof close === 'function') {
              close.call(socket, { target: socket, code: 1006, reason: '', wasClean: false });
            }
          };
          if (state.held !== null) fail();
          else NativeWebSocket.prototype.addEventListener.call(socket, 'open', fail, { once: true });
        },
        /** Release the CURRENT generation's held open: if the native open
         *  already arrived it dispatches now; otherwise it dispatches the
         *  moment the open arrives. Exactly one App onopen per generation. */
        releaseCurrentOpen() {
          const s = current && current.__state;
          if (!s || s.fired || s.detached || s.dead) return;
          s.released = true;
          if (s.held !== null) dispatchOpen(current, s);
        },
        /** Monotonic socket-generation counter: increments for every
         *  constructed socket. The test waits for a bump before releasing so
         *  the release targets the replacement, never the closed old socket. */
        get gen() { return gen; },
      };
    });

    page.on('pageerror', (error) => browserErrors.push(`pageerror: ${error.message}`));
    page.on('console', (message) => {
      if (message.type() === 'error') browserErrors.push(`console: ${message.text()}`);
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');

    // The sidebar mounts IMMEDIATELY, with the socket's open HELD (readyState
    // stays CONNECTING). Wait for the sidebar's pre-load empty state: it only
    // renders while the mount auto-load is still gated (no error, no rows) —
    // deterministic proof we are sampling inside the mount-before-OPEN window.
    await waitFor(
      page,
      () => document.querySelector('.session-sidebar__empty') !== null,
      'sidebar never reached its gated pre-open state while the open was held',
    );
    await assertNoLoadFailed(page, 'during mount-before-OPEN window');

    // Active New-session click DURING the hold must FAIL FAST through the sole
    // lifecycle route: Manage Session -> New session. The panel surfaces a
    // transient `not connected`, never a 15s hang and never
    // `load failed: not connected`. The held open guarantees the socket is
    // still CONNECTING here — no timing race.
    await assertSidebarNewActionAbsent(page, 'obsolete sidebar New action rendered during ready-gate hold');
    const activeErr = await clickNewSessionExpectingFailureViaManage(page, waitFor, 'active New during disconnect', {
      failureText: 'not connected',
      timeoutMs: 3000,
      closePanel: false,
    });
    if (activeErr.includes(SENTINEL)) {
      fail(`active New click surfaced "${SENTINEL}" instead of fail-fast not connected`);
    }
    await page.screenshot({ path: `${evidence}/readygate-active-failfast.png`, fullPage: true });

    // Release the held open: the App's onOpen runs, the gate resolves the
    // mount auto-load, and the sidebar loads the primary session row — no
    // `load failed: not connected`.
    await page.evaluate(() => window.__rpiReadyGateTest.releaseCurrentOpen());
    if (settleAfterRelease > 0) await page.waitForTimeout(settleAfterRelease);
    await waitFor(
      page,
      () => {
        const rows = [...document.querySelectorAll('.session-sidebar__switch')];
        return rows.length >= 1 && rows.every((r) => (r.dataset.sessionId || '') !== '');
      },
      'primary session row never appeared in the sidebar after the release',
    );
    await assertNoLoadFailed(page, 'after released-open sidebar load');
    await page.screenshot({ path: `${evidence}/readygate-sidebar-loaded.png`, fullPage: true });

    // The full Session panel is already open from the fail-fast lifecycle
    // attempt. After release its gated mount load must populate normally.
    await waitFor(
      page,
      () => {
        const name = document.querySelector('[data-testid="session-name-value"]');
        return name !== null;
      },
      'session panel never rendered the current-session block',
    );
    await assertNoLoadFailed(page, 'after session panel mount');
    await page.screenshot({ path: `${evidence}/readygate-panel-loaded.png`, fullPage: true });

    // --- Simulated reconnect: close the socket; the replacement's open is
    //     HELD again, the gate re-arms for the sidebar poll, and the sidebar
    //     reloads with the session row still present. The release below is
    //     issued only after the generation bump, so it targets the
    //     replacement, never the closed old socket. ---
    const genBeforeReconnect = await page.evaluate(() => window.__rpiReadyGateTest.gen);
    await page.evaluate(() => window.__rpiReadyGateTest.closeCurrent());
    await waitFor(
      page,
      () => document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length === 1,
      'socket never dropped after closeCurrent (no transport outage toast)',
      5000,
    );
    // The replacement socket exists only after the reconnect backoff; the gen
    // bump is the deterministic signal that its open is now being held.
    await waitFor(
      page,
      (genBefore) => window.__rpiReadyGateTest.gen > genBefore,
      'replacement socket never created after closeCurrent',
      10000,
      genBeforeReconnect,
    );
    // During the reconnect outage the replacement's open is held, so the gate
    // holds the sidebar poll — the sentinel must not appear.
    await assertNoLoadFailed(page, 'during reconnect outage');
    await page.evaluate(() => window.__rpiReadyGateTest.releaseCurrentOpen());
    if (settleAfterRelease > 0) await page.waitForTimeout(settleAfterRelease);
    await waitFor(
      page,
      () => {
        const rows = [...document.querySelectorAll('.session-sidebar__switch')];
        return rows.length >= 1 && rows.every((r) => (r.dataset.sessionId || '') !== '');
      },
      'primary session row missing after reconnect',
    );
    await assertNoLoadFailed(page, 'after reconnect sidebar reload');
    await page.screenshot({ path: `${evidence}/readygate-reconnect-loaded.png`, fullPage: true });

    // Repeated abnormal generations: three replacements fail with 1006 before
    // the fourth is released. The page must keep retrying and converge without
    // refresh; the retained session rows remain authoritative.
    let failedGen = await page.evaluate(() => window.__rpiReadyGateTest.gen);
    for (let attempt = 0; attempt < 3; attempt += 1) {
      await page.evaluate(() => window.__rpiReadyGateTest.closeCurrent());
      await waitFor(
        page,
        (previous) => window.__rpiReadyGateTest.gen > previous,
        `repeated reconnect attempt ${attempt + 1} never constructed`,
        20000,
        failedGen,
      );
      if (attempt === 0) {
        await waitFor(
          page,
          () => document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length >= 1,
          'no transport outage toast during repeated outage',
          15000,
        );
        await page.screenshot({ path: `${evidence}/readygate-repeated-reconnecting.png`, fullPage: true });
      }
      failedGen = await page.evaluate(() => window.__rpiReadyGateTest.gen);
      await page.evaluate(() => window.__rpiReadyGateTest.failCurrentOpen());
      await waitFor(
        page,
        (previous) => window.__rpiReadyGateTest.gen > previous,
        `repeated reconnect attempt ${attempt + 1} did not retry after 1006`,
        30000,
        failedGen,
      );
      failedGen = await page.evaluate(() => window.__rpiReadyGateTest.gen);
    }
    await page.evaluate(() => window.__rpiReadyGateTest.releaseCurrentOpen());
    await waitFor(
      page,
      () => document.querySelectorAll('.session-sidebar__switch[data-session-id]:not([data-session-id=""])').length >= 1,
      'session rows were not restored after repeated reconnects',
      30000,
    );
    await assertNoLoadFailed(page, 'after repeated reconnect convergence');
    await page.screenshot({ path: `${evidence}/readygate-repeated-reconnect-loaded.png`, fullPage: true });
    if (browserErrors.length > 0) {
      fail(`browser errors observed: ${browserErrors.join(' | ')}`);
    }

    console.log('web-readygate: PASSED (mount gate + active fail-fast + repeated 1006 reconnect convergence, retained session rows, zero browser errors)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-readygate: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
