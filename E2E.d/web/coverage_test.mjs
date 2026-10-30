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
import { assertSidebarNewActionAbsent, createFreshSessionViaManage } from './lib/session-lifecycle.mjs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const wrongToken = process.env.RPI_WRONG_TOKEN || 'wrong-token-abc';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const work = process.env.RPI_WORK || evidence;
const fastReply = process.env.RPI_FAST_REPLY || 'steering-followup-reply';
const slowTail = process.env.RPI_SLOW_TAIL || 'chunk-four-done';
const abortedTail = '-done';

const executed = new Map(); // id -> exact execution count (per-ID ledger, no Set erasure)
// The documented contract this driver must execute EXACTLY once — IN THE
// ACTUAL ASSERTION-SITE (runtime) ORDER. The coverage matrix requires the
// published ledger's executed array to equal the documented array IN ORDER,
// so the documented list tracks the real site order (assertId/record/
// wireTitleJourney call sites) instead of a hand-grouped narrative order.
const DOCUMENTED_IDS = [
  'auth.no-token-probe',
  'auth.wrong-token-toast',
  'auth.good-token-connect',
  'app.primary-action-send',
  'prompt.slow-stream-full',
  'prompt.fast-roundtrip',
  'app.primary-action-stop',
  'abort.early-preserves-text',
  'abort.no-tail-render',
  'abort.neutral-toast',
  'abort.no-error-toast',
  'prompt.new-message-count',
  'md.table-rendered',
  'md.task-list-glyph',
  'mermaid.svg-rendered',
  'katex.rendered',
  'md.no-fence-leak',
  'md.no-separator-leak',
  'md.fence-rendered',
  'md.fence-copy',
  'md.link-safe',
  'md.link-blocked',
  'md.image-safe',
  'md.image-blocked',
  'md.extra-blockquote',
  'md.extra-hr',
  'md.extra-ol',
  'md.extra-nested-list',
  'md.extra-fence-langs',
  'md.extra-mermaid-empty',
  'md.extra-mermaid-error',
  'md.extra-mermaid-no-residue',
  'md.extra-currency',
  'md.extra-image-relative',
  'md.extra-url-policy',
  'app.tool-card',
  'app.tool-card-done',
  'app.tool-card-settled',
  'app.tool-card-web-search',
  'app.tool-card-edit',
  'realtime.overlay-visible',
  'realtime.proxy-alpha-header',
  'realtime.session-update-transcription',
  'realtime.transcript-delta',
  'realtime.transcript-segment',
  'realtime.transcript-commit',
  'realtime.composer-final-screenshot',
  'realtime.transcript-dedup',
  'realtime.transcription-failed-toast',
  'realtime.error-toast',
  'realtime.delegation',
  'realtime.stop',
  'reconnect.pill',
  'reconnect.auto-on',
  'reconnect.transcript-survives',
  'app.tool-card-reconnect',
  'reconnect.roundtrip',
  'switch.model-roundtrip',
  'switch.thinking-roundtrip',
  'todo.create-row',
  'todo.complete-status',
  'todo.counts-done',
  'todo.reopen-status',
  'todo.detail-pane',
  'todo.add-via-enter',
  'todo.dep-link',
  'todo.dep-unlink',
  'todo.detail-complete',
  'todo.detail-reopen',
  'todo.clear-selection',
  'todo.all-rows-counts-converged',
  'goal.empty-state',
  'goal.create',
  'goal.budget-usage',
  'goal.pin',
  'concurrency.second-ws-rpc',
  'concurrency.live-event-reflect',
  'goal.pause-live',
  'goal.resume',
  'goal.journal-order',
  'workflow.create-row',
  'workflow.live-status',
  'workflow.select-row',
  'workflow.pause',
  'workflow.resume',
  'workflow.cancel-status',
  'workflow.remove',
  'workflow.create-via-enter',
  'workflow.integrate',
  'settings.browse-category',
  'settings.secret-redacted-readonly',
  'settings.draft-dirty',
  'settings.apply-persisted',
  'settings.refresh',
  'settings.typed-boolean',
  'settings.typed-enum',
  'settings.typed-number',
  'settings.typed-json',
  'settings.reset',
  'settings.cancel-draft',
  'settings.close',
  'slash.snapcompact',
  'slash.compact-llm',
  'slash.compact-bare',
  'slash.snapcompact-tail',
  'slash.skill-usage-error',
  'slash.skill-rpc',
  'slash.code-review-open',
  'slash.code-review-close',
  'slash.code-review-range',
  'slash.code-review-base',
  'slash.code-review-usage-error',
  'slash.unknown-falls-through',
  'session.sidebar-list',
  'session.rename-panel-sidebar',
  'nav.header-no-duplicate-session',
  'session.history-listed',
  'session.switch-new-id',
  'session.transcript-cleared',
  'subagent.spawn-live',
  'subagent.activity-line',
  'subagent.hub-send',
  'subagent.output-view',
  'subagent.cancel',
  'subagent.spawn-via-enter',
  'subagent.message-via-enter',
  'sidechat.default-tab',
  'sidechat.new-tab',
  'sidechat.tab-activated',
  'sidechat.prompt-reply',
  'mobile.sidebar-toggle-visible',
  'mobile.drawer-opens',
  'mobile.viewport-no-hscroll',
  'mobile.composer-above-fold',
  'mobile.touch-targets-44',
  'mobile.thinking-hidden',
  'mobile.header-no-duplicate-session',
  'concurrency.second-page-connects',
  'nav.panel-open-close',
  'realtime.conn-state-connected',
  'realtime.session-update',
  'realtime.error-message-only',
  'realtime.error-code-only',
  'realtime.error-bare',
  'realtime.dc-error',
  'realtime.conn-disconnected',
  'realtime.conn-failed-teardown',
  'wire.bash-done',
  'wire.bash-empty',
  'wire.bash-error',
  'wire.bash-bound-tail',
  'wire.title-snake',
  'wire.title-acronym',
  'wire.title-kebab',
  'wire.tool-error-card',
  'wire.todo-card-running',
  'wire.todo-structured-view',
];
function record(id) {
  executed.set(id, (executed.get(id) || 0) + 1);
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

// Wait until the main session is user-visibly idle. Enter during an active
// stream is mapped to steer (not a new prompt), so sequential NEW prompts must
// only be issued after the unified composer action returns to Send.
async function waitForIdle(page, label, timeoutMs = 30000) {
  await waitFor(
    page,
    // Production streaming gate (App.tsx): activeStreaming flips the
    // #send-btn aria-label. Missing element fails the gate — never passes
    // on a torn render.
    () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message',
    label,
    timeoutMs
  );
}

// Read a small JSON evidence file written by the mock/fixture (e.g. the
// realtime create-call header record), returning null when absent/invalid.
async function readJsonIfExists(file) {
  try {
    const raw = await fs.promises.readFile(file, 'utf8');
    return JSON.parse(raw);
  } catch {
    return null;
  }
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

const LIVE_WORKFLOW_STATUSES = new Set(['queued', 'planning', 'running', 'paused', 'integrating']);
const DELIVERY_OUTCOMES = new Set(['queued', 'woken', 'revived', 'processed', 'failed']);

function requireWorkflowSnapshot(value, expectedName, label) {
  if (!value || typeof value !== 'object') fail(`${label}: backend returned no workflow snapshot`);
  if (typeof value.workflowId !== 'string' || value.workflowId === '') fail(`${label}: workflowId missing`);
  if (value.name !== expectedName) fail(`${label}: expected name ${expectedName}, got ${JSON.stringify(value.name)}`);
  if (!Number.isSafeInteger(value.generation) || value.generation < 1) fail(`${label}: generation must be a positive integer`);
  if (typeof value.worktree !== 'string' || value.worktree === '') fail(`${label}: projected worktree missing`);
  if (typeof value.branch !== 'string' || value.branch === '') fail(`${label}: projected branch missing`);
  if (value.worktree.startsWith('/') || value.worktree.startsWith('\\')) fail(`${label}: absolute worktree leaked`);
  if (!value.branch.includes(value.workflowId)) fail(`${label}: branch does not carry workflowId: ${value.branch}`);
  if (!LIVE_WORKFLOW_STATUSES.has(value.status)) fail(`${label}: workflow is not live: ${value.status}`);
  return value;
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

function requireTypedReceipt(value, workflow, recipient, expectedOutcome, label) {
  if (value?.workflowId !== workflow.workflowId || value?.generation !== workflow.generation || value?.recipient !== recipient) {
    fail(`${label}: receipt envelope identity mismatch: ${JSON.stringify(value)}`);
  }
  if (!Array.isArray(value.receipts) || value.receipts.length !== 1) fail(`${label}: expected one receipt`);
  const receipt = value.receipts[0];
  if (typeof receipt.to !== 'string' || !DELIVERY_OUTCOMES.has(receipt.outcome)) fail(`${label}: untyped receipt ${JSON.stringify(receipt)}`);
  if (typeof receipt.messageId !== 'string' || receipt.messageId === '') fail(`${label}: receipt messageId missing`);
  if (expectedOutcome === 'delivered' && receipt.outcome === 'failed') fail(`${label}: delivery failed ${JSON.stringify(receipt)}`);
  if (expectedOutcome === 'failed' && (receipt.outcome !== 'failed' || typeof receipt.error !== 'string' || receipt.error === '')) {
    fail(`${label}: failed receipt omitted visible error ${JSON.stringify(receipt)}`);
  }
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

async function waitForWorkflowDetail(rpc, workflow, label) {
  const deadline = Date.now() + 30000;
  let last = 'detail missing';
  while (Date.now() < deadline) {
    try {
      const detail = await rpc.call({ type: 'workflow_detail', workflowId: workflow.workflowId });
      const tasks = Array.isArray(detail.todo)
        ? detail.todo.flatMap((phase) => Array.isArray(phase?.tasks) ? phase.tasks : [])
        : [];
      if (
        detail.id === workflow.workflowId && detail.generation === workflow.generation &&
        detail.name === workflow.name && detail.objective === workflow.objective &&
        LIVE_WORKFLOW_STATUSES.has(detail.status) &&
        detail.worktree?.label === workflow.worktree && detail.worktree?.branch === workflow.branch &&
        typeof detail.supervisor?.name === 'string' && detail.supervisor.name !== '' &&
        Array.isArray(detail.planningActivity) && detail.planningActivity.length > 0 &&
        tasks.length > 0 && Array.isArray(detail.subagents) && detail.subagents.length > 0 &&
        Array.isArray(detail.jobs) && detail.jobs.length > 0
      ) return detail;
      last = JSON.stringify(detail);
    } catch (err) {
      last = err instanceof Error ? err.message : String(err);
    }
    await new Promise((resolve) => setTimeout(resolve, 150));
  }
  fail(`${label}: supervisor/workers/tasks/planning detail never converged: ${last}`);
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

function requireWorkflowDetailIsolation(alpha, beta, alphaDetail, betaDetail) {
  const alphaEncoded = JSON.stringify(alphaDetail);
  const betaEncoded = JSON.stringify(betaDetail);
  for (const marker of [beta.workflowId, beta.name, beta.objective, beta.worktree, beta.branch]) {
    if (alphaEncoded.includes(marker)) fail(`alpha detail crossed with beta marker ${marker}`);
  }
  for (const marker of [alpha.workflowId, alpha.name, alpha.objective, alpha.worktree, alpha.branch]) {
    if (betaEncoded.includes(marker)) fail(`beta detail crossed with alpha marker ${marker}`);
  }
  const alphaJobs = new Set(alphaDetail.jobs.map((job) => job.jobId));
  for (const job of betaDetail.jobs) if (alphaJobs.has(job.jobId)) fail(`workflow job crossed A/B ownership: ${job.jobId}`);
  if (`${alpha.workflowId}@${alpha.generation}` === `${beta.workflowId}@${beta.generation}`) fail('workflow identity tuples collided');
}

async function connectPage(page) {
  if (token) {
    await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
  }
  await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
  await waitFor(page, () => document.title === 'rpi web', 'page title missing');
  // No #conn-state element: the Send button enables only after the reconnect
  // bootstrap completes (connState on).
  await waitFor(page, () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true, 'WS did not reach "connected"');
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
    // WebRTC/media platform stubs for the realtime coverage scenario. The
    // App runs its REAL startRealtime -> setupRealtimeCall flow and
    // the REAL realtime_create_call RPC; only the browser platform APIs
    // (navigator.mediaDevices.getUserMedia, RTCPeerConnection) are replaced so
    // the WebRTC handshake is deterministic. The stubs expose
    // window.__rtStub (sendFrame/openChannel/closeChannel) for the test to
    // drive oai-events frames.
    await page.addInitScript(() => {
      if (window.__rtStub) return;
      class FakeAudioTrack {
        constructor() { this.kind = 'audio'; }
        stop() {}
      }
      class FakeMediaStream {
        constructor() { this.track = new FakeAudioTrack(); }
        getTracks() { return [this.track]; }
      }
      class FakeDataChannel {
        constructor(label) {
          this.label = label;
          this.readyState = 'connecting';
          this.sent = [];
          this.onopen = null;
          this.onmessage = null;
          this.onclose = null;
          this.onerror = null;
        }
        send(data) { this.sent.push(data); }
        close() { this.readyState = 'closed'; }
      }
      class FakePeerConnection {
        constructor() {
          this.localDescription = null;
          this.remoteDescription = null;
          this.connectionState = 'new';
          // iceGatheringState 'complete' makes src/realtime.ts's
          // waitForIceGatheringComplete resolve immediately (the real browser
          // gathers host candidates in well under a second; the fake skips the
          // wait so the E2E flow stays deterministic). The gather wait reads
          // pc.localDescription.sdp AFTER setLocalDescription, so the fake
          // stores the offer there verbatim. The SECOND coverage call arms
          // window.__rtStub.gatherDelay so the fake starts 'gathering' and
          // the test completes it via __rtStub.finishGathering() — driving
          // waitForIceGatheringComplete's icegatheringstatechange event path.
          this.iceGatheringState = window.__rtStub.gatherDelay ? 'gathering' : 'complete';
          this.gatherListeners = [];
          this.channels = [];
          this.onicecandidate = null;
          this.onconnectionstatechange = null;
          this.ontrack = null;
          window.__rtStub.pc = this;
          window.__rtStub.dc = null;
        }
        addEventListener(type, fn) {
          if (type === 'icegatheringstatechange') this.gatherListeners.push(fn);
        }
        removeEventListener(type, fn) {
          if (type === 'icegatheringstatechange') {
            this.gatherListeners = this.gatherListeners.filter((h) => h !== fn);
          }
        }
        finishGathering() {
          this.iceGatheringState = 'complete';
          for (const handler of this.gatherListeners) handler();
        }
        addTrack() {}
        createDataChannel(label) {
          const dc = new FakeDataChannel(label);
          this.channels.push(dc);
          window.__rtStub.dc = dc;
          return dc;
        }
        async createOffer() {
          return { type: 'offer', sdp: 'v=0\r\no=e2e 0 0 IN IP4 127.0.0.1\r\ns=e2e-offer\r\n' };
        }
        async setLocalDescription(desc) { this.localDescription = desc; }
        async setRemoteDescription(desc) {
          this.remoteDescription = desc;
          this.connectionState = 'connected';
          if (this.onconnectionstatechange) this.onconnectionstatechange({});
        }
        close() { this.connectionState = 'closed'; }
      }
      window.__rtStub = {
        pc: null,
        dc: null,
        getUserMedia() { return Promise.resolve(new FakeMediaStream()); },
        openChannel() {
          const dc = window.__rtStub.dc;
          if (dc) {
            dc.readyState = 'open';
            if (dc.onopen) dc.onopen({});
          }
        },
        finishGathering() {
          const pc = window.__rtStub.pc;
          if (pc) pc.finishGathering();
        },
        sendFrame(obj) {
          const dc = window.__rtStub.dc;
          if (dc && dc.onmessage) dc.onmessage({ data: JSON.stringify(obj) });
        },
        closeChannel() {
          const dc = window.__rtStub.dc;
          if (dc) {
            dc.readyState = 'closed';
            if (dc.onclose) dc.onclose({});
          }
        },
      };
      Object.defineProperty(navigator, 'mediaDevices', {
        configurable: true,
        value: { getUserMedia: () => window.__rtStub.getUserMedia() },
      });
      window.RTCPeerConnection = FakePeerConnection;
    });
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');

    // 1. No token: silent boot probe -> never connected, no error toast. The
    //    empty-hint is the authoritative witness (the #conn-state element is
    //    removed): "Connecting to the control plane…" while disconnected,
    //    flipping to "Send a prompt" only after a successful connect.
    let everConnected = false;
    const hintWatch = setInterval(async () => {
      try {
        const hint = await page.evaluate(() => document.querySelector('.empty-hint')?.textContent || '');
        if (hint.includes('Send a prompt to start the session.')) everConnected = true;
      } catch {
        /* page mid-navigation */
      }
    }, 100);
    await waitFor(
      page,
      () => (document.querySelector('.empty-hint')?.textContent || '').includes('Connecting to the control plane'),
      'no-token boot probe never settled into the not-connected empty hint',
      20000
    );
    await page.waitForTimeout(3500);
    clearInterval(hintWatch);
    const noTokenToast = await page.evaluate(() =>
      Array.from(document.querySelectorAll('#toasts .toast')).some((t) => t.textContent.includes('connection failed'))
    );
    if (noTokenToast) fail('no-token boot probe should be silent, but an error toast appeared');
    if (everConnected) fail('WS reached "connected" without a token');
    record('auth.no-token-probe');

    // 2. Wrong token: explicit Connect -> error toast, never connected.
    await page.click('#settings-toggle-btn');
    await waitFor(page, () => document.querySelector('#settings-token-input') !== null, 'settings token input missing');
    await page.fill('#settings-token-input', wrongToken);
    await page.click('#settings-token-save-btn');
    await page.click('#settings-close-btn');
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('#toasts .toast--error')).some((t) =>
          t.textContent.includes('wrong or missing token')
        ),
      'wrong-token error toast never appeared'
    );
    const wrongOn = await page.evaluate(() => (document.querySelector('.empty-hint')?.textContent || '').includes('Send a prompt to start the session.'));
    if (wrongOn) fail('WS reached "connected" with a wrong token');
    record('auth.wrong-token-toast');

    // 3. Good token: Settings save reconnects with rpi-auth.<token>.
    await page.click('#settings-toggle-btn');
    await waitFor(page, () => document.querySelector('#settings-token-input') !== null, 'settings token input missing');
    await page.fill('#settings-token-input', token);
    await page.click('#settings-token-save-btn');
    await page.click('#settings-close-btn');
    await waitFor(
      page,
      () => (document.querySelector('.empty-hint')?.textContent || '').includes('Send a prompt to start the session.'),
      'WS did not reach "connected" with the good token'
    );
    record('auth.good-token-connect');

    // ============ Phase B: prompt round-trips + early abort ============
    // Request 1 in the steering mock is the ~4s slow stream.
    await page.fill('#prompt-input', 'hello from the web e2e');
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, (tail) => document.body.textContent.includes(tail), 'full slow reply never streamed into the DOM', 30000, slowTail);
    await waitFor(page, () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message', 'unified action did not return to Send after the reply completed');
    record('app.primary-action-send');
    record('prompt.slow-stream-full');

    // Request 2 is instant. Wait for user-visible idle before the next NEW
    // prompt: Enter while streaming is mapped to steer, not a fresh request.
    await page.fill('#prompt-input', 'again');
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, (reply) => document.body.textContent.includes(reply), 'fast reply never streamed into the DOM', 30000, fastReply);
    await waitForIdle(page, 'session not idle after fast reply before abort stream');
    record('prompt.fast-roundtrip');

    // Request 3: slow stream, EARLY abort.
    await page.fill('#prompt-input', 'stream a long answer');
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, () => document.body.textContent.includes('steer-3-'), 'third stream never started');
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Stop generating',
      'unified action never switched to Stop for the third stream'
    );
    record('app.primary-action-stop');
    // Dismiss every toast accumulated by earlier phases (e.g. the Phase A
    // wrong-token "connection failed" error toast, which auto-dismisses only
    // after 7s) so the abort's anyError check below is scoped to toasts the
    // abort itself produces — the strong assertion is NOT weakened.
    await page.evaluate(() => {
      document.querySelectorAll('#toasts .toast').forEach((t) => t.click());
    });
    await waitFor(page, () => document.querySelectorAll('#toasts .toast').length === 0, 'stale toasts did not clear before the abort');
    await page.click('#send-btn');
    await waitFor(page, () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message', 'unified action did not return to Send after abort');
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

    // Recovery reply text can land while the run is still marked active;
    // Phase C needs a NEW prompt (not steer), so establish the idle contract
    // before any rich attempt. Each failed non-rich attempt also settles idle
    // before the next retry.
    await waitForIdle(page, 'session not idle after recovery before rich content');

    // ==================== Phase C: rich content ====================
    const richPromptAttempt = async () => {
      await waitForIdle(page, 'session not idle before rich prompt attempt');
      const before = await page.evaluate(() => document.querySelectorAll('.msg--assistant .assistant-text').length);
      await page.fill('#prompt-input', 'render rich content');
      await page.press('#prompt-input', 'Enter');
      await waitFor(
        page,
        (b) => {
          const nodes = [...document.querySelectorAll('.msg--assistant .assistant-text')];
          if (nodes.length <= b) return false;
          if (document.querySelector('table.md-table')) return true;
          return document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message';
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

    // ---- markdown renderer coverage: extra branches (RICH_TEXT_EXTRA) ----
    // hr / blockquote / ordered+nested lists / task-glyph variants /
    // unregistered-language + lang-less fences / empty + invalid mermaid
    // hydration / currency-vs-math / URL-policy extras.
    await page.fill('#prompt-input', 'render markdown extra branches');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.querySelector('#transcript .assistant-text .md-quote') !== null,
      'md.extra: blockquote never rendered',
      30000
    );
    record('md.extra-blockquote');
    await waitFor(
      page,
      () => [...document.querySelectorAll('#transcript .assistant-text hr')].length >= 1,
      'md.extra: hr never rendered',
      10000
    );
    record('md.extra-hr');
    await waitFor(
      page,
      () => document.querySelector('#transcript .assistant-text ol.md-list') !== null,
      'md.extra: ordered list never rendered',
      10000
    );
    record('md.extra-ol');
    const nestedList = await page.evaluate(() => {
      const ol = document.querySelector('#transcript .assistant-text ol.md-list');
      return !!ol && !!ol.querySelector('li ul.md-list');
    });
    if (!nestedList) fail('md.extra: nested list never rendered inside a list item');
    record('md.extra-nested-list');
    const extraFenceLangs = await page.evaluate(() =>
      [...document.querySelectorAll('#transcript .md-fence__lang')].map((n) => n.textContent || '')
    );
    if (!extraFenceLangs.includes('weirdlang')) {
      fail(`md.extra: unregistered-language fence label missing: ${JSON.stringify(extraFenceLangs)}`);
    }
    if (!extraFenceLangs.includes('text')) {
      fail(`md.extra: lang-less fence label missing: ${JSON.stringify(extraFenceLangs)}`);
    }
    record('md.extra-fence-langs');
    // Empty mermaid source: the async hydration empty-source branch clears
    // the host (no rendered/error class, empty textContent).
    await waitFor(
      page,
      () =>
        [...document.querySelectorAll('#transcript .md-mermaid-host')].some(
          (h) =>
            (h.textContent || '').trim() === '' &&
            !h.classList.contains('md-mermaid-host--rendered') &&
            !h.classList.contains('md-mermaid-host--error')
        ),
      'md.extra: empty mermaid host never cleared',
      15000
    );
    record('md.extra-mermaid-empty');
    // Invalid-diagram mermaid: hydration's error branch renders the error
    // host with the escaped source.
    await waitFor(
      page,
      () => document.querySelector('#transcript .md-mermaid-host--error') !== null,
      'md.extra: invalid mermaid never errored',
      15000
    );
    const mermaidError = await page.evaluate(() => {
      const host = document.querySelector('#transcript .md-mermaid-host--error');
      const style = host ? getComputedStyle(host) : null;
      return {
        source: host?.querySelector('.md-mermaid-error__source')?.textContent || '',
        borderLeftWidth: style?.borderLeftWidth || '',
        borderLeftStyle: style?.borderLeftStyle || '',
        paddingLeft: style?.paddingLeft || '',
      };
    });
    if (!mermaidError.source.includes('bad token here')) {
      fail(`md.extra: mermaid error source missing: ${mermaidError.source.slice(0, 120)}`);
    }
    if (mermaidError.borderLeftStyle !== 'none' && mermaidError.borderLeftWidth !== '0px') {
      fail(`md.extra: mermaid fallback retained a left border: ${JSON.stringify(mermaidError)}`);
    }
    if (mermaidError.paddingLeft !== '0px') {
      fail(`md.extra: mermaid fallback retained left padding: ${JSON.stringify(mermaidError)}`);
    }
    record('md.extra-mermaid-error');
    // mermaid 11.16.1's render() appends a temporary wrapper
    // (`<div id="d<id>"><svg id="<id>">…</svg></div>`) to document.body and
    // removes it on success, but on a PARSE error it THROWS before the
    // removal — the parser-error SVG ("Syntax error in text", "mermaid
    // version …") would leak below the composer/footer, one wrapper per
    // failed render. Exercise the failure REPEATEDLY (each retry renders a
    // fresh invalid diagram under a new render id) and prove the page holds
    // no residue while the in-host error fallback keeps rendering.
    for (let attempt = 0; attempt < 2; attempt++) {
      await page.fill('#prompt-input', 'render markdown extra branches');
      await page.press('#prompt-input', 'Enter');
      await waitFor(
        page,
        (n) => document.querySelectorAll('#transcript .md-mermaid-host--error').length >= n,
        `md.extra: repeated invalid mermaid render ${attempt + 2} never errored`,
        30000,
        attempt + 2
      );
    }
    const mermaidResidue = await page.evaluate(() => {
      const bodyText = document.body.textContent || '';
      return {
        syntaxError: bodyText.includes('Syntax error in text'),
        version: bodyText.includes('mermaid version'),
      };
    });
    if (mermaidResidue.syntaxError || mermaidResidue.version) {
      fail(
        `md.extra: mermaid parser error leaked below the composer ` +
        `(Syntax error in text=${mermaidResidue.syntaxError}, mermaid version=${mermaidResidue.version})`
      );
    }
    // The in-host degradation UI survives the cleanup for every attempt.
    const errorHostSources = await page.evaluate(
      () => [...document.querySelectorAll('#transcript .md-mermaid-host--error .md-mermaid-error__source')].map((n) => n.textContent || '')
    );
    if (errorHostSources.length < 3 || !errorHostSources.every((s) => s.includes('bad token here'))) {
      fail(`md.extra: in-host mermaid error fallback lost after cleanup: ${JSON.stringify(errorHostSources)}`);
    }
    record('md.extra-mermaid-no-residue');
    const extraRichText = await lastAssistantText(page);
    if (!extraRichText.includes('5$/unit')) fail('md.extra: currency amount not preserved as literal text');
    record('md.extra-currency');
    // URL-policy extras: ./ relative image allowed, http image allowed,
    // javascript: image blocked, mailto link allowed.
    await waitFor(
      page,
      () => document.querySelector('#transcript img.md-image[alt="rel"]') !== null,
      'md.extra: relative ./ image never rendered',
      10000
    );
    record('md.extra-image-relative');
    await waitFor(
      page,
      () => document.querySelector('#transcript img.md-image[alt="http"]') !== null,
      'md.extra: http image never rendered',
      10000
    );
    await waitFor(
      page,
      () => document.querySelector('#transcript a[href^="mailto:"]') !== null,
      'md.extra: mailto link never rendered',
      10000
    );
    const blockedExtraImg = await page.evaluate(() =>
      [...document.querySelectorAll('#transcript .assistant-text img')].some((i) =>
        (i.getAttribute('src') || '').toLowerCase().startsWith('javascript:')
      )
    );
    if (blockedExtraImg) fail('md.extra: javascript: image leaked as a live img');
    record('md.extra-url-policy');

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
        return last.getAttribute('data-tool-name') === 'read';
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
        return !!last && last.getAttribute('data-tool-status') === 'done';
      },
      'read tool-card never reached done state',
      30000
    );
    const toolCardArgs = await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.tool-card')];
      const last = cards[cards.length - 1];
      if (!last || last.getAttribute('data-tool-name') !== 'read') return null;
      const resultEl = last.querySelector('.tool-card__output') || last.querySelector('.tool-card__result');
      return {
        name: last.getAttribute('data-tool-name') || '',
        result: resultEl ? resultEl.textContent || '' : '',
        summaryPath: last.querySelector('.tool-card__summary-path')?.textContent || '',
      };
    });
    if (!toolCardArgs || toolCardArgs.name !== 'read') fail(`tool-card name wrong: ${JSON.stringify(toolCardArgs)}`);
    if (toolCardArgs.summaryPath !== 'seed.txt') {
      fail(`tool-card summary path never rendered: ${JSON.stringify(toolCardArgs)}`);
    }
    if (!toolCardArgs.result.includes('web coverage seed')) fail(`tool-card result never showed the read file content: "${toolCardArgs.result}"`);
    record('app.tool-card-done');

    await waitFor(
      page,
      () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message',
      'tool-card turn never settled',
      30000,
    );
    const settledToolCard = await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.tool-card')];
      const last = cards[cards.length - 1];
      if (!last) return null;
      const resultEl = last.querySelector('.tool-card__output') || last.querySelector('.tool-card__result');
      return {
        name: last.getAttribute('data-tool-name') || '',
        state: last.getAttribute('data-tool-status') || last.querySelector('.tool-card__state')?.textContent || '',
        result: resultEl ? resultEl.textContent || '' : '',
      };
    });
    if (!settledToolCard || settledToolCard.name !== 'read' || settledToolCard.state !== 'done') {
      fail(`tool-card disappeared or changed after agent settlement: ${JSON.stringify(settledToolCard)}`);
    }
    if (!settledToolCard.result.includes('web coverage seed')) {
      fail(`settled tool-card lost its result: ${JSON.stringify(settledToolCard)}`);
    }
    record('app.tool-card-settled');

    await page.fill('#prompt-input', 'tool-card coverage web search');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => [...document.querySelectorAll('.tool-card')].some((card) =>
        card.getAttribute('data-tool-name') === 'web_search' &&
        card.getAttribute('data-tool-status') === 'error'
      ),
      'unavailable web_search tool card never rendered its deterministic error',
      30000
    );
    const webSearchCard = await page.evaluate(() => {
      const card = [...document.querySelectorAll('.tool-card')].find((node) =>
        node.getAttribute('data-tool-name') === 'web_search'
      );
      if (!card) return null;
      const resultEl = card.querySelector('.tool-card__output') || card.querySelector('.tool-card__result');
      return {
        hasArgs: card.querySelector('.tool-card__args') !== null,
        isError: card.getAttribute('data-tool-status') === 'error' || card.querySelector('.tool-card__state--error') !== null,
        result: resultEl ? resultEl.textContent || '' : '',
      };
    });
    if (!webSearchCard?.hasArgs || !webSearchCard.isError || !webSearchCard.result.includes('Tool web_search not found')) {
      fail(`unavailable web_search card did not render args and the real dispatch error: ${JSON.stringify(webSearchCard)}`);
    }
    record('app.tool-card-web-search');

    await page.fill('#prompt-input', 'tool-card coverage edit seed');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => [...document.querySelectorAll('.tool-card--edit')].some((card) =>
        card.querySelector('.tool-card__state--done') !== null
      ),
      'edit tool card never rendered and settled',
      30000
    );
    const editCard = await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.tool-card--edit')];
      const card = cards[cards.length - 1];
      if (!card) return null;
      return {
        path: card.querySelector('.tool-card__edit-path')?.textContent || '',
        operation: card.querySelector('.tool-card__edit-op')?.textContent || '',
        additions: [...card.querySelectorAll('.tool-card__diff-line--add')].map((line) => line.textContent || ''),
        rawOpen: card.querySelector('details.tool-card__raw[open]') !== null,
      };
    });
    if (!editCard || editCard.path !== 'seed.txt' || editCard.operation !== 'edit') {
      fail(`edit card metadata wrong: ${JSON.stringify(editCard)}`);
    }
    if (!editCard.additions.some((line) => line.includes('web coverage edited')) || editCard.rawOpen) {
      fail(`edit card semantic diff/default collapse wrong: ${JSON.stringify(editCard)}`);
    }
    record('app.tool-card-edit');

    // ============== Codex Live realtime voice call (real App flow) ==============
    // The fixture advertises live.enabled + mode=realtime with
    // realtimeBaseUrl pointing at the loopback mock; the WebRTC platform APIs
    // are stubbed (addInitScript) but the App's REAL startRealtime ->
    // setupRealtimeCall path runs: getUserMedia -> RTCPeerConnection ->
    // addTrack -> createDataChannel('oai-events') -> createOffer ->
    // setLocalDescription -> REAL realtime_create_call RPC (Rust proxy ->
    // mock POST /v1/realtime/calls) -> setRemoteDescription. Events then
    // arrive over the stubbed data channel via window.__rtStub.sendFrame.
    const rtOverlaySel = '#realtime-transcript';
    await page.click('#mic-btn');
    await waitFor(
      page,
      (sel) => document.querySelector(sel) !== null,
      'realtime: overlay never appeared after the mic click',
      30000,
      rtOverlaySel
    );
    record('realtime.overlay-visible');
    // The proxy's POST reached the mock with the contract headers; the mock
    // recorded them (MOCK_REALTIME_EVIDENCE) for this assertion.
    const rtCallMeta = await readJsonIfExists(path.join(evidence, 'realtime-call.json'));
    if (!rtCallMeta || rtCallMeta.openaiAlpha !== 'quicksilver=v2') {
      fail(`realtime: create-call proxy did not send OpenAI-Alpha: quicksilver=v2 (got ${JSON.stringify(rtCallMeta)})`);
    }
    if (!rtCallMeta.callId || !String(rtCallMeta.callId).startsWith('rtc_')) {
      fail(`realtime: create-call call id missing/not rtc_ (got ${JSON.stringify(rtCallMeta)})`);
    }
    if (rtCallMeta.authPresent !== true) {
      fail(`realtime: create-call proxy did not send the Bearer token (got ${JSON.stringify(rtCallMeta)})`);
    }
    if (rtCallMeta.sessionPresent !== true || rtCallMeta.sessionValid !== true
      || rtCallMeta.sessionInputOmitted !== true) {
      fail(`realtime: create-call proxy did not send the AVAS-compatible session object (got ${JSON.stringify(rtCallMeta)})`);
    }
    record('realtime.proxy-alpha-header');
    const sessionUpdate = await page.evaluate(() => {
      const sent = (window.__rtStub.dc?.sent || []).map((raw) => {
        try { return JSON.parse(raw); } catch { return null; }
      });
      return sent.find((frame) => frame?.type === 'session.update') || null;
    });
    if (!sessionUpdate) fail('realtime: no session.update frame sent on the oai-events channel');
    if ('input' in (sessionUpdate.session?.audio || {})
      || 'model' in sessionUpdate.session || 'type' in sessionUpdate.session) {
      fail(`realtime: session.update sent a rejected field (${JSON.stringify(sessionUpdate.session)})`);
    }
    record('realtime.session-update-transcription');
    // Drive a GA input-transcript delta over the data channel: overlay text
    // grows without a React re-render (direct node write).
    await page.evaluate(() =>
      window.__rtStub.sendFrame({ type: 'conversation.item.input_audio_transcription.delta', delta: 'realtime ' })
    );
    await waitFor(
      page,
      () => (document.querySelector('.realtime-transcript__text')?.textContent || '').includes('realtime'),
      'realtime: transcript delta never reached the overlay',
      10000
    );
    record('realtime.transcript-delta');
    // A GA segment event carries the FULL interim segment text: it SUPERSEDES
    // the running preview (Hyper dictation parity) instead of appending.
    await page.evaluate(() =>
      window.__rtStub.sendFrame({
        type: 'conversation.item.input_audio_transcription.segment',
        text: 'realtime segment preview',
      })
    );
    await waitFor(
      page,
      () => (document.querySelector('.realtime-transcript__text')?.textContent || '').trim() === 'realtime segment preview',
      'realtime: segment event never superseded the overlay preview',
      10000
    );
    record('realtime.transcript-segment');
    // A GA final event commits the whole utterance to the composer.
    const userBubblesBeforeFinal = await page.evaluate(() => document.querySelectorAll('.msg--user').length);
    await page.evaluate(() =>
      window.__rtStub.sendFrame({
        type: 'conversation.item.input_audio_transcription.completed',
        transcript: 'realtime hello world',
      })
    );
    await waitFor(
      page,
      () => (document.getElementById('prompt-input')?.value || '').includes('realtime hello world'),
      'realtime: final transcript never committed to the composer',
      10000
    );
    record('realtime.transcript-commit');
    // NO auto-send: the final lands as a DRAFT only — the composer still holds
    // the text, no user bubble was dispatched, and the preview cleared.
    const userBubblesAfterFinal = await page.evaluate(() => document.querySelectorAll('.msg--user').length);
    if (userBubblesAfterFinal !== userBubblesBeforeFinal) {
      fail(`realtime: final transcript auto-sent a message (${userBubblesBeforeFinal} -> ${userBubblesAfterFinal} user bubbles)`);
    }
    const previewCleared = await page.evaluate(() => (document.querySelector('.realtime-transcript__text')?.textContent || '') === '');
    if (!previewCleared) fail('realtime: preview overlay was not cleared after the final commit');
    await page.screenshot({ path: `${evidence}/realtime-composer-final.png`, fullPage: true });
    record('realtime.composer-final-screenshot');
    // The V1 final alias for the same utterance must be deduped.
    await page.evaluate(() =>
      window.__rtStub.sendFrame({ type: 'conversation.input_transcript.done', transcript: 'realtime hello world' })
    );
    await page.waitForTimeout(400);
    const committedOnce = await page.evaluate(
      () => (document.getElementById('prompt-input')?.value.match(/realtime hello world/g) || []).length
    );
    if (committedOnce !== 1) fail(`realtime: final transcript double-committed (count ${committedOnce})`);
    record('realtime.transcript-dedup');
    // A GA transcription failure surfaces a bounded toast through
    // realtimeErrorMessage, commits NOTHING, and clears the stale preview.
    await page.evaluate(() =>
      window.__rtStub.sendFrame({
        type: 'conversation.item.input_audio_transcription.failed',
        error: { message: 'mock asr rejected', code: 'asr_down' },
      })
    );
    await waitFor(
      page,
      () => document.body.textContent.includes('realtime error [asr_down]: mock asr rejected'),
      'realtime: transcription.failed never surfaced a bounded toast',
      10000
    );
    const failedDidNotCommit = await page.evaluate(
      () => (document.getElementById('prompt-input')?.value.match(/realtime hello world/g) || []).length === 1
    );
    if (!failedDidNotCommit) fail('realtime: transcription.failed mutated the composer');
    record('realtime.transcription-failed-toast');
    // An error frame surfaces a bounded toast through realtimeErrorMessage.
    await page.evaluate(() =>
      window.__rtStub.sendFrame({ type: 'error', error: { message: 'realtime boom', code: 'E-2' } })
    );
    await waitFor(
      page,
      () => document.body.textContent.includes('realtime error [E-2]: realtime boom'),
      'realtime: error frame never surfaced a toast',
      10000
    );
    record('realtime.error-toast');
    // A delegation event renders the overlay delegation row.
    await page.evaluate(() =>
      window.__rtStub.sendFrame({ type: 'delegation.created', delegation: { description: 'delegating to writer' } })
    );
    await waitFor(
      page,
      () => (document.querySelector('.realtime-transcript__delegation')?.textContent || '').includes('delegating to writer'),
      'realtime: delegation never rendered in the overlay',
      10000
    );
    record('realtime.delegation');
    // Stop via the same mic button: realtime_stop RPC + overlay teardown.
    await page.click('#mic-btn');
    await waitFor(
      page,
      (sel) => document.querySelector(sel) === null,
      'realtime: overlay never disappeared after the stop click',
      10000,
      rtOverlaySel
    );
    record('realtime.stop');

    // ============== Phase L: reconnect (real server kill) ==============
    // The lane shell (coverage.sh) watches kill-server.marker, kills and
    // respawns the listener on the SAME port with --continue, then writes
    // server-up.marker.
    const killMarker = path.join(work, 'kill-server.marker');
    const upMarker = path.join(work, 'server-up.marker');
    fs.rmSync(killMarker, { force: true });
    fs.rmSync(upMarker, { force: true });
    let sawTransportToast = false;
    const toastWatch = setInterval(async () => {
      try {
        if ((await page.evaluate(() => document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length)) >= 1) {
          sawTransportToast = true;
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
    clearInterval(toastWatch);
    if (!sawTransportToast) fail('no transport outage toast after the server was killed');
    record('reconnect.pill');
    await waitFor(
      page,
      () => document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length === 0,
      'client never auto-reconnected after the listener came back'
    );
    record('reconnect.auto-on');
    const survives = await page.evaluate((tail) => document.body.textContent.includes(tail), slowTail);
    if (!survives) fail('pre-crash assistant reply vanished after the reconnect');
    record('reconnect.transcript-survives');
    const toolCardAfterReconnect = await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.tool-card')];
      const readCard = cards.find((card) => card.getAttribute('data-tool-name') === 'read');
      if (!readCard) return null;
      const resultEl = readCard.querySelector('.tool-card__output') || readCard.querySelector('.tool-card__result');
      return {
        state: readCard.getAttribute('data-tool-status') || readCard.querySelector('.tool-card__state')?.textContent || '',
        result: resultEl ? resultEl.textContent || '' : '',
      };
    });
    if (!toolCardAfterReconnect || toolCardAfterReconnect.state !== 'done' || !toolCardAfterReconnect.result.includes('web coverage seed')) {
      fail(`completed tool-card did not survive authoritative reconnect replay: ${JSON.stringify(toolCardAfterReconnect)}`);
    }
    record('app.tool-card-reconnect');
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

    // ==================== Phase E: unified Todo module ====================
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
    // Re-establish the open precondition explicitly: live Todo updates can
    // settle between dependency mutations, but this branch must be exercised
    // from an in-progress task rather than clicking an absent control.
    if (await page.$('#todo-detail-reopen')) {
      await page.click('#todo-detail-reopen');
    }
    await waitFor(
      page,
      () => {
        const rows = [...document.querySelectorAll('.todo-task')];
        const row = rows.find((r) => r.textContent.includes('web e2e task'));
        const inProgress =
          !!row && row.querySelector('.todo-task__bullet')?.getAttribute('aria-label') === 'in_progress';
        return inProgress && document.getElementById('todo-detail-complete') !== null;
      },
      'detail task not in_progress with Complete visible before Complete coverage'
    );
    await page.click('#todo-detail-complete');
    await waitFor(
      page,
      () => {
        const rows = [...document.querySelectorAll('.todo-task')];
        const row = rows.find((r) => r.textContent.includes('web e2e task'));
        const completed =
          !!row && row.querySelector('.todo-task__bullet')?.getAttribute('aria-label') === 'completed';
        return completed && document.getElementById('todo-detail-reopen') !== null;
      },
      'detail Complete never reached completed with Reopen visible'
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
    // ---- todo convergence: complete every remaining task through real UI
    // actions, then assert the authoritative live projection converges to
    // zero open/active/waiting with an exact done count. Rows and counters
    // derive from the same phases, so a stale counter, a hidden incomplete
    // row, or a phase/global mismatch fails ----
    // The dependency target is still open; complete it via its row action.
    await page.evaluate(() => {
      const rows = [...document.querySelectorAll('.todo-task')];
      const row = rows.find((r) => r.textContent.includes('dep target task'));
      if (!row) throw new Error('dep target task row missing');
      const btn = row.querySelector('.todo-task__action[data-action="complete"]');
      if (!btn) throw new Error('dep target task complete button missing');
      btn.click();
    });
    await waitFor(
      page,
      () => {
        const rows = [...document.querySelectorAll('.todo-task')];
        const row = rows.find((r) => r.textContent.includes('dep target task'));
        return !!row && row.querySelector('.todo-task__bullet')?.getAttribute('aria-label') === 'completed';
      },
      'dep target task never reached completed status'
    );
    // The first task was reopened by the detail-pane Reopen coverage; complete
    // it again through its row action.
    await page.evaluate(() => {
      const rows = [...document.querySelectorAll('.todo-task')];
      const row = rows.find((r) => r.textContent.includes('web e2e task'));
      if (!row) throw new Error('web e2e task row missing');
      const btn = row.querySelector('.todo-task__action[data-action="complete"]');
      if (!btn) throw new Error('web e2e task complete button missing');
      btn.click();
    });
    await waitFor(
      page,
      () => {
        const rows = [...document.querySelectorAll('.todo-task')];
        const row = rows.find((r) => r.textContent.includes('web e2e task'));
        return !!row && row.querySelector('.todo-task__bullet')?.getAttribute('aria-label') === 'completed';
      },
      'web e2e task never reached completed status'
    );
    // Authoritative live projection: every rendered task row is terminal
    // completed, the global counts show zero open/active/waiting with the
    // exact done count, and every phase header reports its own 0 open · N done.
    await assertId(
      page,
      'todo.all-rows-counts-converged',
      () => {
        const rows = [...document.querySelectorAll('.todo-task')];
        if (rows.length !== 2) return false;
        const expectedTasks = ['dep target task', 'web e2e task'];
        const taskNames = rows.map(
          (row) => (row.querySelector('.todo-task__content')?.textContent || '').trim()
        ).sort();
        if (taskNames.some((name, index) => name !== expectedTasks[index])) return false;
        const allCompleted = rows.every(
          (row) => row.querySelector('.todo-task__bullet')?.getAttribute('aria-label') === 'completed'
        );
        if (!allCompleted) return false;
        const items = [...document.querySelectorAll('#todo-counts .todo-panel__counts-item')].map(
          (el) => (el.textContent || '').trim()
        );
        const globalOk =
          items.length === 4 &&
          items[0] === '0 open' &&
          items[1] === '0 active' &&
          items[2] === '0 waiting' &&
          items[3] === '2 done';
        if (!globalOk) return false;
        const phases = [...document.querySelectorAll('.todo-phase')];
        if (phases.length === 0) return false;
        return phases.every((phase) => {
          const phaseRows = [...phase.querySelectorAll('.todo-task')];
          if (phaseRows.length === 0) return false;
          const phaseDone = phaseRows.filter(
            (row) => row.querySelector('.todo-task__bullet')?.getAttribute('aria-label') === 'completed'
          ).length;
          if (phaseDone !== phaseRows.length) return false;
          const countsText = phase.querySelector('.todo-phase__counts')?.textContent || '';
          const m = countsText.match(/^0 open · (\d+) done$/);
          return !!m && Number(m[1]) === phaseDone;
        });
      },
      'todo rows/counters never converged to exactly two completed fixture rows with done=2'
    );

    // ==================== Phase F: goal panel ====================
    await page.click('#goal-panel-btn');
    await waitFor(
      page,
      () => document.getElementById('todo-panel') === null && document.getElementById('goal-panel') !== null,
      'Todo module did not close when goal panel opened'
    );
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
    const workflowSessionId = await page.evaluate(
      () => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
    );
    if (!workflowSessionId) fail('active sessionId missing for workflow acceptance');
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

    // ==================== Phase G: strict concurrent workflow panel ====================
    await page.click('#goal-close-btn');
    await waitFor(page, () => document.getElementById('goal-panel') === null, 'goal panel did not close');
    await page.click('#workflow-toggle-btn');
    await waitFor(page, () => document.getElementById('workflow-panel') !== null, 'workflow panel did not open');
    const [alphaCreated, betaCreated] = await Promise.all([
      rpc.call({ type: 'workflow_create', sessionId: workflowSessionId, name: 'web-e2e-workflow-alpha', objective: 'alpha coverage workflow objective' }),
      rpc.call({ type: 'workflow_create', sessionId: workflowSessionId, name: 'web-e2e-workflow-beta', objective: 'beta coverage workflow objective' }),
    ]);
    if (!alphaCreated?.workflowId || !betaCreated?.workflowId) fail('concurrent workflow create response omitted workflowId');
    const workflowRpc = { call: (command) => rpc.call({ ...command, sessionId: workflowSessionId }) };
    const [alpha, beta] = await Promise.all([
      waitForWorkflowSnapshot(workflowRpc, alphaCreated.workflowId, 'web-e2e-workflow-alpha', 'workflow alpha create'),
      waitForWorkflowSnapshot(workflowRpc, betaCreated.workflowId, 'web-e2e-workflow-beta', 'workflow beta create'),
    ]);
    if (alpha.workflowId === beta.workflowId) fail(`workflowId collision: ${alpha.workflowId}`);
    if (`${alpha.workflowId}@${alpha.generation}` === `${beta.workflowId}@${beta.generation}`) fail('workflow identity tuple collision');
    if (alpha.worktree === beta.worktree) fail(`worktree collision: ${alpha.worktree}`);
    if (alpha.branch === beta.branch) fail(`branch collision: ${alpha.branch}`);
    record('workflow.create-row');

    await waitFor(
      page,
      (names) => names.every((name) => {
        const row = [...document.querySelectorAll('.workflow-row')].find((candidate) =>
          (candidate.querySelector('.workflow-row__name')?.textContent || '') === name
        );
        return !!row && ['queued', 'planning', 'running', 'paused', 'integrating'].includes(row.dataset.status || '');
      }),
      'two workflows were not simultaneously live in the panel',
      30000,
      [alpha.name, beta.name]
    );
    const listed = await workflowRpc.call({ type: 'workflow_list' });
    const listedById = new Map((listed.workflows || []).map((workflow) => [workflow.workflowId, workflow]));
    for (const workflow of [alpha, beta]) {
      const row = listedById.get(workflow.workflowId);
      if (!row || row.generation !== workflow.generation || row.worktree !== workflow.worktree || row.branch !== workflow.branch) {
        fail(`workflow list lost exact identity for ${workflow.name}: ${JSON.stringify(row)}`);
      }
      if (!LIVE_WORKFLOW_STATUSES.has(row.status)) fail(`${workflow.name} was not live in backend list: ${row.status}`);
    }
    record('workflow.live-status');

    const [alphaDetail, betaDetail] = await Promise.all([
      waitForWorkflowDetail(workflowRpc, alpha, 'workflow alpha detail'),
      waitForWorkflowDetail(workflowRpc, beta, 'workflow beta detail'),
    ]);
    requireWorkflowDetailIsolation(alpha, beta, alphaDetail, betaDetail);
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
      'alpha live supervisor/planning/tasks/workers detail never rendered',
      30000,
      alpha
    );
    record('workflow.select-row');
    const foldableBefore = await page.evaluate(() => document.querySelectorAll('.workflow-actor__activity').length);
    await page.click('.workflow-actor.is-foldable .workflow-actor__row');
    await waitFor(
      page,
      (before) => document.querySelectorAll('.workflow-actor__activity').length !== before,
      'workflow actor activity feed never toggled',
      10000,
      foldableBefore
    );

    requireTypedReceipt(
      await workflowRpc.call({
        type: 'workflow_hub_send', workflowId: alpha.workflowId, generation: alpha.generation,
        recipient: alphaDetail.supervisor.name, body: 'alpha coverage receipt',
      }),
      alpha, alphaDetail.supervisor.name, 'delivered', 'alpha workflow send'
    );
    const alphaAfterIrc = await waitForWorkflowIrc(
      workflowRpc, alpha, 'alpha coverage receipt', 'workflow alpha detail after IRC'
    );
    const betaAfterAlphaIrc = await waitForWorkflowIrc(
      workflowRpc, beta, null, 'workflow beta detail after alpha IRC'
    );
    if (!(alphaAfterIrc.recentIrc || []).some((message) => message.text.includes('alpha coverage receipt'))) {
      fail('workflow alpha detail omitted its delivered IRC');
    }
    if ((betaAfterAlphaIrc.recentIrc || []).some((message) => message.text.includes('alpha coverage receipt'))) {
      fail('workflow alpha IRC crossed into beta detail');
    }
    await requireRpcFailure(
      workflowRpc.call({
        type: 'workflow_hub_send', workflowId: alpha.workflowId, generation: alpha.generation + 1,
        recipient: alphaDetail.supervisor.name, body: 'stale generation must fail',
      }),
      'stale workflow send'
    );
    requireTypedReceipt(
      await workflowRpc.call({
        type: 'workflow_hub_send', workflowId: alpha.workflowId, generation: alpha.generation,
        recipient: 'missing-workflow-recipient', body: 'must fail visibly',
      }),
      alpha, 'missing-workflow-recipient', 'failed', 'failed workflow send'
    );
    await waitFor(
      page,
      (body) => [...document.querySelectorAll('.workflow-irc__item')].some((item) => (item.textContent || '').includes(body)),
      'workflow IRC did not render backend-delivered alpha message',
      30000,
      'alpha coverage receipt'
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
    await page.fill('.workflow-irc__body', 'visible failed receipt');
    await waitFor(page, () => document.querySelector('.workflow-irc__send')?.disabled === false, 'failed workflow send did not enable');
    await page.click('.workflow-irc__send');
    await waitFor(
      page,
      () => {
        const receipt = document.querySelector('.workflow-irc__receipt--failed');
        return !!receipt && (receipt.querySelector('.workflow-irc__receipt-error')?.textContent || '').trim() !== '';
      },
      'workflow panel did not show the typed failed receipt error',
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
      'coverage screenshot state lost dual-live detail/worker/IRC evidence',
      30000,
      { names: [alpha.name, beta.name], workflowId: alpha.workflowId }
    );
    const workflowEvidenceName = 'coverage-workflow-concurrent-live-detail-workers-irc.png';
    await page.screenshot({ path: `${evidence}/${workflowEvidenceName}`, fullPage: true });
    console.log(`web-cov: workflow concurrent evidence=${workflowEvidenceName}`);

    await page.click('#workflow-pause-btn');
    await waitFor(
      page,
      (workflowId) => document.querySelector(`[data-workflow-id="${workflowId}"]`)?.getAttribute('data-status') === 'paused',
      'alpha workflow never paused',
      30000,
      alpha.workflowId
    );
    record('workflow.pause');
    await page.click('#workflow-resume-btn');
    await waitFor(
      page,
      (workflowId) => {
        const status = document.querySelector(`[data-workflow-id="${workflowId}"]`)?.getAttribute('data-status') || '';
        return ['planning', 'running', 'integrating'].includes(status);
      },
      'alpha workflow never resumed live',
      30000,
      alpha.workflowId
    );
    record('workflow.resume');
    await workflowRpc.call({ type: 'workflow_cancel', workflowId: alpha.workflowId });
    await waitFor(
      page,
      (workflowId) => document.querySelector(`[data-workflow-id="${workflowId}"]`)?.getAttribute('data-status') === 'cancelled',
      'alpha workflow cancel never reached cancelled',
      30000,
      alpha.workflowId
    );
    record('workflow.cancel-status');
    const betaAfterCancel = await waitForWorkflowSnapshot(
      workflowRpc, beta.workflowId, beta.name, 'beta after alpha cancel'
    );
    if (betaAfterCancel.generation !== beta.generation) fail('beta generation changed after alpha cancel');
    requireTypedReceipt(
      await workflowRpc.call({
        type: 'workflow_hub_send', workflowId: beta.workflowId, generation: beta.generation,
        recipient: betaDetail.supervisor.name, body: 'beta survives alpha cancel',
      }),
      beta, betaDetail.supervisor.name, 'delivered', 'beta after alpha cancel'
    );
    await workflowRpc.call({ type: 'workflow_remove', workflowId: alpha.workflowId });
    await waitFor(
      page,
      (workflowId) => document.querySelector(`[data-workflow-id="${workflowId}"]`) === null,
      'alpha workflow row remained after remove',
      30000,
      alpha.workflowId
    );
    record('workflow.remove');
    const betaAfterRemove = await waitForWorkflowSnapshot(
      workflowRpc, beta.workflowId, beta.name, 'beta after alpha remove'
    );
    if (betaAfterRemove.generation !== beta.generation) fail('beta generation changed after alpha remove');
    requireTypedReceipt(
      await workflowRpc.call({
        type: 'workflow_hub_send', workflowId: beta.workflowId, generation: beta.generation,
        recipient: betaDetail.supervisor.name, body: 'beta survives alpha remove',
      }),
      beta, betaDetail.supervisor.name, 'delivered', 'beta after alpha remove'
    );

    await page.fill('#workflow-create-name', 'web-e2e-workflow-integrate');
    await page.fill('#workflow-create-objective', 'integrate coverage workflow objective');
    await page.press('#workflow-create-objective', 'Enter');
    await waitFor(
      page,
      () => [...document.querySelectorAll('.workflow-row')].some((row) =>
        (row.querySelector('.workflow-row__name')?.textContent || '') === 'web-e2e-workflow-integrate'
      ),
      'workflow created via Enter never appeared',
      30000
    );
    const integrateList = await workflowRpc.call({ type: 'workflow_list' });
    const integrateCreated = (integrateList.workflows || []).find((workflow) => workflow.name === 'web-e2e-workflow-integrate');
    if (!integrateCreated?.workflowId) fail('workflow created via Enter missing from backend list');
    const integrateWorkflow = await waitForWorkflowSnapshot(
      workflowRpc, integrateCreated.workflowId, 'web-e2e-workflow-integrate', 'workflow created via Enter'
    );
    record('workflow.create-via-enter');
    await page.evaluate((workflowId) => {
      const row = document.querySelector(`[data-workflow-id="${workflowId}"]`);
      if (!row) throw new Error(`workflow row ${workflowId} missing`);
      row.click();
    }, integrateWorkflow.workflowId);
    await waitFor(page, () => document.getElementById('workflow-integrate-btn')?.disabled === true, 'live integrate workflow button unexpectedly enabled');
    await page.click('#workflow-pause-btn');
    await waitFor(
      page,
      (workflowId) => document.querySelector(`[data-workflow-id="${workflowId}"]`)?.getAttribute('data-status') === 'paused',
      'workflow never paused before integrate',
      30000,
      integrateWorkflow.workflowId
    );
    await page.click('#workflow-integrate-btn');
    const integrateDeadline = Date.now() + 30000;
    let integrateSnapshot;
    let lastIntegrate = 'snapshot missing';
    while (Date.now() < integrateDeadline) {
      try {
        const snapshot = await workflowRpc.call({ type: 'workflow_get', workflowId: integrateWorkflow.workflowId });
        if (snapshot?.workflowId !== integrateWorkflow.workflowId) {
          fail(`workflow integrate snapshot identity mismatch: ${JSON.stringify(snapshot)}`);
        }
        if (snapshot.generation !== integrateWorkflow.generation) {
          fail(`workflow integrate generation changed: ${JSON.stringify(snapshot)}`);
        }
        if (['completed', 'conflicted', 'failed'].includes(snapshot.status)) {
          if (snapshot.status === 'completed') {
            if (
              typeof snapshot.integration !== 'string' ||
              !snapshot.integration.startsWith('applied:') ||
              snapshot.integration.length <= 'applied:'.length
            ) {
              fail(`workflow integrate completed without applied commit: ${JSON.stringify(snapshot)}`);
            }
            if (snapshot.failure) {
              fail(`workflow integrate completed with failure: ${JSON.stringify(snapshot)}`);
            }
          } else if (snapshot.status === 'conflicted') {
            if (
              typeof snapshot.integration !== 'string' ||
              !snapshot.integration.startsWith('conflicted:') ||
              snapshot.integration.length <= 'conflicted:'.length
            ) {
              fail(`workflow integrate conflicted without conflict details: ${JSON.stringify(snapshot)}`);
            }
          } else if (typeof snapshot.failure !== 'string' || snapshot.failure === '') {
            fail(`workflow integrate failed without failure: ${JSON.stringify(snapshot)}`);
          }
          integrateSnapshot = snapshot;
          break;
        }
        lastIntegrate = JSON.stringify(snapshot);
      } catch (err) {
        fail(`workflow integrate RPC failed: ${err instanceof Error ? err.message : String(err)}`);
      }
      await new Promise((resolve) => setTimeout(resolve, 150));
    }
    if (!integrateSnapshot) fail(`workflow integrate never reached a terminal backend status: ${lastIntegrate}`);
    await waitFor(
      page,
      (expected) => document.querySelector(`[data-workflow-id="${expected.workflowId}"]`)?.getAttribute('data-status') === expected.status,
      'workflow integrate UI row did not project the terminal backend status',
      30000,
      { workflowId: integrateSnapshot.workflowId, status: integrateSnapshot.status }
    );
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

    // ============ Phase H2: slash command dispatch ============
    // Web-supported slash surface (commands.ts parseSupportedCommand +
    // slashDispatch.ts resolveSlashAction): /compact (llm RPC), /compact
    // --snap, /skill, /code-review (panel), /code-review <from> <to>
    // (range args), /code-review <one arg> (base→working tree),
    // /code-review a b c (usage toast), and an unknown /slash that
    // falls through as a NORMAL prompt (user bubble + assistant reply).
    // Every intercepted command renders a labeled summary bubble (or the
    // usage toast) — never an optimistic user bubble.
    const summaryIndexOf = (label) =>
      page.evaluate((want) => {
        const labels = [...document.querySelectorAll('#transcript .msg--summary__label')].map((n) => n.textContent || '');
        const texts = [...document.querySelectorAll('#transcript .msg--summary__text')].map((n) => n.textContent || '');
        const idx = labels.lastIndexOf(want);
        return { idx, hasText: idx >= 0 ? (texts[idx] || '').trim() !== '' : false };
      }, label);
    // /compact --snap -> snapcompact RPC -> 'snapcompact' summary bubble.
    let summaryBefore = await page.evaluate(() => document.querySelectorAll('#transcript .msg--summary').length);

    // /compact --snap -> snapcompact RPC -> 'snapcompact' summary bubble.
    await page.fill('#prompt-input', '/compact --snap');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (b) => {
        const labels = [...document.querySelectorAll('#transcript .msg--summary__label')].map((n) => n.textContent || '');
        const texts = [...document.querySelectorAll('#transcript .msg--summary__text')].map((n) => n.textContent || '');
        const idx = labels.lastIndexOf('snapcompact');
        return idx >= b && (texts[idx] || '').trim() !== '';
      },
      'slash: /compact --snap never rendered a snapcompact summary bubble',
      30000,
      summaryBefore
    );
    const snapSummary = await page.evaluate(() => {
      const labels = [...document.querySelectorAll('#transcript .msg--summary__label')].map((n) => n.textContent || '');
      const texts = [...document.querySelectorAll('#transcript .msg--summary__text')].map((n) => n.textContent || '');
      const idx = labels.lastIndexOf('snapcompact');
      return idx >= 0 ? texts[idx] : '';
    });
    if (!/Snapcompact/.test(snapSummary) && !/snapcompact/.test(snapSummary)) {
      fail(`slash: snapcompact bubble text does not name the report: "${snapSummary}"`);
    }
    record('slash.snapcompact');

    // /compact <instructions> -> llm compact RPC -> 'compact' summary bubble.
    summaryBefore = await page.evaluate(() => document.querySelectorAll('#transcript .msg--summary').length);
    await page.fill('#prompt-input', '/compact reword the session summary');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (b) => {
        const labels = [...document.querySelectorAll('#transcript .msg--summary__label')].map((n) => n.textContent || '');
        const texts = [...document.querySelectorAll('#transcript .msg--summary__text')].map((n) => n.textContent || '');
        const idx = labels.lastIndexOf('compact');
        return idx >= b && (texts[idx] || '').trim() !== '';
      },
      'slash: /compact <instructions> never rendered a compact summary bubble',
      30000,
      summaryBefore
    );
    const compactSummary = await page.evaluate(() => {
      const labels = [...document.querySelectorAll('#transcript .msg--summary__label')].map((n) => n.textContent || '');
      const texts = [...document.querySelectorAll('#transcript .msg--summary__text')].map((n) => n.textContent || '');
      const idx = labels.lastIndexOf('compact');
      return idx >= 0 ? texts[idx] : '';
    });
    if (!/Compact/.test(compactSummary) && !/compact/.test(compactSummary)) {
      fail(`slash: compact bubble text does not name the report: "${compactSummary}"`);
    }
    record('slash.compact-llm');

    // Bare /compact -> llm compact with EMPTY instructions (covers
    // isSnapCompactArgs's empty-args branch: !trimmed -> false).
    summaryBefore = await page.evaluate(() => document.querySelectorAll('#transcript .msg--summary').length);
    await page.fill('#prompt-input', '/compact');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (b) => {
        const labels = [...document.querySelectorAll('#transcript .msg--summary__label')].map((n) => n.textContent || '');
        const texts = [...document.querySelectorAll('#transcript .msg--summary__text')].map((n) => n.textContent || '');
        const idx = labels.lastIndexOf('compact');
        return idx >= b && (texts[idx] || '').trim() !== '';
      },
      'slash: bare /compact never rendered a compact summary bubble',
      30000,
      summaryBefore
    );
    record('slash.compact-bare');

    // /compact --snap <trailing> -> snap path via the leading-flag regex
    // (covers the /^--snap(?:\s|$)/ branch; trailing text is ignored).
    summaryBefore = await page.evaluate(() => document.querySelectorAll('#transcript .msg--summary').length);
    await page.fill('#prompt-input', '/compact --snap ignore the tail');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (b) => {
        const labels = [...document.querySelectorAll('#transcript .msg--summary__label')].map((n) => n.textContent || '');
        const texts = [...document.querySelectorAll('#transcript .msg--summary__text')].map((n) => n.textContent || '');
        const idx = labels.lastIndexOf('snapcompact');
        return idx >= b && (texts[idx] || '').trim() !== '';
      },
      'slash: /compact --snap <tail> never rendered a snapcompact summary bubble',
      30000,
      summaryBefore
    );
    record('slash.snapcompact-tail');

    // Bare /skill -> usage error toast (no summary bubble, no user bubble).
    const userBubblesBeforeSkillError = await page.evaluate(() => document.querySelectorAll('.msg--user').length);
    await page.fill('#prompt-input', '/skill');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.toast--error')).some((t) =>
          t.textContent.includes('usage: /skill')
        ),
      'slash: bare /skill never produced the usage error toast',
      10000
    );
    const userBubblesAfterSkillError = await page.evaluate(() => document.querySelectorAll('.msg--user').length);
    if (userBubblesAfterSkillError !== userBubblesBeforeSkillError) {
      fail(`slash: bare /skill created an optimistic user bubble (+${userBubblesAfterSkillError - userBubblesBeforeSkillError}) — usage errors must not dispatch`);
    }
    record('slash.skill-usage-error');

    // /skill <name> -> skill RPC -> 'skill' summary bubble (result or a
    // visible dispatch error — the backend skill catalog has no 'audit').
    summaryBefore = await page.evaluate(() => document.querySelectorAll('#transcript .msg--summary').length);
    await page.fill('#prompt-input', '/skill audit');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (b) => {
        const labels = [...document.querySelectorAll('#transcript .msg--summary__label')].map((n) => n.textContent || '');
        const texts = [...document.querySelectorAll('#transcript .msg--summary__text')].map((n) => n.textContent || '');
        const idx = labels.lastIndexOf('skill');
        return idx >= b && (texts[idx] || '').trim() !== '';
      },
      'slash: /skill audit never rendered a skill summary bubble',
      30000,
      summaryBefore
    );
    record('slash.skill-rpc');

    // /code-review -> opens the review panel; close via its own Close button.
    await page.fill('#prompt-input', '/code-review');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') !== null,
      'slash: /code-review never opened the review panel',
      30000
    );
    record('slash.code-review-open');
    await page.click('.code-review__close.panel-close');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') === null,
      'slash: code-review panel never closed via its Close button'
    );
    record('slash.code-review-close');

    // /code-review <from> <to> -> panel opens with the range args.
    await page.fill('#prompt-input', '/code-review HEAD~1 HEAD');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') !== null,
      'slash: /code-review range never opened the review panel',
      30000
    );
    record('slash.code-review-range');
    await page.click('.code-review__close.panel-close');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') === null,
      'slash: code-review range panel never closed'
    );
    // /code-review <one arg> -> panel opens as base→working tree.
    await page.fill('#prompt-input', '/code-review HEAD');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') !== null,
      'slash: one-arg /code-review never opened the review panel',
      30000
    );
    record('slash.code-review-base');
    await page.click('.code-review__close.panel-close');
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') === null,
      'slash: one-arg code-review panel never closed'
    );

    // /code-review a b c -> usage error toast (zero, one, or two revisions).
    await page.fill('#prompt-input', '/code-review a b c');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.toast--error')).some((t) =>
          t.textContent.includes('usage: /code-review')
        ),
      'slash: three-arg /code-review never produced the usage error toast',
      10000
    );
    record('slash.code-review-usage-error');

    // Unknown /slash falls through as a NORMAL prompt (no interception):
    // a user bubble with the raw text + the mock's followup assistant reply.
    const userBubblesBeforeFallthrough = await page.evaluate(() => document.querySelectorAll('.msg--user').length);
    await page.fill('#prompt-input', '/definitely-not-a-command');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (b) => {
        const users = [...document.querySelectorAll('.msg--user')];
        return users.length > b && (users[users.length - 1]?.textContent || '').includes('/definitely-not-a-command');
      },
      'slash: unknown /slash did not fall through as a normal user prompt',
      30000,
      userBubblesBeforeFallthrough
    );
    record('slash.unknown-falls-through');
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message',
      'slash: unknown-/slash prompt never finished streaming before the next phase',
      30000
    );

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
    // The header no longer duplicates the session name beside the model
    // selector: the rename proves itself via the Session panel value above
    // and the sidebar row title below; the header must never render
    // `session: <name>` again (desktop and mobile).
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
    record('session.rename-panel-sidebar');
    record('nav.header-no-duplicate-session');
    await page.screenshot({ path: `${evidence}/session-renamed.png`, fullPage: true });
    await waitFor(
      page,
      () => document.querySelectorAll('.session-sidebar__row').length >= 1,
      'no saved sessions listed in the session sidebar',
    );
    record('session.history-listed');
    await assertSidebarNewActionAbsent(page, 'session coverage: obsolete sidebar New action rendered');
    const msgsBeforeSwitch = await page.$$eval('#transcript .msg', (els) => els.length);
    if (msgsBeforeSwitch === 0) fail('no transcript messages before the switch — the clear would be unobservable');
    await createFreshSessionViaManage(page, waitFor, 'session coverage', {
      closePanel: false,
      timeoutMs: 25000,
    });
    record('session.switch-new-id');
    await waitFor(
      page,
      () => {
        const active = document.querySelector('#transcript .transcript-surface:not([hidden])');
        if (!active) return false;
        return active.querySelectorAll('.msg').length === 0 && active.querySelector('.empty-hint') !== null;
      },
      'session switch never cleared the old transcript (empty new-session view missing)'
    );
    const staleTranscript = await page.evaluate(() => document.querySelector('#transcript .transcript-surface:not([hidden])')?.textContent || '');
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
    await page.click('.subagents-panel__filter-btn[data-filter="running"]');
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

    // ============ Phase K: side chat ============
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

    // ==================== Phase M: mobile viewport ====================
    const mobile = await browser.newPage({ viewport: { width: 375, height: 667 } });
    await connectPage(mobile);
    // The fresh page bootstraps the FULL session history (get_messages in the
    // connect bootstrap), so "any assistant text present" is true before the
    // prompt is accepted and the composer shows Send while the session is
    // still idle. Waiting on those two alone races the drawer measurements
    // into the mobile turn's stream window: with odd mock parity the reply is
    // the ~3s slow stream, and the idle-composer metrics then read
    // "Stop generating" mid-turn. Wait on a settle condition tied to THIS
    // prompt instead (mirrors the prime-turn pattern above): a NEW assistant
    // message with non-empty text, the streaming badge cleared (agent_settled
    // settled the turn), and the unified action back to Send message.
    const beforeMobile = await mobile.evaluate(
      () => document.querySelectorAll('.msg--assistant .assistant-text').length
    );
    await mobile.fill('#prompt-input', 'hello from a phone');
    await mobile.press('#prompt-input', 'Enter');
    await waitFor(
      mobile,
      (before) => {
        const nodes = [...document.querySelectorAll('.msg--assistant .assistant-text')];
        return (
          nodes.length > before &&
          (nodes[nodes.length - 1]?.textContent || '').trim() !== '' &&
          document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message'
        );
      },
      'mobile prompt never round-tripped to an idle composer',
      60000,
      beforeMobile
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
      const footer = rect('.app-main > footer');
      const ta = rect('#prompt-input');
      const targets = ['#send-btn', '#settings-toggle-btn', '#todo-toggle-btn'].map((sel) => ({
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
        actionLabel: document.getElementById('send-btn')?.getAttribute('aria-label') || '',
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
    // Idle usable composer: the unified action leaves the textarea dominant.
    if (metrics.textareaW < 240) {
      fail(`#prompt-input usable width is ${metrics.textareaW}px at 375px (must be >= 240px)`);
    }
    if (metrics.textareaH < 44) {
      fail(`#prompt-input usable height is ${metrics.textareaH}px (must be >= 44px)`);
    }
    if (metrics.actionLabel !== 'Send message') {
      fail(`mobile unified action did not return to Send: "${metrics.actionLabel}"`);
    }
    for (const t of metrics.targets) {
      if (t.height < 44) fail(`touch target ${t.sel} is ${t.height}px (must be >= 44px)`);
    }
    record('mobile.touch-targets-44');
    if (metrics.thinkingDisplay === 'none' || metrics.thinkingDisplay === 'missing') {
      fail(`#thinking-select must be visible at phone width (header has room), computed display = ${metrics.thinkingDisplay}`);
    }
    record('mobile.thinking-visible');
    // Mobile rename: same panel-positive + header-absence contract at phone
    // width. The panel toggles live inside the sidebar drawer on phones, so
    // the drawer opens first.
    await mobile.click('#sidebar-toggle-btn');
    await waitFor(
      mobile,
      () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open'),
      'mobile sidebar drawer never opened for the session panel rename'
    );
    await mobile.click('#session-toggle-btn');
    await waitFor(mobile, () => document.getElementById('session-panel') !== null, 'mobile session panel did not open');
    await mobile.fill('#session-rename-input', 'web e2e session');
    await mobile.click('#session-rename-btn');
    await waitFor(
      mobile,
      () => document.querySelector('[data-testid="session-name-value"]')?.textContent === 'web e2e session',
      'mobile session rename never reflected in the panel'
    );
    await waitFor(
      mobile,
      () => {
        const header = document.getElementById('model-select')?.closest('header');
        return document.getElementById('session-name') === null
          && header !== null
          && !header.textContent.includes('session:');
      },
      'mobile header still renders a duplicate session label after the rename'
    );
    record('mobile.header-no-duplicate-session');
    await mobile.screenshot({ path: `${evidence}/session-renamed-mobile.png`, fullPage: true });
    await mobile.click('#session-close-btn');

    // ================ Phase N: second page concurrency ================
    const second = await browser.newPage();
    await connectPage(second);
    const bothOn = await page.evaluate(() => document.getElementById('send-btn')?.hasAttribute('disabled') !== true);
    if (!bothOn) fail('first page dropped its connection while the second page connected');
    record('concurrency.second-page-connects');
    // The first page remains connected while the second page joins.

    // ============ Phase O: desktop panel navigation ============
    // nav.panel-open-close: feature nav opens the unified module and its panel control closes it.
    await page.click('#todo-toggle-btn');
    await waitFor(page, () => {
      const panels = document.querySelectorAll('#todo-panel');
      const panel = panels.length === 1 ? panels[0] : null;
      return panel !== null
        && panel.querySelector('.todo-panel__tasks') !== null
        && panel.querySelector('.todo-child-section') !== null;
    }, 'Unified Todo module did not open with task and child sections (nav)');
    await page.click('#todo-close-btn');
    await waitFor(page, () => document.getElementById('todo-panel') === null, 'Todo module did not close (nav)');
    record('nav.panel-open-close');

    // ---- realtime call #2: ICE-gather event path, session.update, error
    //      frame variants, dc error/close, connection-state buckets ----
    // The second call arms window.__rtStub.gatherDelay so the fake peer
    // connection starts 'gathering'; the test completes it via
    // __rtStub.finishGathering(), driving waitForIceGatheringComplete's
    // icegatheringstatechange listener + finish branches (the first call
    // took the immediate-complete shortcut).
    await page.evaluate(() => {
      window.__rtStub.gatherDelay = true;
    });
    await page.click('#mic-btn');
    await waitFor(
      page,
      (sel) => document.querySelector(sel) !== null,
      'realtime#2: overlay never appeared after the second mic click',
      30000,
      rtOverlaySel
    );
    await waitFor(
      page,
      () => window.__rtStub && window.__rtStub.pc !== null,
      'realtime#2: fake peer connection never created',
      15000
    );
    await page.evaluate(() => window.__rtStub.finishGathering());
    // The overlay exposes the live connection-state bucket once the mock
    // answer lands (classifyRealtimeConnectionState('connected')).
    await waitFor(
      page,
      () => document.getElementById('realtime-conn-state')?.dataset.state === 'connected',
      'realtime#2: overlay never showed the connected connection state',
      20000
    );
    record('realtime.conn-state-connected');
    await page.evaluate(() => window.__rtStub.openChannel());
    const sessionUpdateFrame = await page.evaluate(() => {
      const dc = window.__rtStub.dc;
      if (!dc) return null;
      for (const data of dc.sent) {
        try {
          const obj = JSON.parse(data);
          if (obj && obj.type === 'session.update') return obj;
        } catch {
          /* non-JSON frame */
        }
      }
      return null;
    });
    if (
      !sessionUpdateFrame
      || 'input' in (sessionUpdateFrame.session?.audio || {})
      || 'model' in sessionUpdateFrame.session
      || 'type' in sessionUpdateFrame.session
      || !sessionUpdateFrame.session.audio?.output?.voice
    ) {
      fail(`realtime#2: session.update frame is not AVAS-compatible: ${JSON.stringify(sessionUpdateFrame)}`);
    }
    record('realtime.session-update');
    // Error-frame variants hit realtimeErrorMessage's fallback branches:
    // message-only / code-only / bare (no detail).
    await page.evaluate(() => window.__rtStub.sendFrame({ type: 'error', message: 'bare message detail' }));
    await waitFor(
      page,
      () => document.body.textContent.includes('realtime error: bare message detail'),
      'realtime#2: message-only error frame never toasted',
      10000
    );
    record('realtime.error-message-only');
    await page.evaluate(() => window.__rtStub.sendFrame({ type: 'error', code: 'E-9' }));
    await waitFor(
      page,
      () => document.body.textContent.includes('realtime error [E-9]'),
      'realtime#2: code-only error frame never toasted',
      10000
    );
    record('realtime.error-code-only');
    await page.evaluate(() => window.__rtStub.sendFrame({ type: 'error' }));
    await waitFor(
      page,
      () => document.body.textContent.includes('realtime session error'),
      'realtime#2: bare error frame never produced the default toast',
      10000
    );
    record('realtime.error-bare');
    // Data-channel error -> bounded toast; the call stays up.
    await page.evaluate(() => {
      const dc = window.__rtStub.dc;
      if (dc && dc.onerror) dc.onerror({});
    });
    await waitFor(
      page,
      () => document.body.textContent.includes('realtime data channel error'),
      'realtime#2: data channel error never toasted',
      10000
    );
    record('realtime.dc-error');
    // 'disconnected' connection state (transient ICE loss): interrupt toast,
    // call stays UP.
    await page.evaluate(() => {
      const pc = window.__rtStub.pc;
      pc.connectionState = 'disconnected';
      if (pc.onconnectionstatechange) pc.onconnectionstatechange({});
    });
    await waitFor(
      page,
      () => document.body.textContent.includes('realtime connection interrupted'),
      'realtime#2: disconnected state never toasted the interrupt notice',
      10000
    );
    const overlayAfterDisconnected = await page.evaluate(
      (sel) => document.querySelector(sel) !== null,
      rtOverlaySel
    );
    if (!overlayAfterDisconnected) fail('realtime#2: disconnected state tore the call down (must stay up)');
    record('realtime.conn-disconnected');
    // 'failed' connection state (terminal): toast + teardown (overlay closes).
    await page.evaluate(() => {
      const pc = window.__rtStub.pc;
      pc.connectionState = 'failed';
      if (pc.onconnectionstatechange) pc.onconnectionstatechange({});
    });
    await waitFor(
      page,
      () => document.body.textContent.includes('realtime connection failed'),
      'realtime#2: failed state never toasted the terminal failure',
      10000
    );
    await waitFor(
      page,
      (sel) => document.querySelector(sel) === null,
      'realtime#2: failed state never tore the overlay down',
      15000,
      rtOverlaySel
    );
    record('realtime.conn-failed-teardown');


    // ==================== Phase P: transcript + tool-title wire branches ====================
    // Real-wire typed entries driven through the EXISTING helpers only — the
    // raw second-session RPC client (Bash RPC -> bash_execution_end ->
    // .msg--bash) and the steering mock's content-routed tool calls
    // (tool_execution_start/end -> .tool-card with the generic fallback view).
    // No WebSocket wrapping, no host switching: every frame is produced by the
    // REAL backend and observed in the REAL page DOM.
    const wireActiveSid = await page.evaluate(
      () => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
    );
    if (!wireActiveSid) fail('no active session id found in the sidebar for the wire branch phase');

    // ---- bash_execution_end variants (real Bash RPC -> .msg--bash) ----
    // NOTE: waitFor evaluates the predicate in the PAGE context — wanted
    // command/status values MUST travel via the `arg` parameter, never as
    // closures over Node-scope variables (they are undefined in-page). The
    // predicate receives `arg` as its first (and only) parameter.
    const bashCardWith = (want) => {
      const cards = [...document.querySelectorAll('.msg--bash')];
      const card = cards[cards.length - 1];
      if (!card) return false;
      const cmd = card.querySelector('.bash-command-text')?.textContent || '';
      return cmd === want.command && card.getAttribute('data-bash-status') === want.status;
    };
    await rpc.call({ type: 'bash', sessionId: wireActiveSid, command: 'echo wire-bash-ok', excludeFromContext: false });
    await waitFor(
      page,
      bashCardWith,
      'wire: exit-0 bash never rendered a done .msg--bash card',
      30000,
      { command: 'echo wire-bash-ok', status: 'done' }
    );
    record('wire.bash-done');
    await rpc.call({ type: 'bash', sessionId: wireActiveSid, command: 'true', excludeFromContext: false });
    await waitFor(
      page,
      bashCardWith,
      'wire: empty-output bash never rendered its done card',
      30000,
      { command: 'true', status: 'done' }
    );
    record('wire.bash-empty');
    await rpc.call({ type: 'bash', sessionId: wireActiveSid, command: 'exit 7', excludeFromContext: false });
    await waitFor(
      page,
      (want) => {
        const cards = [...document.querySelectorAll('.msg--bash')];
        const card = cards[cards.length - 1];
        if (!card) return false;
        const cmd = card.querySelector('.bash-command-text')?.textContent || '';
        return cmd === want.command && card.getAttribute('data-bash-status') === 'error' && card.classList.contains('msg--bash--error');
      },
      'wire: non-zero bash exit never rendered an error .msg--bash card',
      30000,
      { command: 'exit 7' }
    );
    record('wire.bash-error');
    // 25 output lines must fold to the last 10 with the "… 15 more lines"
    // leading hint (BASH_OUTPUT_LINE_LIMIT tail bound, boundOutput).
    await rpc.call({ type: 'bash', sessionId: wireActiveSid, command: 'seq 1 25', excludeFromContext: false });
    await waitFor(
      page,
      (want) => {
        const cards = [...document.querySelectorAll('.msg--bash')];
        const card = cards[cards.length - 1];
        if (!card) return false;
        const cmd = card.querySelector('.bash-command-text')?.textContent || '';
        const text = card.textContent || '';
        return cmd === want.command && text.includes('15 more lines') && text.includes('25');
      },
      'wire: long bash output never rendered bounded to the 10-line tail',
      30000,
      { command: 'seq 1 25' }
    );
    record('wire.bash-bound-tail');

    // ---- toolTitle + generic tool-card branches (real tool_execution_* dispatch) ----
    // Each marker makes the backend dispatch an UNKNOWN tool whose wire name
    // exercises humanToolTitle: snake_case Title-Case, whole-word acronym
    // preservation (IRC/RPC), kebab-case Title-Case with acronyms (URL/HTTP),
    // and credential redaction BEFORE title-casing (sk-* -> [REDACTED]).
    // The deterministic "Tool <name> not found" error settles every card.
    const wireTitleJourney = async (marker, toolName, expectedTitle, id) => {
      await waitFor(
        page,
        () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message',
        `wire: session not idle before ${toolName}`,
        30000
      );
      await page.fill('#prompt-input', marker);
      await page.press('#prompt-input', 'Enter');
      await waitFor(
        page,
        (name) => {
          const cards = [...document.querySelectorAll('.tool-card')];
          const card = cards.find((c) => c.getAttribute('data-tool-name') === name);
          return !!card && card.getAttribute('data-tool-status') === 'error';
        },
        `wire: ${toolName} card never settled as error`,
        30000,
        toolName
      );
      const card = await page.evaluate((name) => {
        const cards = [...document.querySelectorAll('.tool-card')];
        const c = cards.find((node) => node.getAttribute('data-tool-name') === name);
        if (!c) return null;
        const title = c.querySelector('.tool-card__title')?.textContent || '';
        const resultEl = c.querySelector('.tool-card__output') || c.querySelector('.tool-card__result');
        return {
          title,
          name: c.getAttribute('data-tool-name') || '',
          status: c.getAttribute('data-tool-status') || '',
          result: resultEl ? resultEl.textContent || '' : '',
        };
      }, toolName);
      if (!card || card.title !== expectedTitle) {
        fail(`wire: ${toolName} card title wrong (want "${expectedTitle}", got ${JSON.stringify(card)})`);
      }
      if (card.status !== 'error' || !card.result.includes('not found')) {
        fail(`wire: ${toolName} card did not surface the dispatch error: ${JSON.stringify(card)}`);
      }
      record(id);
    };
    await wireTitleJourney('tool-title coverage snake', 'code_search', 'Code Search', 'wire.title-snake');
    await wireTitleJourney('tool-title coverage acronym', 'irc_rpc_status', 'IRC RPC Status', 'wire.title-acronym');
    await wireTitleJourney('tool-title coverage kebab', 'url_http_check', 'URL HTTP Check', 'wire.title-kebab');
    // The generic error card additionally proves the raw wire tool name and
    // the error status survive on data attributes while the compact args line
    // (compactToolArgs first-of command/path/pattern) renders, not raw JSON.
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.tool-card')];
        const card = cards.find((c) => c.getAttribute('data-tool-name') === 'code_search');
        return (
          !!card &&
          card.querySelector('.tool-card__summary-line') !== null &&
          card.querySelector('.tool-card__output') !== null
        );
      },
      'wire: generic error card never rendered the compact summary line + output',
      10000
    );
    const genericCard = await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.tool-card')];
      const card = cards.find((c) => c.getAttribute('data-tool-name') === 'code_search');
      if (!card) return null;
      const raw = card.querySelector('.tool-card__raw');
      const argsEl = card.querySelector('.tool-card__args');
      return {
        name: card.getAttribute('data-tool-name') || '',
        status: card.getAttribute('data-tool-status') || '',
        summary: card.querySelector('.tool-card__summary-line')?.textContent || '',
        rawOpen: !!raw && raw.hasAttribute('open'),
        argsJson: argsEl ? argsEl.textContent || '' : '',
      };
    });
    if (!genericCard || genericCard.name !== 'code_search' || genericCard.status !== 'error') {
      fail(`wire: generic card metadata wrong: ${JSON.stringify(genericCard)}`);
    }
    if (genericCard.summary !== 'rpi') fail(`wire: compact args summary wrong: ${JSON.stringify(genericCard.summary)}`);
    if (genericCard.rawOpen) fail('wire: generic card raw args must default to collapsed');
    if (!genericCard.argsJson.includes('"pattern"') || !genericCard.argsJson.includes('"rpi"')) {
      fail(`wire: generic card raw args lost the tool arguments: ${JSON.stringify(genericCard.argsJson)}`);
    }
    record('wire.tool-error-card');

    // ---- transcript coverage: real Todo tool card (structured view) ----
    // The steering mock routes this marker to a real `todo` tool call (op
    // init, two phases). The runtime executes the REAL todo tool, so the
    // transcript renders the structured Todo card: the running frame
    // projects the init args (resolveTodoCardView -> todoPhasesFromInitArgs)
    // and the settled frame projects the result details.phases
    // (parseTodoPhases). Both must surface the phase/task list with typed
    // status markers — never the raw args JSON as the default view.
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message',
      'wire: session not idle before the todo card journey',
      30000
    );
    await page.fill('#prompt-input', 'tool-card coverage todo init');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.tool-card')];
        const card = cards.find((c) => c.getAttribute('data-tool-name') === 'todo');
        return !!card && card.querySelector('.tool-card__todo-phase') !== null;
      },
      'wire: todo tool card never rendered its phase list',
      30000
    );
    record('wire.todo-card-running');
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.tool-card')];
        const card = cards.find((c) => c.getAttribute('data-tool-name') === 'todo');
        return !!card && card.getAttribute('data-tool-status') === 'done';
      },
      'wire: todo tool card never reached the done state',
      30000
    );
    const todoCard = await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.tool-card')];
      const card = cards.find((c) => c.getAttribute('data-tool-name') === 'todo');
      if (!card) return null;
      return {
        status: card.getAttribute('data-tool-status') || '',
        title: card.querySelector('.tool-card__name')?.textContent || '',
        phases: [...card.querySelectorAll('.tool-card__todo-phase')].map((p) => ({
          name: p.querySelector('.tool-card__todo-phase-name')?.textContent || '',
          tasks: [...p.querySelectorAll('.tool-card__todo-task')].map((t) => ({
            content: t.querySelector('.tool-card__todo-content')?.textContent || '',
            status: t.getAttribute('data-status') || '',
          })),
        })),
      };
    });
    if (!todoCard || todoCard.status !== 'done' || todoCard.title !== 'Todo') {
      fail(`wire: todo card metadata wrong: ${JSON.stringify(todoCard)}`);
    }
    const todoPhaseNames = (todoCard.phases || []).map((p) => p.name);
    if (!todoPhaseNames.includes('Plan') || !todoPhaseNames.includes('Build')) {
      fail(`wire: todo card phases missing: ${JSON.stringify(todoPhaseNames)}`);
    }
    const todoTaskStatuses = (todoCard.phases || []).flatMap((p) => p.tasks.map((t) => t.status));
    if (
      todoTaskStatuses.length < 2 ||
      !todoTaskStatuses.every((s) => ['pending', 'in_progress', 'completed', 'abandoned'].includes(s))
    ) {
      fail(`wire: todo card task statuses wrong: ${JSON.stringify(todoTaskStatuses)}`);
    }
    record('wire.todo-structured-view');
    // ---- evidence ----
    // Exact per-ID ledger gate BEFORE any write: every documented id must
    // have executed EXACTLY once, nothing undocumented may execute, and the
    // published executed array is the documented order (the matrix requires
    // ordered equality — never a sorted-set comparison).
    fs.mkdirSync(evidence, { recursive: true });
    const missing = DOCUMENTED_IDS.filter((id) => !executed.has(id));
    if (missing.length > 0) {
      fail(`coverage evidence incomplete: ${missing.join(', ')} never executed (coverage matrix contract)`);
    }
    const overExecuted = [...executed.keys()].filter((id) => !DOCUMENTED_IDS.includes(id));
    if (overExecuted.length > 0) {
      fail(`coverage evidence drift: executed-but-undocumented ids: ${overExecuted.join(', ')}`);
    }
    const repeatedIds = [...executed].filter(([, count]) => count !== 1).map(([id]) => id);
    if (repeatedIds.length > 0) {
      fail(`coverage evidence exact-once violation: ${repeatedIds.join(', ')}`);
    }
    // Publish the REAL ledger: executed is the Map's insertion order (actual
    // execution order — never a reconstruction of DOCUMENTED_IDS) and counts
    // the real per-ID ledger. The gate above proved documented == executed
    // as exact-once sets; the matrix's ordered gate additionally proves the
    // runtime order matches the documented order.
    fs.writeFileSync(path.join(evidence, 'coverage-assertions.json'), JSON.stringify({
      documented: [...DOCUMENTED_IDS],
      executed: [...executed.keys()],
      counts: Object.fromEntries(executed),
    }, null, 2));
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
