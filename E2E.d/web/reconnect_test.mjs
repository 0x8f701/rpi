// Auto-reconnect web E2E lane (playwright half of E2E.d/web/reconnect.sh).
//
// Environment:
//   RPI_URL         http://127.0.0.1:<port>/web
//   RPI_TOKEN       token file content (served via rpi-auth.<token> subprotocol)
//   RPI_FIRST_TAIL  tail of the FIRST slow mock reply ("chunk-four-done")
//   RPI_FAST_REPLY  instant mock reply text ("steering-followup-reply")
//   RPI_WORK        shared dir for the kill/restart handshake with the lane:
//                     test writes kill-server.marker, then writes
//                     outage-observed.marker after the real 1006; the lane
//                     respawns on the SAME port and writes server-up.marker
//   RPI_CHROME      executable path of the system Chrome (optional)
//   RPI_EVIDENCE    evidence dir for screenshots
//
//   - one file_explorer_read is in flight on the killed listener; the same
//     mounted drawer/tab retries exactly once after same-port recovery
// Asserts (regression guard for App.tsx reconnect bootstrap):
//   1. two loaded sessions remain subscribed while session A streams in the
//      background and session B is active
//   2. a same-process WebSocket outage lets A finish while disconnected;
//      automatic reconnect restores A's authoritative completed transcript,
//      keeps B active, and does not duplicate either session transcript
//   3. a real listener restart reports 1006, drives three failed product
//      reconnect generations, recovers on the same port, and round-trips again
//
// Connection-state authority (no #conn-state element — it is removed from
// App.tsx; connection problems surface ONLY through the transport toast):
// the WebSocket shim's nativeAttempts/closeCodes trace the real socket
// generations, the transport outage toast (#toasts .toast[data-toast-kind=
// "transport"]) marks every close and clears on recovery, and the sidebar
// row / Send button bind only after the authoritative bootstrap. A
// mid-stream socket drop (gated mock turn) proves the connected authority is
// the transport itself, never the Send/Stop action: Stop stays enabled during
// the outage while the socket is closed, then recovers with exactly-once
// settlement.

import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';
import { createFreshSessionViaManage } from './lib/session-lifecycle.mjs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const firstTail = process.env.RPI_FIRST_TAIL || 'chunk-four-done';
const fastReply = process.env.RPI_FAST_REPLY || 'steering-followup-reply';
const work = process.env.RPI_WORK || '.';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const mockPort = process.env.RPI_MOCK_PORT || '';
const pageErrors = [];
const consoleErrors = [];

function parsedFrames(entries) {
  return entries.map(({ order, direction, gen, url: socketUrl, raw }) => {
    try {
      return { order, direction, gen, socketUrl, ...JSON.parse(raw) };
    } catch {
      return { order, direction, gen, socketUrl, invalid: true, raw };
    }
  });
}

const KILL_MARKER = path.join(work, 'kill-server.marker');
const UP_MARKER = path.join(work, 'server-up.marker');
const OUTAGE_OBSERVED_MARKER = path.join(work, 'outage-observed.marker');

const backgroundHead = 'steer-1-';
const postRestartTail = process.env.RPI_POST_RESTART_REPLY || 'reconnect-post-restart-reply';
const backgroundReply = `steer-1-ing stream ${firstTail}`;

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

async function waitForError(page, fn, label, timeoutMs = 25000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    throw new Error(`${label} (timeout ${timeoutMs}ms)`);
  }
}

