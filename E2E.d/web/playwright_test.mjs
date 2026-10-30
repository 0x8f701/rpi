// Web client E2E (playwright lane of E2E.d/web/run.sh).
//
// Environment:
//   RPI_URL         http://127.0.0.1:<port>/web
//   RPI_TOKEN       token file content (served via rpi-auth.<token> subprotocol)
//   RPI_SLOW_TAIL   tail of the slow mock reply ("chunk-four-done")
//   RPI_FAST_REPLY  instant mock reply text ("steering-followup-reply")
//   RPI_ABORTED_TAIL final chunk that must NEVER render after abort ("-done")
//   RPI_CHROME      executable path of the system Chrome (optional)
//   RPI_EVIDENCE    evidence dir for screenshots
//
// Asserts: page loads, WS connects (subprotocol), full prompt round-trip
// streams into the DOM, abort cuts a slow stream, later prompt recovers, the
// Todo DAG panel creates/completes/reopens a task with live state, the rich
// content renderer (table/task-list/mermaid/KaTeX), the Workflow panel
// creates a workflow (live status) then cancels it, the Settings panel
// browses by category (secret keys redacted + not editable), edits `theme`
// in a draft, applies, and reflects the persisted value, the Session panel
// renders session info, renames the session (panel + header), lists saved
// sessions, and switches to a new session, the Subagents panel spawns and
// cancels a live job, and the structured Task tool card renders Goal/
// Constraints/Contract + child without raw JSON.

import { chromium } from 'playwright';
import fs from 'node:fs';
import { assertSidebarNewActionAbsent, createFreshSessionViaManage } from './lib/session-lifecycle.mjs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const slowTail = process.env.RPI_SLOW_TAIL || 'chunk-four-done';
const fastReply = process.env.RPI_FAST_REPLY || 'steering-followup-reply';
const abortedTail = process.env.RPI_ABORTED_TAIL || '-done';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

