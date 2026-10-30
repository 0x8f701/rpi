// Web code-review REPLY LIFECYCLE regression (playwright half of
// code_review_reply_lifecycle.sh).
//
// Ticket: "Code Review reply terminal state clears activity" — trace one
// authoritative identity chain (review id, thread/hunk id, reply id, tool
// call id, request generation) from comment submit through start, streaming,
// tool call, and terminal events, and prove every terminal path converges to
// the same projection exactly once:
//   B1 normal slow completion (Abort appears, then clears with the reply
//      committed exactly once)
//   B2 next prompt after completion (Comment/submit restored)
//   B3 completion after a real grep tool call (toolCallId path)
//   B4 user abort mid-stream (Abort settles terminal and restores submit)
//   B5 ordinary mid-stream lifecycle convergence: a later comment streams
//      again and settles with the same terminal projection
//   B6 zero browser console/page errors
// Backend review/reply/toolCall identities are captured from the live WS
// frames and recorded in the report (no fixed sleeps anywhere: every wait is
// waitForFunction with a deadline; any assertion failure exits nonzero).
import { chromium } from 'playwright';
import fs from 'node:fs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const DIRTY_FILE = process.env.RPI_DIRTY_FILE || 'greet.txt';

function fail(message) {
  console.error(`web-code-review-reply-lifecycle: FAIL: ${message}`);
  process.exit(2);
}

/** Event-gated wait: page.waitForFunction with a deadline (no fixed sleeps). */
async function waitFor(page, fn, label, timeoutMs = 20000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs, polling: 25 });
  } catch {
    const dump = await page
      .evaluate(() => ({
        panel: !!document.getElementById('code-review-panel'),
        body: (document.body?.textContent || '').slice(0, 300),
      }))
      .catch(() => ({ panel: false, body: 'page-eval-failed' }));
    fail(`${label} (timeout after ${timeoutMs}ms); page=${JSON.stringify(dump)}`);
  }
}

const lifecycleState = () => ({
  abort: !!document.querySelector('.code-review__action--stop'),
  composerDisabled: document.querySelector('.code-review__comment-input')?.disabled ?? null,
  headerActiveLabel: document.querySelector('.code-review__streaming')?.textContent ?? null,
  hunkStreamingDot: !!document.querySelector(
    '.code-review__hunk-header-row .code-review__streaming-dot',
  ),
  assistantCards: document.querySelectorAll('.code-review__comment--assistant').length,
  threadError: document.querySelector('.code-review__error')?.textContent ?? null,
  panelOpen: !!document.getElementById('code-review-panel'),
});

/** In-page get_state through the live wrapper socket; resolves on the RPC
 *  response frame (chained onmessage handler, event-gated). */
