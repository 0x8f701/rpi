// Web Code Review + Side Chat interaction regression lane (playwright half of
// E2E.d/web/code_review_side_chat.sh).
//
// PERMANENT repo-safe replacement for the deleted one-off probe
// (E2E.d/web/.omlo-code-review-probe.mjs, removed in 84cadf4): that probe
// hardcoded a private LAN endpoint, used fixed 35s sleeps, logged failures
// without failing, and always exited 0. This lane is the opposite: it runs
// against the fixture's isolated random-port listener, waits only on
// events (waitForFunction on DOM/ws state with deadlines — ZERO fixed
// setTimeout/waitForTimeout sleeps), hard-fails every assertion, and never
// carries a private address/session/token.
//
// Environment:
//   RPI_URL          http://127.0.0.1:<port>/web (fixture random port)
//   RPI_TOKEN        token file content (served via rpi-auth.<token> subprotocol)
//   RPI_CHROME       executable path of the system Chrome (optional)
//   RPI_EVIDENCE     evidence dir for screenshots
//   RPI_WORK         lane work dir for the kill/respawn marker handshake
//                    (defaults to the evidence dir)
//   INJECT_FAIL      self-test flag: '1' flips an expected value so the lane
//                    MUST fail (proves the assertions have teeth); default 0
//
// Assertions (each hard-fails with a nonzero exit; no log-and-continue):
//   A1  the fixture session is the authority: the active sidebar session id
//       captured at bootstrap is returned as the owner of a get_state
//       response (target session == fixture session)
//   A2  /code-review opens and settles with a REAL diff: the panel renders
//       the HEAD→working tree comparison label and the controlled dirty
//       fixture file appears as a selectable file row
//   A3  a post-code-review get_state round-trips on the SAME socket
//       generation (the review session causes no transport churn)
//   A4  the Side Chat panel opens from the header toggle, a named side chat
//       is created (side_chat_new forks a side agent — no model call until
//       a prompt) and becomes the ACTIVE tab, and the panel reports mounted
//       (title + composer ready)
//   A5  a REAL listener kill produces exactly one close with code === 1006
//       on the killed generation and exactly one transport toast; the lane
//       respawns on the SAME port (--resume the pre-outage primary) while
//       the shim fails the product's reconnect attempts synchronously
//       BEFORE any TCP socket opens (so no native ERR_CONNECTION_REFUSED is
//       manufactured — the product reconnect state machine owns every
//       generation); the product recovers on its own (generation advances,
//       socket OPEN), the transport toast clears to zero, and the side-chat
//       panel stays mounted with no inline/sidebar error
//   A6  ZERO browser console errors and ZERO page errors across the whole
//       lane (favicon filtered only; native ERR_CONNECTION_REFUSED handling
//       is never loosened — outage generations are shim-owned)
//
// The kill/respawn marker handshake mirrors reconnect.sh and
// code_review_paging.sh: this test writes kill-server.marker, the lane kills
// the listener, this test proves the 1006 + toast and writes
// outage-observed.marker, the lane respawns on the same port with --resume
// and touches server-up.marker, and this test waits for the product's
// recovery on DOM/ws events with deadlines.
import { chromium } from 'playwright';
import fs from 'node:fs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const work = process.env.RPI_WORK || evidence;
const DIRTY_FILE = process.env.RPI_DIRTY_FILE || 'greet.txt';
const INJECT_FAIL = process.env.INJECT_FAIL === '1';
// Self-test: expect TWO outage toasts when exactly one can ever appear.
const EXPECTED_OUTAGE_TOASTS = INJECT_FAIL ? 2 : 1;

function fail(message) {
  console.error(`web-code-review-side-chat: FAIL: ${message}`);
  process.exit(2);
}

/** Event-gated wait: page.waitForFunction with a deadline (no fixed sleeps). */
async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

/** Best-effort JSON decode of a raw WS frame payload; null for non-JSON. */
function parseFrame(raw) {
  try {
    return JSON.parse(raw);
  } catch {
    return null;
  }
}

/** In-page get_state through the live wrapper socket; resolves on the RPC
 *  response frame (event-gated via a chained onmessage handler). */
