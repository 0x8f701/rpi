// Hard-coverage matrix driver — exercises every required Web contract against
// the REAL `rpi --listen` fixture (loopback steering mock + token file +
// RPI_WEB_DEV_DIR coverage bundle) with concrete, machine-readable assertion
// IDs, collecting V8 coverage on every page via the coverage hook
// (E2E.d/web/lib/coverage-hook.mjs, loaded with node --import).
//
// Environment:
//   RPI_URL            http://127.0.0.1:<port>/web
//   RPI_TOKEN          token file content (rpi-auth.<token> subprotocol)
//   RPI_WRONG_TOKEN    token that must be rejected (default wrong-token-abc)
//   RPI_CHROME         system Chrome executable (optional)
//   RPI_EVIDENCE       evidence dir (coverage-assertions.json is written here)
//   RPI_WORK           work dir for the reconnect kill/respawn marker files
//   RPI_FAST_REPLY     instant mock reply ("steering-followup-reply")
//   RPI_SLOW_TAIL      tail of the slow mock stream ("chunk-four-done")
//
// Exit: 0 = every assertion passed + evidence written; 2 = assertion failure;
//       1 = setup failure (playwright unusable).
//
// The steering mock contract (E2E.d/lib/user_mock_server.py):
//   odd request  -> slow stream "steer-<N>-...-done" (~4s)
//   even request -> instant "steering-followup-reply"
//   "render rich content" in the prompt -> RICH_TEXT (table/task-list/mermaid/KaTeX)
//   "web-e2e-subagent" -> slow subagent job stream
// The driver never relies on parity past request 4; later prompts accept any
// reply (side-chat/mobile/reconnect phases).

import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';
import WebSocket from 'ws';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const wrongToken = process.env.RPI_WRONG_TOKEN || 'wrong-token-abc';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const work = process.env.RPI_WORK || evidence;
const fastReply = process.env.RPI_FAST_REPLY || 'steering-followup-reply';
const slowTail = process.env.RPI_SLOW_TAIL || 'chunk-four-done';
const abortedTail = '-done';

const executed = new Set();
function record(id) {
  executed.add(id);
  console.log(`[web-cov:assert] ${id}`);
}

