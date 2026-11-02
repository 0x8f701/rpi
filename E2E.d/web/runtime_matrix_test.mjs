// Web runtime release matrix lane (playwright half of runtime_matrix.sh); IDs
// RM0.1-RM6.3 are registered in coverage_matrix.mjs (feature 'runtime release
// matrix'). Env: RPI_URL, RPI_TOKEN, RPI_CHROME, RPI_EVIDENCE, RPI_MOCK_PORT,
// RPI_PID. Exits 2 on any failure; playwright-only, no fallback, no skip.
import fs from 'node:fs';
import path from 'node:path';
import { chromium } from 'playwright';
import {
  MAX_LOADED_RUNTIMES,
  createControlClient,
  requireRpcData,
  parseLifecycleIdentity,
  loadedIdsFromList,
} from './lib/p0_resource_driver.mjs';
import { createFreshSessionViaManage } from './lib/session-lifecycle.mjs';
import {
  captureResourceSnapshot,
  connectionFdSocketCount,
  listenerEstablishedConnectionCount,
  processTreeRssSnapshot,
  tcpConnectionCount,
  writeEvidenceJson,
} from './lib/p0_resource_metrics.mjs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const mockPort = process.env.RPI_MOCK_PORT || '';
const listenerPid = Number(process.env.RPI_PID);
const coveragePath = path.join(evidence, 'coverage-assertions.json');
const metricsPath = path.join(evidence, 'runtime-matrix-metrics.json');

const MAX_SWITCH_MS = 1000;
const MODEL_TWO = 'user-steering/mock-2';
const FILL_PROMPT = (i) => `rm-fill-${i}`;
const FILL_REPLY = (i) => `sessions-reply: rm-fill-${i}`;
const STREAM_PROMPT = 'sessions-slow-b3';
const STREAM_FIRST_DELTA = 'sessions-slow-b3-1/';
const PPROCESS_LABEL = 'rm-protected-sleep';
const PGOAL_OBJECTIVE = 'rm-goal-protect';
const PTODO_TASK = 'rm-in-progress-task';
const V_GOAL_OBJECTIVE = 'rm-goal-v-complete';
const V_TODO_DONE = 'rm-v-done-task';
const V_ECHO_PROMPT = 'rm-v-persist';
const V_ECHO_REPLY = 'sessions-reply: rm-v-persist';
// Per-pool-slot journey budget (observed ~4.7MB per loaded runtime); the
// plateau spread/slope checks below still catch active leaks independent of
// pool size.
const RSS_GROWTH_FLOOR_BYTES = 8 * 1024 * 1024 * MAX_LOADED_RUNTIMES;
const RSS_PLATEAU_SAMPLES = 12;
const RSS_PLATEAU_CADENCE_MS = 500;
const RSS_PLATEAU_SPREAD_BYTES = 8 * 1024 * 1024;
const RSS_PLATEAU_MAX_SLOPE_BYTES_PER_MS = 4096;
const MAX_FD_SOCKET_GROWTH = 8;
const MAX_LISTENER_ESTABLISHED = 8;