function fail(message) {
  // Exit 2 (not 1): 1 is reserved for playwright SETUP failure (node/
  // chromium/npm), which must NOT be confused with an assertion failure (the
  // mock's request counter is stateful and a rerun would see shifted
  // replies). Any non-1 exit fails the lane.
  console.error(`web: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch (err) {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}
function observePageErrors(page, label, pageErrors, consoleErrors) {
  page.on('pageerror', (error) => pageErrors.push(`${label}: ${error.message}`));
  page.on('console', (message) => {
    if (message.type() === 'error') consoleErrors.push(`${label}: ${message.text()}`);
  });
}

function observeTurnFrames(page) {
  const starts = [];
  const terminals = new Map();
  const terminalWaiters = new Map();
  page.on('websocket', (socket) => {
    socket.on('framereceived', (frame) => {
      let event;
      try { event = JSON.parse(String(frame.payload)); } catch { return; }
      if (event?.type === 'turn_started' && typeof event.sessionId === 'string' && typeof event.turnId === 'string') {
        starts.splice(0).forEach((resolve) => resolve({ sessionId: event.sessionId, turnId: event.turnId }));
      }
      if (event?.type === 'turn_terminal' && typeof event.sessionId === 'string' && typeof event.turnId === 'string') {
        const key = `${event.sessionId}:${event.turnId}`;
        terminals.set(key, event.status);
        const waiters = terminalWaiters.get(key) || [];
        terminalWaiters.delete(key);
        for (const resolve of waiters) resolve(event.status);
      }
    });
  });
  return {
    nextStart: () => new Promise((resolve) => starts.push(resolve)),
    waitForTerminal: (sessionId, turnId, timeoutMs = 30000) => {
      const key = `${sessionId}:${turnId}`;
      const existing = terminals.get(key);
      if (existing !== undefined) return Promise.resolve(existing);
      return new Promise((resolve, reject) => {
        const waiters = terminalWaiters.get(key) || [];
        waiters.push(resolve);
        terminalWaiters.set(key, waiters);
        setTimeout(() => reject(new Error(`turn ${turnId} did not settle`)), timeoutMs);
      });
    },
  };
}

async function lastAssistantText(page) {
  return page.evaluate(() => {
    const nodes = document.querySelectorAll('.msg--assistant .assistant-text');
    return nodes.length ? nodes[nodes.length - 1].textContent : '';
  });
}

const LIVE_WORKFLOW_STATUSES = new Set(['queued', 'planning', 'running', 'paused', 'integrating']);
const DELIVERY_OUTCOMES = new Set(['queued', 'woken', 'revived', 'processed', 'failed']);

function rpcClient(page, wsUrl) {
  let sequence = 0;
  return {
    ready: Promise.resolve(),
    call(command) {
      const id = `workflow-e2e-${++sequence}`;
      return page.evaluate(
        ({ wsUrl: target, token: authToken, command: request, id: requestId }) => new Promise((resolve, reject) => {
          const sessionId = document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '';
          if (!sessionId) {
            reject(new Error('active sessionId missing for workflow RPC'));
            return;
          }
          const protocols = authToken ? [`rpi-auth.${authToken}`] : [];
          const ws = new WebSocket(target, protocols);
          const timer = window.setTimeout(() => {
            ws.close();
            reject(new Error(`rpc timed out: ${request.type}`));
          }, 30000);
          ws.addEventListener('open', () => ws.send(JSON.stringify({ ...request, sessionId, id: requestId })));
          ws.addEventListener('message', (event) => {
            let frame;
            try {
              frame = JSON.parse(String(event.data));
            } catch {
              return;
            }
            if (frame?.type !== 'response' || frame.id !== requestId) return;
            window.clearTimeout(timer);
            ws.close();
            if (frame.success) resolve(frame.data || {});
            else reject(new Error(frame.error || 'rpc failed'));
          });
          ws.addEventListener('error', () => {
            window.clearTimeout(timer);
            reject(new Error(`rpc transport failed: ${request.type}`));
          });
        }),
        { wsUrl, token, command, id }
      );
    },
    close() {},
  };
}

function requireWorkflowSnapshot(value, expectedName, label) {
  if (!value || typeof value !== 'object') fail(`${label}: backend returned no workflow snapshot`);
  const snapshot = value;
  if (typeof snapshot.workflowId !== 'string' || snapshot.workflowId === '') fail(`${label}: workflowId missing`);
  if (snapshot.name !== expectedName) fail(`${label}: expected name ${expectedName}, got ${JSON.stringify(snapshot.name)}`);
  if (!Number.isSafeInteger(snapshot.generation) || snapshot.generation < 1) fail(`${label}: generation must be a positive integer`);
  if (typeof snapshot.worktree !== 'string' || snapshot.worktree === '') fail(`${label}: projected worktree label missing`);
  if (typeof snapshot.branch !== 'string' || snapshot.branch === '') fail(`${label}: projected branch missing`);
  if (snapshot.worktree.startsWith('/') || snapshot.worktree.startsWith('\\')) fail(`${label}: worktree projection leaked an absolute path`);
  if (!snapshot.branch.includes(snapshot.workflowId)) fail(`${label}: branch does not carry its workflowId: ${snapshot.branch}`);
  if (!LIVE_WORKFLOW_STATUSES.has(snapshot.status)) fail(`${label}: workflow is not live: ${snapshot.status}`);
  return snapshot;
}

async function waitForWorkflowSnapshot(rpc, workflowId, expectedName, label) {
  const deadline = Date.now() + 30000;
  let last = 'snapshot missing';
  while (Date.now() < deadline) {
    try {
      const snapshot = await rpc.call({ type: 'workflow_get', workflowId });
      if (
        snapshot?.workflowId === workflowId && snapshot.name === expectedName &&
        Number.isSafeInteger(snapshot.generation) && snapshot.generation > 0 &&
        typeof snapshot.worktree === 'string' && snapshot.worktree !== '' &&
        typeof snapshot.branch === 'string' && snapshot.branch !== '' &&
        LIVE_WORKFLOW_STATUSES.has(snapshot.status)
      ) return requireWorkflowSnapshot(snapshot, expectedName, label);
      last = JSON.stringify(snapshot);
    } catch (err) {
      last = err instanceof Error ? err.message : String(err);
    }
    await new Promise((resolve) => setTimeout(resolve, 150));
  }
  fail(`${label}: workflow never returned live with worktree evidence: ${last}`);
}

function requireTypedReceipt(value, snapshot, expectedRecipient, expectedOutcome, label) {
  if (value?.workflowId !== snapshot.workflowId || value?.generation !== snapshot.generation) {
    fail(`${label}: receipt envelope identity mismatch: ${JSON.stringify(value)}`);
  }
  if (value.recipient !== expectedRecipient || !Array.isArray(value.receipts) || value.receipts.length !== 1) {
    fail(`${label}: malformed receipt envelope: ${JSON.stringify(value)}`);
  }
  const receipt = value.receipts[0];
  if (typeof receipt.to !== 'string' || !DELIVERY_OUTCOMES.has(receipt.outcome)) {
    fail(`${label}: receipt is not typed: ${JSON.stringify(receipt)}`);
  }
  if (expectedOutcome === 'delivered' && receipt.outcome === 'failed') fail(`${label}: delivery failed: ${JSON.stringify(receipt)}`);
  if (expectedOutcome === 'failed' && (receipt.outcome !== 'failed' || typeof receipt.error !== 'string' || receipt.error === '')) {
    fail(`${label}: expected a typed failed receipt with an error: ${JSON.stringify(receipt)}`);
  }
  if (typeof receipt.messageId !== 'string' || receipt.messageId === '') fail(`${label}: receipt messageId missing`);
  return receipt;
}

async function requireRpcFailure(promise, label) {
  try {
    await promise;
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    if (!message.includes('stale') || !message.includes('generation')) {
      fail(`${label}: wrong error ${JSON.stringify(message)}`);
    }
    return;
  }
  fail(`${label}: RPC unexpectedly succeeded`);
}

async function waitForWorkflowDetail(rpc, snapshot, label) {
  const deadline = Date.now() + 30000;
  let lastError = 'detail was never returned';
  while (Date.now() < deadline) {
    try {
      const detail = await rpc.call({ type: 'workflow_detail', workflowId: snapshot.workflowId });
      const todoTasks = Array.isArray(detail.todo)
        ? detail.todo.flatMap((phase) => Array.isArray(phase?.tasks) ? phase.tasks : [])
        : [];
      if (
        detail.id === snapshot.workflowId &&
        detail.generation === snapshot.generation &&
        detail.name === snapshot.name &&
        detail.objective === snapshot.objective &&
        LIVE_WORKFLOW_STATUSES.has(detail.status) &&
        detail.worktree?.label === snapshot.worktree &&
        detail.worktree?.branch === snapshot.branch &&
        typeof detail.supervisor?.name === 'string' && detail.supervisor.name !== '' &&
        Array.isArray(detail.planningActivity) && detail.planningActivity.length > 0 &&
        todoTasks.length > 0 &&
        Array.isArray(detail.subagents) && detail.subagents.length > 0 &&
        Array.isArray(detail.jobs) && detail.jobs.length > 0 &&
        Array.isArray(detail.activeTasks) && detail.activeTasks.length > 0
      ) return detail;
      lastError = JSON.stringify(detail);
    } catch (err) {
      lastError = err instanceof Error ? err.message : String(err);
    }
    await new Promise((resolve) => setTimeout(resolve, 150));
  }
  fail(`${label}: live supervisor/workers/tasks/planning detail never converged: ${lastError}`);
}

function requireWorkflowDetailIsolation(alpha, beta, alphaDetail, betaDetail) {
  const alphaEncoded = JSON.stringify(alphaDetail);
  const betaEncoded = JSON.stringify(betaDetail);
  for (const marker of [beta.workflowId, beta.name, beta.objective, beta.worktree, beta.branch]) {
    if (alphaEncoded.includes(marker)) fail(`workflow alpha detail crossed with beta marker ${marker}`);
  }

  for (const marker of [alpha.workflowId, alpha.name, alpha.objective, alpha.worktree, alpha.branch]) {
    if (betaEncoded.includes(marker)) fail(`workflow beta detail crossed with alpha marker ${marker}`);
  }
  const alphaIdentity = `${alpha.workflowId}@${alpha.generation}`;
  const betaIdentity = `${beta.workflowId}@${beta.generation}`;
  if (alphaIdentity === betaIdentity) fail(`workflow runtime identities collided: ${alphaIdentity}`);
}
async function waitForWorkflowIrc(rpc, workflow, text, label) {
  const deadline = Date.now() + 30000;
  let last = 'detail missing';
  while (Date.now() < deadline) {
    try {
      const detail = await rpc.call({ type: 'workflow_detail', workflowId: workflow.workflowId });
      const identityMatches = detail.id === workflow.workflowId && detail.generation === workflow.generation;
      const messages = Array.isArray(detail.recentIrc) ? detail.recentIrc : [];
      if (identityMatches && (text === null || messages.some((message) => message.text.includes(text)))) return detail;
      last = JSON.stringify(detail);
    } catch (err) {
      last = err instanceof Error ? err.message : String(err);
    }
    await new Promise((resolve) => setTimeout(resolve, 150));
  }
  fail(`${label}: exact workflow IRC detail never appeared: ${last}`);
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath
    ? { executablePath: chromePath }
    : {};
  const browser = await chromium.launch(launchOptions);
  let rpc;
  try {
    const page = await browser.newPage();
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    const pageErrors = [];
    const consoleErrors = [];
    observePageErrors(page, 'desktop', pageErrors, consoleErrors);
    const turnFrames = observeTurnFrames(page);

    // 1. Page loads: GET /web serves the self-contained client.
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    // 2. WS connects via boot localStorage token (rpi-auth.<token>).
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'WS did not reach "connected" (wrong token or listener?)'
    );

    // 3. Prompt round-trip streams the FULL slow reply into the DOM.
    await page.fill('#prompt-input', 'hello from the web e2e');
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, (tail) => document.body.textContent.includes(tail), 'full slow reply never streamed into the DOM', 30000, slowTail);
    await waitFor(
      page,
      () => document.getElementById('activity-strip') === null,
      'activity strip did not unmount after the reply completed'
    );
    await page.screenshot({ path: `${evidence}/roundtrip.png`, fullPage: true });

    // Second prompt hits the instant path of the mock (request 2).
    await page.fill('#prompt-input', 'again');
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, (reply) => document.body.textContent.includes(reply), 'fast reply never streamed into the DOM', 30000, fastReply);
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message',
      'composer not idle after the fast reply',
    );

    // 4. Unified Stop action aborts the slow stream (request 3) mid-flight.
    await page.fill('#prompt-input', 'stream a long answer');
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, () => document.body.textContent.includes('steer-3-'), 'third stream never started');
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Stop generating',
      'unified composer action never switched to Stop'
    );
    await page.click('#send-btn');
    await waitFor(
      page,
      () => document.getElementById('activity-strip') === null,
      'activity strip did not unmount after abort'
    );
    const abortedText = await lastAssistantText(page);
    if (abortedText.includes(abortedTail)) {
      fail(`aborted stream still rendered the final chunks: ${abortedText}`);
    }
    await page.screenshot({ path: `${evidence}/abort.png`, fullPage: true });

    // Recovery: the rpi's turn gate takes a moment to release after an
    // abort; without this settle the next prompt is accepted but never
    // reaches the provider. Then request 4 must round-trip as a NEW
    // assistant message (count-based: the reply text equals request 2's, so
    // a textContent check would stale-pass on the old DOM content).
    await page.waitForTimeout(6000);
    const assistantCountBeforeRecovery = await page.evaluate(
      () => document.querySelectorAll('.msg--assistant .assistant-text').length
    );
    await page.fill('#prompt-input', 'recovery prompt');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (before) => {
        const nodes = [...document.querySelectorAll('.msg--assistant .assistant-text')];
        return nodes.length > before && (nodes[nodes.length - 1]?.textContent || '').includes('steering-followup-reply');
      },
      'post-abort prompt did not round-trip as a new message',
      30000,
      assistantCountBeforeRecovery
    );

    // 5. Unified Todo module: open it, add a task (todo_op append), complete
    //    it (todo_op done), reopen it (todo_op start), assert panel state.
    await page.click('#todo-toggle-btn');
    await waitFor(page, () => {
      const panels = document.querySelectorAll('#todo-panel');
      const panel = panels.length === 1 ? panels[0] : null;
      return panel !== null
        && panel.querySelector('.todo-panel__tasks') !== null
        && panel.querySelector('.todo-child-section') !== null;
    }, 'Unified Todo module did not open with task and child sections');
    if (await page.$('#todos-toggle-btn')) fail('legacy todos-toggle-btn rendered');
    if (await page.$('#todo-child-toggle-btn')) fail('legacy todo-child-toggle-btn rendered');
    await page.fill('#todo-add-phase', 'Plan');
    await page.fill('#todo-add-content', 'web e2e task');
    await page.click('#todo-add-btn');
    await waitFor(
      page,
      () => {
        const rows = [...document.querySelectorAll('.todo-task')];
        return rows.some((row) => row.textContent.includes('web e2e task'));
      },
      'added task never appeared in the todo panel'
    );
    const taskStatus = () =>
      page.evaluate(() => {
        const rows = [...document.querySelectorAll('.todo-task')];
        const row = rows.find((r) => r.textContent.includes('web e2e task'));
        if (!row) return '';
        return row.querySelector('.todo-task__bullet')?.getAttribute('aria-label') || '';
      });
    const created = await taskStatus();
    if (created !== 'pending' && created !== 'in_progress') {
      fail(`new task must be pending or in_progress, got "${created}"`);
    }
    // Complete via the row action.
    await page.evaluate(() => {
      const rows = [...document.querySelectorAll('.todo-task')];
      const row = rows.find((r) => r.textContent.includes('web e2e task'));
      if (!row) throw new Error('task row missing');
      const btn = row.querySelector('.todo-task__action[data-action="complete"]');
      if (!btn) throw new Error('complete button missing');
      btn.click();
    });
    await waitFor(page, () => {
      const rows = [...document.querySelectorAll('.todo-task')];
      const row = rows.find((r) => r.textContent.includes('web e2e task'));
      return !!row && row.querySelector('.todo-task__bullet')?.getAttribute('aria-label') === 'completed';
    }, 'task never reached completed status');
    await waitFor(
      page,
      () => (document.getElementById('todo-counts')?.textContent || '').includes('1 done'),
      'counts never reflected the completed task'
    );
    // Reopen the completed task (todo_op start) and assert in_progress.
    await page.evaluate(() => {
      const rows = [...document.querySelectorAll('.todo-task')];
      const row = rows.find((r) => r.textContent.includes('web e2e task'));
      if (!row) throw new Error('task row missing');
      const btn = row.querySelector('.todo-task__action[data-action="reopen"]');
      if (!btn) throw new Error('reopen button missing');
      btn.click();
    });
    await waitFor(page, () => {
      const rows = [...document.querySelectorAll('.todo-task')];
      const row = rows.find((r) => r.textContent.includes('web e2e task'));
      return !!row && row.querySelector('.todo-task__bullet')?.getAttribute('aria-label') === 'in_progress';
    }, 'reopened task never returned to in_progress');
    // Detail pane: select the task and assert dependency + ready metadata.
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
    await page.screenshot({ path: `${evidence}/todo-panel.png`, fullPage: true });

    // 5b. Todo header counts layout contract (real DOM geometry): every
    // `N label` counter is atomic — the digit and label never split across
    // lines — the WHOLE counts line stays on one row at desktop (the panel
    // is widened to 460px so it fits), and the close chip shares the head
    // row without being squeezed out.
    const countsLayout = await page.evaluate(() => {
      const countsEl = document.getElementById('todo-counts');
      const closeBtn = document.getElementById('todo-close-btn');
      const panel = document.getElementById('todo-panel');
      if (!countsEl || !closeBtn || !panel) return null;
      const lineHeight = parseFloat(getComputedStyle(countsEl).lineHeight) || 0;
      const countsBox = countsEl.getBoundingClientRect();
      const panelBox = panel.getBoundingClientRect();
      const closeBox = closeBtn.getBoundingClientRect();
      // Atom rects: measure every `<digits> <label>` text run (the JSX
      // renders each counter as its own text node, so map match offsets onto
      // the text nodes and use a Range to get the run's real box).
      const text = countsEl.textContent || '';
      const walker = document.createTreeWalker(countsEl, NodeFilter.SHOW_TEXT);
      const nodes = [];
      let node;
      while ((node = walker.nextNode())) nodes.push(node);
      let acc = 0;
      const runs = nodes.map((n) => {
        const r = { node: n, start: acc, end: acc + n.textContent.length };
        acc = r.end;
        return r;
      });
      const atoms = [];
      for (const m of text.matchAll(/\d+\s+\S+/g)) {
        const start = m.index;
        const end = start + m[0].length;
        let sNode = null;
        let sOff = 0;
        let eNode = null;
        let eOff = 0;
        for (const run of runs) {
          if (sNode === null && start < run.end) { sNode = run.node; sOff = start - run.start; }
          if (end <= run.end) { eNode = run.node; eOff = end - run.start; break; }
        }
        if (sNode && eNode) {
          const range = document.createRange();
          range.setStart(sNode, sOff);
          range.setEnd(eNode, eOff);
          const r = range.getBoundingClientRect();
          atoms.push({ text: m[0], height: r.height });
        }
      }
      return {
        countsText: text,
        lineHeight,
        countsHeight: countsBox.height,
        countsSingleLine: countsBox.height <= lineHeight * 1.5,
        atoms,
        atomsSingleLine: atoms.every((a) => a.height <= lineHeight * 1.5),
        panelWidth: panelBox.width,
        closeInsidePanel: closeBox.right <= panelBox.right - 1 && closeBox.left >= panelBox.left,
        closeSharesHeadRow: Math.abs(closeBox.top + closeBox.height / 2 - (countsBox.top + countsBox.height / 2)) < lineHeight,
      };
    });
    if (!countsLayout) fail('todo panel header (counts/close) missing');
    if (countsLayout.atoms.length < 4) fail(`todo counts atoms missing: "${countsLayout.countsText}"`);
    if (!countsLayout.countsSingleLine) {
      fail(`todo counts wrapped at desktop: "${countsLayout.countsText}" height ${countsLayout.countsHeight} vs line-height ${countsLayout.lineHeight}`);
    }
    if (!countsLayout.atomsSingleLine) {
      fail(`todo counts atom split across lines at desktop: ${JSON.stringify(countsLayout.atoms)}`);
    }
    const doneAtom = countsLayout.atoms.find((a) => a.text.endsWith('done'));
    if (!doneAtom) fail('todo counts missing the "N done" atom');
    if (countsLayout.panelWidth < 440) {
      fail(`todo panel not widened on desktop: ${countsLayout.panelWidth}px (counts need a single line)`);
    }
    if (!countsLayout.closeInsidePanel || !countsLayout.closeSharesHeadRow) {
      fail(`todo close chip squeezed or misplaced: inside=${countsLayout.closeInsidePanel} sameRow=${countsLayout.closeSharesHeadRow}`);
    }

    // 5c. The same contract at 390×844 mobile: the counts line MAY wrap
    // between counters on phones, but every `N label` atom (including
    // `0 done`) stays intact and the drawer never overflows the viewport.
    const mobilePage = await browser.newPage({ viewport: { width: 390, height: 844 } });
    if (token) {
      await mobilePage.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    observePageErrors(mobilePage, 'mobile', pageErrors, consoleErrors);
    await mobilePage.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(mobilePage, () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true, 'mobile WS did not connect');
    // Panel toggles live inside the session-sidebar drawer on phones.
    await mobilePage.click('#sidebar-toggle-btn');
    await waitFor(
      mobilePage,
      () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open'),
      'mobile sidebar drawer never opened'
    );
    await mobilePage.click('#todo-toggle-btn');
    await waitFor(mobilePage, () => {
      const panels = document.querySelectorAll('#todo-panel');
      const panel = panels.length === 1 ? panels[0] : null;
      return panel !== null
        && panel.querySelector('.todo-panel__tasks') !== null
        && panel.querySelector('.todo-child-section') !== null;
    }, 'mobile unified Todo module did not open with task and child sections');
    const mobileLayout = await mobilePage.evaluate(() => {
      const countsEl = document.getElementById('todo-counts');
      const closeBtn = document.getElementById('todo-close-btn');
      const panel = document.getElementById('todo-panel');
      if (!countsEl || !closeBtn || !panel) return null;
      const lineHeight = parseFloat(getComputedStyle(countsEl).lineHeight) || 0;
      const countsBox = countsEl.getBoundingClientRect();
      const panelBox = panel.getBoundingClientRect();
      const closeBox = closeBtn.getBoundingClientRect();
      const text = countsEl.textContent || '';
      const walker = document.createTreeWalker(countsEl, NodeFilter.SHOW_TEXT);
      const nodes = [];
      let node;
      while ((node = walker.nextNode())) nodes.push(node);
      let acc = 0;
      const runs = nodes.map((n) => {
        const r = { node: n, start: acc, end: acc + n.textContent.length };
        acc = r.end;
        return r;
      });
      const atoms = [];
      for (const m of text.matchAll(/\d+\s+\S+/g)) {
        const start = m.index;
        const end = start + m[0].length;
        let sNode = null;
        let sOff = 0;
        let eNode = null;
        let eOff = 0;
        for (const run of runs) {
          if (sNode === null && start < run.end) { sNode = run.node; sOff = start - run.start; }
          if (end <= run.end) { eNode = run.node; eOff = end - run.start; break; }
        }
        if (sNode && eNode) {
          const range = document.createRange();
          range.setStart(sNode, sOff);
          range.setEnd(eNode, eOff);
          const r = range.getBoundingClientRect();
          atoms.push({ text: m[0], height: r.height });
        }
      }
      return {
        countsText: text,
        lineHeight,
        atoms,
        atomsSingleLine: atoms.every((a) => a.height <= lineHeight * 1.5),
        panelWidth: panelBox.width,
        countsRight: countsBox.right,
        panelRight: panelBox.right,
        closeInsidePanel: closeBox.right <= panelBox.right - 1 && closeBox.left >= panelBox.left,
        closeSharesHeadRow: Math.abs(closeBox.top + closeBox.height / 2 - (countsBox.top + countsBox.height / 2)) < lineHeight,
        scrollWidth: document.documentElement.scrollWidth,
        innerWidth: window.innerWidth,
      };
    });
    await mobilePage.close();
    if (!mobileLayout) fail('mobile todo panel header (counts/close) missing');
    if (mobileLayout.atoms.length < 4) fail(`mobile todo counts atoms missing: "${mobileLayout.countsText}"`);
    if (!mobileLayout.atomsSingleLine) {
      fail(`mobile todo counts atom split across lines at 390px: ${JSON.stringify(mobileLayout.atoms)}`);
    }
    const mobileDoneAtom = mobileLayout.atoms.find((a) => a.text.endsWith('done'));
    if (!mobileDoneAtom) fail('mobile todo counts missing the "N done" atom');
    if (mobileLayout.panelWidth < mobileLayout.innerWidth - 1) {
      fail(`mobile todo panel not full-screen: ${mobileLayout.panelWidth} vs viewport ${mobileLayout.innerWidth}`);
    }
    if (mobileLayout.countsRight > mobileLayout.panelRight - 1) {
      fail(`mobile todo counts overflow the panel: counts right ${mobileLayout.countsRight} vs panel right ${mobileLayout.panelRight}`);
    }
    if (mobileLayout.scrollWidth > mobileLayout.innerWidth + 1) {
      fail(`mobile horizontal overflow with todo panel open: scrollWidth ${mobileLayout.scrollWidth} > viewport ${mobileLayout.innerWidth}`);
    }
    if (!mobileLayout.closeInsidePanel || !mobileLayout.closeSharesHeadRow) {
      fail(`mobile todo close chip squeezed or misplaced: inside=${mobileLayout.closeInsidePanel} sameRow=${mobileLayout.closeSharesHeadRow}`);
    }

    // 6. Rich content: the mock returns markdown (table + task list), a
    // mermaid flowchart fence, $...$/$$...$$ math, and a `rust,ignore` fence.
    // The upgraded renderer must produce the structured content and highlight
    // Rust tokens through the END of the code block, with no raw fence markers.
    //
    // The mock routes RICH_TEXT by prompt CONTENT ("render rich content" as
    // the request's last user message), so request parity cannot matter.
    // Under parallel-lane load the rpi's post-abort turn gate can release
    // late, shifting which prompt a given request carries; retry the prompt
    // (bounded) until a rich reply actually lands instead of flaking.
    const richPromptAttempt = async () => {
      const before = await page.evaluate(() => document.querySelectorAll('.msg--assistant .assistant-text').length);
      await page.fill('#prompt-input', 'render rich content');
      await page.press('#prompt-input', 'Enter');
      await waitFor(
        page,
        (b) => {
          const nodes = [...document.querySelectorAll('.msg--assistant .assistant-text')];
          if (nodes.length <= b) return false;
          if (document.querySelector('table.md-table')) return true; // rich reply rendered
          // A different reply settled (parity/retry fallback): signal retry.
          return document.getElementById('activity-strip') === null;
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
    await waitFor(page, () => document.querySelector('.md-task-glyph') !== null, 'task-list glyph never rendered', 30000);
    await waitFor(page, () => document.querySelector('.assistant-text svg') !== null, 'mermaid SVG never rendered', 30000);
    await waitFor(page, () => document.querySelector('.assistant-text .katex') !== null, 'KaTeX math never rendered', 30000);
    await waitFor(
      page,
      () => {
        const rustFence = [...document.querySelectorAll('.md-fence')].find(
          (fence) => fence.querySelector('.md-fence__lang')?.textContent?.trim() === 'rust'
        );
        if (!rustFence) return false;
        const code = rustFence.querySelector('code');
        const lateLiteral = [...rustFence.querySelectorAll('.hljs-literal')].some(
          (node) => node.textContent === 'None'
        );
        const lateKeyword = [...rustFence.querySelectorAll('.hljs-keyword')].some(
          (node) => node.textContent === 'match'
        );
        return !!code && code.textContent.includes('None => Err') && lateLiteral && lateKeyword;
      },
      'Rust fence metadata did not highlight tokens through the end of the block',
      30000
    );
    const richText = await lastAssistantText(page);
    if (richText.includes('```')) {
      fail(`raw fence markers leaked into the transcript: ${richText}`);
    }
    if (richText.includes('|---')) {
      fail(`raw table separator leaked into the transcript: ${richText}`);
    }
    await page.screenshot({ path: `${evidence}/rich.png`, fullPage: true });

    // 7. Workflow panel: two real concurrent workflow runtimes must stay live
    //    together with backend-projected identities, worktrees, and detail.
    await page.click('#workflow-toggle-btn');
    await waitFor(
      page,
      () => document.getElementById('todo-panel') === null && document.getElementById('workflow-panel') !== null,
      'Todo module did not close when workflow panel opened'
    );
    const wsUrl = url.replace(/^http/, 'ws').replace(/\/web\/?$/, '/ws');
    rpc = rpcClient(page, wsUrl);
    await rpc.ready;
    const [alphaCreated, betaCreated] = await Promise.all([
      rpc.call({ type: 'workflow_create', name: 'web-e2e-workflow-alpha', objective: 'alpha browser workflow objective' }),
      rpc.call({ type: 'workflow_create', name: 'web-e2e-workflow-beta', objective: 'beta browser workflow objective' }),
    ]);
    if (!alphaCreated?.workflowId || !betaCreated?.workflowId) fail('concurrent workflow create response omitted workflowId');
    const [alpha, beta] = await Promise.all([
      waitForWorkflowSnapshot(rpc, alphaCreated.workflowId, 'web-e2e-workflow-alpha', 'workflow alpha create'),
      waitForWorkflowSnapshot(rpc, betaCreated.workflowId, 'web-e2e-workflow-beta', 'workflow beta create'),
    ]);
    if (alpha.workflowId === beta.workflowId) fail(`concurrent workflows shared workflowId ${alpha.workflowId}`);
    if (`${alpha.workflowId}@${alpha.generation}` === `${beta.workflowId}@${beta.generation}`) fail('concurrent workflow identity tuples collided');
    if (alpha.worktree === beta.worktree) fail(`concurrent workflows shared worktree ${alpha.worktree}`);
    if (alpha.branch === beta.branch) fail(`concurrent workflows shared branch ${alpha.branch}`);

    await waitFor(
      page,
      (names) => names.every((name) => {
        const row = [...document.querySelectorAll('.workflow-row')].find((candidate) =>
          (candidate.querySelector('.workflow-row__name')?.textContent || '') === name
        );
        return !!row && ['queued', 'planning', 'running', 'paused', 'integrating'].includes(row.dataset.status || '');
      }),
      'both concurrent workflows were not simultaneously live in the panel',
      30000,
      [alpha.name, beta.name]
    );
    const listed = await rpc.call({ type: 'workflow_list' });
    const listedById = new Map((listed.workflows || []).map((workflow) => [workflow.workflowId, workflow]));
    for (const workflow of [alpha, beta]) {
      const row = listedById.get(workflow.workflowId);
      if (!row || row.generation !== workflow.generation || row.worktree !== workflow.worktree || row.branch !== workflow.branch) {
        fail(`workflow list lost exact backend identity for ${workflow.name}: ${JSON.stringify(row)}`);
      }
      if (!LIVE_WORKFLOW_STATUSES.has(row.status)) fail(`workflow list no longer shows ${workflow.name} live: ${row.status}`);
    }

    const [alphaDetail, betaDetail] = await Promise.all([
      waitForWorkflowDetail(rpc, alpha, 'workflow alpha detail'),
      waitForWorkflowDetail(rpc, beta, 'workflow beta detail'),
    ]);
    requireWorkflowDetailIsolation(alpha, beta, alphaDetail, betaDetail);
    const extractJobIds = (detail, label) => {
      if (!Array.isArray(detail.jobs)) fail(`${label}: jobs array missing`);
      const ids = detail.jobs.map((job) => job.jobId);
      for (const id of ids) {
        if (typeof id !== 'string' || id.trim() === '') fail(`${label}: extracted job ID is not a non-empty string`);
      }
      if (ids.length === 0) fail(`${label}: expected at least one job ID`);
      if (new Set(ids).size !== ids.length) fail(`${label}: duplicate job IDs ${JSON.stringify(ids)}`);
      return new Set(ids);
    };
    const alphaJobIds = extractJobIds(alphaDetail, 'workflow alpha');
    const betaJobIds = extractJobIds(betaDetail, 'workflow beta');
    for (const id of alphaJobIds) {
      if (betaJobIds.has(id)) fail(`workflow job IDs crossed A/B ownership: ${id}`);
    }

    await page.evaluate((workflowId) => {
      const row = document.querySelector(`[data-workflow-id="${workflowId}"]`);
      if (!row) throw new Error(`workflow row ${workflowId} missing`);
      row.click();
    }, alpha.workflowId);
    await waitFor(
      page,
      (identity) => {
        const meta = document.querySelector('.workflow-detail__meta')?.textContent || '';
        return meta.includes(identity.workflowId) && meta.includes(`gen ${identity.generation}`)
          && document.querySelector('[aria-label="Supervisor"] .workflow-actor') !== null
          && document.querySelector('#workflow-detail-activity .workflow-feed__item') !== null
          && document.querySelector('#workflow-detail-tasks .workflow-task') !== null
          && document.querySelector('#workflow-detail-workers .workflow-worker') !== null;
      },
      'alpha supervisor/planning/tasks/workers were not visible together',
      30000,
      alpha
    );

    const delivered = await rpc.call({
      type: 'workflow_hub_send', workflowId: alpha.workflowId, generation: alpha.generation,
      recipient: alphaDetail.supervisor.name, body: 'alpha ownership receipt',
    });
    const alphaAfterIrc = await waitForWorkflowIrc(
      rpc, alpha, 'alpha ownership receipt', 'workflow alpha detail after IRC'
    );
    const betaAfterAlphaIrc = await waitForWorkflowIrc(
      rpc, beta, null, 'workflow beta detail after alpha IRC'
    );
    if (!(alphaAfterIrc.recentIrc || []).some((message) => message.text.includes('alpha ownership receipt'))) {
      fail('workflow alpha detail omitted its delivered IRC');
    }
    if ((betaAfterAlphaIrc.recentIrc || []).some((message) => message.text.includes('alpha ownership receipt'))) {
      fail('workflow alpha IRC crossed into beta detail');
    }
    requireTypedReceipt(delivered, alpha, alphaDetail.supervisor.name, 'delivered', 'workflow alpha delivery');
    await requireRpcFailure(
      rpc.call({
        type: 'workflow_hub_send', workflowId: alpha.workflowId, generation: alpha.generation + 1,
        recipient: alphaDetail.supervisor.name, body: 'stale generation must fail',
      }),
      'workflow stale-generation send'
    );
    const failedReceipt = await rpc.call({
      type: 'workflow_hub_send', workflowId: alpha.workflowId, generation: alpha.generation,
      recipient: 'missing-workflow-recipient', body: 'must fail visibly',
    });
    requireTypedReceipt(failedReceipt, alpha, 'missing-workflow-recipient', 'failed', 'workflow alpha failed delivery');
    await waitFor(
      page,
      (body) => [...document.querySelectorAll('.workflow-irc__item')].some((item) => (item.textContent || '').includes(body)),
      'workflow IRC history never rendered the sent alpha message',
      30000,
      'alpha ownership receipt'
    );
    await page.evaluate(() => {
      const select = document.querySelector('.workflow-irc__recipient');
      if (!select) throw new Error('workflow recipient select missing');
      const option = document.createElement('option');
      option.value = 'missing-workflow-recipient';
      option.textContent = 'missing-workflow-recipient';
      select.append(option);
      const setter = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set;
      setter.call(select, option.value);
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
    await page.fill('.workflow-irc__body', 'visible failed workflow receipt');
    await waitFor(page, () => document.querySelector('.workflow-irc__send')?.disabled === false, 'failed-receipt send button never enabled');
    await page.click('.workflow-irc__send');
    await waitFor(
      page,
      () => {
        const receipt = document.querySelector('.workflow-irc__receipt--failed');
        return !!receipt && (receipt.querySelector('.workflow-irc__receipt-error')?.textContent || '').trim() !== '';
      },
      'workflow panel never rendered the typed failed receipt and visible error',
      30000
    );
    await waitFor(
      page,
      (state) => {
        const liveRows = state.names.every((name) => {
          const row = [...document.querySelectorAll('.workflow-row')].find((candidate) =>
            (candidate.querySelector('.workflow-row__name')?.textContent || '') === name
          );
          return !!row && ['queued', 'planning', 'running', 'paused', 'integrating'].includes(row.dataset.status || '');
        });
        const detailMeta = document.querySelector('.workflow-detail__meta')?.textContent || '';
        return liveRows && detailMeta.includes(state.workflowId)
          && document.querySelector('#workflow-detail-workers .workflow-worker') !== null
          && document.querySelector('.workflow-irc__item') !== null
          && document.querySelector('.workflow-irc__receipt--failed .workflow-irc__receipt-error') !== null;
      },
      'workflow screenshot state lost dual-live detail/worker/IRC evidence',
      30000,
      { names: [alpha.name, beta.name], workflowId: alpha.workflowId }
    );
    fs.mkdirSync(evidence, { recursive: true });
    const workflowEvidenceName = 'workflow-concurrent-live-detail-workers-irc.png';
    await page.screenshot({ path: `${evidence}/${workflowEvidenceName}`, fullPage: true });
    console.log(`web: workflow concurrent evidence=${workflowEvidenceName}`);

    await rpc.call({ type: 'workflow_cancel', workflowId: alpha.workflowId });
    const betaAfterCancel = await waitForWorkflowSnapshot(
      rpc, beta.workflowId, beta.name, 'workflow beta after alpha cancel'
    );
    requireTypedReceipt(
      await rpc.call({
        type: 'workflow_hub_send', workflowId: beta.workflowId, generation: beta.generation,
        recipient: betaDetail.supervisor.name, body: 'beta remains addressable after alpha cancel',
      }),
      beta, betaDetail.supervisor.name, 'delivered', 'workflow beta after alpha cancel'
    );
    await rpc.call({ type: 'workflow_remove', workflowId: alpha.workflowId });
    const remaining = await rpc.call({ type: 'workflow_list' });
    const remainingIds = new Set((remaining.workflows || []).map((workflow) => workflow.workflowId));
    if (remainingIds.has(alpha.workflowId) || !remainingIds.has(beta.workflowId)) {
      fail(`removing alpha did not leave beta live: ${JSON.stringify(remaining.workflows)}`);
    }
    const betaFinal = await waitForWorkflowSnapshot(
      rpc, beta.workflowId, beta.name, 'workflow beta after alpha remove'
    );
    requireTypedReceipt(
      await rpc.call({
        type: 'workflow_hub_send', workflowId: beta.workflowId, generation: beta.generation,
        recipient: betaDetail.supervisor.name, body: 'beta remains addressable after alpha remove',
      }),
      beta, betaDetail.supervisor.name, 'delivered', 'workflow beta after alpha remove'
    );

    // 8. Settings panel: browse by category, secret refusal (live.sttApiKey
    //    renders redacted with no editable control), open a global draft,
    //    change `theme` (a LIVE string setting), apply, and assert the panel
    //    reflects the persisted value.
    await page.click('#settings-toggle-btn');
    await waitFor(page, () => document.getElementById('settings-panel') !== null, 'settings panel did not open');
    await page.click('.settings-category:has-text("Terminal")');
    await waitFor(
      page,
      () => document.querySelector('[data-setting-key="theme"]') !== null,
      'theme setting row never rendered'
    );
    // Secret refusal: the live.sttApiKey row must show the redacted marker
    // and expose no input/select/textarea control.
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
    if (secretControlCount !== 0) {
      fail(`secret key must not be editable, found ${secretControlCount} controls`);
    }
    // Open a global draft and change theme.
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
    await page.screenshot({ path: `${evidence}/settings.png`, fullPage: true });

    // 9. Session panel: current session info renders, rename round-trips
    //    into the panel + sidebar row title (the header no longer duplicates
    //    the session name), the saved-sessions list is populated, and
    // Persistent left sidebar: lists saved sessions with the current one
    // marked active, and New switches to a fresh session.
    await waitFor(page, () => document.getElementById('session-sidebar') !== null, 'session sidebar did not render');
    await waitFor(
      page,
      () => document.querySelectorAll('.session-sidebar__row').length >= 1,
      'session sidebar never listed saved sessions'
    );
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
    // The header must never render a duplicate `session: <name>` label; the
    // rename proves itself via the panel value above and the sidebar row
    // title below.
    await waitFor(
      page,
      () => {
        const header = document.getElementById('model-select')?.closest('header');
        return document.getElementById('session-name') === null
          && header !== null
          && !header.textContent.includes('session:');
      },
      'header still renders a duplicate session label after the rename'
    );
    await waitFor(
      page,
      () =>
        document.querySelector('.session-sidebar__row--active .session-sidebar__switch .session-sidebar__name')
          ?.textContent?.trim() === 'web e2e session',
      'session rename never reflected in the active sidebar row title',
      25000
    );
    await waitFor(
      page,
      () => document.querySelectorAll('.session-sidebar__row').length >= 1,
      'no saved sessions listed in the session sidebar',
    );
    await assertSidebarNewActionAbsent(page, 'session lifecycle: obsolete sidebar New action rendered');
    const msgsBeforeSwitch = await page.$$eval('#transcript .msg', (els) => els.length);
    if (msgsBeforeSwitch === 0) {
      fail('session lifecycle: no transcript messages before the switch — the clear would be unobservable');
    }
    await createFreshSessionViaManage(page, waitFor, 'session lifecycle', {
      closePanel: false,
      timeoutMs: 25000,
    });
    // F4: a NEW session id must show the EMPTY new-session view — never the
    // previous session's messages under the new header.
    await waitFor(
      page,
      () => {
        const active = document.querySelector('#transcript .transcript-surface:not([hidden])');
        if (!active) return false;
        return active.querySelectorAll('.msg').length === 0 && active.querySelector('.empty-hint') !== null;
      },
      'session switch never cleared the old transcript (empty new-session view missing)'
    );
    const staleTranscript = await page.evaluate(
      () => document.querySelector('#transcript .transcript-surface:not([hidden])')?.textContent || ''
    );
    if (staleTranscript.includes('hello from the web e2e')) {
      fail("session switch retained the old session's messages in the transcript");
    }
    await page.screenshot({ path: `${evidence}/session.png`, fullPage: true });

    // 10. Subagents panel: spawn a faux subagent via the panel (task_spawn),
    //     assert the job card appears with a live status + activity line,
    //     message it (hub_send), view its output (job_output), then cancel it
    //     (job_cancel) and assert the settled status in the list.
    // Close the session drawer first: panels are fixed overlays and would
    // intercept the header toggle click (same fix as the workflow step).
    await page.click('#session-close-btn');
    await waitFor(page, () => document.getElementById('session-panel') === null, 'session panel did not close');
    await page.click('#subagents-toggle-btn');
    await waitFor(page, () => document.getElementById('subagents-panel') !== null, 'subagents panel did not open');
    // The orchestration fixture must be enabled; otherwise the panel hides the
    // spawn form and this step fails fast with a clear message. The panel
    // fetches job_list asynchronously on mount, so WAIT for the spawn form
    // rather than checking once (a race would fail spuriously).
    await waitFor(
      page,
      () => document.getElementById('subagents-panel')?.querySelector('#subagents-spawn-btn') !== null,
      'subagents panel did not show the spawn form (orchestration disabled in fixture?)',
      15000
    );
    await page.selectOption('#subagents-agent-select', 'writer');
    await page.fill('#subagents-task-input', 'web-e2e-subagent: audit the release notes and report findings');
    await page.click('#subagents-spawn-btn');
    // The spawned child's marker prompt streams slowly in the mock, so the job
    // stays queued/running long enough to cancel deterministically.
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        return cards.some((card) => (card.textContent || '').includes('audit the release notes'));
      },
      'spawned subagent job never appeared in the panel',
      30000
    );
    const subagentStatus = () =>
      page.evaluate(() => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
        return card ? card.getAttribute('data-status') || '' : '';
      });
    const liveStatus = await subagentStatus();
    if (!['queued', 'running'].includes(liveStatus)) {
      fail(`spawned subagent must be live (queued/running) before cancel, got "${liveStatus}"`);
    }
    // Activity one-liner: the progress line must render (stage or activity ·
    // elapsed) for the live job.
    const progressLine = await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
      const line = card ? card.querySelector('[data-progress-line]') : null;
      return line ? line.textContent || '' : '';
    });
    if (!progressLine.trim()) {
      fail('spawned subagent never rendered a live activity/elapsed line');
    }
    await page.screenshot({ path: `${evidence}/subagents-spawned.png`, fullPage: true });
    // The job is still running because the mock streams slowly. Open the
    // modal via the Details trigger and assert an
    // accessible dialog (role=dialog, aria-modal, aria-label, correct
    // data-job-id, initial focus on Close) showing the task description, live
    // status/elapsed, latest activity, the live badge, and NON-EMPTY recent
    // history while the job runs — the acceptance that fails when a
    // long-running job exposes only status/output. Exercise Refresh
    // (refetch keeps the transcript live with no error), close via Escape,
    // reopen by clicking the card, and close via the Close button.
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
      if (!card) throw new Error('subagent card missing before modal open');
      card.querySelector('[data-details-trigger]').click();
    });
    await waitFor(page, () => document.querySelector('[data-details-dialog]') !== null, 'detail modal never opened (Details trigger)');
    const detailsA11y = await page.evaluate(() => {
      const dlg = document.querySelector('[data-details-dialog]');
      const close = document.querySelector('[data-details-close]');
      const card = [...document.querySelectorAll('.subagent-job')]
        .find((c) => (c.textContent || '').includes('audit the release notes'));
      return {
        role: dlg?.getAttribute('role') || '',
        ariaModal: dlg?.getAttribute('aria-modal') || '',
        ariaLabel: dlg?.getAttribute('aria-label') || '',
        jobId: dlg?.getAttribute('data-job-id') || '',
        cardJobId: card?.getAttribute('data-job-id') || '',
        focusOnClose: !!close && document.activeElement === close,
      };
    });
    if (detailsA11y.role !== 'dialog') fail(`detail modal must be role=dialog, got "${detailsA11y.role}"`);
    if (detailsA11y.ariaModal !== 'true') fail('detail modal must set aria-modal=true');
    if (detailsA11y.ariaLabel !== 'Subagent job details') fail(`detail modal aria-label mismatch: "${detailsA11y.ariaLabel}"`);
    if (!detailsA11y.jobId || detailsA11y.jobId !== detailsA11y.cardJobId) {
      fail(`detail modal bound to wrong job: dialog=${detailsA11y.jobId} card=${detailsA11y.cardJobId}`);
    }
    if (!detailsA11y.focusOnClose) fail('detail modal initial focus did not land on the Close button');
    const detailsContent = await page.evaluate(() => {
      const txt = (sel) => (document.querySelector(sel)?.textContent || '').trim();
      return {
        description: txt('[data-details-description]'),
        status: txt('[data-details-status] .subagent-job__status'),
        elapsed: txt('[data-details-elapsed]'),
        activity: txt('[data-details-activity]'),
        live: document.querySelector('[data-details-live]') !== null,
      };
    });
    if (!detailsContent.description.includes('audit the release notes')) {
      fail(`detail modal description missing the task text: "${detailsContent.description}"`);
    }
    if (!['queued', 'running'].includes(detailsContent.status)) {
      fail(`detail modal status must be live (queued/running), got "${detailsContent.status}"`);
    }
    if (!detailsContent.elapsed) fail('detail modal elapsed never rendered while running');
    if (!detailsContent.activity) fail('detail modal latest activity never rendered');
    if (!detailsContent.live) fail('detail modal live badge missing while the job is running');
    // Recent history MUST be non-empty while the job is still running.
    await waitFor(
      page,
      () => {
        const pre = document.querySelector('[data-details-history]');
        const text = pre ? pre.textContent.trim() : '';
        return text !== '' && !text.startsWith('(no transcript yet') && !text.startsWith('(transcript unavailable)');
      },
      'detail modal recent history never became non-empty while the job was running',
      15000
    );
    const detailsErrorShown = await page.evaluate(
      () => document.querySelector('[data-details-error]') !== null,
    );
    if (detailsErrorShown) {
      fail('detail modal reported an agent_history error while the job was running');
    }
    await page.screenshot({ path: `${evidence}/subagents-modal-running.png`, fullPage: true });
    // Refresh re-fetches the child transcript: dialog stays open, history
    // remains non-empty, no error appears.
    await page.evaluate(() => document.querySelector('[data-details-refresh]').click());
    await waitFor(
      page,
      () => {
        const pre = document.querySelector('[data-details-history]');
        return !!pre && pre.textContent.trim() !== '' && document.querySelector('[data-details-error]') === null;
      },
      'detail modal Refresh broke the recent history view',
      10000
    );
    // Close via Escape dismisses the modal.
    await page.keyboard.press('Escape');
    await waitFor(page, () => document.querySelector('[data-details-dialog]') === null, 'detail modal did not close on Escape');
    // Reopen by clicking the running job card (the section onClick path) and
    // close via the Close button — proving both dismissal paths.
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
      card.querySelector('.subagent-job__description').click();
    });
    await waitFor(page, () => document.querySelector('[data-details-dialog]') !== null, 'detail modal never opened on card click');
    await page.evaluate(() => document.querySelector('[data-details-close]').click());
    await waitFor(page, () => document.querySelector('[data-details-dialog]') === null, 'detail modal did not close on Close button');

    // Message the subagent via hub_send (per-job message input + Send).
    // React controlled inputs ignore `input.value=`; use the native setter so
    // onChange fires and the Send button enables.
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

    // View the job's delivered output (job_output -> output pane).
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

    // Cancel moves the job out of the default Running filter. Switch to
    // Completed and assert the same card is retained with cancelled status.
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
      const buttons = [...card.querySelectorAll('.subagent-job__action')];
      const cancel = buttons.find((b) => (b.textContent || '').includes('Cancel'));
      cancel.click();
    });
    await waitFor(
      page,
      () => ![...document.querySelectorAll('.subagent-job')]
        .some((card) => (card.textContent || '').includes('audit the release notes')),
      'cancelled subagent remained under Running',
      30000
    );
    await page.click('.subagents-panel__filter-btn[data-filter="completed"]');
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
        return !!card && card.getAttribute('data-status') === 'cancelled';
      },
      'subagent job never appeared as cancelled under Completed',
      30000
    );
    await page.screenshot({ path: `${evidence}/subagents.png`, fullPage: true });

    // Structured Task tool card (main transcript): real `task` tool call with
    // shared context + one child. Assert Goal/Constraints/Contract, child
    // name/agent/status, and that raw args JSON is not the default view.
    const taskCardBefore = await page.evaluate(() => document.querySelectorAll('.tool-card--task').length);
    const taskTurnPromise = turnFrames.nextStart();
    await page.fill('#prompt-input', 'tool-card coverage task spawn');
    await page.press('#prompt-input', 'Enter');
    const taskTurn = await taskTurnPromise;
    await waitFor(
      page,
      (b) => document.querySelectorAll('.tool-card--task').length > b,
      'task tool-card never rendered in the transcript',
      45000,
      taskCardBefore,
    );
    await waitFor(
      page,
      (expected) => {
        const strip = document.getElementById('activity-strip');
        if (strip?.dataset.state !== 'child' || !(strip.textContent || '').includes('Waiting for')) return false;
        const cards = [...document.querySelectorAll('.tool-card--task')];
        const child = cards.flatMap((card) => [...card.querySelectorAll('.tool-card__child')])
          .find((candidate) => (candidate.querySelector('.tool-card__child-target')?.textContent || '').includes('audit the release notes'));
        return strip.dataset.sessionId === expected.sessionId
          && strip.dataset.turnId === expected.turnId
          && Boolean(strip.dataset.jobId)
          && Boolean(strip.dataset.agentId)
          && child?.getAttribute('data-job-id') === strip.dataset.jobId
          && child?.getAttribute('data-agent-id') === strip.dataset.agentId;
      },
      'activity strip never projected the authoritative waiting child identity',
      30000,
      taskTurn,
    );
    await page.screenshot({ path: `${evidence}/activity-strip-child-desktop.png`, fullPage: true });
    const taskCard = await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.tool-card--task')];
      const last = cards.find((card) => [...card.querySelectorAll('.tool-card__child-target')]
        .some((target) => (target.textContent || '').includes('audit the release notes')));
      if (!last) return null;
      const sections = [...last.querySelectorAll('.tool-card__section')].map((s) => ({
        label: s.querySelector('.tool-card__section-label')?.textContent || '',
        body: s.querySelector('.tool-card__section-body')?.textContent || '',
      }));
      const child = [...last.querySelectorAll('.tool-card__child')]
        .find((candidate) => (candidate.querySelector('.tool-card__child-target')?.textContent || '').includes('audit the release notes'));
      const argsPre = last.querySelector('.tool-card__args');
      const rawOpen = last.querySelector('details.tool-card__raw[open]');
      return {
        sections,
        childName: child?.querySelector('.tool-card__child-name')?.textContent || '',
        childAgent: child?.querySelector('.tool-card__child-agent')?.textContent || '',
        childStatus: child?.getAttribute('data-status') || child?.querySelector('.tool-card__child-status')?.textContent || '',
        childTarget: child?.querySelector('.tool-card__child-target')?.textContent || '',
        rawVisible: !!argsPre && !last.querySelector('details.tool-card__raw'),
        rawExpanded: !!rawOpen,
        text: last.textContent || '',
      };
    });
    if (!taskCard) fail('task tool-card evaluate returned null');
    const goal = taskCard.sections.find((s) => s.label === 'Goal');
    const constraints = taskCard.sections.find((s) => s.label === 'Constraints');
    const contract = taskCard.sections.find((s) => s.label === 'Contract');
    if (!goal || !goal.body.includes('Ship the Task card')) fail(`task Goal missing: ${JSON.stringify(taskCard.sections)}`);
    if (!constraints || !constraints.body.includes('Be precise')) fail(`task Constraints missing: ${JSON.stringify(taskCard.sections)}`);
    if (!contract || !contract.body.includes('Keep stable ids')) fail(`task Contract missing: ${JSON.stringify(taskCard.sections)}`);
    if (taskCard.childName !== 'writer') fail(`task child name wrong: ${taskCard.childName}`);
    if (taskCard.childAgent !== 'writer') fail(`task child agent wrong: ${taskCard.childAgent}`);
    if (!taskCard.childTarget.includes('audit the release notes')) fail(`task child target wrong: ${taskCard.childTarget}`);
    if (!taskCard.childStatus) fail('task child status missing');
    if (taskCard.rawVisible || taskCard.rawExpanded) fail('task card dumped raw JSON as the default view');
    if (taskCard.text.includes('"tasks"') && !taskCard.text.includes('Goal')) fail('task card looks like raw JSON dump');
    if (taskCard.text.includes('Goal null')) fail(`task card rendered "Goal null": ${taskCard.text.slice(0, 200)}`);
    const taskTerminal = await turnFrames.waitForTerminal(taskTurn.sessionId, taskTurn.turnId, 30000).catch((error) => fail(error.message));
    if (taskTerminal !== 'succeeded') fail(`Task turn ended with ${taskTerminal || 'unknown'} status`);
    await waitFor(
      page,
      () => document.getElementById('activity-strip') === null,
      'activity strip remained mounted after matching Task turn terminal',
      30000,
    );
    if (pageErrors.length > 0) fail(`page errors: ${JSON.stringify(pageErrors)}`);
    if (consoleErrors.length > 0) fail(`console errors: ${JSON.stringify(consoleErrors)}`);
    await page.screenshot({ path: `${evidence}/task-card.png`, fullPage: true });

    console.log('web: playwright PASSED (page load, WS connect, prompt round-trip, abort, recovery, todo panel, rich content, strict concurrent workflows + receipts/isolation/cancel-remove survival, settings browse/edit/apply + secret refusal, session info/rename/switch/new, subagents spawn/live/detail-modal/message/output/cancel, task tool card)');
  } finally {
    if (rpc) rpc.close();
    await browser.close();
  }
}

main().catch((err) => {
  console.error(`web: playwright crashed: ${err.stack || err}`);
  process.exit(2);
});
