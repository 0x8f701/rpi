// Abort semantics web E2E lane (playwright half of E2E.d/web/abort.sh).
//
// Environment:
//   RPI_URL          http://127.0.0.1:<port>/web
//   RPI_TOKEN        token file content (served via rpi-auth.<token> subprotocol)
//   RPI_SLOW_PREFIX  first chunk of the slow mock stream ("steer-1-")
//   RPI_ABORTED_TAIL final chunk that must NEVER render after abort ("-done")
//   RPI_FAST_REPLY   instant mock reply text ("steering-followup-reply")
//   RPI_CHROME       executable path of the system Chrome (optional)
//   RPI_EVIDENCE     evidence dir for screenshots
//
// Regression guards (B1/B2 web semantics):
//   1. B1 — an EARLY abort (immediately after the first delta lands) must
//      PRESERVE the streamed text in the transcript (the partial assistant
//      message stays) while the final chunks never render.
//   2. B2 — aborting surfaces a NEUTRAL toast ("run aborted"), never an
//      error toast (the abort is user-initiated, not a failure).
//   3. the composer recovers: the next prompt round-trips normally.

import { chromium } from 'playwright';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const slowPrefix = process.env.RPI_SLOW_PREFIX || 'steer-1-';
const abortedTail = process.env.RPI_ABORTED_TAIL || '-done';
const fastReply = process.env.RPI_FAST_REPLY || 'steering-followup-reply';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const pageErrors = [];
const consoleErrors = [];

function parseFrame(payload) {
  try {
    return JSON.parse(payload);
  } catch {
    return null;
  }
}

async function installSocketHarness(page) {
  await page.addInitScript(() => {
    const NativeWebSocket = window.WebSocket;
    const frames = { sent: [], received: [] };
    class TracedWebSocket extends NativeWebSocket {
      constructor(...args) {
        super(...args);
        this.addEventListener('message', (event) => {
          if (typeof event.data === 'string') frames.received.push(event.data);
        });
      }
      send(data) {
        if (typeof data === 'string') frames.sent.push(data);
        return super.send(data);
      }
    }
    for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
      Object.defineProperty(TracedWebSocket, key, { value: NativeWebSocket[key] });
    }
    window.WebSocket = TracedWebSocket;
    window.__rpiAbortFrames = frames;
  });
}

async function waitForFrame(page, kind, expected, label, timeoutMs = 25000) {
  await waitFor(
    page,
    ({ kind: frameKind, expectedFields }) => window.__rpiAbortFrames[frameKind]
      .map((raw) => { try { return JSON.parse(raw); } catch { return null; } })
      .some((frame) => frame && Object.entries(expectedFields).every(([key, value]) => frame[key] === value)),
    label,
    timeoutMs,
    { kind, expectedFields: expected },
  );
  const raw = await page.evaluate(
    ({ kind: frameKind, expectedFields }) => window.__rpiAbortFrames[frameKind].find((candidate) => {
      try {
        const frame = JSON.parse(candidate);
        return Object.entries(expectedFields).every(([key, value]) => frame[key] === value);
      } catch {
        return false;
      }
    }),
    { kind, expectedFields: expected },
  );
  return parseFrame(raw);
}