function rpcGetState(page, label, sessionId) {
  return page.evaluate(
    ({ tag, sessionId: sid }) =>
      new Promise((resolve, reject) => {
        const ws = window.__replyLifecycleSocket?.();
        if (!ws || ws.readyState !== window.WebSocket.OPEN) {
          reject(new Error(`${tag}: no live wrapper socket`));
          return;
        }
        const id = `lifecycle-${tag}-${Date.now()}`;
        const timer = setTimeout(() => reject(new Error(`${tag}: get_state timeout`)), 10000);
        const onMessage = (ev) => {
          let parsed = null;
          try {
            parsed = JSON.parse(ev.data);
          } catch {
            return;
          }
          if (!parsed || parsed.type !== 'response' || parsed.id !== id) return;
          ws.removeEventListener('message', onMessage);
          clearTimeout(timer);
          resolve({
            success: parsed.success === true,
            sessionId: parsed.sessionId,
            error: parsed.error ?? null,
            command: parsed.command ?? null,
          });
        };
        ws.addEventListener('message', onMessage);
        const frame = sid
          ? { type: 'get_state', id, sessionId: sid }
          : { type: 'get_state', id };
        ws.send(JSON.stringify(frame));
      }),
    { tag: label, sessionId: sessionId ?? null },
  );
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage();
    const pageErrors = [];
    const consoleErrors = [];
    const backend = { comments: [], snapshots: [], frames: 0 };
    page.on('pageerror', (err) => pageErrors.push(String(err && err.message ? err.message : err)));
    page.on('console', (msg) => {
      if (msg.type() === 'error') consoleErrors.push(msg.text());
    });
    // Wrap every socket from page load: expose the live socket for in-page
    // RPC and capture code_review_* response identities from the wire.
    await page.addInitScript(() => {
      const NativeWebSocket = window.WebSocket;
      const state = { ws: null, generation: 0 };
      class ObservedWebSocket extends NativeWebSocket {
        constructor(...args) {
          super(...args);
          state.ws = this;
          this.addEventListener('message', (ev) => {
            try {
              const parsed = typeof ev.data === 'string' ? JSON.parse(ev.data) : null;
              if (!parsed) return;
              if (String(parsed.command || '').startsWith('code_review_')) {
                window.__replyLifecycleFrames = window.__replyLifecycleFrames || [];
                window.__replyLifecycleFrames.push({
                  command: parsed.command,
                  success: parsed.success === true,
                  thread: parsed.data?.threads?.[0]
                    ? {
                        replyId: parsed.data.threads[0].replyId ?? null,
                        generation: parsed.data.threads[0].generation ?? null,
                        replyStatus: parsed.data.threads[0].replyStatus ?? null,
                        toolCalls: (parsed.data.threads[0].toolCalls || []).map((t) => t.id),
                      }
                    : null,
                  reviewId: parsed.data?.reviewId ?? null,
                });
              }
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
      window.__replyLifecycleSocket = () => state.ws;
    });
    if (token) {
      await page.addInitScript((t) => {
        // Current App stores per-authority (rpi-web-token:<host>); the legacy
        // global key is migrated on boot. Set both so either contract works.
        window.localStorage.setItem('rpi-web-token', t);
        window.localStorage.setItem(`rpi-web-token:${window.location.host}`, t);
      }, token);
    }

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    // A1 bootstrap: fixture session is the get_state owner.
    await waitFor(
      page,
      () => !!document.querySelector('.session-sidebar__row--active .session-sidebar__switch')
        ?.dataset.sessionId,
      'A1: primary session row did not bind after bootstrap',
      30000,
    );
    const activeSessionId = await page.evaluate(() =>
      document.querySelector('.session-sidebar__row--active .session-sidebar__switch')
        ?.dataset.sessionId || '',
    );
    if (!activeSessionId) fail('A1: no active fixture session id');
    const a1 = await rpcGetState(page, 'a1', activeSessionId).catch((err) => fail(`A1: ${err.message}`));
    if (!a1.success) fail(`A1: get_state failed: ${JSON.stringify(a1)}`);
    if (a1.sessionId !== activeSessionId) {
      fail(`A1: get_state owner does not match the fixture session (owner=${a1.sessionId}, fixture=${activeSessionId})`);
    }
    await page.screenshot({ path: `${evidence}/01-bootstrap.png`, fullPage: true });

    // A2: open the review, select the dirty file + hunk, composer ready.
    await page.fill('#prompt-input', '/code-review');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') !== null,
      'A2: code-review panel did not open on Enter',
    );
    await waitFor(
      page,
      () => {
        const label = document.querySelector('.code-review__label')?.textContent || '';
        return label.includes('HEAD') && label.includes('working tree');
      },
      'A2: comparison label missing',
      30000,
    );
    await waitFor(
      page,
      (dirty) => !!document.querySelector(`button.code-review__file[data-file-path="${dirty}"]`),
      'A2: controlled dirty file did not appear as a selectable diff row',
      30000,
      DIRTY_FILE,
    );
    await page.click(`button.code-review__file[data-file-path="${DIRTY_FILE}"]`);
    await waitFor(
      page,
      () => !!document.querySelector('button.code-review__hunk-header:not([disabled])'),
      'A2: hunk never became selectable',
      30000,
    );
    await page.click('button.code-review__hunk-header');
    await waitFor(
      page,
      () => !!document.querySelector('textarea.code-review__comment-input'),
      'A2: hunk composer never mounted',
    );
    await page.screenshot({ path: `${evidence}/02-review-open.png`, fullPage: true });

    const report = { activeSessionId, phases: [] };
    const submitComment = async (text) => {
      await page.fill('textarea.code-review__comment-input', text);
      await page.click(
        '.code-review__composer-actions button.code-review__action:not(.code-review__action--stop)',
      );
    };

    // ---- B1: normal slow completion settles exactly once ----------------
    await submitComment('review markdown matrix phase B1');
    await waitFor(
      page,
      () => !!document.querySelector('.code-review__action--stop'),
      'B1: Abort never appeared while the reply streamed',
      10000,
    );
    const b1Streaming = await page.evaluate(lifecycleState);
    report.phases.push({ phase: 'B1-streaming', ...b1Streaming });
    if (!b1Streaming.hunkStreamingDot) fail(`B1: per-hunk streaming dot missing while streaming: ${JSON.stringify(b1Streaming)}`);
    const stopCopy = await page.evaluate(() => {
      const btn = document.querySelector('.code-review__action--stop');
      return {
        text: (btn?.textContent || '').replace(/\s+/g, ' ').trim(),
        aria: btn?.getAttribute('aria-label') || '',
        title: btn?.getAttribute('title') || '',
      };
    });
    if (!stopCopy.text.includes('Stop') || stopCopy.text.includes('Abort')
      || !stopCopy.aria.includes('Stop') || stopCopy.aria.includes('Abort')) {
      fail(`B1: interrupt control is not Stop: ${JSON.stringify(stopCopy)}`);
    }
    await page.screenshot({ path: `${evidence}/03-B1-streaming.png`, fullPage: true });
    await waitFor(
      page,
      () => !document.querySelector('.code-review__action--stop')
        && document.querySelectorAll('.code-review__comment--assistant').length >= 1,
      'B1: reply never settled (Abort cleared + assistant card committed)',
      30000,
    );
    const b1Settled = await page.evaluate(lifecycleState);
    report.phases.push({ phase: 'B1-settled', ...b1Settled });
    if (b1Settled.composerDisabled) fail(`B1: composer stayed disabled after completion: ${JSON.stringify(b1Settled)}`);
    if (b1Settled.hunkStreamingDot) fail(`B1: streaming dot survived completion: ${JSON.stringify(b1Settled)}`);
    if (b1Settled.headerActiveLabel && b1Settled.headerActiveLabel.includes('repl')) {
      fail(`B1: header active-replies label survived completion: ${JSON.stringify(b1Settled)}`);
    }
    if (b1Settled.assistantCards !== 1) fail(`B1: expected exactly 1 assistant reply card, got ${b1Settled.assistantCards}`);
    await page.screenshot({ path: `${evidence}/04-B1-settled.png`, fullPage: true });

    // ---- B2: next prompt after completion -------------------------------
    await submitComment('review markdown matrix phase B2');
    await waitFor(
      page,
      () => document.querySelectorAll('.code-review__comment--assistant').length >= 2,
      'B2: second reply never committed',
      30000,
    );
    const b2Settled = await page.evaluate(lifecycleState);
    report.phases.push({ phase: 'B2-settled', ...b2Settled });
    if (b2Settled.composerDisabled) fail(`B2: composer disabled after second reply: ${JSON.stringify(b2Settled)}`);
    if (b2Settled.abort) fail(`B2: Abort resurfaced after terminal: ${JSON.stringify(b2Settled)}`);

    // ---- B3: completion after a real grep tool call ----------------------
    await submitComment('cr grep probe phase B3');
    await waitFor(
      page,
      () => {
        const texts = Array.from(document.querySelectorAll('.code-review__comment-text'))
          .map((el) => el.textContent || '');
        return texts.some((t) => t.includes('cr-grep-echo:'));
      },
      'B3: grep tool echo reply never committed',
      30000,
    );
    const b3Settled = await page.evaluate(lifecycleState);
    report.phases.push({ phase: 'B3-settled', ...b3Settled });
    if (b3Settled.composerDisabled) fail(`B3: composer disabled after tool completion: ${JSON.stringify(b3Settled)}`);
    if (b3Settled.abort) fail(`B3: Abort survived tool completion: ${JSON.stringify(b3Settled)}`);
    if (b3Settled.hunkStreamingDot) fail(`B3: streaming dot survived tool completion: ${JSON.stringify(b3Settled)}`);
    await page.screenshot({ path: `${evidence}/05-B3-tool-settled.png`, fullPage: true });

    // ---- B4: user abort mid-stream settles terminal and restores submit -
    await submitComment('review markdown matrix phase B4');
    await waitFor(
      page,
      () => !!document.querySelector('.code-review__action--stop'),
      'B4: Abort never appeared for the slow stream',
      10000,
    );
    await page.click('.code-review__action--stop');
    await waitFor(
      page,
      () => !document.querySelector('.code-review__action--stop'),
      'B4: Abort never cleared after user abort',
      15000,
    );
    const b4Settled = await page.evaluate(lifecycleState);
    report.phases.push({ phase: 'B4-settled', ...b4Settled });
    if (b4Settled.composerDisabled) fail(`B4: composer disabled after abort: ${JSON.stringify(b4Settled)}`);
    if (b4Settled.hunkStreamingDot) fail(`B4: streaming dot survived abort: ${JSON.stringify(b4Settled)}`);
    await page.screenshot({ path: `${evidence}/06-B4-abort-settled.png`, fullPage: true });
    // The NEXT comment must still run after the abort.
    await page.fill('textarea.code-review__comment-input', 'plain follow-up after abort');
    await page.click(
      '.code-review__composer-actions button.code-review__action:not(.code-review__action--stop)',
    );
    await waitFor(
      page,
      () => {
        const texts = Array.from(document.querySelectorAll('.code-review__comment-text'))
          .map((el) => el.textContent || '');
        return texts.some((t) => t.includes('steering-followup-reply'));
      },
      'B4: post-abort comment never completed',
      30000,
    );

    // ---- B5: ordinary mid-stream lifecycle convergence ------------------
    await submitComment('review markdown matrix phase B5');
    await waitFor(
      page,
      () => !!document.querySelector('.code-review__action--stop'),
      'B5: Abort never appeared while the reply streamed',
      10000,
    );
    // The lifecycle must converge to the same terminal projection one more
    // time: Abort clears, composer restores, panel stays open.
    await waitFor(
      page,
      () => !document.querySelector('.code-review__action--stop')
        && document.querySelector('.code-review__comment-input')?.disabled === false,
      'B5: mid-stream lifecycle did not converge',
      30000,
    );
    const b5Settled = await page.evaluate(lifecycleState);
    report.phases.push({ phase: 'B5-settled', ...b5Settled });
    if (b5Settled.composerDisabled) fail(`B5: composer disabled after mid-stream convergence: ${JSON.stringify(b5Settled)}`);
    if (b5Settled.hunkStreamingDot) fail(`B5: streaming dot survived mid-stream convergence: ${JSON.stringify(b5Settled)}`);
    if (!b5Settled.panelOpen) fail('B5: review panel closed before mid-stream convergence');
    await page.screenshot({ path: `${evidence}/07-B5-midstream-settled.png`, fullPage: true });

    // ---- B6: zero browser console/page errors ---------------------------
    if (pageErrors.length) fail(`B6: page errors: ${JSON.stringify(pageErrors)}`);
    const clean = consoleErrors.filter((t) => !t.includes('favicon'));
    if (clean.length) fail(`B6: console errors: ${JSON.stringify(clean)}`);

    // Captured backend identities from the wire frames.
    const frames = await page.evaluate(() => window.__replyLifecycleFrames || []);
    report.backendFrames = frames.slice(-8);
    report.reviewIds = Array.from(new Set(frames.map((f) => f.reviewId).filter(Boolean)));
    report.replyIds = Array.from(new Set(frames.map((f) => f.thread?.replyId).filter(Boolean)));
    report.toolCallIds = Array.from(
      new Set(frames.flatMap((f) => f.thread?.toolCalls || []).filter(Boolean)),
    );
    if (report.replyIds.length === 0) fail('B6: no backend replyId captured from code_review_* frames');
    if (report.toolCallIds.length === 0) fail('B6: no backend toolCallId captured from the grep probe');
    report.pageErrors = pageErrors;
    report.consoleErrors = clean;
    fs.writeFileSync(
      `${evidence}/code-review-reply-lifecycle-report.json`,
      JSON.stringify(report, null, 2),
    );
    console.log(
      'web-code-review-reply-lifecycle: PASSED (A1 fixture owner; A2 review+hunk composer; B1 slow reply settles exactly once with Abort/dot/label cleared; B2 next prompt; B3 tool-completion convergence; B4 abort settles terminal + submit restored; B5 later mid-stream comment converges the same way; B6 zero browser errors)',
    );
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-code-review-reply-lifecycle: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(3);
});