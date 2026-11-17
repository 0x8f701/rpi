// Slow-client WebSocket contract E2E lane (playwright half of
// E2E.d/web/slowclient.sh).
//
// Environment:
//   RPI_URL          http://127.0.0.1:<port>/web
//   RPI_TOKEN        token file content (served via rpi-auth.<token> subprotocol)
//   RPI_CHROME       executable path of the system Chrome (optional)
//   RPI_EVIDENCE     evidence dir for metrics JSON
//
// Mock scenario: `slowclient` (E2E.d/lib/user_mock_server.py) —
//   slowclient-burst-<N>-<bytes>          N text deltas, back-to-back
//   slowclient-burstslow-<N>-<bytes>-<d>  N text deltas at <d>s cadence
//   slowclient-burstslow-<N>-<bytes>-<d>-<salt>  same, with a per-request
//                                         salt scoping the chunk prefix
//                                         (s<N>:<salt>:) and done marker
//                                         (slowclient-done-<N>-<salt>) so
//                                         repeated same-size floods stay
//                                         distinguishable in one transcript
//   slowclient-heavy-<count>              one heavy final message (KaTeX +
//                                         mermaid + table)
//
// What the lane measures/asserts (slow-client contract):
//   C0   boot: page + WS connect
//   C1   calibration: a small burst drains with bounded per-message handler
//        cost, no long tasks, no close; full message ORDER preserved
//   C2   heavy finalize: KaTeX + mermaid final messages render (and the
//        finalization long tasks are recorded, not asserted away)
//   C3   heavy-transcript amplification: a delta burst while heavy final
//        messages are in the transcript must NOT re-render the whole
//        transcript per delta (max handler cost / long tasks bounded);
//        sampled deltas stay in order; no close
//   C4   controlled renderer stall: a synthetic main-thread busy loop
//        overlapping a delta flood must not produce a false 1008. Every
//        combo, including the 7s stall, is below the 15s outbound grace
//        and is a JS stall, not a TCP no-read. Each turn completes with
//        no close; the 7s flood also proves exact salted content/order.
//        Dead-client eviction stays in the Rust TCP/stall tests.
//   C5   streaming Markdown authority: the real listener's text_delta paints
//        <strong>, a policy-safe <a>, inline code, and a fenced code block
//        while #send-btn is Stop generating and before the held real
//        message_end/turn_terminal frames are delivered; releasing those
//        frames commits the authoritative final, and a real second session
//        cannot receive or mutate the first session's stream/item
//   C6   close toast guard: a test-owned reverse proxy holds one real
//        side_chat_list response, then sends a server 1008 close and
//        finishes the handshake. The app MUST surface that close as a
//        "connection closed (code 1008...)" toast. This is close-surface
//        evidence, not dead-client eviction.
//   C7   close surface: the toast carries the controlled 1008 reason, and
//        the same side_chat_list response was not delivered through the
//        close handler. This does not observe the product pending-registry
//        reject; that remains the existing pending unit test.
// Evidence: the lane always writes <evidence>/slowclient-metrics.json with
// per-step wire-receive frames (CDP), JS-handler timestamps/costs, longtask
// records, close codes/reasons, drain times, and message-order proofs — the
// assertion failures of a pre-fix baseline are thereby fully documented.

import { chromium } from 'playwright';
import fs from 'node:fs';
import http from 'node:http';
import WebSocket, { WebSocketServer } from 'ws';
import { createFreshSessionViaManage } from './lib/session-lifecycle.mjs';
import { clickFeature } from './lib/feature-menu.mjs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
// Tunable escalation knobs (defaults are the reproduction envelope).
const STALL_MS = Number(process.env.RPI_SLOWCLIENT_STALL_MS || '0');
const BURST_N = Number(process.env.RPI_SLOWCLIENT_BURST_N || '0');
// Lenient mode: record threshold failures as metrics instead of exiting, so a
// pre-fix baseline run still documents EVERY step (1008 reproduction, long
// tasks, false timeout). The hard gate (default) fails on the first violation.
const LENIENT = process.env.RPI_SLOWCLIENT_LENIENT === '1';
const softFailures = [];

const DOCUMENTED_IDS = [
  'C0.1', 'C0.2',
  'C1.1', 'C1.2', 'C1.3',
  'C2.1',
  'C3.1', 'C3.2', 'C3.3',
  'C4.1', 'C4.2', 'C4.3',
  'C5.1', 'C5.2', 'C5.3',
  'C6.1',
  'C7.1', 'C7.2',
];
const executed = new Set();
const metrics = { steps: {}, run: {} };

function record(id) {
  executed.add(id);
  console.log(`[web-slowclient:assert] ${id}`);
}

class LaneFailure extends Error {}

function fail(message) {
  // Thrown (not process.exit) so the finally block always writes the metrics
  // evidence before the process exits — a failing run must still document.
  throw new LaneFailure(message);
}

/** Threshold assertion: hard-fails the gate by default; in lenient mode the
 *  violation is recorded in metrics and the run continues so a pre-fix
 *  baseline still documents every step. */
function assertBound(ok, message) {
  if (ok) return;
  if (LENIENT) {
    softFailures.push(message);
    console.error(`web-slowclient: SOFT-FAIL (lenient): ${message}`);
    return;
  }
  fail(message);
}

function writeEvidence() {
  const missing = DOCUMENTED_IDS.filter((id) => !executed.has(id));
  if (missing.length > 0) {
    console.error(`web-slowclient: documented assertions never executed: ${missing.join(', ')}`);
  }
  try {
    fs.writeFileSync(
      `${evidence}/slowclient-metrics.json`,
      JSON.stringify({ executed: [...executed].sort(), missing, softFailures, metrics }, null, 2)
    );
    fs.writeFileSync(
      `${evidence}/coverage-assertions.json`,
      JSON.stringify({ executed: [...executed].sort() }, null, 2)
    );
  } catch (err) {
    console.error(`web-slowclient: could not write evidence: ${err.message}`);
  }
}

async function waitFor(page, fn, label, timeoutMs = 30000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
  const sid = typeof label === 'string' ? (label.match(/^C\d+\.\d+/) || [null])[0] : null;
  if (sid) record(sid);
}

async function connOn(page, label) {
  await waitFor(
    page,
    () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
    label || 'C0.2: WS did not reach connected',
    30000
  );
}

/** Wait until the turn completes: the marker text is in the DOM AND
 *  #send-btn is not Stop generating (message_end/agent_settled committed
 *  the final render). */
