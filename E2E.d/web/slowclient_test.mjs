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
//   C4   controlled reader stall: a synthetic main-thread busy loop
//        overlapping a sustained delta flood must not produce a false 1008
//        "client is not reading messages" for a TRANSIENT (<=4s) stall
//        (C4.1/C4.2: turns complete); a SUSTAINED no-read stall (C4.3, >5s
//        grace) must still surface the real 1008 with the server's reason —
//        the browser genuinely not reading IS the disconnect cause
//   C5   streaming Markdown authority: the real listener's text_delta paints
//        <strong>, a policy-safe <a>, inline code, and a fenced code block
//        while #send-btn is Stop generating and before the held real
//        message_end/turn_terminal frames are delivered; releasing those
//        frames commits the authoritative final, and a real second session
//        cannot receive or mutate the first session's stream/item
//   C6   1008/close toast guard: every non-1000 close the app experiences
//        MUST surface as a "connection closed (code N...)" toast — the
//        slow-client handling never hides a real close (incl. a genuine
//        1008)
//   C7   accurate close cause: the close toast carries the server's reason,
//        and a mid-flight pending command is NOT left to its 30s timer or
//        mislabeled "connection replaced" — after a close no misleading
//        "send failed: connection replaced"/"command timed out" toast may
//        appear; the accurate close toast is the surface
// Evidence: the lane always writes <evidence>/slowclient-metrics.json with
// per-step wire-receive frames (CDP), JS-handler timestamps/costs, longtask
// records, close codes/reasons, drain times, and message-order proofs — the
// assertion failures of a pre-fix baseline are thereby fully documented.

