// Web compact-badge auto-compaction lane (playwright half of
// E2E.d/web/compact_badge.sh).
//
// Reported bug: after a REAL post-turn threshold compaction the header
// `#compact-badge` kept pulsing forever, even though the authoritative
// `get_state.isCompacting` was already false and the page had received no
// false state snapshot (the badge must clear from `compaction_end` itself,
// not from a later state poll).
//
// Fixture: the REAL `rpi --listen` binary + the repo `sessions` mock behind
// lib/compact_gate_mock.py, which holds ONLY the summarizer request. A real
// page prompt (~5.8k chars echoed back, so the mock reports >4000 usage
// tokens against a 4000-token window) drives the app's own post-turn
// threshold compaction, so the page's own 5s stream watchdog performs the
// automatic `get_state` that paints the badge — no signals, no UI mutation.
// Releasing the gate file delivers the real `compaction_end`.
//
// Assertions:
//   CB1  real `compaction_start` with reason=threshold on the page socket,
//        and the page's OWN watchdog state read reports isCompacting=true
//   CB2  badge visible during the compaction, painted with the theme's
//        --accent token (dark AND light), label "compacting", data-mode "llm"
//   CB3  prefers-reduced-motion:reduce removes the pulse animation
//   CB4  mobile 390x844 keeps the badge visible with zero horizontal overflow
//   CB5  real `compaction_end` (ordered after start, same session) with the
//        receipt timestamped on the PAGE clock
//   CB6  authoritative get_state.isCompacting === false after the page clear
//        (confirmed after, so the authority round-trip cannot extend the
//        badge's budget)
//   CB7  measured on ONE page clock and armed only while THIS round's badge
//        is visibly painted: the MutationObserver's actual hide time minus the
//        page's own compaction_end receipt must lie in [0, 3000]ms, the badge
//        must be unpainted at the check, and no false get_state snapshot
//        inside that window may precede the hide — the clear proves out from
//        the end event, never from a later authority poll
//   CB8  final state: not painted (hidden + display:none, or removed from the
//        header) in dark, light, and mobile 390x844 + screenshots
//
// Environment (same contract as the other web lanes):
//   RPI_URL           http://127.0.0.1:<port>/web
//   RPI_TOKEN         token file content (rpi-auth.<token> subprotocol)
//   RPI_CHROME        system Chrome executable (optional)
//   RPI_EVIDENCE      evidence dir (compact-badge-evidence.json + screenshots)
//   RPI_GATE_RELEASE  release file; writing it lets the held summary through
//
// Exit: 0 = every assertion held; 2 = assertion failure; 1 = setup failure.

import { chromium } from 'playwright';
import WebSocket from 'ws';
import fs from 'node:fs';
import path from 'node:path';

const url = process.env.RPI_URL || '';
const token = process.env.RPI_TOKEN || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const chromePath = process.env.RPI_CHROME || '';
const releaseFile = process.env.RPI_GATE_RELEASE || '';

const pageErrors = [];
const sentFrames = [];
const receivedFrames = [];
const checks = [];

let page = null;

function record(id) {
  checks.push(id);
  console.log(`[web-compact-badge:assert] ${id}`);
}

function fail(message) {
  throw new Error(message);
}

function stopSetup(message) {
  const error = new Error(message);
  error.setup = true;
  throw error;
}

function onPageSocket(ws) {
  if (!ws.url().includes('/ws')) return;
  ws.on('framesent', (frame) => {
    if (typeof frame.payload === 'string') sentFrames.push({ t: Date.now(), payload: frame.payload });
  });
  ws.on('framereceived', (frame) => {
    if (typeof frame.payload === 'string') receivedFrames.push({ t: Date.now(), payload: frame.payload });
  });
}

function parseFrames(list, since = 0, type = null) {
  const out = [];
  for (const item of list) {
    if (item.t < since) continue;
    let frame;
    try {
      frame = JSON.parse(item.payload);
    } catch {
      continue;
    }
    if (!frame || typeof frame !== 'object') continue;
    if (type && frame.type !== type) continue;
    out.push({ t: item.t, frame });
  }
  return out;
}

async function waitForFrame(type, since, label, { timeoutMs = 30000, pred = () => true } = {}) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    for (const entry of parseFrames(receivedFrames, since, type)) if (pred(entry.frame)) return entry;
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  fail(`${label}: no matching ${type} within ${timeoutMs}ms`);
}

