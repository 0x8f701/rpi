// Web Todo session E2E (playwright half of the unified Todo Web lane).
//
// Environment:
//   RPI_URL         http://127.0.0.1:<port>/web
//   RPI_TOKEN       token file content (served via rpi-auth.<token> subprotocol)
//   RPI_WORK        shared dir for the kill/restart handshake with the lane:
//                     test writes kill-server.marker, lane respawns the
//                     listener on the SAME port and writes server-up.marker
//   RPI_CHROME      executable path of the system Chrome (optional)
//   RPI_EVIDENCE    evidence dir for screenshots
//
// Drives the REAL unified Todo module against the real listener. No fake
// backend state. Ordinary Tasks and durable Child sessions/details are
// simultaneous sections of one Todo panel. Typed RPCs stay distinct:
// todo_child_create / list / select / send / add_task / hub_send / pause /
// resume / stop. Parent cleanup and a browser WebSocket drop do not stop the
// child. A real-listener request with a wrong parentSessionId+childSessionId
// pairing fails closed without mutating the selected child. Ordinary Todo DAG,
// Activity, and Workflow remain distinct domains.


import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';
import { createFreshSessionViaManage } from './lib/session-lifecycle.mjs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const work = process.env.RPI_WORK || '.';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

const KILL_MARKER = path.join(work, 'kill-server.marker');
const UP_MARKER = path.join(work, 'server-up.marker');

const PARENT_SENTINEL = 'todo-child parent fork sentinel';
const PARENT_POST_FORK = 'todo-child parent post-fork only';
const PARENT_RESTART_ANCHOR = 'todo-child parent restart anchor';

const CHILD_A_LABEL = 'e2e-child-a';
const CHILD_B_LABEL = 'e2e-child-b';
const CHILD_A_TASK = 'e2e child A typed task';
const CHILD_A_STALE_TASK = 'e2e child A stale task must not land';
const CHILD_A_SEND = 'e2e child A typed send';
const CHILD_A_HUB = 'e2e child A hub body';
const CHILD_B_SEND = 'e2e child B typed send';
const ORDINARY_TODO = 'ordinary-todo-dag-task';
const WRONG_PARENT_ID = '00000000-0000-4000-8000-00000000aa01';
const CHILD_A_AFTER_SWITCH = 'e2e child A after displayed switch';
const CHILD_A_AFTER_RECONNECT = 'e2e child A after websocket reconnect';
const CHILD_COLLECTION_LABELS = ['Tasks', 'Receipts', 'Agents', 'Jobs', 'Activity', 'IRC', 'Transcript'];

const sentFrames = [];
const receivedFrames = [];
const pageErrors = [];
const consoleErrors = [];

function fail(message) {
  console.error(`web-todo-child: FAIL: ${message}`);
  process.exitCode = 2;
  throw new Error(message);
}

function documentedWsUrl() {
  if (!url) return '';
  try {
    const page = new URL(url);
    const scheme = page.protocol === 'https:' ? 'wss:' : 'ws:';
    return `${scheme}//${page.host}/ws`;
  } catch {
    return '';
  }
}
const DOCUMENTED_WS = documentedWsUrl();

function isDocumentedListenerOutageConsole(text) {
  return typeof text === 'string'
    && DOCUMENTED_WS !== ''
    && text.includes('WebSocket connection to ')
    && text.includes(DOCUMENTED_WS)
    && text.includes('net::ERR_CONNECTION_REFUSED');
}

{
  const sample = DOCUMENTED_WS || 'ws://127.0.0.1:1/ws';
  const refused = `WebSocket connection to '${sample}' failed: Error in connection establishment: net::ERR_CONNECTION_REFUSED`;
  if (DOCUMENTED_WS && !isDocumentedListenerOutageConsole(refused)) {
    fail('listener-outage console classifier missed a documented Chromium refused /ws error');
  }
  if (isDocumentedListenerOutageConsole('TypeError: boom')) {
    fail('listener-outage console classifier swallowed an unrelated error');
  }
  if (isDocumentedListenerOutageConsole("WebSocket connection to 'ws://evil.example/ws' failed: Error in connection establishment: net::ERR_CONNECTION_REFUSED")) {
    fail('listener-outage console classifier swallowed an other-host /ws error');
  }
}