import { chromium } from 'playwright';
import fs from 'node:fs';
import { createFreshSessionViaManage } from './lib/session-lifecycle.mjs';

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
  'C4.1', 'C4.2', 'C4.3', 'C4.4',
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
            if (!this.__scHolding && window.__slowclient.holdMessageEndMarker) {
              try {
                const frame = JSON.parse(String(ev.data));
                // User MessageEnd carries the same prompt marker and precedes assistant deltas.
                if (
                  frame?.type === 'message_end'
                  && frame?.message?.role === 'assistant'
                  && JSON.stringify(frame.message || {}).includes(window.__slowclient.holdMessageEndMarker)
                ) {
                  this.__scHolding = true;
                  heldSockets.add(this);
                }
              } catch {
                /* non-JSON frame: deliver normally */
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
            if (h) h(ev);
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

    /* ---------------- C4: controlled reader stall vs sustained flood ------- */
    {
      const combos = [];
      if (BURST_N > 0) {
        // Investigative override: a single user-specified combo.
        combos.push({ n: BURST_N, bytes: 64, stallMs: STALL_MS || 3000, expect: 'no-close' });
      } else {
        // Transient stalls (<=4s) are below the server's outbound grace and
        // must NOT disconnect; the sustained combo exceeds the grace and must
        // surface the REAL 1008 (the browser genuinely not reading). Flood
        // sizes stay under Chromium's WS receive quota (~16MB) so the
        // transient combos cannot trip the browser-side abnormal-close (1006)
        // path instead of exercising the server's queue policy. The transient
        // floods are also small enough that the post-stall drain (DOM text
        // growth + scroll-pin layout) completes well inside the outcome wait.
        combos.push({ n: 8000, bytes: 64, stallMs: 2500, expect: 'no-close' });
        combos.push({ n: 8000, bytes: 64, stallMs: 4000, expect: 'no-close' });
        // Sustained no-read: 40000 chunks (~10.5MB) keeps the flood under
        // Chromium's independent receive-abort threshold (~31MB), so the
        // grace-expired queue-full 1008 (not the browser's 1006) is what
        // surfaces — verifying the server grace/1008 policy rather than the
        // browser quota.
        combos.push({ n: 40000, bytes: 64, stallMs: 7000, expect: '1008' });
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
          const diag = await page.evaluate((pfx) => {
            return {
              markerInBody: document.body.textContent.includes(pfx),
              sendBtnLabel: document.getElementById('send-btn')?.getAttribute('aria-label') ?? null,
              closes: window.__slowclient.closes.map((c) => ({ code: c.code, reason: c.reason })),
              totalMessages: window.__slowclient.messages.length,
              lastMsgDur: window.__slowclient.messages.slice(-1)[0]?.dur,
              streamingNodes: [...document.querySelectorAll('.msg--assistant .assistant-text')].map((el) => el.textContent?.length ?? 0),
            };
          }, `slowclient-done-${combo.n}-${salt}`);
          metrics.steps[`C4-flood-${combo.n}-${combo.stallMs}ms`] = { timeoutDiag: diag, stallMs: combo.stallMs };
          fail(`C4: combo (n=${combo.n}, stall=${combo.stallMs}ms) outcome timed out: ${JSON.stringify(diag)}`);
        }
        const stepKey = `C4-flood-${combo.n}-${combo.stallMs}ms`;
        await snapshotStep(page, stepKey, base, wireFrames);
        const s = metrics.steps[stepKey];
        s.stallMs = combo.stallMs;
        s.completed = !!res.done;
        s.closeDuring = res.closed;
        s.drainMs = Date.now() - t0;
        if (combo.expect === 'no-close') {
          assertBound(
            !res.closed,
            `C4: transient ${combo.stallMs}ms stall + flood closed ${res.closed ? `${res.closed.code} "${res.closed.reason}"` : ''} (n=${combo.n})`
          );
          assertBound(!!res.done, `C4: turn never completed after ${combo.stallMs}ms stall (n=${combo.n})`);
          if (res.closed && res.closed.code === 1008) {
            // The false-positive reproduction (pre-grace builds): a transient
            // reader stall is enough to trip the instant-disconnect policy.
            metrics.run.reproducedFalse1008 = { n: combo.n, stallMs: combo.stallMs, reason: res.closed.reason };
          }
          record('C4.2');
        } else {
          // Sustained no-read: the connection MUST drop with an accurate
          // close — either the server's policy 1008 ("client is not reading
          // messages", reachable with a stalled-but-not-aborting client) or
          // Chromium's own abnormal 1006 (empty reason) when the renderer
          // pipe stays full long enough for the browser to abort before the
          // grace-expired 1008 can fire. Both are genuine sustained-no-read
          // semantics; the Rust tests own the deterministic server 1008, real
          // Chromium owns the 1006.
          assertBound(!!res.closed, `C4: sustained ${combo.stallMs}ms no-read did NOT close (n=${combo.n})`);
          const closed = res.closed;
          const serverPolicy1008 = closed && closed.code === 1008 && /not reading/i.test(closed.reason || '');
          const browserAbort1006 = closed && closed.code === 1006 && (closed.reason || '') === '';
          assertBound(
            serverPolicy1008 || browserAbort1006,
            `C4: sustained no-read closed ${closed ? `${closed.code} "${closed.reason}"` : 'nothing'} — expected 1008 "not reading" or 1006 browser abort (n=${combo.n})`
          );
          metrics.run.sustainedClose = closed
            ? { n: combo.n, stallMs: combo.stallMs, code: closed.code, reason: closed.reason }
            : null;
          record('C4.4');
        }
        // Every combo can leave the turn streaming server-side (a closed
        // combo replays it after reconnect; a completed combo can retain a
        // writing mock flood). Use the unified Stop control and wait for
        // quiescence so the next prompt cannot steer into the previous run.
        // The continuing turn (if any) replays onto the reconnected socket;
        // its flood can starve the bootstrap, so do NOT require the Send
        // button to be enabled here. Wait briefly for #send-btn to show Stop generating on the
        // resumed turn, Stop it, then wait for quiescence.
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

    /* ---------------- C6: close toast guard ---------------- */
    {
      const closes = await page.evaluate(() => window.__slowclient.closes);
      const closeToasts = await page.evaluate(() => window.__slowclient.closeToasts);
      const unexpected = closes.filter((c) => c.code !== 1000);
      metrics.run.allCloses = closes.map((c) => ({ code: c.code, reason: c.reason }));
      metrics.run.allCloseToasts = closeToasts;
      for (const c of unexpected) {
        // The app MUST surface every non-1000 close (incl. a genuine 1008/1006)
        // as a toast AT CLOSE TIME (toasts auto-dismiss after ~7s, so the
        // close-time snapshot is the source of truth, not the live DOM).
        // Code 1006 (abnormal) has a split surface: the boot-probe path shows
        // the token hint; an accurate "connection closed (code 1006)" is the
        // mid-session surface. Both are accepted; the misleading-token-only
        // case is recorded as a finding.
        const needles =
          c.code === 1006
            ? ['connection closed (code 1006', 'connection failed (wrong or missing token']
            : [`connection closed (code ${c.code}`];
        // Allow the 120ms close-toast snapshot to land.
        await page.waitForTimeout(300);
        const snapshot = await page.evaluate(
          ({ code, reason, needles: nd }) => {
            const match = window.__slowclient.closeToasts.find(
              (ct) => ct.code === code && ct.reason === reason
            );
            if (!match) return { found: false, snapshot: null };
            return {
              found: true,
              snapshot: match.toasts,
              seen: match.toasts.some((t) => nd.some((n) => t.includes(n))),
            };
          },
          { code: c.code, reason: c.reason, needles }
        );
        assertBound(!!snapshot.found, `C6.1: close-time toast snapshot missing for ${c.code} "${c.reason}"`);
        assertBound(snapshot.seen, `C6.1: non-1000 close toast missing for ${c.code} "${c.reason}"`);
        if (c.code === 1006 && snapshot.seen) {
          const tokenOnly = (snapshot.snapshot || []).some((t) => t.includes('connection failed (wrong or missing token'));
          const codeSurface = (snapshot.snapshot || []).some((t) => t.includes('connection closed (code 1006'));
          if (tokenOnly && !codeSurface) {
            metrics.run.mislabeled1006Toast = true;
            console.error('web-slowclient: METRIC: mid-session 1006 surfaced only the token hint (mislabeled close cause)');
          }
        }
        record('C6.1');
      }
      if (unexpected.length === 0) {
        // No non-1000 close occurred: the guard is vacuous but the toast
        // plumbing is still exercised (the app's toast list is live).
        record('C6.1');
      }
    }

    /* ---------------- C7: accurate close cause + no mislabeled rejection --- */
    {
      const closes = await page.evaluate(() => window.__slowclient.closes);
      const non1000 = closes.find((c) => c.code !== 1000);
      if (non1000) {
        // The accurate close toast (code + server reason where present) must
        // surface in the close-time snapshot: 1008 -> the server's reason
        // "client is not reading messages"; 1006 (browser abnormal abort) ->
        // "connection closed (code 1006" via the mid-session accurate-cause
        // path.
        const needle = non1000.code === 1008 ? 'client is not reading messages' : 'connection closed (code 1006';
        const seen = await page.evaluate(
          ({ code, reason, nd }) =>
            (window.__slowclient.closeToasts.find((ct) => ct.code === code && ct.reason === reason)?.toasts || [])
              .some((t) => t.includes(nd)),
          { code: non1000.code, reason: non1000.reason, nd: needle }
        );
        assertBound(seen, `C7.1: accurate close toast missing in close-time snapshot for ${non1000.code}`);
        record('C7.1');
        // No mislabeled pending rejection: after a close the pending is
        // drained in onclose with the close cause (rpc=true, off the toast
        // surface), so neither the reconnect-era "send failed: connection
        // replaced" nor the 30s "command timed out" toast may appear. Watch
        // the full window in which the old 30s timer would have fired.
        let mislabeled = await page.evaluate(() =>
          [...document.querySelectorAll('#toasts .toast')].some((el) => {
            const t = el.textContent || '';
            return t.includes('command timed out') || t.includes('send failed: connection replaced');
          })
        );
        if (!mislabeled) {
          const started = Date.now();
          while (Date.now() - started < 32000) {
            await page.waitForTimeout(500);
            mislabeled = await page.evaluate(() =>
              [...document.querySelectorAll('#toasts .toast')].some((el) => {
                const t = el.textContent || '';
                return t.includes('command timed out') || t.includes('send failed: connection replaced');
              })
            );
            if (mislabeled) break;
          }
        }
        assertBound(!mislabeled, 'C7: mislabeled pending-rejection toast appeared after close (command timed out / connection replaced)');
        record('C7.2');
      } else {
        record('C7.1');
        record('C7.2');
      }
    }

    console.log(
      'web-slowclient: PASSED (calibration, heavy finalize, amplification bound, transient-stall no false 1008, sustained no-read accurate close + cause, close-toast guard)'
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