async function waitFor(page, fn, label, timeoutMs = 30000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

/** Arm (or re-arm) the hide observation for the CURRENT compaction round.
 * Called only once the badge is visibly in flight, so `hiddenAt` can never
 * latch a pre-round state: the attribute observer watches the badge element
 * itself, and removal is matched by node identity on its parent. */
async function armBadgeObserver() {
  return page.evaluate(() => {
    const badge = document.getElementById('compact-badge');
    if (!badge || !badge.parentElement) return false;
    for (const observer of window.__compactBadgeObservers || []) observer.disconnect();
    const state = {
      transitions: [],
      hiddenAt: null,
      armedAt: performance.now(),
      armedWhileVisible: badge.hidden === false && getComputedStyle(badge).display !== 'none',
    };
    window.__compactBadge = state;
    const note = (kind) => {
      state.transitions.push({ t: performance.now(), kind });
      if (state.hiddenAt === null) state.hiddenAt = performance.now();
    };
    const attributeObserver = new MutationObserver(() => {
      if (badge.hidden && state.hiddenAt === null) note('attribute');
    });
    attributeObserver.observe(badge, { attributes: true, attributeFilter: ['hidden'] });
    const removalObserver = new MutationObserver((records) => {
      for (const record of records) {
        for (const node of record.removedNodes) {
          if (node === badge && state.hiddenAt === null) note('unmounted');
        }
      }
    });
    removalObserver.observe(badge.parentElement, { childList: true });
    window.__compactBadgeObservers = [attributeObserver, removalObserver];
    return state.armedWhileVisible;
  });
}

const twoFrames = () => page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));

/* Authoritative state client: a second socket so the page's own wire stays
 * untouched (the badge must never depend on it). */
function rpcClient() {
  const socket = new WebSocket(url.replace(/^http/, 'ws').replace(/\/web$/, '/ws'), token ? [`rpi-auth.${token}`] : []);
  const pending = new Map();
  let seq = 0;
  socket.on('message', (raw) => {
    let frame;
    try {
      frame = JSON.parse(String(raw));
    } catch {
      return;
    }
    if (frame && frame.type === 'response' && frame.id && pending.has(frame.id)) {
      const { resolve, reject, timer } = pending.get(frame.id);
      pending.delete(frame.id);
      clearTimeout(timer);
      if (frame.success) resolve(frame.data || {});
      else reject(new Error(frame.error || 'rpc failed'));
    }
  });
  const ready = new Promise((resolve, reject) => {
    socket.once('open', resolve);
    socket.once('error', reject);
  });
  return {
    ready,
    async call(command, timeout = 20000) {
      await ready;
      const id = `compact-badge-${++seq}`;
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          if (pending.delete(id)) reject(new Error(`rpc timeout: ${command.type}`));
        }, timeout);
        pending.set(id, { resolve, reject, timer });
        socket.send(JSON.stringify({ ...command, id }));
      });
    },
    close() {
      try {
        socket.close();
      } catch {
        /* already closed */
      }
    },
  };
}

const probe = () => page.evaluate(() => {
  const badge = document.getElementById('compact-badge');
  const root = document.documentElement;
  const badgeStyle = badge ? getComputedStyle(badge) : null;
  const rootStyle = getComputedStyle(root);
  return {
    theme: root.getAttribute('data-theme') || 'dark',
    present: Boolean(badge),
    hidden: badge ? badge.hidden : null,
    display: badgeStyle ? badgeStyle.display : null,
    animationName: badgeStyle ? badgeStyle.animationName : null,
    background: badgeStyle ? badgeStyle.backgroundColor : null,
    color: badgeStyle ? badgeStyle.color : null,
    label: badge ? (badge.textContent || '').trim() : null,
    dataMode: badge ? badge.getAttribute('data-mode') : null,
    accent: rootStyle.getPropertyValue('--accent').trim(),
    scrollWidth: root.scrollWidth,
    clientWidth: root.clientWidth,
  };
});

function hexToRgb(hex) {
  const match = /^#([0-9a-f]{6})$/i.exec(String(hex).trim());
  if (!match) return null;
  const value = parseInt(match[1], 16);
  return `rgb(${(value >> 16) & 255}, ${(value >> 8) & 255}, ${value & 255})`;
}

/** The user-visible contract: mounted, not `hidden`, not display:none.
 * Unmounting the badge is an equally valid clear, so the after-end checks
 * assert the paint, never the DOM shape. */
function badgeVisible(sample) {
  return sample.present === true && sample.hidden === false && sample.display !== 'none';
}

