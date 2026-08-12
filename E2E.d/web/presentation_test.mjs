// Presentation regression lane — real Chromium assertions for the
// tool-card / Command / process / write / read presentation, the Thinking
// streaming lifecycle, the composer control equal-height, the session
// sidebar provider grouping + search, and the inline image / video media
// contracts. Every assertion observes the live DOM (computed styles,
// bounding rects, naturalWidth, video attributes) — never source text.
//
// Drives the REAL `rpi --listen` fixture (loopback steering mock + token
// file). The mock seeds are content-routed by EXACT prompt markers
// ('presentation bash success', 'presentation process error', …) added
// additively to E2E.d/lib/user_mock_server.py; the durable bashExecution
// path is driven directly through a second WebSocket RPC client.
//
// Environment:
//   RPI_URL            http://127.0.0.1:<port>/web
//   RPI_TOKEN          token file content (rpi-auth.<token> subprotocol)
//   RPI_CHROME         system Chrome executable (optional)
//   RPI_EVIDENCE       evidence dir (coverage-assertions.json written here)
//
// Exit: 0 = every assertion passed + evidence written; 2 = assertion failure;
//       1 = setup failure (playwright unusable).

import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';
import WebSocket from 'ws';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

const executed = new Set();
function record(id) {
  executed.add(id);
  console.log(`[web-pres:assert] ${id}`);
}
function fail(message) {
  console.error(`web-pres: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 30000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

/** Assertion with an ID: waits for a DOM condition, records the ID on pass. */
async function assertId(page, id, fn, label, timeoutMs = 30000, arg) {
  await waitFor(page, fn, `${id}: ${label}`, timeoutMs, arg);
  record(id);
}

async function connectPage(page) {
  if (token) {
    await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
  }
  await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
  await waitFor(page, () => document.title === 'rpi web', 'page title missing');
  await waitFor(page, () => document.querySelector('#conn-state') !== null, 'conn-state missing');
  await waitFor(page, () => document.getElementById('conn-state').dataset.state === 'on', 'WS did not reach "connected"');
}

/** Send a prompt. */
async function sendPrompt(page, text) {
  await page.fill('#prompt-input', text);
  await page.press('#prompt-input', 'Enter');
}

/** Wait for the turn to settle (stream badge cleared). */
async function waitForSettled(page, timeoutMs = 40000) {
  await waitFor(page, () => document.getElementById('stream-badge').hidden === true, 'turn did not settle (stream badge still on)', timeoutMs);
}

/** Raw RPC client over the SAME listener (second WS session). */
function rpcClient(wsUrl) {
  const ws = new WebSocket(wsUrl, token ? [`rpi-auth.${token}`] : []);
  const pending = new Map();
  let seq = 0;
  ws.on('message', (raw) => {
    let frame;
    try {
      frame = JSON.parse(String(raw));
    } catch {
      return;
    }
    if (frame && frame.type === 'response' && frame.id && pending.has(frame.id)) {
      const { resolve, reject } = pending.get(frame.id);
      pending.delete(frame.id);
      if (frame.success) resolve(frame.data || {});
      else reject(new Error(frame.error || 'rpc failed'));
    }
  });
  const ready = new Promise((resolve, reject) => {
    ws.on('open', resolve);
    ws.on('error', reject);
  });
  return {
    ready,
    async call(command) {
      await ready;
      const id = `pres-${++seq}`;
      return new Promise((resolve, reject) => {
        pending.set(id, { resolve, reject });
        ws.send(JSON.stringify({ ...command, id }));
        setTimeout(() => {
          if (pending.delete(id)) reject(new Error(`rpc timed out: ${command.type}`));
        }, 15000);
      });
    },
    close() {
      try {
        ws.close();
      } catch {
        /* already closed */
      }
    },
  };
}

/** Derive the raw ws:// URL from the http:// page URL for the RPC client. */
function wsUrlFrom(pageUrl) {
  return pageUrl.replace(/^http/, 'ws').replace(/\/web$/, '/ws');
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  let rpc;
  try {
    // ============ connect + composer equal height (desktop) ============
    const page = await browser.newPage();
    page.on('pageerror', (err) => console.error(`web-pres: page error: ${err.message}`));
    await connectPage(page);

    await assertId(
      page,
      'pres.composer-equal-height',
      () => {
        const btn = document.getElementById('command-btn');
        const ta = document.getElementById('prompt-input');
        const send = document.getElementById('send-btn');
        if (!btn || !ta || !send) return false;
        const h = [btn, ta, send].map((el) => el.getBoundingClientRect().height);
        return Math.max(...h) - Math.min(...h) <= 1;
      },
      'composer command/input/send heights differ by >1px (desktop)',
    );

    // ============ Command card success (green border, title, clamp, no raw args) ============
    await sendPrompt(page, 'presentation bash success');
    await assertId(
      page,
      'pres.command-title',
      () => {
        const cards = [...document.querySelectorAll('.tool-card--command')];
        const card = cards[cards.length - 1];
        if (!card) return false;
        return (card.querySelector('.tool-card__title')?.textContent || '') === 'Command';
      },
      'Command card title is not "Command"',
    );
    // No "bash" as a visible title (the old UI showed the raw tool name).
    {
      const cards = await page.evaluate(() => [...document.querySelectorAll('.tool-card--command')].map((c) => c.querySelector('.tool-card__title')?.textContent || ''));
      if (cards.some((t) => t.toLowerCase() === 'bash')) fail('a Command card showed "bash" as its title instead of "Command"');
      record('pres.command-no-bash-title');
    }
    // No raw args / no raw JSON shell in the default (non-expanded) state.
    await assertId(
      page,
      'pres.command-no-default-args',
      () => {
        const cards = [...document.querySelectorAll('.tool-card--command')];
        const card = cards[cards.length - 1];
        if (!card) return false;
        return card.querySelector('.tool-card__args') === null && card.querySelector('.tool-card__raw') === null;
      },
      'Command card rendered raw args / raw JSON shell',
    );
    // data-tool-status + success green border.
    await assertId(
      page,
      'pres.command-success-green',
      () => {
        const cards = [...document.querySelectorAll('.tool-card--command')];
        const card = cards[cards.length - 1];
        if (!card) return false;
        if (card.getAttribute('data-tool-status') !== 'done') return false;
        if (!card.classList.contains('tool-card--done')) return false;
        const m = (getComputedStyle(card).borderTopColor || '').match(/rgba?\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)/i);
        if (!m) return false;
        const r = +m[1], g = +m[2], b = +m[3];
        return g > r && g > b && g > 60;
      },
      'success Command card border is not green',
    );
    // Two-line clamp: a 6-line command overflows the clamped box, proving the
    // display is bounded to ~2 lines (an unclamped card grows to fit, so
    // scrollHeight === clientHeight).
    await assertId(
      page,
      'pres.command-two-line-clamp',
      () => {
        const cards = [...document.querySelectorAll('.tool-card--command')];
        const card = cards[cards.length - 1];
        if (!card) return false;
        const text = card.querySelector('.tool-card__command-text');
        if (!text) return false;
        return text.scrollHeight - text.clientHeight > 1;
      },
      'Command card command text is not clamped to two lines',
    );
    await waitForSettled(page);

    // ============ Command card failure (red border) ============
    await sendPrompt(page, 'presentation bash error');
    await assertId(
      page,
      'pres.command-failure-red',
      () => {
        const cards = [...document.querySelectorAll('.tool-card--command')];
        const card = cards[cards.length - 1];
        if (!card) return false;
        if (card.getAttribute('data-tool-status') !== 'error') return false;
        if (!card.classList.contains('tool-card--error')) return false;
        const m = (getComputedStyle(card).borderTopColor || '').match(/rgba?\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)/i);
        if (!m) return false;
        const r = +m[1], g = +m[2], b = +m[3];
        return r > g && r > b && r > 60;
      },
      'failure Command card border is not red',
    );
    await waitForSettled(page);

    // ============ Write card success (path summary, no raw content) ============
    await sendPrompt(page, 'presentation write success');
    await assertId(
      page,
      'pres.write-summary',
      () => {
        const cards = [...document.querySelectorAll('.tool-card--write')];
        const card = cards[cards.length - 1];
        if (!card) return false;
        if (card.getAttribute('data-tool-status') !== 'done') return false;
        if (card.querySelector('.tool-card__args') !== null) return false;
        return (card.querySelector('.tool-card__summary-path')?.textContent || '').includes('notes.txt');
      },
      'write card did not render the path summary without raw args',
    );
    {
      const leak = await page.evaluate(() => {
        const cards = [...document.querySelectorAll('.tool-card--write')];
        const card = cards[cards.length - 1];
        if (!card) return false;
        // The large `content` must NOT be dumped as raw JSON in the default view.
        return card.textContent.includes('"content"') || (card.textContent.match(/write payload line/g) || []).length > 6;
      });
      if (leak) fail('write card leaked the raw content JSON / unbounded payload into the default view');
    }
    await waitForSettled(page);

    // ============ Read card (path summary + inline image render) ============
    await sendPrompt(page, 'presentation read image');
    await assertId(
      page,
      'pres.read-summary',
      () => {
        const cards = [...document.querySelectorAll('.tool-card--read')];
        const card = cards[cards.length - 1];
        if (!card) return false;
        if (card.querySelector('.tool-card__args') !== null) return false;
        return (card.querySelector('.tool-card__summary-path')?.textContent || '').includes('logo.png');
      },
      'read card did not render the path summary without raw args',
    );
    await assertId(
      page,
      'pres.image-render',
      () => {
        const cards = [...document.querySelectorAll('.tool-card--read')];
        const card = cards[cards.length - 1];
        if (!card) return false;
        const img = card.querySelector('img.tool-media__image');
        return !!img && img.naturalWidth > 0;
      },
      'read image card did not render a decoded inline image',
    );
    await waitForSettled(page);

    // ============ Process cards long + short (human summary, equal width) ============
    await sendPrompt(page, 'presentation process long');
    await assertId(
      page,
      'pres.process-long',
      () => {
        const cards = [...document.querySelectorAll('.tool-card--process')];
        const card = cards[cards.length - 1];
        if (!card) return false;
        if (card.getAttribute('data-tool-status') !== 'done') return false;
        if (card.querySelector('.tool-card__args') !== null) return false;
        const label = card.querySelector('.tool-card__summary-line')?.textContent || card.querySelector('.tool-card__summary')?.textContent || '';
        return label.trim().length > 0;
      },
      'process (long) card did not render a human summary without raw args',
    );
    await waitForSettled(page);

    await sendPrompt(page, 'presentation process short');
    await assertId(
      page,
      'pres.process-short',
      () => {
        const cards = [...document.querySelectorAll('.tool-card--process')];
        if (cards.length < 2) return false;
        const card = cards[cards.length - 1];
        if (card.getAttribute('data-tool-status') !== 'done') return false;
        if (card.querySelector('.tool-card__args') !== null) return false;
        const label = card.querySelector('.tool-card__summary-line')?.textContent || card.querySelector('.tool-card__summary')?.textContent || '';
        return label.trim().length > 0;
      },
      'process (short) card did not render a human summary without raw args',
    );
    await waitForSettled(page);

    record('pres.process-summary');
    // Both process cards must be the same width (not sized to label content).
    await assertId(
      page,
      'pres.process-equal-width',
      () => {
        const cards = [...document.querySelectorAll('.tool-card--process')];
        if (cards.length < 2) return false;
        const w = cards.map((c) => c.getBoundingClientRect().width);
        return Math.max(...w) - Math.min(...w) <= 1;
      },
      'process cards differ in width by >1px',
    );

    // ============ Process error (.op required property missing) ============
    await sendPrompt(page, 'presentation process error');
    await assertId(
      page,
      'pres.process-error-op',
      () => {
        const cards = [...document.querySelectorAll('.tool-card--process')];
        const card = cards[cards.length - 1];
        if (!card) return false;
        if (card.getAttribute('data-tool-status') !== 'error') return false;
        if (!card.classList.contains('tool-card--error')) return false;
        const err = (card.querySelector('.tool-card__summary-error')?.textContent || '').toLowerCase();
        return err.includes('op') && err.includes('missing');
      },
      'process error card did not surface the .op required-property-missing error',
    );
    await waitForSettled(page);

    // ============ hub wait — humanized card, never the raw {ids,op,timeoutMs} ============
    // Reported regression: the transcript showed `hub / running… /
    // {ids,op,timeoutMs}`. The REAL hub tool runs here (the loopback session
    // is the orchestration main agent): a 1.5s message-wait renders the
    // humanized running card (fixed "Waiting" title + clear feedback), then
    // settles into concise timeout copy — with no raw args / internal ids.
    await sendPrompt(page, 'presentation hub wait');
    await assertId(
      page,
      'pres.hub-wait-running',
      () => {
        const cards = [...document.querySelectorAll('.tool-card--hub')];
        const card = cards[cards.length - 1];
        if (!card) return false;
        if (card.getAttribute('data-tool-status') !== 'running') return false;
        const head = (card.querySelector('.tool-card__head')?.textContent || '');
        const txt = card.textContent || '';
        if (!head.includes('Waiting')) return false;
        if (!txt.includes('Waiting for an agent message')) return false;
        // The raw args envelope / internal ids / timeoutMs must never render.
        if (card.querySelector('.tool-card__args') !== null || card.querySelector('.tool-card__raw') !== null) return false;
        if (txt.includes('timeoutMs') || txt.includes('"op"') || txt.includes('"ids"')) return false;
        return true;
      },
      'hub wait running card did not render the humanized waiting view',
      15000,
    );
    // RPC client (second WS session) for the typed delivery below; reused by
    // the durable bashExecution section further down.
    rpc = rpcClient(wsUrlFrom(url));
    await rpc.ready;
    await waitForSettled(page);
    await assertId(
      page,
      'pres.hub-wait-timeout',
      () => {
        const cards = [...document.querySelectorAll('.tool-card--hub')];
        const card = cards[cards.length - 1];
        if (!card) return false;
        if (card.getAttribute('data-tool-status') !== 'done') return false;
        const txt = card.textContent || '';
        if (!txt.includes('No message')) return false;
        if (card.querySelector('.tool-card__args') !== null || card.querySelector('.tool-card__raw') !== null) return false;
        if (txt.includes('timeoutMs') || txt.includes('"op"') || txt.includes('"ids"')) return false;
        return true;
      },
      'hub wait timeout card did not render concise copy without raw JSON',
    );

    // ============ hub wait — typed incoming IRC (details.message) ============
    // A 20s message-wait lets the driver RPC-deliver a Main→Main mailbox
    // message mid-wait; the REAL hub tool drains it and the settled card
    // carries the typed details.message projection (fixed "IRC" title, body,
    // never the model-facing `[m1] Main: …` prose / raw JSON).
    const activeSidHub = await page.evaluate(
      () => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
    );
    if (!activeSidHub) fail('no active session id found in the sidebar for the hub RPC');
    await sendPrompt(page, 'presentation hub wait typed');
    await assertId(
      page,
      'pres.hub-wait-typed-running',
      () => {
        const cards = [...document.querySelectorAll('.tool-card--hub')];
        const card = cards[cards.length - 1];
        if (!card) return false;
        return card.getAttribute('data-tool-status') === 'running'
          && (card.textContent || '').includes('Waiting for an agent message');
      },
      'hub wait typed: running card never appeared',
      15000,
    );
    const hubWaitReceipt = await rpc.call({
      type: 'hub_send',
      sessionId: activeSidHub,
      to: 'Main',
      body: 'hello-hub-e2e',
      replyTo: null,
    });
    if (!Array.isArray(hubWaitReceipt.receipts) || hubWaitReceipt.receipts.length === 0) {
      fail('hub_send RPC returned no receipts for the typed hub wait');
    }
    await waitForSettled(page);
    await assertId(
      page,
      'pres.hub-wait-typed-irc',
      () => {
        const cards = [...document.querySelectorAll('.tool-card--hub')];
        const card = cards[cards.length - 1];
        if (!card) return false;
        if (card.getAttribute('data-tool-status') !== 'done') return false;
        const head = (card.querySelector('.tool-card__head')?.textContent || '');
        const txt = card.textContent || '';
        if (!head.includes('IRC')) return false;
        if (!txt.includes('hello-hub-e2e')) return false;
        // The model-facing `[m1] Main: …` prose must not leak into the view.
        if (/\[m[0-9]+\]\s*Main\s*:/.test(txt)) return false;
        if (card.querySelector('.tool-card__args') !== null || card.querySelector('.tool-card__raw') !== null) return false;
        return true;
      },
      'hub wait settled card did not render the typed incoming IRC',
    );

    // ============ hub send — no regression: outgoing message + outcome ============
    // The real hub send to an unknown recipient UUID returns a failed
    // receipt; the card keeps the outgoing message + outcome label and never
    // dumps the raw args envelope.
    await sendPrompt(page, 'presentation hub send');
    await waitForSettled(page);
    await assertId(
      page,
      'pres.hub-send-card',
      () => {
        const cards = [...document.querySelectorAll('.tool-card--hub')];
        const card = cards[cards.length - 1];
        if (!card) return false;
        if (card.getAttribute('data-tool-status') !== 'done') return false;
        const head = (card.querySelector('.tool-card__head')?.textContent || '');
        const txt = card.textContent || '';
        if (!head.includes('IRC')) return false;
        if (!txt.includes('ping presentation hub send')) return false;
        if (!txt.includes('failed')) return false;
        if (card.querySelector('.tool-card__args') !== null || card.querySelector('.tool-card__raw') !== null) return false;
        return true;
      },
      'hub send card did not render the outgoing message + outcome without raw JSON',
    );

    // ============ no raw JSON / no "done" text across all tool cards ============
    await assertId(
      page,
      'pres.no-raw-json',
      () => {
        const variants = ['.tool-card--command', '.tool-card--process', '.tool-card--write', '.tool-card--read'];
        for (const sel of variants) {
          for (const card of document.querySelectorAll(sel)) {
            if (card.querySelector('.tool-card__args') !== null) return false;
            // Default (non-expanded) view must not dump raw JSON key:value noise.
            const txt = card.textContent || '';
            if (txt.includes('"path":') || txt.includes('"command":') || txt.includes('"op":') || txt.includes('"argv":')) return false;
          }
        }
        return true;
      },
      'a tool card leaked raw JSON args into the default view',
    );
    await assertId(
      page,
      'pres.no-done-text',
      () => {
        for (const card of document.querySelectorAll('.tool-card')) {
          if (card.getAttribute('data-tool-status') === 'done' && card.querySelector('.tool-card__state') !== null) return false;
        }
        return true;
      },
      'a done tool card rendered a visible "done" state label',
    );
    // All tool cards in the transcript share one width (presentation invariant).
    await assertId(
      page,
      'pres.tool-cards-equal-width',
      () => {
        const cards = [...document.querySelectorAll('#transcript .tool-card')];
        if (cards.length < 2) return false;
        const w = cards.map((c) => c.getBoundingClientRect().width);
        return Math.max(...w) - Math.min(...w) <= 1;
      },
      'tool cards in the transcript differ in width by >1px',
    );

    // ============ Thinking — streaming visible, final hidden ============
    // Enable reasoning so the mock's reasoning_content deltas are requested.
    if (await page.evaluate(() => document.getElementById('thinking-select') !== null)) {
      await page.selectOption('#thinking-select', 'high');
    }
    await sendPrompt(page, 'presentation thinking turn');
    // During the stream the <details class="thinking"> must be visible with
    // a non-empty body — and OPEN by default (default-visible contract), so
    // the body actually renders on screen until the user collapses it.
    await assertId(
      page,
      'pres.thinking-streaming-visible',
      () => {
        const t = document.querySelector('.thinking');
        if (!t || t.hidden !== false) return false;
        const body = t.querySelector('.thinking__body');
        if (!body || (body.textContent || '').length === 0) return false;
        if (t.open !== true) return false;
        return body.getBoundingClientRect().height > 0;
      },
      'thinking block never appeared open during the streaming turn',
      20000,
    );
    // Header contract: brain icon + title "Thinking" — never the bare
    // lowercase `thinking` marker, never a `> thinking` blockquote feel.
    await assertId(
      page,
      'pres.thinking-header-icon',
      () => {
        const t = document.querySelector('.thinking');
        if (!t) return false;
        const summary = t.querySelector('.thinking__summary');
        if (!summary) return false;
        if ((summary.textContent || '').trim() !== 'Thinking') return false;
        return t.querySelector('svg.thinking__icon') !== null;
      },
      'thinking summary is not brain icon + "Thinking"',
      15000,
    );
    await assertId(
      page,
      'pres.thinking-no-bare-marker',
      () => {
        const t = document.querySelector('.thinking');
        if (!t) return false;
        // No bare `thinking` label and no `>`-prefixed blockquote marker
        // anywhere in the thinking block's visible text.
        const txt = (t.textContent || '').toLowerCase();
        if (txt.includes('> thinking')) return false;
        const summary = t.querySelector('.thinking__summary');
        if (!summary) return false;
        const label = (summary.textContent || '').trim();
        if (label !== 'Thinking' && label !== 'THINKING') return false;
        return true;
      },
      'thinking block still shows a bare `> thinking` marker',
      15000,
    );
    // Body contract: the mock's literal `\n` sequences must render as REAL
    // multiple lines (≥3), with no literal backslash-n left on the line.
    await assertId(
      page,
      'pres.thinking-multiline-body',
      () => {
        const t = document.querySelector('.thinking');
        if (!t || t.hidden !== false) return false;
        const body = t.querySelector('.thinking__body');
        if (!body) return false;
        const text = body.textContent || '';
        if (text.includes('\\n')) return false;
        return text.split('\n').length >= 3;
      },
      'thinking body did not render real multiple lines from literal \\n',
      15000,
    );
    await waitForSettled(page);
    // After completion the final assistant turn must contain NO thinking block.
    await assertId(
      page,
      'pres.thinking-final-hidden',
      () => {
        const nodes = [...document.querySelectorAll('.msg--assistant')];
        if (nodes.length === 0) return false;
        const last = nodes[nodes.length - 1];
        return last.querySelector('.thinking') === null;
      },
      'completed assistant turn still contains a thinking block',
    );

    // ============ Thinking + streamed tool call — no raw JSON ever ============
    // Regression guard for the removed `.assistant-toolcall` raw-JSON surface:
    // a turn that streams reasoning AND a bash tool call (incremental
    // tool_calls argument fragments, one toolcall_delta per chunk) must never
    // render the raw `{"command":…}` JSON / command args into the user DOM —
    // neither while the stream is in flight, after finalize, nor as a
    // transient element. The structured Command card (driven by the
    // tool_execution_* events) is the ONLY tool presentation. A MutationObserver
    // watchdog installed BEFORE the prompt records any `.assistant-toolcall`
    // element or raw-JSON text that appears at any point in the turn, so the
    // assertion is timing-proof even for a brief mid-stream flash.
    await page.evaluate(() => {
      window.__thinkBashWatch = { toolcallDivs: 0, rawJsonText: 0 };
      new MutationObserver((mutations) => {
        for (const m of mutations) {
          if (m.type === 'childList') {
            for (const n of m.addedNodes) {
              if (n.nodeType !== 1 || !n.matches) continue;
              if (n.matches('.assistant-toolcall') || (n.querySelector && n.querySelector('.assistant-toolcall'))) {
                window.__thinkBashWatch.toolcallDivs += 1;
              }
            }
          } else if (m.type === 'characterData') {
            const v = m.target.nodeValue || '';
            if (v.includes('{"command"') || v.includes('"command":')) {
              window.__thinkBashWatch.rawJsonText += 1;
            }
          }
        }
      }).observe(document.body, { childList: true, subtree: true, characterData: true });
    });
    await sendPrompt(page, 'presentation think bash');
    // Thinking block streams open with the reasoning prose (never raw JSON).
    await assertId(
      page,
      'pres.tb.thinking-prose',
      () => {
        const t = document.querySelector('.thinking');
        if (!t || t.hidden !== false) return false;
        const body = t.querySelector('.thinking__body');
        if (!body) return false;
        const text = body.textContent || '';
        return text.includes('planning tool use') && text.includes('think-bash thinking');
      },
      'thinking block did not show the reasoning prose during the think+bash stream',
      20000,
    );
    // While the Command card is live and the turn still streams: no
    // `.assistant-toolcall` element, no raw JSON/args anywhere in the
    // transcript, and the structured card carries the Command title.
    await assertId(
      page,
      'pres.tb.card-live-no-raw',
      () => {
        const badge = document.getElementById('stream-badge');
        if (!badge || badge.hidden !== false) return false;
        if (document.querySelector('#transcript .assistant-toolcall') !== null) return false;
        const txt = (document.getElementById('transcript')?.textContent || '');
        if (txt.includes('{"command"') || txt.includes('"command":')) return false;
        const cards = [...document.querySelectorAll('.tool-card--command')];
        const card = cards[cards.length - 1];
        if (!card) return false;
        if ((card.querySelector('.tool-card__title')?.textContent || '') !== 'Command') return false;
        const status = card.getAttribute('data-tool-status');
        return status === 'running' || status === 'done';
      },
      'raw tool-call JSON appeared in the transcript while the Command card was live',
      25000,
    );
    await waitForSettled(page);
    // After the turn: structured card done, follow-up text round-tripped, and
    // the watchdog never saw a raw-JSON surface at any point in the turn.
    await assertId(
      page,
      'pres.tb.card-done-no-raw',
      () => {
        const cards = [...document.querySelectorAll('.tool-card--command')];
        const card = cards[cards.length - 1];
        if (!card) return false;
        if (card.getAttribute('data-tool-status') !== 'done') return false;
        if ((card.querySelector('.tool-card__title')?.textContent || '') !== 'Command') return false;
        if (card.querySelector('.tool-card__args') !== null || card.querySelector('.tool-card__raw') !== null) return false;
        const txt = (document.getElementById('transcript')?.textContent || '');
        if (txt.includes('{"command"') || txt.includes('"command":')) return false;
        if (!txt.includes('think bash complete')) return false;
        const watch = window.__thinkBashWatch || {};
        return watch.toolcallDivs === 0 && watch.rawJsonText === 0;
      },
      'raw JSON leaked during the think+bash turn or the structured card is missing',
      15000,
    );

    // ============ Thinking — 390px viewport: long/CJK lines wrap, no overflow ============
    {
      const narrow = await browser.newPage({ viewport: { width: 390, height: 844 } });
      await connectPage(narrow);
      await sendPrompt(narrow, 'presentation thinking turn');
      // The long unbroken run in the reasoning body must WRAP (overflow-wrap:
      // anywhere), so neither the document nor the thinking body's own box
      // overflows the 390px viewport while the block streams.
      await assertId(
        narrow,
        'pres.thinking-narrow-no-overflow',
        () => {
          const t = document.querySelector('.thinking');
          if (!t || t.hidden !== false) return false;
          const body = t.querySelector('.thinking__body');
          if (!body || (body.textContent || '').length === 0) return false;
          // Body-level: the long token wraps INSIDE the box (scrollWidth must
          // not exceed clientWidth) instead of widening the layout.
          if (body.scrollWidth > body.clientWidth + 1) return false;
          // Document-level: the page itself stays inside the 390px viewport.
          if (document.documentElement.scrollWidth > window.innerWidth + 1) return false;
          const rect = body.getBoundingClientRect();
          return rect.left >= -1 && rect.right <= window.innerWidth + 1;
        },
        'thinking body overflows a 390px viewport (long line did not wrap)',
        20000,
      );
      await waitForSettled(narrow);
      await narrow.close();
    }

    // ============ durable bashExecution (RPC → live .msg--bash) ============
    // The durable `execute_bash` RPC publishes a BashExecutionEnd session
    // event the web client renders live as a .msg--bash card (success →
    // msg--bash--done, failure → msg--bash--error), distinct from the bash
    // tool's Command card. Route the RPC to the page's ACTIVE session (read
    // from the sidebar) so the event lands in the active view — a bare RPC
    // targets the listener primary runtime, which may be a background session.
    const activeSid = await page.evaluate(() => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '');
    if (!activeSid) fail('no active session id found in the sidebar for the durable bash RPC');
    await rpc.call({ type: 'bash', sessionId: activeSid, command: 'echo durable-bash-ok', excludeFromContext: false });
    await assertId(
      page,
      'pres.bash-execution-durable',
      () => {
        const cards = [...document.querySelectorAll('.msg--bash')];
        const card = cards[cards.length - 1];
        if (!card) return false;
        if (card.getAttribute('data-bash-status') !== 'done') return false;
        return card.classList.contains('msg--bash--done');
      },
      'durable bashExecution never rendered a .msg--bash done card',
    );

    // ============ session sidebar — providers + no tmp/UUID + search ============
    await waitFor(page, () => document.getElementById('session-sidebar') !== null, 'session sidebar did not render');
    await waitFor(page, () => document.querySelectorAll('.session-sidebar__row').length >= 1, 'session sidebar never listed rows');
    await assertId(
      page,
      'pres.session-providers-only',
      () => {
        const heads = [...document.querySelectorAll('.session-sidebar__group-head[data-group-kind="provider"]')];
        if (heads.length === 0) return false;
        const allowed = new Set(['rpi', 'codex', 'grok', 'omp']);
        return heads.every((h) => {
          const label = (h.querySelector('.session-sidebar__group-label')?.textContent || '').trim();
          const attr = (h.getAttribute('data-provider') || '').trim();
          return allowed.has(label.toLowerCase()) || allowed.has(attr.toLowerCase());
        });
      },
      'a top-level provider group is not one of rpi/Codex/Grok/OMP',
    );
    await assertId(
      page,
      'pres.session-no-tmp-uuid',
      () => {
        const heads = [...document.querySelectorAll('.session-sidebar__group-head[data-group-kind="provider"]')];
        return heads.every((h) => {
          const label = (h.querySelector('.session-sidebar__group-label')?.textContent || '').trim();
          return !/tmp/i.test(label) && !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(label);
        });
      },
      'a tmp/UUID top-level provider group appeared',
    );
    // Search filters rows.
    const rowsBefore = await page.evaluate(() => [...document.querySelectorAll('.session-sidebar__row')].filter((r) => r.offsetParent !== null).length);
    await page.fill('#session-sidebar-search', 'OMP external');
    await assertId(
      page,
      'pres.session-search-filter',
      (before) => [...document.querySelectorAll('.session-sidebar__row')].filter((r) => r.offsetParent !== null).length < before,
      'search query did not reduce the visible session rows',
      15000,
      rowsBefore,
    );
    // Clear restores the full list.
    await page.click('#session-sidebar-search-clear');
    await assertId(
      page,
      'pres.session-search-clear',
      (before) => [...document.querySelectorAll('.session-sidebar__row')].filter((r) => r.offsetParent !== null).length >= before,
      'search clear did not restore the full session list',
      15000,
      rowsBefore,
    );

    // ============ media video (controls + preload + no autoplay) ============
    await sendPrompt(page, 'presentation media video');
    await assertId(
      page,
      'pres.video-controls',
      () => {
        const cards = [...document.querySelectorAll('.tool-card--read')];
        const card = cards[cards.length - 1];
        if (!card) return false;
        const v = card.querySelector('video.tool-media__video');
        if (!v) return false;
        if (!v.controls) return false;
        if ((v.getAttribute('preload') || '') !== 'metadata') return false;
        // No autoplay: the element must be paused with no autoplay attr.
        if (v.autoplay) return false;
        return v.paused;
      },
      'media video did not render with controls + preload=metadata + no autoplay',
    );
    await waitForSettled(page);

    // ============ hostile media rejected (no image / no video) ============
    await sendPrompt(page, 'presentation media hostile');
    await waitForSettled(page);
    await assertId(
      page,
      'pres.media-hostile-rejected',
      () => {
        const cards = [...document.querySelectorAll('.tool-card--read')];
        const card = cards[cards.length - 1];
        if (!card) return false;
        return card.querySelector('.tool-media__image') === null && card.querySelector('.tool-media__video') === null;
      },
      'hostile media was rendered instead of rejected',
    );

    // ============ composer controls (mobile viewport) ============
    // At phone width the textarea occupies its own row, so the three controls
    // are not on the same baseline; assert the two action buttons share one
    // height and the textarea keeps a usable width (per the mobile contract).
    const mobile = await browser.newPage({ viewport: { width: 375, height: 667 } });
    await connectPage(mobile);
    await assertId(
      mobile,
      'pres.composer-equal-height-mobile',
      () => {
        const btn = document.getElementById('command-btn');
        const send = document.getElementById('send-btn');
        const ta = document.getElementById('prompt-input');
        if (!btn || !send || !ta) return false;
        const bh = btn.getBoundingClientRect().height;
        const sh = send.getBoundingClientRect().height;
        if (Math.abs(bh - sh) > 1) return false;
        return ta.getBoundingClientRect().width >= 240;
      },
      'mobile composer buttons differ in height by >1px or textarea width <240px',
    );

    await fs.promises.mkdir(evidence, { recursive: true });
    fs.writeFileSync(
      path.join(evidence, 'coverage-assertions.json'),
      JSON.stringify({ executed: [...executed].sort() }, null, 2),
    );
    console.log(`web-pres: PASS — ${executed.size} assertions`);
  } finally {
    if (rpc) {
      try {
        rpc.close();
      } catch {
        /* ignore */
      }
    }
    await browser.close();
  }
}

main().catch((err) => {
  console.error(`web-pres: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});