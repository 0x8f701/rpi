// Web multi-session E2E lane — PLAYWRIGHT-ONLY (E2E.d/web/sessions.sh).
//
// Environment:
//   RPI_URL         http://127.0.0.1:<port>/web
//   RPI_TOKEN       token file content (served via rpi-auth.<token> subprotocol)
//   RPI_CHROME      executable path of the system Chrome (optional)
//   RPI_EVIDENCE    evidence dir for screenshots + executed-assertion evidence
//
// REQUIRES the MultiSessionRuntimeManager backend: top-level `sessionId`
// command routing, lifecycle responses `{sessionId,state,messages}`, every
// event tagged with the owning `sessionId`, the bounded loaded pool
// (MAX_LOADED_RUNTIMES from lib/p0_resource_driver.mjs — one test constant
// matching the backend's MAX_LOADED_SESSIONS=16) with idle LRU replacement of
// one non-primary runtime, and the `loaded` overlay on session_list rows. The
// lane FAILS (exit 2) whenever any assertion fails; there is no agent-browser
// fallback and no skip.
//
// Mock scenario: `sessions` (E2E.d/lib/user_mock_server.py) — replies are
// routed by exact prompt text; `sessions-slow-a*` and `sessions-slow-b` are
// slow streams, anything else is an instant echo.
//
// Assertion matrix (feature -> IDs, used by the coverage report):
//   T0  boot + token connect + primary session row
//   T1  T1.1 slow A stream starts
//       T1.2 New while A streams -> B active with empty new-session view
//       T1.3 B prompt round-trips while A keeps running
//       T1.4 A background events bump A's unread badge (B stays 0)
//       T1.5 switch back to A: full stream text, unread cleared
//       T1.6 A authoritative transcript restore (history preserved)
//   T3  T3.1 failed switch preserves composer chips and intake budget
//       T3.2 successful switch clears chips and releases intake budget
//   T2  T2.1 second slow stream on A
//       T2.2 New -> C; C slow stream starts
//       T2.3 abort C -> neutral "run aborted" toast
//       T2.4 aborted C text never contains the final tail chunks
//       T2.5 A unaffected: completes fully after C's abort
//   T6  T6.1 header has NO feature buttons (they live in the sidebar nav)
//       T6.2 desktop collapse -> compact rail with reopen control
//       T6.3 rail reopen restores the sidebar
//       T6.4 sidebar Manage opens the session panel
//   T4  T4.1 fill the bounded 16-runtime pool
//       T4.2 create past the cap succeeds by retiring the exact expected idle LRU
//       T4.3 loaded count stays 16 and the retired runtime remains durable in catalog
//   T5  T5.1 Todo state never leaks A<->B
//       T5.2 Goal state never leaks A<->B
//       T5.3 Workflow state never leaks A<->B
//   T7  T7.4 mobile 390x844 header has NO feature buttons (same nav contract)
//       T7.1 Android 390x844: hamburger visible, drawer opens
//       T7.2 failed pick keeps the drawer/current session/error; successful pick closes
//       T7.3 picked session's transcript restores from the backend; the pick
//       preserves the mobile page's own pre-pick catalog order (no cross-page baseline)
//   T8  T8.1 fork draft lands in composer; parent→fork round-trip restores it before reload
//       T8.2 refresh keeps one durable fork row + identity and child ancestry/exclusion
//       T8.3 after refresh, parent and fork remain independently selectable (ancestry only)
//
// Unrecorded hard-fail contracts (same exit-2 gate, no coverage-matrix ID):
//   * lifecycle attachment isolation on the real surface: the New cut's
//     source A keeps its exact chips + unsent draft in its view slot while
//     the fresh target's composer mounts FULLY EMPTY (zero chips, zero
//     draft), and returning to A restores the owner's exact chip names/order
//     and draft; the Fork cut's source B keeps its chip + unsent draft while
//     the fork child carries ONLY the fork editorText (zero chips, never the
//     source draft), and returning to B restores both exactly
//
// Recording discipline: each complete T<x>.<y> contract records its
// machine-readable ID EXACTLY ONCE via an explicit record(id) at its final
// assertion site — never implicitly from waitFor/clickRow labels (a wait is
// one step inside a contract, not a passing assertion). On full success the
// lane writes $RPI_EVIDENCE/coverage-assertions.json
// ({documented, executed, counts}) from the REAL Map ledger in actual
// execution order. The Web coverage matrix
// (E2E.d/web/coverage_matrix.mjs, feature "multi-session") requires ALL of
// DOCUMENTED_IDS below IN EXACTLY THIS ORDER: the matrix fails when any
// named contract is absent or reordered, and this lane fails — before
// writing evidence — unless every documented ID executed exactly once in
// exactly the documented order, so the 10th lane is quantitatively gated,
// never just a shell PASS.

import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';
import { MAX_LOADED_RUNTIMES } from './lib/p0_resource_driver.mjs';
import {
  assertSidebarNewActionAbsent,
  closeManageSession,
  createFreshSessionViaManage,
  openManageSession,
} from './lib/session-lifecycle.mjs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const mockPort = process.env.RPI_MOCK_PORT || '';
const coveragePath = path.join(evidence, 'coverage-assertions.json');

// Machine-readable executed-assertion evidence for the coverage matrix: a
// per-ID Map ledger. record(id) is called EXPLICITLY once at each complete
// contract's final assertion site (waitFor/clickRow labels are diagnostics
// only and never record), so the Map's insertion order is the actual
// execution order. The ledger is written to
// $RPI_EVIDENCE/coverage-assertions.json only after the exact-once gate
// below has proven documented === executed === counts.
const DOCUMENTED_IDS = [
  'T0.1', 'T0.2', 'T0.3',
  'T1.1', 'T1.2', 'T1.3', 'T1.4', 'T1.5', 'T1.6',
  'T3.1', 'T3.2',
  'T2.1', 'T2.2', 'T2.3', 'T2.4', 'T2.5',
  'T6.1', 'T6.2', 'T6.3', 'T6.4',
  'T8.1', 'T8.2', 'T8.3',
  'T4.1', 'T4.2', 'T4.3',
  'T5.1', 'T5.2', 'T5.3',
  'T7.4', 'T7.1', 'T7.2', 'T7.3',
];
const executed = new Map(); // id -> exact execution count (per-ID ledger, no Set erasure)
function record(id) {
  executed.set(id, (executed.get(id) || 0) + 1);
  console.log(`[web-sess:assert] ${id}`);
}

// P0 fork-freeze latency evidence: wall-clock stage marks (T0 boot, T2 abort
// isolation, T3 attachment cutover, T4 LRU cap, T5 workflow isolation) plus
// fork DOM observations are written to $RPI_EVIDENCE/sessions-stage-timestamps.json
// for the independent latency measurement pass.
const stageMarks = {};
function markStage(key) {
  stageMarks[key] = Date.now();
}

function fail(message) {
  // Exit 2 (not 1): run.sh treats 1 as "npm install unavailable -> fall
  // back", which is FORBIDDEN for this lane. Any non-zero exit fails it.
  console.error(`web-sessions: FAIL: ${message}`);
  process.exit(2);
}

const WORKFLOW_PROGRESS_STATUSES = new Set(['queued', 'planning', 'running', 'paused', 'integrating']);
const WORKFLOW_FAILURE_STATUSES = new Set(['failed', 'cancelled', 'conflicted']);
const APPLIED_INTEGRATION = /^applied:[0-9A-Za-z][0-9A-Za-z._\/-]*$/;

function workflowFailureReason(workflow) {
  const reason = typeof workflow?.failure === 'string' && workflow.failure.trim() !== ''
    ? workflow.failure.trim()
    : typeof workflow?.integration === 'string' && workflow.integration.trim() !== ''
      ? workflow.integration.trim()
      : 'no surfaced reason';
  return reason.slice(0, 512);
}

// Pure snapshot gate: callers decide whether a terminal error is expected
// (the negative probe) or lane-fatal (the healthy workflows).
function classifyWorkflowSnapshot(workflow, expectedWorkflowId, expectedName) {
  if (!workflow || typeof workflow !== 'object') {
    return { kind: 'terminal-error', message: `snapshot is not an object: ${JSON.stringify(workflow)}` };
  }
  const returnedWorkflowId = typeof workflow.workflowId === 'string' ? workflow.workflowId : workflow.id;
  if (returnedWorkflowId !== expectedWorkflowId) {
    return { kind: 'terminal-error', message: `workflowId mismatch: expected ${expectedWorkflowId}, got ${JSON.stringify(returnedWorkflowId)}` };
  }
  if (workflow.name !== expectedName) {
    return { kind: 'terminal-error', message: `workflow name mismatch: expected ${expectedName}, got ${JSON.stringify(workflow.name)}` };
  }
  if (!Number.isSafeInteger(workflow.generation) || workflow.generation < 1) {
    return { kind: 'terminal-error', message: `invalid workflow generation: ${JSON.stringify(workflow.generation)}` };
  }
  if (WORKFLOW_FAILURE_STATUSES.has(workflow.status)) {
    return { kind: 'terminal-error', message: `terminal ${workflow.status}: ${workflowFailureReason(workflow)}` };
  }
  if (workflow.status === 'completed') {
    if (typeof workflow.integration !== 'string' || !APPLIED_INTEGRATION.test(workflow.integration)) {
      return { kind: 'terminal-error', message: `completed workflow lacks applied:<nonempty hex/ref> integration: ${JSON.stringify(workflow.integration)}` };
    }
    return { kind: 'success' };
  }
  if (WORKFLOW_PROGRESS_STATUSES.has(workflow.status)) return { kind: 'progress' };
  return { kind: 'terminal-error', message: `unknown workflow status ${JSON.stringify(workflow.status)}` };
}

function selfCheckWorkflowClassifier() {
  const base = { workflowId: 'wf-check', name: 'check', generation: 1 };
  const cases = [
    [{ ...base, status: 'running' }, 'progress'],
    [{ ...base, status: 'completed', integration: 'applied:deadbeef' }, 'success'],
    [{ ...base, status: 'completed', integration: 'applied:' }, 'terminal-error'],
    [{ ...base, status: 'failed', failure: 'expected probe failure' }, 'terminal-error'],
    [{ ...base, workflowId: 'foreign', status: 'running' }, 'terminal-error'],
  ];
  for (const [snapshot, expectedKind] of cases) {
    const actual = classifyWorkflowSnapshot(snapshot, base.workflowId, base.name).kind;
    if (actual !== expectedKind) throw new Error(`workflow classifier self-check: expected ${expectedKind}, got ${actual}`);
  }
}

selfCheckWorkflowClassifier();

function monitorBrowserErrors(page, label) {
  const errors = [];
  page.on('pageerror', (err) => {
    errors.push(`${label} pageerror: ${err.message}`);
  });
  page.on('console', (message) => {
    if (message.type() === 'error') errors.push(`${label} console: ${message.text()}`);
  });
  page.on('websocket', (socket) => {
    socket.on('socketerror', (error) => {
      errors.push(`${label} websocket: ${String(error)}`);
    });
  });
  return errors;
}

async function waitFor(page, fn, id, timeoutMs = 30000, arg, diagnose) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    const detail = diagnose ? await page.evaluate(diagnose, arg).catch(() => null) : null;
    fail(`${id} (timeout ${timeoutMs}ms${detail === null ? '' : `; state ${JSON.stringify(detail)}`})`);
  }
  // Labels are diagnostics only: waitFor NEVER records assertion ids —
  // each contract records explicitly via exactly one record(id) at its
  // completion site.
}

async function activeTranscript(page) {
  return page.evaluate(() =>
    document.querySelector('#transcript .transcript-surface:not([hidden])')?.textContent || '');
}

async function rowIds(page) {
  return page.evaluate(() =>
    [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId || '')
  );
}

function assertStableSessionOrder(current, previous, label) {
  const previousSet = new Set(previous);
  const retainedPrevious = previous.filter((id) => current.includes(id));
  const retainedCurrent = current.filter((id) => previousSet.has(id));
  if (JSON.stringify(retainedCurrent) !== JSON.stringify(retainedPrevious)) {
    fail(`${label}: retained session relative order changed: ${JSON.stringify({ previous, current, retainedPrevious, retainedCurrent })}`);
  }
  const firstNew = current.findIndex((id) => !previousSet.has(id));
  if (firstNew !== -1 && current.slice(firstNew).some((id) => previousSet.has(id))) {
    fail(`${label}: new session rows did not append: ${JSON.stringify({ previous, current })}`);
  }
}

async function activeSessionId(page) {
  return page.evaluate(() =>
    document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
  );
}

async function clickRow(page, sid, id) {
  const isAlreadyActive = await page.evaluate((s) => {
    const row = [...document.querySelectorAll('.session-sidebar__row')]
      .find((candidate) => candidate.querySelector('.session-sidebar__switch')?.dataset.sessionId === s);
    return row?.classList.contains('session-sidebar__row--active') === true;
  }, sid);
  if (!isAlreadyActive) {
    await waitFor(
      page,
      (s) => {
        const rows = [...document.querySelectorAll('.session-sidebar__switch')];
        const row = rows.find((candidate) => candidate.dataset.sessionId === s);
        return row !== undefined && !row.disabled;
      },
      `${id}: session row ${sid} never became clickable`,
      30000,
      sid
    );
    await page.evaluate((s) => {
      const rows = [...document.querySelectorAll('.session-sidebar__switch')];
      const row = rows.find((candidate) => candidate.dataset.sessionId === s);
      row.click();
    }, sid);
  }
  await waitFor(
    page,
    (s) => {
      const row = [...document.querySelectorAll('.session-sidebar__row')]
        .find((candidate) => candidate.querySelector('.session-sidebar__switch')?.dataset.sessionId === s);
      return row?.classList.contains('session-sidebar__row--active') === true;
    },
    `${id}: session ${sid} never became active`,
    30000,
    sid
  );
}