function waitForFile(file, label, timeoutMs = 60000) {
  // Promise-based polling (setTimeout), NOT Atomics.wait: a blocking sleep
  // would freeze the event loop and starve the socket/toast observers that
  // gate the reconnect assertions.
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

async function rpc(page, command) {
  const response = await page.evaluate(async ({ command, token }) => {
    const headers = { 'content-type': 'application/json' };
    if (token) headers.authorization = `Bearer ${token}`;
    const result = await fetch('/rpc', {
      method: 'POST',
      headers,
      body: JSON.stringify(command),
    });
    return { status: result.status, text: await result.text() };
  }, { command, token });
  let frame;
  try {
    frame = JSON.parse(response.text);
  } catch {
    fail(`RPC ${command.type} returned non-JSON status=${response.status}`);
  }
  if (response.status !== 200 || !frame.success) {
    fail(`RPC ${command.type} failed status=${response.status}: ${frame.error || response.text}`);
  }
  return frame.data || {};
}

async function activeSessionId(page) {
  return page.evaluate(() =>
    document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
  );
}

function rectanglesOverlap(a, b) {
  return a && b
    && a.x < b.x + b.width
    && a.x + a.width > b.x
    && a.y < b.y + b.height
    && a.y + a.height > b.y;
}

async function captureReconnectState(page, state, options) {
  const panelWasOpen = (await page.locator('#session-panel').count()) === 1;
  if (panelWasOpen) {
    await page.evaluate(() => document.getElementById('session-close-btn')?.click());
    await waitFor(page, () => document.getElementById('session-panel') === null, `${state}: panel did not close for composer proof`);
  }
  const captures = [];
  for (const viewport of [
    { name: 'desktop', width: 1440, height: 1000 },
    { name: 'mobile', width: 390, height: 844 },
  ]) {
    await page.setViewportSize({ width: viewport.width, height: viewport.height });
    if (viewport.name === 'desktop') {
      await waitFor(page, () => document.querySelector('.app-layout')?.classList.contains('app-layout--files-overlay') !== true,
        `${state}: desktop Files overlay class stayed on at ${viewport.width}`);
      const railOpen = await page.locator('.app-layout').evaluate((layout) =>
        layout.classList.contains('app-layout--drawer-open'));
      if (!railOpen) {
        await page.evaluate(() => document.getElementById('rail-reopen-btn')?.click());
        await waitFor(page, () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true,
          `${state}: desktop rail did not reopen for composer proof`);
      }
    } else {
      await waitFor(page, () => {
        const layout = document.querySelector('.app-layout');
        const filesOpen = document.querySelector('.file-explorer') !== null;
        return !filesOpen || layout?.classList.contains('app-layout--files-overlay') === true;
      }, `${state}: mobile Files overlay class did not track innerWidth`);
      const sidebarOpen = await page.locator('#sidebar-toggle-btn').getAttribute('aria-pressed');
      if (sidebarOpen !== 'true') {
        await page.evaluate(() => document.getElementById('sidebar-toggle-btn')?.click());
      }
      await waitFor(page, () => document.getElementById('sidebar-toggle-btn')?.getAttribute('aria-pressed') === 'true',
        `${state}: mobile session surface did not open`);
    }
    await waitFor(page, () => {
      const send = document.getElementById('send-btn');
      return send !== null && send.getBoundingClientRect().width > 0;
    }, `${state}: Send was not visible at ${viewport.name}`);
    const proof = await page.evaluate(({ reply, sendDisabled, toastCount }) => {
      const rectangle = (element) => {
        if (!element) return null;
        const box = element.getBoundingClientRect();
        return { x: box.x, y: box.y, width: box.width, height: box.height };
      };
      const assistant = [...document.querySelectorAll('#transcript .msg--assistant')]
        .find((entry) => (entry.textContent || '').includes(reply));
      const activeSession = document.querySelector('.session-sidebar__row--active');
      const composerInput = document.getElementById('prompt-input');
      const send = document.getElementById('send-btn');
      const toast = document.querySelector('#toasts .toast[data-toast-kind="transport"]');
      const sidebar = document.querySelector('.session-sidebar');
      const overlay = document.getElementById('reconnect-overlay');
      return {
        transportToasts: document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length,
        overlay: overlay !== null,
        activeSessionId: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
        retainedReplyVisible: assistant !== undefined,
        sendDisabled: send?.hasAttribute('disabled') === true,
        sendOpacity: send ? getComputedStyle(send).opacity : '',
        transportToasts: document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length,
        expected: { sendDisabled, toastCount },
        activeSessionBox: rectangle(activeSession),
        inputBox: rectangle(composerInput),
        sendBox: rectangle(send),
        replyBox: rectangle(assistant),
        toastBox: rectangle(toast),
        sidebarBox: rectangle(sidebar),
        viewport: { width: innerWidth, height: innerHeight },
      };
    }, { reply: options.reply, sendDisabled: options.sendDisabled, toastCount: options.toastCount });
    const isVisible = (box) => box
      && box.width > 0
      && box.height > 0
      && box.x >= 0
      && box.y >= 0
      && box.x + box.width <= proof.viewport.width
      && box.y + box.height <= proof.viewport.height;
    if (
      !proof.retainedReplyVisible
      || proof.sendDisabled !== options.sendDisabled
      || proof.transportToasts !== options.toastCount
      || !isVisible(proof.activeSessionBox)
      || !isVisible(proof.replyBox)
      || !isVisible(proof.inputBox)
      || !isVisible(proof.sendBox)
    ) {
      fail(`${state}: incomplete ${viewport.name} visual proof: ${JSON.stringify(proof)}`);
    }
    if (options.sendDisabled && Number(proof.sendOpacity) > 0.75) {
      fail(`${state}: disabled Send was not visibly muted at ${viewport.name}: ${JSON.stringify(proof)}`);
    }
    if (
      !proof.overlay && (
        rectanglesOverlap(proof.toastBox, proof.sendBox)
        || rectanglesOverlap(proof.toastBox, proof.inputBox)
        || rectanglesOverlap(proof.toastBox, proof.replyBox)
        || rectanglesOverlap(proof.toastBox, proof.activeSessionBox)
      )
    ) {
      fail(`${state}: toast obscured composer or retained content at ${viewport.name}: ${JSON.stringify(proof)}`);
    }
    const file = `${evidence}/reconnect-${state}-${viewport.name}.png`;
    await page.screenshot({ path: file });
    captures.push({ viewport: viewport.name, file, proof });
  }
  await page.setViewportSize({ width: 1280, height: 720 });
  await waitFor(page, () => document.querySelector('.app-layout')?.classList.contains('app-layout--files-overlay') !== true,
    `${state}: desktop Files overlay class stayed on after 1280 restore`);
  // Desktop-to-mobile collapse leaves sidebarOpen=false; returning to 1280
  // keeps the 44px rail, which hides #session-toggle-btn. Reopen the rail
  // so Files can stay mounted while Session is clicked.
  const railOpen = await page.locator('.app-layout').evaluate((layout) =>
    layout.classList.contains('app-layout--drawer-open'));
  if (!railOpen) {
    await page.evaluate(() => document.getElementById('rail-reopen-btn')?.click());
    await waitFor(page, () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true,
      `${state}: desktop rail did not reopen after composer proof`);
  }
  if (panelWasOpen) {
    await page.evaluate(() => document.getElementById('session-toggle-btn')?.click());
    await waitFor(page, () => document.getElementById('session-panel') !== null, `${state}: panel did not reopen after composer proof`);
  }
  return captures;
}

async function switchSessionWithCorrelation(page, socketFrames, targetSessionId, preRestartGeneration, label) {
  const sourceSessionId = await activeSessionId(page);
  if (!sourceSessionId || sourceSessionId === targetSessionId) {
    fail(`${label}: requires a real source-to-target switch (${sourceSessionId} -> ${targetSessionId})`);
  }
  const sentStart = socketFrames.sent.length;
  const receivedStart = socketFrames.received.length;
  await switchSession(page, targetSessionId, label);
  const commands = parsedFrames(socketFrames.sent.slice(sentStart))
    .filter((frame) => frame.type === 'switch_session');
  if (commands.length === 0) {
    const appliedSessionId = await activeSessionId(page);
    if (appliedSessionId !== targetSessionId) {
      fail(`${label}: exact-ready switch did not apply ${targetSessionId} (got ${appliedSessionId})`);
    }
    return { commandId: null, exactReady: true, sourceSessionId, targetSessionId };
  }
  if (commands.length !== 1) fail(`${label}: expected one switch_session command, got ${JSON.stringify(commands)}`);
  const command = commands[0];
  if (typeof command.id !== 'string' || command.id === '') fail(`${label}: switch command id missing`);
  if (command.gen <= preRestartGeneration) {
    fail(`${label}: switch command used pre-restart socket generation ${command.gen} <= ${preRestartGeneration}`);
  }
  if (command.sessionId !== sourceSessionId) {
    fail(`${label}: command owner mismatch (expected ${sourceSessionId}, got ${command.sessionId})`);
  }
  const responses = parsedFrames(socketFrames.received.slice(receivedStart))
    .filter((frame) => frame.type === 'response' && frame.id === command.id);
  if (responses.length !== 1) fail(`${label}: expected one matching response, got ${JSON.stringify(responses)}`);
  const response = responses[0];
  if (response.gen !== command.gen || response.success !== true) {
    fail(`${label}: response generation/outcome mismatch: ${JSON.stringify({ command, response })}`);
  }
  if (response.sessionId !== sourceSessionId || response.data?.sessionId !== targetSessionId) {
    fail(`${label}: response owner/target mismatch: ${JSON.stringify({ command, response })}`);
  }
  const appliedSessionId = await activeSessionId(page);
  if (appliedSessionId !== targetSessionId) {
    fail(`${label}: applied session mismatch (expected ${targetSessionId}, got ${appliedSessionId})`);
  }
  return {
    commandId: command.id,
    exactReady: false,
    sourceSessionId,
    targetSessionId,
  };
}

async function switchSession(page, sessionId, label, socketFrames = null) {
  const active = await activeSessionId(page);
  if (active === sessionId) return;
  try {
    await page.waitForFunction(
      (sid) => {
        const row = [...document.querySelectorAll('.session-sidebar__switch')]
          .find((candidate) => candidate.dataset.sessionId === sid);
        return row !== undefined && !row.disabled;
      },
      sessionId,
      { timeout: 30000 },
    );
  } catch {
    const diagnostic = await page.evaluate(() => ({
      transportToasts: document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length,
      activeSessionId: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
      rows: [...document.querySelectorAll('.session-sidebar__switch')].map((row) => ({
        sessionId: row.dataset.sessionId || '',
        disabled: row.disabled,
        switching: row.closest('.session-sidebar__row')?.dataset.switching || '',
        text: row.textContent || '',
      })),
      sidebarError: document.querySelector('.session-sidebar__error')?.textContent || '',
      toasts: [...document.querySelectorAll('#toasts .toast')].map((toast) => toast.textContent || ''),
    }));
    const frameTail = socketFrames ? {
      sent: parsedFrames(socketFrames.sent.slice(-20)),
      received: parsedFrames(socketFrames.received.slice(-30)),
    } : null;
    fail(`${label}: session row never became clickable: ${JSON.stringify({ diagnostic, frameTail })}`);
  }
  const sentStart = socketFrames?.sent.length ?? 0;
  const receivedStart = socketFrames?.received.length ?? 0;
  await page.evaluate((sid) => {
    [...document.querySelectorAll('.session-sidebar__switch')]
      .find((candidate) => candidate.dataset.sessionId === sid)?.click();
  }, sessionId);
  try {
    await page.waitForFunction(
      (sid) => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId === sid,
      sessionId,
      { timeout: 30000 },
    );
  } catch {
    const diagnostic = await page.evaluate(() => ({
      transportToasts: document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length,
      activeSessionId: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
      rows: [...document.querySelectorAll('.session-sidebar__switch')].map((row) => ({
        sessionId: row.dataset.sessionId || '',
        disabled: row.disabled,
        switching: row.closest('.session-sidebar__row')?.dataset.switching || '',
        text: row.textContent || '',
      })),
      sidebarError: document.querySelector('.session-sidebar__error')?.textContent || '',
      toasts: [...document.querySelectorAll('#toasts .toast')].map((toast) => toast.textContent || ''),
    }));
    const frames = socketFrames ? {
      sent: parsedFrames(socketFrames.sent.slice(sentStart)),
      received: parsedFrames(socketFrames.received.slice(receivedStart)),
    } : null;
    fail(`${label}: session never became active: ${JSON.stringify({ diagnostic, frames })}`);
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
    // Transparent observer: preserve native behavior while recording the
    // real socket close code. `failNextAttempts` makes the product's next N
    // connect() generations fail synchronously before Chromium opens a TCP
    // socket. The product still owns the only connect/scheduleReconnect/
    // ReadyGate loop; this seam avoids manufacturing native
    // ERR_CONNECTION_REFUSED console errors while the listener is already
    // back on the same port.
    await page.addInitScript(() => {
      const NativeWebSocket = window.WebSocket;
      const state = {
        attempts: 0,
        nativeAttempts: 0,
        failedAttempts: [],
        closeCodes: [],
        isBlocked: false,
        failuresRemaining: 0,
        sessionListFailuresRemaining: 0,
        injectedSessionListFailures: 0,
      };
      let current = null;
      let isBlocked = false;
      let failuresRemaining = 0;
      let sessionListFailuresRemaining = 0;
      let holdFileReads = false;
      const hijackedListIds = new Set();
      const heldFileReadIds = new Set();
      class ObservedWebSocket extends NativeWebSocket {
        constructor(...args) {
          state.attempts += 1;
          state.isBlocked = isBlocked;
          state.failuresRemaining = failuresRemaining;
          if (isBlocked || failuresRemaining > 0) {
            if (failuresRemaining > 0) failuresRemaining -= 1;
            state.failuresRemaining = failuresRemaining;
            state.failedAttempts.push(state.attempts);
            throw new Error('web-reconnect test outage');
          }
          super(...args);
          current = this;
          state.nativeAttempts += 1;
          this.addEventListener('close', (event) => state.closeCodes.push(event.code));
          // Transparent response interceptor: registered BEFORE the app's
          // onmessage, it rewrites the next N session_list responses into a
          // realistic non-transport business failure (the real RPC-limit
          // rejection shape) so the sidebar's silent background-failure
          // contract is exercised in a real browser without touching the
          // listener. The real response is swallowed for the hijacked id.
          this.addEventListener('message', (event) => {
            if (event.__rpiInjected) return;
            let frame;
            try {
              frame = JSON.parse(event.data);
            } catch {
              return;
            }
            if (!frame || frame.type !== 'response' || typeof frame.id !== 'string') return;
            if (heldFileReadIds.has(frame.id)) {
              heldFileReadIds.delete(frame.id);
              event.stopImmediatePropagation();
              return;
            }
            if (!hijackedListIds.has(frame.id)) return;
            hijackedListIds.delete(frame.id);
            if (sessionListFailuresRemaining <= 0) return;
            sessionListFailuresRemaining -= 1;
            state.injectedSessionListFailures += 1;
            state.sessionListFailuresRemaining = sessionListFailuresRemaining;
            event.stopImmediatePropagation();
            const fake = new MessageEvent('message', {
              data: JSON.stringify({
                type: 'response',
                id: frame.id,
                success: false,
                error: 'command session_list failed: too many concurrent RPC commands (limit 16)',
              }),
            });
            fake.__rpiInjected = true;
            this.dispatchEvent(fake);
          });
        }
        send(data) {
          if (typeof data === 'string') {
            try {
              const frame = JSON.parse(data);
              if (holdFileReads && frame && frame.type === 'file_explorer_read' && typeof frame.id === 'string') {
                heldFileReadIds.add(frame.id);
              }
              if (sessionListFailuresRemaining > 0 && frame && frame.type === 'session_list' && typeof frame.id === 'string') {
                hijackedListIds.add(frame.id);
              }
            } catch {
              // not a product control-plane frame
            }
          }
          super.send(data);
        }
      }
      for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
        Object.defineProperty(ObservedWebSocket, key, { value: NativeWebSocket[key] });
      }
      window.WebSocket = ObservedWebSocket;
      window.__rpiReconnectObservation = state;
      window.__rpiReconnectTest = {
        disconnect() {
          isBlocked = true;
          state.isBlocked = true;
          current?.close(4000, 'test outage');
        },
        reconnect() {
          isBlocked = false;
          state.isBlocked = false;
        },
        failNextAttempts(count) {
          holdFileReads = false;
          failuresRemaining = count;
          state.failuresRemaining = count;
        },
        holdNextFileReads() {
          holdFileReads = true;
        },
        failNextSessionLists(count) {
          sessionListFailuresRemaining = count;
          state.sessionListFailuresRemaining = count;
        },
      };
    });
    const socketFrames = { nextGen: 0, nextOrder: 0, sent: [], received: [], timeline: [] };
    page.on('websocket', (socket) => {
      const gen = ++socketFrames.nextGen;
      const socketUrl = socket.url();
      socket.on('framesent', (frame) => {
        if (typeof frame.payload !== 'string') return;
        const entry = { order: ++socketFrames.nextOrder, gen, url: socketUrl, raw: frame.payload };
        socketFrames.sent.push(entry);
        socketFrames.timeline.push({ ...entry, direction: 'sent' });
      });
      socket.on('framereceived', (frame) => {
        if (typeof frame.payload !== 'string') return;
        const entry = { order: ++socketFrames.nextOrder, gen, url: socketUrl, raw: frame.payload };
        socketFrames.received.push(entry);
        socketFrames.timeline.push({ ...entry, direction: 'received' });
      });
    });
    page.on('pageerror', (err) => pageErrors.push(err.message));
    page.on('console', (message) => {
      if (message.type() === 'error') consoleErrors.push(message.text());
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    // Connected authority without the #conn-state element: the shim observes a
    // native WebSocket constructor, and the sidebar row binds only after the
    // authoritative bootstrap completes. The pill element must be GONE —
    // connection problems surface ONLY through the transport toast.
    await waitFor(
      page,
      () => (window.__rpiReconnectObservation?.nativeAttempts || 0) >= 1,
      'WS connect attempt never constructed'
    );
    await waitFor(page, () => document.getElementById('conn-state') === null, 'conn-state element still present (must be removed)');
    await page.screenshot({ path: `${evidence}/reconnect-header-no-pill.png`, fullPage: true });
    await waitFor(
      page,
      () => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId,
      'primary session row did not bind after WebSocket bootstrap',
      30000,
    );

    const sessionA = await activeSessionId(page);
    if (!sessionA) fail('primary session id missing before reconnect scenario');
    fs.writeFileSync(path.join(work, 'listener-primary-session-id'), `${sessionA}\n`);


    // Request 1 streams on A. Create B while A remains in flight so A becomes
    // a loaded background subscription, then disconnect the browser socket.
    await page.fill('#prompt-input', 'finish this turn while the browser socket is down');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (head) => document.body.textContent.includes(head),
      'background slow turn never started streaming',
      30000,
      backgroundHead
    );
    const created = await createFreshSessionViaManage(page, waitFor, 'create session B before reconnect');
    const sessionB = created.sessionId;
    if (!sessionB || sessionB === sessionA) fail('fresh session B identity missing');
    await page.fill('#prompt-input', 'complete on active session B before reconnect');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (reply) => document.body.textContent.includes(reply),
      'session B reply did not round-trip before reconnect',
      30000,
      fastReply
    );
    await page.evaluate(() => window.__rpiReconnectTest.disconnect());
    await waitFor(
      page,
      () => document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length === 1,
      'same-process outage never raised the transport outage toast'
    );
    await waitFor(
      page,
      () => (window.__rpiReconnectObservation?.closeCodes.length || 0) >= 1,
      'blocked reconnect never closed the observed socket',
    );
    const outageTransportToasts = await page.locator('#toasts .toast[data-toast-kind="transport"]').count();
    if (outageTransportToasts !== 1) fail(`outage transport toast count was not exactly one (${outageTransportToasts})`);
    const outageComposer = await page.evaluate(() => ({
      sendDisabled: document.getElementById('send-btn')?.hasAttribute('disabled') === true,
      sendOpacity: getComputedStyle(document.getElementById('send-btn')).opacity,
      sessionId: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
      transcript: document.getElementById('transcript')?.textContent || '',
    }));
    if (!outageComposer.sendDisabled || Number(outageComposer.sendOpacity) > 0.5) {
      fail(`send remained active during outage: ${JSON.stringify(outageComposer)}`);
    }
    if (outageComposer.sessionId !== sessionB || !outageComposer.transcript.includes(fastReply)) {
      fail(`outage changed active session or transcript: ${JSON.stringify(outageComposer)}`);
    }
    await page.screenshot({ path: `${evidence}/reconnect-outage.png`, fullPage: true });

    await page.evaluate(() => window.__rpiReconnectTest.reconnect());
    await waitFor(
      page,
      () => document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length === 0,
      'same-process outage never auto-reconnected (transport toast did not clear)',
      30000
    );
    await waitFor(
      page,
      () => document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length === 0,
      'transport toast did not clear after recovery',
    );
    const restoredSend = await page.evaluate(() => ({
      disabled: document.getElementById('send-btn')?.hasAttribute('disabled') === true,
      opacity: getComputedStyle(document.getElementById('send-btn')).opacity,
    }));
    if (restoredSend.disabled) fail(`send did not recover after reconnect: ${JSON.stringify(restoredSend)}`);
    const activeAfterReconnect = await activeSessionId(page);
    if (activeAfterReconnect !== sessionB) {
      fail(`reconnect changed the active session (expected B=${sessionB}, got ${activeAfterReconnect})`);
    }
    const bReplyCount = await page.locator('#transcript').getByText(fastReply, { exact: false }).count();
    if (bReplyCount !== 1) fail(`session B transcript duplicated after reconnect (count=${bReplyCount})`);
    if ((await page.locator('#transcript').textContent() || '').includes(backgroundReply)) {
      fail('background session A reply leaked into active session B after reconnect');
    }

    // A must settle and reach its recorder before the process restart. A
    // browser transport outage does not stop the resident runtime, but killing
    // the listener while the slow provider response is still in flight would
    // leave A without a durable catalog row and make post-restart A→B→A
    // correlation impossible. Exercise the authoritative background restore,
    // then return to B as the active restart owner.
    await switchSession(page, sessionA, 'restore background session A before process restart', socketFrames);
    await waitFor(
      page,
      (reply) => document.getElementById('transcript')?.textContent.includes(reply),
      'background session A did not settle before process restart',
      30000,
      backgroundReply,
    );
    const aReplyCount = await page.locator('#transcript').getByText(backgroundReply, { exact: false }).count();
    if (aReplyCount !== 1) fail(`session A transcript count before process restart was ${aReplyCount}`);
    await switchSession(page, sessionB, 'return to active session B before process restart', socketFrames);
    if (!(await page.locator('#transcript').textContent() || '').includes(fastReply)) {
      fail('session B transcript was not restored before process restart');
    }

    const taskCardBefore = await page.locator('.tool-card--task').count();
    await page.fill('#prompt-input', 'restart-held task spawn');
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, (count) => document.querySelectorAll('.tool-card--task').length > count,
      'restart Task card never rendered', 30000, taskCardBefore);
    await waitFor(page, () => {
      const child = document.querySelector('.tool-card--task .tool-card__child[data-job-id][data-agent-id][data-parent-id="Main"]');
      const status = child?.getAttribute('data-status');
      return status === 'queued' || status === 'running';
    }, 'restart Task child never reached queued/running', 30000);
    const taskBeforeKill = await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.tool-card--task')];
      const card = cards.find((candidate) => (candidate.textContent || '').includes('restart-held-subagent'));
      const child = card?.querySelector('.tool-card__child');
      return {
        toolCallId: card?.getAttribute('data-tool-call-id') || '',
        jobId: child?.getAttribute('data-job-id') || '',
        agentId: child?.getAttribute('data-agent-id') || '',
        parentId: child?.getAttribute('data-parent-id') || '',
        status: child?.getAttribute('data-status') || '',
      };
    });
    const taskBackendBeforeKill = await rpc(page, { type: 'job_list', sessionId: sessionB });
    const taskJobBeforeKill = (taskBackendBeforeKill.jobs || []).find((job) => job.id === taskBeforeKill.jobId);
    if (!taskBeforeKill.toolCallId || !taskBeforeKill.jobId || !taskBeforeKill.agentId
      || taskBeforeKill.parentId !== 'Main' || !['queued', 'running'].includes(taskBeforeKill.status)
      || taskBackendBeforeKill.groupId === '' || !taskJobBeforeKill
      || !['queued', 'running'].includes(taskJobBeforeKill.status)
      || taskJobBeforeKill.agentId !== taskBeforeKill.agentId || taskJobBeforeKill.parentId !== taskBeforeKill.parentId) {
      fail(`pre-restart Task identity mismatch: ${JSON.stringify({ taskBeforeKill, taskBackendBeforeKill })}`);
    }

    // Keep the Files drawer and one selected tab mounted across the REAL
    // listener kill. The first file_explorer_read must be in flight on the
    // killed socket; the recovered connection retries that exact identity
    // once without closing/reopening the tab.

    // Real listener kill while both A and B are durable, B is active, and the
    // Session panel remains visible. The restart must retain the active view
    // and permit frame-correlated A→B switching afterward.
    await page.evaluate(() => document.getElementById('session-toggle-btn')?.click());
    await waitFor(page, () => document.getElementById('session-panel') !== null, 'session panel did not open before real outage');
    await waitFor(page, () => {
      const dts = [...document.querySelectorAll('#session-panel dl dt')];
      const index = dts.findIndex((dt) => dt.textContent === 'Session id');
      const value = index >= 0 ? (dts[index]?.nextElementSibling?.textContent || '').trim() : '';
      return value !== '' && value !== '—';
    }, 'session panel content did not load');
    await page.click('#file-explorer-toggle-btn');
    await waitFor(page, () => document.querySelector('.file-explorer') !== null,
      'File Explorer did not open before real outage');
    const beforeKill = await page.evaluate(() => ({
      attempts: window.__rpiReconnectObservation?.attempts || 0,
      nativeAttempts: window.__rpiReconnectObservation?.nativeAttempts || 0,
      failedAttemptCount: window.__rpiReconnectObservation?.failedAttempts.length || 0,
      activeSessionId: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
      sidebarRows: document.querySelectorAll('.session-sidebar__row').length,
      panelText: document.getElementById('session-panel')?.textContent || '',
      transcript: document.getElementById('transcript')?.textContent || '',
      fileDrawerOpen: document.querySelector('.file-explorer') !== null,
      filePath: '000-reconnect-large.txt',
    }));
    const activeVisuals = await captureReconnectState(page, 'active', {
      reply: fastReply,
      sendDisabled: false,
      toastCount: 0,
    });
    const fileReadStart = socketFrames.sent.length;
    await waitFor(page, () => [...document.querySelectorAll('.file-explorer__row')]
      .some((candidate) => (candidate.querySelector('.file-explorer__row-path')?.textContent || '') === '000-reconnect-large.txt'
        && candidate.disabled !== true),
    'large reconnect fixture never appeared in File Explorer', 30000);
    await page.evaluate(() => window.__rpiReconnectTest.holdNextFileReads());
    await page.evaluate(() => {
      const row = [...document.querySelectorAll('.file-explorer__row')]
        .find((candidate) => (candidate.querySelector('.file-explorer__row-path')?.textContent || '') === '000-reconnect-large.txt');
      row?.click();
    });
    const fileReadDeadline = Date.now() + 30000;
    while (Date.now() < fileReadDeadline) {
      if (parsedFrames(socketFrames.sent.slice(fileReadStart))
        .some((frame) => frame.type === 'file_explorer_read' && frame.path === '000-reconnect-large.txt')) break;
      await page.waitForTimeout(20);
    }
    if (!parsedFrames(socketFrames.sent.slice(fileReadStart))
      .some((frame) => frame.type === 'file_explorer_read' && frame.path === '000-reconnect-large.txt')) {
      fail('file_explorer_read was not sent before real listener kill');
    }
    const preKillFileReads = parsedFrames(socketFrames.sent.slice(fileReadStart))
      .filter((frame) => frame.type === 'file_explorer_read' && frame.path === '000-reconnect-large.txt');
    if (preKillFileReads.length !== 1) {
      fail(`pre-kill File read count mismatch: ${JSON.stringify(preKillFileReads)}`);
    }
    await page.evaluate(() => window.__rpiReconnectTest.failNextAttempts(3));
    const preRestartGeneration = socketFrames.nextGen;
    fs.writeFileSync(KILL_MARKER, 'kill now\n');
    await waitFor(
      page,
      () => document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length === 1,
      'real kill did not raise the transport outage toast',
      30000,
    );
    await waitFor(
      page,
      () => window.__rpiReconnectObservation?.closeCodes.includes(1006) === true,
      'real listener kill did not produce close code 1006',
      30000,
    );
    // The shell may now restore the same port. The next three product-owned
    // reconnect generations are still forced to fail synchronously by the
    // shim, so Chromium never opens a refused TCP connection.
    fs.writeFileSync(OUTAGE_OBSERVED_MARKER, 'real close code 1006 observed\n');
    await waitForFile(UP_MARKER, 'lane never respawned the listener');
    const failedAttemptTransitions = [];
    for (let count = 1; count <= 3; count += 1) {
      await waitFor(
        page,
        ({ baseline, count }) => (window.__rpiReconnectObservation?.failedAttempts.length || 0) >= baseline + count,
        `real outage did not execute failed reconnect attempt ${count}`,
        30000,
        { baseline: beforeKill.failedAttemptCount, count },
      );
      failedAttemptTransitions.push(await page.evaluate((baseline) => ({
        failedAttempts: [...(window.__rpiReconnectObservation?.failedAttempts || [])].slice(baseline),
        transportToasts: document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length,
        sendDisabled: document.getElementById('send-btn')?.hasAttribute('disabled') === true,
        sidebarErrors: document.querySelectorAll('.session-sidebar__error').length,
        panelErrors: document.querySelectorAll('.panel__status--error').length,
      }), beforeKill.failedAttemptCount));
    }
    await page.evaluate(() => window.__rpiReconnectTest.failNextAttempts(50));
    const outage = await page.evaluate((baseline) => ({
      attempts: window.__rpiReconnectObservation?.attempts || 0,
      nativeAttempts: window.__rpiReconnectObservation?.nativeAttempts || 0,
      failedAttempts: [...(window.__rpiReconnectObservation?.failedAttempts || [])].slice(baseline.failedAttemptCount),
      closeCodes: [...(window.__rpiReconnectObservation?.closeCodes || [])],
      activeSessionId: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
      sidebarRows: document.querySelectorAll('.session-sidebar__row').length,
      sidebarErrors: document.querySelectorAll('.session-sidebar__error').length,
      sidebarText: document.getElementById('session-sidebar')?.textContent || '',
      bodySansToasts: (() => {
        const clone = document.body.cloneNode(true);
        clone.querySelector('#toasts')?.remove();
        return clone.textContent || '';
      })(),
      panelErrors: document.querySelectorAll('.panel__status--error').length,
      panelText: document.getElementById('session-panel')?.textContent || '',
      transcript: document.getElementById('transcript')?.textContent || '',
      transportToasts: document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length,
      baseline,
    }), beforeKill);
    const forbiddenSidebar = ['load failed', 'session_list failed', 'too many concurrent RPC commands', 'workflow_detail'];
    const outageLeak = forbiddenSidebar.find((needle) => outage.bodySansToasts.includes(needle) || outage.sidebarText.includes(needle));
    if (outageLeak) fail(`real outage leaked sidebar error string "${outageLeak}": ${JSON.stringify(outage)}`);
    if (!outage.closeCodes.includes(1006) || outage.failedAttempts.length < 3 || outage.attempts < beforeKill.attempts + 3) {
      fail(`real outage provenance incomplete: ${JSON.stringify({ outage, failedAttemptTransitions })}`);
    }
    if (failedAttemptTransitions.some((entry) => entry.sendDisabled !== true)) {
      fail(`failed generations escaped the disconnected Send state: ${JSON.stringify(failedAttemptTransitions)}`);
    }
    if (outage.transportToasts !== 1) fail(`real outage transport toast count was not one: ${JSON.stringify(outage)}`);
    if (outage.sidebarErrors !== 0 || outage.panelErrors !== 0) fail(`real outage leaked sidebar/panel error: ${JSON.stringify(outage)}`);
    if (outage.activeSessionId !== beforeKill.activeSessionId || outage.sidebarRows !== beforeKill.sidebarRows) fail(`real outage changed retained session list: ${JSON.stringify(outage)}`);
    if (outage.panelText !== beforeKill.panelText || outage.transcript !== beforeKill.transcript) fail(`real outage changed retained panel/transcript: ${JSON.stringify(outage)}`);
    const outageVisuals = await captureReconnectState(page, 'outage', {
      reply: fastReply,
      sendDisabled: true,
      toastCount: 1,
    });
    await page.evaluate(() => window.__rpiReconnectTest.failNextAttempts(0));
    try {
      await waitForError(page, () => document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length === 0, 'client never auto-reconnected after real listener recovery', 30000);
    } catch (error) {
      const recoveryFailure = await page.evaluate(() => ({
        observation: window.__rpiReconnectObservation,
        sendDisabled: document.getElementById('send-btn')?.hasAttribute('disabled') === true,
        activeSessionId: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
        transportToasts: [...document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]')].map((entry) => entry.textContent || ''),
        sidebarErrors: [...document.querySelectorAll('.session-sidebar__error')].map((entry) => entry.textContent || ''),
        panelErrors: [...document.querySelectorAll('.panel__status--error')].map((entry) => entry.textContent || ''),
      }));
      const diagnostic = {
        error: error.message,
        recoveryFailure,
        pageErrors,
        consoleErrors,
        socketGeneration: socketFrames.nextGen,
        sentFrames: socketFrames.sent.slice(-20),
        receivedFrames: socketFrames.received.slice(-20),
      };
      fs.writeFileSync(path.join(evidence, 'reconnect-recovery-failure.json'), JSON.stringify(diagnostic, null, 2));
      await page.screenshot({ path: `${evidence}/reconnect-recovery-failure.png`, fullPage: true });
      fail(`${error.message}: ${JSON.stringify(diagnostic)}`);
    }
    await waitFor(page, () => document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length === 0, 'transport toast did not clear after real recovery');
    const recovered = await page.evaluate(() => ({
      activeSessionId: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
      sidebarRows: document.querySelectorAll('.session-sidebar__row').length,
      sidebarErrors: document.querySelectorAll('.session-sidebar__error').length,
      panelErrors: document.querySelectorAll('.panel__status--error').length,
      panelText: document.getElementById('session-panel')?.textContent || '',
      transcript: document.getElementById('transcript')?.textContent || '',
      transportToasts: document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length,
    }));
    if (recovered.transportToasts !== 0 || recovered.sidebarErrors !== 0 || recovered.panelErrors !== 0) fail(`recovery retained transport error surfaces: ${JSON.stringify(recovered)}`);
    if (beforeKill.activeSessionId !== sessionB || recovered.activeSessionId !== sessionB) fail(`recovery changed retained session identity: ${JSON.stringify({ sessionB, beforeKill, recovered })}`);
    if (recovered.sidebarRows < 1 || recovered.sidebarRows > beforeKill.sidebarRows) fail(`recovery restored an invalid authoritative session catalog: ${JSON.stringify({ beforeKill, recovered })}`);
    if (!recovered.transcript.includes(fastReply)) fail(`recovery lost transcript: ${JSON.stringify(recovered)}`);
    await waitFor(page, () => {
      const content = document.querySelector('.file-explorer__content-viewport')?.textContent || '';
      return document.querySelector('.file-explorer') !== null
        && content.includes('reconnect-file-line');
    }, 'in-flight File read did not self-recover in the same mounted tab', 30000);
    const fileReads = parsedFrames(socketFrames.sent.slice(fileReadStart))
      .filter((frame) => frame.type === 'file_explorer_read' && frame.path === '000-reconnect-large.txt');
    if (fileReads.length !== 2 || fileReads[0].gen >= fileReads[1].gen) {
      fail(`File read retry must occur exactly once on the recovered generation: ${JSON.stringify(fileReads)}`);
    }
    const fileRecovery = await page.evaluate(() => ({
      drawerOpen: document.querySelector('.file-explorer') !== null,
      activeTab: document.querySelector('.file-explorer__row--active')?.getAttribute('title') || '',
      content: document.querySelector('.file-explorer__content-viewport')?.textContent || '',
      retryVisible: document.querySelector('.file-explorer__retry') !== null,
    }));
    if (!fileRecovery.drawerOpen || !fileRecovery.activeTab.includes('000-reconnect-large.txt')
      || !fileRecovery.content.includes('reconnect-file-line') || fileRecovery.retryVisible) {
      fail(`File recovery surface mismatch: ${JSON.stringify(fileRecovery)}`);
    }
    await page.screenshot({ path: `${evidence}/reconnect-file-recovered.png`, fullPage: true });
    await page.click('#session-toggle-btn');
    await waitFor(page, (sessionId) => {
      const panel = document.getElementById('session-panel');
      return panel !== null
        && (panel.textContent || '').includes(sessionId)
        && !(panel.textContent || '').includes('No saved sessions found.');
    }, 'recovered Session panel never restored authoritative session content', 30000, sessionB);
    // ---- Injected non-transport session_list failure (real RPC-limit
    // rejection shape, rewritten in the page's WS shim): the sidebar retains
    // its catalog with ZERO .session-sidebar__error nodes and none of the
    // sidebar error strings anywhere while the background refresh fails,
    // across a window that crosses at least one 8s poll. ----
    const preInject = await page.evaluate(() => ({
      rows: document.querySelectorAll('.session-sidebar__row').length,
    }));
    await page.evaluate(() => window.__rpiReconnectTest.failNextSessionLists(2));
    const injectDeadline = Date.now() + 25000;
    let injectedSamples = 0;
    let injectedCount = 0;
    for (;;) {
      const sample = await page.evaluate(() => ({
        injected: window.__rpiReconnectObservation?.injectedSessionListFailures || 0,
        sidebarErrorCount: document.querySelectorAll('.session-sidebar__error').length,
        sidebarText: document.getElementById('session-sidebar')?.textContent || '',
        bodySansToasts: (() => {
          const clone = document.body.cloneNode(true);
          clone.querySelector('#toasts')?.remove();
          return clone.textContent || '';
        })(),
        rows: document.querySelectorAll('.session-sidebar__row').length,
      }));
      injectedSamples += 1;
      injectedCount = sample.injected;
      if (sample.sidebarErrorCount !== 0) {
        fail(`injected session_list failure rendered .session-sidebar__error (sample ${injectedSamples}): ${JSON.stringify(sample)}`);
      }
      const leak = forbiddenSidebar.find((needle) => sample.bodySansToasts.includes(needle) || sample.sidebarText.includes(needle));
      if (leak) fail(`injected session_list failure leaked sidebar error string "${leak}" (sample ${injectedSamples})`);
      if (sample.rows !== preInject.rows) {
        fail(`injected session_list failure changed retained rows (${preInject.rows} -> ${sample.rows}, sample ${injectedSamples})`);
      }
      if (sample.injected >= 2) break;
      if (Date.now() >= injectDeadline) fail(`injected session_list failures did not land twice before the deadline (${sample.injected})`);
      await page.waitForTimeout(400);
    }
    await page.screenshot({ path: `${evidence}/reconnect-sessionlist-rpclimit-desktop.png`, fullPage: false });
    await page.setViewportSize({ width: 375, height: 667 });
    await page.waitForTimeout(400);
    await page.screenshot({ path: `${evidence}/reconnect-sessionlist-rpclimit-mobile.png`, fullPage: false });
    await page.setViewportSize({ width: 1280, height: 720 });
    await waitFor(page, (expected) => {
      const card = [...document.querySelectorAll('.tool-card--task')]
        .find((candidate) => candidate.getAttribute('data-tool-call-id') === expected.toolCallId);
      const child = card?.querySelector('.tool-card__child');
      return child?.getAttribute('data-job-id') === expected.jobId
        && child?.getAttribute('data-agent-id') === expected.agentId
        && child?.getAttribute('data-parent-id') === expected.parentId
        && child?.getAttribute('data-status') === 'completed'
        && (child.textContent || '').includes('restart-held-subagent identity survived exactly-once');
    }, 'restarted Task did not revive and settle with exact identity', 60000, taskBeforeKill);
    const taskBackendAfterRecovery = await rpc(page, { type: 'job_list', sessionId: sessionB });
    const taskJobAfterRecovery = (taskBackendAfterRecovery.jobs || []).find((job) => job.id === taskBeforeKill.jobId);
    const taskAfterRecovery = await page.evaluate((expected) => {
      const matches = [...document.querySelectorAll('.tool-card--task')]
        .filter((candidate) => candidate.getAttribute('data-tool-call-id') === expected.toolCallId);
      const child = matches[0]?.querySelector('.tool-card__child');
      return {
        cardCount: matches.length,
        jobId: child?.getAttribute('data-job-id') || '',
        agentId: child?.getAttribute('data-agent-id') || '',
        parentId: child?.getAttribute('data-parent-id') || '',
        status: child?.getAttribute('data-status') || '',
        shutdown: matches.some((card) => (card.textContent || '').includes('orchestration shut down')),
        outputCount: matches.filter((card) => (card.textContent || '').includes('restart-held-subagent identity survived exactly-once')).length,
      };
    }, taskBeforeKill);
    if (taskBackendAfterRecovery.groupId !== taskBackendBeforeKill.groupId || !taskJobAfterRecovery
      || taskJobAfterRecovery.id !== taskBeforeKill.jobId || taskJobAfterRecovery.agentId !== taskBeforeKill.agentId
      || taskJobAfterRecovery.parentId !== taskBeforeKill.parentId || taskJobAfterRecovery.status !== 'completed'
      || taskAfterRecovery.cardCount !== 1 || taskAfterRecovery.jobId !== taskBeforeKill.jobId
      || taskAfterRecovery.agentId !== taskBeforeKill.agentId || taskAfterRecovery.parentId !== taskBeforeKill.parentId
      || taskAfterRecovery.status !== 'completed' || taskAfterRecovery.shutdown || taskAfterRecovery.outputCount !== 1) {
      fail(`recovered Task authority mismatch: ${JSON.stringify({ taskBeforeKill, taskBackendBeforeKill, taskBackendAfterRecovery, taskAfterRecovery })}`);
    }
    fs.writeFileSync(path.join(evidence, 'restart-task-identity.json'), JSON.stringify({
      sessionId: sessionB,
      groupId: taskBackendAfterRecovery.groupId,
      toolCallId: taskBeforeKill.toolCallId,
      jobId: taskBeforeKill.jobId,
      agentId: taskBeforeKill.agentId,
      parentId: taskBeforeKill.parentId,
      status: taskAfterRecovery.status,
      outputCount: taskAfterRecovery.outputCount,
    }, null, 2));
    await page.screenshot({ path: `${evidence}/reconnect-task-revived.png`, fullPage: true });
    const recoveredGeneration = socketFrames.nextGen;
    const recoveredTimeline = parsedFrames(
      socketFrames.timeline.filter((entry) => entry.gen === recoveredGeneration),
    );
    const recoveredSentTimeline = recoveredTimeline.filter((frame) => frame.direction === 'sent' && frame.type !== 'ping');
    const bootstrapRequest = recoveredSentTimeline[0];
    const bootstrapResponse = recoveredTimeline.find(
      (frame) => frame.direction === 'received'
        && frame.type === 'response'
        && frame.id === bootstrapRequest?.id
        && frame.success === true,
    );
    const sentBeforeBootstrapResponse = recoveredSentTimeline.filter(
      (frame) => frame.order < (bootstrapResponse?.order ?? Number.MAX_SAFE_INTEGER),
    );
    const catalogSwitch = recoveredTimeline.find(
      (frame) => frame.direction === 'sent'
        && frame.type === 'switch_session'
        && frame.sessionId === bootstrapResponse?.sessionId,
    );
    const catalogSwitchResponse = recoveredTimeline.find(
      (frame) => frame.direction === 'received'
        && frame.type === 'response'
        && frame.id === catalogSwitch?.id
        && frame.success === true
        && frame.data?.sessionId === sessionB,
    );
    const restoredBSubscription = recoveredTimeline.find(
      (frame) => frame.direction === 'sent'
        && frame.type === 'subscribe'
        && frame.sessionId === sessionB
        && frame.order > (catalogSwitchResponse?.order ?? Number.MAX_SAFE_INTEGER),
    );
    if (
      !bootstrapRequest
      || bootstrapRequest.type !== 'get_state'
      || bootstrapRequest.sessionId !== undefined
      || !bootstrapResponse
      || sentBeforeBootstrapResponse.length !== 1
      || sentBeforeBootstrapResponse[0]?.order !== bootstrapRequest.order
      || !catalogSwitch
      || !catalogSwitchResponse
      || !restoredBSubscription
      || !(bootstrapRequest.order < bootstrapResponse.order
        && bootstrapResponse.order < catalogSwitch.order
        && catalogSwitch.order < catalogSwitchResponse.order
        && catalogSwitchResponse.order < restoredBSubscription.order)
    ) {
      fail(`post-restart bootstrap frame order mismatch: ${JSON.stringify(recoveredTimeline)}`);
    }
    const bootstrapFrameOrder = {
      socketGeneration: recoveredGeneration,
      bootstrapRequest,
      bootstrapResponse,
      catalogSwitch,
      catalogSwitchResponse,
      restoredBSubscription,
      orderedSentTimeline: recoveredSentTimeline,
    };
    const recoveredVisuals = await captureReconnectState(page, 'recovered', {
      reply: fastReply,
      sendDisabled: false,
      toastCount: 0,
    });
    await page.evaluate(() => document.getElementById('session-close-btn')?.click());
    await waitFor(page, () => document.getElementById('session-panel') === null, 'session panel did not close before post-reconnect switch');
    const switchToA = await switchSessionWithCorrelation(
      page, socketFrames, sessionA, preRestartGeneration, 'post-restart switch B to A',
    );
    if (!(await page.locator('#transcript').textContent() || '').includes('finish this turn while the browser socket is down')) {
      fail('post-restart switch to A did not apply A transcript');
    }
    await page.screenshot({ path: `${evidence}/reconnect-post-switch-a.png`, fullPage: true });
    const switchToB = await switchSessionWithCorrelation(
      page, socketFrames, sessionB, preRestartGeneration, 'post-restart switch A to B',
    );
    if (!(await page.locator('#transcript').textContent() || '').includes(fastReply)) {
      fail('post-restart switch to B did not apply B transcript');
    }
    await page.screenshot({ path: `${evidence}/reconnect-post-switch-b.png`, fullPage: true });
    const promptSentStart = socketFrames.sent.length;
    const promptReceivedStart = socketFrames.received.length;
    await page.fill('#prompt-input', 'reconnect post restart');
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, () => (document.getElementById('prompt-input')?.value || '') === '', 'post-reconnect prompt was not accepted');
    try {
      await waitForError(page, (reply) => document.body.textContent.includes(reply), 'post-reconnect prompt did not round-trip', 30000, postRestartTail);
    } catch (error) {
      const diagnostic = await page.evaluate(() => ({
        sendDisabled: document.getElementById('send-btn')?.hasAttribute('disabled') === true,
        composerValue: document.getElementById('prompt-input')?.value || '',
        transcript: document.getElementById('transcript')?.textContent || '',
        toasts: [...document.querySelectorAll('#toasts .toast')].map((toast) => toast.textContent || ''),
      }));
      fail(`${error.message}: ${JSON.stringify(diagnostic)}`);
    }
    const postRestartCount = await page.evaluate((reply) => {
      const text = document.getElementById('transcript')?.textContent || '';
      if (!reply) return 0;
      let count = 0;
      let from = 0;
      while (from <= text.length) {
        const found = text.indexOf(reply, from);
        if (found < 0) break;
        count += 1;
        from = found + reply.length;
      }
      return count;
    }, postRestartTail);
    if (postRestartCount !== 1) {
      const transcript = await page.evaluate(() => document.getElementById('transcript')?.textContent || '');
      fail(`post-reconnect assistant reply count was ${postRestartCount}, expected 1: ${JSON.stringify({ postRestartTail, transcript })}`);
    }
    const promptCommands = parsedFrames(socketFrames.sent.slice(promptSentStart))
      .filter((frame) => frame.type === 'prompt');
    if (promptCommands.length !== 1) fail(`post-reconnect prompt command count mismatch: ${JSON.stringify(promptCommands)}`);
    const promptCommand = promptCommands[0];
    const promptResponses = parsedFrames(socketFrames.received.slice(promptReceivedStart))
      .filter((frame) => frame.type === 'response' && frame.id === promptCommand.id);
    if (
      typeof promptCommand.id !== 'string' || promptCommand.id === '' ||
      promptCommand.gen <= preRestartGeneration || promptCommand.sessionId !== sessionB ||
      promptResponses.length !== 1 || promptResponses[0].gen !== promptCommand.gen ||
      promptResponses[0].success !== true || promptResponses[0].sessionId !== sessionB ||
      await activeSessionId(page) !== sessionB
    ) {
      fail(`post-reconnect prompt ownership mismatch: ${JSON.stringify({ promptCommand, promptResponses, active: await activeSessionId(page) })}`);
    }
    // Active-turn outage authority (regression): while a reply is STREAMING,
    // the Send button shows Stop (enabled) even when the transport is down —
    // the connected authority is the transport itself (socket closed + the
    // single transport outage toast), never Send/Stop mode. The mock's
    // "mid-stream gate" marker streams its first delta immediately and holds
    // the terminal delta behind POST /__release-midstream, so the drop
    // provably happens mid-turn. Recovery must clear the toast, re-open the
    // socket, and settle the in-flight turn exactly once.
    if (!mockPort) fail('RPI_MOCK_PORT is required for the mid-stream gate fixture');
    await waitFor(page, () => document.getElementById('send-btn')?.getAttribute('aria-label') !== 'Stop generating',
      'post-restart turn did not settle before the next prompt', 30000);
    await page.fill('#prompt-input', 'mid-stream gate');
    await page.press('#prompt-input', 'Enter');
    const midStreamPrompt = parsedFrames(socketFrames.sent).filter((frame) => frame.type === 'prompt' && frame.message === 'mid-stream gate');
    if (midStreamPrompt.length !== 1) fail(`mid-stream prompt was not dispatched exactly once: ${JSON.stringify(parsedFrames(socketFrames.sent).slice(-4))}`);
    await waitFor(
      page,
      () => document.body.textContent.includes('mid-stream-head-'),
      'precondition: gated turn never streamed its first delta',
      30000,
    );
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Stop generating',
      'precondition: turn never entered the streaming Stop state',
      30000,
    );
    await page.evaluate(() => window.__rpiReconnectTest.disconnect());
    await waitFor(
      page,
      () => document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length === 1,
      'mid-stream drop never raised the transport outage toast',
      30000,
    );
    const midStreamState = await page.evaluate(() => ({
      transportToasts: document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length,
      stopEnabled: document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      stopLabel: document.getElementById('send-btn')?.getAttribute('aria-label') || '',
      headVisible: document.body.textContent.includes('mid-stream-head-'),
    }));
    if (midStreamState.transportToasts !== 1) fail(`mid-stream outage toast count wrong: ${JSON.stringify(midStreamState)}`);
    // Stop must REMAIN available during the outage — proving the connected
    // authority is the transport, not Send/Stop mode (a Send-disabled
    // assertion would false-pass here).
    if (!midStreamState.stopEnabled || midStreamState.stopLabel !== 'Stop generating') {
      fail(`Stop action disabled during mid-stream outage: ${JSON.stringify(midStreamState)}`);
    }
    if (!midStreamState.headVisible) fail(`gated turn lost its streamed head during the outage: ${JSON.stringify(midStreamState)}`);
    await page.evaluate(() => window.__rpiReconnectTest.reconnect());
    await waitFor(
      page,
      () => document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length === 0,
      'mid-stream outage never auto-reconnected (transport toast did not clear)',
      30000,
    );
    // Release the gated terminal delta only after the socket is back: the
    // in-flight turn must settle exactly once on the recovered connection.
    // The release POST goes to the mock from NODE (the mock's control
    // endpoint has no CORS headers, so a browser fetch would be blocked).
    const releaseResponse = await fetch(`http://127.0.0.1:${mockPort}/__release-midstream`, { method: 'POST' });
    if (!releaseResponse.ok) fail(`mid-stream release POST failed: ${releaseResponse.status}`);
    await waitFor(
      page,
      () => document.body.textContent.includes('mid-stream-done'),
      'gated turn never settled after the mid-stream reconnect',
      60000,
    );
    const midStreamDoneCount = await page.evaluate(() => {
      const text = document.getElementById('transcript')?.textContent || '';
      return text.split('mid-stream-done').length - 1;
    });
    if (midStreamDoneCount !== 1) fail(`mid-stream turn settled ${midStreamDoneCount} times (expected exactly once)`);
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message',
      'in-flight turn never returned to the Send state after the mid-stream reconnect',
      60000,
    );
    if (pageErrors.length > 0 || consoleErrors.length > 0) {
      fail(`browser errors observed: ${JSON.stringify({ pageErrors, consoleErrors })}`);
    }
    await page.screenshot({ path: `${evidence}/reconnect-settled-desktop.png` });
    fs.writeFileSync(path.join(evidence, 'reconnect-assertions.json'), JSON.stringify({
      pageErrors,
      consoleErrors,
      beforeKill,
      outage,
      failedAttemptTransitions,
      recovered,
      bootstrapFrameOrder,
      visuals: { active: activeVisuals, outage: outageVisuals, recovered: recoveredVisuals },
      sessionA,
      sessionB,
      switchToA,
      switchToB,
      prompt: {
        commandId: promptCommand.id,
        socketGeneration: promptCommand.gen,
        responseOwner: promptResponses[0].sessionId,
        appliedSessionId: sessionB,
      },
    }, null, 2));
    console.log('web-reconnect: MOBILE 390x844 active/outage/recovered: PASSED');
    console.log(`web-reconnect: RAW browser errors pageErrors=${JSON.stringify(pageErrors)} consoleErrors=${JSON.stringify(consoleErrors)}: PASSED`);
    console.log('web-reconnect: PASSED (real 1006 + three failed product reconnect generations + retained surfaces + same-port recovery)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-reconnect: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