function fail(message) {
  // Exit 2 (not 1): 1 means "playwright setup failure (node/chromium/npm)",
  // which must NOT be confused with an assertion failure (the mock's request
  // counter is stateful and a rerun would see shifted replies).
  console.error(`web-abort: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage();
    await installSocketHarness(page);
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    page.on('pageerror', (err) => pageErrors.push(err.message));
    page.on('console', (message) => {
      if (message.type() === 'error') consoleErrors.push(message.text());
    });
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'WS did not reach "connected"'
    );

    // Request 1 in the steering mock is the ~3.6s slow stream.
    await page.fill('#prompt-input', 'write a very long answer');
    await page.press('#prompt-input', 'Enter');
    const promptFrame = await waitForFrame(page, 'sent', { type: 'prompt' }, 'typed prompt frame missing');
    if (!promptFrame.id || !promptFrame.sessionId) fail(`typed prompt identity missing: ${JSON.stringify(promptFrame)}`);
    const promptResponse = await waitForFrame(page, 'received', { type: 'response', id: promptFrame.id }, 'typed prompt response missing');
    if (promptResponse.success !== true || promptResponse.command !== 'prompt' || promptResponse.sessionId !== promptFrame.sessionId) {
      fail(`typed prompt response authority mismatch: ${JSON.stringify(promptResponse)}`);
    }
    const turnId = promptResponse.data?.turnId;
    if (typeof turnId !== 'string' || !turnId) fail(`typed prompt response omitted turnId: ${JSON.stringify(promptResponse)}`);
    await waitForFrame(page, 'received', { type: 'turn_started', sessionId: promptFrame.sessionId, turnId }, 'authoritative turn_started missing');
    await waitFor(
      page,
      (prefix) => document.body.textContent.includes(prefix),
      'slow stream never started (first delta missing)',
      30000,
      slowPrefix,
    );

    // EARLY abort: the unified composer action changes to Stop as soon as the
    // first delta is visible. The next chunks arrive 0.6s apart.
    await waitFor(page, () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Stop generating', 'unified action never switched to Stop');
    await page.screenshot({ path: `${evidence}/abort-active.png`, fullPage: true });
    await page.click('#send-btn');
    const abortFrame = await waitForFrame(page, 'sent', { type: 'abort', sessionId: promptFrame.sessionId }, 'typed abort frame missing');
    if (!abortFrame.id) fail(`typed abort request id missing: ${JSON.stringify(abortFrame)}`);
    const abortResponse = await waitForFrame(page, 'received', { type: 'response', id: abortFrame.id }, 'typed abort response missing');
    if (abortResponse.success !== true || abortResponse.command !== 'abort' || abortResponse.sessionId !== promptFrame.sessionId) {
      fail(`typed abort response authority mismatch: ${JSON.stringify(abortResponse)}`);
    }
    const terminalFrame = await waitForFrame(
      page,
      'received',
      { type: 'turn_terminal', sessionId: promptFrame.sessionId, turnId, status: 'aborted' },
      'authoritative aborted turn_terminal missing',
    );
    if (terminalFrame.message !== null && typeof terminalFrame.message !== 'string') {
      fail(`turn_terminal message shape invalid: ${JSON.stringify(terminalFrame)}`);
    }
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message',
      'composer did not return to Send after abort',
    );
    const preserved = await page.evaluate(() => {
      const nodes = document.querySelectorAll('.msg--assistant .assistant-text');
      return nodes.length ? nodes[nodes.length - 1].textContent : '';
    });
    if (!preserved.includes(slowPrefix)) {
      fail(`B1: aborted message lost the streamed text: "${preserved}"`);
    }
    if (preserved.includes(abortedTail)) {
      fail(`B1: aborted message rendered chunks past the abort: "${preserved}"`);
    }
    await page.screenshot({ path: `${evidence}/abort-early.png`, fullPage: true });

    // B2: the abort produced a NEUTRAL toast — "run aborted", no error class.
    await waitFor(
      page,
      () => Array.from(document.querySelectorAll('#toasts .toast')).some((t) => t.textContent.includes('run aborted')),
      'neutral "run aborted" toast never appeared'
    );
    const toastState = await page.evaluate(() => {
      const toasts = Array.from(document.querySelectorAll('#toasts .toast'));
      return {
        neutral: toasts.some((t) => t.textContent.includes('run aborted')),
        neutralIsError: toasts.some((t) => t.textContent.includes('run aborted') && t.classList.contains('toast--error')),
        anyError: toasts.some((t) => t.classList.contains('toast--error')),
      };
    });
    if (!toastState.neutral) fail('B2: "run aborted" toast missing');
    if (toastState.neutralIsError) fail('B2: abort toast rendered as an ERROR toast');
    if (toastState.anyError) fail('B2: an unexpected error toast appeared alongside the abort');
    await page.screenshot({ path: `${evidence}/abort-toast.png`, fullPage: true });

    // Recovery: request 2 in the mock is instant again.
    await page.fill('#prompt-input', 'recovery prompt');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (reply) => document.body.textContent.includes(reply),
      'post-abort prompt did not round-trip',
      30000,
      fastReply
    );

    if (pageErrors.length || consoleErrors.length) {
      fail(`browser errors: ${JSON.stringify({ pageErrors, consoleErrors })}`);
    }
    await page.screenshot({ path: `${evidence}/abort-recovered.png`, fullPage: true });
    console.log(`web-abort: PASSED (prompt=${promptFrame.id} turn=${turnId} abort=${abortFrame.id}; early-abort preserves text + neutral toast + recovery)`);
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-abort: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