async function rpcGetState(page, label) {
  const sent = await page.evaluate((probeId) => {
    const ws = window.__scSideChat.ws;
    if (!ws || ws.readyState !== window.WebSocket.OPEN) {
      return { ok: false, reason: `socket not open (readyState=${ws && ws.readyState})` };
    }
    const sessionId = document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '';
    if (!sessionId) return { ok: false, reason: 'no canonical active sidebar row sessionId for get_state' };
    const prev = ws.onmessage;
    ws.onmessage = function (ev) {
      try {
        const parsed = typeof ev.data === 'string' ? JSON.parse(ev.data) : null;
        if (parsed && parsed.type === 'response' && parsed.id === probeId) {
          window.__scSideChat.probes = window.__scSideChat.probes || {};
          window.__scSideChat.probes[probeId] = {
            success: parsed.success === true,
            sessionId: parsed.sessionId || (parsed.data && parsed.data.sessionId) || null,
            error: parsed.success ? null : (parsed.error || 'rpc failed'),
          };
        }
      } catch {
        /* ignore */
      }
      if (typeof prev === 'function') prev.call(ws, ev);
    };
    ws.send(JSON.stringify({ type: 'get_state', id: probeId, sessionId }));
    return { ok: true, sessionId, probeId };
  }, `sc-${label}-${Date.now()}`);
  if (!sent.ok) fail(`${label}: get_state could not send: ${sent.reason}`);
  await waitFor(
    page,
    (probeId) => {
      const p = window.__scSideChat && window.__scSideChat.probes && window.__scSideChat.probes[probeId];
      return !!(p && (p.success || p.error));
    },
    `${label}: get_state did not round-trip`,
    10000,
    sent.probeId
  );
  const result = await page.evaluate((probeId) => window.__scSideChat.probes[probeId], sent.probeId);
  return { ...result, sentSessionId: sent.sessionId };
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
    // Pre-navigation WebSocket observer: wraps EVERY socket from page load so
    // the bootstrap socket is observable. Records generation/closes/pongs/
    // attempts and hands the live socket + failNextAttempts (pre-TCP shim)
    // to the test. No open-delay arming exists in this lane — observation only.
    await page.addInitScript(() => {
      const NativeWebSocket = window.WebSocket;
      const state = {
        generation: 0,
        closes: [],
        pongs: 0,
        attempts: 0,
        failedAttempts: [],
        ws: null,
        probes: {},
      };
      let failuresRemaining = 0;
      class ObservedWebSocket extends NativeWebSocket {
        constructor(...args) {
          state.attempts += 1;
          // Outage seam (A5): the product's reconnect attempts fail
          // synchronously BEFORE Chromium opens a TCP socket, so the
          // respawning listener on the same port is never hit by a refused
          // connect while it is down (keeps the strict zero-console-error
          // policy intact — no native ERR_CONNECTION_REFUSED is
          // manufactured; the product reconnect state machine owns every
          // generation, mirroring reconnect.sh and code_review_paging.sh).
          if (failuresRemaining > 0) {
            failuresRemaining -= 1;
            state.failedAttempts.push(state.attempts);
            throw new Error('web-code-review-side-chat outage');
          }
          super(...args);
          state.generation += 1;
          const gen = state.generation;
          state.ws = this;
          this.addEventListener('close', (ev) => {
            state.closes.push({ gen, code: ev.code, reason: ev.reason, at: Date.now() });
          });
          this.addEventListener('message', (ev) => {
            try {
              const parsed = typeof ev.data === 'string' ? JSON.parse(ev.data) : null;
              if (parsed && parsed.type === 'pong') state.pongs += 1;
            } catch {
              /* ignore */
            }
          });
        }
      }
      for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
        Object.defineProperty(ObservedWebSocket, key, { value: NativeWebSocket[key] });
      }
      window.WebSocket = ObservedWebSocket;
      window.__scSideChat = state;
      window.__scSideChatTest = {
        failNextAttempts(count) {
          failuresRemaining = count;
          state.failuresRemaining = count;
        },
      };
    });

    const pageErrors = [];
    const consoleErrors = [];
    page.on('pageerror', (err) => {
      pageErrors.push(String(err && err.message ? err.message : err));
    });
    page.on('console', (msg) => {
      if (msg.type() === 'error') consoleErrors.push(msg.text());
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    // A1 bootstrap: the wrapper observed a connection AND the live socket is
    // OPEN (authority is the wrapper, not the conn-state pill).
    await waitFor(
      page,
      () => document.title === 'rpi web',
      'A1: page title missing'
    );
    await waitFor(
      page,
      () => window.__scSideChat && window.__scSideChat.generation >= 1
        && window.__scSideChat.ws && window.__scSideChat.ws.readyState === window.WebSocket.OPEN,
      'A1: WS did not reach connected',
      30000
    );
    await waitFor(
      page,
      () => !!document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId,
      'A1: primary session row did not bind after bootstrap',
      30000
    );
    const generationBefore = await page.evaluate(() => window.__scSideChat.generation);
    const activeSessionId = await page.evaluate(() =>
      document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
    );
    if (!activeSessionId) fail('A1: no active fixture session id');
    const a1 = await rpcGetState(page, 'a1');
    if (!a1.success) fail(`A1: get_state failed: ${JSON.stringify(a1)}`);
    if (a1.sessionId !== activeSessionId) {
      fail(`A1: get_state owner does not match the fixture session (owner=${a1.sessionId}, fixture=${activeSessionId})`);
    }
    await page.screenshot({ path: `${evidence}/01-connected.png`, fullPage: true });

    // A2: /code-review opens and settles with a real diff of the dirty file.
    await page.fill('#prompt-input', '/code-review');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') !== null,
      'A2: code-review panel did not open on Enter',
      15000
    );
    await page.screenshot({ path: `${evidence}/02-code-review-open.png`, fullPage: true });
    await waitFor(
      page,
      () => {
        const label = document.querySelector('.code-review__label')?.textContent || '';
        return label.includes('HEAD') && label.includes('working tree');
      },
      'A2: comparison label missing',
      30000
    );
    await waitFor(
      page,
      (dirty) => Array.from(document.querySelectorAll('button.code-review__file'))
        .some((b) => b.getAttribute('data-file-path') === dirty),
      'A2: controlled dirty file did not appear as a selectable diff row',
      30000,
      DIRTY_FILE
    );
    await page.screenshot({ path: `${evidence}/03-code-review-settled.png`, fullPage: true });

    // A3: post-code-review get_state round-trips on the SAME generation.
    const a3 = await rpcGetState(page, 'a3');
    if (!a3.success) fail(`A3: post-code-review get_state failed: ${JSON.stringify(a3)}`);
    if (a3.sessionId !== activeSessionId) {
      fail(`A3: post-code-review get_state owner drifted (owner=${a3.sessionId}, fixture=${activeSessionId})`);
    }
    const generationAfterCr = await page.evaluate(() => window.__scSideChat.generation);
    if (generationAfterCr !== generationBefore) {
      fail(`A3: socket generation changed across the code review (${generationBefore} -> ${generationAfterCr})`);
    }
    await page.screenshot({ path: `${evidence}/04-post-cr-getstate.png`, fullPage: true });

    // A4: Side Chat opens via #sidechat-toggle-btn (App.tsx:8327), mounts
    // #side-chat-panel (SideChatPanel.tsx:675, App.tsx:3825, styles.css:2607),
    // and New is button.side-chat__new (SideChatPanel.tsx:708-718) not an input.
    await page.evaluate(() => {
      document.getElementById('sidechat-toggle-btn')?.click();
    });
    await waitFor(
      page,
      () => document.querySelector('#side-chat-panel[aria-label="Side chat"] .side-chat') !== null,
      'A4: side chat panel did not mount',
      15000
    );
    const sideChatMounted = await page.evaluate(() => ({
      title: document.querySelector('.side-chat__title')?.textContent || '',
      composer: document.querySelector('.side-chat__composer textarea') !== null,
      newTab: document.querySelector('button.side-chat__new[aria-label="New side-chat tab"]') !== null,
    }));
    if (!sideChatMounted.title.includes('Side chat')) fail(`A4: side chat title missing: ${JSON.stringify(sideChatMounted)}`);
    if (!sideChatMounted.composer || !sideChatMounted.newTab) {
      fail(`A4: side chat panel did not report mounted (composer/new missing): ${JSON.stringify(sideChatMounted)}`);
    }
    const tabsBefore = await page.evaluate(() => document.querySelectorAll('.side-chat__tab-select').length);
    await page.click('button.side-chat__new[aria-label="New side-chat tab"]');
    await waitFor(
      page,
      (before) => document.querySelectorAll('.side-chat__tab-select').length > before
        && !!document.querySelector('.side-chat__tab--active .side-chat__tab-select')?.textContent.trim(),
      'A4: created side chat never became the active tab',
      15000,
      tabsBefore
    );
    const sideChatState = await page.evaluate(() => ({
      activeTab: document.querySelector('.side-chat__tab--active .side-chat__tab-select')?.textContent.trim() || '',
      composerReady: document.querySelector('.side-chat__composer textarea') !== null,
      tabs: Array.from(document.querySelectorAll('.side-chat__tab-select')).map((t) => t.textContent.trim()),
    }));
    if (!sideChatState.activeTab || !sideChatState.composerReady || !sideChatState.tabs.includes(sideChatState.activeTab)) {
      fail(`A4: side chat creation did not stick: ${JSON.stringify(sideChatState)}`);
    }
    await page.screenshot({ path: `${evidence}/05-side-chat-created.png`, fullPage: true });

    // Seed the primary session with a real prompt round-trip BEFORE the
    // kill: the respawned listener --resumes this exact session id (A5's
    // same-port recovery needs the pre-outage primary to be durable). The
    // only prior LLM calls in this lane are none (side-chat creation forks
    // without a model call), so this prompt is mock request 1 (odd) -> the
    // steering slow stream ending in the deterministic "chunk-four-done"
    // tail (the reconnect lane's FIRST_TAIL marker).
    await page.fill('#prompt-input', 'seed the primary session for the respawn');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.body.textContent.includes('chunk-four-done'),
      'A5: seed prompt round-trip did not complete before the kill',
      30000
    );

    // A5: REAL abnormal close — the lane KILLS the rpi listener process so
    // the transport is severed at the socket level with NO WebSocket.close
    // call; the close handshake can never complete and Chromium reports the
    // genuine abnormal-closure code 1006. The lane respawns the listener on
    // the SAME port (--resume the pre-outage primary); the shim fails the
    // product's next reconnect attempts synchronously before any TCP socket
    // opens (no native ERR_CONNECTION_REFUSED manufactured).
    const preKill = await page.evaluate(() => ({
      generation: window.__scSideChat.generation,
      failedAttempts: (window.__scSideChat.failedAttempts || []).length,
      closes: window.__scSideChat.closes.slice(),
    }));
    const primarySessionId = await page.evaluate(() =>
      document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
    );
    if (!primarySessionId) fail('A5: no active sessionId for the respawn --resume');
    fs.writeFileSync(`${work}/listener-primary-session-id`, `${primarySessionId}\n`);
    await page.evaluate(() => window.__scSideChatTest.failNextAttempts(3));
    fs.writeFileSync(`${work}/kill-server.marker`, 'kill now\n');
    await waitFor(
      page,
      () => (window.__scSideChat.closes || []).length > 0,
      'A5: real listener kill did not close the live socket',
      30000
    );
    // The toast render follows the onclose handler's state update; wait for
    // it so the exact-count sample below is race-free.
    await waitFor(
      page,
      () => document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length === 1,
      'A5: transport toast did not appear for the outage',
      10000
    );
    const outage = await page.evaluate(() => ({
      closes: window.__scSideChat.closes.slice(),
      generation: window.__scSideChat.generation,
      transportToasts: document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length,
      failedAttempts: (window.__scSideChat.failedAttempts || []).length,
    }));
    const killCloses = outage.closes.filter((c) => c.gen === preKill.generation);
    if (killCloses.length !== 1 || killCloses[0].code !== 1006) {
      fail(`A5: expected exactly one 1006 on the killed generation (real listener kill — no synthetic close(code) is used), got ${JSON.stringify(killCloses)}`);
    }
    if (outage.transportToasts !== EXPECTED_OUTAGE_TOASTS) {
      fail(`A5: expected exactly ${EXPECTED_OUTAGE_TOASTS} transport toast(s) after the real 1006, got ${outage.transportToasts}`);
    }
    fs.writeFileSync(`${work}/outage-observed.marker`, 'real close code 1006 observed\n');
    // The lane respawns the listener on the same port. The three shimmed
    // failed generations prove the product keeps retrying while the port is
    // down; then the respawned listener accepts and the client recovers.
    await waitFor(
      page,
      (baseline) => (window.__scSideChat.failedAttempts || []).length >= baseline + 3,
      'A5: the product did not execute three failed reconnect attempts during the outage',
      40000,
      preKill.failedAttempts
    );
    await waitFor(
      page,
      (killedGen) => window.__scSideChat.generation > killedGen
        && window.__scSideChat.ws && window.__scSideChat.ws.readyState === window.WebSocket.OPEN,
      'A5: did not reconnect after the listener respawned',
      90000,
      preKill.generation
    );
    // The transport toast clears after recovery (same contract as the
    // reconnect lane); the outage sample already pinned the exact count.
    await waitFor(
      page,
      () => document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length === 0,
      'A5: transport toast did not clear after recovery',
      15000
    );
    const recovered = await page.evaluate(() => ({
      generation: window.__scSideChat.generation,
      closes: window.__scSideChat.closes.slice(),
      sideChatMounted: document.querySelector('#side-chat-panel[aria-label="Side chat"] .side-chat') !== null,
      composerReady: document.querySelector('.side-chat__composer textarea') !== null,
      inlineError: document.querySelector('.code-review__error')?.textContent || null,
      sidebarError: document.querySelector('.session-sidebar__error')?.textContent || null,
      transportToasts: document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length,
    }));
    if (recovered.generation <= preKill.generation) {
      fail(`A5: post-kill generation did not advance: ${JSON.stringify(recovered)}`);
    }
    if (recovered.closes.length !== outage.closes.length) {
      fail(`A5: unexpected extra closes after recovery: ${JSON.stringify(recovered.closes)}`);
    }
    if (recovered.transportToasts !== 0) fail(`A5: transport toast survived recovery: ${recovered.transportToasts}`);
    if (!recovered.sideChatMounted || !recovered.composerReady) {
      fail(`A5: side chat panel did not stay mounted after recovery: ${JSON.stringify(recovered)}`);
    }
    if ((recovered.inlineError || '').includes('connection closed') || (recovered.inlineError || '').includes('1006')) {
      fail(`A5: transport error rendered inline after reconnect: ${recovered.inlineError}`);
    }
    if (recovered.sidebarError) fail(`A5: sidebar error after reconnect: ${recovered.sidebarError}`);
    await page.screenshot({ path: `${evidence}/06-recovered.png`, fullPage: true });

    // A6: the WHOLE lane ran with ZERO browser console/page errors.
    const cleanConsole = consoleErrors.filter((t) => !t.includes('favicon'));
    if (pageErrors.length) fail(`A6: page errors: ${JSON.stringify(pageErrors)}`);
    if (cleanConsole.length) fail(`A6: console errors: ${JSON.stringify(cleanConsole)}`);
    await page.screenshot({ path: `${evidence}/07-final.png`, fullPage: true });

    fs.writeFileSync(`${evidence}/code-review-side-chat-report.json`, JSON.stringify({
      fixtureSessionId: activeSessionId,
      generationBefore,
      generationAfterCr,
      a1,
      a3,
      sideChat: sideChatState,
      killCloses,
      outage: {
        transportToasts: outage.transportToasts,
        generation: outage.generation,
      },
      recovered: {
        generation: recovered.generation,
        sideChatMounted: recovered.sideChatMounted,
        composerReady: recovered.composerReady,
        transportToasts: recovered.transportToasts,
        inlineError: recovered.inlineError,
        sidebarError: recovered.sidebarError,
      },
      pageErrors,
      consoleErrors: cleanConsole,
    }, null, 2));

    console.log('web-code-review-side-chat: PASSED (A1 fixture-session get_state owner; A2 /code-review opens + real dirty-file diff; A3 post-CR get_state on the same generation; A4 side chat panel mounts + named chat created + active tab + composer ready; A5 real 1006 kill -> exactly one close/toast -> same-port --resume respawn -> recovery generation + panel stays mounted + toast clears; A6 zero browser console/page errors)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-code-review-side-chat: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