async function shot(name) {
  const file = path.join(evidence, `${name}.png`);
  await page.screenshot({ path: file });
  return file;
}

async function setTheme(theme) {
  await page.evaluate((value) => {
    if (value) document.documentElement.setAttribute('data-theme', value);
    else document.documentElement.removeAttribute('data-theme');
  }, theme);
  await twoFrames();
}

async function main() {
  if (!url) stopSetup('RPI_URL is required');
  if (!releaseFile) stopSetup('RPI_GATE_RELEASE is required');
  fs.mkdirSync(evidence, { recursive: true });
  fs.rmSync(releaseFile, { force: true });

  let browser = null;
  let rpc = null;
  const screenshots = [];
  try {
    browser = await chromium.launch(chromePath ? { executablePath: chromePath } : {});
    const context = await browser.newContext({ viewport: { width: 1280, height: 900 } });
    page = await context.newPage();
    page.on('websocket', onPageSocket);
    page.on('pageerror', (error) => pageErrors.push(String((error && error.message) || error)));
    if (token) {
      await page.addInitScript((value) => {
        try {
          window.localStorage.setItem('rpi-web-token', value);
        } catch {
          /* storage unavailable: the connect gate below fails */
        }
      }, token);
    }
    // Single monotonic clock for the CB7 claim: the PAGE's own socket frames
    // and the badge's DOM transition are both timestamped with the page's
    // performance.now(), so "hidden promptly after the end" is measured as the
    // real end-receipt -> hide delta, never a Node-side polling time.
    await page.addInitScript(() => {
      const wire = { received: [], sent: [], endAt: null };
      window.__compactWire = wire;
      const NativeWebSocket = window.WebSocket;
      class ObservedWebSocket extends NativeWebSocket {
        constructor(...args) {
          super(...args);
          const nativeSend = this.send.bind(this);
          this.send = (data) => {
            try {
              const frame = JSON.parse(String(data));
              if (frame && typeof frame === 'object') {
                wire.sent.push({ t: performance.now(), type: frame.type ?? null, command: frame.command ?? null });
              }
            } catch {
              /* non-JSON payloads are not part of the wire contract */
            }
            return nativeSend(data);
          };
          this.addEventListener('message', (event) => {
            if (typeof event.data !== 'string') return;
            let frame;
            try {
              frame = JSON.parse(event.data);
            } catch {
              return;
            }
            if (!frame || typeof frame !== 'object') return;
            const t = performance.now();
            wire.received.push({
              t,
              type: frame.type ?? null,
              command: frame.command ?? null,
              sessionId: frame.sessionId ?? null,
              sequence: frame.sequence ?? null,
              reason: frame.reason ?? null,
              isCompacting: frame.type === 'response' && frame.command === 'get_state'
                ? (frame.data && frame.data.isCompacting) ?? null
                : undefined,
            });
            if (frame.type === 'compaction_end' && wire.endAt === null) wire.endAt = t;
          });
        }
      }
      for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
        Object.defineProperty(ObservedWebSocket, key, { value: NativeWebSocket[key] });
      }
      window.WebSocket = ObservedWebSocket;
    });
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title', 20000);
    await waitFor(page, () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true, 'initial WS connected', 30000);

    rpc = rpcClient();
    await rpc.ready;
    const boot = await rpc.call({ type: 'get_state' });
    const sid = String(boot.sessionId || '');
    if (!sid) fail('boot get_state carried no sessionId');
    if (boot.isCompacting !== false) fail(`fresh session is already compacting (${boot.isCompacting})`);
    const idle = await rpc.call({ type: 'get_state', sessionId: sid });
    if (idle.isCompacting !== false) fail(`idle session already compacting (${idle.isCompacting})`);

    // Real prompt -> the app's own post-turn threshold compaction. The mock
    // echo reports ~5800 usage tokens against the 4000-token window, so
    // finish_run compacts and compact_gate_mock.py holds the summary request.
    const longPrompt = `${'x'.repeat(5800)}-AUTO-COMPACT-TAIL`;
    const promptAt = Date.now();
    await page.fill('#prompt-input', longPrompt);
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, (tail) => document.body.textContent.includes(tail), 'long mock reply tail', 30000, 'AUTO-COMPACT-TAIL');

    const start = await waitForFrame('compaction_start', promptAt, 'threshold compaction_start', { timeoutMs: 20000 });
    if (start.frame.sessionId !== sid) fail(`compaction_start sessionId ${start.frame.sessionId} != ${sid}`);
    if (start.frame.reason !== 'threshold') fail(`compaction_start reason=${start.frame.reason}, expected threshold`);

    const during = await rpc.call({ type: 'get_state', sessionId: sid });
    if (during.isCompacting !== true) fail(`authoritative isCompacting=${during.isCompacting} while the threshold summary is gated`);

    // The turn is parked inside finish_run (no turn_terminal yet), so the
    // page's own 5s stream watchdog issues the automatic get_state that
    // paints the badge. This is the runtime answer to "can the page state be
    // true at auto-compaction start" — the badge must come up from it.
    const pageState = await waitForFrame('response', start.t, 'page watchdog get_state(true)', {
      timeoutMs: 20000,
      pred: (frame) => frame.command === 'get_state' && frame.success === true
        && frame.sessionId === sid && frame.data && frame.data.isCompacting === true,
    });
    record('CB1');
    await waitFor(page, () => {
      const badge = document.getElementById('compact-badge');
      return Boolean(badge) && badge.hidden === false;
    }, 'badge visible after the watchdog state read', 5000);
    await twoFrames();
    // Observation is armed only now, while this round's badge is visibly
    // painted, so the hide timestamp cannot latch a pre-round state.
    const armedWhileVisible = await armBadgeObserver();
    if (armedWhileVisible !== true) fail('badge was not visibly painted when the hide observation was armed');

    const inFlightDark = await probe();
    if (inFlightDark.present !== true || inFlightDark.hidden !== false) fail(`badge not visible in flight (${JSON.stringify(inFlightDark)})`);
    if (inFlightDark.label !== 'compacting') fail(`badge label ${JSON.stringify(inFlightDark.label)} != "compacting"`);
    if (inFlightDark.dataMode !== 'llm') fail(`badge data-mode ${inFlightDark.dataMode} != llm`);
    if (hexToRgb(inFlightDark.accent) !== inFlightDark.background) {
      fail(`dark badge background ${inFlightDark.background} does not paint the --accent token ${inFlightDark.accent}`);
    }
    if (inFlightDark.animationName !== 'pulse') fail(`dark badge animation ${inFlightDark.animationName} != pulse`);
    screenshots.push(await shot('compact-badge-dark-inflight'));

    await setTheme('light');
    const inFlightLight = await probe();
    if (inFlightLight.theme !== 'light') fail('light theme did not apply');
    if (inFlightLight.hidden !== false) fail('badge disappeared when the light theme applied');
    if (hexToRgb(inFlightLight.accent) !== inFlightLight.background) {
      fail(`light badge background ${inFlightLight.background} does not paint the --accent token ${inFlightLight.accent}`);
    }
    screenshots.push(await shot('compact-badge-light-inflight'));
    await setTheme(null);
    record('CB2');

    // Reduced motion removes the pulse (the badge stays a static visible pill).
    await page.emulateMedia({ reducedMotion: 'reduce' });
    await twoFrames();
    const reduced = await probe();
    if (reduced.hidden !== false) fail('badge disappeared under prefers-reduced-motion');
    if (reduced.animationName !== 'none') fail(`reduced-motion badge animation ${reduced.animationName} != none`);
    await page.emulateMedia({ reducedMotion: 'no-preference' });
    await twoFrames();
    record('CB3');

    // Mobile keeps the same truthful chrome with no horizontal overflow.
    await page.setViewportSize({ width: 390, height: 844 });
    await twoFrames();
    const mobile = await probe();
    if (mobile.hidden !== false) fail('badge is not visible at 390x844 during compaction');
    if (mobile.scrollWidth > mobile.clientWidth) {
      fail(`mobile horizontal overflow during compaction (${mobile.scrollWidth} > ${mobile.clientWidth})`);
    }
    screenshots.push(await shot('compact-badge-mobile-inflight'));
    await page.setViewportSize({ width: 1280, height: 900 });
    await twoFrames();
    record('CB4');

    // Release the held summary: the real compaction_end must follow. The end
    // receipt is taken from the PAGE's own socket (page clock), so the
    // promptness claim below is measured end-receipt -> observed hide on ONE
    // monotonic clock.
    fs.writeFileSync(releaseFile, '1');
    const end = await waitForFrame('compaction_end', start.t, 'compaction_end after gate release', { timeoutMs: 30000 });
    if (end.frame.sessionId !== sid) fail(`compaction_end sessionId ${end.frame.sessionId} != ${sid}`);
    if (typeof start.frame.sequence !== 'number' || typeof end.frame.sequence !== 'number' || end.frame.sequence <= start.frame.sequence) {
      fail(`event order invalid: start=${start.frame.sequence} end=${end.frame.sequence}`);
    }
    if (end.frame.reason !== 'threshold') fail(`compaction_end reason=${end.frame.reason}, expected threshold`);
    const endAt = await page.waitForFunction(() => window.__compactWire?.endAt ?? null, undefined, { timeout: 5000 });
    const endPageAt = await endAt.jsonValue();
    if (typeof endPageAt !== 'number') fail('the page socket never observed compaction_end (no page-clock end receipt)');
    record('CB5');

    // CB7: the badge must clear from the end event itself. Measured entirely
    // in the page, on this round's observation only: the MutationObserver's
    // actual hide time minus the page's own compaction_end receipt must lie in
    // [0, 3000]ms, the badge must be unpainted at the check, and NO false
    // get_state snapshot may fall inside that window (a poll-driven clear
    // would need exactly such a snapshot). Nothing here waits on Node-side
    // polling, so a slow authority round-trip cannot buy the badge extra time.
    let hideDeadlineHit = true;
    try {
      await page.waitForFunction((limit) => {
        const state = window.__compactBadge;
        const end = window.__compactWire?.endAt;
        if (!state || state.armedWhileVisible !== true || state.hiddenAt === null) return false;
        if (typeof end !== 'number') return false;
        const delta = state.hiddenAt - end;
        return delta >= 0 && delta <= limit;
      }, 3000, { timeout: 5000 });
    } catch {
      hideDeadlineHit = false;
    }
    const badgeTimeline = await page.evaluate(() => window.__compactBadge || null);
    const wire = await page.evaluate(() => {
      const state = window.__compactWire || { received: [], sent: [], endAt: null };
      return {
        endAt: state.endAt,
        sent: state.sent,
        received: state.received,
      };
    });
    const actualHideAt = badgeTimeline ? badgeTimeline.hiddenAt : null;
    if (typeof wire.endAt !== 'number') {
      fail('the page socket never recorded a compaction_end receipt for this round');
    }
    const hideDeltaMs = typeof actualHideAt === 'number' && typeof wire.endAt === 'number'
      ? actualHideAt - wire.endAt
      : null;
    // Bounded to this round: frames from the end receipt through the actual
    // hide only — the page's legitimate bootstrap/idle false snapshots (which
    // precede the round) can never be counted.
    const falseStatesBeforeHide = wire.received.filter((frame) => frame.t >= wire.endAt
      && frame.t <= actualHideAt && frame.command === 'get_state' && frame.isCompacting === false);
    const pageGetsBeforeHide = wire.sent.filter((frame) => frame.t >= wire.endAt
      && frame.t <= actualHideAt && frame.type === 'get_state');
    const pageGetsAfterEnd = wire.sent.filter((frame) => frame.t >= wire.endAt && frame.type === 'get_state');
    const falseStatesAfterEnd = wire.received.filter((frame) => frame.t >= wire.endAt
      && frame.command === 'get_state' && frame.isCompacting === false);
    if (!hideDeadlineHit || hideDeltaMs === null || hideDeltaMs < 0 || hideDeltaMs > 3000) {
      const now = await probe();
      fail(`badge did not clear in [0, 3000]ms after the page-clock compaction_end receipt (delta=${hideDeltaMs}ms hidden=${now.hidden} display=${now.display} animation=${now.animationName} theme=${now.theme})`);
    }
    if (falseStatesBeforeHide.length !== 0) {
      fail(`page received ${falseStatesBeforeHide.length} false get_state snapshot(s) between compaction_end and the hide — the clear came from a state poll, not the end event`);
    }
    if (!badgeTimeline || actualHideAt === null) {
      fail('the badge hide transition was not observed by the in-page MutationObserver');
    }
    const clearedAtCb7 = await probe();
    if (badgeVisible(clearedAtCb7)) {
      fail(`badge still painted at CB7 (${JSON.stringify(clearedAtCb7)})`);
    }
    record('CB7');

    // Authority lags the published end by design (the activity guard is still
    // in scope), so it is confirmed AFTER the page-side clear — it can never
    // extend the badge's budget above.
    let after = null;
    const deadline = Date.now() + 5000;
    while (Date.now() < deadline) {
      const state = await rpc.call({ type: 'get_state', sessionId: sid });
      if (state.isCompacting === false) {
        after = state;
        break;
      }
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    if (!after) fail('authoritative isCompacting never settled false after compaction_end');
    record('CB6');

    await twoFrames();
    const afterEndDark = await probe();
    if (badgeVisible(afterEndDark)) {
      fail(`post-end badge still painted (${JSON.stringify(afterEndDark)})`);
    }
    screenshots.push(await shot('compact-badge-dark-after-end'));
    await setTheme('light');
    const afterEndLight = await probe();
    if (badgeVisible(afterEndLight)) {
      fail(`post-end light badge still painted (${JSON.stringify(afterEndLight)})`);
    }
    screenshots.push(await shot('compact-badge-light-after-end'));
    await setTheme(null);
    // The phone breakpoint must not resurrect the cleared badge either: the
    // header re-lays out at 390x844, so a clear that depended on desktop-only
    // geometry would show up here.
    await page.setViewportSize({ width: 390, height: 844 });
    await twoFrames();
    const afterEndMobile = await probe();
    if (badgeVisible(afterEndMobile)) {
      fail(`post-end mobile badge still painted (${JSON.stringify(afterEndMobile)})`);
    }
    if (afterEndMobile.scrollWidth > afterEndMobile.clientWidth) {
      fail(`mobile horizontal overflow after the end (${afterEndMobile.scrollWidth} > ${afterEndMobile.clientWidth})`);
    }
    screenshots.push(await shot('compact-badge-mobile-after-end'));
    await page.setViewportSize({ width: 1280, height: 900 });
    await twoFrames();
    record('CB8');

    const report = {
      generatedAt: new Date().toISOString(),
      url,
      evidence,
      checks,
      events: {
        start: { t: start.t, sequence: start.frame.sequence, reason: start.frame.reason },
        end: { t: end.t, sequence: end.frame.sequence, reason: end.frame.reason, result: end.frame.result ?? null },
      },
      authoritative: {
        sessionId: sid,
        boot: { isCompacting: boot.isCompacting },
        during: { isCompacting: during.isCompacting },
        after: { isCompacting: after.isCompacting },
        pageWatchdogStateAt: pageState.t,
      },
      trace: {
        pageEndAt: wire.endAt,
        actualHideAt,
        hideDeltaMs,
        hideDeadlineHit,
        pageGetStateAfterEnd: pageGetsAfterEnd.length,
        pageFalseGetStateAfterEnd: falseStatesAfterEnd.length,
        pageGetStateBeforeHide: pageGetsBeforeHide.length,
        pageFalseGetStateBeforeHide: falseStatesBeforeHide.length,
        badgeTimeline,
      },
      render: {
        inFlight: { dark: inFlightDark, light: inFlightLight, reducedMotion: reduced, mobile },
        afterEnd: { dark: afterEndDark, light: afterEndLight, mobile: afterEndMobile },
      },
      screenshots,
      pageErrors,
    };
    fs.writeFileSync(path.join(evidence, 'compact-badge-evidence.json'), `${JSON.stringify(report, null, 2)}\n`);
    if (pageErrors.length > 0) fail(`browser page errors: ${pageErrors.join(' | ')}`);
    console.log(`[web-compact-badge] PASS: start seq=${start.frame.sequence} end seq=${end.frame.sequence}, authority idle, badge cleared ${hideDeltaMs}ms after the page-clock compaction_end with no false state snapshot before the hide`);
  } finally {
    try {
      rpc?.close();
    } catch {
      /* already closed */
    }
    try {
      await browser?.close();
    } catch {
      /* already closed */
    }
  }
}

try {
  await main();
} catch (error) {
  const message = (error && error.message) || String(error);
  const setup = Boolean(error && error.setup);
  // A red run must leave the measured frames behind: the partial ledger is
  // what proves which assertion failed and what the page actually did.
  if (process.env.RPI_EVIDENCE) {
    try {
      fs.mkdirSync(evidence, { recursive: true });
      fs.writeFileSync(
        path.join(evidence, 'compact-badge-partial.json'),
        `${JSON.stringify({
          generatedAt: new Date().toISOString(),
          stage: setup ? 'setup' : 'assertion',
          passedChecks: checks,
          error: message,
          pageErrors,
        }, null, 2)}\n`,
      );
    } catch {
      /* evidence write must never mask the failure */
    }
  }
  console.error(`web-compact-badge: ${setup ? 'SETUP FAILED' : 'FAIL'}: ${message}`);
  process.exit(setup ? 1 : 2);
}