async function waitFor(page, fn, label, timeoutMs = 30000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

function waitForFile(file, label, timeoutMs = 60000) {
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

async function waitForIdle(page, label, timeoutMs = 30000) {
  await waitFor(
    page,
    () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message',
    label,
    timeoutMs,
  );
}

async function activeSessionId(page) {
  return page.evaluate(() =>
    document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
  );
}

async function switchSession(page, sessionId, label) {
  const active = await activeSessionId(page);
  if (active === sessionId) return;
  await waitFor(
    page,
    (sid) => {
      const row = [...document.querySelectorAll('.session-sidebar__switch')]
        .find((candidate) => candidate.dataset.sessionId === sid);
      return row !== undefined && !row.disabled;
    },
    `${label}: session row never became clickable`,
    30000,
    sessionId,
  );
  await page.evaluate((sid) => {
    [...document.querySelectorAll('.session-sidebar__switch')]
      .find((candidate) => candidate.dataset.sessionId === sid)?.click();
  }, sessionId);
  await waitFor(
    page,
    (sid) => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId === sid,
    `${label}: session never became active`,
    30000,
    sessionId,
  );
}


async function openTodoPanel(page, label) {
  const already = await page.evaluate(() => document.querySelectorAll('#todo-panel').length === 1);
  if (!already) {
    const clicked = await page.evaluate(() => {
      const button = document.getElementById('todo-toggle-btn');
      if (!button) return false;
      button.click();
      return true;
    });
    if (!clicked) fail(`${label}: Todo module entry missing`);
  }
  await waitFor(
    page,
    () => document.querySelectorAll('#todo-panel').length === 1
      && document.getElementById('todo-child-panel') === null
      && document.querySelector('.todo-panel__tasks') !== null
      && document.querySelector('.todo-child-section') !== null
      && document.getElementById('todo-add-content') !== null
      && document.getElementById('todo-child-list') !== null
      && document.getElementById('todo-child-detail') !== null
      && document.getElementById('todo-child-create-btn') !== null,
    `${label}: unified Todo panel did not open with both sections`,
  );
}

async function assertUnifiedVisible(page, label, { ordinaryTask = '', childId = '', childLabel = '' } = {}) {
  await waitFor(
    page,
    (state) => {
      if (document.querySelectorAll('#todo-panel').length !== 1 || document.getElementById('todo-child-panel')) return false;
      if (!document.querySelector('.todo-panel__tasks') || !document.querySelector('.todo-child-section')) return false;
      if (state.ordinaryTask && ![...document.querySelectorAll('.todo-task')]
        .some((row) => (row.textContent || '').includes(state.ordinaryTask))) return false;
      if (!state.childId) return true;
      const selected = document.querySelector('.todo-child-row.is-selected');
      const detail = document.querySelector('#todo-child-detail .todo-child-detail');
      if (selected?.dataset.childSessionId !== state.childId || !detail) return false;
      if (state.childLabel && !(selected.textContent || '').includes(state.childLabel)) return false;
      return state.collectionLabels.every((name) =>
        document.querySelector(`#todo-child-detail .todo-child-detail__section[aria-label="${name}"]`) !== null);
    },
    `${label}: ordinary task and complete selected-child projection were not simultaneous`,
    30000,
    { ordinaryTask, childId, childLabel, collectionLabels: CHILD_COLLECTION_LABELS },
  );
}

async function promptAndSettle(page, text, label) {
  await waitForIdle(page, `${label}: pre-send idle`);
  const before = await page.evaluate(() => document.querySelectorAll('.msg--user').length);
  await page.fill('#prompt-input', text);
  await page.press('#prompt-input', 'Enter');
  await waitFor(
    page,
    (state) => {
      const users = [...document.querySelectorAll('.msg--user')];
      if (users.length <= state.before) return false;
      const last = users[users.length - 1]?.textContent || '';
      const actionLabel = document.getElementById('send-btn')?.getAttribute('aria-label');
      return last.includes(state.text) && actionLabel === 'Send message';
    },
    `${label}: parent prompt did not settle`,
    30000,
    { before, text },
  );
}

function parseFrame(payload) {
  try {
    return JSON.parse(payload);
  } catch {
    return null;
  }
}

function framesOfType(type, since = 0) {
  return sentFrames.slice(since).map((raw) => {
    const frame = parseFrame(raw);
    return frame ? { ...frame, __raw: raw } : null;
  }).filter((frame) => frame && frame.type === type);
}

async function waitForDispatched(page, type, label, since, expected = {}, timeoutMs = 30000) {
  await waitFor(
    page,
    (state) => {
      const frames = window.__rpiTodoChildFrames.sent.slice(state.since)
        .map((raw) => { try { return JSON.parse(raw); } catch { return null; } })
        .filter((frame) => frame?.type === state.type
          && Object.entries(state.expected).every(([key, value]) => frame[key] === value));
      return frames.some((request) => window.__rpiTodoChildFrames.received
        .map((raw) => { try { return JSON.parse(raw); } catch { return null; } })
        .some((response) => response?.type === 'response' && response.id === request.id));
    },
    `${label}: current ${type} response did not arrive`,
    timeoutMs,
    { type, since, expected },
  );
  return assertDispatched(type, label, since, expected);
}


function assertDispatched(type, label, since = 0, expected = {}) {
  const frames = framesOfType(type, since).filter((frame) =>
    Object.entries(expected).every(([key, value]) => frame[key] === value));
  if (frames.length === 0) fail(`${label}: never dispatched current ${type} with ${JSON.stringify(expected)}`);
  const frame = frames[frames.length - 1];
  if (typeof frame.id !== 'string' || !frame.id) fail(`${label}: ${type} request id missing`);
  const response = receivedFrames
    .map(parseFrame)
    .find((candidate) => candidate && candidate.type === 'response' && candidate.id === frame.id);
  if (!response) fail(`${label}: ${type} response missing for request ${frame.id}`);
  if (response.command !== type || response.sessionId !== expected.sessionId) {
    fail(`${label}: ${type} response authority mismatch: ${JSON.stringify(response)}`);
  }
  if (response.success !== true) fail(`${label}: ${type} failed: ${response.error || 'unknown error'}`);
  return { frame, response };
}

function assertTypedDistinct(label) {
  const send = framesOfType('todo_child_send');
  const addTask = framesOfType('todo_child_add_task');
  const hub = framesOfType('todo_child_hub_send');
  if (send.some((frame) => frame.type === 'todo_child_hub_send' || frame.type === 'todo_child_add_task')) {
    fail(`${label}: todo_child_send mixed with another typed command`);
  }
  if (addTask.some((frame) => typeof frame.message === 'string' && frame.message.length > 0 && !frame.content)) {
    fail(`${label}: todo_child_add_task used the send payload shape`);
  }
  if (hub.some((frame) => frame.message && !frame.body)) {
    fail(`${label}: todo_child_hub_send used the send payload shape`);
  }
  const forbidden = ['hub_send', 'todo_op', 'task_spawn', 'workflow_hub_send'];
  for (const type of ['todo_child_send', 'todo_child_add_task', 'todo_child_hub_send']) {
    for (const frame of framesOfType(type)) {
      if (forbidden.includes(frame.type)) fail(`${label}: ${type} collided with ${frame.type}`);
      if (type === 'todo_child_send' && (frame.content || frame.to)) {
        fail(`${label}: todo_child_send carried add-task/hub fields`);
      }
      if (type === 'todo_child_add_task' && (frame.message || frame.to || frame.body)) {
        fail(`${label}: todo_child_add_task carried send/hub fields`);
      }
      if (type === 'todo_child_hub_send' && (frame.message || frame.content)) {
        fail(`${label}: todo_child_hub_send carried send/add-task fields`);
      }
    }
  }
  if (send.length === 0 || addTask.length === 0 || hub.length === 0) {
    fail(`${label}: expected all three typed commands (send=${send.length} add_task=${addTask.length} hub=${hub.length})`);
  }
}

async function selectedChildId(page) {
  return page.evaluate(() =>
    document.querySelector('.todo-child-row.is-selected')?.dataset.childSessionId || '',
  );
}

async function detailText(page) {
  return page.evaluate(() => document.getElementById('todo-child-detail')?.textContent || '');
}

async function detailAuthority(page) {
  return page.evaluate(() => {
    const pane = document.getElementById('todo-child-detail');
    const detail = pane?.querySelector('.todo-child-detail') || null;
    const termValue = (label) => {
      const term = [...(detail?.querySelectorAll('dt') || [])]
        .find((node) => (node.textContent || '').trim() === label);
      return term?.nextElementSibling || null;
    };
    const revision = Number((termValue('Revision')?.textContent || '').trim());
    return {
      selectedChildId: document.querySelector('.todo-child-row.is-selected')?.dataset.childSessionId || '',
      detailChildId: (detail?.querySelector('h2 code')?.textContent || '').trim(),
      parentSessionId: (termValue('Parent provenance')?.querySelector('code')?.textContent || '').trim(),
      revision: Number.isSafeInteger(revision) ? revision : null,
      text: pane?.textContent || '',
    };
  });
}


async function ordinaryTasksText(page) {
  return page.evaluate(() => document.querySelector('.todo-panel__tasks')?.textContent || '');
}

async function childSectionText(page) {
  return page.evaluate(() => document.querySelector('.todo-child-section')?.textContent || '');
}

async function installTodoSocketHarness(page) {
  await page.addInitScript(() => {
    const NativeWebSocket = window.WebSocket;
    let current = null;
    let blocked = false;
    const frameLog = { sent: [], received: [] };
    const control = {
      rewriteAddTaskRevision: null,
      disconnect() { blocked = true; current?.close(4000, 'test outage'); },
      reconnect() { blocked = false; },
      sendRaw(data) {
        if (!current || current.readyState !== NativeWebSocket.OPEN) throw new Error('todo-child test has no open websocket');
        current.send(data);
      },
    };
    class TestWebSocket extends NativeWebSocket {
      constructor(...args) {
        if (blocked) throw new Error('todo-child test outage');
        super(...args);
        current = this;
        this.addEventListener('message', (event) => { if (typeof event.data === 'string') frameLog.received.push(event.data); });
      }
      send(data) {
        if (typeof data === 'string' && control.rewriteAddTaskRevision != null) {
          const parsed = JSON.parse(data);
          if (parsed?.type !== 'todo_child_add_task') throw new Error('todo-child revision rewrite armed for a non-add-task frame');
          parsed.expectedRevision = control.rewriteAddTaskRevision;
          control.rewriteAddTaskRevision = null;
          data = JSON.stringify(parsed);
        }
        if (typeof data === 'string') {
          frameLog.sent.push(data);
          window.dispatchEvent(new CustomEvent('rpi-todo-frame-sent', { detail: data }));
        }
        return super.send(data);
      }
    }
    for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) Object.defineProperty(TestWebSocket, key, { value: NativeWebSocket[key] });
    window.WebSocket = TestWebSocket;
    window.__rpiTodoChildTest = control;
    window.__rpiTodoChildFrames = frameLog;
  });
}

async function httpRpc(page, command) {
  const response = await page.evaluate(async (body) => {
    const result = await fetch('/rpc', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });
    return { status: result.status, text: await result.text() };
  }, command);
  let frame;
  try { frame = JSON.parse(response.text); } catch { fail(`RPC ${command.type} returned non-JSON`); }
  if (response.status !== 200 || frame.success !== true) fail(`RPC ${command.type} failed: ${frame.error || response.text}`);
  return frame.data || {};
}