async function waitTurnDone(page, marker, label, timeoutMs = 90000) {
  await waitFor(page, (m) => document.body.textContent.includes(m), label, timeoutMs, marker);
  await waitFor(
    page,
    () => document.getElementById('send-btn')?.getAttribute('aria-label') !== 'Stop generating',
    `${label}: send-btn never left Stop generating (turn not settled)`,
    20000
  );
}

/** Send a prompt and wait for its turn to finish. */
async function promptAndWait(page, prompt, marker, label, timeoutMs = 90000) {
  await page.fill('#prompt-input', prompt);
  await page.press('#prompt-input', 'Enter');
  await waitTurnDone(page, marker, label, timeoutMs);
}

/** Block the renderer main thread for `ms` with a synchronous busy loop
 *  (deliberately exceeding the 50ms long-task threshold). */
async function injectStall(page, ms) {
  await page.evaluate(
    (delay) => {
      const end = performance.now() + delay;
      while (performance.now() < end) {
        /* busy */
      }
    },
    ms
  );
}

function percentile(sorted, p) {
  if (sorted.length === 0) return 0;
  const idx = Math.min(sorted.length - 1, Math.max(0, Math.floor((p / 100) * sorted.length)));
  return sorted[idx];
}
/** Counts plus first/last 50 numeric samples. No payload strings. */
function boundSamples(items, pick) {
  const samples = items.map(pick);
  return {
    count: samples.length,
    first50: samples.slice(0, 50),
    last50: samples.slice(-50),
  };
}


/** Per-step metrics snapshot from the page instrumentation + wire frames.
 *  `base` is a baseline captured at step start ({msgs, longtasks, closes,
 *  wall}); every metric is a DELTA over the step so cumulative state from
 *  earlier steps (or the reconnect storm after a close) never leaks in. */
async function snapshotStep(page, step, base, wireFrames) {
  const diag = await page.evaluate(() => window.__slowclient);
  const msgs = diag.messages.slice(base.msgs);
  const longtasks = diag.longtasks.slice(base.longtasks);
  const closes = diag.closes.slice(base.closes);
  const handlerDurs = msgs.map((m) => m.dur).sort((a, b) => a - b);
  const stepFrames = wireFrames.filter((f) => f.t >= base.wall);
  const stats = {
    step,
    messageEvents: msgs.length,
    handlerTotalMs: handlerDurs.reduce((a, b) => a + b, 0),
    handlerP50Ms: percentile(handlerDurs, 50),
    handlerP95Ms: percentile(handlerDurs, 95),
    handlerMaxMs: handlerDurs.length ? handlerDurs[handlerDurs.length - 1] : 0,
    wireFrames: stepFrames.length,
    wireBytes: stepFrames.reduce((a, f) => a + f.len, 0),
    longtasks: longtasks.length,
    longtaskTotalMs: longtasks.reduce((a, t) => a + t.dur, 0),
    longtaskMaxMs: longtasks.reduce((a, t) => Math.max(a, t.dur), 0),
    closes: closes.map((c) => ({ code: c.code, reason: c.reason })),
  };
  metrics.steps[step] = stats;
  return stats;
}

/** Capture a per-step baseline for snapshotStep. */
async function stepBase(page) {
  const diag = await page.evaluate(() => window.__slowclient);
  return { msgs: diag.messages.length, longtasks: diag.longtasks.length, closes: diag.closes.length, wall: Date.now() };
}