async function rowUnread(page, sid) {
  return page.evaluate((s) => {
    const badge = document.querySelector(`.session-sidebar__unread[data-unread-for="${s}"]`);
    return badge ? Number(badge.dataset.unread || 0) : 0;
  }, sid);
}

async function waitForNewRow(page, knownIds, id, timeoutMs = 30000) {
  // The newly created session's catalog row (its recorder file exists from
  // session start; the sidebar reloads after every lifecycle op + polls).
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
  await waitFor(
    page,
    () => document.querySelector('#transcript .empty-hint') !== null,
    id,
    30000
  );
}

// Wait until the active session is user-visibly idle. Enter while a run is
// still active is mapped to steer (App.tsx), so sequential NEW prompts must
// only be issued after the unified composer action returns to Send.
async function waitForIdle(page, id, timeoutMs = 30000) {
  await waitFor(
    page,
    // Production streaming gate (App.tsx): activeStreaming flips the
    // #send-btn aria-label between Send message and Stop generating. The
    // missing element fails the gate — a torn render can never pass idle.
    () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message',
    id,
    timeoutMs
  );
}

async function promptAndWait(page, prompt, reply, id, timeoutMs = 30000) {
  // Intended as a new prompt (not mid-stream steer): establish idle first.
  await waitForIdle(page, `${id}: pre-send idle`, timeoutMs);
  await page.fill('#prompt-input', prompt);
  await page.press('#prompt-input', 'Enter');
  await waitFor(page, (r) => document.body.textContent.includes(r), id, timeoutMs, reply);
}

async function waitForBadge(page, visible, id, timeoutMs = 30000) {
  await waitFor(
    page,
    (v) =>
      document.getElementById('send-btn')?.getAttribute('aria-label') ===
      (v ? 'Stop generating' : 'Send message'),
    id,
    timeoutMs,
    visible
  );
}

async function lastAssistantText(page) {
  return page.evaluate(() => {
    const nodes = document.querySelectorAll('.msg--assistant .assistant-text');
    return nodes.length ? nodes[nodes.length - 1].textContent || '' : '';
  });
}

const FEATURE_BUTTON_IDS = [
  'chat-console-link',
  'todo-toggle-btn',
  'activity-toggle-btn',
  'goal-panel-btn',
  'workflow-toggle-btn',
  'sidechat-toggle-btn',
  'subagents-toggle-btn',
  'personas-toggle-btn',
  'session-toggle-btn',
  'loop-toggle-btn',
  'processes-toggle-btn',
  'file-explorer-toggle-btn',
  'settings-toggle-btn',
];
const LEGACY_TODO_BUTTON_IDS = ['todos-toggle-btn', 'todo-child-toggle-btn'];

// Track counting for grid-template-columns. A RENDERED grid resolves the
// used value to px tracks ("187px 187px" -> 2), but a display:none element
// — the mobile drawer nav before T7.1 opens it — keeps the DECLARED value
// unresolved ("repeat(2, minmax(0px, 1fr))"), whose naive .split(' ')
// miscounts the repeat() internals as extra columns (measured: 3 tokens
// for a real 2-column rule). Count real tracks instead: split on
// top-level whitespace only (whitespace inside parens never splits), then
// expand repeat(N, list) to N x tracks(list), so the declared mobile
// column rule is measured exactly whether the drawer is open or not.
function gridTemplateTrackCount(value) {
  const tracks = [];
  let depth = 0;
  let token = '';
  for (const ch of value) {
    if (ch === '(') depth += 1;
    if (ch === ')') depth -= 1;
    if (depth === 0 && /\s/.test(ch)) {
      if (token) tracks.push(token);
      token = '';
      continue;
    }
    token += ch;
  }
  if (token) tracks.push(token);
  return tracks.reduce((n, track) => {
    const repeated = track.match(/^repeat\((\d+),(.*)\)$/);
    return n + (repeated ? Number(repeated[1]) * gridTemplateTrackCount(repeated[2]) : 1);
  }, 0);
}
async function assertNoHeaderFeatureButtons(page, id) {
  const bad = await page.evaluate((ids) => {
    const header = document.querySelector('header');
    if (!header) return ['no header'];
    return ids.filter((btnId) => header.querySelector(`#${btnId}`) !== null);
  }, FEATURE_BUTTON_IDS);
  if (bad.length > 0) {
    fail(`${id}: header contains feature buttons: ${bad.join(', ')}`);
  }
  const missing = await page.evaluate((ids) => {
    const nav = document.querySelector('.session-sidebar__nav');
    if (!nav) return ids.slice();
    return ids.filter((btnId) => nav.querySelector(`#${btnId}`) === null);
  }, FEATURE_BUTTON_IDS);
  if (missing.length > 0) {
    fail(`${id}: sidebar nav missing feature buttons: ${missing.join(', ')}`);
  }
  const legacy = await page.evaluate((ids) => ids.filter((btnId) => document.getElementById(btnId) !== null), LEGACY_TODO_BUTTON_IDS);
  if (legacy.length > 0) fail(`${id}: legacy Todo sidebar buttons rendered: ${legacy.join(', ')}`);
  const layout = await page.evaluate(() => {
    const nav = document.querySelector('.session-sidebar__nav');
    if (!nav) return null;
    return {
      count: nav.querySelectorAll(':scope > .panel-toggle').length,
      declaredColumns: getComputedStyle(nav).gridTemplateColumns,
      maintainPresent: nav.textContent.includes('Maintain'),
    };
  });
  // The declared mobile column rule is repeat(2, minmax(0, 1fr)) in both the
  // base .session-sidebar__nav rule and the ≤720px drawer block; resolve it
  // to an exact track count so the closed drawer (display:none, unresolved
  // computed value) is held to the same 2-column contract as the desktop nav.
  const columns = layout ? gridTemplateTrackCount(layout.declaredColumns) : 0;
  if (!layout || layout.count !== FEATURE_BUTTON_IDS.length || columns !== 2 || layout.maintainPresent) {
    fail(`${id}: sidebar nav must contain exactly ${FEATURE_BUTTON_IDS.length} declared items in 2 columns with no Maintain (${JSON.stringify({ ...layout, columns })})`);
  }
  record(id); // ONE explicit record: this helper completes the whole T6.1/T7.4 contract
}