const DOCUMENTED_IDS = [
  'RM0.1', 'RM0.2',
  'RM1.1',
  'RM2.1', 'RM2.2', 'RM2.3', 'RM2.4',
  'RM3.1', 'RM3.2',
  'RM4.1', 'RM4.2', 'RM4.3',
  'RM5.1', 'RM5.2',
  'RM6.1', 'RM6.2', 'RM6.3',
];
const executed = new Map();
function record(id) {
  executed.set(id, (executed.get(id) || 0) + 1);
}
const stageMarks = {};
function markStage(key) {
  stageMarks[key] = Date.now();
}
function fail(message) {
  throw new Error(`runtime-matrix: ${message}`);
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

function isDocumentedChromiumWsNetError(text) {
  return typeof text === 'string'
    && DOCUMENTED_WS !== ''
    && text.includes('WebSocket connection to ')
    && text.includes(DOCUMENTED_WS)
    && (text.includes('net::ERR_INTERNET_DISCONNECTED') || text.includes('net::ERR_CONNECTION_REFUSED'));
}

{
  const sample = DOCUMENTED_WS || 'ws://127.0.0.1:1/ws';
  const disconnected = `WebSocket connection to '${sample}' failed: Error in connection establishment: net::ERR_INTERNET_DISCONNECTED`;
  const refused = `WebSocket connection to '${sample}' failed: Error in connection establishment: net::ERR_CONNECTION_REFUSED`;
  if (DOCUMENTED_WS && (!isDocumentedChromiumWsNetError(disconnected) || !isDocumentedChromiumWsNetError(refused))) {
    fail('Chromium /ws net-error classifier missed a documented console error');
  }
  if (isDocumentedChromiumWsNetError('TypeError: boom')) {
    fail('Chromium /ws net-error classifier swallowed an unrelated error');
  }
  if (isDocumentedChromiumWsNetError("WebSocket connection to 'ws://evil.example/ws' failed: Error in connection establishment: net::ERR_CONNECTION_REFUSED")) {
    fail('Chromium /ws net-error classifier swallowed an other-host /ws error');
  }
}

function unexpectedBrowserErrors(entries) {
  return entries.filter((entry) => {
    const text = entry.replace(/^desktop (console|pageerror|websocket): /, '');
    return !isDocumentedChromiumWsNetError(text);
  });
}
async function waitFor(page, fn, id, timeoutMs = 30000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${id} (timeout ${timeoutMs}ms)`);
  }
}
function sessionState(data) {
  if (data && typeof data.state === 'object' && data.state !== null && typeof data.state.cwd === 'string') {
    return data.state;
  }
  return data;
}
function monitorBrowserErrors(page, label) {
  const errors = [];
  page.on('pageerror', (err) => errors.push(`${label} pageerror: ${err.message}`));
  page.on('console', (message) => {
    if (message.type() === 'error') errors.push(`${label} console: ${message.text()}`);
  });
  page.on('websocket', (socket) => {
    socket.on('socketerror', (error) => errors.push(`${label} websocket: ${String(error)}`));
  });
  return errors;
}
async function assertScreenshot(page, filePath, label) {
  await page.screenshot({ path: filePath, fullPage: true });
  if (!fs.existsSync(filePath) || fs.statSync(filePath).size === 0) {
    fail(`${label}: screenshot did not land on disk: ${path.basename(filePath)}`);
  }
}
async function activeTranscript(page) {
  return page.evaluate(() =>
    document.querySelector('#transcript .transcript-surface:not([hidden])')?.textContent || '');
}
async function activeSessionId(page) {
  return page.evaluate(() =>
    document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '');
}
async function loadedIds(page) {
  return page.evaluate(() => [...document.querySelectorAll('.session-sidebar__loaded')]
    .map((row) => row.getAttribute('data-loaded-for') || '').filter(Boolean));
}
async function clickRow(page, sid, id) {
  const visible = await page.evaluate((s) => {
    const row = [...document.querySelectorAll('.session-sidebar__switch')]
      .find((candidate) => candidate.dataset.sessionId === s);
    return row !== undefined && !row.disabled;
  }, sid);
  if (!visible) {
    const search = page.locator('#session-sidebar-search');
    if ((await search.count()) === 0) fail(`${id}: session row ${sid} missing and search box is absent`);
    await search.fill(sid);
    await waitFor(
      page,
      (s) => {
        const row = [...document.querySelectorAll('.session-sidebar__switch')]
          .find((candidate) => candidate.dataset.sessionId === s);
        return row !== undefined && !row.disabled;
      },
      `${id}: session row ${sid} never became clickable after search`,
      30000,
      sid
    );
  }
  const row = page.locator(`.session-sidebar__switch[data-session-id="${sid}"]`);
  await row.scrollIntoViewIfNeeded();
  await row.click();
  if (!visible) {
    const clear = page.locator('#session-sidebar-search-clear');
    if ((await clear.count()) > 0) await clear.click();
    else await page.locator('#session-sidebar-search').fill('');
  }
  await waitFor(
    page,
    (s) => {
      const rowEl = [...document.querySelectorAll('.session-sidebar__row')]
        .find((candidate) => candidate.querySelector('.session-sidebar__switch')?.dataset.sessionId === s);
      const el = document.getElementById('session-lifecycle');
      return rowEl?.classList.contains('session-sidebar__row--active') === true
        && el?.dataset.displayedSessionId === s
        && el?.dataset.authoritativeReady === 'true'
        && el?.dataset.pendingGeneration === '';
    },
    `${id}: session ${sid} never became authoritative-ready`,
    30000,
    sid
  );
}
async function waitForNewRow(page, knownIds, id, timeoutMs = 30000) {
  await waitFor(
    page,
    (known) => {
      const ids = [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId || '');
      return ids.some((sid) => sid !== '' && !known.includes(sid));
    },
    id,
    timeoutMs,
    knownIds
  );
  return page.evaluate((known) => {
    const ids = [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId || '');
    return ids.find((sid) => sid !== '' && !known.includes(sid)) || '';
  }, knownIds);
}
async function waitForEmptyView(page, id) {
  await waitFor(page, () => document.querySelector('#transcript .empty-hint') !== null, id, 30000);
}
async function waitForIdle(page, id, timeoutMs = 30000) {
  await waitFor(
    page,
    () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message',
    id,
    timeoutMs
  );
}
async function promptAndWait(page, prompt, reply, id, timeoutMs = 30000) {
  await waitForIdle(page, `${id}: pre-send idle`, timeoutMs);
  await page.fill('#prompt-input', prompt);
  await page.press('#prompt-input', 'Enter');
  await waitFor(page, (r) => document.body.textContent.includes(r), id, timeoutMs, reply);
}
async function assertZeroErrorToasts(page, id) {
  const toasts = await page.evaluate(() => [...document.querySelectorAll('#toasts .toast--error')]
    .map((t) => t.textContent || ''));
  if (toasts.length !== 0) fail(`${id}: unexpected error toast(s): ${JSON.stringify(toasts)}`);
}
async function assertProtectedLoaded(page, ids, id) {
  const loaded = await loadedIds(page);
  const missing = ids.filter((sid) => !loaded.includes(sid));
  if (loaded.length !== MAX_LOADED_RUNTIMES) {
    fail(`${id}: loaded pool is ${loaded.length}, expected exactly ${MAX_LOADED_RUNTIMES}: ${JSON.stringify(loaded)}`);
  }
  if (missing.length !== 0) fail(`${id}: protected/required sessions left the loaded pool: ${JSON.stringify(missing)}`);
  return loaded;
}

async function main() {
  if (!url) fail('RPI_URL is required');
  if (!mockPort) fail('RPI_MOCK_PORT is required for the held-stream release gate');
  if (!Number.isFinite(listenerPid) || listenerPid <= 0) fail('RPI_PID is required for the resource bounds');
  fs.mkdirSync(evidence, { recursive: true });
  fs.rmSync(coveragePath, { force: true });
  fs.rmSync(metricsPath, { force: true });
  const wsUrl = url.replace(/^http/, 'ws').replace(/\/web\/?$/, '/ws');
  const listenerPort = Number(new URL(url).port);
  const browser = await chromium.launch(chromePath ? { executablePath: chromePath } : {});
  try {
    const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    const browserErrors = monitorBrowserErrors(page, 'desktop');
    await page.addInitScript(() => {
      const NativeWebSocket = window.WebSocket;
      const control = { frames: [], live: new Set(), nextSeq: 0 };
      class RmTestWebSocket extends NativeWebSocket {
        constructor(...args) {
          super(...args);
          control.live.add(this);
          this.addEventListener('close', () => { control.live.delete(this); });
          this.addEventListener('message', (event) => {
            let frame;
            try { frame = JSON.parse(String(event.data)); } catch { return; }
            if (!frame || typeof frame.type !== 'string') return;
            const data = frame.data && typeof frame.data === 'object' ? frame.data : {};
            control.frames.push({
              dir: 'in',
              seq: control.nextSeq++,
              t: Date.now(),
              type: frame.type,
              id: typeof frame.id === 'string' ? frame.id : '',
              sessionId: typeof frame.sessionId === 'string' ? frame.sessionId : (typeof data.sessionId === 'string' ? data.sessionId : ''),
              runtimeIncarnation: typeof frame.runtimeIncarnation === 'string' ? frame.runtimeIncarnation : '',
              reason: typeof frame.reason === 'string' ? frame.reason : '',
              watermark: Number.isFinite(frame.watermark) ? frame.watermark : null,
              cursor: Number.isFinite(frame.cursor) ? frame.cursor : (Number.isFinite(data.cursor) ? data.cursor : null),
              messages: Array.isArray(frame.messages) ? frame.messages.length : (Array.isArray(data.messages) ? data.messages.length : null),
              success: frame.success === true ? true : (frame.success === false ? false : null),
              processId: frame.process && typeof frame.process.id === 'string' ? frame.process.id : '',
              processState: frame.process && typeof frame.process.state === 'string' ? frame.process.state : '',
              processLabel: frame.process && typeof frame.process.label === 'string' ? frame.process.label : '',
            });
            if (control.frames.length > 65536) control.frames.shift();
          });
        }
        send(data) {
          if (typeof data === 'string') {
            try {
              const frame = JSON.parse(data);
              if (frame && typeof frame.type === 'string') {
                control.frames.push({
                  dir: 'out',
                  seq: control.nextSeq++,
                  t: Date.now(),
                  type: frame.type,
                  id: typeof frame.id === 'string' ? frame.id : '',
                  sessionId: typeof frame.sessionId === 'string' ? frame.sessionId : '',
                  catalogSessionId: typeof frame.catalogSessionId === 'string' ? frame.catalogSessionId : '',
                  cursor: Number.isFinite(frame.cursor) ? frame.cursor : null,
                  runtimeIncarnation: typeof frame.runtimeIncarnation === 'string' ? frame.runtimeIncarnation : '',
                });
                if (control.frames.length > 65536) control.frames.shift();
              }
            } catch {
              // Non-JSON frames pass through unrecorded.
            }
          }
          return super.send(data);
        }
      }
      for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
        Object.defineProperty(RmTestWebSocket, key, { value: NativeWebSocket[key] });
      }
      window.WebSocket = RmTestWebSocket;
      window.__rmWireTest = control;
    });

    const rpcSequence = { n: 0 };
    const rpc = async (sessionId, command) => {
      const requestId = `rm-rpc-${++rpcSequence.n}`;
      return page.evaluate(
        ({ wsUrl: target, authToken, command: request, sessionId: owner, id: requestId }) =>
          new Promise((resolve, reject) => {
            const protocols = authToken ? [`rpi-auth.${authToken}`] : [];
            const ws = new WebSocket(target, protocols);
            const timer = window.setTimeout(() => {
              ws.close();
              reject(new Error(`rpc timed out: ${request.type}`));
            }, 30000);
            ws.addEventListener('open', () => ws.send(JSON.stringify({ ...request, sessionId: owner, id: requestId })));
            ws.addEventListener('message', (event) => {
              let frame;
              try { frame = JSON.parse(String(event.data)); } catch { return; }
              if (frame?.type !== 'response' || frame.id !== requestId) return;
              window.clearTimeout(timer);
              ws.close();
              if (frame.sessionId !== owner) {
                reject(new Error(`rpc owner mismatch: expected ${owner}, got ${JSON.stringify(frame.sessionId)}`));
                return;
              }
              if (frame.success !== true) {
                reject(new Error(frame.error || `rpc failed: ${request.type}`));
                return;
              }
              resolve(frame.data || {});
            });
            ws.addEventListener('error', () => {
              window.clearTimeout(timer);
              reject(new Error(`rpc transport failed: ${request.type}`));
            });
          }),
        { wsUrl, authToken: token, command, sessionId, id: requestId }
      );
    };
    const snapshotProbe = async (sessionId, incarnation) => {
      const requestId = `rm-sub-${++rpcSequence.n}`;
      return page.evaluate(
        ({ wsUrl: target, authToken, sessionId: owner, incarnation: expected, id: requestId }) =>
          new Promise((resolve, reject) => {
            const protocols = authToken ? [`rpi-auth.${authToken}`] : [];
            const ws = new WebSocket(target, protocols);
            const timer = window.setTimeout(() => {
              ws.close();
              reject(new Error('snapshot probe timed out'));
            }, 30000);
            ws.addEventListener('open', () => ws.send(JSON.stringify({
              type: 'subscribe', sessionId: owner, runtimeIncarnation: expected, cursor: 0, id: requestId,
            })));
            ws.addEventListener('message', (event) => {
              let frame;
              try { frame = JSON.parse(String(event.data)); } catch { return; }
              if (frame?.type !== 'subscription_snapshot' || frame.sessionId !== owner) return;
              if (frame.runtimeIncarnation !== expected) return;
              window.clearTimeout(timer);
              ws.close();
              resolve({
                watermark: frame.watermark,
                reason: frame.reason,
                runtimeIncarnation: frame.runtimeIncarnation,
                messageCount: Array.isArray(frame.messages) ? frame.messages.length : -1,
              });
            });
            ws.addEventListener('error', () => {
              window.clearTimeout(timer);
              reject(new Error('snapshot probe transport failed'));
            });
          }),
        { wsUrl, authToken: token, sessionId, incarnation, id: requestId }
      );
    };
    const outgoingFrames = () => page.evaluate(() => (window.__rmWireTest?.frames || []).filter((f) => f.dir === 'out'));
    const allFrames = () => page.evaluate(() => window.__rmWireTest?.frames || []);
    const nextFrameSeq = async () => page.evaluate(() => window.__rmWireTest?.nextSeq || 0);
    const framesSince = async (seq) => (await allFrames()).filter((f) => typeof f.seq === 'number' && f.seq >= seq);
    const measureExactReadySwitch = async (target, tail, id) => {
      await page.evaluate(({ sid, tailText }) => {
        window.__rmSwitchTiming = null;
        const timing = { clickAt: null, firstMutationMs: null, activeRowMs: null, readyMs: null, tailMs: null, done: false };
        window.__rmSwitchTiming = timing;
        const onPointerDown = (event) => {
          const btn = event.target && event.target.closest ? event.target.closest('.session-sidebar__switch') : null;
          if (!btn || btn.dataset.sessionId !== sid || timing.clickAt !== null) return;
          timing.clickAt = performance.now();
        };
        document.addEventListener('pointerdown', onPointerDown, { capture: true });
        const observer = new MutationObserver(() => {
          if (timing.clickAt !== null && timing.firstMutationMs === null) {
            timing.firstMutationMs = performance.now() - timing.clickAt;
            observer.disconnect();
          }
        });
        observer.observe(document.body, { childList: true, subtree: true, attributes: true });
        const poll = () => {
          if (timing.done) return;
          if (timing.clickAt === null) {
            requestAnimationFrame(poll);
            return;
          }
          const now = performance.now();
          const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch');
          const lifecycle = document.getElementById('session-lifecycle');
          if (timing.activeRowMs === null && active?.dataset.sessionId === sid) timing.activeRowMs = now - timing.clickAt;
          if (timing.readyMs === null && lifecycle?.dataset.displayedSessionId === sid
            && lifecycle.dataset.authoritativeReady === 'true' && lifecycle.dataset.pendingGeneration === '') {
            timing.readyMs = performance.now() - timing.clickAt;
          }
          if (timing.tailMs === null && (document.getElementById('transcript')?.textContent || '').includes(tailText)) {
            timing.tailMs = performance.now() - timing.clickAt;
          }
          if (timing.activeRowMs !== null && timing.readyMs !== null && timing.tailMs !== null) {
            timing.done = true;
            document.removeEventListener('pointerdown', onPointerDown, { capture: true });
            return;
          }
          requestAnimationFrame(poll);
        };
        requestAnimationFrame(poll);
      }, { sid: target, tailText: tail });
      const wireBase = await nextFrameSeq();
      const row = page.locator(`.session-sidebar__switch[data-session-id="${target}"]`);
      await row.scrollIntoViewIfNeeded();
      await row.click();
      try {
        await page.waitForFunction(() => window.__rmSwitchTiming?.done === true, undefined, { timeout: 15000 });
      } catch {
        const dump = await page.evaluate((sid) => {
          const el = document.getElementById('session-lifecycle');
          return {
            timing: window.__rmSwitchTiming,
            active: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
            displayed: el?.dataset.displayedSessionId || '',
            ready: el?.dataset.authoritativeReady || '',
            pending: el?.dataset.pendingGeneration || '',
            transcriptHasTarget: (document.getElementById('transcript')?.textContent || '').includes(sid),
          };
        }, target);
        fail(`${id}: exact-ready switch milestones never settled for ${target}: ${JSON.stringify(dump)}`);
      }
      const timing = await page.evaluate(() => ({ ...window.__rmSwitchTiming }));
      const wire = await framesSince(wireBase);
      const switchFrames = wire.filter((f) => f.dir === 'out' && f.type === 'switch_session' && f.catalogSessionId === target).length;
      const subscribeFrames = wire.filter((f) => f.dir === 'out' && f.type === 'subscribe' && f.sessionId === target).length;
      const tailHit = await page.evaluate((t) => (document.getElementById('transcript')?.textContent || '').includes(t), tail);
      if (switchFrames !== 0 || subscribeFrames !== 0) {
        fail(`${id}: exact-ready switch must be zero-wire: ${JSON.stringify({ switchFrames, subscribeFrames })}`);
      }
      if (!tailHit) fail(`${id}: target transcript tail missing after the exact-ready cutover`);
      const durations = { activeRowMs: timing.activeRowMs, readyMs: timing.readyMs, tailMs: timing.tailMs };
      for (const [milestone, ms] of Object.entries(durations)) {
        if (typeof ms !== 'number' || ms < 0) fail(`${id}: missing ${milestone} timing: ${JSON.stringify(timing)}`);
        if (ms >= MAX_SWITCH_MS) fail(`${id}: ${milestone} took ${ms}ms, hard gate is <${MAX_SWITCH_MS}ms`);
      }
      if (typeof timing.firstMutationMs !== 'number' || timing.firstMutationMs > 50) {
        fail(`${id}: cutover paint was neither synchronous nor next-animation-frame: ${timing.firstMutationMs}ms`);
      }
      await assertZeroErrorToasts(page, id);
      return { target, durations, firstMutationMs: timing.firstMutationMs, switchFrames, subscribeFrames };
    };
    const outgoingCount = async (predicate) => {
      const frames = await outgoingFrames();
      return frames.filter(predicate).length;
    };
    const mockControl = async (controlPath) => {
      const response = await fetch(`http://127.0.0.1:${mockPort}${controlPath}`, { method: 'POST' });
      if (!response.ok) fail(`mock control ${controlPath} failed with ${response.status}`);
      return {};
    };
    const catalogRows = async (viaSessionId) => {
      const data = await rpc(viaSessionId, { type: 'session_list', scope: 'all_projects' });
      if (!Array.isArray(data.sessions)) fail(`session_list returned no sessions array: ${JSON.stringify(Object.keys(data))}`);
      return data.sessions;
    };

    markStage('rm0Boot');
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'RM0.1: page title missing');
    await waitFor(page, () => document.querySelector('.session-sidebar__row--active .session-sidebar__switch') !== null,
      'RM0.1: active primary session row missing');
    try {
      await page.waitForFunction(
        (modelTwo) => {
          const opts = Array.from(document.getElementById('model-select')?.options || []).map((o) => o.value);
          const levels = Array.from(document.getElementById('thinking-select')?.options || []).map((o) => o.value);
          return opts.includes('user-steering/mock') && opts.includes(modelTwo) && levels.includes('high');
        },
        MODEL_TWO,
        { timeout: 30000 }
      );
    } catch {
      const dump = await page.evaluate(() => ({
        models: Array.from(document.getElementById('model-select')?.options || []).map((o) => o.value),
        levels: Array.from(document.getElementById('thinking-select')?.options || []).map((o) => o.value),
        send: document.getElementById('send-btn')?.getAttribute('aria-label') || '',
        sendDisabled: document.getElementById('send-btn')?.hasAttribute('disabled') === true,
      }));
      fail(`RM0.1: fixture must expose both models and the high thinking level: ${JSON.stringify(dump)}`);
    }
    const primaryId = await activeSessionId(page);
    if (!primaryId) fail('RM0.1: primary session id missing');
    record('RM0.1');
    const before = {
      snapshot: await captureResourceSnapshot({ page: null, pid: listenerPid }),
      fdSocketCount: await connectionFdSocketCount(listenerPid),
      tcp: await tcpConnectionCount(listenerPid),
      listenerEstablished: await listenerEstablishedConnectionCount(listenerPid, listenerPort),
      rss: await processTreeRssSnapshot(listenerPid),
    };
    const baselineConnectionSignal = before.snapshot.fdSocketCount > 0
      ? before.snapshot.fdSocketCount
      : before.listenerEstablished;
    if (!(before.snapshot.rssBytes > 0) || !(baselineConnectionSignal > 0) || before.tcp === null) {
      fail(`RM0.2: baseline resource snapshot not measurable: ${JSON.stringify(before)}`);
    }
    record('RM0.2');

    markStage('rm1Fill');
    const knownIds = [primaryId];
    const fill = [];
    const bootLoaded = await loadedIds(page);
    if (bootLoaded.length !== 1 || !bootLoaded.includes(primaryId)) {
      fail(`RM1.1: fixture must boot with only the primary loaded: ${JSON.stringify(bootLoaded)}`);
    }
    for (let i = 1; i <= MAX_LOADED_RUNTIMES - 1; i++) {
      const created = await createFreshSessionViaManage(page, waitFor, `RM1 fill ${i}`, { closePanel: true });
      await waitForEmptyView(page, `RM1 fill ${i}: new session did not show the empty view`);
      await promptAndWait(page, FILL_PROMPT(i), FILL_REPLY(i), `RM1 fill ${i}: echo never round-tripped`);
      const sid = await waitForNewRow(page, knownIds, `RM1 fill ${i}: catalog row never appeared`, 30000);
      knownIds.push(sid);
      if (created.sessionId !== sid) {
        fail(`RM1 fill ${i}: manage identity ${created.sessionId} != catalog row ${sid}`);
      }
      fill.push(sid);
    }
    const loadedAtCap = await loadedIds(page);
    if (loadedAtCap.length !== MAX_LOADED_RUNTIMES || !loadedAtCap.includes(primaryId)) {
      fail(`RM1.1: pool did not reach ${MAX_LOADED_RUNTIMES} with the primary resident: ${JSON.stringify(loadedAtCap)}`);
    }
    record('RM1.1');
    await assertScreenshot(page, path.join(evidence, 'runtime-matrix-rm1-cap.png'), 'RM1.1');
    const pStream = fill[0];
    const pProcess = fill[1];
    const pGoal = fill[2];
    const pTodo = fill[3];
    const victim = fill[4];
    const reopenRetiree = fill[5];
    const exactA = fill[fill.length - 2];
    const exactB = fill[fill.length - 1];
    const protectedIds = [pStream, pProcess, pGoal, pTodo];
    if (!exactA || !exactB || exactA === victim || exactB === victim || exactA === exactB) {
      fail(`RM1.1: warm-control ids missing after fill: ${JSON.stringify({ fill: fill.length, exactA, exactB, victim })}`);
    }

    markStage('rm2Protection');
    await clickRow(page, pStream, 'RM2.1: switch to the streaming-protected session');
    await page.fill('#prompt-input', STREAM_PROMPT);
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (delta) => (document.getElementById('transcript')?.textContent || '').includes(delta),
      'RM2.1: held stream first delta never landed',
      30000,
      STREAM_FIRST_DELTA,
    );
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Stop generating',
      'RM2.1: held stream did not pin the Stop action (is_streaming)'
    );
    record('RM2.1');
    await clickRow(page, pProcess, 'RM2.2: switch to the process-protected session');
    const processState = sessionState(await rpc(pProcess, { type: 'get_state' }));
    const processCwd = processState?.cwd;
    if (typeof processCwd !== 'string' || !processCwd.startsWith('/')) {
      fail(`RM2.2: process session cwd is not an absolute directory: ${JSON.stringify(processCwd)}`);
    }
    const spawned = await rpc(pProcess, {
      type: 'process_spawn',
      spec: { argv: ['sleep', '3600'], cwd: processCwd, label: PPROCESS_LABEL },
    });
    if (typeof spawned.id !== 'string' || spawned.id === '' || spawned.state !== 'running') {
      fail(`RM2.2: process_spawn did not produce a real running process: ${JSON.stringify(spawned)}`);
    }
    const procRows = await rpc(pProcess, { type: 'process_list' });
    const liveProc = Array.isArray(procRows) ? procRows.find((row) => row.id === spawned.id) : null;
    if (!liveProc || liveProc.state !== 'running') {
      fail(`RM2.2: backend process_list does not report the active protected process: ${JSON.stringify({ procRows, spawned })}`);
    }
    const startedDeadline = Date.now() + 10000;
    let startedEvents = [];
    while (Date.now() < startedDeadline) {
      startedEvents = (await allFrames()).filter((f) => f.dir === 'in'
        && f.type === 'process_started' && f.sessionId === pProcess);
      if (startedEvents.length === 1) break;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    if (startedEvents.length !== 1) fail(`RM2.2: expected exactly one process_started event, got ${JSON.stringify(startedEvents)}`);
    if (startedEvents[0].processId !== spawned.id || startedEvents[0].processState !== 'running'
      || startedEvents[0].processLabel !== PPROCESS_LABEL) {
      fail(`RM2.2: process_started identity does not match the spawn response: ${JSON.stringify({ event: startedEvents[0], spawned })}`);
    }
    record('RM2.2');
    await clickRow(page, pGoal, 'RM2.3: switch to the goal-protected session');
    await page.click('#goal-panel-btn');
    await waitFor(page, () => document.getElementById('goal-panel') !== null, 'RM2.3: goal panel did not open');
    await waitFor(
      page,
      () => document.getElementById('goal-panel')?.dataset.hasGoal === 'false'
        && document.getElementById('goal-create-form') !== null
        && document.getElementById('goal-objective-input') !== null,
      'RM2.3: goal create form never mounted'
    );
    await page.fill('#goal-objective-input', PGOAL_OBJECTIVE);
    await waitFor(
      page,
      () => document.getElementById('goal-create-btn')?.disabled === false,
      'RM2.3: goal create button stayed disabled'
    );
    await page.click('#goal-create-btn');
    await waitFor(
      page,
      (objective) => document.getElementById('goal-panel')?.dataset.hasGoal === 'true'
        && document.getElementById('goal-panel')?.dataset.lifecycle === 'active'
        && (document.getElementById('goal-objective')?.textContent || '').includes(objective),
      'RM2.3: active goal never created',
      30000,
      PGOAL_OBJECTIVE,
    );
    await page.click('#goal-close-btn');
    await waitFor(page, () => document.getElementById('goal-panel') === null, 'RM2.3: goal panel did not close');
    record('RM2.3');
    const todoSet = await rpc(pTodo, {
      type: 'set_todos',
      phases: [{ name: 'Plan', tasks: [{ id: 'task-rm-prog', content: PTODO_TASK, status: 'in_progress' }] }],
    });
    const todoSetTask = todoSet.phases?.[0]?.tasks?.find((task) => task.content === PTODO_TASK);
    if (!todoSetTask || todoSetTask.status !== 'in_progress') {
      fail(`RM2.4: in-progress todo did not install: ${JSON.stringify(todoSet)}`);
    }
    record('RM2.4');

    markStage('rm3VictimSetup');
    await clickRow(page, victim, 'RM3 setup: switch to the future eviction victim');
    await page.selectOption('#model-select', MODEL_TWO);
    await waitFor(
      page,
      (modelTwo) => document.getElementById('model-select')?.value === modelTwo,
      'RM3 setup: model switch never round-tripped',
      30000,
      MODEL_TWO,
    );
    await page.selectOption('#thinking-select', 'high');
    await waitFor(page, () => document.getElementById('thinking-select')?.value === 'high',
      'RM3 setup: thinking switch never round-tripped');
    const settingsDeadline = Date.now() + 15000;
    let victimSettings = sessionState(await rpc(victim, { type: 'get_state' }));
    while (Date.now() < settingsDeadline) {
      const modelKey = victimSettings.model
        ? `${victimSettings.model.provider}/${victimSettings.model.id}`
        : '';
      if (modelKey === MODEL_TWO && victimSettings.thinkingLevel === 'high') break;
      await new Promise((resolve) => setTimeout(resolve, 100));
      victimSettings = sessionState(await rpc(victim, { type: 'get_state' }));
    }
    const victimModelKey = victimSettings.model
      ? `${victimSettings.model.provider}/${victimSettings.model.id}`
      : '';
    if (victimModelKey !== MODEL_TWO) {
      fail(`RM3 setup: get_state model is ${JSON.stringify(victimSettings.model)}, expected ${MODEL_TWO}`);
    }
    if (victimSettings.thinkingLevel !== 'high') {
      fail(`RM3 setup: get_state thinkingLevel is ${JSON.stringify(victimSettings.thinkingLevel)}, expected high`);
    }
    const vTodo = await rpc(victim, {
      type: 'set_todos',
      phases: [{ name: 'Ship', tasks: [{ id: 'task-rm-v1', content: V_TODO_DONE, status: 'completed' }] }],
    });
    if (vTodo.phases?.[0]?.tasks?.find((task) => task.content === V_TODO_DONE)?.status !== 'completed') {
      fail(`RM3 setup: victim completed todo did not install: ${JSON.stringify(vTodo)}`);
    }
    await page.click('#goal-panel-btn');
    await waitFor(page, () => document.getElementById('goal-panel') !== null, 'RM3 setup: goal panel did not open on victim');
    await waitFor(
      page,
      () => document.getElementById('goal-panel')?.dataset.hasGoal === 'false'
        && document.getElementById('goal-create-form') !== null
        && document.getElementById('goal-objective-input') !== null,
      'RM3 setup: victim goal create form never mounted'
    );
    await page.fill('#goal-objective-input', V_GOAL_OBJECTIVE);
    await waitFor(
      page,
      () => document.getElementById('goal-create-btn')?.disabled === false,
      'RM3 setup: victim goal create button stayed disabled'
    );
    await page.click('#goal-create-btn');
    await waitFor(
      page,
      (objective) => document.getElementById('goal-panel')?.dataset.hasGoal === 'true'
        && document.getElementById('goal-panel')?.dataset.lifecycle === 'active'
        && (document.getElementById('goal-objective')?.textContent || '').includes(objective),
      'RM3 setup: victim goal never created',
      30000,
      V_GOAL_OBJECTIVE,
    );
    await page.click('#goal-complete-btn');
    await waitFor(
      page,
      () => document.getElementById('goal-panel')?.getAttribute('data-lifecycle') === 'completed',
      'RM3 setup: victim goal never reached the terminal completed lifecycle'
    );
    await page.click('#goal-close-btn');
    await waitFor(page, () => document.getElementById('goal-panel') === null, 'RM3 setup: victim goal panel did not close');
    await promptAndWait(page, V_ECHO_PROMPT, V_ECHO_REPLY, 'RM3 setup: victim echo never round-tripped');
    const pre = await rpc(victim, { type: 'get_state' });
    const preIncarnation = pre.runtimeIncarnation;
    const preState = sessionState(pre);
    if (typeof preIncarnation !== 'string' || preIncarnation === '') fail('RM3 setup: victim incarnation missing from get_state');
    if (preState.isStreaming !== false || preState.pendingMessageCount !== 0) {
      fail(`RM3 setup: victim must be idle-eligible before eviction: ${JSON.stringify({ isStreaming: preState.isStreaming, pending: preState.pendingMessageCount })}`);
    }
    const preCursor = await snapshotProbe(victim, preIncarnation);
    if (preCursor.reason !== 'cold-full' || preCursor.watermark === null || preCursor.messageCount !== preState.messageCount) {
      fail(`RM3 setup: pre-eviction cursor probe inconsistent: ${JSON.stringify({ preCursor, messageCount: preState.messageCount })}`);
    }
    const preTranscript = await activeTranscript(page);
    const preLifecycle = await page.evaluate(() => {
      const el = document.getElementById('session-lifecycle');
      return { stateMessages: el?.dataset.stateMessages || '', ready: el?.dataset.authoritativeReady || '' };
    });
    if (preLifecycle.ready !== 'true' || preLifecycle.stateMessages !== String(preState.messageCount)) {
      fail(`RM3 setup: authoritative lifecycle marker mismatch: ${JSON.stringify({ preLifecycle, messageCount: preState.messageCount })}`);
    }
    await assertScreenshot(page, path.join(evidence, 'runtime-matrix-rm3-pre-eviction.png'), 'RM3 setup');

    const idleStamp = fill.filter((sid) => !protectedIds.includes(sid));
    if (idleStamp[0] !== victim) {
      fail(`RM3 setup: victim must be the oldest idle fill row: ${JSON.stringify({ victim, idleStamp: idleStamp.slice(0, 8) })}`);
    }
    await clickRow(page, primaryId, 'RM3: restore primary display before the over-cap create');
    // get_state is housekeeping and must not touch IsolatedApplication LRU
    // recency; restamp every idle row with an explicit user set_model (same
    // model, no side effect) so the eviction order stays fill order.
    const stampModel = await rpc(victim, { type: 'get_state' }).then((state) => sessionState(state).model);
    if (!stampModel || typeof stampModel.provider !== 'string' || typeof stampModel.id !== 'string') {
      fail(`RM3 setup: victim get_state missing model for LRU restamp: ${JSON.stringify(stampModel)}`);
    }
    for (const sid of idleStamp) {
      await rpc(sid, { type: 'set_model', provider: stampModel.provider, modelId: stampModel.id });
    }
    await rpc(primaryId, { type: 'get_state' });
    const streamHold = sessionState(await rpc(pStream, { type: 'get_state' }));
    if (streamHold.isStreaming !== true) {
      fail(`RM3 setup: streaming-protected session is not in-flight: ${JSON.stringify({ isStreaming: streamHold.isStreaming })}`);
    }
    const procHold = await rpc(pProcess, { type: 'process_list' });
    if (!Array.isArray(procHold) || !procHold.some((row) => row && row.state === 'running')) {
      fail(`RM3 setup: process-protected session has no running process: ${JSON.stringify(procHold)}`);
    }
    const goalHold = sessionState(await rpc(pGoal, { type: 'get_state' }));
    if (!goalHold.goal?.current || goalHold.goal.current.lifecycle === 'completed' || goalHold.goal.current.lifecycle === 'dropped') {
      fail(`RM3 setup: goal-protected session is not non-terminal: ${JSON.stringify(goalHold.goal)}`);
    }
    const todoHold = sessionState(await rpc(pTodo, { type: 'get_state' }));
    const todoHoldTask = (todoHold.todoPhases || []).flatMap((phase) => phase.tasks || []).find((task) => task.content === PTODO_TASK);
    if (!todoHoldTask || todoHoldTask.status !== 'in_progress') {
      fail(`RM3 setup: todo-protected session lost the in-progress task: ${JSON.stringify(todoHold.todoPhases)}`);
    }

    markStage('rm3Eviction');
    const missBaseline = await nextFrameSeq();
    await page.context().setOffline(true);
    await page.evaluate(() => {
      for (const socket of [...(window.__rmWireTest?.live || [])]) {
        try { socket.close(); } catch { /* already closed */ }
      }
    });
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') === true,
      'RM3: page A did not disconnect before peer eviction',
      15000
    );
    const peer = createControlClient({
      url: wsUrl,
      token,
      WebSocketImpl: globalThis.WebSocket,
      callTimeoutMs: 30000,
    });
    let replacementId = '';
    try {
      await peer.ready;
      peer.setPhase('evict');
      const unloadAfter = peer.frames.length;
      const created = await peer.call({ type: 'new_session', sessionId: primaryId });
      const replacement = parseLifecycleIdentity(await requireRpcData(created, 'new_session:evict'));
      replacementId = replacement.sessionId;
      let victimUnload = null;
      try {
        victimUnload = await peer.waitFor(
          (frame) => frame.type === 'session_unloaded'
            && frame.sessionId === victim
            && frame.reason === 'lru'
            && frame.runtimeIncarnation === preIncarnation,
          'RM3.1: peer never observed victim session_unloaded reason=lru',
          30000,
          { afterIndex: unloadAfter }
        );
      } catch (error) {
        const unloads = peer.frames
          .slice(unloadAfter)
          .map((entry) => entry.frame)
          .filter((frame) => frame && frame.type === 'session_unloaded');
        fail(`RM3.1: peer never observed victim session_unloaded reason=lru (${error.message}); unloads=${JSON.stringify(unloads)}`);
      }
      if (!victimUnload) fail('RM3.1: peer victim unload missing');
      const listFrame = await peer.call({ type: 'session_list', scope: 'all_projects', sessionId: primaryId });
      const listed = loadedIdsFromList(await requireRpcData(listFrame, 'session_list:after-evict'));
      const loadedIdsAfter = listed.map((row) => row.sessionId);
      if (loadedIdsAfter.length !== MAX_LOADED_RUNTIMES) {
        fail(`RM3.1: peer loaded count ${loadedIdsAfter.length} !== ${MAX_LOADED_RUNTIMES}: ${JSON.stringify(loadedIdsAfter)}`);
      }
      if (loadedIdsAfter.includes(victim)) fail(`RM3.1: victim stayed loaded on peer: ${JSON.stringify(loadedIdsAfter)}`);
      if (!loadedIdsAfter.includes(exactA) || !loadedIdsAfter.includes(exactB) || !loadedIdsAfter.includes(primaryId)) {
        fail(`RM3.1: warm controls left the peer loaded pool: ${JSON.stringify({ loadedIdsAfter, exactA, exactB, primaryId })}`);
      }
      const missed = (await framesSince(missBaseline)).filter((f) => f.dir === 'in' && f.type === 'session_unloaded' && f.sessionId === victim);
      if (missed.length !== 0) fail(`RM3.1: disconnected page A observed the victim unload: ${JSON.stringify(missed)}`);
      if (await outgoingCount((f) => f.type === 'close_session') !== 0) fail('RM3.1: a close_session frame was dispatched (manual close forbidden)');
      await page.context().setOffline(false);
      await waitFor(
        page,
        () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
        'RM3: page A did not reconnect',
        30000
      );
    } finally {
      await peer.close().catch(() => {});
    }
    if (!replacementId) fail('RM3: over-cap create did not return a replacement id');
    knownIds.push(replacementId);
    const seventeenthId = await waitForNewRow(page, knownIds.filter((id) => id !== replacementId), 'RM3: replacement catalog row never appeared on page A', 30000);
    if (seventeenthId !== replacementId) {
      fail(`RM3: replacement identity mismatch ${JSON.stringify({ created: replacementId, row: seventeenthId })}`);
    }
    await waitFor(
      page,
      () => document.getElementById('session-lifecycle')?.dataset.authoritativeReady === 'true'
        && document.getElementById('session-lifecycle')?.dataset.pendingGeneration === '',
      'RM3: reconnect never reached authoritative-ready',
      30000
    );
    const loadedAfterEviction = await assertProtectedLoaded(page, [primaryId, ...protectedIds, exactA, exactB], 'RM3.1');
    if (loadedAfterEviction.includes(victim)) fail(`RM3.1: victim stayed loaded on page A after reconnect: ${JSON.stringify(loadedAfterEviction)}`);
    await assertZeroErrorToasts(page, 'RM3.1');
    record('RM3.1');
    const catalogAfterEviction = await catalogRows(seventeenthId);
    const victimRow = catalogAfterEviction.find((row) => row.sessionId === victim);
    if (!victimRow || victimRow.loaded === true || typeof victimRow.path !== 'string' || victimRow.path === '') {
      fail(`RM3.2: victim lost its durable catalog row: ${JSON.stringify(victimRow || null)}`);
    }
    if (victimRow.runtimeIncarnation) fail(`RM3.2: unloaded victim row still carries an incarnation hint: ${JSON.stringify(victimRow.runtimeIncarnation)}`);
    await clickRow(page, exactA, 'RM3.3: restamp exactA on the reconnect socket');
    await waitFor(
      page,
      (reply) => (document.getElementById('transcript')?.textContent || '').includes(reply),
      'RM3.3: exactA transcript tail missing after reconnect visit',
      30000,
      FILL_REPLY(fill.length - 1),
    );
    await clickRow(page, exactB, 'RM3.3: restamp exactB on the reconnect socket');
    await waitFor(
      page,
      (reply) => (document.getElementById('transcript')?.textContent || '').includes(reply),
      'RM3.3: exactB transcript tail missing after reconnect visit',
      30000,
      FILL_REPLY(fill.length),
    );
    const warmA = await measureExactReadySwitch(exactA, FILL_REPLY(fill.length - 1), 'RM3.3');
    const warmB = await measureExactReadySwitch(exactB, FILL_REPLY(fill.length), 'RM3.3');
    await assertScreenshot(page, path.join(evidence, 'runtime-matrix-rm3-evicted.png'), 'RM3.2');
    record('RM3.2');

    markStage('rm4Reopen');
    const reopenBase = await nextFrameSeq();
    await clickRow(page, victim, 'RM4.1: cold reopen of the LRU-retired victim');
    await waitFor(
      page,
      ({ sid, reply }) => {
        const el = document.getElementById('session-lifecycle');
        return el?.dataset.displayedSessionId === sid
          && el?.dataset.authoritativeReady === 'true'
          && el?.dataset.pendingGeneration === ''
          && document.body.textContent.includes(reply);
      },
      'RM4.1: reopened victim never reached authoritative-ready with its transcript',
      30000,
      { sid: victim, reply: V_ECHO_REPLY },
    );
    const reopenFrames = await framesSince(reopenBase);
    const reopenSwitches = reopenFrames.filter((f) => f.dir === 'out' && f.type === 'switch_session' && f.catalogSessionId === victim);
    const reopenSubscribes = reopenFrames.filter((f) => f.dir === 'out' && f.type === 'subscribe' && f.sessionId === victim);
    if (reopenSwitches.length !== 1 || reopenSubscribes.length !== 1) {
      fail(`RM4.1: cold reopen must dispatch exactly one switch_session + one subscribe: ${JSON.stringify({ switches: reopenSwitches.length, subscribes: reopenSubscribes.length })}`);
    }
    if (reopenSubscribes[0].cursor !== 0) fail(`RM4.1: cold reopen subscribe cursor must be 0: ${JSON.stringify(reopenSubscribes[0])}`);
    if (reopenSwitches[0].runtimeIncarnation) fail('RM4.1: cold reopen switch_session must not carry a stale incarnation');
    const reopenHandoff = reopenFrames.find((f) => f.dir === 'in' && f.type === 'response' && f.id === reopenSwitches[0].id && f.success === true);
    if (!reopenHandoff || reopenHandoff.cursor === null) {
      fail(`RM4.1: cold reopen switch response missing handoff cursor: ${JSON.stringify(reopenFrames.filter((f) => f.type === 'response'))}`);
    }
    const reopenCut = reopenFrames.find((f) => f.dir === 'in' && f.type === 'subscription_snapshot'
      && f.sessionId === victim && f.reason === 'cold-full');
    if (!reopenCut || reopenCut.runtimeIncarnation === '' || reopenCut.runtimeIncarnation === preIncarnation) {
      fail(`RM4.1: cold-full cut for the reopened victim missing or stale: ${JSON.stringify(reopenFrames.filter((f) => f.type === 'subscription_snapshot' && f.sessionId === victim))}`);
    }
    if (reopenCut.watermark === null || reopenCut.watermark < 0) {
      fail(`RM4.1: cold-full cut missing a protocol watermark: ${JSON.stringify(reopenCut)}`);
    }
    if (reopenHandoff.cursor !== reopenCut.watermark) {
      fail(`RM4.1: handoff cursor ${reopenHandoff.cursor} !== new-incarnation watermark ${reopenCut.watermark}`);
    }
    if (reopenCut.messages !== preState.messageCount) {
      fail(`RM4.1: cold-full cut carried ${reopenCut.messages} messages, expected ${preState.messageCount}`);
    }
    const loadedAfterReopen = await assertProtectedLoaded(page, [primaryId, ...protectedIds, victim, seventeenthId, exactA], 'RM4.1');
    if (loadedAfterReopen.includes(reopenRetiree) || !loadedAfterEviction.includes(reopenRetiree)) {
      fail(`RM4.1: reopen must retire exactly the next LRU idle ${reopenRetiree}: ${JSON.stringify({ loadedAfterReopen, loadedAfterEviction })}`);
    }
    const reopenRetireeUnload = (await allFrames()).filter((f) => f.dir === 'in' && f.type === 'session_unloaded' && f.sessionId === reopenRetiree);
    if (reopenRetireeUnload.length !== 1 || reopenRetireeUnload[0].reason !== 'lru') {
      fail(`RM4.1: reopen retiree unload evidence wrong: ${JSON.stringify(reopenRetireeUnload)}`);
    }
    await assertZeroErrorToasts(page, 'RM4.1');
    record('RM4.1');
    const reopenTranscript = await activeTranscript(page);
    if (reopenTranscript !== preTranscript) {
      fail(`RM4.2: reopened transcript is not byte-exact vs the pre-eviction transcript: preLen=${preTranscript.length} postLen=${reopenTranscript.length} preHas=${preTranscript.includes(V_ECHO_REPLY)} postHas=${reopenTranscript.includes(V_ECHO_REPLY)}`);
    }
    const post = await rpc(victim, { type: 'get_state' });
    const postState = sessionState(post);
    if (post.runtimeIncarnation === preIncarnation) fail('RM4.2: reopened runtime kept the retired incarnation');
    if (postState.messageCount !== preState.messageCount) {
      fail(`RM4.2: message count regressed: pre=${preState.messageCount} post=${postState.messageCount}`);
    }
    if (JSON.stringify(postState.model) !== JSON.stringify(preState.model)) {
      fail(`RM4.2: model identity not restored exactly: ${JSON.stringify({ pre: preState.model, post: postState.model })}`);
    }
    if (postState.thinkingLevel !== preState.thinkingLevel) {
      fail(`RM4.2: thinking level not restored exactly: ${JSON.stringify({ pre: preState.thinkingLevel, post: postState.thinkingLevel })}`);
    }
    await waitFor(
      page,
      (modelTwo) => document.getElementById('model-select')?.value === modelTwo,
      'RM4.2: reopened model selection was not restored',
      10000,
      MODEL_TWO,
    );
    await waitFor(
      page,
      (level) => document.getElementById('thinking-select')?.value === level,
      'RM4.2: reopened thinking level was not restored',
      10000,
      'high',
    );
    if (JSON.stringify(postState.todoPhases) !== JSON.stringify(preState.todoPhases)) {
      fail(`RM4.2: Todo phases not restored exactly: ${JSON.stringify({ pre: preState.todoPhases, post: postState.todoPhases })}`);
    }
    if (JSON.stringify(postState.goal) !== JSON.stringify(preState.goal)) {
      fail(`RM4.2: Goal state not restored exactly: ${JSON.stringify({ pre: preState.goal, post: postState.goal })}`);
    }
    await page.click('#todo-toggle-btn');
    await waitFor(page, () => document.getElementById('todo-panel') !== null, 'RM4.2: todo panel did not open on the reopened victim');
    if (!(await page.evaluate(() => document.getElementById('todo-panel')?.textContent || '')).includes(V_TODO_DONE)) {
      fail('RM4.2: restored Todo panel is missing the victim task');
    }
    await page.click('#todo-close-btn');
    await page.click('#goal-panel-btn');
    await waitFor(page, () => document.getElementById('goal-panel') !== null, 'RM4.2: goal panel did not reopen');
    const goalLifecycle = await page.evaluate(() => ({
      has: document.getElementById('goal-panel')?.getAttribute('data-has-goal'),
      lifecycle: document.getElementById('goal-panel')?.getAttribute('data-lifecycle'),
      objective: document.getElementById('goal-objective')?.textContent || '',
    }));
    if (goalLifecycle.has !== 'true' || goalLifecycle.lifecycle !== 'completed' || !goalLifecycle.objective.includes(V_GOAL_OBJECTIVE)) {
      fail(`RM4.2: restored Goal state wrong: ${JSON.stringify(goalLifecycle)}`);
    }
    await page.click('#goal-close-btn');
    await assertScreenshot(page, path.join(evidence, 'runtime-matrix-rm4-reopened.png'), 'RM4.2');
    record('RM4.2');
    const catalogAfterReopen = await catalogRows(seventeenthId);
    const reopenedRow = catalogAfterReopen.find((row) => row.sessionId === victim);
    if (!reopenedRow || reopenedRow.loaded !== true || reopenedRow.runtimeIncarnation !== post.runtimeIncarnation) {
      fail(`RM4.3: catalog row did not adopt the new incarnation: ${JSON.stringify(reopenedRow || null)}`);
    }
    const victimResyncs = (await allFrames()).filter((f) => f.dir === 'in' && f.type === 'session_resync' && f.sessionId === victim);
    if (victimResyncs.length !== 0) fail(`RM4.3: reopened victim required a resync (gap): ${JSON.stringify(victimResyncs)}`);
    record('RM4.3');

    markStage('rm5ExactReady');
    const legA = await measureExactReadySwitch(exactA, FILL_REPLY(fill.length - 1), 'RM5.1');
    const legB = await measureExactReadySwitch(exactB, FILL_REPLY(fill.length), 'RM5.2');
    record('RM5.1');
    record('RM5.2');

    markStage('rm6Release');
    await mockControl('/__release-session');
    const settleDeadline = Date.now() + 30000;
    let streamState = null;
    while (Date.now() < settleDeadline) {
      streamState = sessionState(await rpc(pStream, { type: 'get_state' }));
      if (streamState.isStreaming === false) break;
      await new Promise((resolve) => setTimeout(resolve, 250));
    }
    if (streamState?.isStreaming !== false) fail('RM6.1: held stream never settled after the release POST');
    const stopBaseline = await nextFrameSeq();
    const stopped = await rpc(pProcess, { type: 'process_stop', processId: spawned.id });
    if (stopped.id !== spawned.id || stopped.state !== 'exited') {
      fail(`RM6.1: process_stop response is not the exact terminal state: ${JSON.stringify({ stopped, spawned })}`);
    }
    const exitDeadline = Date.now() + 15000;
    let exitEvents = [];
    while (Date.now() < exitDeadline) {
      exitEvents = (await framesSince(stopBaseline)).filter((f) => f.dir === 'in'
        && f.type === 'process_exited' && f.sessionId === pProcess);
      if (exitEvents.length === 1 && exitEvents[0].processId === spawned.id && exitEvents[0].processState === 'exited') {
        break;
      }
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    if (exitEvents.length !== 1 || exitEvents[0].processId !== spawned.id || exitEvents[0].processState !== 'exited') {
      fail(`RM6.1: terminal process_exited event mismatch after stop: ${JSON.stringify(exitEvents)}`);
    }
    const pruneDeadline = Date.now() + 15000;
    let procPruned = false;
    while (Date.now() < pruneDeadline) {
      const rows = await rpc(pProcess, { type: 'process_list' });
      procPruned = Array.isArray(rows) && !rows.some((row) => row.id === spawned.id);
      if (procPruned) break;
      await new Promise((resolve) => setTimeout(resolve, 250));
    }
    if (!procPruned) fail(`RM6.1: stopped process ${spawned.id} still listed after process_stop (prune failed)`);
    const loadedAfterRelease = await assertProtectedLoaded(page, [primaryId, ...protectedIds, victim, seventeenthId], 'RM6.1');
    await assertZeroErrorToasts(page, 'RM6.1');
    // get_state is housekeeping and must not touch IsolatedApplication LRU
    // recency. The pressure create must retire the just-stopped process
    // session as the EXACT oldest idle: it stopped at 1010 while every other
    // idle row was restamped after it, so keep that invariant explicit with
    // an ordered user set_model pass over the idle rows (same model, no side
    // effect).
    const releaseModel = sessionState(await rpc(pProcess, { type: 'get_state' })).model;
    if (!releaseModel || typeof releaseModel.provider !== 'string' || typeof releaseModel.id !== 'string') {
      fail(`RM6.1: process session get_state missing model for IsolatedApplication LRU restamp: ${JSON.stringify(releaseModel)}`);
    }
    const restampLoaded = await loadedIds(page);
    if (!restampLoaded.includes(pProcess)) {
      fail(`RM6.1: process session left the loaded pool before the recency restamp: ${JSON.stringify({ pProcess, restampLoaded })}`);
    }
    for (const sid of [pProcess, pStream, victim, seventeenthId, ...fill.slice(5).filter((sid) => sid !== pProcess)]) {
      if (!restampLoaded.includes(sid)) continue;
      await rpc(sid, { type: 'set_model', provider: releaseModel.provider, modelId: releaseModel.id });
    }
    const postStop = await createFreshSessionViaManage(page, waitFor, 'RM6: post-stop pressure create', { closePanel: true });
    await waitForEmptyView(page, 'RM6: post-stop session did not show the empty view');
    await promptAndWait(page, 'rm-post-stop', 'sessions-reply: rm-post-stop', 'RM6: post-stop echo never round-tripped');
    const postStopId = await waitForNewRow(page, knownIds, 'RM6: post-stop catalog row never appeared', 30000);
    knownIds.push(postStopId);
    if (postStop.sessionId !== postStopId) fail(`RM6: post-stop identity mismatch ${JSON.stringify(postStop)}`);
    const allUnloads = (await allFrames()).filter((f) => f.dir === 'in' && f.type === 'session_unloaded');
    const pressureUnloads = allUnloads.filter((f) => f.sessionId === pProcess);
    if (pressureUnloads.length !== 1 || pressureUnloads[0].reason !== 'lru') {
      fail(`RM6.1: post-stop pressure must retire the stopped process session as exact LRU: pProcess=${JSON.stringify(pressureUnloads)} all=${JSON.stringify(allUnloads.slice(-4))} map=${JSON.stringify({ pProcess, victim, seventeenthId, fillHead: fill.slice(0, 7) })}`);
    }
    const loadedAfterPressure = await assertProtectedLoaded(
      page,
      [primaryId, pStream, pGoal, pTodo, victim, seventeenthId, postStopId, exactA],
      'RM6.1'
    );
    if (loadedAfterPressure.includes(pProcess) || !loadedAfterRelease.includes(pProcess)) {
      fail(`RM6.1: post-stop pool membership wrong for the retired process session: ${JSON.stringify({ loadedAfterPressure, loadedAfterRelease })}`);
    }
    record('RM6.1');
    markStage('rm6After');
    const after = {
      snapshot: await captureResourceSnapshot({ page: null, pid: listenerPid }),
      fdSocketCount: await connectionFdSocketCount(listenerPid),
      tcp: await tcpConnectionCount(listenerPid),
      listenerEstablished: await listenerEstablishedConnectionCount(listenerPid, listenerPort),
      rss: await processTreeRssSnapshot(listenerPid),
    };
    const afterConnectionSignal = after.fdSocketCount !== null ? after.fdSocketCount : after.listenerEstablished;
    if (after.snapshot.rssBytes === null || afterConnectionSignal === null) {
      fail(`RM6.2: after-journey resource snapshot not measurable: ${JSON.stringify(after)}`);
    }
    const growth = {
      rssBytes: after.snapshot.rssBytes - before.snapshot.rssBytes,
      fdSocketCount: after.snapshot.fdSocketCount === null || before.snapshot.fdSocketCount === null
        ? null
        : after.snapshot.fdSocketCount - before.snapshot.fdSocketCount,
      listenerEstablished: after.listenerEstablished === null || before.listenerEstablished === null
        ? null
        : after.listenerEstablished - before.listenerEstablished,
    };
    if (growth.fdSocketCount !== null && growth.fdSocketCount > MAX_FD_SOCKET_GROWTH) {
      fail(`RM6.2: fd/socket growth ${growth.fdSocketCount} exceeds the bound ${MAX_FD_SOCKET_GROWTH}`);
    }
    if (growth.listenerEstablished !== null && after.listenerEstablished > MAX_LISTENER_ESTABLISHED) {
      fail(`RM6.2: established listener connections ${after.listenerEstablished} exceed the bound ${MAX_LISTENER_ESTABLISHED}`);
    }
    const maxRssGrowthBytes = Math.max(RSS_GROWTH_FLOOR_BYTES, Math.round(before.snapshot.rssBytes / 2));
    if (growth.rssBytes > maxRssGrowthBytes) {
      fail(`RM6.2: RSS growth ${growth.rssBytes} exceeds the workload bound ${maxRssGrowthBytes} (leak)`);
    }
    const plateau = [];
    for (let i = 0; i < RSS_PLATEAU_SAMPLES; i++) {
      const sample = await processTreeRssSnapshot(listenerPid);
      plateau.push({ tMs: sample?.sampledAtMs ?? Date.now(), rssBytes: sample?.rssBytes ?? null });
      await new Promise((resolve) => setTimeout(resolve, RSS_PLATEAU_CADENCE_MS));
    }
    const plateauRss = plateau.filter((sample) => Number.isFinite(sample.rssBytes));
    if (plateauRss.length !== RSS_PLATEAU_SAMPLES) fail(`RM6.2: RSS plateau sampling incomplete: ${JSON.stringify(plateau)}`);
    const plateauSpread = Math.max(...plateauRss.map((s) => s.rssBytes)) - Math.min(...plateauRss.map((s) => s.rssBytes));
    if (plateauSpread > RSS_PLATEAU_SPREAD_BYTES) {
      fail(`RM6.2: RSS did not plateau after settle (spread ${plateauSpread} bytes — active leak)`);
    }
    const n = plateauRss.length;
    const meanT = plateauRss.reduce((sum, s) => sum + s.tMs, 0) / n;
    const meanRss = plateauRss.reduce((sum, s) => sum + s.rssBytes, 0) / n;
    let cov = 0;
    let varT = 0;
    for (const s of plateauRss) {
      cov += (s.tMs - meanT) * (s.rssBytes - meanRss);
      varT += (s.tMs - meanT) ** 2;
    }
    const plateauSlopeBytesPerMs = varT === 0 ? 0 : cov / varT;
    if (Math.abs(plateauSlopeBytesPerMs) > RSS_PLATEAU_MAX_SLOPE_BYTES_PER_MS) {
      fail(`RM6.2: RSS slope ${plateauSlopeBytesPerMs} bytes/ms exceeds ${RSS_PLATEAU_MAX_SLOPE_BYTES_PER_MS} (active leak)`);
    }
    record('RM6.2');
    const unexpected = unexpectedBrowserErrors(browserErrors);
    if (unexpected.length !== 0) fail(`RM6.3: browser errors recorded: ${JSON.stringify(unexpected)}`);
    await assertScreenshot(page, path.join(evidence, 'runtime-matrix-rm6-final.png'), 'RM6.3');
    await writeEvidenceJson(metricsPath, {
      lane: 'web-runtime-matrix',
      bounds: {
        maxFdSocketGrowth: MAX_FD_SOCKET_GROWTH,
        maxListenerEstablished: MAX_LISTENER_ESTABLISHED,
        maxRssGrowthBytes,
        rssPlateauSamples: RSS_PLATEAU_SAMPLES,
        rssPlateauCadenceMs: RSS_PLATEAU_CADENCE_MS,
        rssPlateauSpreadBytes: RSS_PLATEAU_SPREAD_BYTES,
        rssPlateauMaxSlopeBytesPerMs: RSS_PLATEAU_MAX_SLOPE_BYTES_PER_MS,
        maxSwitchMs: MAX_SWITCH_MS,
      },
      plateauSlopeBytesPerMs,
      plateauSamples: plateau,
      before,
      after,
      growth,
      switchTimings: { exactReadyA: legA, exactReadyB: legB },
      pool: { atCap: loadedAtCap, afterEviction: loadedAfterEviction, afterReopen: loadedAfterReopen, afterRelease: loadedAfterRelease },
      stages: stageMarks,
    });
    if (!fs.existsSync(metricsPath) || fs.statSync(metricsPath).size === 0) fail('RM6.3: metrics evidence JSON did not land on disk');
    record('RM6.3');

    const executedIds = [...executed.keys()];
    if (executedIds.length !== DOCUMENTED_IDS.length
      || executedIds.some((id, index) => id !== DOCUMENTED_IDS[index])
      || executedIds.some((id) => executed.get(id) !== 1)) {
      fail(`coverage ledger drift: documented=${JSON.stringify(DOCUMENTED_IDS)} executed=${JSON.stringify(executedIds)}`);
    }
    fs.writeFileSync(coveragePath, JSON.stringify({
      documented: DOCUMENTED_IDS,
      executed: executedIds,
      counts: Object.fromEntries(executedIds.map((id) => [id, executed.get(id)])),
    }, null, 2));
    console.log(`runtime-matrix: PASSED (${executedIds.length}/${DOCUMENTED_IDS.length} contracts, pool 16 -> evict LRU -> cold reopen -> zero-wire exact-ready, resources bounded)`);
  } finally {
    await browser.close();
  }
}

main().catch((err) => {
  console.error(err instanceof Error ? err.message : String(err));
  process.exit(2);
});