function assertOrderInTranscript(page, prefix, indices, label) {
  // Sampled burst indices must appear in the final transcript in ascending
  // position (message order preserved end-to-end). `prefix` (s<N>:) makes the
  // chunk markers unique to this burst so restored/earlier transcript text
  // cannot satisfy the check.
  return page.evaluate(
    ({ pfx, idx, lbl }) => {
      const text = [...document.querySelectorAll('.msg--assistant .assistant-text')]
        .map((el) => el.textContent || '')
        .join('\n');
      const positions = [];
      for (const i of idx) {
        const pos = text.indexOf(`${pfx}${i}/`);
        if (pos === -1) return { ok: false, reason: `${pfx}${i}/ missing`, lbl };
        positions.push(pos);
      }
      for (let i = 1; i < positions.length; i++) {
        if (positions[i] <= positions[i - 1]) {
          return { ok: false, reason: `order break at ${pfx}${idx[i]}/ (pos ${positions[i]} <= ${positions[i - 1]})`, lbl };
        }
      }
      return { ok: true, positions: positions.length, lbl };
    },
    { pfx: prefix, idx: indices, lbl: label }
  );
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
    const page = await context.newPage();

    // Instrument BEFORE the app loads: a WebSocket subclass that wraps the
    // app's property handlers (the app uses ws.onmessage/onclose assignment)
    // to timestamp every message handler invocation, plus a longtask
    // observer. The app's handlers are preserved verbatim — this only
    // measures around them.
    await page.addInitScript((t) => {
      const heldSockets = new Set();
      window.__slowclient = {
        messages: [], closes: [], closeToasts: [], longtasks: [], opens: 0,
        settlements: [],
        holdMessageEndMarker: '',
        heldFrames: 0,
        releaseHeldFrames() {
          for (const socket of [...heldSockets]) socket.__scReleaseHeld();
        },
      };
      window.localStorage.setItem('rpi-web-token', t);
      const authority = new URL(window.location.href).host;
      window.localStorage.setItem(`rpi-web-token:${encodeURIComponent(authority)}`, t);
      const NativeWS = window.WebSocket;
      class DiagWebSocket extends NativeWS {
        constructor(...args) {
          super(...args);
          this.__scHandlers = { message: null, close: null, open: null, error: null };
          this.__scHolding = false;
          this.__scHeld = [];
          this.__scReleaseHeld = () => {
            this.__scHolding = false;
            heldSockets.delete(this);
            const frames = this.__scHeld.splice(0);
            window.__slowclient.heldFrames = Math.max(0, window.__slowclient.heldFrames - frames.length);
            for (const deliver of frames) deliver();
          };
          Object.defineProperty(this, 'onmessage', {
            configurable: true,
            get() { return this.__scHandlers.message; },
            set(fn) { this.__scHandlers.message = fn; },
          });
          Object.defineProperty(this, 'onclose', {
            configurable: true,
            get() { return this.__scHandlers.close; },
            set(fn) { this.__scHandlers.close = fn; },
          });
          Object.defineProperty(this, 'onopen', {
            configurable: true,
            get() { return this.__scHandlers.open; },
            set(fn) { this.__scHandlers.open = fn; },
          });
          Object.defineProperty(this, 'onerror', {
            configurable: true,
            get() { return this.__scHandlers.error; },
            set(fn) { this.__scHandlers.error = fn; },
          });
          const nativeSend = this.send.bind(this);
          this.send = (data, ...sendArgs) => {
            try {
              const frame = JSON.parse(String(data));
              if (frame && typeof frame.id === 'string' && frame.type === 'side_chat_list') {
                window.__slowclient.settlements.push({ id: frame.id, kind: 'sent', type: frame.type });
              }
            } catch {
              /* non-JSON send: not an RPC command */
            }
            return nativeSend(data, ...sendArgs);
          };
          super.addEventListener('message', (ev) => {
            const deliver = () => {
              const t0 = performance.now();
              const h = this.__scHandlers.message;
              if (!h) return;
              try {
                h(ev);
              } finally {
                window.__slowclient.messages.push({
                  t0,
                  dur: performance.now() - t0,
                  len: String(ev.data).length,
                });
              }
            };
            let parsed = null;
            try { parsed = JSON.parse(String(ev.data)); } catch { parsed = null; }
            if (parsed && parsed.type === 'response' && typeof parsed.id === 'string') {
              window.__slowclient.settlements.push({
                id: parsed.id,
                kind: parsed.success === false ? 'response-failure' : 'response',
                command: parsed.command || '',
              });
            }
            if (!this.__scHolding && window.__slowclient.holdMessageEndMarker && parsed) {
              if (
                parsed.type === 'message_end'
                && parsed.message?.role === 'assistant'
                && JSON.stringify(parsed.message || {}).includes(window.__slowclient.holdMessageEndMarker)
              ) {
                this.__scHolding = true;
                heldSockets.add(this);
              }
            }
            if (this.__scHolding) {
              this.__scHeld.push(deliver);
              window.__slowclient.heldFrames += 1;
              return;
            }
            deliver();
          });
          super.addEventListener('close', (ev) => {
            heldSockets.delete(this);
            this.__scHeld.length = 0;
            window.__slowclient.closes.push({
              code: ev.code,
              reason: ev.reason || '',
              t: performance.now(),
            });
            // Toasts auto-dismiss after ~7s, so capture the toast surface a
            // tick after the close handler (and its React render) runs — the
            // close-cause assertions run minutes later and must not depend on
            // the live DOM.
            setTimeout(() => {
              window.__slowclient.closeToasts.push({
                code: ev.code,
                reason: ev.reason || '',
                toasts: [...document.querySelectorAll('#toasts .toast')].map((el) => el.textContent || ''),
              });
            }, 120);
            const h = this.__scHandlers.close;
            if (h) {
              const before = window.__slowclient.settlements.length;
              try {
                h(ev);
              } finally {
                window.__slowclient.settlements.push({
                  id: '',
                  kind: 'close-handler',
                  code: ev.code,
                  reason: ev.reason || '',
                  responsesDuringClose: window.__slowclient.settlements
                    .slice(before)
                    .filter((row) => row.kind === 'response' || row.kind === 'response-failure').length,
                });
              }
            }
          });
          super.addEventListener('open', () => {
            window.__slowclient.opens += 1;
            const h = this.__scHandlers.open;
            if (h) h();
          });
          super.addEventListener('error', () => {
            const h = this.__scHandlers.error;
            if (h) h();
          });
        }
      }
      for (const k of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
        Object.defineProperty(DiagWebSocket, k, { value: NativeWS[k] });
      }
      window.WebSocket = DiagWebSocket;
      if (typeof PerformanceObserver === 'function') {
        try {
          const po = new PerformanceObserver((list) => {
            for (const entry of list.getEntries()) {
              window.__slowclient.longtasks.push({ start: entry.startTime, dur: entry.duration });
            }
          });
          po.observe({ entryTypes: ['longtask'] });
          window.__slowclient.longtaskObserver = po;
        } catch {
          /* longtask observer unavailable: metrics degrade gracefully */
        }
      }
    }, token);

    // Wire-level receive frames (Playwright's network-level WS hook): per-step
    // byte/frame counts show whether the browser kept reading the socket while
    // the main thread was stalled.
    const wireFrames = [];
    page.on('websocket', (ws) => {
      ws.on('framereceived', (frame) => {
        const payload = typeof frame.payload === 'string' ? frame.payload : '';
        if (payload) wireFrames.push({ t: Date.now(), len: payload.length });
      });
    });

    page.on('pageerror', (err) => {
      console.error(`web-slowclient: page error: ${err.message}`);
    });

    /* ---------------- C0: boot + connect ---------------- */
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'C0.1: page title missing');
    await connOn(page, 'C0.2: WS did not reach connected');
    const bootCloseCount = await page.evaluate(() => window.__slowclient.closes.length);
    if (bootCloseCount !== 0) {
      fail(`C0: unexpected close at boot: ${JSON.stringify(await page.evaluate(() => window.__slowclient.closes))}`);
    }

    /* ---------------- C5: pre-end Markdown + session isolation ----------- */
    {
      // Use the real Manage Session -> new_session lifecycle so the streamed
      // assistant has a different authoritative session from the boot session.
      const { beforeSessionId: sessionA, sessionId: sessionB } = await createFreshSessionViaManage(
        page,
        waitFor,
        'C5 streaming Markdown session setup',
      );
      const marker = `stream-md-${Date.now().toString(36)}`;
      // The slowclient fixture echoes unmatched prompts as
      // `slowclient-unrouted: <prompt>`, so this exact prompt is real streamed
      // Markdown while its unique marker still identifies message_end.
      const prompt = [
        `**stream strong ${marker}** and [safe stream link](https://example.com/stream?q=1) with \`inline_stream_code()\`.`,
        '',
        '```rust',
        'fn streamed_fence() -> &\'static str { "safe" }',
        '```',
      ].join('\n');
      const beforeCount = await page.evaluate(() => document.querySelectorAll('.msg--assistant .assistant-text').length);
      await page.evaluate((m) => { window.__slowclient.holdMessageEndMarker = m; }, marker);
      await page.fill('#prompt-input', prompt);
      await page.press('#prompt-input', 'Enter');

      // The real text_delta must have rendered structured Markdown while the
      // real message_end and later terminal frames are still held at the
      // instrumented slow-client boundary. No DOM is inserted by the test.
      try {
        await page.waitForFunction(
          ({ sid, mark, before }) => {
            const btn = document.getElementById('send-btn');
            if (!btn || btn.getAttribute('aria-label') !== 'Stop generating' || window.__slowclient.heldFrames < 1) return false;
            const nodes = [...document.querySelectorAll('.assistant-text[data-stream-item-id]')];
            const live = nodes.find((el) => el.dataset.streamSessionId === sid && (el.textContent || '').includes(mark));
            if (!live || document.querySelectorAll('.msg--assistant .assistant-text').length <= before) return false;
            const link = live.querySelector('a');
            const inline = [...live.querySelectorAll('code.md-code')].find((el) => !el.closest('.md-fence'));
            const fence = live.querySelector('.md-fence .md-fence__pre code.md-code');
            return live.querySelector('strong')?.textContent === `stream strong ${mark}`
              && link?.textContent === 'safe stream link'
              && link?.getAttribute('href') === 'https://example.com/stream?q=1'
              && inline?.textContent === 'inline_stream_code()'
              && (fence?.textContent || '').includes('fn streamed_fence()');
          },
          { sid: sessionB, mark: marker, before: beforeCount },
          { timeout: 30000 },
        );
      } catch {
        const dump = await page.evaluate(() => ({
          heldFrames: window.__slowclient.heldFrames,
          aria: document.getElementById('send-btn')?.getAttribute('aria-label') || '',
          live: [...document.querySelectorAll('.assistant-text[data-stream-item-id]')].map((el) => ({
            sid: el.dataset.streamSessionId || '',
            html: (el.innerHTML || '').slice(0, 240),
          })),
        }));
        fail(`C5.1: real pre-message_end streaming Markdown DOM never appeared (timeout 30000ms) dump=${JSON.stringify(dump)}`);
      }
      record('C5.1');
      const streamingIdentity = await page.evaluate((mark) => {
        const live = [...document.querySelectorAll('.assistant-text[data-stream-item-id]')]
          .find((el) => (el.textContent || '').includes(mark));
        return live ? {
          sessionId: live.dataset.streamSessionId || '',
          itemId: live.dataset.streamItemId || '',
          text: live.textContent || '',
        } : null;
      }, marker);
      if (!streamingIdentity || streamingIdentity.sessionId !== sessionB || !streamingIdentity.itemId) {
        fail(`C5.1: streaming assistant identity missing or cross-session (${JSON.stringify(streamingIdentity)})`);
      }

      await page.evaluate(() => {
        window.__slowclient.holdMessageEndMarker = '';
        window.__slowclient.releaseHeldFrames();
      });
      await waitFor(
        page,
        (mark) => {
          const btn = document.getElementById('send-btn');
          if (btn && btn.getAttribute('aria-label') === 'Stop generating') return false;
          const final = [...document.querySelectorAll('.msg--assistant .assistant-text')]
            .find((el) => !el.hasAttribute('data-stream-item-id') && (el.textContent || '').includes(mark));
          if (!final) return false;
          const link = final.querySelector('a');
          const inline = [...final.querySelectorAll('code.md-code')].find((el) => !el.closest('.md-fence'));
          const fence = final.querySelector('.md-fence .md-fence__pre code.md-code');
          return final.querySelector('strong')?.textContent === `stream strong ${mark}`
            && link?.getAttribute('href') === 'https://example.com/stream?q=1'
            && inline?.textContent === 'inline_stream_code()'
            && (fence?.textContent || '').includes('fn streamed_fence()');
        },
        'C5.2: authoritative final Markdown structure never committed',
        30000,
        marker,
      );
      const finalB = await page.evaluate((mark) => {
        const final = [...document.querySelectorAll('.msg--assistant .assistant-text')]
          .find((el) => (el.textContent || '').includes(mark));
        return final ? { text: final.textContent || '', html: final.innerHTML } : null;
      }, marker);
      if (!finalB || finalB.text !== streamingIdentity.text) {
        fail('C5.2: authoritative final text did not equal the accumulated streamed text');
      }

      const switchedToA = await page.evaluate((sid) => {
        const row = [...document.querySelectorAll('.session-sidebar__switch')].find((el) => el.dataset.sessionId === sid);
        if (!row) return false;
        row.click();
        return true;
      }, sessionA);
      if (!switchedToA) fail(`C5.3: boot session row ${sessionA} missing`);
      await waitFor(
        page,
        (sid) => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId === sid,
        'C5.3: did not switch to isolation session',
        30000,
        sessionA,
      );
      const isolationPrompt = 'slowclient-burst-24-16';
      await promptAndWait(page, isolationPrompt, 'slowclient-done-24', 'C5.3: isolation-session stream never completed', 60000);
      const leakedIntoA = await page.evaluate(
        ({ mark, itemId }) => {
          const active = document.querySelector('#transcript .transcript-surface:not([hidden])');
          if (!active) return true;
          return (active.textContent || '').includes(mark)
            || active.querySelector(`[data-stream-item-id="${CSS.escape(itemId)}"]`) !== null;
        },
        { mark: marker, itemId: streamingIdentity.itemId },
      );
      if (leakedIntoA) fail('C5.3: session B Markdown/item identity leaked into the visible isolation session');

      const switchedToB = await page.evaluate((sid) => {
        const row = [...document.querySelectorAll('.session-sidebar__switch')].find((el) => el.dataset.sessionId === sid);
        if (!row) return false;
        row.click();
        return true;
      }, sessionB);
      if (!switchedToB) fail(`C5.3: Markdown session row ${sessionB} missing`);
      await waitFor(
        page,
        ({ sid, mark }) => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId === sid
          && (document.querySelector('#transcript .transcript-surface:not([hidden])')?.textContent || '').includes(mark),
        'C5.3: did not restore isolated Markdown session',
        30000,
        { sid: sessionB, mark: marker },
      );
      const restoredB = await page.evaluate((mark) => {
        const final = [...document.querySelectorAll('.msg--assistant .assistant-text')]
          .find((el) => (el.textContent || '').includes(mark));
        return final ? { text: final.textContent || '', html: final.innerHTML } : null;
      }, marker);
      if (!restoredB || restoredB.text !== finalB.text || restoredB.html !== finalB.html) {
        fail('C5.3: another session stream mutated the authoritative Markdown final');
      }
      record('C5.3');
      metrics.steps['C5-streaming-markdown'] = { sessionA, sessionB, itemId: streamingIdentity.itemId };
    }

    /* ---------------- C1: calibration burst ---------------- */
    {
      const t0 = Date.now();
      const base = await stepBase(page);
      const closesBefore = await page.evaluate(() => window.__slowclient.closes.length);
      await promptAndWait(page, 'slowclient-burst-200-16', 'slowclient-done-200', 'C1.1: calibration burst never completed', 60000);
      const msgsAfter = await page.evaluate(() => window.__slowclient.messages.length);
      const closesAfter = await page.evaluate(() => window.__slowclient.closes.length);
      const stepMsgs = msgsAfter - base.msgs;
      if (stepMsgs < 190) {
        fail(`C1: calibration burst delivered only ${stepMsgs} message events (expected ~200 deltas + lifecycle)`);
      }
      const order = await assertOrderInTranscript(
        page,
        's200:',
        Array.from({ length: 199 }, (_, i) => i),
        'C1.2'
      );
      if (!order.ok) fail(`C1: message order broken: ${order.reason}`);
      record('C1.2');
      await snapshotStep(page, 'C1-calibration', base, wireFrames);
      const s = metrics.steps['C1-calibration'];
      assertBound(s.handlerMaxMs <= 20, `C1: calibration handler too expensive (max ${s.handlerMaxMs}ms)`);
      assertBound(s.longtaskMaxMs <= 300, `C1: calibration produced a long task (${s.longtaskMaxMs}ms) — burst must be cheap`);
      if (closesAfter !== closesBefore) {
        fail(`C1: unexpected close during calibration: ${JSON.stringify(await page.evaluate(() => window.__slowclient.closes))}`);
      }
      record('C1.3');
      metrics.steps['C1-calibration'].drainMs = Date.now() - t0;
    }

    /* ---------------- C2: heavy finalization (KaTeX + mermaid) ---------------- */
    {
      const t0 = Date.now();
      const base = await stepBase(page);
      for (let i = 0; i < 2; i++) {
        await promptAndWait(page, 'slowclient-heavy-12', 'slowclient-heavy-done', `C2.1: heavy final message ${i + 1} never completed`, 60000);
      }
      const katex = await page.evaluate(() => document.querySelectorAll('.assistant-text .katex').length);
      const mermaid = await page.evaluate(() => document.querySelectorAll('.md-mermaid-host--rendered').length);
      if (katex < 12) fail(`C2: KaTeX never rendered fully (${katex} < 12)`);
      if (mermaid < 1) fail(`C2: mermaid never rendered (${mermaid} hosts)`);
      // Wait for the ASYNC mermaid hydration to fully finish (both hosts
      // rendered): its synchronous render portion is a long task, and letting
      // it overlap the next step's delta burst would jank the flood and flake
      // the no-1008 assertions.
      await waitFor(
        page,
        () => document.querySelectorAll('.md-mermaid-host--rendered').length >= 2,
        'C2.1: mermaid hydration never finished',
        30000
      );
      record('C2.1');
      await snapshotStep(page, 'C2-heavy-finalize', base, wireFrames);
      metrics.steps['C2-heavy-finalize'].drainMs = Date.now() - t0;
    }

    /* ---------------- C3: heavy-transcript burst amplification ---------------- */
    {
      const t0 = Date.now();
      const base = await stepBase(page);
      const closesBefore = await page.evaluate(() => window.__slowclient.closes.length);
      await promptAndWait(page, 'slowclient-burst-4000-32', 'slowclient-done-4000', 'C3.1: amplification burst never completed', 120000);
      const order = await assertOrderInTranscript(
        page,
        's4000:',
        [0, 500, 1000, 1500, 2000, 2500, 3000, 3500],
        'C3.2'
      );
      if (!order.ok) {
        const diag = await page.evaluate(() => {
          const nodes = [...document.querySelectorAll('.msg--assistant .assistant-text')];
          const last = nodes.length ? (nodes[nodes.length - 1].textContent || '') : '';
          let markerEl = null;
          if (document.body.textContent.includes('slowclient-burst-done')) {
            const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
            let n;
            while ((n = walker.nextNode())) {
              if ((n.textContent || '').includes('slowclient-burst-done')) {
                const p = n.parentElement;
                markerEl = p ? `${p.tagName}.${p.className}` : 'text-only';
                break;
              }
            }
          }
          return {
            assistantCount: nodes.length,
            lastLen: last.length,
            lastTail: last.slice(-300),
            hasMarker: document.body.textContent.includes('slowclient-burst-done'),
            markerEl,
            closes: window.__slowclient.closes.map((c) => ({ code: c.code, reason: c.reason })),
            totalMessages: window.__slowclient.messages.length,
            sendBtnLabel: document.getElementById('send-btn')?.getAttribute('aria-label') ?? null,
          };
        });
        metrics.steps['C3-diagnostic'] = diag;
        fail(`C3: message order broken under amplification: ${order.reason} (diag=${JSON.stringify(diag)})`);
      }
      record('C3.2');
      const closesAfter = await page.evaluate(() => window.__slowclient.closes.length);
      assertBound(
        closesAfter === closesBefore,
        `C3: close during amplification burst: ${JSON.stringify(await page.evaluate(() => window.__slowclient.closes))}`
      );
      await snapshotStep(page, 'C3-amplification', base, wireFrames);
      const s = metrics.steps['C3-amplification'];
      metrics.steps['C3-amplification'].drainMs = Date.now() - t0;
      // The hot path must NOT re-render the whole heavy transcript per delta:
      // bounded per-message cost in a plain (unstalled) burst. Long tasks from
      // the heavy finalize itself are recorded as metrics, not gated (they are
      // inherent to rendering the KaTeX/mermaid content once).
      assertBound(s.handlerP95Ms <= 20, `C3: per-delta handler too expensive with heavy transcript (p95 ${s.handlerP95Ms}ms) — hot path re-renders per chunk`);
      assertBound(s.handlerMaxMs <= 200, `C3: a single delta handler blocked the main thread ${s.handlerMaxMs}ms`);
      record('C3.3');
    }

    /* ---------------- C4: renderer stall must not false-close ---------- */
    {
      const combos = [];
      if (BURST_N > 0) {
        combos.push({ n: BURST_N, bytes: 64, stallMs: STALL_MS || 3000, expect: 'no-close' });
      } else {
        // Renderer busy loops, not paused TCP sockets. Every duration is
        // below the 15s outbound grace, so none may disconnect.
        combos.push({ n: 8000, bytes: 64, stallMs: 2500, expect: 'no-close' });
        combos.push({ n: 8000, bytes: 64, stallMs: 4000, expect: 'no-close' });
        combos.push({ n: 40000, bytes: 64, stallMs: 7000, expect: 'no-close' });
      }
      metrics.run.stallCombos = combos;
      for (const combo of combos) {
        await connOn(page, 'C4.0: WS not connected before stall combo');
        const t0 = Date.now();
        const base = await stepBase(page);
        const closesBefore = await page.evaluate(() => window.__slowclient.closes.length);
        // Salt the burstslow prompt per combo: repeated n=8000 combos would
        // otherwise reuse the `s8000:`/`slowclient-done-8000` transcript text
        // of the earlier combo, which would satisfy the C4.1 anchor and the
        // outcome marker instantly — the stall would then overlap the flood
        // START instead of its middle (a stale-anchor sequencing bug). The
        // mock scopes the chunk prefix (s<N>:<salt>:) and the done marker
        // (slowclient-done-<N>-<salt>) to this combo's flood.
        const salt = `c${combo.n}s${combo.stallMs}`;
        const prompt = `slowclient-burstslow-${combo.n}-${combo.bytes}-0.0-${salt}`;
        await page.fill('#prompt-input', prompt);
        await page.press('#prompt-input', 'Enter');
        // Start the stall as soon as THIS burst's first delta renders (the
        // s<N>:<salt>: prefix is unique per combo, so restored transcript text
        // from an earlier flood cannot fake the start).
        await waitFor(
          page,
          (pfx) => document.body.textContent.includes(`${pfx}0/`),
          'C4.1: flood never started',
          30000,
          `s${combo.n}:${salt}:`
        );
        record('C4.1');
        await injectStall(page, combo.stallMs);
        // Outcome race: turn completes, or a NEW close appears (1008 when the
        // browser stops reading beyond the server's grace; 1006 when a huge
        // undrained flood trips Chromium's receive quota), or timeout. The
        // predicate returns null while neither has happened so waitForFunction
        // keeps polling.
        let res;
        try {
          const outcome = await page.waitForFunction(
            ({ marker, closes }) => {
              const done = document.body.textContent.includes(marker) &&
                document.getElementById('send-btn')?.getAttribute('aria-label') !== 'Stop generating';
              if (window.__slowclient.closes.length > closes) {
                const c = window.__slowclient.closes.slice(-1)[0];
                return { done: false, closed: { code: c.code, reason: c.reason } };
              }
              if (done) return { done: true, closed: null };
              return null;
            },
            { marker: `slowclient-done-${combo.n}-${salt}`, closes: closesBefore },
            { timeout: 90000 }
          );
          res = await outcome.jsonValue();
        } catch (err) {
          const timeoutAt = Date.now();
          const diag = await page.evaluate((arg) => {
            const sc = window.__slowclient;
            const msgs = sc.messages.slice(arg.msgs);
            const longtasks = sc.longtasks.slice(arg.longtasks);
            const sample = (items, pick) => {
              const samples = items.map(pick);
              return { count: samples.length, first50: samples.slice(0, 50), last50: samples.slice(-50) };
            };
            return {
              markerInBody: document.body.textContent.includes(arg.marker),
              sendBtnLabel: document.getElementById('send-btn')?.getAttribute('aria-label') ?? null,
              closes: sc.closes.map((c) => ({ code: c.code, reason: c.reason })),
              totalMessages: sc.messages.length,
              lastMsgDur: sc.messages.slice(-1)[0]?.dur,
              streamingNodes: [...document.querySelectorAll('.msg--assistant .assistant-text')].map((el) => el.textContent?.length ?? 0),
              message: sample(msgs, (m) => ({ t0: m.t0, len: m.len, dur: m.dur })),
              longtasks: sample(longtasks, (t) => ({ start: t.start, dur: t.dur })),
            };
          }, { marker: `slowclient-done-${combo.n}-${salt}`, msgs: base.msgs, longtasks: base.longtasks });
          const stepFrames = wireFrames.filter((f) => f.t >= base.wall && f.t <= timeoutAt);
          diag.wire = boundSamples(stepFrames, (f) => ({ t: f.t, len: f.len }));
          metrics.steps[`C4-flood-${combo.n}-${combo.stallMs}ms`] = { timeoutDiag: diag, stallMs: combo.stallMs, timedOut: true };
          fail(`C4: combo (n=${combo.n}, stall=${combo.stallMs}ms) outcome timed out: ${JSON.stringify(diag)}`);
        }
        const stepKey = `C4-flood-${combo.n}-${combo.stallMs}ms`;
        await snapshotStep(page, stepKey, base, wireFrames);
        const s = metrics.steps[stepKey];
        s.stallMs = combo.stallMs;
        s.completed = !!res.done;
        s.closeDuring = res.closed;
        s.drainMs = Date.now() - t0;
        assertBound(
          !res.closed,
          `C4: ${combo.stallMs}ms renderer stall + flood closed ${res.closed ? `${res.closed.code} "${res.closed.reason}"` : ''} (n=${combo.n})`
        );
        assertBound(!!res.done, `C4: turn never completed after ${combo.stallMs}ms renderer stall (n=${combo.n})`);
        if (res.closed && res.closed.code === 1008) {
          metrics.run.reproducedFalse1008 = { n: combo.n, stallMs: combo.stallMs, reason: res.closed.reason };
        }
        if (combo.n === 40000 && combo.stallMs === 7000) {
          const marker = `slowclient-done-${combo.n}-${salt}`;
          const exact = await page.evaluate(({ pfx, count, bytes, marker: done }) => {
            const pad = (value) => (value.length >= bytes ? value : value + 'x'.repeat(bytes - value.length));
            let expected = '';
            for (let i = 0; i < count - 1; i += 1) expected += pad(`${pfx}${i}/`);
            expected += pad(done);
            const nodes = [...document.querySelectorAll('.msg--assistant .assistant-text')];
            const matches = nodes.filter((el) => (el.textContent || '') === expected);
            if (matches.length !== 1) {
              return {
                ok: false,
                reason: `expected one exact salted assistant item, found ${matches.length}`,
                expectedLen: expected.length,
              };
            }
            return { ok: true, chunks: count, bytes: expected.length };
          }, { pfx: `s${combo.n}:${salt}:`, count: combo.n, bytes: combo.bytes, marker });
          assertBound(exact.ok, `C4: 7s flood content/order failed: ${exact.reason}`);
          s.exactChunks = exact.chunks || 0;
        }
        record('C4.2');
        // A completed combo can retain a writing mock flood. Stop it so the
        // next prompt cannot steer into the previous run.
        try {
          await waitFor(
            page,
            () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Stop generating',
            'C4.5: turn did not resume streaming after combo',
            15000
          );
        } catch {
          // The turn may already have ended server-side; skip the Stop.
        }
        await page.evaluate(() => {
          const stop = document.getElementById('send-btn');
          if (stop && stop.getAttribute('aria-label') === 'Stop generating') {
            stop.click();
          }
        });
        try {
          await waitFor(
            page,
            () => document.getElementById('send-btn')?.getAttribute('aria-label') !== 'Stop generating',
            'C4.5: turn never stopped after combo',
            30000
          );
        } catch {
          // The turn may already have ended server-side; the settle below
          // still absorbs late replay frames.
        }
        await page.waitForTimeout(1500);
      }
      record('C4.3');
    }

    /* ---------------- C6/C7: held RPC + server close handshake ---------- */
    {
      const CONTROLLED_REASON = 'slowclient controlled close';
      const upstream = new URL(url);
      const proxy = await startCloseProxy(upstream.hostname, Number(upstream.port));
      metrics.run.controlledClose = { proxy: proxy.url, reason: CONTROLLED_REASON };
      let bodyError = null;
      try {
        await page.goto(`${proxy.url}/web`, { waitUntil: 'domcontentloaded', timeout: 20000 });
        await connOn(page, 'C6.0: WS not connected through close proxy');
        const closesBefore = await page.evaluate(() => window.__slowclient.closes.length);
        const captured = proxy.arm();
        await clickFeature(page, '#sidechat-toggle-btn');
        await waitFor(
          page,
          () => !!document.getElementById('side-chat-panel'),
          'C6.0: side chat panel did not open',
          10000
        );
        const pendingId = await captured;
        await proxy.waitHeld(pendingId);
        const stillPending = await page.evaluate((id) => {
          const rows = window.__slowclient.settlements;
          return rows.some((row) => row.id === id && row.kind === 'sent')
            && !rows.some((row) => row.id === id && row.kind !== 'sent');
        }, pendingId);
        assertBound(stillPending, `C6: side_chat_list ${pendingId} response was delivered before close`);
        await proxy.sendClose(1008, CONTROLLED_REASON);
        await page.waitForFunction(
          (before) => window.__slowclient.closes.length > before,
          closesBefore,
          { timeout: 5000 }
        );
        await page.waitForTimeout(300);
        const closed = await page.evaluate((before) => {
          const row = window.__slowclient.closes.slice(before).find((item) => item.code !== 1000);
          return row ? { code: row.code, reason: row.reason } : null;
        }, closesBefore);
        assertBound(!!closed, 'C6.1: controlled server close was not observed');
        assertBound(closed.code === 1008, `C6.1: controlled close code ${closed && closed.code}, expected 1008`);
        assertBound(
          closed.reason === CONTROLLED_REASON,
          `C6.1: controlled close reason ${JSON.stringify(closed && closed.reason)}`
        );
        const snapshot = await page.evaluate(({ code, reason }) => {
          const match = window.__slowclient.closeToasts.find((ct) => ct.code === code && ct.reason === reason);
          if (!match) return { found: false, snapshot: [] };
          const needle = `connection closed (code ${code}`;
          return { found: true, snapshot: match.toasts, seen: match.toasts.some((text) => text.includes(needle)) };
        }, closed);
        assertBound(snapshot.found, `C6.1: close-time toast snapshot missing for ${closed.code}`);
        assertBound(snapshot.seen, `C6.1: connection closed toast missing for ${closed.code}`);
        record('C6.1');
        const accurate = await page.evaluate(({ code, reason }) =>
          (window.__slowclient.closeToasts.find((ct) => ct.code === code && ct.reason === reason)?.toasts || [])
            .some((text) => text.includes(`connection closed (code ${code}`) && text.includes(reason)),
          closed
        );
        assertBound(accurate, 'C7.1: close toast missing the controlled 1008 reason');
        record('C7.1');
        const settlement = await page.evaluate((id) => {
          const rows = window.__slowclient.settlements.filter((row) => row.id === id);
          const handler = window.__slowclient.settlements.find((row) => row.kind === 'close-handler');
          return {
            sent: rows.filter((row) => row.kind === 'sent').length,
            response: rows.filter((row) => row.kind === 'response' || row.kind === 'response-failure').length,
            handler: handler || null,
          };
        }, pendingId);
        assertBound(settlement.sent === 1, `C7.2: side_chat_list ${pendingId} was not sent once`);
        assertBound(settlement.response === 0, `C7.2: held side_chat_list ${pendingId} response was delivered`);
        assertBound(
          !!settlement.handler && settlement.handler.code === 1008 && settlement.handler.reason === CONTROLLED_REASON,
          `C7.2: close handler did not observe controlled 1008 (${JSON.stringify(settlement.handler)})`
        );
        assertBound(
          settlement.handler.responsesDuringClose === 0,
          `C7.2: ${settlement.handler.responsesDuringClose} response(s) arrived during close handling`
        );
        const mislabeled = await page.evaluate(() =>
          [...document.querySelectorAll('#toasts .toast')].some((el) => {
            const text = el.textContent || '';
            return text.includes('timed out after')
              || text.includes('send failed: connection replaced')
              || text.includes('connection replaced');
          })
        );
        assertBound(!mislabeled, 'C7.2: timeout or connection-replaced toast appeared after controlled close');
        record('C7.2');
      } catch (error) {
        bodyError = error;
      } finally {
        let cleanupError = null;
        try {
          await proxy.close();
        } catch (error) {
          cleanupError = error;
        }
        if (bodyError && cleanupError) {
          const aggregate = new Error(`${bodyError.message}; ${cleanupError.message}`);
          aggregate.errors = [bodyError, cleanupError];
          throw aggregate;
        }
        if (bodyError) throw bodyError;
        if (cleanupError) throw cleanupError;
      }
    }

    console.log(
      'web-slowclient: PASSED (calibration, heavy finalize, amplification bound, renderer-stall no false 1008, exact 7s content, controlled 1008 close + cause)'
    );
  } finally {
    writeEvidence();
    await browser.close();
  }
}