async function main() {
  if (!url) fail('RPI_URL is required');
  fs.mkdirSync(evidence, { recursive: true });
  fs.rmSync(coveragePath, { force: true });
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  let workflowRpcSequence = 0;
  try {
    const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    const desktopErrors = monitorBrowserErrors(page, 'desktop');
    // Wire observer + exact-switch failure injection:
    //   - frames[] passively records every outgoing control frame (type +
    //     owner ids, plus `name` when the frame carries one) and every
    //     response envelope (command, correlation id, ownership sessionId,
    //     success) so contracts are asserted from the real request/response
    //     surface: the exact-ready fast-switch contract (ZERO
    //     switch_session/subscribe), the exact-one-cold-RPC contract, and
    //     the T4 fixture's correlated rename round-trip.
    //   - armFailure() rewrites the NEXT switch_session frame's sessionPath to
    //     a missing file — the negotiated-path failure used by T3.1.
    //   - stripNextRowIncarnation(sid) rewrites the NEXT session_list
    //     response so `sid`'s catalog row carries NO runtimeIncarnation hint.
    //     A hint-less row is the product's fail-closed fast-gate input (the
    //     advisory hint is never trusted), so the click must negotiate on the
    //     wire — that is how T3.1's failed negotiated switch is reached after
    //     both sessions became exact-ready.
    await page.addInitScript(() => {
      const NativeWebSocket = window.WebSocket;
      const control = {
        failNext: false,
        intercepted: false,
        failLifecycleNext: false,
        lifecycleIntercepted: false,
        stripRowIncarnationFor: '',
        strippedRowIncarnation: false,
        frames: [],
        armFailure() {
          this.failNext = true;
          this.intercepted = false;
        },
        armLifecycleFailure() {
          this.failLifecycleNext = true;
          this.lifecycleIntercepted = false;
        },
        stripNextRowIncarnation(sid) {
          this.stripRowIncarnationFor = sid;
          this.strippedRowIncarnation = false;
        },
      };
      class SessionAttachmentTestWebSocket extends NativeWebSocket {
        constructor(...args) {
          super(...args);
          // Registered BEFORE the app's own message listener: a stripped
          // response is re-dispatched with the incarnation removed and the
          // original event suppressed, so the sidebar renders the hint-less
          // row while every other consumer sees untouched data.
          this.addEventListener('message', (event) => {
            if (event.__rpiInjected) return;
            if (!control.stripRowIncarnationFor) return;
            let frame;
            try {
              frame = JSON.parse(event.data);
            } catch {
              return;
            }
            if (!frame || frame.type !== 'response' || frame.success !== true) return;
            const rows = frame.data && Array.isArray(frame.data.sessions) ? frame.data.sessions : null;
            if (!rows) return;
            const index = rows.findIndex(
              (row) => row && row.sessionId === control.stripRowIncarnationFor
                && typeof row.runtimeIncarnation === 'string' && row.runtimeIncarnation !== ''
            );
            if (index < 0) return;
            control.stripRowIncarnationFor = '';
            control.strippedRowIncarnation = true;
            event.stopImmediatePropagation();
            const clone = JSON.parse(JSON.stringify(frame));
            delete clone.data.sessions[index].runtimeIncarnation;
            const fake = new MessageEvent('message', { data: JSON.stringify(clone) });
            fake.__rpiInjected = true;
            this.dispatchEvent(fake);
          });
          // Response surface: record every response envelope so a request
          // can be paired with its actual wire outcome (id correlation).
          this.addEventListener('message', (event) => {
            if (event.__rpiInjected) return;
            let frame;
            try {
              frame = JSON.parse(event.data);
            } catch {
              return;
            }
            if (!frame || frame.type !== 'response') return;
            control.frames.push({
              t: Date.now(),
              type: 'response',
              id: frame.id,
              command: frame.command,
              sessionId: frame.sessionId,
              success: frame.success === true,
            });
            if (control.frames.length > 16000) control.frames.shift();
          });
        }
        send(data) {
          let outgoing = data;
          if (typeof data === 'string') {
            try {
              const frame = JSON.parse(data);
              if (control.failNext && frame?.type === 'switch_session') {
                control.failNext = false;
                control.intercepted = true;
                outgoing = JSON.stringify({
                  ...frame,
                  sessionPath: 'missing-session-for-attachment-regression.jsonl',
                });
              } else if (control.failLifecycleNext
                && (frame?.type === 'session_fork_latest' || frame?.type === 'fork')) {
                // P0 fork-freeze probe: force the fork lifecycle RPC to fail
                // so the exact-one-outcome-toast contract is observable.
                control.failLifecycleNext = false;
                control.lifecycleIntercepted = true;
                outgoing = frame.type === 'fork'
                  ? JSON.stringify({ ...frame, sessionId: 'missing-lifecycle-probe.jsonl' })
                  : JSON.stringify({ ...frame, sessionPath: 'missing-lifecycle-probe.jsonl' });
              }
              if (frame && typeof frame.type === 'string') {
                control.frames.push({
                  t: Date.now(),
                  type: frame.type,
                  id: frame.id,
                  sessionId: frame.sessionId,
                  catalogSessionId: frame.catalogSessionId,
                  name: typeof frame.name === 'string' ? frame.name : undefined,
                });
                if (control.frames.length > 16000) control.frames.shift();
              }
            } catch {
              // Non-JSON frames pass through unchanged.
            }
          }
          return super.send(outgoing);
        }
      }
      for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
        Object.defineProperty(SessionAttachmentTestWebSocket, key, { value: NativeWebSocket[key] });
      }
      window.WebSocket = SessionAttachmentTestWebSocket;
      window.__rpiSessionAttachmentTest = control;
    });

    const workflowRpc = (sessionId, command) => {
      const id = `sessions-workflow-${++workflowRpcSequence}`;
      const wsUrl = url.replace(/^http/, 'ws').replace(/\/web\/?$/, '/ws');
      return page.evaluate(
        ({ wsUrl: target, token: authToken, command: request, sessionId: owner, id: requestId }) => new Promise((resolve, reject) => {
          const protocols = authToken ? [`rpi-auth.${authToken}`] : [];
          const ws = new WebSocket(target, protocols);
          const timer = window.setTimeout(() => {
            ws.close();
            reject(new Error(`rpc timed out: ${request.type}`));
          }, 30000);
          ws.addEventListener('open', () => ws.send(JSON.stringify({ ...request, sessionId: owner, id: requestId })));
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
            if (frame.sessionId !== owner) {
              reject(new Error(`rpc response owner mismatch: expected ${owner}, got ${JSON.stringify(frame.sessionId)}`));
              return;
            }
            if (!frame.success) {
              reject(new Error(frame.error || 'rpc failed'));
              return;
            }
            const data = frame.data || {};
            if (typeof request.workflowId === 'string') {
              const returnedWorkflowId = request.type === 'workflow_detail' ? data.id : data.workflowId;
              if (returnedWorkflowId !== request.workflowId) {
                reject(new Error(`rpc workflowId mismatch: expected ${request.workflowId}, got ${JSON.stringify(returnedWorkflowId)}`));
                return;
              }
            }
            if (request.type === 'workflow_create' && (typeof data.workflowId !== 'string' || data.workflowId === '')) {
              reject(new Error('workflow_create response omitted workflowId'));
              return;
            }
            resolve(data);
          });
          ws.addEventListener('error', () => {
            window.clearTimeout(timer);
            reject(new Error(`rpc transport failed: ${request.type}`));
          });
        }),
        { wsUrl, token, command, sessionId, id }
      );
    };


    const validateCompletedWorkflow = async (sessionId, workflow, expectedName, label) => {
      const detail = await workflowRpc(sessionId, { type: 'workflow_detail', workflowId: workflow.workflowId });
      if (detail.id !== workflow.workflowId || detail.name !== expectedName
        || detail.generation !== workflow.generation || detail.status !== 'completed') {
        fail(`${label}: completed workflow detail identity/status mismatch: ${JSON.stringify(detail)}`);
      }
      const tasks = Array.isArray(detail.todo) ? detail.todo.flatMap((phase) => Array.isArray(phase.tasks) ? phase.tasks : []) : [];
      if (tasks.length !== 2) fail(`${label}: completed DAG must contain exactly two tasks: ${JSON.stringify(tasks)}`);
      const identityTag = `[workflowId=${workflow.workflowId};name=${expectedName === 'a-wf' ? 'a-wf-objective' : 'b-wf-objective'}]`;
      const compile = tasks.find((task) => task.content === `compile widget ${identityTag}`);
      const ship = tasks.find((task) => task.content === `ship widget ${identityTag}`);
      if (!compile || !ship) fail(`${label}: expected compile/ship tasks missing: ${JSON.stringify(tasks)}`);
      if (compile.status !== 'completed' || ship.status !== 'completed') fail(`${label}: terminal DAG has incomplete tasks: ${JSON.stringify(tasks)}`);
      if (!Array.isArray(ship.dependsOn) || ship.dependsOn.length !== 1 || ship.dependsOn[0] !== compile.id) {
        fail(`${label}: ship dependency did not resolve to compile task identity: ${JSON.stringify(tasks)}`);
      }
      if (tasks.some((task) => task.status === 'abandoned' || !task.id)) fail(`${label}: terminal DAG has abandoned/missing task identity`);
      const ownership = Array.isArray(workflow.ownership) ? workflow.ownership : [];
      const ownedTaskIds = new Set(ownership.map((row) => row.todoTaskId));
      if (ownership.length !== 2 || ownedTaskIds.size !== 2
        || !ownedTaskIds.has(compile.id) || !ownedTaskIds.has(ship.id)
        || ownership.some((row) => row.workflowId !== workflow.workflowId || row.generation !== workflow.generation)) {
        fail(`${label}: completed task ownership is not exact-once: ${JSON.stringify(ownership)}`);
      }
      return workflow;
    };
    const waitForSessionWorkflow = async (sessionId, workflowId, expectedName, label, expectedTerminal = false) => {
      const deadline = Date.now() + 60000;
      let last = 'snapshot missing';
      while (Date.now() < deadline) {
        let workflow;
        try {
          workflow = await workflowRpc(sessionId, { type: 'workflow_get', workflowId });
        } catch (err) {
          last = err instanceof Error ? err.message : String(err);
          await new Promise((resolve) => setTimeout(resolve, 150));
          continue;
        }
        const classification = classifyWorkflowSnapshot(workflow, workflowId, expectedName);
        if (classification.kind === 'terminal-error') {
          if (expectedTerminal) return classification;
          fail(`${label}: ${classification.message}`);
        }
        if (classification.kind === 'success') {
          if (expectedTerminal) return { kind: 'unexpected-success', message: JSON.stringify(workflow) };
          return validateCompletedWorkflow(sessionId, workflow, expectedName, label);
        }
        last = JSON.stringify(workflow);
        await new Promise((resolve) => setTimeout(resolve, 150));
      }
      if (expectedTerminal) return { kind: 'timeout', message: last };
      fail(`${label}: workflow never completed healthily: ${last}`);
    };
    const mockControl = async (path) => {
      if (!mockPort) fail('T5.3: RPI_MOCK_PORT is required for event-gated workflow proof');
      const response = await fetch(`http://127.0.0.1:${mockPort}${path}`, { method: 'POST' });
      if (!response.ok) fail(`T5.3: mock control ${path} failed with ${response.status}`);
      return response.status === 204 ? {} : response.json();
    };

    /* ---------------- T0: boot + connect ---------------- */
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'T0.1: page title missing');
    record('T0.1');
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'T0.2: WS did not reach "connected"'
    );
    record('T0.2');
    // The primary session's sidebar row (recorder file exists at boot).
    await waitFor(
      page,
      () => {
        const rows = [...document.querySelectorAll('.session-sidebar__switch')];
        return rows.length >= 1 && rows.every((r) => (r.dataset.sessionId || '') !== '');
      },
      'T0.3: primary session row never appeared in the sidebar'
    );
    const sessionA = (await rowIds(page))[0];
    if (!sessionA) fail('T0.3: could not read the primary session id');
    await assertSidebarNewActionAbsent(page, 'T0.3: obsolete sidebar New action rendered');
    record('T0.3');
    const knownIds = [sessionA];
    let catalogOrder = await rowIds(page);
    await page.screenshot({ path: `${evidence}/sessions-t0-boot.png`, fullPage: true });
    markStage('t0BootReady');

    /* ---------------- T1: slow A continues while B is active ---------------- */
    // T1.1: instant round-trip persists A, then a slow stream starts on A.
    await promptAndWait(page, 'a-prompt-0', 'sessions-reply: a-prompt-0', 'T1.1: A initial prompt never round-tripped');
    await promptAndWait(page, 'sessions-slow-a', 'sessions-slow-a-1/', 'T1.1: A slow stream never started');
    await waitForBadge(page, true, 'T1.1: A streaming badge never appeared');
    record('T1.1');

    // T1.2: New while A is mid-stream; the fresh session B shows the empty view.
    await createFreshSessionViaManage(page, waitFor, 'T1.2', { closePanel: true });
    await waitForEmptyView(page, 'T1.2: session B did not show the empty new-session view');
    const bTranscript = await activeTranscript(page);
    if (bTranscript.includes('slow-a-') || bTranscript.includes('sessions-reply: a-prompt-0')) {
      fail("T1.2: session B's empty view leaked session A's transcript");
    }
    record('T1.2');

    // T1.3: B's prompt completes while A's stream keeps running (the prompt
    // also flushes B's recorder so its sidebar row appears deterministically).
    await promptAndWait(page, 'b-prompt-1', 'sessions-reply: b-prompt-1', 'T1.3: B prompt never round-tripped while A streams');
    const bPromptTranscript = await activeTranscript(page);
    if (!bPromptTranscript.includes('sessions-reply: b-prompt-1')
      || bPromptTranscript.includes('sessions-reply: a-prompt-0')
      || bPromptTranscript.includes('slow-a-')) {
      fail("T1.3: active session B transcript was not isolated from session A");
    }
    record('T1.3');
    const sessionB = await waitForNewRow(page, knownIds, 'T1.2: session B row never appeared in the sidebar');
    knownIds.push(sessionB);
    {
      const afterB = await rowIds(page);
      assertStableSessionOrder(afterB, catalogOrder, 'T1.2: after session B');
      catalogOrder = afterB;
    }

    // T1.4: A's background events bumped A's unread badge; B (active) stays 0.
    await waitFor(
      page,
      (s) => {
        const badge = document.querySelector(`.session-sidebar__unread[data-unread-for="${s}"]`);
        return badge !== null && Number(badge.dataset.unread || 0) >= 1;
      },
      'T1.4: A background streaming never bumped A\'s unread badge',
      30000,
      sessionA
    );
    const unreadBWhileA = await rowUnread(page, sessionB);
    if (unreadBWhileA !== 0) {
      fail(`T1.4: active session B has an unread badge (${unreadBWhileA}) while it is the active view`);
    }
    record('T1.4');
    await page.screenshot({ path: `${evidence}/sessions-t1-background.png`, fullPage: true });
    await page.screenshot({ path: `${evidence}/sessions-switch-b.png`, fullPage: true });

    // T1.5/T1.6: switch back to A — full stream text restored, unread cleared.
    await clickRow(page, sessionA, 'T1.5: switch back to session A');
    await waitFor(page, (s) => document.body.textContent.includes('slow-a-done'), 'T1.5: A\'s slow stream never completed (while B was active)', 40000);
    await waitForBadge(page, false, 'T1.5: A streaming badge did not clear after the stream completed');
    if ((await rowUnread(page, sessionA)) !== 0) {
      fail("T1.5: A's unread badge never cleared after switching back");
    }
    record('T1.5');
    const aTranscript = await activeTranscript(page);
    if (!aTranscript.includes('sessions-reply: a-prompt-0') || !aTranscript.includes('slow-a-done')) {
      fail("T1.6: A's authoritative transcript was not restored (history missing)");
    }
    if (aTranscript.includes('sessions-reply: b-prompt-1')) {
      fail("T1.6: restored session A transcript leaked session B");
    }
    await page.screenshot({ path: `${evidence}/sessions-t1-restored.png`, fullPage: true });
    await page.screenshot({ path: `${evidence}/sessions-switch-a-restored.png`, fullPage: true });
    const successfulAbaState = await page.evaluate((sid) => ({
      active: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
      sidebar: document.querySelector('.session-sidebar__error')?.textContent || '',
      errorToasts: document.querySelectorAll('#toasts .toast--error').length,
      expected: sid,
    }), sessionA);
    if (successfulAbaState.active !== successfulAbaState.expected
      || successfulAbaState.sidebar !== '' || successfulAbaState.errorToasts !== 0) {
      fail(`T1.6: successful A-B-A identity/error state was invalid: ${JSON.stringify(successfulAbaState)}`);
    }
    {
      const afterAba = await rowIds(page);
      assertStableSessionOrder(afterAba, catalogOrder, 'T1.6: after A-B-A');
      catalogOrder = afterAba;
    }
    record('T1.6');
    /* ---- exact-ready fast-switch proof (repeated A-B-A after the first
     * loads): every same-socket repeated click must flip the active row and
     * transcript ownership synchronously or by the NEXT animation frame with
     * ZERO switch_session/subscribe frames, no switching marker, and no
     * error toast. Evidence lands in switch-fast-metrics.json (counts/paint
     * provenance only, written after the full matrix executes). ---- */
    const switchFastEvidence = { exactReady: [], composerRoute: null, retiredColdRpc: null };
    const runFastSwitchProof = async (target, ownerMarker, label) => {
      const from = await activeSessionId(page);
      if (from === target) fail(`${label}: pre-click active row already equals ${target}`);
      await waitFor(
        page,
        (s) => {
          const row = [...document.querySelectorAll('.session-sidebar__switch')]
            .find((candidate) => candidate.dataset.sessionId === s);
          return row !== undefined && !row.disabled;
        },
        `${label}: target row never became clickable`,
        30000,
        target
      );
      const frameBase = await page.evaluate(() => (window.__rpiSessionAttachmentTest?.frames || []).length);
      const paint = await page.evaluate(
        ({ sid, marker }) =>
          new Promise((resolve) => {
            const row = [...document.querySelectorAll('.session-sidebar__switch')]
              .find((candidate) => candidate.dataset.sessionId === sid);
            if (!row) {
              resolve({ error: 'row missing' });
              return;
            }
            const clickAt = performance.now();
            row.click();
            const sample = () => ({
              active:
                document.querySelector('.session-sidebar__row--active .session-sidebar__switch')
                  ?.dataset.sessionId || '',
              owned:
                document.getElementById('transcript')?.textContent?.includes(marker) === true,
              switching: document.querySelector('.session-sidebar__row--switching') !== null,
            });
            const sync = sample();
            window.requestAnimationFrame(() => {
              window.requestAnimationFrame(() => {
                const next = sample();
                resolve({
                  active: next.active,
                  owned: next.owned,
                  switching: next.switching,
                  paint: sync.active === sid && sync.owned ? 'sync' : 'next-frame',
                  paintMs: Math.round(performance.now() - clickAt),
                });
              });
            });
          }),
        { sid: target, marker: ownerMarker }
      );
      if (paint.error) fail(`${label}: ${paint.error}`);
      if (paint.active !== target) {
        fail(`${label}: active row was ${JSON.stringify(paint.active)} after one animation frame, expected ${target}`);
      }
      if (!paint.owned) fail(`${label}: transcript ownership never flipped within one animation frame`);
      if (paint.switching) fail(`${label}: switching marker rendered during an exact-ready switch`);
      await page.waitForTimeout(300);
      const wire = await page.evaluate(
        (from) => {
          const frames = (window.__rpiSessionAttachmentTest?.frames || []).slice(from);
          return {
            switchSession: frames.filter((frame) => frame.type === 'switch_session').length,
            subscribe: frames.filter((frame) => frame.type === 'subscribe').length,
          };
        },
        frameBase
      );
      if (wire.switchSession !== 0 || wire.subscribe !== 0) {
        fail(`${label}: exact-ready switch emitted wire frames: ${JSON.stringify(wire)}`);
      }
      if ((await page.evaluate(() => document.querySelectorAll('#toasts .toast--error').length)) !== 0) {
        fail(`${label}: exact-ready switch surfaced an error toast`);
      }
      if ((await activeSessionId(page)) !== target) fail(`${label}: active row regressed after the switch`);
      const record = { from, target, paint: paint.paint, paintMs: paint.paintMs, wire };
      switchFastEvidence.exactReady.push(record);
      return record;
    };
    const fastOwnerA = 'sessions-reply: a-prompt-0';
    const fastOwnerB = 'sessions-reply: b-prompt-1';
    await runFastSwitchProof(sessionB, fastOwnerB, 'exact-ready A→B (repeat 1)');
    {
      // Composer route proof bound to the SAME fast switch: the very next
      // prompt must ride B's command target without dispatching any
      // switch_session (and without re-routing to A).
      const routeBase = await page.evaluate(() => (window.__rpiSessionAttachmentTest?.frames || []).length);
      await promptAndWait(
        page,
        'sessions-fast-route-probe',
        'sessions-reply: sessions-fast-route-probe',
        'fast-route probe never round-tripped on B after the exact-ready switch'
      );
      const routeFrames = await page.evaluate(
        (from) => (window.__rpiSessionAttachmentTest?.frames || []).slice(from),
        routeBase
      );
      const promptFrames = routeFrames.filter((frame) => frame.type === 'prompt');
      if (promptFrames.length !== 1 || promptFrames[0].sessionId !== sessionB) {
        fail(`fast-route probe prompt ownership mismatch: ${JSON.stringify(promptFrames.map((frame) => frame.sessionId))}`);
      }
      if (routeFrames.some((frame) => frame.type === 'switch_session')) {
        fail('fast-route probe dispatched a switch_session frame');
      }
      if ((await activeSessionId(page)) !== sessionB) {
        fail('fast-route probe moved the active session away from B');
      }
      switchFastEvidence.composerRoute = { routedSessionId: sessionB, promptFrames: 1, extraSwitchFrames: 0 };
    }
    await runFastSwitchProof(sessionA, fastOwnerA, 'exact-ready B→A (repeat 2)');
    await runFastSwitchProof(sessionB, fastOwnerB, 'exact-ready A→B (repeat 3)');
    await runFastSwitchProof(sessionA, fastOwnerA, 'exact-ready B→A (repeat 4)');
    markStage('t3AttachmentsStart');

    /* ---------------- T3: composer attachments follow session cutover ---------------- */
    await page.setInputFiles('input[type=file]', Array.from({ length: 8 }, (_, index) => ({
      name: `session-a-${index + 1}.txt`,
      mimeType: 'text/plain',
      buffer: Buffer.from(`session A attachment ${index + 1}`),
    })));
    await waitFor(
      page,
      () => document.querySelectorAll('.composer-attachment').length === 8,
      'T3.1: session A attachment chips did not reach the intake cap'
    );
    // Both A and B are exact-ready on this socket by now, so a plain click
    // would take the zero-wire fast path and there would be no switch_session
    // to fail. The failed-switch contract belongs to the NEGOTIATED path:
    // strip B's advisory catalog incarnation hint (the product's documented
    // fail-closed fast-gate input — a null/stale hint is never trusted) so
    // the click below must negotiate on the wire and the sessionPath
    // interception applies.
    await page.evaluate((sid) => window.__rpiSessionAttachmentTest.stripNextRowIncarnation(sid), sessionB);
    await waitFor(
      page,
      () => window.__rpiSessionAttachmentTest?.strippedRowIncarnation === true,
      'T3.1: stale-hint strip never landed on a session_list response',
      20000
    );
    await page.evaluate(() => window.__rpiSessionAttachmentTest.armFailure());
    await page.evaluate((sid) => {
      const row = [...document.querySelectorAll('.session-sidebar__switch')]
        .find((candidate) => candidate.dataset.sessionId === sid);
      if (!row) throw new Error(`missing session row ${sid}`);
      row.click();
    }, sessionB);
    await waitFor(
      page,
      ({ currentSid, targetSid }) => {
        const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '';
        const target = [...document.querySelectorAll('.session-sidebar__switch')]
          .find((candidate) => candidate.dataset.sessionId === targetSid);
        const switchErrors = [...document.querySelectorAll('#toasts .toast--error')];
        const errorText = switchErrors[0]?.textContent || '';
        return window.__rpiSessionAttachmentTest.intercepted === true
          && active === currentSid
          && document.querySelector('.session-sidebar__error') === null
          && switchErrors.length === 1
          && errorText.includes('switch to ')
          && errorText.includes(' failed')
          && errorText.toLowerCase().includes('session')
          && target?.disabled === false
          && document.querySelectorAll('.composer-attachment').length === 8;
      },
      'T3.1: failed switch did not preserve session A chips/current session/exactly-one toast',
      20000,
      { currentSid: sessionA, targetSid: sessionB },
      ({ targetSid }) => {
        const target = [...document.querySelectorAll('.session-sidebar__switch')]
          .find((candidate) => candidate.dataset.sessionId === targetSid);
        return {
          active: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
          sidebarError: document.querySelector('.session-sidebar__error')?.textContent || '',
          errorToasts: [...document.querySelectorAll('#toasts .toast--error')].map((toast) => toast.textContent || ''),
          targetDisabled: target?.disabled,
          attachmentCount: document.querySelectorAll('.composer-attachment').length,
          intercepted: window.__rpiSessionAttachmentTest.intercepted,
        };
      }
    );
    await page.screenshot({ path: `${evidence}/sessions-failed-switch-toast.png`, fullPage: true });
    await page.setInputFiles('input[type=file]', {
      name: 'failed-switch-budget-probe.txt',
      mimeType: 'text/plain',
      buffer: Buffer.from('must be refused while the eight attachment reservations remain'),
    });
    await waitFor(
      page,
      () => [...document.querySelectorAll('#toasts .toast')].some((toast) =>
        (toast.textContent || '').includes('over 8 attachments')
      ),
      'T3.1: failed switch released the session A intake budget'
    );
    if (await page.evaluate(() => document.querySelectorAll('.composer-attachment').length !== 8)) {
      fail('T3.1: failed switch budget probe changed the preserved chip queue');
    }
    record('T3.1');
    await clickRow(page, sessionB, 'T3.2: successful switch to B clears session A composer state');
    await waitFor(
      page,
      () => document.querySelector('#composer-attachments') === null,
      'T3.2: successful switch left session A attachment chips mounted'
    );
    await page.setInputFiles('input[type=file]', {
      name: 'session-b-budget-probe.txt',
      mimeType: 'text/plain',
      buffer: Buffer.from('accepted only when the previous session budget was reset'),
    });
    if (desktopErrors.length > 0) {
      fail(`T3.2: unexpected desktop browser errors: ${desktopErrors.join(' | ')}`);
    }
    await waitFor(
      page,
      () => document.querySelectorAll('.composer-attachment').length === 1
        && (document.querySelector('.composer-attachment__name')?.textContent || '') === 'session-b-budget-probe.txt',
      'T3.2: successful switch did not release the attachment intake budget'
    );
    await page.click('.composer-attachment__remove');
    await waitFor(
      page,
      () => document.querySelector('#composer-attachments') === null,
      'T3.2: session B budget probe chip did not clear'
    );
    record('T3.2');
    markStage('t2AbortIsolationStart');

    await clickRow(page, sessionA, 'T2.1: restore A after attachment-isolation checks');
    /* ---------------- T2: abort/toast isolation ---------------- */
    await promptAndWait(page, 'sessions-slow-a2', 'sessions-slow-a2-1/', 'T2.1: A second slow stream never started');
    await waitForBadge(page, true, 'T2.1: A streaming badge missing for the second stream');
    record('T2.1');

    /* The T2.1 prompt legitimately dispatched A's eight restored chips with
     * the message (the attachments contract); re-attach the same eight so the
     * lifecycle isolation below carries the exact New-cut source view. ---- */
    await page.setInputFiles('input[type=file]', Array.from({ length: 8 }, (_, index) => ({
      name: `session-a-${index + 1}.txt`,
      mimeType: 'text/plain',
      buffer: Buffer.from(`session A attachment ${index + 1}`),
    })));
    await waitFor(
      page,
      () => document.querySelectorAll('.composer-attachment').length === 8,
      'T2.1: re-attached chips did not reach the intake cap',
      20000
    );

    /* ---- Lifecycle attachment isolation on the real surface (unrecorded
     * hard-fail contract): A's exact eight chips (restored by the switch
     * back from B) plus one UNSent draft belong to A's composer view slot.
     * The New cut below must save that view into A's slot and mount the
     * fresh target C with a FULLY EMPTY composer; the T2.5 switch back must
     * restore the owner's exact chip names in order and the exact draft. ---- */
    const expectedNewCutChips = Array.from({ length: 8 }, (_, index) => `session-a-${index + 1}.txt`);
    await waitFor(
      page,
      (expected) => {
        const names = [...document.querySelectorAll('.composer-attachment__name')].map((el) => el.textContent || '');
        return names.length === 8 && names.every((name, index) => name === expected[index]);
      },
      'lifecycle isolation: source A chips were not restored exactly after the A/B switch round trip',
      20000,
      expectedNewCutChips,
      (expected) => ({
        mounted: [...document.querySelectorAll('.composer-attachment__name')].map((el) => el.textContent || ''),
        expected,
      })
    );
    const newCutSourceDraft = 'a-unsent-draft-must-survive-new-cut';
    await page.fill('#prompt-input', newCutSourceDraft);

    await createFreshSessionViaManage(page, waitFor, 'T2.2', { closePanel: true });
    await waitForEmptyView(page, 'T2.2: session C did not show the empty new-session view');
    {
      const freshTarget = await page.evaluate(() => ({
        chips: document.querySelectorAll('.composer-attachment').length,
        chipQueueMounted: document.querySelector('#composer-attachments') !== null,
        draft: document.getElementById('prompt-input')?.value ?? '',
      }));
      if (freshTarget.chips !== 0 || freshTarget.chipQueueMounted || freshTarget.draft !== '') {
        fail(`lifecycle isolation: the fresh New target leaked source composer state (chips=${freshTarget.chips}, queue=${freshTarget.chipQueueMounted}, draft=${JSON.stringify(freshTarget.draft)})`);
      }
    }
    await promptAndWait(page, 'sessions-slow-b', 'sessions-slow-b-1/', 'T2.2: C slow stream never started');
    const sessionC = await waitForNewRow(page, knownIds, 'T2.2: session C row never appeared');
    knownIds.push(sessionC);
    {
      const afterC = await rowIds(page);
      assertStableSessionOrder(afterC, catalogOrder, 'T2.2: after session C');
      catalogOrder = afterC;
    }
    await waitForBadge(page, true, 'T2.2: C streaming badge missing');
    record('T2.2');

    // T2.3: unified Stop action aborts C -> neutral toast.
    await waitFor(page, () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Stop generating', 'T2.3: unified action never switched to Stop');
    await page.click('#send-btn');
    await waitFor(
      page,
      () => {
        const toasts = [...document.querySelectorAll('#toasts .toast')];
        return toasts.some(
          (t) => (t.textContent || '').includes('run aborted') && !t.classList.contains('toast--error')
        );
      },
      'T2.3: abort never surfaced the neutral "run aborted" toast',
      15000
    );
    await waitForBadge(page, false, 'T2.3: streaming badge did not clear after abort');
    record('T2.3');
    const cAborted = await lastAssistantText(page);
    if (cAborted.includes('slow-b-done')) {
      fail(`T2.4: aborted C stream still rendered the final tail: ${cAborted}`);
    }
    record('T2.4');

    // T2.5: A was not affected — its stream completes in full.
    await clickRow(page, sessionA, 'T2.5: switch back to A after aborting C');
    await waitFor(page, (s) => document.body.textContent.includes('sessions-slow-a2-'), 'T2.5: A\'s second stream vanished (abort of C leaked into A?)', 15000);
    await waitFor(page, (s) => document.body.textContent.includes('slow-a2-done'), 'T2.5: A\'s second stream never completed (abort of C affected A)', 40000);
    await waitForBadge(page, false, 'T2.5: A streaming badge did not clear after slow-a2 completed');
    /* Lifecycle attachment isolation, return leg: the switch back to A must
     * restore the owner's EXACT view — the same eight chips in order plus
     * the exact unsent draft that rode the New cut. */
    await waitFor(
      page,
      (expected) => {
        const chips = [...document.querySelectorAll('.composer-attachment__name')].map((el) => el.textContent || '');
        return chips.length === expected.chips
          && expected.names.every((name, index) => chips[index] === name)
          && (document.getElementById('prompt-input')?.value ?? '') === expected.draft;
      },
      'T2.5: returning to A did not restore the exact owner chips + unsent draft across the New cut',
      20000,
      { chips: 8, names: expectedNewCutChips, draft: newCutSourceDraft },
      (expected) => ({
        mounted: [...document.querySelectorAll('.composer-attachment__name')].map((el) => el.textContent || ''),
        draft: document.getElementById('prompt-input')?.value ?? '',
        expected,
      })
    );
    record('T2.5');
    await page.screenshot({ path: `${evidence}/sessions-t2-abort-isolation.png`, fullPage: true });

    /* ---------------- T6: desktop rail collapse/reopen + sidebar controls ---------------- */
    await closeManageSession(page, waitFor, 'T6: close leftover session panel before rail collapse');
    await assertNoHeaderFeatureButtons(page, 'T6.1');
    // T6.2: collapse the rail.
    await page.click('#sidebar-toggle-btn');
    await waitFor(
      page,
      () => {
        const layout = document.querySelector('.app-layout');
        const nav = document.querySelector('.session-sidebar__nav');
        const reopen = document.getElementById('rail-reopen-btn');
        const sidebar = document.querySelector('.session-sidebar');
        if (!layout || !sidebar || !reopen) return false;
        if (getComputedStyle(reopen).display === 'none') return false;
        if (nav && getComputedStyle(nav).display !== 'none') return false;
        return sidebar.getBoundingClientRect().width <= 60;
      },
      'T6.2: desktop rail never collapsed to the compact strip',
      15000
    );
    record('T6.2');
    await page.screenshot({ path: `${evidence}/sessions-t6-collapsed.png`, fullPage: true });
    // T6.3: reopen via the rail's dedicated control.
    await page.click('#rail-reopen-btn');
    await waitFor(
      page,
      () => {
        const nav = document.querySelector('.session-sidebar__nav');
        const sidebar = document.querySelector('.session-sidebar');
        if (!nav || !sidebar) return false;
        return getComputedStyle(nav).display !== 'none' && sidebar.getBoundingClientRect().width >= 200;
      },
      'T6.3: rail reopen never restored the sidebar'
    );
    record('T6.3');
    // T6.4: Manage opens the session panel, and the removed header New action
    // remains absent before and after the panel closes.
    await openManageSession(page, waitFor, 'T6.4');
    await closeManageSession(page, waitFor, 'T6.4');
    await waitFor(
      page,
      () => {
        const nav = document.querySelector('.session-sidebar__nav');
        const sidebar = document.querySelector('.session-sidebar');
        const manage = document.getElementById('sidebar-manage-btn');
        if (!nav || !sidebar || !manage) return false;
        if (getComputedStyle(nav).display === 'none') return false;
        if (getComputedStyle(manage).display === 'none') return false;
        return sidebar.getBoundingClientRect().width >= 200;
      },
      'T6.4: desktop rail/Manage Session control not visible after panel close'
    );
    await assertSidebarNewActionAbsent(page, 'T6.4: obsolete sidebar New action returned after panel close');
    record('T6.4');

    /* ---------------- T8: materialized fork ancestry + composer round-trip before reload ---------------- */
    await clickRow(page, sessionB, 'T8.1: switch to B before fork prompt');
    const forkAncestorPrompt = 'fork ancestor turn';
    const forkAncestorReply = `sessions-reply: ${forkAncestorPrompt}`;
    const forkPrompt = 'PONG fork draft exact';
    const forkReply = `sessions-reply: ${forkPrompt}`;
    await promptAndWait(page, forkAncestorPrompt, forkAncestorReply, 'T8.1: first ancestry turn did not settle');
    await promptAndWait(page, forkPrompt, forkReply, 'T8.1: selected ancestry turn did not settle');
    /* Lifecycle attachment isolation on the real surface (unrecorded
     * hard-fail contract), Fork cut: source B carries one attachment chip and
     * one UNSent draft when the fork fires. The fork child must mount with
     * ZERO chips and the fork editorText as its only draft, and the return
     * switch to B must restore B's exact chip + draft. */
    await page.setInputFiles('input[type=file]', {
      name: 'session-b-fork-chip.txt',
      mimeType: 'text/plain',
      buffer: Buffer.from('session B fork chip'),
    });
    await waitFor(
      page,
      (name) => document.querySelectorAll('.composer-attachment').length === 1
        && (document.querySelector('.composer-attachment__name')?.textContent || '') === name,
      'T8.1: session B fork chip never attached',
      20000,
      'session-b-fork-chip.txt'
    );
    const forkSourceDraft = 'b-unsent-draft-must-not-reach-fork-child';
    await page.fill('#prompt-input', forkSourceDraft);
    await openManageSession(page, waitFor, 'T8.1: open Manage Session for fork');
    await waitFor(page, (prompt) => [...document.querySelectorAll('#session-fork-list [role="option"]')]
      .some((option) => option.textContent === prompt),
    'T8.1: selected fork point missing from Manage Session', 30000, forkPrompt);
    const selectedFork = await page.evaluate((prompt) => {
      // The fork candidate window is the accessible bounded listbox
      // (#session-fork-list); selection happens through a real option click
      // (role=option -> onClick -> forkEntry), verified through the ARIA
      // selected/activedescendant state — never a removed native <select>.
      const list = document.getElementById('session-fork-list');
      if (!list) return false;
      const option = [...list.querySelectorAll('[role="option"]')]
        .find((candidate) => candidate.textContent === prompt);
      if (!(option instanceof HTMLElement) || option.id === '') return false;
      option.click();
      return option.getAttribute('aria-selected') === 'true'
        && list.getAttribute('aria-activedescendant') === option.id;
    }, forkPrompt);
    const parentId = await activeSessionId(page);
    // P0 fork-freeze observation: sample the fork button's busy state every
    // animation frame from before the click, so in-progress evidence
    // (disabled/aria-busy) MUST be painted by the next frame after the click.
    await page.evaluate(() => {
      const probe = { clickAt: 0, frames: [] };
      const btn = document.getElementById('session-fork-btn');
      btn?.addEventListener('click', () => { probe.clickAt = performance.now(); }, { capture: true, once: true });
      const sample = () => {
        const forkBtn = document.getElementById('session-fork-btn');
        if (forkBtn) {
          probe.frames.push({
            t: performance.now(),
            disabled: forkBtn.disabled,
            ariaBusy: forkBtn.getAttribute('aria-busy') === 'true',
          });
        }
        if (probe.frames.length < 90) window.requestAnimationFrame(sample);
      };
      window.requestAnimationFrame(sample);
      window.__rpiForkProgressProbe = probe;
    });
    const rowsBeforeFork = await rowIds(page);
    await page.click('#session-fork-btn');
    await waitFor(page, (parent) => {
      const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '';
      return active !== '' && active !== parent;
    }, 'T8.1: Fork did not activate a distinct child session', 30000, parentId);
    const forkSession = await activeSessionId(page);
    // Catalog revision invalidation (P0): immediately after the lifecycle cut
    // — the same painted state as activation, NO reload and NO 8s catalog
    // poll — the sidebar rows must already include the child exactly once,
    // marked active, with zero error toasts and a clean sidebar surface.
    const rowsAfterForkCut = await rowIds(page);
    if (rowsBeforeFork.includes(forkSession)) {
      fail(`T8.1: fork child row pre-existed the lifecycle cut: ${JSON.stringify(rowsBeforeFork)}`);
    }
    if (rowsAfterForkCut.filter((id) => id === forkSession).length !== 1) {
      fail(`T8.1: fork child row not immediately present exactly once after lifecycle cut (no reload/poll): ${JSON.stringify({ rowsBeforeFork, rowsAfterForkCut })}`);
    }
    const forkOutcomeState = await page.evaluate(() => ({
      active: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
      errorToasts: document.querySelectorAll('#toasts .toast--error').length,
      sidebarError: document.querySelector('.session-sidebar__error')?.textContent || '',
    }));
    if (forkOutcomeState.active !== forkSession) {
      fail(`T8.1: fork child row not active immediately after lifecycle cut: ${JSON.stringify(forkOutcomeState)}`);
    }
    if (forkOutcomeState.errorToasts !== 0 || forkOutcomeState.sidebarError !== '') {
      fail(`T8.1: fork lifecycle success surfaced errors: ${JSON.stringify(forkOutcomeState)}`);
    }
    const forkProbe = await page.evaluate(() => window.__rpiForkProgressProbe);
    const firstBusyFrame = (forkProbe?.frames ?? []).find((frame) => frame.disabled || frame.ariaBusy);
    if (!firstBusyFrame) {
      fail('T8.1: fork in-progress evidence (disabled/aria-busy) was never painted after the click');
    }
    const busyLagMs = firstBusyFrame.t - forkProbe.clickAt;
    if (!(busyLagMs >= 0) || busyLagMs > 50) {
      fail(`T8.1: fork in-progress evidence appeared ${busyLagMs.toFixed(1)}ms after the click (must be by the next animation frame)`);
    }
    stageMarks['t8.1ForkClick'] = forkProbe.clickAt;
    stageMarks['t8.1ForkBusyPainted'] = firstBusyFrame.t;
    stageMarks['t8.1ForkChildRowVisible'] = Date.now();
    await waitFor(page, (draft) => document.getElementById('prompt-input')?.value === draft,
      'T8.1: selected fork prompt was not applied to composer', 30000, forkPrompt);
    /* Lifecycle attachment isolation, fork cut (unrecorded hard-fail): the
     * child composer mounts with ZERO attachment chips — the fork's
     * editorText (asserted above) is its ONLY draft and B's chip never
     * leaks in. The editorText equality above already fails any source-draft
     * leak; the chip queue must be absent outright. */
    if ((await page.evaluate(() => document.querySelectorAll('.composer-attachment').length !== 0
      || document.querySelector('#composer-attachments') !== null))) {
      fail('T8.1: fork child composer leaked the fork source attachment chips');
    }
    const assertChildAncestry = (text, label) => {
      const promptAt = text.indexOf(forkAncestorPrompt);
      const replyAt = text.indexOf(forkAncestorReply);
      if (promptAt < 0 || replyAt <= promptAt || text.includes(forkPrompt) || text.includes(forkReply)) {
        fail(`${label}: child prefix/exclusion mismatch: ${JSON.stringify(text)}`);
      }
    };
    assertChildAncestry(await activeTranscript(page), 'T8.1');
    await closeManageSession(page, waitFor, 'T8.1: close Manage Session after fork');
    knownIds.push(forkSession);
    {
      const afterFork = await rowIds(page);
      assertStableSessionOrder(afterFork, catalogOrder, 'T8.1: after fork row append');
      catalogOrder = afterFork;
    }
    await page.screenshot({ path: `${evidence}/sessions-t8-fork-immediate-catalog.png`, fullPage: true });

    // P0 fork-failure outcome: the sidebar's Fork latest RPC is forced to fail
    // at the transport frame level. The failure must surface EXACTLY one
    // error toast, keep the active session on the fork child, and leave the
    // sidebar error surface untouched.
    await page.evaluate(() => window.__rpiSessionAttachmentTest.armLifecycleFailure());
    const parentRowSwitch = page.locator(`.session-sidebar__switch[data-session-id="${sessionB}"]`);
    await parentRowSwitch.click({ button: 'right' });
    await page.locator('.session-sidebar__menu-item', { hasText: 'Fork latest' }).first().click();
    await waitFor(
      page,
      () => window.__rpiSessionAttachmentTest.lifecycleIntercepted === true
        && document.querySelectorAll('#toasts .toast--error').length === 1,
      'T8.1: failed sidebar fork did not surface exactly one error toast',
      20000,
      null,
      () => ({
        intercepted: window.__rpiSessionAttachmentTest?.lifecycleIntercepted,
        errorToasts: [...document.querySelectorAll('#toasts .toast--error')].map((t) => t.textContent || ''),
        active: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
      })
    );
    const failedForkState = await page.evaluate(() => ({
      active: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
      errorToasts: document.querySelectorAll('#toasts .toast--error').length,
      sidebarError: document.querySelector('.session-sidebar__error')?.textContent || '',
    }));
    if (failedForkState.active !== forkSession || failedForkState.errorToasts !== 1 || failedForkState.sidebarError !== '') {
      fail(`T8.1: failed fork did not retain state with exactly one outcome toast: ${JSON.stringify(failedForkState)}`);
    }
    await page.screenshot({ path: `${evidence}/sessions-t8-fork-failure-one-toast.png`, fullPage: true });

    // Composer round-trip runs before reload while the in-memory composer
    // draft still exists: parent → fork reselect must restore the exact
    // forkPrompt draft. No durable composer draft store exists, so this
    // contract cannot be re-checked after reload.
    await clickRow(page, sessionB, 'T8.1: switch from fork to parent for composer round-trip');
    /* Lifecycle attachment isolation, return leg: switching back to the fork
     * source restores B's EXACT owner view — the same one chip and the exact
     * unsent draft that rode the fork cut. */
    await waitFor(
      page,
      (expected) => {
        const chips = [...document.querySelectorAll('.composer-attachment__name')].map((el) => el.textContent || '');
        return chips.length === 1 && chips[0] === expected.chip
          && (document.getElementById('prompt-input')?.value ?? '') === expected.draft;
      },
      'T8.1: returning to the fork source did not restore its exact chip + unsent draft',
      20000,
      { chip: 'session-b-fork-chip.txt', draft: forkSourceDraft },
      (expected) => ({
        mounted: [...document.querySelectorAll('.composer-attachment__name')].map((el) => el.textContent || ''),
        draft: document.getElementById('prompt-input')?.value ?? '',
        expected,
      })
    );
    await clickRow(page, forkSession, 'T8.1: switch from parent back to fork');
    await waitFor(page, (draft) => document.getElementById('prompt-input')?.value === draft,
      'T8.1: parent/child round-trip lost selected composer draft', 30000, forkPrompt);
    record('T8.1');

    await page.reload({ waitUntil: 'domcontentloaded' });
    await waitFor(page, (sid) => {
      const row = [...document.querySelectorAll('.session-sidebar__switch')]
        .find((candidate) => candidate.dataset.sessionId === sid);
      return row !== undefined && row.closest('.session-sidebar__row')?.classList.contains('session-sidebar__row--active');
    }, 'T8.2: refreshed fork did not stay visible/active', 30000, forkSession);
    // The T8.1 composer round-trip ran before reload while the in-memory
    // draft existed; no durable composer draft store exists, so after reload
    // the durable contract is active identity/row + ancestry/exclusion only.
    assertChildAncestry(await activeTranscript(page), 'T8.2');
    const refreshedRows = await rowIds(page);
    if (refreshedRows.filter((id) => id === forkSession).length !== 1) {
      fail(`T8.2: refreshed fork row occurrence mismatch: ${JSON.stringify(refreshedRows)}`);
    }
    // The reload rebuilt the sidebar from one authoritative session_list; the
    // pre-reload retained order no longer applies across this boundary.
    catalogOrder = refreshedRows;
    record('T8.2');

    await clickRow(page, sessionB, 'T8.3: switch from fork to parent');
    const parentText = await activeTranscript(page);
    if (parentText.indexOf(forkAncestorPrompt) < 0 || parentText.indexOf(forkAncestorReply) <= parentText.indexOf(forkAncestorPrompt)
      || parentText.indexOf(forkPrompt) <= parentText.indexOf(forkAncestorReply) || parentText.indexOf(forkReply) <= parentText.indexOf(forkPrompt)) {
      fail(`T8.3: parent transcript lost exact two-turn ancestry order: ${JSON.stringify(parentText)}`);
    }
    // After reload the composer draft is not guaranteed; T8.3 proves parent
    // and fork remain independently selectable via durable ancestry only.
    await clickRow(page, forkSession, 'T8.3: switch from parent back to fork');
    assertChildAncestry(await activeTranscript(page), 'T8.3');
    record('T8.3');

    // T4 fixture: every pre-existing session clicked after the cap must
    // survive LRU retirement in the sidebar. Unnamed native rows under the
    // lane's temp workspace are catalog-classified `temporary` and
    // visibleSidebarSessions drops a row the moment LRU unloads it. C is the
    // deterministic T4.2 victim and is cold-reopened at T4.3; that reopen
    // loads C back into the full pool and evicts the next-LRU idle B, which
    // T5/T7 click later. Assign durable names through the production Session
    // panel rename: a named catalog row is never temporary, while the T4.2
    // recency stamping below still selects C as the exact victim.
    const assignDurableSessionName = async (sid, name, label) => {
      await clickRow(page, sid, `${label}: switch for the durable rename`);
      await waitFor(
        page,
        () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
        `${label}: switch never settled (composer send still disabled by the switch-pending gate)`,
        30000
      );
      await openManageSession(page, waitFor, `${label}: session panel for the durable rename`);
      // The rendered Session id comes only from the panel's own successful
      // get_state, so rename only once the panel is provably owned by sid.
      await waitFor(
        page,
        (owner) => [...document.querySelectorAll('#session-panel dt')]
          .find((el) => el.textContent?.trim() === 'Session id')?.nextElementSibling?.textContent === owner,
        `${label}: session panel never confirmed ownership before the rename`,
        30000,
        sid,
        () => ({
          panelSessionId: [...document.querySelectorAll('#session-panel dt')]
            .find((el) => el.textContent?.trim() === 'Session id')?.nextElementSibling?.textContent || '',
          panelName: document.querySelector('#session-panel [data-testid="session-name-value"]')?.textContent || '',
          activeRow: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
        })
      );
      // session_ux Enter commit: keyboard-typed value + Enter through the
      // panel's own onKeyDown handler; the wire window is timestamp-based.
      const frameBase = await page.evaluate(
        () => (window.__rpiSessionAttachmentTest?.frames || []).at(-1)?.t ?? 0
      );
      await page.click('#session-rename-input');
      await page.keyboard.type(name, { delay: 20 });
      await page.keyboard.press('Enter');
      await waitFor(
        page,
        ({ base, owner, wanted }) => {
          const frames = (window.__rpiSessionAttachmentTest?.frames || []).filter((frame) => frame.t >= base);
          const renames = frames.filter((frame) => frame.type === 'set_session_name');
          return renames.length === 1
            && renames[0].sessionId === owner
            && renames[0].name === wanted
            && frames.some((frame) => frame.type === 'response'
              && frame.command === 'set_session_name'
              && frame.id === renames[0].id
              && frame.success === true);
        },
        `${label}: no single set_session_name request correlated with a successful response`,
        30000,
        { base: frameBase, owner: sid, wanted: name },
        ({ base }) => ({
          frames: (window.__rpiSessionAttachmentTest?.frames || []).filter((frame) => frame.t >= base),
          panelName: document.querySelector('#session-panel [data-testid="session-name-value"]')?.textContent || '',
        })
      );
      await waitFor(
        page,
        (wanted) => document.querySelector('#session-panel [data-testid="session-name-value"]')?.textContent === wanted,
        `${label}: session panel never confirmed the durable rename`,
        30000,
        name,
        () => document.querySelector('#session-panel [data-testid="session-name-value"]')?.textContent || ''
      );
      await closeManageSession(page, waitFor, `${label}: close the session panel after the durable rename`);
      // The sidebar row updates solely from a background session_list (the
      // 8s poll here; no switch follows), so await the wire truth before the
      // title assert. The scan reads the journal afresh per request and the
      // rename entry is already on disk (durable recorder), so this response
      // necessarily carries the named non-temporary row.
      await waitFor(
        page,
        ({ base }) => {
          const frames = (window.__rpiSessionAttachmentTest?.frames || []).filter((frame) => frame.t >= base);
          const lists = frames.filter((frame) => frame.type === 'session_list');
          return lists.length >= 1
            && frames.some((frame) => frame.type === 'response'
              && frame.command === 'session_list'
              && frame.id === lists[lists.length - 1].id
              && frame.success === true);
        },
        `${label}: no authoritative session_list refresh landed after the durable rename`,
        30000,
        { base: frameBase, owner: sid },
        ({ base, owner }) => ({
          frames: (window.__rpiSessionAttachmentTest?.frames || []).filter((frame) => frame.t >= base),
          rowTitle: [...document.querySelectorAll('.session-sidebar__row')]
            .find((candidate) => candidate.querySelector('.session-sidebar__switch')?.dataset.sessionId === owner)
            ?.querySelector('.session-sidebar__name')?.textContent || '',
        })
      );
      await waitFor(
        page,
        ({ wanted, owner }) => [...document.querySelectorAll('.session-sidebar__row')]
          .find((candidate) => candidate.querySelector('.session-sidebar__switch')?.dataset.sessionId === owner)
          ?.querySelector('.session-sidebar__name')?.textContent === wanted,
        `${label}: renamed durable row never reached the sidebar catalog`,
        30000,
        { owner: sid, wanted: name },
        () => [...document.querySelectorAll('.session-sidebar__row')]
          .map((candidate) => `${candidate.querySelector('.session-sidebar__switch')?.dataset.sessionId || ''}=${candidate.querySelector('.session-sidebar__name')?.textContent || ''}`)
      );
    };
    await assignDurableSessionName(sessionB, 't4-durable-session-b', 'T4 fixture: session B');
    await assignDurableSessionName(sessionC, 't4-lru-victim-durable', 'T4 fixture: session C');
    await clickRow(page, sessionA, 'T4 fixture: restore primary after the durable renames');
    markStage('t4LruCapStart');

    /* ---------------- T4: 16-runtime cap with idle LRU replacement ---------------- */
    await clickRow(page, sessionA, 'T4.1: switch back to A before creating more sessions');
    const created = [];
    const loadedBeforeCap = await page.evaluate(() => document.querySelectorAll('.session-sidebar__loaded').length);
    // A, B, C, and the T8 fork child are the four loaded runtimes the earlier
    // phases leave behind.
    if (loadedBeforeCap !== 4) {
      fail(`T4.1: invalid authoritative loaded count before cap (${loadedBeforeCap}, expected A/B/C/fork)`);
    }
    const sessionsNeeded = MAX_LOADED_RUNTIMES - loadedBeforeCap;
    for (let i = 1; i <= sessionsNeeded; i++) {
      await createFreshSessionViaManage(page, waitFor, `T4.1: session S${i}`, { closePanel: true });
      await waitForEmptyView(page, `T4.1: session S${i} did not show the empty new-session view`);
      const reply = `sessions-reply: persist-${i}`;
      await promptAndWait(page, `persist-${i}`, reply, `T4.1: persist-${i} never round-tripped`);
      const sid = await waitForNewRow(page, knownIds, `T4.1: session S${i} row never appeared`, 30000);
      knownIds.push(sid);
      created.push({ sid, reply });
      {
        const afterCreated = await rowIds(page);
        assertStableSessionOrder(afterCreated, catalogOrder, `T4.1: after session S${i}`);
        catalogOrder = afterCreated;
      }
    }
    const loadedAtCap = await page.evaluate(() => [...document.querySelectorAll('.session-sidebar__loaded')]
      .map((row) => row.getAttribute('data-loaded-for') || '').filter(Boolean));
    if (loadedAtCap.length !== MAX_LOADED_RUNTIMES) fail(`T4.1: loaded pool did not reach ${MAX_LOADED_RUNTIMES}: ${JSON.stringify(loadedAtCap)}`);
    if (!loadedAtCap.includes(sessionA)) {
      fail(`T4.1: primary runtime missing from the loaded pool: ${JSON.stringify(loadedAtCap)}`);
    }
    record('T4.1');
    // Stamp recency so the next create must retire one known idle
    // non-primary. FIFO/random/MRU cannot match: FIFO would retire B (first
    // non-primary); MRU would retire the last touch.
    if (!loadedAtCap.includes(sessionC) || sessionC === sessionA) {
      fail(`T4.2: session C is not a loaded idle non-primary: ${JSON.stringify(loadedAtCap)}`);
    }
    const expectedOldest = sessionC;
    const recentIdle = loadedAtCap.find((id) => id !== sessionA && id !== expectedOldest);
    if (!recentIdle) {
      fail(`T4.2: need a second idle non-primary to pin LRU vs MRU: ${JSON.stringify(loadedAtCap)}`);
    }
    const recencyOrder = [
      expectedOldest,
      ...loadedAtCap.filter((id) => id !== expectedOldest && id !== recentIdle && id !== sessionA),
      recentIdle,
      sessionA,
    ];
    // get_state is Housekeeping and does not stamp IsolatedApplication LRU
    // recency; user set_model (same model, no side effect) is the recency
    // touch that keeps this order authoritative.
    const t4StampState = await workflowRpc(expectedOldest, { type: 'get_state' });
    const t4Model = t4StampState?.model;
    if (!t4Model || typeof t4Model.provider !== 'string' || typeof t4Model.id !== 'string') {
      fail(`T4.2: get_state missing model for LRU recency restamp: ${JSON.stringify(t4StampState)}`);
    }
    for (const sid of recencyOrder) {
      await workflowRpc(sid, { type: 'set_model', provider: t4Model.provider, modelId: t4Model.id });
    }
    await clickRow(page, sessionA, 'T4.2: restore primary before LRU replacement create');
    const replacement = await createFreshSessionViaManage(page, waitFor, 'T4.2: LRU replacement create', { closePanel: true });
    await waitForEmptyView(page, 'T4.2: replacement session did not show empty view');
    await promptAndWait(page, 'replacement-persist', 'sessions-reply: replacement-persist', 'T4.2: replacement prompt never round-tripped');
    const replacementId = await waitForNewRow(page, knownIds, 'T4.2: replacement session row never appeared', 30000);
    knownIds.push(replacementId);
    if (replacement.sessionId !== replacementId) fail(`T4.2: replacement identity mismatch ${JSON.stringify({ replacement, replacementId })}`);
    const loadedAfterReplace = await page.evaluate(() => [...document.querySelectorAll('.session-sidebar__loaded')]
      .map((row) => row.getAttribute('data-loaded-for') || '').filter(Boolean));
    if (loadedAfterReplace.length !== MAX_LOADED_RUNTIMES || !loadedAfterReplace.includes(replacementId) || !loadedAfterReplace.includes(sessionA) || !loadedAfterReplace.includes(recentIdle)) {
      fail(`T4.2: LRU replacement pool mismatch: ${JSON.stringify({ loadedAfterReplace, replacementId, sessionA, recentIdle })}`);
    }
    const retired = loadedAtCap.find((id) => !loadedAfterReplace.includes(id));
    if (retired !== expectedOldest) {
      fail(`T4.2: retired runtime was not the exact expected idle LRU: ${JSON.stringify({ expectedOldest, retired, recencyOrder, loadedAtCap, loadedAfterReplace })}`);
    }
    record('T4.2');
    const catalogAfterReplace = await workflowRpc(replacementId, { type: 'session_list', scope: 'all_projects' });
    const retiredRow = Array.isArray(catalogAfterReplace.sessions)
      ? catalogAfterReplace.sessions.find((row) => row?.sessionId === retired)
      : null;
    if (!retiredRow || retiredRow.loaded === true || retiredRow.temporary === true || loadedAfterReplace.includes(retired)) {
      fail(`T4.3: retired runtime lost its durable catalog row, stayed loaded, or degraded to temporary: ${JSON.stringify({ retired, retiredRow, loadedAfterReplace })}`);
    }
    const loadedAfterReload = await page.evaluate(() => document.querySelectorAll('.session-sidebar__loaded').length);
    if (loadedAfterReload !== MAX_LOADED_RUNTIMES) fail(`T4.3: loaded count changed after replacement (${loadedAfterReload})`);
    await page.screenshot({ path: `${evidence}/sessions-t4-retired-durable.png`, fullPage: true });
    // Exact-switch contract for LRU retirement: the retired row was unloaded
    // (its ready entry invalidated by the session_unloaded cut) and its
    // catalog row carries no loaded incarnation hint, so reopening it must
    // cost EXACTLY ONE cold switch_session + one subscribe + the cold-full
    // cut — never zero and never more.
    {
      const retiredBase = await page.evaluate(() => (window.__rpiSessionAttachmentTest?.frames || []).length);
      await clickRow(page, retired, 'T4.3: cold reopen of the LRU-retired row');
      const retiredFrames = await page.evaluate(
        (from) => (window.__rpiSessionAttachmentTest?.frames || []).slice(from),
        retiredBase
      );
      const retiredSwitches = retiredFrames.filter(
        (frame) => frame.type === 'switch_session' && frame.catalogSessionId === retired
      ).length;
      const retiredSubscribes = retiredFrames.filter(
        (frame) => frame.type === 'subscribe' && frame.sessionId === retired
      ).length;
      if (retiredSwitches !== 1 || retiredSubscribes !== 1) {
        fail(`T4.3: LRU-retired row reopen must dispatch exactly one cold switch_session + one subscribe, got ${JSON.stringify({ retiredSwitches, retiredSubscribes })}`);
      }
      if ((await page.evaluate(() => document.querySelectorAll('#toasts .toast--error').length)) !== 0) {
        fail('T4.3: LRU-retired row reopen surfaced an error toast');
      }
      switchFastEvidence.retiredColdRpc = {
        sessionId: retired,
        switchSessionFrames: retiredSwitches,
        subscribeFrames: retiredSubscribes,
      };
    }
    {
      const afterLru = await rowIds(page);
      assertStableSessionOrder(afterLru, catalogOrder, 'T4.3: after LRU replacement');
      catalogOrder = afterLru;
    }
    record('T4.3');
    markStage('t5WorkflowIsolationStart');

    /* ---------------- T5: Todo/Goal/Workflow isolation ---------------- */
    // T5.1 Todo: create in A, verify absent in B and back in A.
    await clickRow(page, sessionA, 'T5.1: switch to A for the todo leak test');
    await page.click('#todo-toggle-btn');
    await waitFor(page, () => {
      const panels = document.querySelectorAll('#todo-panel');
      const panel = panels.length === 1 ? panels[0] : null;
      return panel !== null
        && panel.querySelector('.todo-panel__tasks') !== null
        && panel.querySelector('.todo-child-section') !== null;
    }, 'T5.1: unified Todo module did not open on A with task and child sections');
    await page.fill('#todo-add-phase', 'Plan');
    await page.fill('#todo-add-content', 'a-todo-task');
    await page.click('#todo-add-btn');
    await waitFor(
      page,
      () => [...document.querySelectorAll('.todo-task')].some((r) => (r.textContent || '').includes('a-todo-task')),
      'T5.1: todo task never appeared on A'
    );
    await page.click('#todo-close-btn');
    await waitFor(page, () => document.getElementById('todo-panel') === null, 'T5.1: todo panel did not close on A');

    await clickRow(page, sessionB, 'T5.1: switch to B for the todo leak check');
    await page.click('#todo-toggle-btn');
    await waitFor(page, () => {
      const panels = document.querySelectorAll('#todo-panel');
      const panel = panels.length === 1 ? panels[0] : null;
      return panel !== null
        && panel.querySelector('.todo-panel__tasks') !== null
        && panel.querySelector('.todo-child-section') !== null;
    }, 'T5.1: unified Todo module did not open on B with task and child sections');
    const bTodoText = await page.evaluate(() => document.getElementById('todo-panel')?.textContent || '');
    if (bTodoText.includes('a-todo-task')) {
      fail("T5.1: session A's todo task leaked into session B's todo panel");
    }
    await page.fill('#todo-add-phase', 'Plan');
    await page.fill('#todo-add-content', 'b-todo-task');
    await page.click('#todo-add-btn');
    await waitFor(
      page,
      () => [...document.querySelectorAll('.todo-task')].some((r) => (r.textContent || '').includes('b-todo-task')),
      'T5.1: todo task never appeared on B'
    );
    await page.click('#todo-close-btn');
    await waitFor(page, () => document.getElementById('todo-panel') === null, 'T5.1: todo panel did not close on B');

    await clickRow(page, sessionA, 'T5.1: switch back to A to verify todo isolation');
    await page.click('#todo-toggle-btn');
    await waitFor(page, () => {
      const panels = document.querySelectorAll('#todo-panel');
      const panel = panels.length === 1 ? panels[0] : null;
      return panel !== null
        && panel.querySelector('.todo-panel__tasks') !== null
        && panel.querySelector('.todo-child-section') !== null;
    }, 'T5.1: unified Todo module did not reopen on A with task and child sections');
    await waitFor(
      page,
      () => [...document.querySelectorAll('.todo-task')].some((r) => (r.textContent || '').includes('a-todo-task')),
      "T5.1: A's todo task lost after the B round-trip"
    );
    const aTodoText = await page.evaluate(() => document.getElementById('todo-panel')?.textContent || '');
    if (aTodoText.includes('b-todo-task')) {
      fail("T5.1: session B's todo task leaked into session A's todo panel");
    }
    await page.click('#todo-close-btn');
    await waitFor(page, () => document.getElementById('todo-panel') === null, 'T5.1: todo panel did not close on A (final)');
    record('T5.1');

    // T5.2 Goal: create in A, verify B shows the empty create form.
    await page.click('#goal-panel-btn');
    await waitFor(page, () => document.getElementById('goal-panel') !== null, 'T5.2: goal panel did not open on A');
    await page.fill('#goal-objective-input', 'a-goal-objective');
    await page.click('#goal-create-btn');
    await waitFor(
      page,
      () => (document.getElementById('goal-objective')?.textContent || '').includes('a-goal-objective'),
      'T5.2: goal never created on A',
      30000
    );
    await page.click('#goal-close-btn');
    await waitFor(page, () => document.getElementById('goal-panel') === null, 'T5.2: goal panel did not close on A');

    await clickRow(page, sessionB, 'T5.2: switch to B for the goal leak check');
    await page.click('#goal-panel-btn');
    await waitFor(page, () => document.getElementById('goal-panel') !== null, 'T5.2: goal panel did not open on B');
    const bGoalState = await page.evaluate(() => document.getElementById('goal-panel')?.getAttribute('data-has-goal') || '');
    const bGoalHasForm = await page.evaluate(() => document.getElementById('goal-create-form') !== null);
    const bGoalText = await page.evaluate(() => document.getElementById('goal-panel')?.textContent || '');
    if (bGoalState !== 'false' || !bGoalHasForm || bGoalText.includes('a-goal-objective')) {
      fail(`T5.2: session A's goal leaked into session B (has-goal=${bGoalState}, form=${bGoalHasForm})`);
    }
    await page.click('#goal-close-btn');
    await waitFor(page, () => document.getElementById('goal-panel') === null, 'T5.2: goal panel did not close on B');
    record('T5.2');

    const failureCreated = await workflowRpc(sessionA, {
      type: 'workflow_create', name: 'fail-closed-probe', objective: 'sessions-fail-closed-objective',
    });
    const failureClassification = await waitForSessionWorkflow(
      sessionA,
      failureCreated.workflowId,
      'fail-closed-probe',
      'T5.3 fail-closed probe',
      true
    );
    if (failureClassification.kind !== 'terminal-error'
      || !failureClassification.message.includes('terminal failed')
      || failureClassification.message.includes('no surfaced reason')
      || fs.existsSync(coveragePath)) {
      fail(`T5.3: failed workflow was not classified before coverage evidence: ${JSON.stringify(failureClassification)}`);
    }
    await workflowRpc(sessionA, { type: 'workflow_remove', workflowId: failureCreated.workflowId });

    // T5.3 Workflow: each session owns and completes its exact two-task DAG.
    const workflowACreated = await workflowRpc(sessionA, {
      type: 'workflow_create', name: 'a-wf', objective: 'a-wf-objective',
    });
    const workflowBCreated = await workflowRpc(sessionB, {
      type: 'workflow_create', name: 'b-wf', objective: 'b-wf-objective',
    });
    if (!workflowACreated.workflowId || !workflowBCreated.workflowId) fail('T5.3: workflow create response missing workflowId');
    const heldDeadline = Date.now() + 30000;
    let heldStats = {};
    while (Date.now() < heldDeadline) {
      heldStats = await mockControl('/__session-workers-held');
      if (heldStats.held >= 2 && heldStats.workers >= 2) break;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    if (heldStats.held < 2 || heldStats.workers < 2) fail(`T5.3: first dependency wave never became authoritatively held: ${JSON.stringify(heldStats)}`);
    for (const [owner, created, name] of [[sessionA, workflowACreated, 'a-wf'], [sessionB, workflowBCreated, 'b-wf']]) {
      const progress = await workflowRpc(owner, { type: 'workflow_detail', workflowId: created.workflowId });
      const progressClassification = classifyWorkflowSnapshot(progress, created.workflowId, name);
      if (progressClassification.kind !== 'progress') {
        fail(`T5.3: ${name} held workflow was not live progress: ${progressClassification.message || JSON.stringify(progress)}`);
      }
      const tasks = Array.isArray(progress.todo) ? progress.todo.flatMap((phase) => phase.tasks || []) : [];
      const objective = name === 'a-wf' ? 'a-wf-objective' : 'b-wf-objective';
      const identityTag = `[workflowId=${created.workflowId};name=${objective}]`;
      const compile = tasks.find((task) => task.content === `compile widget ${identityTag}`);
      const ship = tasks.find((task) => task.content === `ship widget ${identityTag}`);
      if (!compile || !ship || !['in_progress', 'completed'].includes(compile.status)
        || ship.status !== 'pending' || ship.ready !== false || !Array.isArray(ship.dependsOn)
        || ship.dependsOn.length !== 1 || ship.dependsOn[0] !== compile.id) {
        fail(`T5.3: ${name} did not expose compile-running/ship-blocked dependency transition: ${JSON.stringify(tasks)}`);
      }
    }
    await mockControl('/__release-session-workers');
    await mockControl('/__release-session-workers');
    const secondWaveDeadline = Date.now() + 30000;
    while (Date.now() < secondWaveDeadline) {
      heldStats = await mockControl('/__session-workers-held');
      if (heldStats.completed >= 2 && heldStats.held >= 2 && heldStats.workers >= 4) break;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    if (heldStats.completed < 2 || heldStats.held < 2 || heldStats.workers < 4) {
      fail(`T5.3: dependent ship wave never became held after compile completion: ${JSON.stringify(heldStats)}`);
    }
    await mockControl('/__release-session-workers');
    await mockControl('/__release-session-workers');
    const [workflowA, workflowB] = await Promise.all([
      waitForSessionWorkflow(sessionA, workflowACreated.workflowId, 'a-wf', 'T5.3 session A'),
      waitForSessionWorkflow(sessionB, workflowBCreated.workflowId, 'b-wf', 'T5.3 session B'),
    ]);
    const finalStats = await mockControl('/__session-workflow-stats');
    const expectedAssignments = new Map([
      [workflowACreated.workflowId, 'a-wf-objective'],
      [workflowBCreated.workflowId, 'b-wf-objective'],
    ]);
    const observedAssignments = finalStats.assignments || {};
    if (finalStats.planning !== 6 || finalStats.held !== 0
      || finalStats.workers !== 4 || finalStats.completed !== 4
      || Object.keys(observedAssignments).length !== expectedAssignments.size
      || [...expectedAssignments].some(([workflowId, name]) => {
        const record = observedAssignments[workflowId];
        return record?.name !== name
          || Object.keys(record.tasks || {}).length !== 2
          || record.tasks.compile !== 1
          || record.tasks.ship !== 1;
      })) {
      fail(`T5.3: required tasks did not complete exactly once per workflow: ${JSON.stringify(finalStats)}`);
    }
    for (const [owner, workflow, expectedName, expectedId] of [
      [sessionA, workflowA, 'a-wf', workflowACreated.workflowId],
      [sessionB, workflowB, 'b-wf', workflowBCreated.workflowId],
    ]) {
      if (workflow.workflowId !== expectedId) fail(`T5.3: ${expectedName} workflowId mismatch ${workflow.workflowId}`);
      if (!Number.isSafeInteger(workflow.generation) || workflow.generation < 1) fail(`T5.3: ${expectedName} generation is not positive`);
      if (workflow.name !== expectedName) fail(`T5.3: ${expectedName} create name mismatch`);
      if (typeof workflow.worktree !== 'string' || workflow.worktree.trim() === '') fail(`T5.3: ${expectedName} missing worktree`);
      if (typeof workflow.branch !== 'string' || workflow.branch.trim() === '') fail(`T5.3: ${expectedName} missing branch`);
      if (!workflow.branch.includes(workflow.workflowId)) fail(`T5.3: ${expectedName} branch lost workflow identity`);
      if (workflow.status !== 'completed') {
        fail(`T5.3: ${expectedName} did not remain completed: ${workflow.status}`);
      }
      const ownerList = await workflowRpc(owner, { type: 'workflow_list' });
      const ownerRows = Array.isArray(ownerList.workflows) ? ownerList.workflows : [];
      const exact = ownerRows.find((row) => row.workflowId === workflow.workflowId);
      if (!exact || exact.generation !== workflow.generation || exact.worktree !== workflow.worktree || exact.branch !== workflow.branch) {
        fail(`T5.3: ${expectedName} exact identity missing from owning session list: ${JSON.stringify(ownerRows)}`);
      }
    }
    if (workflowA.workflowId === workflowB.workflowId) fail(`T5.3: A/B workflowId collision ${workflowA.workflowId}`);
    if (`${workflowA.workflowId}@${workflowA.generation}` === `${workflowB.workflowId}@${workflowB.generation}`) {
      fail('T5.3: A/B workflow identity tuple collision');
    }
    if (workflowA.worktree === workflowB.worktree) fail(`T5.3: A/B worktree collision ${workflowA.worktree}`);
    if (workflowA.branch === workflowB.branch) fail(`T5.3: A/B branch collision ${workflowA.branch}`);

    const [listA, listB] = await Promise.all([
      workflowRpc(sessionA, { type: 'workflow_list' }),
      workflowRpc(sessionB, { type: 'workflow_list' }),
    ]);
    const idsA = new Set((listA.workflows || []).map((workflow) => workflow.workflowId));
    const idsB = new Set((listB.workflows || []).map((workflow) => workflow.workflowId));
    if (!idsA.has(workflowA.workflowId) || idsA.has(workflowB.workflowId)) fail(`T5.3: session A workflow list crossed ownership: ${JSON.stringify(listA.workflows)}`);
    if (!idsB.has(workflowB.workflowId) || idsB.has(workflowA.workflowId)) fail(`T5.3: session B workflow list crossed ownership: ${JSON.stringify(listB.workflows)}`);

    await clickRow(page, sessionA, 'T5.3: switch to A for workflow list isolation');
    await page.click('#workflow-toggle-btn');
    await waitFor(page, () => document.getElementById('workflow-panel') !== null, 'T5.3: workflow panel did not open on A');
    await waitFor(
      page,
      (identity) => {
        const ids = [...document.querySelectorAll('.workflow-row')].map((row) => row.dataset.workflowId || '');
        return ids.includes(identity.own) && !ids.includes(identity.foreign);
      },
      'T5.3: session A panel did not show only its workflow list',
      30000,
      { own: workflowA.workflowId, foreign: workflowB.workflowId }
    );
    await page.click('#workflow-close-btn');
    await waitFor(page, () => document.getElementById('workflow-panel') === null, 'T5.3: workflow panel did not close on A');

    await clickRow(page, sessionB, 'T5.3: switch to B for workflow list isolation');
    await page.click('#workflow-toggle-btn');
    await waitFor(page, () => document.getElementById('workflow-panel') !== null, 'T5.3: workflow panel did not open on B');
    await waitFor(
      page,
      (identity) => {
        const ids = [...document.querySelectorAll('.workflow-row')].map((row) => row.dataset.workflowId || '');
        return ids.includes(identity.own) && !ids.includes(identity.foreign);
      },
      'T5.3: session B panel did not show only its workflow list',
      30000,
      { own: workflowB.workflowId, foreign: workflowA.workflowId }
    );
    await page.screenshot({ path: `${evidence}/sessions-t5-workflow-list-ab-isolation.png`, fullPage: true });
    await page.click('#workflow-close-btn');
    await waitFor(page, () => document.getElementById('workflow-panel') === null, 'T5.3: workflow panel did not close on B');
    record('T5.3');

    /* ---------------- T7: Android 390x844 drawer ---------------- */
    const mobile = await browser.newPage({ viewport: { width: 390, height: 844 } });
    if (token) {
      await mobile.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    await mobile.addInitScript((sid) => {
      window.localStorage.setItem(`rpi-web-session:${encodeURIComponent(window.location.host)}`, sid);
    }, sessionA);
    // Mobile shim: same switch_session failure injection as the desktop lane,
    // plus a boot-time strip of B's advisory catalog incarnation hint. With
    // the hint removed, restoreReconnectCatalog never pre-subscribes B and no
    // ready entry is ever stamped for it on this fresh socket, so the T7.2
    // failed pick exercises the NEGOTIATED path (its switch_session
    // interception applies) instead of the zero-wire exact-ready fast path.
    await mobile.addInitScript((stripSid) => {
      const NativeWebSocket = window.WebSocket;
      const control = {
        failNext: false,
        intercepted: false,
        stripRowIncarnationFor: stripSid,
        strippedRowIncarnation: false,
        armFailure() {
          this.failNext = true;
          this.intercepted = false;
        },
      };
      class SessionSwitchTestWebSocket extends NativeWebSocket {
        constructor(...args) {
          super(...args);
          this.addEventListener('message', (event) => {
            if (event.__rpiInjected) return;
            if (!control.stripRowIncarnationFor) return;
            let frame;
            try {
              frame = JSON.parse(event.data);
            } catch {
              return;
            }
            if (!frame || frame.type !== 'response' || frame.success !== true) return;
            const rows = frame.data && Array.isArray(frame.data.sessions) ? frame.data.sessions : null;
            if (!rows) return;
            const index = rows.findIndex(
              (row) => row && row.sessionId === control.stripRowIncarnationFor
                && typeof row.runtimeIncarnation === 'string' && row.runtimeIncarnation !== ''
            );
            if (index < 0) return;
            control.stripRowIncarnationFor = '';
            control.strippedRowIncarnation = true;
            event.stopImmediatePropagation();
            const clone = JSON.parse(JSON.stringify(frame));
            delete clone.data.sessions[index].runtimeIncarnation;
            const fake = new MessageEvent('message', { data: JSON.stringify(clone) });
            fake.__rpiInjected = true;
            this.dispatchEvent(fake);
          });
        }
        send(data) {
          let outgoing = data;
          if (control.failNext && typeof data === 'string') {
            try {
              const frame = JSON.parse(data);
              if (frame?.type === 'switch_session') {
                control.failNext = false;
                control.intercepted = true;
                outgoing = JSON.stringify({
                  ...frame,
                  sessionPath: 'missing-session-for-mobile-regression.jsonl',
                });
              }
            } catch {
              // Non-JSON frames pass through unchanged.
            }
          }
          return super.send(outgoing);
        }
      }
      for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
        Object.defineProperty(SessionSwitchTestWebSocket, key, { value: NativeWebSocket[key] });
      }
      window.WebSocket = SessionSwitchTestWebSocket;
      window.__rpiSessionSwitchTest = control;
    }, sessionB);
    const mobileErrors = monitorBrowserErrors(mobile, 'mobile');
    await mobile.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(mobile, () => document.title === 'rpi web', 'T7.1: mobile page title missing');
    await waitFor(
      mobile,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'T7.1: mobile WS did not reach "connected"'
    );
    await waitFor(
      mobile,
      (sid) => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId === sid,
      'T7.1: mobile page did not restore prepared session A',
      30000,
      sessionA
    );
    await assertNoHeaderFeatureButtons(mobile, 'T7.4');
    const hamburgerDisplay = await mobile.evaluate(
      () => getComputedStyle(document.getElementById('sidebar-toggle-btn')).display
    );
    if (hamburgerDisplay === 'none') {
      fail('T7.1: #sidebar-toggle-btn is hidden at 390x844 (CSS cascade regression)');
    }
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
          sidebar.getBoundingClientRect().left <= 1
        );
      },
      'T7.1: session drawer never opened from the hamburger'
    );
    await mobile.screenshot({ path: `${evidence}/sessions-t7-drawer.png`, fullPage: true });
    // The fork child from T8.1 must also be visible on the mobile drawer's
    // catalog (same revision-driven list); capture the drawer evidence here.
    // This fresh mobile page renders the backend's authoritative newest-first
    // catalog — its order legitimately differs from the desktop page's
    // retained `catalogOrder` baseline (T4.1 appended 16 rows at the desktop
    // tail while the backend sorts newest-first, and reconcileSessionRowOrder
    // keeps each page's OWN retained order stable across refreshes) — so
    // T7.3 compares the post-pick rows against THIS page's pre-pick order,
    // never against the desktop cross-page stale baseline.
    const mobilePrePickOrder = await rowIds(mobile);
    if (mobilePrePickOrder.filter((id) => id === forkSession).length !== 1) {
      fail(`T7.1: mobile drawer does not show the T8.1 fork child exactly once: ${JSON.stringify(mobilePrePickOrder)}`);
    }
    await mobile.screenshot({ path: `${evidence}/sessions-t7-drawer-fork-row.png`, fullPage: true });
    record('T7.1');
    // T7.2: a failed pick must retain the visible drawer and current session,
    // surface exactly one bottom-right error toast, and leave the sidebar clean.
    await mobile.evaluate(() => window.__rpiSessionSwitchTest.armFailure());
    await mobile.evaluate((sid) => {
      const row = [...document.querySelectorAll('.session-sidebar__switch')]
        .find((candidate) => candidate.dataset.sessionId === sid);
      if (!row) throw new Error(`missing session row ${sid}`);
      row.click();
    }, sessionB);
    await waitFor(
      mobile,
      ({ targetSid, currentSid }) => {
        const layout = document.querySelector('.app-layout');
        const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '';
        const target = [...document.querySelectorAll('.session-sidebar__switch')]
          .find((candidate) => candidate.dataset.sessionId === targetSid);
        const switchErrors = [...document.querySelectorAll('#toasts .toast--error')];
        const errorText = switchErrors[0]?.textContent || '';
        return window.__rpiSessionSwitchTest.intercepted === true
          && layout?.classList.contains('app-layout--drawer-open') === true
          && active === currentSid
          && document.querySelector('.session-sidebar__error') === null
          && switchErrors.length === 1
          && errorText.includes('switch to ')
          && errorText.includes(' failed')
          && errorText.toLowerCase().includes('session')
          && target?.disabled === false;
      },
      'T7.2: failed session pick did not retain drawer/current session/exactly-one toast',
      20000,
      { targetSid: sessionB, currentSid: sessionA }
    );
    const failedSwitchState = await mobile.evaluate(() => {
      const errorToasts = [...document.querySelectorAll('#toasts .toast--error')];
      return {
        active: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
        drawerOpen: document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true,
        sidebarError: document.querySelector('.session-sidebar__error')?.textContent || '',
        errorToastCount: errorToasts.length,
        errorToastText: errorToasts[0]?.textContent || '',
      };
    });
    if (failedSwitchState.active !== sessionA || !failedSwitchState.drawerOpen
      || failedSwitchState.sidebarError !== '' || failedSwitchState.errorToastCount !== 1
      || !failedSwitchState.errorToastText.includes('switch to ')
      || !failedSwitchState.errorToastText.includes(' failed')
      || !failedSwitchState.errorToastText.toLowerCase().includes('session')) {
      fail(`T7.2: failed switch state was not retained: ${JSON.stringify(failedSwitchState)}`);
    }
    await mobile.screenshot({ path: `${evidence}/sessions-t7-failed-switch-toast.png`, fullPage: true });
    // Failed-pick no-reorder: the injected switch_session failure must leave
    // this page's own catalog order exactly as observed before the pick.
    assertStableSessionOrder(await rowIds(mobile), mobilePrePickOrder, 'T7.2: after failed mobile pick');
    await clickRow(mobile, sessionB, 'T7.2: successful pick session B from the drawer');
    await waitFor(
      mobile,
      () => !document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open'),
      'T7.2: drawer never closed after the session pick',
      20000
    );
    record('T7.2');
    await waitFor(
      mobile,
      (r) => document.body.textContent.includes(r),
      'T7.3: picked session B transcript never restored from the backend',
      30000,
      'sessions-reply: b-prompt-1'
    );
    await mobile.screenshot({ path: `${evidence}/sessions-t7-picked.png`, fullPage: true });
    {
      const afterMobilePick = await rowIds(mobile);
      assertStableSessionOrder(afterMobilePick, mobilePrePickOrder, 'T7.3: after mobile session pick');
    }
    record('T7.3');
    await mobile.close();
    const browserErrors = [...desktopErrors, ...mobileErrors];
    if (browserErrors.length > 0) {
      fail(`unexpected browser errors: ${browserErrors.join(' | ')}`);
    }

    // The lane may only report PASS (and may only write evidence) once the
    // FULL documented matrix executed: a renamed/renumbered contract that no
    // longer records its ID fails the lane instead of silently shrinking the
    // evidence the coverage matrix quantifies. Startup deleted any stale
    // coverage-assertions.json; this rename is the only write.
    const missing = DOCUMENTED_IDS.filter((id) => !executed.has(id));
    if (missing.length > 0) {
      fail(`T-lane evidence incomplete: ${missing.join(', ')} never executed (coverage matrix contract)`);
    }
    const overExecuted = [...executed.keys()].filter((id) => !DOCUMENTED_IDS.includes(id));
    if (overExecuted.length > 0) {
      fail(`T-lane evidence drift: executed-but-undocumented ids: ${overExecuted.join(', ')}`);
    }
    const repeatedIds = [...executed].filter(([, count]) => count !== 1).map(([id]) => id);
    if (repeatedIds.length > 0) {
      fail(`T-lane evidence exact-once violation: ${repeatedIds.join(', ')}`);
    }
    // Exact Map gate before ANY evidence write: the recorded ledger must be
    // EXACTLY the documented matrix — same ids, same order, every count 1 —
    // and the published executed array is the REAL Map insertion order
    // (actual execution order), never a reconstruction of DOCUMENTED_IDS.
    const executedOrder = [...executed.keys()];
    if (executedOrder.length !== DOCUMENTED_IDS.length
      || executedOrder.some((id, index) => id !== DOCUMENTED_IDS[index])) {
      fail(`T-lane evidence order mismatch: executed ${JSON.stringify(executedOrder)} != documented ${JSON.stringify(DOCUMENTED_IDS)}`);
    }
    const counts = Object.fromEntries([...executed].map(([id, count]) => [id, count]));
    const coverageTmp = `${coveragePath}.tmp`;
    fs.writeFileSync(coverageTmp, JSON.stringify({
      documented: [...DOCUMENTED_IDS],
      executed: executedOrder,
      counts,
    }, null, 2));
    // P0 fork-freeze latency evidence: wall-clock stage marks + fork DOM
    // observations for the independent latency measurement pass. Written only
    // after the full matrix executed, alongside coverage-assertions.json.
    fs.writeFileSync(
      path.join(evidence, 'sessions-stage-timestamps.json'),
      JSON.stringify({ stageMarks, units: 'milliseconds-since-epoch-unless-noted' }, null, 2),
    );
    // Exact-ready fast-switch timing/count evidence (counts + paint
    // provenance only, never transcript content) — written only after the
    // full matrix executed and every exact-switch assertion passed.
    fs.writeFileSync(
      path.join(evidence, 'switch-fast-metrics.json'),
      JSON.stringify(switchFastEvidence, null, 2),
    );
    fs.renameSync(coverageTmp, coveragePath);

    console.log(`web-sessions: PASSED (${executed.size}/${DOCUMENTED_IDS.length} assertions, T0 boot, T1 concurrent streaming + unread + restore, T3 lifecycle attachment isolation, T2 abort/toast isolation, T6 rail collapse/reopen + header, T8 materialized fork ancestry + composer-only cut, T4 16-runtime cap + idle LRU replacement, T5 Todo/Goal/Workflow completed DAG + fail-closed isolation, T7 390x844 drawer failure-retain + success-close) — evidence at ${coveragePath}`);
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-sessions: playwright crashed: ${err && err.stack ? err.stack : err}`);
  process.exit(2);
});