async function waitForRpc(page, command, predicate, label, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  let latest;
  while (Date.now() <= deadline) {
    latest = await httpRpc(page, command);
    if (predicate(latest)) return latest;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  fail(`${label} (timeout ${timeoutMs}ms)`);
}


async function liveMain() {
  if (!url) fail('RPI_URL is required');
  const browser = await chromium.launch(chromePath ? { executablePath: chromePath } : {});
  const livePageErrors = [];
  const liveConsoleErrors = [];
  const routePrompt = 'Implement the acceptance edit by creating todo-child-web-live-marker.txt in the current workspace with exactly WEB_TODO_CHILD_EDIT_6F21 plus one trailing newline; then use one supervised foreground shell process that waits until todo-child-web-live-release.txt exists before replying CHILD_ROUTE_RELEASED_WEB_6F21.';
  const followUp = 'Reply with exactly MAIN_FOLLOWUP_WEB_OK_6F21 and no other text.';
  const laterMessage = 'Reply with exactly SAME_CHILD_WEB_OK_6F21 and no other text.';
  try {
    const page = await browser.newPage();
    await installTodoSocketHarness(page);
    page.on('pageerror', (error) => livePageErrors.push(error.message));
    page.on('console', (message) => { if (message.type() === 'error') liveConsoleErrors.push(message.text()); });
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true, 'live WebSocket connection (Send stayed disabled)');
    const primaryState = await httpRpc(page, { type: 'get_state' });
    if (!primaryState.sessionId) fail('authoritative primary state omitted sessionId');
    await waitFor(
      page,
      () => Boolean(document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId),
      'live initial active session row',
      60000,
    );
    await switchSession(page, primaryState.sessionId, 'live select primary session');
    const sessionA = await activeSessionId(page);
    await waitFor(page, () => Boolean(document.getElementById('model-select')?.value), 'live provider/model identity');
    const modelIdentity = await page.inputValue('#model-select');
    const baselineState = await httpRpc(page, { type: 'get_state', sessionId: sessionA });
    const baselineJobs = await httpRpc(page, { type: 'job_list', sessionId: sessionA });

    await page.fill('#prompt-input', routePrompt);
    await page.press('#prompt-input', 'Enter');
    try {
      await page.waitForFunction(
        () => [...document.querySelectorAll('.msg--summary')].filter((node) => (node.textContent || '').includes('Routed to Todo ·')).length === 1,
        undefined,
        { timeout: 60000 },
      );
    } catch {
      const state = await httpRpc(page, { type: 'get_state', sessionId: sessionA });
      const children = await httpRpc(page, { type: 'todo_child_list', sessionId: sessionA, parentSessionId: sessionA });
      const dom = await page.evaluate(() => ({
        summaries: [...document.querySelectorAll('.msg--summary')].map((node) => (node.textContent || '').slice(0, 200)),
        activeSessionId: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
      }));
      fail(`typed live route missing: ${JSON.stringify({
        sessionA,
        model: state.model ? `${state.model.provider}/${state.model.id}` : '',
        isStreaming: state.isStreaming,
        messageCount: state.messageCount,
        children: children.map((child) => ({ childSessionId: child.childSessionId, status: child.status })),
        dom,
      })}`);
    }
    const routeProjection = await page.evaluate(() => {
      const node = [...document.querySelectorAll('.msg--summary')].find((candidate) => (candidate.textContent || '').includes('Routed to Todo ·'));
      return node?.textContent || '';
    });
    const match = routeProjection.match(/Routed to Todo · ([0-9a-f-]{36})/i);
    if (!match) fail(`typed route child identity missing: ${routeProjection}`);
    const childId = match[1];
    await openTodoPanel(page, 'live route');
    await waitFor(page, (id) => document.querySelector('.todo-child-row.is-selected')?.dataset.childSessionId === id, 'live DOM child selection', 60000, childId);
    await waitFor(page, () => (document.getElementById('todo-child-detail')?.textContent || '').includes('Running'), 'live child running', 120000);
    const backendList = await waitForRpc(
      page,
      { type: 'todo_child_list', sessionId: sessionA, parentSessionId: sessionA },
      (children) => children.some((child) => child.childSessionId === childId),
      'authoritative live child catalog',
      60000,
    );
    if (backendList.length !== 1 || backendList[0]?.childSessionId !== childId) {
      const frames = await page.evaluate(() => {
        const parsedSent = window.__rpiTodoChildFrames.sent.map((frame) => JSON.parse(frame));
        const parsedReceived = window.__rpiTodoChildFrames.received.map((frame) => JSON.parse(frame));
        const prompt = parsedSent.filter((frame) => frame.type === 'prompt').at(-1);
        const routed = parsedReceived.filter((frame) => frame.type === 'todo_child_routed').at(-1);
        return {
          prompt: prompt ? { id: prompt.id, sessionId: prompt.sessionId } : null,
          routed: routed ? {
            sessionId: routed.sessionId,
            parentSessionId: routed.receipt?.parentSessionId,
            childSessionId: routed.receipt?.childSessionId,
          } : null,
        };
      });
      fail(`backend/DOM child identity mismatch: ${JSON.stringify({ sessionA, childId, backendList, frames })}`);
    }
    const routeDetail = await httpRpc(page, { type: 'todo_child_detail', sessionId: sessionA, parentSessionId: sessionA, childSessionId: childId });
    if (routeDetail.receipts?.length !== 1) fail(`exactly one typed route receipt required: ${JSON.stringify(routeDetail.receipts)}`);
    const routeReceiptId = routeDetail.receipts[0].id;
    const routeFrame = await page.evaluate((receiptId) => {
      const frames = window.__rpiTodoChildFrames.received
        .map((raw) => { try { return JSON.parse(raw); } catch { return null; } })
        .filter((frame) => frame?.type === 'todo_child_routed' && frame.receipt?.id === receiptId);
      return frames.length === 1 ? frames[0] : null;
    }, routeReceiptId);
    if (!routeFrame
      || routeFrame.sessionId !== sessionA
      || routeFrame.receipt?.parentSessionId !== sessionA
      || routeFrame.receipt?.childSessionId !== childId
      || routeFrame.receipt?.status !== 'queued') {
      fail(`typed route frame identity/status mismatch: ${JSON.stringify(routeFrame)}`);
    }
    if (routeDetail.receipts[0].status !== 'running') {
      fail(`typed route receipt did not reach Running: ${JSON.stringify(routeDetail.receipts[0])}`);
    }
    const postRouteState = await httpRpc(page, { type: 'get_state', sessionId: sessionA });
    const postRouteJobs = await httpRpc(page, { type: 'job_list', sessionId: sessionA });
    if (postRouteState.messageCount !== baselineState.messageCount) fail('route admitted a parent model turn/transcript record');
    if (JSON.stringify(postRouteState.todoPhases) !== JSON.stringify(baselineState.todoPhases)) fail('route polluted parent Todo');
    if (`${postRouteState.model?.provider}/${postRouteState.model?.id}` !== modelIdentity) fail('route changed parent model');
    if ((postRouteJobs.jobs || []).length !== (baselineJobs.jobs || []).length) fail('route created parent task jobs');
    await page.screenshot({ path: `${evidence}/todo-child-routing-web.png`, fullPage: true });

    await page.click('#todo-close-btn');
    await waitFor(
      page,
      () => {
        const input = document.getElementById('prompt-input');
        const send = document.getElementById('send-btn');
        return input && !input.disabled && send && !send.disabled && send.getAttribute('aria-label') === 'Send message';
      },
      'live Main composer editable while Todo child runs',
    );
    await promptAndSettle(page, followUp, 'live Main follow-up');
    await waitFor(page, () => (document.getElementById('transcript')?.textContent || '').includes('MAIN_FOLLOWUP_WEB_OK_6F21'), 'live Main follow-up result', 180000);
    const afterFollowUp = await httpRpc(page, { type: 'todo_child_detail', sessionId: sessionA, parentSessionId: sessionA, childSessionId: childId });
    if (!afterFollowUp.isRunning) fail('live child stopped during Main follow-up');
    const routeWhileMainSettled = (afterFollowUp.receipts || []).find((receipt) => receipt.id === routeReceiptId);
    if (routeWhileMainSettled?.status !== 'running') fail(`live route receipt stopped during Main follow-up: ${JSON.stringify(routeWhileMainSettled)}`);
    await openTodoPanel(page, 'live Main settled while child running');
    await waitFor(
      page,
      () => document.querySelector('#todo-child-detail [role="status"]')?.getAttribute('aria-label') === 'Lifecycle status: Running',
      'live Running lifecycle projection after Main settlement',
    );
    const simultaneousState = await page.evaluate((id) => {
      const row = document.querySelector(`.todo-child-row.is-selected[data-child-session-id="${id}"]`);
      const lifecycle = document.querySelector('#todo-child-detail [role="status"]');
      return {
        childSessionId: row?.dataset.childSessionId || '',
        childStatus: row?.dataset.status || '',
        lifecycleLabel: lifecycle?.getAttribute('aria-label') || '',
        mainResultVisible: (document.getElementById('transcript')?.textContent || '').includes('MAIN_FOLLOWUP_WEB_OK_6F21'),
      };
    }, childId);
    if (simultaneousState.childSessionId !== childId
      || simultaneousState.childStatus !== 'running'
      || simultaneousState.lifecycleLabel !== 'Lifecycle status: Running'
      || !simultaneousState.mainResultVisible) {
      fail(`live Main/child concurrent state mismatch: ${JSON.stringify(simultaneousState)}`);
    }
    await page.screenshot({ path: `${evidence}/todo-child-running-after-main.png`, fullPage: true });
    const markerPath = path.join(work, 'workspace', 'todo-child-web-live-marker.txt');
    await waitForFile(markerPath, 'live shared-cwd marker', 180000);
    if (fs.readFileSync(markerPath, 'utf8') !== 'WEB_TODO_CHILD_EDIT_6F21\n') fail('live shared-cwd marker mismatch');

    const created = await createFreshSessionViaManage(page, waitFor, 'live create session B');
    const sessionB = created.sessionId;
    await openTodoPanel(page, 'live session B');
    const listB = await httpRpc(page, { type: 'todo_child_list', sessionId: sessionB, parentSessionId: sessionB });
    if (listB.length !== 0) fail('live session B contains session A child');
    await switchSession(page, sessionA, 'live restore session A');
    await openTodoPanel(page, 'live restored A');
    await waitFor(page, (id) => document.querySelector('.todo-child-row.is-selected')?.dataset.childSessionId === id, 'live restored child', 60000, childId);

    await page.evaluate(() => window.__rpiTodoChildTest.disconnect());
    await waitFor(page, () => document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length === 1, 'live reconnecting state (no transport toast)');
    await page.screenshot({ path: `${evidence}/todo-child-routing-reconnecting.png`, fullPage: true });
    await page.evaluate(() => window.__rpiTodoChildTest.reconnect());
    await waitFor(page, () => document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length === 0, 'live reconnected state (transport toast did not clear)', 60000);
    await openTodoPanel(page, 'live after reconnect');
    await waitFor(page, (id) => document.querySelector('.todo-child-row.is-selected')?.dataset.childSessionId === id, 'live child after reconnect', 60000, childId);

    fs.writeFileSync(path.join(work, 'workspace', 'todo-child-web-live-release.txt'), 'release\n');
    await page.fill('#todo-child-send-body', laterMessage);
    await page.click('#todo-child-send-btn');
    await waitFor(page, () => (document.getElementById('todo-child-detail')?.textContent || '').includes('SAME_CHILD_WEB_OK_6F21'), 'live same-child send result', 180000);
    const finalDetail = await waitForRpc(
      page,
      { type: 'todo_child_detail', sessionId: sessionA, parentSessionId: sessionA, childSessionId: childId },
      (detail) => detail.childSessionId === childId
        && detail.receipts?.some((receipt) => receipt.summary?.includes('SAME_CHILD_WEB_OK_6F21')),
      'authoritative same-child receipt summary',
      180000,
    );
    const routeReceiptRecords = (finalDetail.receipts || []).filter((receipt) => receipt.id === routeReceiptId);
    if (routeReceiptRecords.length !== 1
      || routeReceiptRecords[0].status !== 'completed'
      || routeReceiptRecords[0].summary !== 'CHILD_ROUTE_RELEASED_WEB_6F21') {
      fail(`typed route must settle once on its original receipt: ${JSON.stringify(routeReceiptRecords)}`);
    }
    if (finalDetail.childSessionId !== childId || !finalDetail.receipts?.some((receipt) => receipt.summary?.includes('SAME_CHILD_WEB_OK_6F21'))) {
      fail(`live same-child backend receipt missing: ${JSON.stringify({
        childId,
        detailChildId: finalDetail.childSessionId,
        receipts: (finalDetail.receipts || []).map((receipt) => ({
          id: receipt.id,
          status: receipt.status,
          summary: receipt.summary,
        })),
      })}`);
    }
    const routeProjectionCount = await page.locator('.msg--summary').filter({ hasText: 'Routed to Todo ·' }).count();
    if (routeProjectionCount !== 1) fail(`exactly one routed summary required after child reuse: ${routeProjectionCount}`);
    const receiptCountBeforeShutdown = finalDetail.receipts?.length || 0;
    await page.fill('#todo-child-send-body', 'Keep this receipt active for listener shutdown: start one supervised foreground shell process that waits until todo-child-web-shutdown-release.txt exists before replying WEB_SHUTDOWN_RELEASED_6F21.');
    await page.click('#todo-child-send-btn');
    await waitForRpc(
      page,
      { type: 'todo_child_detail', sessionId: sessionA, parentSessionId: sessionA, childSessionId: childId },
      (detail) => (detail.receipts?.length || 0) > receiptCountBeforeShutdown
        && detail.receipts.some((receipt) => receipt.status === 'running'),
      'authoritative live shutdown receipt running',
      120000,
    );
    await page.screenshot({ path: `${evidence}/todo-child-routing-final.png`, fullPage: true });
    if (livePageErrors.length || liveConsoleErrors.length) fail(`live browser errors: ${JSON.stringify({ livePageErrors, liveConsoleErrors })}`);
    fs.writeFileSync(`${evidence}/web-evidence.json`, JSON.stringify({
      surface: 'web', parentSessionId: sessionA, childSessionId: childId, routeReceiptId,
      routeReceiptCount: 1,
      routeReceiptStatuses: [routeFrame.receipt.status, routeWhileMainSettled.status, routeReceiptRecords[0].status],
      routeTerminalDeliveryCount: routeReceiptRecords.length,
      parentModelTurnCountForRoute: postRouteState.messageCount - baselineState.messageCount,
      parentTaskJobCount: (postRouteJobs.jobs || []).length - (baselineJobs.jobs || []).length,
      parentTodoSnapshotCount: postRouteState.todoPhases?.length || 0, parentRoutePromptPresent: false,
      providerModelIdentity: modelIdentity, mainFollowUpCompletedWhileChildRunning: true,
      mainComposerEditableWhileChildRunning: true, sharedCwdEdit: true,
      sameChildSend: true, displaySwitchSurvived: true, webSocketDisconnectSurvived: true,
      shutdownFailurePropagated: false, backendDomChildIdEqual: true,
      screenshots: ['todo-child-routing-web.png', 'todo-child-running-after-main.png', 'todo-child-routing-reconnecting.png', 'todo-child-routing-final.png'],
      pageErrors: livePageErrors, consoleErrors: liveConsoleErrors,
    }, null, 2) + '\n');
  } finally {
    await browser.close().catch(() => {});
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
    await installTodoSocketHarness(page);
    await page.exposeFunction('__rpiTodoFrameSent', (payload) => {
      if (typeof payload === 'string') sentFrames.push(payload);
    });
    await page.addInitScript(() => {
      window.addEventListener('rpi-todo-frame-sent', (event) => {
        window.__rpiTodoFrameSent(event.detail);
      });
    });


    page.on('pageerror', (err) => {
      pageErrors.push(String(err?.message || err));
    });
    page.on('console', (message) => {
      if (message.type() === 'error') consoleErrors.push(message.text());
    });
    page.on('websocket', (ws) => {
      ws.on('framesent', () => {});
      ws.on('framereceived', (frame) => {
        const payload = typeof frame.payload === 'string' ? frame.payload : '';
        if (payload) receivedFrames.push(payload);
      });
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'WS did not reach "connected"',
    );

    await waitFor(
      page,
      () => Boolean(document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId),
      'primary session id missing',
    );
    const sessionA = await activeSessionId(page);
    await waitFor(
      page,
      () => Boolean(document.getElementById('model-select')?.value),
      'primary session model/provider identity missing',
    );
    const mainModelIdentity = await page.inputValue('#model-select');


    await promptAndSettle(page, PARENT_SENTINEL, 'parent fork sentinel');

    const todoNavPresence = await page.evaluate(() => {
      const sidebar = document.querySelector('.session-sidebar__nav');
      const labels = sidebar ? [...sidebar.querySelectorAll('button')].map((button) => (button.textContent || '').trim()) : [];
      return {
        todo: labels.filter((label) => label === 'Todo').length,
        legacy: labels.filter((label) => label === 'Todos' || label === 'Todo children').length,
      };
    });
    if (todoNavPresence.todo !== 1 || todoNavPresence.legacy !== 0) {
      fail(`Todo feature-nav presence mismatch: ${JSON.stringify(todoNavPresence)}`);
    }

    await page.click('#command-btn');
    await waitFor(
      page,
      () => document.querySelector('.command-picker__popover') !== null,
      'Todo command picker did not open',
    );
    await waitFor(
      page,
      () => !document.querySelector('.command-picker__hint')
        && document.querySelectorAll('.command-picker__option .command-picker__name').length > 0,
      'Todo command picker catalog did not load',
    );
    const todoCommandRoots = await page.evaluate(() =>
      [...document.querySelectorAll('.command-picker__option .command-picker__name')]
        .map((node) => (node.textContent || '').trim())
        .filter((name) => name === '/todo' || name === '/todo-child'));
    if (todoCommandRoots.length !== 1 || todoCommandRoots[0] !== '/todo') {
      fail(`Todo picker command roots mismatch: ${JSON.stringify(todoCommandRoots)}`);
    }
    await page.press('.command-picker__search', 'Escape');
    await waitFor(
      page,
      () => document.querySelector('.command-picker__popover') === null,
      'Todo command picker did not close',
    );

    await openTodoPanel(page, 'initial open');
    await waitFor(
      page,
      () => (document.querySelector('.todo-child-list__empty')?.textContent || '').includes('autonomous session'),
      'empty Todo child list never rendered',
    );
    const initialSurfaces = await page.evaluate(() => ({
      todoRoots: document.querySelectorAll('#todo-panel').length,
      childRoot: document.getElementById('todo-child-panel') !== null,
      tasks: document.querySelector('.todo-panel__tasks') !== null,
      children: document.querySelector('.todo-child-section') !== null,
      activity: document.getElementById('activity-panel') !== null,
      workflow: document.getElementById('workflow-panel') !== null,
    }));
    if (initialSurfaces.todoRoots !== 1 || initialSurfaces.childRoot || !initialSurfaces.tasks
      || !initialSurfaces.children || initialSurfaces.activity || initialSurfaces.workflow) {
      fail(`unified Todo surface mismatch: ${JSON.stringify(initialSurfaces)}`);
    }
    await page.screenshot({ path: `${evidence}/todo-child-empty.png`, fullPage: true });

    await page.fill('#todo-add-phase', 'Plan');
    await page.fill('#todo-add-content', ORDINARY_TODO);
    await page.click('#todo-add-btn');
    await waitFor(
      page,
      (task) => [...document.querySelectorAll('.todo-task')].some((row) => (row.textContent || '').includes(task))
        && document.getElementById('todo-child-list') !== null,
      'ordinary Todo DAG task never appeared beside the child section',
      30000,
      ORDINARY_TODO,
    );
    await page.click('#todo-close-btn');
    await waitFor(page, () => document.getElementById('todo-panel') === null, 'Todo module did not close');
    await page.fill('#prompt-input', '/todo');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.querySelectorAll('#todo-panel').length === 1
        && document.getElementById('todo-child-panel') === null
        && document.querySelector('.todo-panel__tasks') !== null
        && document.querySelector('.todo-child-section') !== null,
      '/todo did not open the unified Todo surface',
    );
    const childListFocused = await page.evaluate(() => document.activeElement === document.getElementById('todo-child-list'));
    if (childListFocused) fail('/todo retained the removed todo-child focus alias behavior');
    await page.click('#todo-close-btn');
    await waitFor(page, () => document.getElementById('todo-panel') === null, '/todo Todo panel did not close');
    const beforeUnknownAlias = await page.evaluate(() => document.querySelectorAll('.msg--user').length);
    const aliasFrameStart = sentFrames.length;
    await page.fill('#prompt-input', '/todo-child');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (before) => document.querySelectorAll('.msg--user').length > before
        && document.getElementById('todo-panel') === null,
      '/todo-child did not follow normal unknown-command prompt behavior',
      30000,
      beforeUnknownAlias,
    );
    const aliasPrompt = framesOfType('prompt', aliasFrameStart)
      .find((frame) => frame.message === '/todo-child');
    if (!aliasPrompt) fail('/todo-child did not dispatch through the normal prompt path');
    await waitForIdle(page, '/todo-child unknown-command turn did not settle');

    await page.click('#activity-toggle-btn');
    await waitFor(page, () => document.getElementById('activity-panel') !== null, 'Activity panel did not open');
    const activityText = await page.evaluate(() => document.getElementById('activity-panel')?.textContent || '');
    if (!activityText.includes('Activity') || activityText.includes('Todo children')) {
      fail('Activity panel is not a distinct ordinary-activity surface');
    }
    if (await page.evaluate(() => document.getElementById('todo-panel') !== null)) {
      fail('Activity panel left the unified Todo panel mounted');
    }
    await page.click('#activity-close-btn');
    await waitFor(page, () => document.getElementById('activity-panel') === null, 'Activity panel did not close');

    await page.click('#workflow-toggle-btn');
    await waitFor(page, () => document.getElementById('workflow-panel') !== null, 'Workflow panel did not open');
    if (await page.evaluate(() => document.getElementById('todo-panel') !== null)) {
      fail('Workflow panel left the unified Todo panel mounted');
    }
    await page.click('#workflow-close-btn');
    await waitFor(page, () => document.getElementById('workflow-panel') === null, 'Workflow panel did not close');

    const createStart = sentFrames.length;
    await page.fill('#prompt-input', `/todo new ${CHILD_A_LABEL}`);
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (label) => document.querySelectorAll('#todo-panel').length === 1
        && [...document.querySelectorAll('.todo-child-row')]
          .some((row) => (row.textContent || '').includes(label)),
      '/todo new did not create and show child A in the unified Todo surface',
      30000,
      CHILD_A_LABEL,
    );
    await waitFor(
      page,
      (label) => [...document.querySelectorAll('.todo-child-row')].some((row) => (row.textContent || '').includes(label)),
      'created Todo child A never listed',
      30000,
      CHILD_A_LABEL,
    );
    await waitFor(
      page,
      () => (document.getElementById('todo-child-counts')?.textContent || '').includes('1 child'),
      'Todo child counts never showed 1 child',
    );
    await waitFor(
      page,
      (label) => {
        const selected = document.querySelector('.todo-child-row.is-selected');
        const detail = document.querySelector('.todo-child-detail');
        return !!selected
          && (selected.textContent || '').includes(label)
          && !!detail
          && (document.getElementById('todo-child-detail')?.textContent || '').includes(label);
      },
      'created Todo child A was never selected with detail',
      30000,
      CHILD_A_LABEL,
    );
    const childA = await selectedChildId(page);
    if (!childA) fail('child A session id missing after create/select');
    await assertUnifiedVisible(page, 'child A unified projection', {
      ordinaryTask: ORDINARY_TODO,
      childId: childA,
      childLabel: CHILD_A_LABEL,
    });
    const detailA = await detailText(page);
    if (!detailA.includes(sessionA) || !detailA.includes(childA)) {
      fail('child A detail missing parent+child identity');
    }
    if (!detailA.includes(PARENT_SENTINEL)) {
      fail('child A fork transcript missing the parent sentinel');
    }
    const ordinaryAfterChildCreate = await ordinaryTasksText(page);
    if (!ordinaryAfterChildCreate.includes(ORDINARY_TODO)) {
      fail('ordinary Todo DAG task disappeared after creating a durable child');
    }
    if (ordinaryAfterChildCreate.includes(CHILD_A_LABEL) || ordinaryAfterChildCreate.includes(childA)) {
      fail('durable child identity leaked into ordinary Todo task rows');
    }
    const childAfterOrdinary = await childSectionText(page);
    if (childAfterOrdinary.includes(ORDINARY_TODO)) {
      fail('ordinary Todo DAG content leaked into the child section');
    }
    const createProof = assertDispatched('todo_child_create', 'create child A', createStart, {
      sessionId: sessionA,
      parentSessionId: sessionA,
    });
    const createIdentity = createProof.response.data?.identity;
    if (createIdentity?.parentSessionId !== sessionA || createIdentity?.childSessionId !== childA) {
      fail(`todo_child_create response identity mismatch: ${JSON.stringify(createProof.response.data)}`);
    }
    const createListProof = assertDispatched('todo_child_list', 'list after create child A', createStart, {
      sessionId: sessionA,
      parentSessionId: sessionA,
    });
    if (!Array.isArray(createListProof.response.data)
      || !createListProof.response.data.some((entry) => entry?.childSessionId === childA)) {
      fail(`todo_child_list omitted child A: ${JSON.stringify(createListProof.response.data)}`);
    }

    if (await activeSessionId(page) !== sessionA) {
      fail('Todo child selection switched or rebound the main session');
    }
    if (await page.inputValue('#model-select') !== mainModelIdentity) {
      fail('Todo child create changed the main provider/model identity');
    }

    await page.screenshot({ path: `${evidence}/todo-child-created.png`, fullPage: true });

    await waitForIdle(page, 'parent post-fork idle');
    await promptAndSettle(page, PARENT_POST_FORK, 'parent post-fork append');
    await openTodoPanel(page, 'reopen after parent post-fork');
    const childARow = await page.evaluate((id) => document.getElementById(`todo-child-row-${id}`), childA);
    if (!childARow) {
      await page.evaluate((id) => {
        const row = [...document.querySelectorAll('.todo-child-row')]
          .find((candidate) => candidate.dataset.childSessionId === id);
        row?.click();
      }, childA);
    }
    await waitFor(
      page,
      (id) => document.querySelector('.todo-child-row.is-selected')?.dataset.childSessionId === id
        && document.querySelector('.todo-child-detail') !== null,
      'child A detail did not restore after parent post-fork',
      30000,
      childA,
    );
    const afterParentAppend = await detailText(page);
    if (afterParentAppend.includes(PARENT_POST_FORK)) {
      fail('post-fork parent append leaked into the child fork transcript');
    }
    const parentTranscript = await page.evaluate(() => document.getElementById('transcript')?.textContent || '');
    if (!parentTranscript.includes(PARENT_POST_FORK)) {
      fail('parent transcript missing the post-fork parent message');
    }
    if (parentTranscript.includes(CHILD_A_SEND)) {
      fail('child send leaked into the parent transcript before it was sent');
    }

    const addTaskStart = sentFrames.length;

    await page.fill('#todo-child-task-content', CHILD_A_TASK);
    await page.click('#todo-child-add-task-btn');
    await waitFor(
      page,
      (task) => {
        const detail = document.querySelector('.todo-child-detail');
        return !!detail && (detail.textContent || '').includes(task);
      },
      'typed add-task never appeared in child detail',
      30000,
      CHILD_A_TASK,
    );
    assertDispatched('todo_child_add_task', 'add-task', addTaskStart, {
      sessionId: sessionA,
      parentSessionId: sessionA,
      childSessionId: childA,
    });


    await page.evaluate(() => { window.__rpiTodoChildTest.rewriteAddTaskRevision = 0; });
    await page.fill('#todo-child-task-content', CHILD_A_STALE_TASK);
    await page.click('#todo-child-add-task-btn');
    await waitFor(
      page,
      (task) => {
        const errors = [...document.querySelectorAll('.todo-child-section__error')].map((node) => (node.textContent || '').toLowerCase());
        const detail = document.querySelector('.todo-child-detail')?.textContent || '';
        const stale = errors.some((text) => text.includes('stale') || text.includes('revision'));
        return stale && !detail.includes(task);
      },
      'stale add-task was not rejected fail-closed',
      20000,
      CHILD_A_STALE_TASK,
    );
    const afterStale = await detailText(page);
    if (!afterStale.includes(CHILD_A_TASK)) fail('successful add-task vanished after the stale rejection');

    const sendStart = sentFrames.length;

    await page.fill('#todo-child-send-body', CHILD_A_SEND);
    await page.click('#todo-child-send-btn');
    await waitFor(
      page,
      (marker) => {
        const detail = document.getElementById('todo-child-detail')?.textContent || '';
        return detail.includes(marker);
      },
      'typed send never appeared in child detail',
      30000,
      CHILD_A_SEND,
    );
    assertDispatched('todo_child_send', 'send', sendStart, {
      sessionId: sessionA,
      parentSessionId: sessionA,
      childSessionId: childA,
    });

    const parentAfterSend = await page.evaluate(() => document.getElementById('transcript')?.textContent || '');
    if (parentAfterSend.includes(CHILD_A_SEND)) {
      fail('typed child send leaked into the parent transcript');
    }

    await waitFor(
      page,
      () => [...document.querySelectorAll('#todo-child-hub-to option')].some((option) => option.value.trim() !== ''),
      'hub recipient list never populated from child supervisor/agents',
      20000,
    );
    const hubRecipient = await page.evaluate(() => {
      const option = [...document.querySelectorAll('#todo-child-hub-to option')].find((node) => node.value.trim() !== '');
      return option ? option.value : '';
    });
    if (!hubRecipient) fail('hub recipient missing after supervisor projection');
    await page.selectOption('#todo-child-hub-to', hubRecipient);
    const hubStart = sentFrames.length;

    await page.fill('#todo-child-hub-body', CHILD_A_HUB);
    await page.click('#todo-child-hub-send-btn');
    await waitFor(
      page,
      (body) => {
        const receipts = [...document.querySelectorAll('.todo-child-hub__receipt')];
        const detail = document.getElementById('todo-child-detail')?.textContent || '';
        return receipts.some((receipt) => receipt.dataset.outcome !== 'failed') && detail.includes(body);
      },
      'typed hub-send never produced a successful receipt and authoritative child detail',
      30000,
      CHILD_A_HUB,
    );
    assertDispatched('todo_child_hub_send', 'hub-send', hubStart, {
      sessionId: sessionA,
      parentSessionId: sessionA,
      childSessionId: childA,
    });
    assertTypedDistinct('typed send/add-task/hub-send');
    await page.screenshot({ path: `${evidence}/todo-child-typed-commands.png`, fullPage: true });
    await assertUnifiedVisible(page, 'typed child commands preserve unified projection', {
      ordinaryTask: ORDINARY_TODO,
      childId: childA,
      childLabel: CHILD_A_LABEL,
    });
    const tasksAfterChildCommands = await ordinaryTasksText(page);
    if (!tasksAfterChildCommands.includes(ORDINARY_TODO)) {
      fail('ordinary Todo DAG task vanished after child send/add-task');
    }
    if (tasksAfterChildCommands.includes(CHILD_A_SEND) || tasksAfterChildCommands.includes(CHILD_A_TASK)) {
      fail('child send/add-task entered ordinary Todo task rows');
    }

    const projectionBeforeMismatch = await detailAuthority(page);
    if (projectionBeforeMismatch.selectedChildId !== childA
      || projectionBeforeMismatch.detailChildId !== childA
      || projectionBeforeMismatch.parentSessionId !== sessionA
      || projectionBeforeMismatch.revision == null) {
      fail('child A authority was not admitted before the wrong-identity RPC');
    }
    const mismatchId = `todo-child-mismatch-${Date.now()}`;
    const receivedBeforeMismatch = receivedFrames.length;
    await page.evaluate((payload) => window.__rpiTodoChildTest.sendRaw(JSON.stringify(payload)), {
      type: 'todo_child_detail',
      id: mismatchId,
      sessionId: sessionA,
      parentSessionId: WRONG_PARENT_ID,
      childSessionId: childA,
    });
    const deadline = Date.now() + 10000;
    let mismatchResponse = null;
    while (Date.now() < deadline) {
      for (let i = receivedBeforeMismatch; i < receivedFrames.length; i += 1) {
        const frame = parseFrame(receivedFrames[i]);
        if (frame && frame.type === 'response' && frame.id === mismatchId) {
          mismatchResponse = frame;
          break;
        }
      }
      if (mismatchResponse) break;
      await page.waitForTimeout(50);
    }
    if (!mismatchResponse) fail('real listener never answered the wrong parent+child pairing');
    if (mismatchResponse.success === true) {
      fail('wrong parentSessionId+childSessionId pairing was accepted by the real listener');
    }
    const projectionAfterMismatch = await detailAuthority(page);
    if (projectionAfterMismatch.selectedChildId !== childA) {
      fail('wrong-identity RPC changed the selected Todo child');
    }
    if (projectionAfterMismatch.detailChildId !== childA
      || projectionAfterMismatch.parentSessionId !== sessionA) {
      fail('wrong-identity RPC changed the admitted Todo child authority');
    }
    if (projectionAfterMismatch.revision == null
      || projectionAfterMismatch.revision < projectionBeforeMismatch.revision) {
      fail('wrong-identity RPC regressed the admitted Todo child revision');
    }
    const detailAfterMismatch = projectionAfterMismatch.text;
    if (detailAfterMismatch.includes(WRONG_PARENT_ID)) fail('wrong parent identity leaked into child detail');
    if (!detailAfterMismatch.includes(CHILD_A_SEND) || !detailAfterMismatch.includes(CHILD_A_TASK)) {
      fail('selected child A projection lost typed send/add-task after the failed pairing');
    }

    await waitFor(
      page,
      () => !document.getElementById('todo-child-pause-btn')?.disabled,
      'todo_child_pause never became enabled',
      20000,
    );
    const pauseStart = sentFrames.length;
    await page.click('#todo-child-pause-btn');
    const pauseProof = await waitForDispatched(page, 'todo_child_pause', 'pause', pauseStart, {
      sessionId: sessionA,
      parentSessionId: sessionA,
      childSessionId: childA,
    });
    await waitFor(
      page,
      () => (document.querySelector('.todo-child-detail-pane__status')?.textContent || '').toLowerCase() === 'paused'
        && document.querySelector('.todo-child-row.is-selected')?.dataset.status === 'paused',
      'todo_child_pause never reached authoritative paused state',
      20000,
    );
    if (pauseProof.response.data?.parentSessionId !== sessionA
      || pauseProof.response.data?.childSessionId !== childA
      || pauseProof.response.data?.status !== 'paused') {
      fail(`todo_child_pause response mismatch: ${JSON.stringify(pauseProof.response.data)}`);
    }
    await waitFor(
      page,
      () => !document.getElementById('todo-child-resume-btn')?.disabled,
      'todo_child_resume never became enabled',
      20000,
    );
    const resumeStart = sentFrames.length;
    await page.click('#todo-child-resume-btn');
    const resumeProof = await waitForDispatched(page, 'todo_child_resume', 'resume', resumeStart, {
      sessionId: sessionA,
      parentSessionId: sessionA,
      childSessionId: childA,
    });
    await waitFor(
      page,
      () => (document.querySelector('.todo-child-detail-pane__status')?.textContent || '').toLowerCase() === 'idle'
        && document.querySelector('.todo-child-row.is-selected')?.dataset.status === 'idle',
      'todo_child_resume never reached authoritative idle state',
      20000,
    );
    if (resumeProof.response.data?.parentSessionId !== sessionA
      || resumeProof.response.data?.childSessionId !== childA
      || resumeProof.response.data?.status !== 'idle') {
      fail(`todo_child_resume response mismatch: ${JSON.stringify(resumeProof.response.data)}`);
    }
    if (await page.inputValue('#model-select') !== mainModelIdentity) {
      fail('Todo child lifecycle controls changed the main provider/model identity');
    }


    const createBStart = sentFrames.length;

    await page.fill('#todo-child-create-label', CHILD_B_LABEL);
    await page.click('#todo-child-create-btn');
    await waitFor(
      page,
      (label) => [...document.querySelectorAll('.todo-child-row')].some((row) => (row.textContent || '').includes(label)),
      'created Todo child B never listed',
      30000,
      CHILD_B_LABEL,
    );
    await waitFor(
      page,
      () => (document.getElementById('todo-child-counts')?.textContent || '').includes('2 children'),
      'Todo child counts never showed 2 children',
    );
    const childB = await page.evaluate((label) => {
      const row = [...document.querySelectorAll('.todo-child-row')]
        .find((candidate) => (candidate.textContent || '').includes(label));
      return row?.dataset.childSessionId || '';
    }, CHILD_B_LABEL);
    if (!childB || childB === childA) fail('child B identity missing or collided with child A');
    const createBProof = assertDispatched('todo_child_create', 'create child B', createBStart, {
      sessionId: sessionA,
      parentSessionId: sessionA,
    });
    if (createBProof.response.data?.identity?.parentSessionId !== sessionA
      || createBProof.response.data?.identity?.childSessionId !== childB) {
      fail(`todo_child_create child B identity mismatch: ${JSON.stringify(createBProof.response.data)}`);
    }
    await page.evaluate((id) => {
      document.querySelector(`.todo-child-row[data-child-session-id="${id}"]`)?.click();
    }, childA);
    await waitFor(
      page,
      (id) => document.querySelector('.todo-child-row.is-selected')?.dataset.childSessionId === id,
      'child A was not selected before explicit child B selection',
      20000,
      childA,
    );


    const selectBStart = sentFrames.length;

    await page.evaluate((id) => {
      document.querySelector(`.todo-child-row[data-child-session-id="${id}"]`)?.click();
    }, childB);
    await waitFor(
      page,
      (id) => document.querySelector('.todo-child-row.is-selected')?.dataset.childSessionId === id
        && (document.querySelector('.todo-child-detail')?.getAttribute('aria-label') || '').includes(id),
      'selecting child B never swapped the detail identity',
      30000,
      childB,
    );
    assertDispatched('todo_child_select', 'select child B', selectBStart, {
      sessionId: sessionA,
      parentSessionId: sessionA,
      childSessionId: childB,
    });

    const detailB = await detailText(page);
    if (detailB.includes(CHILD_A_SEND) || detailB.includes(CHILD_A_TASK)) {
      fail('child A receipts/tasks leaked into child B detail');
    }
    const sendBStart = sentFrames.length;

    await page.fill('#todo-child-send-body', CHILD_B_SEND);
    await page.click('#todo-child-send-btn');
    await waitFor(
      page,
      (marker) => (document.getElementById('todo-child-detail')?.textContent || '').includes(marker),
      'child B typed send never appeared in B detail',
      30000,
      CHILD_B_SEND,
    );
    assertDispatched('todo_child_send', 'send child B', sendBStart, {
      sessionId: sessionA,
      parentSessionId: sessionA,
      childSessionId: childB,
    });

    if (await activeSessionId(page) !== sessionA) {
      fail('switching Todo child identity switched the main session');
    }
    const selectAStart = sentFrames.length;

    await page.evaluate((id) => {
      document.querySelector(`.todo-child-row[data-child-session-id="${id}"]`)?.click();
    }, childA);
    await waitFor(
      page,
      (state) => {
        const selected = document.querySelector('.todo-child-row.is-selected')?.dataset.childSessionId;
        const detail = document.getElementById('todo-child-detail')?.textContent || '';
        return selected === state.childA && detail.includes(state.sendA) && !detail.includes(state.sendB);
      },
      'switching back to child A leaked child B state',
      30000,
      { childA, sendA: CHILD_A_SEND, sendB: CHILD_B_SEND },
    );
    assertDispatched('todo_child_select', 'select child A again', selectAStart, {
      sessionId: sessionA,
      parentSessionId: sessionA,
      childSessionId: childA,
    });

    if (await activeSessionId(page) !== sessionA) {
      fail('returning to Todo child A switched the main session');
    }
    if (await page.inputValue('#model-select') !== mainModelIdentity) {
      fail('Todo child selection changed the main provider/model identity');
    }


    const created = await createFreshSessionViaManage(page, waitFor, 'create parent session B');
    const sessionB = created.sessionId;
    if (!sessionB || sessionB === sessionA) fail('fresh parent session B identity missing');
    const sessionBListStart = await page.evaluate(() => window.__rpiTodoChildFrames.sent.length);

    await openTodoPanel(page, 'session B isolation');
    await waitFor(
      page,
      (state) => {
        const listRequests = window.__rpiTodoChildFrames.sent.slice(state.start)
          .map((raw) => { try { return JSON.parse(raw); } catch { return null; } })
          .filter((frame) => frame?.type === 'todo_child_list'
            && frame.sessionId === state.sessionB
            && frame.parentSessionId === state.sessionB);
        return listRequests.some((request) => window.__rpiTodoChildFrames.received
          .map((raw) => { try { return JSON.parse(raw); } catch { return null; } })
          .some((response) => response?.type === 'response'
            && response.id === request.id
            && response.command === 'todo_child_list'
            && response.sessionId === state.sessionB
            && response.success === true
            && Array.isArray(response.data)
            && response.data.length === 0));
      },
      'session B never received an authoritative empty todo_child_list response',
      30000,
      { start: sessionBListStart, sessionB },
    );
    const childSectionB = await childSectionText(page);
    const ordinaryB = await ordinaryTasksText(page);
    if (childSectionB.includes(CHILD_A_SEND) || childSectionB.includes(CHILD_A_LABEL)
      || ordinaryB.includes(ORDINARY_TODO)) {
      fail('session A Todo domains leaked into authoritative session B projection');
    }
    const bEmpty = await page.evaluate(() => document.querySelector('.todo-child-list__empty') !== null
      && document.querySelectorAll('.todo-child-row').length === 0);
    if (!bEmpty) fail('session B listed Todo children that do not belong to it');
    await page.screenshot({ path: `${evidence}/todo-session-b-isolation.png`, fullPage: true });

    await switchSession(page, sessionA, 'restore parent A after isolation');
    await openTodoPanel(page, 'restore parent A Todo');
    await waitFor(
      page,
      (state) => [...document.querySelectorAll('.todo-child-row')]
        .some((row) => row.dataset.childSessionId === state.childA && (row.textContent || '').includes(state.labelA)),
      'parent A Todo children did not survive the session switch',
      30000,
      { childA, labelA: CHILD_A_LABEL },
    );
    const statusAfterSwitchStart = sentFrames.length;
    await page.fill('#todo-child-send-body', CHILD_A_AFTER_SWITCH);
    await page.click('#todo-child-send-btn');
    await waitFor(
      page,
      (marker) => (document.getElementById('todo-child-detail')?.textContent || '').includes(marker),
      'parent A did not accept later child work after A→B→A',
      30000,
      CHILD_A_AFTER_SWITCH,
    );
    const statusAfterSwitchProof = assertDispatched('todo_child_send', 'send after A→B→A', statusAfterSwitchStart, {
      sessionId: sessionA,
      parentSessionId: sessionA,
      childSessionId: childA,
    });
    if (statusAfterSwitchProof.response.data?.childSessionId !== childA) {
      fail(`A→B→A send changed backend child identity: ${JSON.stringify(statusAfterSwitchProof.response.data)}`);
    }
    await promptAndSettle(page, PARENT_RESTART_ANCHOR, 'parent restart selection anchor');
    await openTodoPanel(page, 'after parent restart selection anchor');
    await waitFor(
      page,
      (state) => {
        const selected = document.querySelector('.todo-child-row.is-selected')?.dataset.childSessionId;
        const detail = document.getElementById('todo-child-detail')?.textContent || '';
        return selected === state.childA && !detail.includes(state.anchor);
      },
      'restart anchor changed the selected child fork projection',
      30000,
      { childA, anchor: PARENT_RESTART_ANCHOR },
    );
    const restoredA = await childSectionText(page);
    if (restoredA.includes(CHILD_B_SEND) && !(await page.evaluate((id) =>
      [...document.querySelectorAll('.todo-child-row')].some((row) => row.dataset.childSessionId === id)
    , childB))) {
      fail('child B send leaked into parent A without child B listed');
    }
    const restoredOrdinaryA = await ordinaryTasksText(page);
    if (!restoredOrdinaryA.includes(ORDINARY_TODO)) {
      fail('parent A ordinary Todo DAG did not recover with its child sessions');
    }

    await page.evaluate((id) => {
      document.querySelector(`.todo-child-row[data-child-session-id="${id}"]`)?.click();
    }, childA);
    await waitFor(
      page,
      (id) => document.querySelector('.todo-child-row.is-selected')?.dataset.childSessionId === id,
      'child A could not be reselected before reconnect',
      20000,
      childA,
    );

    await page.evaluate(() => window.__rpiTodoChildTest.disconnect());
    await waitFor(
      page,
      () => document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length === 1,
      'same-process outage never raised the transport outage toast',
    );
    await page.screenshot({ path: `${evidence}/todo-listener-reconnecting.png`, fullPage: true });

    await page.evaluate(() => window.__rpiTodoChildTest.reconnect());
    await waitFor(
      page,
      () => document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length === 0,
      'same-process outage never auto-reconnected (transport toast did not clear)',
      30000,
    );
    await page.click('#todo-close-btn');
    await waitFor(page, () => document.getElementById('todo-panel') === null, 'Todo panel did not close after WS reconnect');
    await openTodoPanel(page, 'after WS reconnect');
    await waitFor(
      page,
      (state) => [...document.querySelectorAll('.todo-child-row')]
        .some((row) => row.dataset.childSessionId === state.childA),
      'Todo child A vanished after the WebSocket drop',
      30000,
      { childA },
    );
    await assertUnifiedVisible(page, 'WS reconnect unified recovery', {
      ordinaryTask: ORDINARY_TODO,
      childId: childA,
      childLabel: CHILD_A_LABEL,
    });
    const sendAfterReconnectStart = sentFrames.length;
    await page.fill('#todo-child-send-body', CHILD_A_AFTER_RECONNECT);
    await page.click('#todo-child-send-btn');
    await waitFor(
      page,
      (marker) => (document.getElementById('todo-child-detail')?.textContent || '').includes(marker),
      'Todo child A did not accept work after the WebSocket reconnect',
      30000,
      CHILD_A_AFTER_RECONNECT,
    );
    const sendAfterReconnectProof = assertDispatched(
      'todo_child_send',
      'send after WebSocket reconnect',
      sendAfterReconnectStart,
      { sessionId: sessionA, parentSessionId: sessionA, childSessionId: childA },
    );
    if (sendAfterReconnectProof.response.data?.childSessionId !== childA) {
      fail(`WebSocket reconnect changed backend child identity: ${JSON.stringify(sendAfterReconnectProof.response.data)}`);
    }
    const shutdownFrames = receivedFrames
      .map(parseFrame)
      .filter((frame) => JSON.stringify(frame).toLowerCase().includes('orchestration shut down'));
    if (shutdownFrames.length !== 0) {
      fail(`disconnect or displayed switch shut down orchestration: ${JSON.stringify(shutdownFrames)}`);
    }
    const reconnectErrors = await page.evaluate(() => ({
      sendDisabled: document.getElementById('send-btn')?.hasAttribute('disabled') === true,
      panel: [...document.querySelectorAll('.todo-child-section__error')]
        .map((node) => (node.textContent || '').trim())
        .filter(Boolean),
      toasts: [...document.querySelectorAll('#toasts .toast')]
        .map((node) => (node.textContent || '').trim())
        .filter(Boolean),
    }));
    const staleReconnectErrors = [...reconnectErrors.panel, ...reconnectErrors.toasts]
      .filter((message) => message.includes('connection closed') || message.includes('send failed'));
    if (reconnectErrors.sendDisabled || staleReconnectErrors.length !== 0) {
      fail(`Todo child reconnect retained transport errors: ${JSON.stringify(reconnectErrors)}`);
    }
    await page.screenshot({ path: `${evidence}/todo-child-ws-reconnect.png`, fullPage: true });

    let sawTransportToast = false;
    const observedToasts = [];
    const toastWatch = setInterval(async () => {
      try {
        const count = await page.evaluate(() =>
          document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length
        );
        observedToasts.push(count);
        if (count >= 1) sawTransportToast = true;
      } catch {
        /* page mid-navigation / closed */
      }
    }, 100);
    fs.writeFileSync(KILL_MARKER, 'kill now\n');
    await waitForFile(UP_MARKER, 'lane never respawned the listener');
    clearInterval(toastWatch);
    if (!sawTransportToast) {
      fail(`no transport outage toast after the server was killed (observed counts: ${observedToasts.slice(-25).join(',')})`);
    }
    await waitFor(
      page,
      () => document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length === 0,
      'client never auto-reconnected after the listener came back',
    );
    if (await page.evaluate(() => document.getElementById('todo-panel') !== null)) {
      await page.click('#todo-close-btn');
      await waitFor(page, () => document.getElementById('todo-panel') === null, 'stale Todo panel did not close after durable restart');
    }
    await openTodoPanel(page, 'after durable restart');
    await waitFor(
      page,
      (state) => [...document.querySelectorAll('.todo-child-row')]
        .some((row) => row.dataset.childSessionId === state.childA && (row.textContent || '').includes(state.labelA)),
      'durable Todo child A did not recover after listener restart',
      30000,
      { childA, labelA: CHILD_A_LABEL },
    );
    const restartSelectBStart = sentFrames.length;
    await page.evaluate((id) => {
      document.querySelector(`.todo-child-row[data-child-session-id="${id}"]`)?.click();
    }, childB);
    await waitFor(
      page,
      (id) => document.querySelector('.todo-child-row.is-selected')?.dataset.childSessionId === id,
      'child B could not establish backend selection after durable restart',
      20000,
      childB,
    );
    await waitForDispatched(page, 'todo_child_select', 'select child B after restart', restartSelectBStart, {
      sessionId: sessionA,
      parentSessionId: sessionA,
      childSessionId: childB,
    });
    const restartSelectAStart = sentFrames.length;
    await page.evaluate((id) => {
      document.querySelector(`.todo-child-row[data-child-session-id="${id}"]`)?.click();
    }, childA);
    await waitForDispatched(page, 'todo_child_select', 'select child A after restart', restartSelectAStart, {
      sessionId: sessionA,
      parentSessionId: sessionA,
      childSessionId: childA,
    });
    await waitFor(
      page,
      (state) => {
        const selected = document.querySelector('.todo-child-row.is-selected')?.dataset.childSessionId;
        const detail = document.getElementById('todo-child-detail')?.textContent || '';
        return selected === state.childA && detail.includes(state.task);
      },
      'child A detail did not recover add-task state after restart',
      30000,
      { childA, task: CHILD_A_TASK },
    );
    if (await activeSessionId(page) !== sessionA || await page.inputValue('#model-select') !== mainModelIdentity) {
      fail('durable Todo restart changed the main session/provider/model identity');
    }
    await page.screenshot({ path: `${evidence}/todo-durable-restart.png`, fullPage: true });

    await waitFor(
      page,
      () => !document.getElementById('todo-child-stop-btn')?.disabled,
      'todo_child_stop never became enabled',
      20000,
    );
    const stopStart = sentFrames.length;
    await page.click('#todo-child-stop-btn');
    const stopProof = await waitForDispatched(page, 'todo_child_stop', 'stop', stopStart, {
      sessionId: sessionA,
      parentSessionId: sessionA,
      childSessionId: childA,
    });
    if (stopProof.response.data?.parentSessionId !== sessionA
      || stopProof.response.data?.childSessionId !== childA
      || stopProof.response.data?.status !== 'stopped') {
      fail(`todo_child_stop response mismatch: ${JSON.stringify(stopProof.response.data)}`);
    }

    await waitFor(
      page,
      (id) => {
        const selected = document.querySelector('.todo-child-row.is-selected');
        const detail = document.querySelector('.todo-child-detail');
        const detailStatus = document.querySelector('.todo-child-detail-pane__status');
        const errors = [...document.querySelectorAll('.todo-child-section__error')]
          .map((node) => (node.textContent || '').trim())
          .filter(Boolean);
        return selected?.dataset.childSessionId === id
          && selected?.dataset.status === 'stopped'
          && (detailStatus?.textContent || '').trim().toLowerCase() === 'stopped'
          && (detail?.getAttribute('aria-label') || '').includes(id)
          && (detail?.textContent || '').includes('Stopped')
          && errors.length === 0;
      },
      'todo_child_stop never reached one authoritative stopped list/detail projection',
      30000,
      childA,
    );
    const finalToasts = await page.evaluate(() => [...document.querySelectorAll('#toasts .toast')]
      .map((node) => (node.textContent || '').trim())
      .filter(Boolean));
    if (finalToasts.length !== 0) {
      fail(`stale toast remained after authoritative stopped projection: ${JSON.stringify(finalToasts)}`);
    }

    await assertUnifiedVisible(page, 'final unified Todo proof', {
      ordinaryTask: ORDINARY_TODO,
      childId: childA,
      childLabel: CHILD_A_LABEL,
    });
    const finalOrdinary = await ordinaryTasksText(page);
    if (finalOrdinary.includes(CHILD_A_SEND) || finalOrdinary.includes(CHILD_A_TASK)) {
      fail('final ordinary task section contains child-only state');
    }
    const finalChild = await childSectionText(page);
    if (finalChild.includes(ORDINARY_TODO)) {
      fail('final child section contains ordinary Todo DAG state');
    }
    await page.screenshot({ path: `${evidence}/todo-unified.png`, fullPage: true });
    const unexpectedConsole = consoleErrors.filter((text) => !isDocumentedListenerOutageConsole(text));
    if (pageErrors.length || unexpectedConsole.length) {
      fail(`browser errors: ${JSON.stringify({ pageErrors, consoleErrors: unexpectedConsole })}`);
    }
    console.log('web-todo-child: PASSED (one unified Todo root + simultaneous ordinary/child projections + selection preservation + typed RPCs + parent/child/session isolation + WS drop + durable restart)');
  } finally {
    await browser.close().catch(() => {});
  }
}

if (process.env.RPI_TODO_CHILD_LIVE === '1') {
  liveMain().catch((err) => {
    console.error(`web-todo-child-live: FAIL: ${err && err.message ? err.message : err}`);
    process.exitCode = 2;
  });
} else {
  main().catch((err) => {
    if (process.exitCode !== 2) {
      console.error(`web-todo-child: FAIL: ${err && err.message ? err.message : err}`);
      process.exitCode = 2;
    }
  });
}