main().catch((err) => {
  if (err instanceof LaneFailure) {
    console.error(`web-slowclient: FAIL: ${err.message}`);
  } else {
    console.error(`web-slowclient: unexpected error: ${err && err.stack ? err.stack : err}`);
  }
  writeEvidence();
  process.exit(2);
});

function startCloseProxy(upstreamHost, upstreamPort) {
  const httpServer = http.createServer((req, res) => {
    const target = http.request({
      host: upstreamHost,
      port: upstreamPort,
      method: req.method,
      path: req.url,
      headers: { ...req.headers, host: `${upstreamHost}:${upstreamPort}` },
    }, (upstream) => {
      res.writeHead(upstream.statusCode || 502, upstream.headers);
      upstream.pipe(res);
    });
    target.on('error', (error) => {
      proxy.fail(error);
      if (!res.headersSent) res.writeHead(502);
      res.end();
    });
    req.pipe(target);
  });
  const wss = new WebSocketServer({ noServer: true });
  const sockets = new Set();
  let holdId = '';
  let held = null;
  const proxy = {
    errors: [],
    armed: false,
    capture: null,
    fail(error) { proxy.errors.push(error); },
    url: '',
    arm() {
      proxy.armed = true;
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('side_chat_list request was not captured before forwarding')), 5000);
        proxy.capture = (id) => {
          clearTimeout(timer);
          resolve(id);
        };
      });
    },
    async waitHeld(id) {
      const deadline = Date.now() + 5000;
      while (!held || held.id !== id) {
        if (proxy.errors.length) throw proxy.errors[0];
        if (Date.now() >= deadline) throw new Error(`matching side_chat_list response ${id} was not held`);
        await new Promise((resolve) => setTimeout(resolve, 25));
      }
    },
    async sendClose(code, reason) {
      if (!held || held.released) throw new Error('cannot close before the captured side_chat_list response is held');
      const ack = new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('close handshake timed out before opcode 8 ack')), 1000);
        held.browser.once('close', (ackCode, ackReason) => {
          clearTimeout(timer);
          if (ackCode !== code || String(ackReason) !== reason) {
            reject(new Error(`close ack ${ackCode} ${JSON.stringify(String(ackReason))} != ${code} ${JSON.stringify(reason)}`));
            return;
          }
          resolve();
        });
      });
      held.browser.close(code, reason);
      await ack;
    },
    async close() {
      const closed = await Promise.allSettled([...sockets].map((socket) => new Promise((resolve) => {
        if (socket.readyState === WebSocket.CLOSED) {
          resolve();
          return;
        }
        const timer = setTimeout(() => {
          proxy.fail(new Error('websocket cleanup close timed out'));
          resolve();
        }, 1000);
        socket.once('close', () => {
          clearTimeout(timer);
          resolve();
        });
        if (socket.readyState !== WebSocket.CLOSING) socket.close(1001, 'proxy cleanup');
      })));
      for (const result of closed) if (result.status === 'rejected') proxy.fail(result.reason);
      const wssResult = await new Promise((resolve) => wss.close((error) => resolve(error || null)));
      const httpResult = await new Promise((resolve) => httpServer.close((error) => resolve(error || null)));
      if (wssResult) proxy.fail(wssResult);
      if (httpResult) proxy.fail(httpResult);
      if (proxy.errors.length === 1) throw proxy.errors[0];
      if (proxy.errors.length > 1) {
        const error = new Error(proxy.errors.map((failure) => failure.message || String(failure)).join('; '));
        error.errors = [...proxy.errors];
        throw error;
      }
    },
  };
  httpServer.on('error', (error) => proxy.fail(error));
  httpServer.on('clientError', (error) => proxy.fail(error));
  wss.on('error', (error) => proxy.fail(error));
  httpServer.on('upgrade', (req, socket, head) => {
    socket.on('error', (error) => proxy.fail(error));
    if (req.url !== '/ws') {
      socket.destroy();
      return;
    }
    wss.handleUpgrade(req, socket, head, (browser) => {
      sockets.add(browser);
      const upstream = new WebSocket(`ws://${upstreamHost}:${upstreamPort}/ws`, {
        headers: { host: req.headers.host, origin: req.headers.origin || '' },
        protocols: req.headers['sec-websocket-protocol'],
      });
      sockets.add(upstream);
      const toUpstream = [];
      const toBrowser = [];
      let opened = false;
      const release = (error) => {
        if (held) held.released = true;
        proxy.fail(error);
        for (const item of toBrowser.splice(0)) {
          if (browser.readyState === WebSocket.OPEN) browser.send(item.data, { binary: item.binary });
        }
      };
      upstream.on('open', () => {
        opened = true;
        for (const item of toUpstream.splice(0)) upstream.send(item.data, { binary: item.binary });
      });
      upstream.on('message', (data, isBinary) => {
        const text = isBinary ? '' : String(data);
        let matched = false;
        if (holdId && (!held || held.released)) {
          try {
            const frame = JSON.parse(text);
            matched = frame.type === 'response' && frame.command === 'side_chat_list' && frame.id === holdId && frame.success === true;
            if (matched) held = { id: frame.id, browser, released: false };
          } catch (error) {
            release(error);
            return;
          }
        }
        if (matched) return;
        if (browser.readyState === WebSocket.OPEN) browser.send(data, { binary: isBinary });
        else toBrowser.push({ data, binary: isBinary });
      });
      browser.on('message', (data, isBinary) => {
        let commandId = '';
        if (proxy.armed && !holdId && !isBinary) {
          try {
            const frame = JSON.parse(String(data));
            if (frame.type === 'side_chat_list' && typeof frame.id === 'string') commandId = frame.id;
          } catch (error) {
            proxy.fail(error);
            return;
          }
        }
        if (commandId) holdId = commandId;
        if (upstream.readyState === WebSocket.OPEN) upstream.send(data, { binary: isBinary });
        else if (!opened) toUpstream.push({ data, binary: isBinary });
        else {
          proxy.fail(new Error('browser frame arrived before upstream websocket opened'));
          return;
        }
        if (commandId && proxy.capture) proxy.capture(commandId);
      });
      upstream.on('error', release);
      browser.on('error', release);
      upstream.on('close', () => { if (!held || held.released) browser.close(); });
      browser.on('close', () => upstream.close());
    });
  });
  return new Promise((resolve, reject) => {
    httpServer.once('error', reject);
    httpServer.listen(0, '127.0.0.1', () => {
      proxy.url = `http://127.0.0.1:${httpServer.address().port}`;
      resolve(proxy);
    });
  });
}