function fail(message) {
  console.error(`web-cov: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

/** Assertion with an ID: waits for a DOM condition, records the ID on pass. */
async function assertId(page, id, fn, label, timeoutMs = 25000, arg) {
  await waitFor(page, fn, `${id}: ${label}`, timeoutMs, arg);
  record(id);
}

async function lastAssistantText(page) {
  return page.evaluate(() => {
    const nodes = document.querySelectorAll('.msg--assistant .assistant-text');
    return nodes.length ? nodes[nodes.length - 1].textContent : '';
  });
}

/** Raw RPC client over the SAME listener (second WS session — concurrency). */
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
      const id = `e2e-${++seq}`;
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

async function connectPage(page) {
  await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
  await waitFor(page, () => document.title === 'rpi web', 'page title missing');
  await waitFor(page, () => document.querySelector('#conn-state') !== null, 'conn-state missing');
  await page.fill('#token-input', token);
  await page.click('#connect-btn');
  await waitFor(page, () => document.getElementById('conn-state').dataset.state === 'on', 'WS did not reach "connected"');
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  let rpc;
  try {
    // ================= Phase A: auth (no/wrong/good token) =================
    const page = await browser.newPage();
    page.on('pageerror', (err) => {
      console.error(`web-cov: page error: ${err.message}`);
    });
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(page, () => document.querySelector('#conn-state') !== null, 'conn-state missing');

    // 1. No token: silent boot probe -> reconnecting, never connected, no error toast.
    let everOn = false;
    const stateWatch = setInterval(async () => {
      try {
        const state = await page.evaluate(() => document.getElementById('conn-state')?.dataset.state || '');
        if (state === 'on') everOn = true;
      } catch {
        /* page mid-navigation */
      }
    }, 100);
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'reconnecting',
      'no-token boot probe never reached reconnecting',
      20000
    );
    await page.waitForTimeout(3500);
    clearInterval(stateWatch);
    const noTokenToast = await page.evaluate(() =>
      Array.from(document.querySelectorAll('#toasts .toast')).some((t) => t.textContent.includes('connection failed'))
    );
    if (noTokenToast) fail('no-token boot probe should be silent, but an error toast appeared');
    if (everOn) fail('WS reached "connected" without a token');
    record('auth.no-token-probe');

    // 2. Wrong token: explicit Connect -> error toast, never connected.
    await page.fill('#token-input', wrongToken);
    await page.click('#connect-btn');
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('#toasts .toast--error')).some((t) =>
          t.textContent.includes('wrong or missing token')
        ),
      'wrong-token error toast never appeared'
    );
    const wrongOn = await page.evaluate(() => document.getElementById('conn-state').dataset.state === 'on');
    if (wrongOn) fail('WS reached "connected" with a wrong token');
    record('auth.wrong-token-toast');

    // 3. Good token: Connect -> connected.
    await page.fill('#token-input', token);
    await page.press('#token-input', 'Enter');
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'WS did not reach "connected" with the good token'
    );
    record('auth.good-token-connect');

    // ============ Phase B: prompt round-trips + early abort ============
    // Request 1 in the steering mock is the ~4s slow stream.
    await page.fill('#prompt-input', 'hello from the web e2e');
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, (tail) => document.body.textContent.includes(tail), 'full slow reply never streamed into the DOM', 30000, slowTail);
    await waitFor(page, () => document.getElementById('stream-badge').hidden === true, 'streaming badge did not clear after the reply completed');
    const sendLabelWhenIdle = await page.textContent('#send-btn');
    if (sendLabelWhenIdle?.trim() !== 'Send') {
      fail(`primary submit did not return to Send while idle: "${sendLabelWhenIdle}"`);
    }
    record('app.primary-submit-send');
    record('prompt.slow-stream-full');

    // Request 2 is instant.
    await page.fill('#prompt-input', 'again');
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, (reply) => document.body.textContent.includes(reply), 'fast reply never streamed into the DOM', 30000, fastReply);
    record('prompt.fast-roundtrip');

    // Request 3: slow stream, EARLY abort.
    await page.fill('#prompt-input', 'stream a long answer');
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, () => document.body.textContent.includes('steer-3-'), 'third stream never started');
    await waitFor(
      page,
      () => document.getElementById('stream-badge').hidden === false,
      'streaming badge never appeared for the third stream'
    );
    const sendLabelWhileStreaming = await page.textContent('#send-btn');
    if (sendLabelWhileStreaming?.trim() !== 'Steer') {
      fail(`primary submit did not switch to Steer while streaming: "${sendLabelWhileStreaming}"`);
    }
    record('app.primary-submit-steer');
    // Dismiss every toast accumulated by earlier phases (e.g. the Phase A
    // wrong-token "connection failed" error toast, which auto-dismisses only
    // after 7s) so the abort's anyError check below is scoped to toasts the
    // abort itself produces — the strong assertion is NOT weakened.
    await page.evaluate(() => {
      document.querySelectorAll('#toasts .toast').forEach((t) => t.click());
    });
    await waitFor(page, () => document.querySelectorAll('#toasts .toast').length === 0, 'stale toasts did not clear before the abort');
    await page.click('#abort-btn');
    await waitFor(page, () => document.getElementById('stream-badge').hidden === true, 'streaming badge did not clear after abort');
    const preserved = await lastAssistantText(page);
    if (!preserved.includes('steer-3-')) fail(`aborted message lost the streamed text: "${preserved}"`);
    if (preserved.includes(abortedTail)) fail(`aborted message rendered chunks past the abort: "${preserved}"`);
    record('abort.early-preserves-text');
    record('abort.no-tail-render');
    await waitFor(
      page,
      () => Array.from(document.querySelectorAll('#toasts .toast')).some((t) => t.textContent.includes('run aborted')),
      'neutral "run aborted" toast never appeared'
    );
    const toastState = await page.evaluate(() => {
      const toasts = Array.from(document.querySelectorAll('#toasts .toast'));
      return {
        neutralIsError: toasts.some((t) => t.textContent.includes('run aborted') && t.classList.contains('toast--error')),
        anyError: toasts.some((t) => t.classList.contains('toast--error')),
      };
    });
    if (toastState.neutralIsError) fail('abort toast rendered as an ERROR toast');
    if (toastState.anyError) fail('an unexpected error toast appeared alongside the abort');
    record('abort.neutral-toast');
    record('abort.no-error-toast');

    // Request 4: recovery after the turn-gate settle -> NEW assistant message.
    await page.waitForTimeout(6000);
    const assistantCountBeforeRecovery = await page.evaluate(
      () => document.querySelectorAll('.msg--assistant .assistant-text').length
    );
    await page.fill('#prompt-input', 'recovery prompt');
    await page.press('#prompt-input', 'Enter');
    // Request 4 is even in the steering mock -> instant "steering-followup-reply"
    // (== fastReply). The arg carries BOTH the pre-count and fastReply as a
    // single object: page.waitForFunction evaluates the predicate in the PAGE
    // context, so a closure over the Node-scope `fastReply` would be undefined
    // there (the original `.includes(fastReply)` form always read undefined
    // and timed out — passing it via the arg restores the strong assertion).
    await waitFor(
      page,
      (ctx) => {
        const nodes = [...document.querySelectorAll('.msg--assistant .assistant-text')];
        return nodes.length > ctx.before && (nodes[nodes.length - 1]?.textContent || '').includes(ctx.fr);
      },
      'post-abort prompt did not round-trip as a new message',
      30000,
      { before: assistantCountBeforeRecovery, fr: fastReply }
    );
    record('prompt.new-message-count');

    // ==================== Phase C: rich content ====================
    const richPromptAttempt = async () => {
      const before = await page.evaluate(() => document.querySelectorAll('.msg--assistant .assistant-text').length);
      await page.fill('#prompt-input', 'render rich content');
      await page.press('#prompt-input', 'Enter');
      await waitFor(
        page,
        (b) => {
          const nodes = [...document.querySelectorAll('.msg--assistant .assistant-text')];
          if (nodes.length <= b) return false;
          if (document.querySelector('table.md-table')) return true;
          return document.getElementById('stream-badge')?.hidden === true;
        },
        'rich prompt never produced a reply',
        30000,
        before
      );
      return page.evaluate(() => !!document.querySelector('table.md-table'));
    };
    let richRendered = false;
    for (let attempt = 1; attempt <= 3 && !richRendered; attempt++) {
      richRendered = await richPromptAttempt();
    }
    if (!richRendered) fail('markdown table never rendered (after 3 prompt attempts)');
    record('md.table-rendered');
    await waitFor(page, () => document.querySelector('.md-task-glyph') !== null, 'task-list glyph never rendered', 30000);
    record('md.task-list-glyph');
    await waitFor(page, () => document.querySelector('.assistant-text svg') !== null, 'mermaid SVG never rendered', 30000);
    record('mermaid.svg-rendered');
    await waitFor(page, () => document.querySelector('.assistant-text .katex') !== null, 'KaTeX math never rendered', 30000);
    record('katex.rendered');
    const richText = await lastAssistantText(page);
    if (richText.includes('```')) fail('raw fence markers leaked into the transcript');
    if (richText.includes('|---')) fail('raw table separator leaked into the transcript');
    record('md.no-fence-leak');
    record('md.no-separator-leak');

    // ---- markdown renderer coverage: ordinary code fence + URL policy ----
    await waitFor(page, () => document.querySelector('.md-fence__copy') !== null, 'ordinary code fence copy button never rendered', 15000);
    record('md.fence-rendered');
    const fenceLang = await page.evaluate(() => document.querySelector('.md-fence__lang')?.textContent || '');
    if (!fenceLang.includes('text')) fail(`code fence lang label wrong: "${fenceLang}"`);
    // Click the delegated copy button and observe the flash state.
    await page.click('.md-fence__copy');
    await waitFor(page, () => (document.querySelector('.md-fence__copy')?.textContent || '') === 'copied', 'fence copy button never flashed "copied"', 10000);
    record('md.fence-copy');
    // Safe link renders as a live anchor with the allowlisted http href.
    await waitFor(page, () => document.querySelector('a[href="https://example.org/pi-web"]') !== null, 'safe http link anchor never rendered', 10000);
    record('md.link-safe');
    // Blocked javascript: link must NOT become a live anchor; label text shows inert.
    const linkPolicy = await page.evaluate(() => {
      const live = Array.from(document.querySelectorAll('.assistant-text a')).map((a) => a.getAttribute('href') || '');
      return {
        jsHref: live.some((h) => h.toLowerCase().startsWith('javascript:')),
        evilText: document.body.textContent.includes('evil'),
      };
    });
    if (linkPolicy.jsHref) fail('a javascript: link leaked as a live anchor');
    if (!linkPolicy.evilText) fail('blocked link label text "evil" never rendered inert');
    record('md.link-blocked');
    // Safe data:image PNG renders as an <img>; select by stable alt text and
    // verify its raw attribute separately because Chromium may normalize data
    // URL selector matching while the streaming DOM is settling.
    await waitFor(page, () => {
      const image = document.querySelector('img.md-image[alt="pic"]');
      return (image?.getAttribute('src') || '').startsWith('data:image/png;base64,');
    }, 'safe data:image PNG never rendered', 15000);
    record('md.image-safe');
    const imagePolicy = await page.evaluate(() => {
      const imgs = Array.from(document.querySelectorAll('.assistant-text img')).map((i) => i.getAttribute('src') || '');
      return {
        textHtmlImg: imgs.some((s) => s.toLowerCase().startsWith('data:text/html')),
        safePngImg: imgs.some((s) => s.startsWith('data:image/png')),
      };
    });
    if (imagePolicy.textHtmlImg) fail('a data:text/html image leaked as a live <img>');
    if (!imagePolicy.safePngImg) fail('the safe data:image PNG did not render as a live <img>');
    record('md.image-blocked');

    // ---- Phase C2: tool execution card (App.ToolCard + redact.safeJson) ----
    // A prompt carrying the marker makes the agent call the `read` tool once;
    // the tool_execution_start/end events render a .tool-card in the transcript.
    const toolCardBefore = await page.evaluate(() => document.querySelectorAll('.tool-card').length);
    await page.fill('#prompt-input', 'tool-card coverage read seed');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (b) => {
        const cards = [...document.querySelectorAll('.tool-card')];
        if (cards.length <= b) return false;
        const last = cards[cards.length - 1];
        return (last.querySelector('.tool-card__name')?.textContent || '') === 'read';
      },
      'read tool-card never rendered in the transcript',
      30000,
      toolCardBefore
    );
    record('app.tool-card');
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.tool-card')];
        const last = cards[cards.length - 1];
        return !!last && last.querySelector('.tool-card__state--done') !== null;
    },
      'read tool-card never reached done state',
      30000
    );
    const toolCardArgs = await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.tool-card')];
      const last = cards[cards.length - 1];
      return last
        ? { name: last.querySelector('.tool-card__name')?.textContent || '', result: last.querySelector('.tool-card__result')?.textContent || '', hasArgs: last.querySelector('.tool-card__args') !== null }
        : null;
    });
    if (!toolCardArgs || toolCardArgs.name !== 'read') fail(`tool-card name wrong: ${JSON.stringify(toolCardArgs)}`);
    if (!toolCardArgs.hasArgs) fail('tool-card args pre (safeJson) never rendered');
    if (!toolCardArgs.result.includes('web coverage seed')) fail(`tool-card result never showed the read file content: "${toolCardArgs.result}"`);
    record('app.tool-card-done');

    // ============== Phase L: reconnect (real server kill) ==============
    // The lane shell (coverage.sh) watches kill-server.marker, kills and
    // respawns the listener on the SAME port with --continue, then writes
    // server-up.marker.
    const killMarker = path.join(work, 'kill-server.marker');
    const upMarker = path.join(work, 'server-up.marker');
    fs.rmSync(killMarker, { force: true });
    fs.rmSync(upMarker, { force: true });
    let sawReconnecting = false;
    const pillWatch = setInterval(async () => {
      try {
        if ((await page.evaluate(() => document.getElementById('conn-state')?.dataset.state || '')) === 'reconnecting') {
          sawReconnecting = true;
        }
      } catch {
        /* page mid-navigation / closed */
      }
    }, 100);
    fs.writeFileSync(killMarker, 'kill now\n');
    const upDeadline = Date.now() + 120000;
    while (!fs.existsSync(upMarker) && Date.now() < upDeadline) {
      await new Promise((r) => setTimeout(r, 200));
    }
    if (!fs.existsSync(upMarker)) fail('lane never respawned the listener (server-up.marker missing)');
    clearInterval(pillWatch);
    if (!sawReconnecting) fail('conn-state never entered "reconnecting" after the server was killed');
    record('reconnect.pill');
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'client never auto-reconnected after the listener came back'
    );
    record('reconnect.auto-on');
    const survives = await page.evaluate((tail) => document.body.textContent.includes(tail), slowTail);
    if (!survives) fail('pre-crash assistant reply vanished after the reconnect');
    record('reconnect.transcript-survives');
    const msgsBeforePost = await page.evaluate(() => document.querySelectorAll('.msg--assistant .assistant-text').length);
    await page.fill('#prompt-input', 'still here after the restart');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (before) => {
        const nodes = [...document.querySelectorAll('.msg--assistant .assistant-text')];
        return nodes.length > before && (nodes[nodes.length - 1]?.textContent || '').trim() !== '';
      },
      'post-reconnect prompt did not round-trip',
      60000,
      msgsBeforePost
    );
    record('reconnect.roundtrip');

    // ============ Phase D: model + thinking switch ============
    await waitFor(page, () => document.getElementById('model-select') !== null, 'model select missing');
    const modelOptions = await page.$$eval('#model-select option', (opts) => opts.map((o) => o.value));
    if (!modelOptions.includes('user-steering/mock-2')) {
      fail(`fixture must expose the second model for the switch contract, got [${modelOptions.join(', ')}]`);
    }
    await page.selectOption('#model-select', 'user-steering/mock-2');
    await waitFor(
      page,
      () => document.getElementById('model-select')?.value === 'user-steering/mock-2',
      'model switch never round-tripped into the select'
    );
    record('switch.model-roundtrip');
    await waitFor(page, () => document.getElementById('thinking-select') !== null, 'thinking select missing');
    const levels = await page.$$eval('#thinking-select option', (opts) => opts.map((o) => o.value));
    if (!levels.includes('high')) fail(`thinking levels should include "high", got [${levels.join(', ')}]`);
    await page.selectOption('#thinking-select', 'high');
    await waitFor(
      page,
      () => document.getElementById('thinking-select')?.value === 'high',
      'thinking-level switch never round-tripped into the select'
    );
    record('switch.thinking-roundtrip');

    // ==================== Phase E: todo panel ====================
    await page.click('#todos-toggle-btn');
    await waitFor(page, () => document.getElementById('todo-panel') !== null, 'todo panel did not open');
    await page.fill('#todo-add-phase', 'Plan');
    await page.fill('#todo-add-content', 'web e2e task');
    await page.click('#todo-add-btn');
    await waitFor(
      page,
      () => [...document.querySelectorAll('.todo-task')].some((row) => row.textContent.includes('web e2e task')),
      'added task never appeared in the todo panel'
    );
    record('todo.create-row');
    await page.evaluate(() => {
      const rows = [...document.querySelectorAll('.todo-task')];
      const row = rows.find((r) => r.textContent.includes('web e2e task'));
      if (!row) throw new Error('task row missing');
      const btn = row.querySelector('.todo-task__action[data-action="complete"]');
      if (!btn) throw new Error('complete button missing');
      btn.click();
    });
    await waitFor(
      page,
      () => {
        const rows = [...document.querySelectorAll('.todo-task')];
        const row = rows.find((r) => r.textContent.includes('web e2e task'));
        return !!row && row.querySelector('.todo-task__bullet')?.getAttribute('aria-label') === 'completed';
      },
      'task never reached completed status'
    );
    record('todo.complete-status');
    await waitFor(
      page,
      () => (document.getElementById('todo-counts')?.textContent || '').includes('1 done'),
      'counts never reflected the completed task'
    );
    record('todo.counts-done');
    await page.evaluate(() => {
      const rows = [...document.querySelectorAll('.todo-task')];
      const row = rows.find((r) => r.textContent.includes('web e2e task'));
      if (!row) throw new Error('task row missing');
      const btn = row.querySelector('.todo-task__action[data-action="reopen"]');
      if (!btn) throw new Error('reopen button missing');
      btn.click();
    });
    await waitFor(
      page,
      () => {
        const rows = [...document.querySelectorAll('.todo-task')];
        const row = rows.find((r) => r.textContent.includes('web e2e task'));
        return !!row && row.querySelector('.todo-task__bullet')?.getAttribute('aria-label') === 'in_progress';
      },
      'reopened task never returned to in_progress'
    );
    record('todo.reopen-status');
    await page.evaluate(() => {
      const rows = [...document.querySelectorAll('.todo-task')];
      const row = rows.find((r) => r.textContent.includes('web e2e task'));
      if (!row) throw new Error('task row missing');
      row.click();
    });
    await waitFor(
      page,
      () => (document.getElementById('todo-detail')?.textContent || '').includes('web e2e task'),
      'task detail pane never rendered'
    );
    record('todo.detail-pane');

    // ---- todo lifecycle coverage: add via Enter, dependency link/unlink,
    // detail-pane Complete/Reopen, clear selection ----
    // Add a SECOND task via Enter (covers the content input onKeyDown path).
    await page.fill('#todo-add-phase', 'Plan');
    await page.fill('#todo-add-content', 'dep target task');
    await page.press('#todo-add-content', 'Enter');
    await waitFor(
      page,
      () => [...document.querySelectorAll('.todo-task')].some((r) => r.textContent.includes('dep target task')),
      'second task (added via Enter) never appeared'
    );
    record('todo.add-via-enter');
    // Re-select the first task so the detail pane is the dependency editor.
    await page.evaluate(() => {
      const rows = [...document.querySelectorAll('.todo-task')];
      const row = rows.find((r) => r.textContent.includes('web e2e task'));
      if (!row) throw new Error('first task row missing');
      row.click();
    });
    await waitFor(page, () => document.getElementById('todo-detail') !== null, 'detail pane did not reopen');
    // Toggle the dependency editor (covers the edit-toggle onClick).
    await page.click('.todo-detail__toggle');
    await waitFor(page, () => document.getElementById('todo-dep-select') !== null, 'dependency editor select never rendered');
    // Choose the second task as a dependency (covers the dep-select onChange).
    await page.selectOption('#todo-dep-select', { label: 'dep target task' });
    // Link it (covers linkDependency) and assert the dep label resolves to the
    // dependency's content via taskContentById.
    await page.click('.todo-detail__dep-add button');
    await waitFor(
      page,
      () => [...document.querySelectorAll('.todo-detail__dep-label')].some((el) => el.textContent.includes('dep target task')),
      'linked dependency label never rendered the dependency content'
    );
    record('todo.dep-link');
    // Unlink it (covers unlinkDependency).
    await page.click('.todo-detail__unlink');
    await waitFor(
      page,
      () => document.querySelectorAll('.todo-detail__unlink').length === 0,
      'unlink never removed the dependency chip'
    );
    record('todo.dep-unlink');
    // Detail-pane Complete button (covers the open-branch detail onClick).
    await page.click('#todo-detail-complete');
    await waitFor(
      page,
      () => {
        const rows = [...document.querySelectorAll('.todo-task')];
        const row = rows.find((r) => r.textContent.includes('web e2e task'));
        return !!row && row.querySelector('.todo-task__bullet')?.getAttribute('aria-label') === 'completed';
      },
      'detail Complete never reached completed'
    );
    record('todo.detail-complete');
    // Detail-pane Reopen button (covers the completed-branch detail onClick).
    await page.click('#todo-detail-reopen');
    await waitFor(
      page,
      () => {
        const rows = [...document.querySelectorAll('.todo-task')];
        const row = rows.find((r) => r.textContent.includes('web e2e task'));
        return !!row && row.querySelector('.todo-task__bullet')?.getAttribute('aria-label') === 'in_progress';
      },
      'detail Reopen never returned to in_progress'
    );
    record('todo.detail-reopen');
    // Clear the selection (covers the detail-head close onClick).
    await page.click('.todo-detail__head .todo-panel__close');
    await waitFor(page, () => document.getElementById('todo-detail') === null, 'detail pane did not clear');
    record('todo.clear-selection');

    // ==================== Phase F: goal panel ====================
    await page.click('#todo-close-btn');
    await waitFor(page, () => document.getElementById('todo-panel') === null, 'todo panel did not close');
    await page.click('#goal-panel-btn');
    await waitFor(page, () => document.getElementById('goal-panel') !== null, 'goal panel missing');
    await waitFor(
      page,
      () => document.getElementById('goal-panel').dataset.hasGoal === 'false',
      'goal panel should start with no goal'
    );
    record('goal.empty-state');
    const objective = `ship the web goal e2e ${Date.now()}`;
    await page.fill('#goal-objective-input', objective);
    await page.fill('#goal-budget-input', '100');
    await page.click('#goal-create-btn');
    await waitFor(
      page,
      (obj) =>
        document.getElementById('goal-panel').dataset.hasGoal === 'true' &&
        document.getElementById('goal-panel').dataset.lifecycle === 'active' &&
        (document.getElementById('goal-objective') || {}).textContent === obj,
      'created goal never appeared in the panel',
      30000,
      objective
    );
    record('goal.create');
    const budgetText = await page.textContent('#goal-budget');
    if (!budgetText.includes('100 token budget')) fail(`budget line wrong: ${budgetText}`);
    const usageText = await page.textContent('#goal-usage');
    if (!usageText.includes('0/100 tokens')) fail(`usage line wrong: ${usageText}`);
    record('goal.budget-usage');
    const pinText = 'stay focused';
    await page.fill('#goal-pin-input', pinText);
    await page.click('#goal-pin-btn');
    await waitFor(
      page,
      (pin) =>
        Array.from(document.querySelectorAll('#goal-pins li .goal-pin__text')).some((el) => el.textContent === pin),
      'pinned text never appeared in the panel',
      30000,
      pinText
    );
    record('goal.pin');

    // Live update via a SECOND WS client (concurrency): goal_pause flips the
    // panel purely from the pushed event.
    const wsUrl = url.replace(/^http/, 'ws').replace(/\/web\/?$/, '/ws');
    rpc = rpcClient(wsUrl);
    const paused = await rpc.call({ type: 'goal_pause' });
    if (!paused || paused.lifecycle !== 'paused') fail(`raw goal_pause returned ${JSON.stringify(paused)}`);
    record('concurrency.second-ws-rpc');
    await waitFor(
      page,
      () => document.getElementById('goal-panel').dataset.lifecycle === 'paused',
      'panel never flipped to paused from the live event'
    );
    record('concurrency.live-event-reflect');
    record('goal.pause-live');
    await page.click('#goal-resume-btn');
    await waitFor(
      page,
      () => document.getElementById('goal-panel').dataset.lifecycle === 'active',
      'panel never returned to active after resume'
    );
    record('goal.resume');
    // The panel lifecycle flips to active before the journal replays the
    // 'resumed' entry (async), so reading the journal immediately can race and
    // miss it — wait for the resumed entry to render first.
    await waitFor(
      page,
      () => document.querySelector('#goal-journal li[data-kind="resumed"]') !== null,
      'journal never replayed the resumed event'
    );
    const kinds = await page.$$eval('#goal-journal li', (lis) => lis.map((li) => li.dataset.kind));
    const expected = ['created', 'pins_updated', 'paused', 'resumed'];
    for (const kind of expected) {
      if (!kinds.includes(kind)) fail(`journal replay missing ${kind}; got ${JSON.stringify(kinds)}`);
    }
    const firstCreated = kinds.indexOf('created');
    const firstPinned = kinds.indexOf('pins_updated');
    const firstPaused = kinds.indexOf('paused');
    const firstResumed = kinds.indexOf('resumed');
    if (!(firstCreated < firstPinned && firstPinned < firstPaused && firstPaused < firstResumed)) {
      fail(`journal replay out of order: ${JSON.stringify(kinds)}`);
    }
    record('goal.journal-order');

    // ==================== Phase G: workflow panel ====================
    await page.click('#goal-close-btn');
    await waitFor(page, () => document.getElementById('goal-panel') === null, 'goal panel did not close');
    await page.click('#workflow-toggle-btn');
    await waitFor(page, () => document.getElementById('workflow-panel') !== null, 'workflow panel did not open');
    await page.fill('#workflow-create-name', 'web-e2e-workflow');
    await page.fill('#workflow-create-objective', 'created from the browser e2e');
    await page.click('#workflow-create-btn');
    await waitFor(
      page,
      () => [...document.querySelectorAll('.workflow-row')].some((row) => row.textContent.includes('web-e2e-workflow')),
      'created workflow never appeared in the workflow list',
      30000
    );
    record('workflow.create-row');
    const workflowStatus = await page.evaluate(() => {
      const rows = [...document.querySelectorAll('.workflow-row')];
      const row = rows.find((r) => r.textContent.includes('web-e2e-workflow'));
      return row ? row.getAttribute('data-status') || '' : '';
    });
    if (!['queued', 'planning', 'running', 'paused', 'integrating'].includes(workflowStatus)) {
      fail(`created workflow must be live (queued/planning/running/paused/integrating), got "${workflowStatus}"`);
    }
    record('workflow.live-status');
    await page.click('#workflow-cancel-btn');
    await waitFor(
      page,
      () => {
        const rows = [...document.querySelectorAll('.workflow-row')];
        const row = rows.find((r) => r.textContent.includes('web-e2e-workflow'));
        return !!row && row.getAttribute('data-status') === 'cancelled';
      },
      'cancel never applied (status never reached cancelled)',
      30000
    );
    record('workflow.cancel-status');

    // ---- workflow lifecycle coverage: create via Enter, row select,
    // pause/resume/integrate/remove, actor activity toggle ----
    // Create a second workflow via Enter (covers the objective onKeyDown).
    await page.fill('#workflow-create-name', 'web-e2e-workflow-2');
    await page.fill('#workflow-create-objective', 'lifecycle coverage workflow');
    await page.press('#workflow-create-objective', 'Enter');
    await waitFor(
      page,
      () => [...document.querySelectorAll('.workflow-row')].some((row) => row.textContent.includes('web-e2e-workflow-2')),
      'second workflow (created via Enter) never appeared',
      30000
    );
    record('workflow.create-via-enter');
    // Click the row to select it (covers the row onClick -> selectWorkflow).
    await page.evaluate(() => {
      const rows = [...document.querySelectorAll('.workflow-row')];
      const row = rows.find((r) => r.textContent.includes('web-e2e-workflow-2'));
      if (!row) throw new Error('workflow-2 row missing');
      row.click();
    });
    await waitFor(
      page,
      () => (document.querySelector('.workflow-detail__name')?.textContent || '').includes('web-e2e-workflow-2'),
      'selecting the workflow row never rendered its detail pane'
    );
    record('workflow.select-row');
    // Pause -> paused (covers the pause onClick).
    await page.click('#workflow-pause-btn');
    await waitFor(
      page,
      () => {
        const rows = [...document.querySelectorAll('.workflow-row')];
        const row = rows.find((r) => r.textContent.includes('web-e2e-workflow-2'));
        return !!row && row.getAttribute('data-status') === 'paused';
      },
      'pause never reached paused',
      30000
    );
    record('workflow.pause');
    // Resume -> live again (covers the resume onClick).
    await page.click('#workflow-resume-btn');
    await waitFor(
      page,
      () => {
        const rows = [...document.querySelectorAll('.workflow-row')];
        const row = rows.find((r) => r.textContent.includes('web-e2e-workflow-2'));
        return !!row && ['running', 'planning', 'integrating'].includes(row.getAttribute('data-status') || '');
      },
      'resume never returned the workflow to a live state',
      30000
    );
    record('workflow.resume');
    // Best-effort actor activity toggle (covers ActorRow onClick when an actor
    // has activity). Skipped silently if no foldable actor appears in time.
    const actorToggleOk = await page
      .waitForFunction(
        () => document.querySelector('.workflow-actor.is-foldable') !== null,
        { timeout: 6000 }
      )
      .then(() => true)
      .catch(() => false);
    if (actorToggleOk) {
      const expandedBefore = await page.evaluate(() => document.querySelectorAll('.workflow-actor__activity').length);
      await page.click('.workflow-actor.is-foldable .workflow-actor__row');
      await waitFor(
        page,
        (b) => document.querySelectorAll('.workflow-actor__activity').length !== b,
        'actor activity feed never toggled',
        10000,
        expandedBefore
      );
      // Best-effort: no record() — the actor feed is non-deterministic, so it
      // is not a matrix-gated assertion; the click still exercises ActorRow's
      // onClick for coverage when a foldable actor is present.
    }
    // Remove workflow-2 (covers the remove onClick) — deterministic: the row
    // leaves the list (the manager cancels non-terminal workflows first).
    await page.click('#workflow-remove-btn');
    await waitFor(
      page,
      () => [...document.querySelectorAll('.workflow-row')].every((r) => !r.textContent.includes('web-e2e-workflow-2')),
      'remove never dropped the workflow-2 row',
      30000
    );
    record('workflow.remove');
    // Integrate (covers the integrate onClick) on a fresh workflow. Pause so
    // the button is enabled, click it, and accept any observable outcome
    // (integration element, detail error, status transition, or row removal).
    // The click fires the handler for coverage regardless; integrate on a
    // worktree with no new commits may no-op, so the outcome wait is bounded
    // and non-fatal — but the panel vanishing still fails the lane.
    await page.fill('#workflow-create-name', 'web-e2e-workflow-3');
    await page.fill('#workflow-create-objective', 'integrate coverage workflow');
    await page.click('#workflow-create-btn');
    await waitFor(
      page,
      () => [...document.querySelectorAll('.workflow-row')].some((row) => row.textContent.includes('web-e2e-workflow-3')),
      'third workflow never appeared for integrate',
      30000
    );
    await page.evaluate(() => {
      const rows = [...document.querySelectorAll('.workflow-row')];
      const row = rows.find((r) => r.textContent.includes('web-e2e-workflow-3'));
      if (!row) throw new Error('workflow-3 row missing');
      row.click();
    });
    await waitFor(
      page,
      () => (document.querySelector('.workflow-detail__name')?.textContent || '').includes('web-e2e-workflow-3'),
      'workflow-3 detail never rendered'
    );
    await page.click('#workflow-pause-btn');
    await waitFor(
      page,
      () => {
        const rows = [...document.querySelectorAll('.workflow-row')];
        const row = rows.find((r) => r.textContent.includes('web-e2e-workflow-3'));
        return !!row && row.getAttribute('data-status') === 'paused';
      },
      'workflow-3 pause never reached paused before integrate',
      30000
    );
    await page.click('#workflow-integrate-btn');
    const integrateOutcome = await page
      .waitForFunction(
        () => {
          const rows = [...document.querySelectorAll('.workflow-row')];
          const row = rows.find((r) => r.textContent.includes('web-e2e-workflow-3'));
          const status = row ? row.getAttribute('data-status') || '' : 'removed';
          const detailError = document.querySelector('#workflow-detail .workflow-panel__error') !== null;
          const integration = document.querySelector('.workflow-detail__integration') !== null;
          return detailError || integration || status === 'removed' || ['integrating', 'completed', 'conflicted', 'cancelled'].includes(status);
        },
        { timeout: 12000 }
      )
      .then(() => true)
      .catch(() => false);
    if (!integrateOutcome) {
      const panelOk = await page.evaluate(() => document.getElementById('workflow-panel') !== null);
      if (!panelOk) fail('workflow panel vanished after integrate (no observable outcome)');
    }
    record('workflow.integrate');

    // ==================== Phase H: settings panel ====================
    await page.click('#workflow-close-btn');
    await waitFor(page, () => document.getElementById('workflow-panel') === null, 'workflow panel did not close');
    await page.click('#settings-toggle-btn');
    await waitFor(page, () => document.getElementById('settings-panel') !== null, 'settings panel did not open');
    await page.click('.settings-category:has-text("Terminal")');
    await waitFor(page, () => document.querySelector('[data-setting-key="theme"]') !== null, 'theme setting row never rendered');
    record('settings.browse-category');
    await page.click('.settings-category:has-text("Live")');
    await waitFor(
      page,
      () => document.querySelector('[data-setting-key="live.sttApiKey"]') !== null,
      'live.sttApiKey row never rendered'
    );
    const secretText = await page.evaluate(() => {
      const row = document.querySelector('[data-setting-key="live.sttApiKey"]');
      return row ? row.textContent : '';
    });
    if (!secretText.includes('[redacted]') && !secretText.includes('[secret]')) {
      fail(`secret key must render redacted, got: ${secretText}`);
    }
    const secretControlCount = await page.evaluate(() => {
      const row = document.querySelector('[data-setting-key="live.sttApiKey"]');
      return row ? row.querySelectorAll('input, select, textarea').length : -1;
    });
    if (secretControlCount !== 0) fail(`secret key must not be editable, found ${secretControlCount} controls`);
    record('settings.secret-redacted-readonly');
    await page.click('.settings-category:has-text("Terminal")');
    await page.click('#settings-edit-btn');
    await waitFor(page, () => document.getElementById('settings-apply-btn') !== null, 'draft never opened (Apply button missing)');
    const themeInputSel = '[data-setting-key="theme"] input[type="text"]';
    await waitFor(page, (sel) => document.querySelector(sel) !== null, 'theme input never rendered in draft mode', 25000, themeInputSel);
    await page.fill(themeInputSel, 'e2e-theme');
    await page.evaluate(() => {
      const el = document.querySelector('[data-setting-key="theme"] input[type="text"]');
      if (el) el.blur();
    });
    await waitFor(
      page,
      () => (document.querySelector('[data-setting-key="theme"] .setting-row__dirty')?.textContent || '') === 'dirty',
      'theme edit never staged as dirty in the draft'
    );
    record('settings.draft-dirty');
    await page.click('#settings-apply-btn');
    await waitFor(page, () => document.getElementById('settings-edit-btn') !== null, 'apply never closed the draft (Edit button missing)');
    await waitFor(
      page,
      () => {
        const input = document.querySelector('[data-setting-key="theme"] input[type="text"]');
        return !!input && input.value === 'e2e-theme';
      },
      'applied theme never reflected in the settings panel'
    );
    record('settings.apply-persisted');

    // ---- settings coverage: refresh, typed control variants (boolean/enum/
    // number/JSON textarea), reset-to-default, cancel draft, close ----
    await page.click('#settings-refresh-btn');
    await waitFor(page, () => document.querySelector('[data-setting-key="theme"]') !== null, 'refresh never reloaded the catalog');
    record('settings.refresh');
    // Open a second draft to exercise every typed SettingControl variant.
    await page.click('.settings-category:has-text("Terminal")');
    await page.click('#settings-edit-btn');
    await waitFor(page, () => document.getElementById('settings-apply-btn') !== null, 'second draft never opened');
    const settingsNotBusy = () => document.getElementById('settings-apply-btn')?.disabled === false;
    await waitFor(page, settingsNotBusy, 'settings stayed busy before typed edits');
    // Boolean checkbox (onChange -> parseTypedValue boolean branch).
    await waitFor(page, () => document.querySelector('.setting-row input[type="checkbox"]:not([disabled])') !== null, 'no editable boolean setting', 10000);
    await page.click('.setting-row input[type="checkbox"]:not([disabled])');
    await waitFor(page, settingsNotBusy, 'boolean edit never settled');
    record('settings.typed-boolean');
    // Enum select (onChange -> parseTypedValue enum branch).
    await waitFor(page, () => document.querySelector('.setting-row select:not([disabled])') !== null, 'no editable enum setting', 10000);
    const enumOptions = await page.$$eval('.setting-row select:not([disabled]) option', (opts) => opts.map((o) => o.value));
    const currentEnum = await page.$eval('.setting-row select:not([disabled])', (el) => el.value);
    const enumTarget = (enumOptions.find((v) => v && v !== currentEnum) || enumOptions.find((v) => v) || '');
    if (enumTarget) await page.selectOption('.setting-row select:not([disabled])', enumTarget);
    await waitFor(page, settingsNotBusy, 'enum edit never settled');
    record('settings.typed-enum');
    // Number input (onChange + onBlur -> parseTypedValue integer branch).
    await waitFor(page, () => document.querySelector('.setting-row input[type="number"]:not([disabled])') !== null, 'no editable number setting', 10000);
    await page.fill('.setting-row input[type="number"]:not([disabled])', '80');
    await page.evaluate(() => document.querySelector('.setting-row input[type="number"]:not([disabled])')?.blur());
    await waitFor(page, settingsNotBusy, 'number edit never settled');
    record('settings.typed-number');
    // JSON object textarea (onBlur -> parseTypedValue object branch).
    await waitFor(page, () => document.querySelector('.setting-row textarea:not([disabled])') !== null, 'no editable JSON setting', 10000);
    await page.fill('.setting-row textarea:not([disabled])', '{"e2e": true}');
    await page.evaluate(() => document.querySelector('.setting-row textarea:not([disabled])')?.blur());
    await waitFor(page, settingsNotBusy, 'json edit never settled');
    record('settings.typed-json');
    // Reset-to-default (covers commitReset -> settings_reset).
    await waitFor(page, () => document.querySelector('.setting-row__reset:not([disabled])') !== null, 'no reset button in draft', 10000);
    await page.click('.setting-row__reset:not([disabled])');
    await waitFor(page, settingsNotBusy, 'reset never settled');
    record('settings.reset');
    // Cancel the draft (covers cancelDraft -> settings_cancel, discards the
    // typed edits so the fixture settings are unchanged).
    await page.click('#settings-cancel-btn');
    await waitFor(page, () => document.getElementById('settings-edit-btn') !== null, 'cancel never closed the draft');
    record('settings.cancel-draft');
    // Close the settings panel via its own close button (covers App's
    // settings onClose callback).
    await page.click('#settings-close-btn');
    await waitFor(page, () => document.getElementById('settings-panel') === null, 'settings panel did not close');
    record('settings.close');

    // ==================== Phase I: sessions panel ====================
    await waitFor(page, () => document.getElementById('session-sidebar') !== null, 'session sidebar did not render');
    await waitFor(
      page,
      () => document.querySelectorAll('.session-sidebar__row').length >= 1,
      'session sidebar never listed saved sessions'
    );
    record('session.sidebar-list');
    await page.click('#session-toggle-btn');
    await waitFor(page, () => document.getElementById('session-panel') !== null, 'session panel did not open');
    const nameValueSel = '[data-testid="session-name-value"]';
    await waitFor(page, (sel) => document.querySelector(sel) !== null, 'current session name never rendered', 25000, nameValueSel);
    await page.fill('#session-rename-input', 'web e2e session');
    await page.click('#session-rename-btn');
    await waitFor(
      page,
      () => document.querySelector('[data-testid="session-name-value"]')?.textContent === 'web e2e session',
      'session rename never reflected in the panel'
    );
    await waitFor(
      page,
      () => (document.getElementById('session-name')?.textContent || '').includes('web e2e session'),
      'session rename never reflected in the header'
    );
    record('session.rename-panel-header');
    record('nav.header-session-name');
    await waitFor(
      page,
      () => document.querySelectorAll('.session-row').length >= 1,
      'no saved sessions listed in the session panel'
    );
    record('session.history-listed');
    const sessionIdBefore = await page.evaluate(() => {
      const dts = [...document.querySelectorAll('#session-panel dl dt')];
      const idx = dts.findIndex((dt) => dt.textContent === 'Session id');
      return idx >= 0 ? dts[idx]?.nextElementSibling?.textContent || '' : '';
    });
    if (!sessionIdBefore) fail('current session id never rendered');
    const msgsBeforeSwitch = await page.$$eval('#transcript .msg', (els) => els.length);
    if (msgsBeforeSwitch === 0) fail('no transcript messages before the switch — the clear would be unobservable');
    await page.click('#session-new-btn');
    const newIdOk = await page
      .waitForFunction(
        (b) => {
          const dts = [...document.querySelectorAll('#session-panel dl dt')];
          const idx = dts.findIndex((dt) => dt.textContent === 'Session id');
          const dd = idx >= 0 ? dts[idx].nextElementSibling : null;
          const id = dd ? dd.textContent || '' : '';
          return id !== '' && id !== b;
        },
        sessionIdBefore,
        { timeout: 25000 }
      )
      .then(() => true)
      .catch(() => false);
    if (!newIdOk) fail('new session never produced a different session id');
    record('session.switch-new-id');
    await waitFor(
      page,
      () =>
        document.querySelectorAll('#transcript .msg').length === 0 &&
        document.querySelector('#transcript .empty-hint') !== null,
      'session switch never cleared the old transcript (empty new-session view missing)'
    );
    const staleTranscript = await page.evaluate(() => document.getElementById('transcript')?.textContent || '');
    if (staleTranscript.includes('hello from the web e2e')) {
      fail('session switch retained the old session messages in the transcript');
    }
    record('session.transcript-cleared');

    // ==================== Phase J: subagents panel ====================
    await page.click('#session-close-btn');
    await waitFor(page, () => document.getElementById('session-panel') === null, 'session panel did not close');
    await page.click('#subagents-toggle-btn');
    await waitFor(page, () => document.getElementById('subagents-panel') !== null, 'subagents panel did not open');
    await waitFor(
      page,
      () => document.getElementById('subagents-panel')?.querySelector('#subagents-spawn-btn') !== null,
      'subagents panel did not show the spawn form (orchestration disabled in fixture?)',
      15000
    );
    await page.selectOption('#subagents-agent-select', 'writer');
    await page.fill('#subagents-task-input', 'web-e2e-subagent: audit the release notes and report findings');
    await page.click('#subagents-spawn-btn');
    await waitFor(
      page,
      () =>
        [...document.querySelectorAll('.subagent-job')].some((card) =>
          (card.textContent || '').includes('audit the release notes')
        ),
      'spawned subagent job never appeared in the panel',
      30000
    );
    const subagentStatus = await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
      return card ? card.getAttribute('data-status') || '' : '';
    });
    if (!['queued', 'running'].includes(subagentStatus)) {
      fail(`spawned subagent must be live (queued/running) before cancel, got "${subagentStatus}"`);
    }
    record('subagent.spawn-live');
    const progressLine = await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
      const line = card ? card.querySelector('[data-progress-line]') : null;
      return line ? line.textContent || '' : '';
    });
    if (!progressLine.trim()) fail('spawned subagent never rendered a live activity/elapsed line');
    record('subagent.activity-line');
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
      if (!card) throw new Error('subagent card missing');
      const input = card.querySelector('.subagent-job__message-input');
      if (!input) throw new Error('message input missing');
      const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
      setter.call(input, 'status report?');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
        const btn = card ? card.querySelector('.subagent-job__message .subagent-job__action') : null;
        return !!btn && !btn.disabled;
      },
      'hub send button never enabled after typing a message'
    );
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
      card.querySelector('.subagent-job__message .subagent-job__action').click();
    });
    await waitFor(
      page,
      () => (document.querySelector('[data-panel-toast]')?.textContent || '').includes('message delivered'),
      'hub_send never reported a delivered receipt'
    );
    record('subagent.hub-send');
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
      const buttons = [...card.querySelectorAll('.subagent-job__action')];
      const output = buttons.find((b) => (b.textContent || '').includes('Output'));
      output.click();
    });
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
        return !!card && card.querySelector('[data-output-view]') !== null;
      },
      'job output pane never opened'
    );
    record('subagent.output-view');
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
      const buttons = [...card.querySelectorAll('.subagent-job__action')];
      const cancel = buttons.find((b) => (b.textContent || '').includes('Cancel'));
      cancel.click();
    });
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
        return !!card && card.getAttribute('data-status') === 'cancelled';
      },
      'subagent job never reached cancelled status',
      30000
    );
    record('subagent.cancel');

    // ---- subagents coverage: spawn via Enter + message via Enter ----
    // Spawn a second job via Enter (covers the task input onKeyDown). A
    // second live job also gives the panel more than one card.
    await page.fill('#subagents-task-input', 'web-e2e-subagent: second job for enter spawn');
    await page.press('#subagents-task-input', 'Enter');
    await waitFor(
      page,
      () =>
        [...document.querySelectorAll('.subagent-job')].some((card) =>
          (card.textContent || '').includes('second job for enter spawn')
        ),
      'second subagent job (spawned via Enter) never appeared',
      30000
    );
    record('subagent.spawn-via-enter');
    // Type a message and send it via Enter (covers the message input
    // onKeyDown -> sendMessage path).
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('second job for enter spawn'));
      if (!card) throw new Error('second subagent card missing');
      const input = card.querySelector('.subagent-job__message-input');
      if (!input) throw new Error('message input missing');
      const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
      setter.call(input, 'ping via enter');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await page.press('.subagent-job[data-status="running"] .subagent-job__message-input, .subagent-job[data-status="queued"] .subagent-job__message-input', 'Enter');
    await waitFor(
      page,
      () => (document.querySelector('[data-panel-toast]')?.textContent || '').includes('message delivered'),
      'message sent via Enter never reported a delivered receipt',
      30000
    );
    record('subagent.message-via-enter');
    // Best-effort: an incoming message_delivered event (child -> Main) would
    // refresh the job activity line via recordMessage. Wait briefly; do not
    // fail if the canned mock child does not reply within the window.
    await page.waitForTimeout(2500);

    // ============ Phase K: side chat + maintenance ============
    await page.click('#subagents-close-btn');
    await waitFor(page, () => document.getElementById('subagents-panel') === null, 'subagents panel did not close');
    await page.click('#sidechat-toggle-btn');
    await waitFor(page, () => document.querySelector('.side-chat') !== null, 'side chat panel did not open');
    await waitFor(
      page,
      () => document.querySelectorAll('.side-chat__tab').length >= 1,
      'side chat never showed the default tab'
    );
    record('sidechat.default-tab');
    await page.fill('.side-chat__new input', 'regression_tab');
    await page.click('.side-chat__new button');
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.side-chat__tab-select')).some((b) =>
          b.textContent.includes('regression_tab')
        ),
      'the new tab never appeared in the tab list'
    );
    record('sidechat.new-tab');
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.side-chat__tab-select')).some(
          (b) => b.textContent.includes('regression_tab') && b.getAttribute('aria-selected') === 'true'
        ),
      'the new tab was not activated'
    );
    record('sidechat.tab-activated');
    await page.fill('.side-chat__composer textarea', 'hello side agent');
    await page.click('.side-chat__composer button');
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.side-chat__entry--assistant .side-chat__text')).some((el) =>
          el.textContent.trim() !== ''
        ),
      'side-chat assistant entry never rendered a reply',
      90000
    );
    const sideEntry = await page.evaluate(() => {
      const entries = Array.from(document.querySelectorAll('.side-chat__entry--assistant'));
      const last = entries[entries.length - 1];
      return last
        ? { text: last.querySelector('.side-chat__text')?.textContent || '', error: last.classList.contains('side-chat__entry--error') }
        : null;
    });
    if (!sideEntry) fail('side-chat assistant entry missing');
    if (sideEntry.error) fail(`side-chat turn ended in an error entry: ${sideEntry.text}`);
    record('sidechat.prompt-reply');
    await page.click('.side-chat .panel-close');
    await waitFor(page, () => document.querySelector('.side-chat') === null, 'side chat panel did not close');
    // Prime the active (post-switch) session with two user turns so snapcompact
    // has something to archive: the steering fixture lowers
    // compaction.snapKeepTurns to 1 (coverage.sh settings.json), so exactly two
    // user turns let find_snap_cut_point land a cut (mirrors E2E.d/web/extras.sh).
    // Mock parity is not relied on past request 4, so accept any non-empty reply.
    for (let i = 0; i < 2; i += 1) {
      // Wait until the main session is idle before prompting: a prompt sent
      // while a run is in flight is rejected by the turn-gate (not queued), so
      // each priming turn must only be issued once #stream-badge clears.
      await waitFor(
        page,
        () => document.getElementById('stream-badge').hidden === true,
        `main session still streaming before priming turn ${i + 1}`,
        30000
      );
      const beforePrime = await page.evaluate(
        () => document.querySelectorAll('.msg--assistant .assistant-text').length
      );
      await page.fill('#prompt-input', `prime turn ${i + 1}`);
      await page.press('#prompt-input', 'Enter');
      // Wait for a new assistant message AND for the turn to finish (streaming
      // cleared) so the next priming prompt is accepted and snapcompact sees a
      // complete user+assistant pair to archive.
      await waitFor(
        page,
        (before) => {
          const nodes = [...document.querySelectorAll('.msg--assistant .assistant-text')];
          return (
            nodes.length > before &&
            (nodes[nodes.length - 1]?.textContent || '').trim() !== '' &&
            document.getElementById('stream-badge').hidden === true
          );
        },
        `priming turn ${i + 1} never completed before snapcompact`,
        30000,
        beforePrime
      );
    }
    await page.click('#maintenance-toggle-btn');
    await waitFor(page, () => document.querySelector('.maintenance') !== null, 'maintenance panel did not open');
    const clickMaintenanceAction = async (label) => {
      const clicked = await page.evaluate((want) => {
        const btn = Array.from(document.querySelectorAll('.maintenance__action')).find((b) => b.textContent.includes(want));
        if (!btn) return false;
        btn.click();
        return true;
      }, label);
      if (!clicked) fail(`maintenance action "${label}" not found`);
    };
    await clickMaintenanceAction('Snapcompact');
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.maintenance__result')).some((r) =>
          r.textContent.includes('estimated tokens')
        ),
      'snapcompact never rendered the A→B token report'
    );
    const ab = await page.evaluate(() =>
      Array.from(document.querySelectorAll('.maintenance__result'))
        .map((r) => r.textContent)
        .join(' | ')
    );
    if (!ab.includes('→')) fail(`snapcompact report has no A→B arrow: ${ab}`);
    record('maintenance.snapcompact-ab');
    await clickMaintenanceAction('Rewind…');
    await waitFor(
      page,
      () =>
        document.querySelector('.maintenance__list') !== null ||
        Array.from(document.querySelectorAll('.maintenance__result')).some((r) => r.textContent.includes('rewind')),
      'rewind list never appeared'
    );
    record('maintenance.rewind-list');
    await clickMaintenanceAction('Handoff');
    await waitFor(page, () => document.querySelector('.maintenance__handoff') !== null, 'handoff envelope never rendered');
    record('maintenance.handoff');
    await clickMaintenanceAction('Queue…');
    await waitFor(page, () => document.querySelector('.maintenance__queue') !== null, 'queue view never rendered');
    await clickMaintenanceAction('Cancel queue');
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.maintenance__result')).some((r) => r.textContent.includes('Cancelled')),
      'queue cancel never reported'
    );
    record('maintenance.queue-cancel');

    // ---- App coverage: maintenance panel close (onClosePanel) ----
    // Close the maintenance panel via its own close button (covers App's
    // maintenance onClosePanel callback, distinct from a panel-toggle swap).
    await page.click('.maintenance .panel-close');
    await waitFor(page, () => document.querySelector('.maintenance') === null, 'maintenance panel did not close via its close button');
    record('app.maintenance-close');
    // The dedicated Steer/Follow up composer controls were removed (the
    // primary Send/Steer button + Enter already cover both verbs, and Abort
    // is active-only), so there are no #steer-btn/#followup-btn clicks to
    // record here. The primary submit's send/steer labels are asserted in
    // Phase A (app.primary-submit-send / app.primary-submit-steer).

    // ==================== Phase M: mobile viewport ====================
    const mobile = await browser.newPage({ viewport: { width: 375, height: 667 } });
    await connectPage(mobile);
    await mobile.fill('#prompt-input', 'hello from a phone');
    await mobile.press('#prompt-input', 'Enter');
    await waitFor(
      mobile,
      () => {
        const nodes = [...document.querySelectorAll('.msg--assistant .assistant-text')];
        return nodes.length > 0 && (nodes[nodes.length - 1]?.textContent || '').trim() !== '';
      },
      'mobile prompt never round-tripped',
      60000
    );
    await waitFor(
      mobile,
      () => document.getElementById('send-btn')?.textContent?.trim() === 'Send' && document.getElementById('abort-btn') === null,
      'mobile composer never returned to idle after prompt',
      60000
    );
    const toggleDisplay = await mobile.evaluate(
      () => getComputedStyle(document.getElementById('sidebar-toggle-btn')).display
    );
    if (toggleDisplay === 'none') fail('#sidebar-toggle-btn is hidden at phone width');
    const toggleBox = await mobile.locator('#sidebar-toggle-btn').boundingBox();
    if (!toggleBox || toggleBox.width < 20 || toggleBox.height < 20) {
      fail(`#sidebar-toggle-btn has no clickable area at phone width (box ${JSON.stringify(toggleBox)})`);
    }
    record('mobile.sidebar-toggle-visible');
    await mobile.click('#sidebar-toggle-btn');
    await waitFor(
      mobile,
      () => {
        const layout = document.querySelector('.app-layout');
        const sidebar = document.querySelector('.session-sidebar');
        return (
          layout !== null &&
          layout.classList.contains('app-layout--drawer-open') &&
          sidebar !== null &&
          (sidebar.getBoundingClientRect().left || -1) <= 1
        );
      },
      'session sidebar drawer never opened from the hamburger'
    );
    record('mobile.drawer-opens');
    // The mobile drawer overlays the app while open. Close it before measuring
    // the composer's usable width; the dedicated mobile lane separately checks
    // the full-screen drawer contract while measuring the idle composer from
    // the Todo panel overlay.
    await mobile.evaluate(() => document.getElementById('sidebar-toggle-btn')?.click());
    await waitFor(
      mobile,
      () => !document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open'),
      'session sidebar drawer never closed before composer measurement'
    );
    const metrics = await mobile.evaluate(() => {
      const rect = (sel) => {
        const el = document.querySelector(sel);
        if (!el) return null;
        const r = el.getBoundingClientRect();
        return { top: r.top, bottom: r.bottom, left: r.left, width: r.width, height: r.height };
      };
      const footer = rect('footer');
      const ta = rect('#prompt-input');
      // #abort-btn is active-only, so it is NOT in the DOM while idle; its
      // 44px touch target is exercised while streaming (Phase A abort path).
      const targets = ['#send-btn', '#connect-btn', '#todos-toggle-btn'].map((sel) => ({
        sel,
        height: rect(sel) ? rect(sel).height : -1,
      }));
      const thinking = document.getElementById('thinking-select');
      return {
        innerWidth: window.innerWidth,
        innerHeight: window.innerHeight,
        scrollWidth: document.documentElement.scrollWidth,
        composerBottom: footer ? footer.bottom : -1,
        textareaW: ta ? ta.width : -1,
        textareaH: ta ? ta.height : -1,
        targets,
        abortPresent: document.querySelector('#abort-btn') !== null,
        thinkingDisplay: thinking ? getComputedStyle(thinking).display : 'missing',
      };
    });
    if (metrics.scrollWidth > metrics.innerWidth + 1) {
      fail(`horizontal overflow: scrollWidth ${metrics.scrollWidth} > viewport ${metrics.innerWidth}`);
    }
    record('mobile.viewport-no-hscroll');
    if (metrics.composerBottom < 0 || metrics.composerBottom > metrics.innerHeight) {
      fail(`composer sits below the fold: bottom ${metrics.composerBottom} > innerHeight ${metrics.innerHeight}`);
    }
    record('mobile.composer-above-fold');
    // Idle usable composer: with the dedicated Steer/Follow up controls gone
    // and Abort active-only, the textarea must be the dominant composer
    // element on a phone and keep a usable entry height.
    if (metrics.textareaW < 240) {
      fail(`#prompt-input usable width is ${metrics.textareaW}px at 375px (must be >= 240px)`);
    }
    if (metrics.textareaH < 44) {
      fail(`#prompt-input usable height is ${metrics.textareaH}px (must be >= 44px)`);
    }
    if (metrics.abortPresent) {
      fail('#abort-btn must not render while idle (active-only composer); found in DOM');
    }
    for (const t of metrics.targets) {
      if (t.height < 44) fail(`touch target ${t.sel} is ${t.height}px (must be >= 44px)`);
    }
    record('mobile.touch-targets-44');
    if (metrics.thinkingDisplay !== 'none') {
      fail(`#thinking-select should be hidden at phone width, computed display = ${metrics.thinkingDisplay}`);
    }
    record('mobile.thinking-hidden');

    // ================ Phase N: second page concurrency ================
    const second = await browser.newPage();
    await connectPage(second);
    const bothOn = await page.evaluate(() => document.getElementById('conn-state').dataset.state === 'on');
    if (!bothOn) fail('first page dropped its connection while the second page connected');
    await second.close();
    record('concurrency.second-page-connects');

    // ============ Phase O: desktop panel navigation ============
    // nav.panel-open-close: header toggles open/close a panel round-trip.
    await page.click('#todos-toggle-btn');
    await waitFor(page, () => document.getElementById('todo-panel') !== null, 'todo panel did not open (nav)');
    await page.click('#todo-close-btn');
    await waitFor(page, () => document.getElementById('todo-panel') === null, 'todo panel did not close (nav)');
    record('nav.panel-open-close');

    // ---- evidence ----
    fs.mkdirSync(evidence, { recursive: true });
    fs.writeFileSync(path.join(evidence, 'coverage-assertions.json'), JSON.stringify({ executed: [...executed] }, null, 2));
    console.log(`web-cov: PASSED (${executed.size} assertions) — evidence at ${path.join(evidence, 'coverage-assertions.json')}`);
  } finally {
    if (rpc) rpc.close();
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-cov: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
