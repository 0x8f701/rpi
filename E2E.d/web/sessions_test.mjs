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
//   T6  T6.1 one header feature-menu entry; closed has no sidebar feature items;
//       six-viewport full row + >=200px list; 12 menu entries/click-through actions;
//       Esc/outside + focus.
//       T6.2 desktop collapse -> compact rail with reopen control
//       T6.3 rail reopen restores the sidebar
//       T6.4 sidebar Manage opens the session panel
//   T4  T4.1 fill the bounded 16-runtime pool
//       T4.2 create past the cap succeeds by retiring the exact expected idle LRU
//       T4.3 loaded count stays 16 and the retired runtime remains durable in catalog
//   T5  T5.1 Todo state never leaks A<->B
//       T5.2 Goal state never leaks A<->B
//       T5.3 Workflow state never leaks A<->B
//   T7  T7.4 mobile 390x844: same one-entry feature-menu contract.
//       T7.1 Android 390x844: feature menu trigger visible, Sessions opens drawer
//       T7.2 failed pick keeps the drawer/current session/error; successful pick closes
//       T7.3 picked session's transcript restores from the backend; the pick
//       preserves the mobile page's own pre-pick catalog order (no cross-page baseline)
//   T8  T8.1 fork draft lands in composer; parent→fork round-trip restores it before reload
//       T8.2 refresh keeps one durable fork row + identity and child ancestry/exclusion
//       T8.3 after refresh, parent and fork remain independently selectable (ancestry only)
//   T9  T9.1 selected-path focus default: only the active session's ancestor
//       path is expanded; the non-selected focus project root is closed
//       (desktop + phone + iPad portrait/landscape evidence screenshots)
//       T9.2 manual collapse of the selected path survives an explicit refresh
//       T9.3 non-selected manual expansion retires at the identity switch;
//       A→B→A with zero toggles under B never resurrects it
//
// Unrecorded hard-fail contracts (same exit-2 gate, no coverage-matrix ID):
//   * lifecycle attachment isolation on the real surface: the New cut's
//     source A keeps its exact chips + unsent draft in its view slot while
//     the fresh target's composer mounts FULLY EMPTY (zero chips, zero
//     draft), and returning to A restores the owner's exact chip names/order
//     and draft; the Fork cut's source B keeps its chip + unsent draft while
//     the fork child carries ONLY the fork editorText (zero chips, never the
//     source draft), and returning to B restores both exactly
//   * New from a loaded non-primary, and Fork: the active sidebar button's
//     data-session-id equals the panel Session id, differs from every
//     previous id, the catalog row is the loaded identity, the panel Session
//     file names that id, and the selected row intersects the sidebar
//     viewport (DOM presence is not enough). The read is taken before any
//     manual scroll — New/Fork do not auto-scroll. Same contract on the
//     existing desktop 1280x800 page and the T7 390x844 drawer. No coverage
//     id: record() stays on the documented matrix only.
//   * starred main assistant replies (no coverage id): a settled reply's
//     Star control persists across reload of the same browser context,
//     Unstar removes it and the current minimap, an identical body with a
//     different entry id stays independent, a second session shows none of
//     the first session's minimap entries, and a minimap click on a reply
//     outside the initial 240-item window mounts that reply with the
//     transcript projection still bounded by 240 mounted items and follow
//     left false. Desktop is the existing 1280x800 page; the same page is
//     also checked at 390x844. Console and page errors stay fatal.
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
  assertExactActiveSidebarRow,
  assertSidebarNewActionAbsent,
  closeManageSession,
  createFreshSessionViaManage,
  openManageSession,
  readActiveSidebarRow,
} from './lib/session-lifecycle.mjs';
import { openSessionsDrawer, closeSessionsDrawer } from './lib/feature-menu.mjs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const mockPort = process.env.RPI_MOCK_PORT || '';
const focusCwd = process.env.RPI_FOCUS_CWD || '';
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
  'T9.1', 'T9.2', 'T9.3', 'T9.4',
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
    if (detail !== null) fs.writeFileSync(path.join(evidence, 'sessions-c-return-failure.json'), JSON.stringify(detail, null, 2));
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
        const row = [...document.querySelectorAll('.session-sidebar__switch')]
          .find((candidate) => candidate.dataset.sessionId === s);
        if (!(row instanceof HTMLElement) || row.disabled) return false;
        const group = row.closest('.session-sidebar__group');
        const head = group?.querySelector('.session-sidebar__group-head');
        if (head instanceof HTMLElement && head.getAttribute('aria-expanded') === 'false') head.click();
        const box = row.getBoundingClientRect();
        return box.width > 0 && box.height > 0;
      },
      `${id}: session row ${sid} never became visible`,
      30000,
      sid,
    );
    const clicked = await page.evaluate((s) => {
      const row = [...document.querySelectorAll('.session-sidebar__switch')]
        .find((candidate) => candidate.dataset.sessionId === s);
      if (!(row instanceof HTMLElement) || row.disabled) return false;
      const box = row.getBoundingClientRect();
      if (box.width <= 0 || box.height <= 0) return false;
      row.click();
      return true;
    }, sid);
    if (!clicked) fail(`${id}: session row ${sid} was not visibly clickable`);
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
  // Lengths/counts ONLY (never prompt or reply text) — typed state for the
  // echo-timeout diagnosis, written by waitFor's existing failure dump.
  await waitFor(page, (r) => document.body.textContent.includes(r), id, timeoutMs, reply, (r) => ({
    replyLen: r.length, draftLen: (document.getElementById('prompt-input')?.value || '').length, attachments: document.querySelectorAll('.composer-attachment').length,
    sendLabel: document.getElementById('send-btn')?.getAttribute('aria-label') || '', sessionId: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
    follow: document.querySelector('main#transcript [data-runtime-key]')?.getAttribute('data-live-follow') || '', mountedRows: document.querySelector('main#transcript [data-runtime-key] .transcript-content')?.children.length ?? -1,
    scrollLog: (window.__starScrollLog || []).slice(-30),
  }));
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

const STAR_SURFACE = 'main#transcript [data-runtime-key] .transcript-content';
const PROJECTION_BOUND = 240;
const LONG_STAR_TURNS = 260;
// Append-regression repro (user-reported): the minimap jump is correct at
// first, then lands at the wrong offset once MORE real turns have appended
// to the SAME session. Reuses the pad construction below — never a new
// session.
const APPEND_STAR_TURNS = 40;
const STAR_MID_TURN = 100;

function replyStarsStorageKey(authority, sessionId) {
  return `rpi-web-reply-stars:${encodeURIComponent(authority)}:${encodeURIComponent(sessionId)}`;
}

async function readReplyStarSnapshot(page) {
  return page.evaluate(({ surface, bound }) => {
    const root = document.querySelector(surface);
    const buttons = root
      ? [...root.querySelectorAll('button.assistant-star-toggle')]
      : [];
    const stars = buttons.map((button) => {
      const row = button.closest('[data-item-id]');
      return {
        label: button.getAttribute('aria-label') || '',
        pressed: button.getAttribute('aria-pressed'),
        entryId: button.getAttribute('data-entry-id') || row?.getAttribute('data-entry-id') || '',
        itemId: row?.getAttribute('data-item-id') || '',
        text: (row?.querySelector('.assistant-text')?.textContent || '').replace(/\\s+/g, ' ').trim(),
      };
    });
    const minimap = document.querySelector('.conversation-with-minimap .starred-reply-minimap');
    const entries = minimap
      ? [...minimap.querySelectorAll('button[data-entry-id]')].map((button) => ({
        entryId: button.getAttribute('data-entry-id') || '',
        text: (button.textContent || '').replace(/\\s+/g, ' ').trim(),
      }))
      : [];
    const content = document.querySelector(`${surface}`);
    const mounted = content ? content.children.length : -1;
    const follow = document.querySelector('main#transcript [data-runtime-key]')?.getAttribute('data-live-follow') || '';
    const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '';
    const userStars = [...document.querySelectorAll('.msg--user button.assistant-star-toggle, .tool-card button.assistant-star-toggle, #side-chat-panel button.assistant-star-toggle, #todo-panel button.assistant-star-toggle')].length;
    return {
      stars,
      entries,
      mounted,
      bound,
      follow,
      active,
      userStars,
      saving: root ? (root.textContent || '').includes('Saving reply…') : false,
    };
  }, { surface: STAR_SURFACE, bound: PROJECTION_BOUND });
}

async function waitForSettledStar(page, marker, label) {
  await waitFor(page, (want) => {
    const root = document.querySelector('main#transcript [data-runtime-key] .transcript-content');
    if (!root || (root.textContent || '').includes('Saving reply…')) return false;
    return [...root.querySelectorAll('.msg--assistant')].some((row) => {
      const text = row.querySelector('.assistant-text')?.textContent || '';
      const button = row.querySelector('button.assistant-star-toggle');
      return text.includes(want)
        && button?.getAttribute('aria-label') === 'Star'
        && button.getAttribute('aria-pressed') === 'false'
        && (button.getAttribute('data-entry-id') || row.getAttribute('data-entry-id') || '') !== '';
    });
  }, label, 30000, marker);
}

function starForMarker(snapshot, marker) {
  return snapshot.stars.find((star) => star.text.includes(marker)) || null;
}

// Typed minimap-jump geometry (ids + px ONLY — never transcript body text):
// the row is located through its stamped assistant star button, mirroring
// App.tsx's entry-id resolution.
function starJumpGeometry(page, entryId) {
  return page.evaluate((want) => {
    const scroll = document.querySelector('main#transcript');
    const root = document.querySelector('main#transcript [data-runtime-key] .transcript-content');
    const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
      .find((candidate) => (candidate.getAttribute('data-entry-id')
        || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === want);
    const row = button?.closest('.msg--assistant') || null;
    const scrollBox = scroll?.getBoundingClientRect() || null;
    const rowBox = row?.getBoundingClientRect() || null;
    const first = root?.children[0] || null;
    const scrollTop = scroll?.scrollTop ?? null;
    const maxScroll = scroll ? scroll.scrollHeight - scroll.clientHeight : null;
    return {
      entryId: want,
      found: !!row,
      itemId: row?.getAttribute('data-item-id') || '',
      rectTop: rowBox ? rowBox.top : null,
      rowTopInViewport: rowBox && scrollBox ? rowBox.top - scrollBox.top : null,
      rowTopInContent: rowBox && scrollBox && scrollTop !== null ? rowBox.top - scrollBox.top + scrollTop : null,
      visibleOverlapPx: rowBox && scrollBox ? Math.min(rowBox.bottom, scrollBox.bottom) - Math.max(rowBox.top, scrollBox.top) : 0,
      viewport: scrollBox ? { top: scrollBox.top, bottom: scrollBox.bottom, height: scrollBox.height, width: scrollBox.width } : null,
      scrollTop,
      scrollHeight: scroll?.scrollHeight ?? null,
      clientHeight: scroll?.clientHeight ?? null,
      maxScroll,
      windowStartItemId: first?.getAttribute('data-item-id') || '',
      windowStartEntryId: first?.getAttribute('data-entry-id') || '',
      mounted: root ? root.children.length : -1,
      follow: document.querySelector('main#transcript [data-runtime-key]')?.getAttribute('data-live-follow') || '',
      minimapListed: [...document.querySelectorAll('.starred-reply-minimap button[data-entry-id]')]
        .some((candidate) => candidate.getAttribute('data-entry-id') === want),
    };
  }, entryId);
}

// Failure evidence: typed geometry JSON + desktop/mobile screenshots. The
// JSON carries ids and pixel geometry only — never transcript body text.
async function dumpStarJumpRegression(phase, page, sample) {
  fs.writeFileSync(
    path.join(evidence, 'sessions-star-append-jump-regression.json'),
    JSON.stringify({ phase, viewport: page.viewportSize(), geometry: sample, capturedAt: new Date().toISOString() }, null, 2),
  );
  await page.screenshot({ path: `${evidence}/sessions-star-append-jump-desktop.png`, fullPage: false });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.screenshot({ path: `${evidence}/sessions-star-append-jump-mobile.png`, fullPage: false });
  await page.setViewportSize({ width: 1280, height: 800 });
}
// Bounded rAF sampler: a real minimap jump must land its entryId target at |top|<=1px and hold it mounted, unique-count, non-empty itemId (re-read every frame) for 120 frames.
const STAR_JUMP_HOLD_FRAMES = 120;
const STAR_JUMP_LAND_BOUND_FRAMES = 900;
function dumpStarJumpSampler(label, entryId, phase, frames, viewport) {
  fs.writeFileSync(
    path.join(evidence, `sessions-star-jump-sampler-${label.replace(/[^a-zA-Z0-9]+/g, '-')}.json`),
    JSON.stringify({ label, entryId, phase, viewport, frames }, null, 2),
  );
}
async function sampledMinimapJumpClick(page, entryId, label, { mayExpand = false } = {}) {
  const jump = page.locator(`.starred-reply-minimap button[data-entry-id="${entryId}"]`);
  if (mayExpand && !(await jump.isVisible())) {
    const expand = page.getByRole('button', { name: 'Show starred replies', exact: true });
    const expandBox = await expand.boundingBox();
    if (!expandBox || expandBox.width < 44 || expandBox.height < 44) {
      fail(`${label}: minimap expand target is below 44px (${JSON.stringify(expandBox)})`);
    }
    await expand.click();
    await waitFor(page, () => {
      const rail = document.querySelector('.starred-reply-minimap');
      const box = rail?.getBoundingClientRect();
      return !!rail && !rail.classList.contains('is-collapsed') && !!box && box.width > 44;
    }, `${label}: minimap did not expand before the sampled jump`, 10000);
  }
  await page.evaluate((want) => {
    window.__starJumpFrames = [];
    window.__starJumpDone = false;
    window.__starJumpArmed = false;
    const onArm = (event) => {
      const button = event.target?.closest?.('.starred-reply-minimap button[data-entry-id]');
      if (!button || button.getAttribute('data-entry-id') !== want.id) return;
      window.__starJumpArmed = true;
      document.removeEventListener('click', onArm, true);
    };
    document.addEventListener('click', onArm, true);
    window.__starJumpDisarm = () => {
      document.removeEventListener('click', onArm, true);
      delete window.__starJumpDisarm;
    };
    const state = () => {
      const scroll = document.querySelector('main#transcript');
      const viewport = scroll ? scroll.getBoundingClientRect() : null;
      const buttons = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
        .filter((candidate) => (candidate.getAttribute('data-entry-id')
          || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === want.id);
      const row = buttons[0] ? buttons[0].closest('.msg--assistant') : null;
      const box = row && viewport ? row.getBoundingClientRect() : null;
      return {
        armed: window.__starJumpArmed === true,
        count: buttons.length,
        mounted: !!row,
        itemId: row?.getAttribute('data-item-id') || '',
        top: box && viewport ? box.top - viewport.top : null,
        follow: document.querySelector('main#transcript [data-runtime-key]')?.getAttribute('data-live-follow') || '',
        total: document.querySelector('main#transcript [data-runtime-key] .transcript-content')?.children.length ?? -1,
      };
    };
    let hold = -1;
    let frames = 0;
    const step = () => {
      frames += 1;
      const sample = state();
      if (!sample.armed) {
        window.__starJumpFrames.push({ phase: 'pre', ...sample });
      } else if (hold < 0) {
        if (sample.count > 1) {
          window.__starJumpFrames.push({ phase: 'dup', ...sample });
          window.__starJumpDone = true;
          return;
        }
        // |top|<=1 — a NEGATIVE top (overshoot above the viewport top, e.g.
        // -658) is NOT a landing, it is the reported drift bug.
        const landed = sample.count === 1 && sample.mounted && sample.itemId !== ''
          && sample.top !== null && Math.abs(sample.top) <= 1
          && sample.follow === 'false' && sample.total > 0 && sample.total <= want.mount;
        window.__starJumpFrames.push({ phase: landed ? 'landing' : 'probe', ...sample });
        if (landed) hold = 0;
      } else {
        hold += 1;
        window.__starJumpFrames.push({ phase: 'hold', hold, ...sample });
        if (hold >= want.hold) { window.__starJumpDone = true; return; }
      }
      if (frames >= want.bound) { window.__starJumpDone = true; return; }
      window.__starJumpRaf = requestAnimationFrame(step);
    };
    window.__starJumpRaf = requestAnimationFrame(step);
  }, { id: entryId, hold: STAR_JUMP_HOLD_FRAMES, bound: STAR_JUMP_LAND_BOUND_FRAMES, mount: PROJECTION_BOUND });
  await jump.click();
  try {
    await page.waitForFunction(() => window.__starJumpDone === true, undefined, { timeout: 30000 });
  } catch {
    const partial = await page.evaluate(() => window.__starJumpFrames || []);
    dumpStarJumpSampler(label, entryId, 'no-verdict', partial, page.viewportSize());
    fail(`${label}: the sampled minimap jump never reached a bounded verdict within 30s (entry ${entryId})`);
  }
  let frames;
  try {
    frames = await page.evaluate(() => window.__starJumpFrames || []);
  } finally {
    // Cleanup runs even when the frames read throws; the error propagates.
    await page.evaluate(() => {
      cancelAnimationFrame(window.__starJumpRaf || 0);
      if (window.__starJumpDisarm) window.__starJumpDisarm();
    });
  }
  const bail = (phase, message) => {
    dumpStarJumpSampler(label, entryId, phase, frames, page.viewportSize());
    fail(`${label}: ${message}`);
  };
  if (!frames.some((frame) => frame.armed)) {
    bail('never-armed', `the real minimap click never armed the sampler (entry ${entryId}, first frames ${JSON.stringify(frames.slice(0, 3))})`);
  }
  const dup = frames.find((frame) => frame.phase === 'dup');
  if (dup) {
    bail('duplicate-target', `the entryId matched ${dup.count} star toggles — the jump target is not unique (${JSON.stringify(dup)})`);
  }
  const landing = frames.findIndex((frame) => frame.phase === 'landing');
  if (landing < 0) {
    bail('never-landed', `the minimap jump never landed the target at |top|<=1px within the bounded window (entry ${entryId}, last frames ${JSON.stringify(frames.slice(-3))})`);
  }
  const held = frames.filter((frame) => frame.phase === 'hold');
  if (held.length < STAR_JUMP_HOLD_FRAMES) {
    bail('short-hold', `the jump landing held only ${held.length}/${STAR_JUMP_HOLD_FRAMES} frames (entry ${entryId})`);
  }
  const drifted = held.find((frame) => frame.count !== 1 || !frame.mounted || frame.itemId === ''
    || frame.top === null || Math.abs(frame.top) > 1
    || frame.follow !== 'false' || frame.total < 0 || frame.total > PROJECTION_BOUND);
  if (drifted) {
    bail('drifted', `the jump landing broke at hold frame ${drifted.hold} — |top|>1, unmount, lost itemId, duplicate count, follow!=false, or total>bound (entry ${entryId}, landing ${JSON.stringify(frames[landing])}, drifted ${JSON.stringify(drifted)})`);
  }
  dumpStarJumpSampler(label, entryId, 'passed', frames, page.viewportSize());
}

async function lastAssistantText(page) {
  return page.evaluate(() => {
    const nodes = document.querySelectorAll('.msg--assistant .assistant-text');
    return nodes.length ? nodes[nodes.length - 1].textContent || '' : '';
  });
}

const FEATURE_BUTTON_IDS = [
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
const FEATURE_MENU_BTN = '#feature-menu-btn';
const FEATURE_MENU = '#feature-menu';
const VIEWPORTS_6 = [
  [1920, 1080, 'desktop'], [1920, 600, 'desktop-600'], [1280, 633, 'desktop-633'],
  [390, 844, 'phone'], [834, 1194, 'ipad-portrait'], [1194, 834, 'ipad-landscape'],
];

async function openFeatureMenu(page) {
  const visible = () => page.evaluate((sel) => {
    const menu = document.querySelector(sel);
    if (!menu) return false;
    const style = getComputedStyle(menu);
    return !menu.hidden && style.display !== 'none' && style.visibility !== 'hidden'
      && menu.getBoundingClientRect().height > 0;
  }, FEATURE_MENU);
  if (!(await visible())) await page.click(FEATURE_MENU_BTN);
  await waitFor(page, (sel) => {
    const menu = document.querySelector(sel);
    if (!menu) return false;
    const style = getComputedStyle(menu);
    return !menu.hidden && style.display !== 'none' && style.visibility !== 'hidden'
      && menu.getBoundingClientRect().height > 0;
  }, 'feature menu did not open', 15000, FEATURE_MENU);
  await waitFor(page, (sel) => document.querySelector(sel)?.getAttribute('aria-expanded') === 'true',
    'feature-menu trigger did not report aria-expanded=true', 15000, FEATURE_MENU_BTN);
}

async function closeFeatureMenu(page, how) {
  if (how === 'escape') await page.keyboard.press('Escape');
  else if (how === 'outside') await page.click('#prompt-input');
  else await page.click(FEATURE_MENU_BTN);
  await waitFor(page, (sel) => {
    const menu = document.querySelector(sel);
    return !menu || menu.hidden || getComputedStyle(menu).display === 'none';
  }, `feature menu did not close via ${how}`, 15000, FEATURE_MENU);
  const focus = await page.evaluate(() => document.activeElement?.id || '');
  if (how === 'outside') {
    if (focus !== 'prompt-input') fail(`outside pointerdown did not preserve focus on the click target (focus=${focus})`);
  } else if (focus !== 'feature-menu-btn') {
    fail(`feature menu ${how} close did not return focus to #feature-menu-btn (focus=${focus})`);
  }
}

async function assertFeatureMenuSurface(page, id, { clickThrough = false } = {}) {
  const closed = await page.evaluate((ids) => {
    const header = document.querySelector('header');
    const menu = document.getElementById('feature-menu');
    const chatLink = document.getElementById('chat-console-link');
    const chatRect = chatLink?.getBoundingClientRect();
    const chatStyle = chatLink ? getComputedStyle(chatLink) : null;
    return {
      triggers: document.querySelectorAll('#feature-menu-btn').length,
      inHeader: !!header?.querySelector('#feature-menu-btn'),
      expanded: document.getElementById('feature-menu-btn')?.getAttribute('aria-expanded'),
      headerIds: ids.filter((x) => header?.querySelector(`#${x}`) !== null),
      sidebarIds: ids.filter((x) => document.querySelector(`#session-sidebar #${x}`) !== null),
      nav: !!document.querySelector('.session-sidebar__nav'),
      actionable: ids.some((x) => {
        const el = document.getElementById(x);
        const r = el?.getBoundingClientRect();
        return !!r && r.width > 0 && r.height > 0 && getComputedStyle(el).visibility !== 'hidden';
      }),
      duplicates: ids.filter((x) => document.querySelectorAll(`#${x}`).length > 1),
      menuClosed: !menu || menu.hidden || getComputedStyle(menu).display === 'none'
        || getComputedStyle(menu).visibility === 'hidden' || menu.getBoundingClientRect().height === 0,
      controls: ['model-select', 'thinking-select', 'feature-menu-btn'].every((x) => {
        const r = document.getElementById(x)?.getBoundingClientRect();
        return !!r && r.width > 0 && r.height > 0;
      }),
      brand: {
        inBrandNav: !!document.querySelector('header .brand-switch > a#chat-console-link'),
        inMenu: !!menu && !!chatLink && menu.contains(chatLink),
        href: chatLink?.getAttribute('href') ?? '',
        width: chatRect?.width ?? 0,
        height: chatRect?.height ?? 0,
        visible: !!chatStyle && chatStyle.display !== 'none' && chatStyle.visibility !== 'hidden'
          && !!chatRect && chatRect.width > 0 && chatRect.height > 0,
      },
    };
  }, FEATURE_BUTTON_IDS);
  if (closed.triggers !== 1 || !closed.inHeader || closed.expanded !== 'false') {
    fail(`${id}: expected exactly ONE closed #feature-menu-btn in the header`);
  }
  const headerGlyphs = await page.evaluate(() => {
    const header = document.querySelector('header');
    const trigger = document.getElementById('feature-menu-btn');
    return {
      navToggles: document.querySelectorAll('header .panel-toggle').length,
      featureIsSoleToggle: document.querySelector('header .panel-toggle')?.id === 'feature-menu-btn',
      feature: trigger?.textContent?.trim() ?? '',
      hamburgerCount: (header?.textContent?.match(/☰/g) ?? []).length,
      dots: header?.textContent?.includes('⋮') ?? false,
      visibleMenuLabel: trigger?.querySelector('.feature-menu-btn__label') != null
        || /(^|\s)Menu(\s|$)/.test(header?.textContent ?? ''),
    };
  });
  if (headerGlyphs.navToggles !== 1 || !headerGlyphs.featureIsSoleToggle) fail(`${id}: the header must keep exactly ONE nav toggle — the feature menu trigger (got ${JSON.stringify(headerGlyphs)})`);
  if (headerGlyphs.hamburgerCount !== 1 || headerGlyphs.feature !== '☰') fail(`${id}: the feature menu trigger must be the ONE ☰ glyph (got ${JSON.stringify(headerGlyphs)})`);
  if (headerGlyphs.dots || headerGlyphs.visibleMenuLabel) fail(`${id}: the header menu trigger must not render ⋮ or a visible Menu label (got ${JSON.stringify(headerGlyphs)})`);
  if (closed.headerIds.length || closed.sidebarIds.length || closed.nav) {
    fail(`${id}: feature entries still live in the header chrome or the session sidebar`);
  }
  if (!closed.menuClosed || closed.actionable) fail(`${id}: feature menu is visible/actionable before the trigger`);
  if (closed.duplicates.length) fail(`${id}: duplicate feature ids: ${closed.duplicates.join(', ')}`);
  if (!closed.controls) fail(`${id}: the feature entry crowded out header controls`);
  if (!closed.brand.inBrandNav || closed.brand.inMenu) {
    fail(`${id}: Chat entry must live in the brand nav as an independent link`);
  }
  if (closed.brand.href !== '/chat') fail(`${id}: brand Chat entry lost href=/chat`);
  if (!closed.brand.visible || closed.brand.width < 44 || closed.brand.height < 44) {
    fail(`${id}: brand Chat entry is not a visible 44px target (${closed.brand.width}x${closed.brand.height})`);
  }

  await openFeatureMenu(page);
  const open = await page.evaluate((ids) => {
    const menu = document.getElementById('feature-menu');
    const box = menu.getBoundingClientRect();
    const header = document.querySelector('header').getBoundingClientRect();
    const input = document.getElementById('prompt-input');
    const composer = (input.closest('footer') ?? input).getBoundingClientRect();
    const items = ids.map((x) => {
      const el = document.getElementById(x);
      el.scrollIntoView({ block: 'nearest' });
      const r = el.getBoundingClientRect();
      const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
      return {
        id: x,
        count: document.querySelectorAll(`#${x}`).length,
        height: r.height,
        inMenu: menu.contains(el),
        inside: r.top >= box.top && r.bottom <= box.bottom && r.left >= box.left && r.right <= box.right
          && r.top >= 0 && r.bottom <= innerHeight && r.left >= 0 && r.right <= innerWidth,
        hit: hit === el || el.contains(hit),
        pressed: el.getAttribute('aria-pressed'),
      };
    });
    return {
      top: box.top, bottom: box.bottom, left: box.left, right: box.right,
      headerBottom: header.bottom, composerTop: composer.top, items,
      menuItems: [...menu.children].filter((child) => child.tagName === 'BUTTON' || child.tagName === 'A').length,
      inHeader: document.querySelector('header').contains(menu),
      inSidebar: !!document.getElementById('session-sidebar')?.contains(menu),
      expanded: document.getElementById('feature-menu-btn').getAttribute('aria-expanded'),
    };
  }, FEATURE_BUTTON_IDS);
  if (open.expanded !== 'true') fail(`${id}: feature-menu trigger did not report aria-expanded=true`);
  if (open.inHeader || open.inSidebar) fail(`${id}: #feature-menu must not live inside the header or the session sidebar`);
  if (open.top < open.headerBottom) fail(`${id}: feature menu covers the header`);
  if (open.bottom > open.composerTop) fail(`${id}: feature menu covers the composer`);
  if (open.left < 0 || open.right > (await page.evaluate(() => innerWidth))) fail(`${id}: feature menu leaves the viewport`);
  if (open.menuItems !== 13) fail(`${id}: feature menu has ${open.menuItems} entries, expected 13`);
  for (const item of open.items) {
    if (item.count !== 1) fail(`${id}: ${item.id} is not unique in the open feature menu`);
    if (!item.inMenu) fail(`${id}: ${item.id} is not inside #feature-menu`);
    if (item.height < 44 || !item.inside || !item.hit) fail(`${id}: ${item.id} is not a reachable 44px action`);
    if (item.pressed !== 'false') fail(`${id}: ${item.id} aria-pressed=${item.pressed}`);
  }
  // #sidebar-menu-item dedicated contract: FIRST menu entry, real 44px hit
  // target, and a real click that drives the drawer with aria-pressed in
  // sync (kept out of the 12-panel click-through loop above on purpose).
  const sessionsItem = await page.evaluate(() => {
    const menu = document.getElementById('feature-menu');
    const item = document.getElementById('sidebar-menu-item');
    if (!menu || !item) return null;
    const first = menu.children[0];
    const box = item.getBoundingClientRect();
    const hit = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
    return {
      firstId: first instanceof HTMLElement ? first.id : first?.tagName || '',
      inside: menu.contains(item),
      hit: hit === item || item.contains(hit),
      width: box.width,
      height: box.height,
    };
  });
  if (!sessionsItem || sessionsItem.firstId !== 'sidebar-menu-item' || !sessionsItem.inside) {
    fail(`${id}: #sidebar-menu-item must be the FIRST feature menu entry (got ${JSON.stringify(sessionsItem)})`);
  }
  if (!sessionsItem.hit || sessionsItem.width < 44 || sessionsItem.height < 44) {
    fail(`${id}: #sidebar-menu-item is not a reachable 44px action (got ${JSON.stringify(sessionsItem)})`);
  }
  const pressedBefore = await page.evaluate(() => document.getElementById('sidebar-menu-item')?.getAttribute('aria-pressed'));
  const wantOpen = pressedBefore !== 'true';
  await page.click('#sidebar-menu-item');
  await waitFor(page, (open) => {
    const item = document.getElementById('sidebar-menu-item');
    return document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === open
      && item?.getAttribute('aria-pressed') === (open ? 'true' : 'false');
  }, `${id}: the Sessions entry real click never drove the drawer with pressed in sync`, 30000, wantOpen);
  await openSessionsDrawer(page);
  if (clickThrough) {
    const panels = {
      'todo-toggle-btn': ['#todo-panel', '#todo-close-btn'],
      'activity-toggle-btn': ['#activity-panel', '#activity-close-btn'],
      'goal-panel-btn': ['#goal-panel', '#goal-close-btn'],
      'workflow-toggle-btn': ['#workflow-panel', '#workflow-close-btn'],
      'sidechat-toggle-btn': ['#side-chat-panel', '#side-chat-panel .panel-close'],
      'subagents-toggle-btn': ['#subagents-panel', '#subagents-close-btn'],
      'personas-toggle-btn': ['#personas-panel', '#personas-close-btn'],
      'session-toggle-btn': ['#session-panel', '#session-close-btn'],
      'loop-toggle-btn': ['#loop-panel', '#loop-close-btn'],
      'processes-toggle-btn': ['#processes-panel', '#processes-close-btn'],
      'file-explorer-toggle-btn': ['.file-explorer', '.file-explorer__close'],
      'settings-toggle-btn': ['#settings-panel', '#settings-close-btn'],
    };
    for (const [item, [panel, close]] of Object.entries(panels)) {
      await openFeatureMenu(page);
      await page.click(`#${item}`);
      await waitFor(page, (sel) => !!document.querySelector(sel), `${id}: ${item} did not open ${panel}`, 15000, panel);
      await page.click(close);
      await waitFor(page, (sel) => !document.querySelector(sel), `${id}: ${item} panel did not close`, 15000, panel);
    }
  }
  for (const how of ['escape', 'outside', 'trigger']) {
    await openFeatureMenu(page);
    await closeFeatureMenu(page, how);
  }
}

async function assertSessionGeometry(page, id) {
  const original = page.viewportSize();
  for (const [width, height, name] of VIEWPORTS_6) {
    await page.setViewportSize({ width, height });
    if (width <= 720) {
      await waitFor(page, () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') !== true,
        `${id}: ${name} drawer did not settle closed`, 30000);
      await openSessionsDrawer(page);
    } else if (!(await page.evaluate(() => !!document.querySelector('.app-layout--drawer-open')))) {
      await page.click('#rail-reopen-btn');
    }
    await waitFor(page, () => {
      const list = document.querySelector('.session-sidebar__list');
      return !!list && list.getBoundingClientRect().height > 0;
    }, `${id}: ${name} Sessions list never became visible`, 30000);
    await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    const geom = await page.evaluate((ids) => {
      const list = document.querySelector('.session-sidebar__list');
      document.querySelector('.session-sidebar__row--active')?.scrollIntoView({ block: 'nearest' });
      const box = list.getBoundingClientRect();
      const fullRows = [...document.querySelectorAll('.session-sidebar__row')].filter((row) => {
        const r = row.getBoundingClientRect();
        if (r.height < 44 || r.top < box.top || r.bottom > box.bottom || r.top < 0 || r.bottom > innerHeight) return false;
        const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
        return hit === row || row.contains(hit);
      }).length;
      return {
        listHeight: list.clientHeight,
        fullRows,
        nav: !!document.querySelector('.session-sidebar__nav'),
        featureIds: ids.filter((x) => document.querySelector(`#session-sidebar #${x}`) !== null),
      };
    }, FEATURE_BUTTON_IDS);
    if (geom.nav || geom.featureIds.length) fail(`${id}: ${name} sidebar still contains feature nav/entries`);
    if (geom.listHeight < 200) fail(`${id}: ${name} Sessions list ${geom.listHeight}px < 200px`);
    if (geom.fullRows < 1) fail(`${id}: ${name} shows no fully visible session row`);
    await page.screenshot({ path: `${evidence}/sessions-t6-1-${name}.png` });
  }
  await page.setViewportSize(original ?? { width: 1280, height: 800 });
  if (!(await page.evaluate(() => !!document.querySelector('.app-layout--drawer-open')))) await page.click('#rail-reopen-btn');
  await waitFor(page, () => Math.abs((document.querySelector('.session-sidebar')?.getBoundingClientRect().width ?? 0) - 195.5) <= 1,
    `${id}: desktop rail did not settle open after the viewport sweep`, 30000);
}

async function clickFeature(page, selector) {
  await openFeatureMenu(page);
  await page.click(selector);
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
        importedAliasFor: '',
        importedAliasForeignId: '',
        importedAliasActive: false,
        armImportedAlias(nativeId, foreignId) {
          this.importedAliasFor = nativeId;
          this.importedAliasForeignId = foreignId;
          this.importedAliasActive = true;
        },
        clearImportedAlias() {
          this.importedAliasFor = '';
          this.importedAliasForeignId = '';
          this.importedAliasActive = false;
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
          this.addEventListener('message', (event) => {
            if (event.__rpiInjected || !control.importedAliasActive) return;
            let frame;
            try { frame = JSON.parse(event.data); } catch { return; }
            const rows = frame?.type === 'response' && frame.success === true && Array.isArray(frame.data?.sessions)
              ? frame.data.sessions : null;
            const aliasIndex = rows?.findIndex((row) => row && row.sessionId === control.importedAliasFor) ?? -1;
            if (aliasIndex < 0) return;
            event.stopImmediatePropagation();
            const clone = JSON.parse(JSON.stringify(frame));
            const alias = clone.data.sessions[aliasIndex];
            alias.nativeSessionId = alias.sessionId;
            if (!alias.nativePath) alias.nativePath = alias.path;
            alias.sessionId = control.importedAliasForeignId;
            alias.status = 'already_imported';
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
    // Unrecorded: at the existing desktop shell (>=1280, and above the
    // ≤720px drawer breakpoint) the New Session footer control matches the
    // center composer #prompt-input height. Padding is not part of the contract.
    const newSessionHeight = await page.evaluate(() => {
      const box = (selector) => {
        const el = document.querySelector(selector);
        if (!el) return null;
        const rect = el.getBoundingClientRect();
        return { width: rect.width, height: rect.height };
      };
      return {
        innerWidth: window.innerWidth,
        narrow: window.matchMedia('(max-width: 720px)').matches,
        prompt: box('#prompt-input'),
        button: box('#sidebar-manage-btn'),
      };
    });
    if (newSessionHeight.innerWidth < 1280 || newSessionHeight.narrow) {
      await page.screenshot({ path: `${evidence}/sessions-t0-boot.png`, fullPage: true });
      fail(`New Session height: desktop shell was not open (${JSON.stringify(newSessionHeight)})`);
    }
    if (!newSessionHeight.prompt || newSessionHeight.prompt.width <= 0 || newSessionHeight.prompt.height <= 0
      || !newSessionHeight.button || newSessionHeight.button.width <= 0 || newSessionHeight.button.height <= 0) {
      await page.screenshot({ path: `${evidence}/sessions-t0-boot.png`, fullPage: true });
      fail(`New Session height: #prompt-input or button is not visible (${JSON.stringify(newSessionHeight)})`);
    }
    if (Math.abs(newSessionHeight.button.height - newSessionHeight.prompt.height) > 1) {
      await page.screenshot({ path: `${evidence}/sessions-t0-boot.png`, fullPage: true });
      fail(`New Session height ${newSessionHeight.button.height}px differs from #prompt-input ${newSessionHeight.prompt.height}px`);
    }
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
    // Unrecorded: one New from completed loaded non-primary C, after the
    // abort contract. Created at 390x844, then the same id is checked on the
    // reopened desktop rail. No second create, so T4's loaded count is unchanged.
    await clickRow(page, sessionC, 'lifecycle: switch to completed non-primary C before New');
    await page.setViewportSize({ width: 390, height: 844 });
    await waitFor(page, () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') !== true, 'lifecycle: 390x844 sidebar did not settle closed');
    await openSessionsDrawer(page);
    await waitFor(page, () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true && (document.querySelector('.session-sidebar')?.getBoundingClientRect().width ?? 0) > 0, 'lifecycle: 390x844 drawer did not open before New');
    const nonPrimaryNew = await createFreshSessionViaManage(page, waitFor, 'lifecycle: New from loaded non-primary', { closePanel: false });
    await waitFor(page, () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') !== true, 'lifecycle: mobile New did not close the drawer');
    await openSessionsDrawer(page);
    await waitFor(page, () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true && (document.querySelector('.session-sidebar')?.getBoundingClientRect().width ?? 0) > 0, 'lifecycle: 390x844 drawer did not reopen after New');
    const nonPrimaryMobile = await readActiveSidebarRow(page);
    try {
      assertExactActiveSidebarRow(nonPrimaryMobile, 'lifecycle: mobile New from loaded non-primary', { previousIds: [sessionA, sessionB, sessionC, nonPrimaryNew.beforeSessionId] });
    } catch (error) {
      fail(error instanceof Error ? error.message : String(error));
    }
    if (nonPrimaryNew.sessionId !== nonPrimaryMobile.active) fail(`lifecycle: mobile New did not activate its catalog row: ${JSON.stringify({ nonPrimaryNew, nonPrimaryMobile })}`);
    await page.screenshot({ path: `${evidence}/sessions-new-from-loaded-nonprimary-390.png`, fullPage: false });
    await page.setViewportSize({ width: 1280, height: 800 });
    if (!(await page.evaluate(() => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true))) await page.click('#rail-reopen-btn');
    await waitFor(page, () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true && Math.abs((document.querySelector('.session-sidebar')?.getBoundingClientRect().width ?? 0) - 195.5) <= 1, 'lifecycle: desktop rail did not reopen after mobile New');
    const nonPrimaryDesktop = await readActiveSidebarRow(page);
    try {
      assertExactActiveSidebarRow(nonPrimaryDesktop, 'lifecycle: desktop New from loaded non-primary', { previousIds: [sessionA, sessionB, sessionC, nonPrimaryNew.beforeSessionId] });
    } catch (error) {
      fail(error instanceof Error ? error.message : String(error));
    }
    if (nonPrimaryDesktop.active !== nonPrimaryNew.sessionId) fail(`lifecycle: desktop rail did not keep the mobile New id: ${JSON.stringify({ nonPrimaryNew, nonPrimaryDesktop })}`);
    await page.screenshot({ path: `${evidence}/sessions-new-from-loaded-nonprimary-desktop.png`, fullPage: false });
    await closeManageSession(page, waitFor, 'lifecycle: close panel after New from loaded non-primary');
    knownIds.push(nonPrimaryNew.sessionId);
    {
      const afterNonPrimary = await rowIds(page);
      assertStableSessionOrder(afterNonPrimary, catalogOrder, 'lifecycle: after New from loaded non-primary');
      catalogOrder = afterNonPrimary;
    }
    await clickRow(page, sessionC, 'lifecycle: restore completed C after New');

    /* ---------------- T6: desktop rail collapse/reopen + sidebar controls ---------------- */
    await closeManageSession(page, waitFor, 'T6: close leftover session panel before rail collapse');
    // T6.1: one header entry, 13 click-through actions, six-viewport list geometry.
    await assertFeatureMenuSurface(page, 'T6.1', { clickThrough: true });
    await assertSessionGeometry(page, 'T6.1');
    record('T6.1');
    // T6.2: collapse the rail; the header feature entry stays reachable.
    await page.click('#sidebar-collapse-btn');
    await waitFor(
      page,
      () => {
        const layout = document.querySelector('.app-layout');
        const reopen = document.getElementById('rail-reopen-btn');
        const sidebar = document.querySelector('.session-sidebar');
        const list = document.querySelector('.session-sidebar__list');
        if (!layout || !sidebar || !reopen) return false;
        if (getComputedStyle(reopen).display === 'none') return false;
        if (list && getComputedStyle(list).display !== 'none') return false;
        return !layout.classList.contains('app-layout--drawer-open') && sidebar.getBoundingClientRect().width <= 60;
      },
      'T6.2: desktop rail never collapsed to the compact strip',
      15000
    );
    await openFeatureMenu(page);
    await closeFeatureMenu(page, 'escape');
    record('T6.2');
    await page.screenshot({ path: `${evidence}/sessions-t6-collapsed.png`, fullPage: true });
    // T6.3: reopen via the rail's dedicated control.
    await page.click('#rail-reopen-btn');
    await waitFor(
      page,
      () => {
        const sidebar = document.querySelector('.session-sidebar');
        const list = document.querySelector('.session-sidebar__list');
        const search = document.querySelector('.session-sidebar__search-box');
        const nav = document.querySelector('.session-sidebar__nav');
        if (!sidebar || !list || !search) return false;
        return document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true
          && Math.abs(sidebar.getBoundingClientRect().width - 195.5) <= 1
          && getComputedStyle(list).display !== 'none'
          && list.getBoundingClientRect().height > 0
          && getComputedStyle(search).display !== 'none'
          && nav === null;
      },
      'T6.3: rail reopen never restored the sidebar session surface'
    );
    record('T6.3');
    // T6.4: Manage opens the session panel, and the removed header New action
    // remains absent before and after the panel closes.
    await openManageSession(page, waitFor, 'T6.4');
    await closeManageSession(page, waitFor, 'T6.4');
    await waitFor(
      page,
      () => {
        const sidebar = document.querySelector('.session-sidebar');
        const list = document.querySelector('.session-sidebar__list');
        const manage = document.getElementById('sidebar-manage-btn');
        if (!sidebar || !list || !manage) return false;
        if (getComputedStyle(list).display === 'none') return false;
        if (getComputedStyle(manage).display === 'none') return false;
        return Math.abs(sidebar.getBoundingClientRect().width - 195.5) <= 1
          && document.querySelector('.session-sidebar__nav') === null;
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
    const forkRow = await readActiveSidebarRow(page);
    try {
      assertExactActiveSidebarRow(forkRow, 'T8.1: Fork', { previousIds: [parentId, sessionA, sessionB, sessionC] });
    } catch (error) {
      fail(error instanceof Error ? error.message : String(error));
    }
    if (forkRow.active !== forkSession) {
      fail(`T8.1: Fork active row is not the activated child: ${JSON.stringify({ forkRow, forkSession })}`);
    }
    await page.screenshot({ path: `${evidence}/sessions-t8-fork-row-viewport.png`, fullPage: false });
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
    await page.setViewportSize({ width: 390, height: 844 });
    await openSessionsDrawer(page);
    await waitFor(
      page,
      () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true
        && (document.querySelector('.session-sidebar')?.getBoundingClientRect().width ?? 0) > 0,
      'T8.1: 390x844 drawer did not open for the fork row',
    );
    const forkRowMobile = await readActiveSidebarRow(page);
    try {
      assertExactActiveSidebarRow(forkRowMobile, 'T8.1: Fork at 390x844', { previousIds: [parentId, sessionA, sessionB, sessionC] });
    } catch (error) {
      fail(error instanceof Error ? error.message : String(error));
    }
    if (forkRowMobile.active !== forkSession) {
      fail(`T8.1: 390x844 Fork active row is not the activated child: ${JSON.stringify({ forkRowMobile, forkSession })}`);
    }
    await page.screenshot({ path: `${evidence}/sessions-t8-fork-row-viewport-390.png`, fullPage: false });
    await page.setViewportSize({ width: 1280, height: 800 });
    await waitFor(
      page,
      () => Math.abs((document.querySelector('.session-sidebar')?.getBoundingClientRect().width ?? 0) - 195.5) <= 1,
      'T8.1: desktop sidebar did not return after the 390x844 fork check',
    );
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
    // A, B, C, the unrecorded New-from-loaded-nonprimary child, and the T8
    // fork child are the five loaded runtimes the earlier phases leave behind.
    if (loadedBeforeCap !== 5) {
      fail(`T4.1: invalid authoritative loaded count before cap (${loadedBeforeCap}, expected A/B/C/nonprimary-new/fork)`);
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

    /* ---------------- T9: sidebar selected-path focus default ----------------
     * The default tree expands ONLY the active session's ancestor path
     * (provider root + its own project subgroup); every other root/subgroup
     * is closed. A manual collapse of the selected path survives an explicit
     * refresh; a manual expansion of a non-selected group is retired by an
     * identity switch, and A→B→A with zero toggles under B never resurrects
     * it. The fixture Z session (distinct project) is created AFTER the T4
     * cap so the T4.1 loaded-count fixture is untouched. ------------------- */
    if (!focusCwd) fail('T9: RPI_FOCUS_CWD (the sessions-focus-zoo fixture directory) was not provided by sessions.sh');
    {
      const focusProject = focusCwd.split('/').filter(Boolean).pop() || '';
      if (!focusProject) fail('T9: cannot derive the focus project label from RPI_FOCUS_CWD');
      await createFreshSessionViaManage(page, waitFor, 'T9 fixture: session Z', { closePanel: true, explicitCwd: focusCwd });
      const sessionZ = await waitForNewRow(page, knownIds, 'T9 fixture: session Z row never appeared');
      knownIds.push(sessionZ);
      /* Identity switches under the selected-only default must go through
       * the EXACT-sid search: a non-selected row inside a CLOSED group is
       * not rendered at all, so clickRow's auto-expand never sees it. The
       * search renders the exact target (groups open without explicit
       * intent), the click activates it, and clearing the query restores
       * the selected-default tree for the NEW identity. */
      const switchViaSearch = async (sid, id) => {
        await page.fill('#session-sidebar-search', sid);
        await waitFor(page, (s) => [...document.querySelectorAll('.session-sidebar__switch')].some((r) => r.dataset.sessionId === s),
          `${id}: the exact-sid search never rendered row ${sid}`, 15000, sid);
        await page.locator(`.session-sidebar__switch[data-session-id="${sid}"]`).first().click();
        await waitFor(page, (s) => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId === s,
          `${id}: ${sid} never became active`, 30000, sid);
        await page.fill('#session-sidebar-search', '');
        await waitFor(page, () => document.querySelector('#session-sidebar-search-clear') === null,
          `${id}: the search query never cleared`, 15000);
        await waitFor(page, (s) => [...document.querySelectorAll('.session-sidebar__switch')].some((r) => r.dataset.sessionId === s),
          `${id}: the re-selected row never re-rendered after clearing the search`, 15000, sid);
      };
      await switchViaSearch(sessionA, 'T9: return to A before the default-focus assertions');
      const groupState = () => page.evaluate(() =>
        [...document.querySelectorAll('.session-sidebar__group-head')].map((h) => ({
          kind: h.getAttribute('data-group-kind') || '',
          provider: h.getAttribute('data-provider') || '',
          label: h.querySelector('.session-sidebar__group-label')?.textContent || '',
          expanded: h.getAttribute('aria-expanded') === 'true',
        })));
      /* T9.1 DEFAULT FOCUS: the only expanded rpi project is A's exact
       * workspace key; the focus project is exactly one closed rpi head. */
      const heads9 = await groupState();
      const roots9 = heads9.filter((h) => h.kind === 'provider');
      if (roots9.length !== 1 || roots9[0].provider !== 'rpi' || roots9[0].expanded !== true) {
        fail(`T9.1: the selected provider root is not the single expanded root: ${JSON.stringify(roots9)}`);
      }
      const expandedProjects = heads9.filter((h) => h.kind === 'project' && h.provider === 'rpi' && h.expanded);
      if (expandedProjects.length !== 1 || expandedProjects[0].label !== 'workspace') {
        fail(`T9.1: expanded rpi project is not exactly A's workspace: ${JSON.stringify(expandedProjects)}`);
      }
      const zooHeads = heads9.filter((h) => h.kind === 'project' && h.provider === 'rpi' && h.label === focusProject);
      if (zooHeads.length !== 1 || zooHeads[0].expanded) {
        fail(`T9.1: expected exactly one closed rpi focus project, got ${JSON.stringify(zooHeads)}`);
      }
      if ((await rowIds(page)).includes(sessionZ)) fail('T9.1: the non-selected focus row renders while its root is closed');
      const projectHead = (label) => page.locator('.session-sidebar__group-head[data-provider="rpi"][data-subgroup-kind="project"]')
        .filter({ has: page.locator('.session-sidebar__group-label').getByText(label, { exact: true }) });
      const clickExactProject = async (label, id) => {
        const head = projectHead(label);
        const count = await head.count();
        if (count !== 1) fail(`${id}: expected exactly one rpi project head "${label}", got ${count}`);
        await head.click();
      };
      record('T9.1');

      /* T9.2 MANUAL COLLAPSE of the selected path survives an explicit refresh. */
      await clickExactProject('workspace', 'T9.2');
      await waitFor(page, (s) => ![...document.querySelectorAll('.session-sidebar__switch')].some((r) => r.dataset.sessionId === s),
        'T9.2: collapsing the selected path never hid the active row', 15000, sessionA);
      const refreshBase = await page.evaluate(() => (window.__rpiSessionAttachmentTest?.frames || []).length);
      await page.click('#session-sidebar-refresh-btn');
      await waitFor(
        page,
        (from) => {
          const frames = (window.__rpiSessionAttachmentTest?.frames || []).slice(from);
          const lists = frames.filter((frame) => frame.type === 'session_list');
          return lists.length >= 1
            && frames.some((frame) => frame.type === 'response'
              && frame.command === 'session_list'
              && frame.id === lists[lists.length - 1].id
              && frame.success === true);
        },
        'T9.2: the explicit refresh session_list never completed',
        30000,
        refreshBase,
      );
      if ((await rowIds(page)).includes(sessionA)) fail('T9.2: the refresh re-expanded the manually collapsed selected path');
      await clickExactProject('workspace', 'T9.2 re-expand');
      await waitFor(page, (s) => [...document.querySelectorAll('.session-sidebar__switch')].some((r) => r.dataset.sessionId === s),
        'T9.2: re-expanding the selected path never restored the active row', 15000, sessionA);
      record('T9.2');

      /* T9.3 RETIREMENT: the A-identity manual expansion of the non-selected
       * Z group dies at the switch to B and NEVER resurrects on A→B→A. */
      await clickExactProject(focusProject, 'T9.3');
      await waitFor(page, (s) => [...document.querySelectorAll('.session-sidebar__switch')].some((r) => r.dataset.sessionId === s),
        'T9.3: the manually expanded focus row never rendered', 15000, sessionZ);
      await switchViaSearch(sessionB, 'T9.3: switch to B for the retirement leg');
      if ((await rowIds(page)).includes(sessionZ)) fail('T9.3: the A-identity manual expansion survived the switch to B');
      await switchViaSearch(sessionA, 'T9.3: switch back to A with zero toggles under B');
      if ((await rowIds(page)).includes(sessionZ)) fail('T9.3: A→B→A resurrected the retired expansion (stale intent not reset)');
      if (!(await rowIds(page)).includes(sessionA)) fail('T9.3: returning to A never re-expanded its own selected path');
      await page.screenshot({ path: `${evidence}/sessions-focus-aba.png`, fullPage: true });
      record('T9.3');

      /* T9.4 FOUR-VIEWPORT selected-default evidence: desktop / phone / iPad
       * portrait + landscape all show the same selected-default tree. */
      for (const [w, h, name] of [[1280, 800, 'desktop'], [390, 844, 'phone'], [834, 1194, 'ipad-portrait'], [1194, 834, 'ipad-landscape']]) {
        await page.setViewportSize({ width: w, height: h });
        if (w <= 720) {
          await waitFor(page, () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') !== true,
            `T9.4: the ${name} breakpoint never closed the drawer`, 30000);
          await openSessionsDrawer(page);
        } else {
          const reopen = page.locator('#rail-reopen-btn');
          if (await reopen.isVisible()) await reopen.click();
        }
        await waitFor(page, () => {
          const sidebarList = document.querySelector('.session-sidebar__list');
          const box = sidebarList?.getBoundingClientRect();
          return document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true
            && !!box && box.width > 0 && box.height > 0;
        }, `T9.4: the ${name} sidebar never became visible`, 30000);
        const headsV = await groupState();
        const rootsV = headsV.filter((x) => x.kind === 'provider');
        if (rootsV.length !== 1 || rootsV[0].provider !== 'rpi' || !rootsV[0].expanded) {
          fail(`T9.4: the ${name} viewport lost the selected provider root: ${JSON.stringify(rootsV)}`);
        }
        const openV = headsV.filter((x) => x.kind === 'project' && x.provider === 'rpi' && x.expanded);
        if (openV.length !== 1 || openV[0].label !== 'workspace') {
          fail(`T9.4: the ${name} viewport expanded project is not exactly workspace: ${JSON.stringify(openV)}`);
        }
        const zooV = headsV.filter((x) => x.kind === 'project' && x.provider === 'rpi' && x.label === focusProject);
        if (zooV.length !== 1 || zooV[0].expanded) fail(`T9.4: expected exactly one closed rpi focus project, got ${JSON.stringify(zooV)}`);
        await page.screenshot({ path: `${evidence}/sessions-focus-default-${name}.png` });
        const list = page.locator('.session-sidebar__list');
        const scrollTop = await list.evaluate((el) => el.scrollTop);
        await projectHead(focusProject).scrollIntoViewIfNeeded();
        await page.screenshot({ path: `${evidence}/sessions-focus-default-${name}-closedbranch.png` });
        await list.evaluate((el, y) => { el.scrollTop = y; }, scrollTop);
      }
      await page.setViewportSize({ width: 1280, height: 800 });
      record('T9.4');
    }
    markStage('t5WorkflowIsolationStart');

    /* ---------------- T5: Todo/Goal/Workflow isolation ---------------- */
    // T5.1 Todo: create in A, verify absent in B and back in A.
    await clickRow(page, sessionA, 'T5.1: switch to A for the todo leak test');
    await clickFeature(page, '#todo-toggle-btn');
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
    await clickFeature(page, '#todo-toggle-btn');
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
    await clickFeature(page, '#todo-toggle-btn');
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
    await clickFeature(page, '#goal-panel-btn');
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
    await clickFeature(page, '#goal-panel-btn');
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
    await clickFeature(page, '#workflow-toggle-btn');
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
    await clickFeature(page, '#workflow-toggle-btn');
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
    await assertFeatureMenuSurface(mobile, 'T7.4');
    record('T7.4');
    const menuDisplay = await mobile.evaluate(
      () => getComputedStyle(document.getElementById('feature-menu-btn')).display
    );
    if (menuDisplay === 'none') {
      fail('T7.1: #feature-menu-btn is hidden at 390x844 (CSS cascade regression)');
    }
    await openSessionsDrawer(mobile);
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
      'T7.1: session drawer never opened from the feature menu'
    );
    // Unrecorded: the open 390x844 drawer keeps the New Session footer a
    // >=44px touch target and the same height as the center #prompt-input.
    // Padding is not part of the contract. mobile_test.mjs owns the same
    // touch bound on its own page; this page is the sessions lane surface.
    const mobileNewSessionHeight = await mobile.evaluate(() => {
      const box = (selector) => {
        const el = document.querySelector(selector);
        if (!el) return null;
        const rect = el.getBoundingClientRect();
        return { width: rect.width, height: rect.height };
      };
      return {
        innerWidth: window.innerWidth,
        narrow: window.matchMedia('(max-width: 720px)').matches,
        drawerOpen: document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true,
        prompt: box('#prompt-input'),
        button: box('#sidebar-manage-btn'),
      };
    });
    if (mobileNewSessionHeight.innerWidth !== 390 || !mobileNewSessionHeight.narrow || !mobileNewSessionHeight.drawerOpen
      || !mobileNewSessionHeight.prompt || mobileNewSessionHeight.prompt.width <= 0 || mobileNewSessionHeight.prompt.height <= 0
      || !mobileNewSessionHeight.button || mobileNewSessionHeight.button.width <= 0 || mobileNewSessionHeight.button.height < 44
      || Math.abs(mobileNewSessionHeight.button.height - mobileNewSessionHeight.prompt.height) > 1) {
      await mobile.screenshot({ path: `${evidence}/sessions-t7-drawer.png`, fullPage: true });
      fail(`New Session height at 390x844: button ${mobileNewSessionHeight.button?.height}px differs from #prompt-input ${mobileNewSessionHeight.prompt?.height}px or is below the 44px touch bound (${JSON.stringify(mobileNewSessionHeight)})`);
    }
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
    const importedNativeId = await page.evaluate(() => document.getElementById('session-lifecycle')?.dataset.displayedSessionId || '');
    if (!importedNativeId) fail('imported alias: displayed runtime was empty before rewrite');
    const importedForeignId = 'foreign-alias-current';
    await page.evaluate(({ nativeId, foreignId }) => {
      window.__rpiSessionAttachmentTest.armImportedAlias(nativeId, foreignId);
    }, { nativeId: importedNativeId, foreignId: importedForeignId });
    await waitFor(
      page,
      ({ foreignId, nativeId }) => {
        const button = [...document.querySelectorAll('.session-sidebar__switch')]
          .find((candidate) => candidate.dataset.sessionId === foreignId);
        const row = button?.closest('li.session-sidebar__row');
        const displayed = document.getElementById('session-lifecycle')?.dataset.displayedSessionId || '';
        return row?.classList.contains('session-sidebar__row--active') === true && displayed === nativeId;
      },
      'imported alias: foreign row was not the active displayed runtime',
      30000,
      { foreignId: importedForeignId, nativeId: importedNativeId },
    );
    await page.setViewportSize({ width: 375, height: 667 });
    await waitFor(page, () => window.innerWidth === 375 && document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') !== true,
      'imported alias: phone drawer did not settle closed');
    await openSessionsDrawer(page);
    await waitFor(page, () => {
      const sidebar = document.querySelector('.session-sidebar');
      const row = document.querySelector('.session-sidebar__row--active');
      const box = sidebar?.getBoundingClientRect();
      return document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true
        && box?.left === 0 && box.width === 375
        && (row?.getBoundingClientRect().height ?? 0) > 0;
    }, 'imported alias: phone drawer did not open');
    await page.evaluate(() => {
      for (const head of document.querySelectorAll('.session-sidebar__group-head[aria-expanded="false"]')) {
        if (head instanceof HTMLElement) head.click();
      }
    });
    await waitFor(page, () => {
      const list = document.querySelector('.session-sidebar__list');
      const row = document.querySelector('.session-sidebar__row--active');
      return Boolean(list && row && list.scrollHeight > list.clientHeight + row.getBoundingClientRect().height);
    }, 'imported alias: expanded catalog never overflowed the list');
    await page.evaluate(async () => {
      const list = document.querySelector('.session-sidebar__list');
      const row = document.querySelector('.session-sidebar__row--active');
      if (!list || !row) return;
      const before = row.getBoundingClientRect();
      const listBefore = list.getBoundingClientRect();
      const visibleMid = listBefore.top + list.clientHeight / 2;
      const rowMid = before.top + before.height / 2;
      list.scrollTop = rowMid >= visibleMid ? 0 : list.scrollHeight;
      await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    });
    const hiddenBefore = await page.evaluate(() => {
      const list = document.querySelector('.session-sidebar__list');
      const row = document.querySelector('.session-sidebar__row--active');
      if (!list || !row) return false;
      const listBox = list.getBoundingClientRect();
      const rowBox = row.getBoundingClientRect();
      return rowBox.bottom <= listBox.top || rowBox.top >= listBox.bottom;
    });
    if (!hiddenBefore) fail('imported alias: active row was not outside the list before drawer reopen');
    const drawerStillOpen = await page.evaluate(() => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true);
    if (!drawerStillOpen) fail('imported alias: drawer closed before the close click');
    await closeSessionsDrawer(page);
    try {
      await waitFor(page, () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') !== true, 'imported alias: sidebar did not close');
    } catch (error) {
      const state = await page.evaluate(() => ({
        drawerOpen: document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true,
        viewportWidth: window.innerWidth,
      }));
      await page.screenshot({ path: `${evidence}/sessions-imported-alias-close-failure.png`, fullPage: false }).catch(() => {});
      console.log(`web-sessions: imported alias close failure: ${JSON.stringify(state)}`);
      throw error;
    }
    await openSessionsDrawer(page);
    await waitFor(page, () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true, 'imported alias: sidebar did not reopen');
    const revealed = await page.evaluate((foreignId) => {
      const list = document.querySelector('.session-sidebar__list');
      const button = [...document.querySelectorAll('.session-sidebar__switch')]
        .find((candidate) => candidate.dataset.sessionId === foreignId);
      const row = button?.closest('li.session-sidebar__row');
      if (!list || !row) return null;
      const listBox = list.getBoundingClientRect();
      const rowBox = row.getBoundingClientRect();
      return {
        visible: rowBox.top >= listBox.top - 1 && rowBox.bottom <= listBox.bottom + 1,
        row: { top: rowBox.top, bottom: rowBox.bottom, height: rowBox.height },
        list: { top: listBox.top, bottom: listBox.bottom, height: listBox.height, scrollTop: list.scrollTop },
      };
    }, importedForeignId);
    if (revealed?.visible !== true) {
      await page.screenshot({ path: `${evidence}/sessions-imported-alias-reveal-failure.png`, fullPage: false }).catch(() => {});
      console.log(`web-sessions: imported alias reveal failure: ${JSON.stringify(revealed)}`);
      fail('imported alias: reopening the sidebar did not reveal the full active row');
    }
    await page.screenshot({ path: `${evidence}/sessions-imported-alias-revealed.png`, fullPage: false });
    await page.evaluate(() => window.__rpiSessionAttachmentTest.clearImportedAlias());

    /* Unrecorded hard-fail: starred assistant replies. The sessions fixture
     * has no pre-seeded long transcript, and adding a catalog row would move
     * the order this lane already locked. The long history is therefore the
     * live primary, grown by the mock's exact-prompt echo after every
     * catalog assertion. Reload stays in this browser context, so the
     * page's own localStorage is the store under test. */
    await page.setViewportSize({ width: 1280, height: 800 });
    await clickRow(page, sessionA, 'stars: return to the live primary before the long history');
    await waitForIdle(page, 'stars: primary idle before the long history');
    const earlyMarker = 'sessions-reply: star-early-marker';
    const lateMarker = 'sessions-reply: star-late-marker';
    const twinMarker = 'sessions-reply: star-twin-body';
    await promptAndWait(page, 'star-early-marker', earlyMarker, 'stars: early reply never settled');
    await waitForSettledStar(page, earlyMarker, 'stars: early assistant reply never exposed Star');
    let stars = await readReplyStarSnapshot(page);
    const early = starForMarker(stars, earlyMarker);
    if (!early || early.label !== 'Star' || early.pressed !== 'false' || early.entryId === '') {
      fail(`stars: early settled reply has no unpressed Star (${JSON.stringify(early)})`);
    }
    if (stars.userStars !== 0) fail(`stars: a non-assistant surface exposed a star control (${stars.userStars})`);
    const earlyButton = page.locator(`${STAR_SURFACE} .msg--assistant`, { hasText: earlyMarker }).locator('button.assistant-star-toggle');
    await earlyButton.click();
    await waitFor(page, (entryId) => {
      const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
        .find((candidate) => (candidate.getAttribute('data-entry-id') || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === entryId);
      const minimap = [...document.querySelectorAll('.starred-reply-minimap button[data-entry-id]')]
        .some((candidate) => candidate.getAttribute('data-entry-id') === entryId);
      return button?.getAttribute('aria-pressed') === 'true' && button.getAttribute('aria-label') === 'Unstar' && minimap;
    }, 'stars: Star did not settle pressed with a minimap entry', 10000, early.entryId);
    /* Real-box sizing (no CSS probe): on this wide fine-pointer viewport the
     * reply star must hang fully OUTSIDE the text block — right edge at or
     * left of the text block's left edge, vertically intersecting the first
     * text line — inside the row's reserved left band (no clipping, no
     * horizontal overflow). Each rail row is 44px tall with the
     * preview as the jump (stamped with data-entry-id) and a SEPARATE gold
     * unstar (labelled, NEVER stamped — button[data-entry-id] is the row
     * snapshot's jump identity). */
    // Park the pointer: the real click above leaves the star hovered, which
    // would mask its resting chrome (hover paints the bg-hover fill). Settle
    // any running style animation first so the resting read below is a true
    // settled state, never a transient frame of a hidden-state transition.
    await page.mouse.move(0, 0);
    await waitFor(page, (entryId) => {
      const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
        .find((candidate) => (candidate.getAttribute('data-entry-id') || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === entryId);
      if (!button) return false;
      // Force the style/transition flush, then require it to be settled.
      getComputedStyle(button).opacity;
      return button.getAnimations().every((animation) => animation.playState !== 'running');
    }, 'stars: the outside star never settled after the pointer parked', 5000, early.entryId);
    const starGeometry = await page.evaluate((entryId) => {
      const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
        .find((candidate) => (candidate.getAttribute('data-entry-id') || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === entryId);
      const row = button?.closest('.msg--assistant');
      const text = row?.querySelector('.assistant-text');
      if (!button || !text) return null;
      const box = button.getBoundingClientRect();
      const textBox = text.getBoundingClientRect();
      const glyph = button.querySelector('span');
      const actions = button.closest('.msg--assistant__actions');
      const scroll = document.querySelector('main#transcript');
      const rowBox = row.getBoundingClientRect();
      /* Sol: the first-line gate uses the REAL rect of the first rendered
       * codepoint — TreeWalker SHOW_TEXT, first non-empty text node with a
       * non-zero visible rect, then a char Range over the first
       * non-whitespace codepoint (surrogate-safe). NO fallback to block
       * rects or line-height estimates; no visible text = null (asserted
       * failure). */
      const firstChar = (() => {
        const walker = document.createTreeWalker(text, NodeFilter.SHOW_TEXT, {
          acceptNode: (node) => {
            if (!node.nodeValue || node.nodeValue.trim().length === 0) return NodeFilter.FILTER_REJECT;
            const probe = document.createRange();
            probe.selectNodeContents(node);
            for (const rect of probe.getClientRects()) {
              if (rect.width > 0 && rect.height > 0) return NodeFilter.FILTER_ACCEPT;
            }
            return NodeFilter.FILTER_REJECT;
          },
        });
        for (let node = walker.nextNode(); node; node = walker.nextNode()) {
          const value = node.nodeValue;
          for (let i = 0; i < value.length;) {
            const code = value.codePointAt(i);
            const units = code > 0xffff ? 2 : 1;
            const ch = String.fromCodePoint(code);
            i += units;
            if (/\S/.test(ch)) {
              const r = document.createRange();
              r.setStart(node, i - units);
              r.setEnd(node, i);
              const rects = r.getClientRects();
              if (rects.length > 0 && rects[0].width > 0 && rects[0].height > 0) {
                return { left: rects[0].left, right: rects[0].right, top: rects[0].top, height: rects[0].height };
              }
            }
          }
        }
        return null;
      })();
      const label = button.querySelector('.assistant-star-toggle__label');
      const labelBox = label ? label.getBoundingClientRect() : null;
      const hitEl = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
      return {
        outsideGate: window.matchMedia('(min-width: 769px) and (pointer: fine) and (hover: hover)').matches,
        width: box.width,
        height: box.height,
        top: box.top,
        bottom: box.bottom,
        left: box.left,
        right: box.right,
        /* The star lives in the row's reserved left band, inside the row's
         * own paint box: row containment (left >= rowLeft) plus right <=
         * text left prove no clipping by construction; transcriptOverflow
         * double-checks the scroll container itself. */
        rowLeft: rowBox.left,
        transcriptOverflow: scroll ? scroll.scrollWidth - scroll.clientWidth : -1,
        /* Resting chrome: the star is FULLY VISIBLE at rest on both pointer
         * classes (opacity 1, never a hidden/quiet resting state). Hover and
         * keyboard focus give real feedback through the bg-hover fill and the
         * focus ring — never by revealing a hidden button. */
        opacity: parseFloat(getComputedStyle(button).opacity),
        hitTarget: !!hitEl && (hitEl === button || button.contains(hitEl)),
        textBlockLeft: textBox.left,
        firstLineTop: firstChar ? firstChar.top : -1,
        firstLineHeight: firstChar ? firstChar.height : -1,
        firstCharRect: firstChar ? { left: firstChar.left, right: firstChar.right } : null,
        glyphFontSize: glyph ? parseFloat(getComputedStyle(glyph).fontSize) : -1,
        inActions: actions !== null,
        actionsBeforeText: !!actions && (actions.compareDocumentPosition(text) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0,
        starControls: row.querySelectorAll('button.assistant-star-toggle').length,
        ariaLabel: button.getAttribute('aria-label') || '',
        labelHidden: !!label && (getComputedStyle(label).display === 'none' || !labelBox || labelBox.width === 0),
      };
    }, early.entryId);
    if (!starGeometry) {
      fail('stars: the starred reply lost its star control before the sizing probe');
    } else {
      if (!starGeometry.outsideGate) fail(`stars: this desktop viewport is not on the outside-gutter star presentation (${JSON.stringify(starGeometry)})`);
      if (starGeometry.height < 20 || starGeometry.height > 32 || starGeometry.width < 20 || starGeometry.width > 24) {
        fail(`stars: reply star target is ${starGeometry.width}x${starGeometry.height}px (outside-gutter ghost must be a ~24px-tall glyph-only hit, 20px fine minimum; touch expansion is coarse-only)`);
      }
      if (starGeometry.right > starGeometry.textBlockLeft + 1) {
        fail(`stars: reply star is not fully left-outside the text block (right ${starGeometry.right} vs text left ${starGeometry.textBlockLeft})`);
      }
      if (starGeometry.firstLineTop < 0 || !(starGeometry.firstLineHeight > 0)) {
        fail(`stars: reply body produced no visible first-codepoint text rect (TreeWalker found none; ${JSON.stringify(starGeometry.firstCharRect)})`);
      }
      if (!(starGeometry.top < starGeometry.firstLineTop + starGeometry.firstLineHeight && starGeometry.bottom > starGeometry.firstLineTop)) {
        fail(`stars: reply star does not intersect the first text line (${JSON.stringify(starGeometry)})`);
      }
      if (starGeometry.left < starGeometry.rowLeft - 1) {
        fail(`stars: reply star left the row's reserved band (left ${starGeometry.left} vs row left ${starGeometry.rowLeft})`);
      }
      if (starGeometry.transcriptOverflow > 1) {
        fail(`stars: the outside star introduced horizontal overflow (${starGeometry.transcriptOverflow}px)`);
      }
      /* Rects do not prove hittability: paint containment or an overlay can
       * clip the box while its rect still reads full-size. The star must
       * receive hits at its own center — and the real Playwright click above
       * (earlyButton.click()) already exercised the same target. */
      if (!starGeometry.hitTarget) fail(`stars: elementFromPoint at the star center missed the button (${JSON.stringify(starGeometry)})`);
      if (!starGeometry.inActions || !starGeometry.actionsBeforeText) fail('stars: star control must stay in .msg--assistant__actions before the answer body');
      if (starGeometry.glyphFontSize !== 13) {
        fail(`stars: reply star glyph is ${starGeometry.glyphFontSize}px (must be 13px)`);
      }
      if (starGeometry.starControls !== 1) fail(`stars: expected exactly one star control (found ${starGeometry.starControls})`);
      if (starGeometry.ariaLabel !== 'Unstar' || !starGeometry.labelHidden) fail(`stars: outside star must keep its accessible Unstar name with the visible label hidden (${JSON.stringify(starGeometry)})`);
      /* Visible resting state: after the pointer parks, the star must be
       * FULLY VISIBLE — a hidden/quiet rest (anything below opacity 1) must
       * FAIL this lane. Feedback is not gated on an opacity transition:
       * hover/focus below must paint real chrome. */
      if (starGeometry.opacity !== 1) {
        fail(`stars: resting outside star must be fully visible at opacity 1 (got ${starGeometry.opacity}) — hidden resting chrome is not an accepted state`);
      }
      /* Keyboard reachability with REAL keyboard modality, on a scalable
        path: focus starts at the composer (DOM tail — a real click, never
        script focus for the pass evidence), then real Shift+Tab walks
        BACKWARD into the transcript. bg1783 proved a forward walk from the
        top cannot reach a deep star within a bounded press count; walking
        backward from the tail reaches a star in a few presses regardless
        of transcript depth. ANY assistant star landing as activeElement
        with :focus-visible proves the control is keyboard-reachable, and
        the ring is its real focus feedback. */
      await page.click('#prompt-input');
      let keyboardStar = null;
      for (let press = 0; press < 60 && !(keyboardStar && keyboardStar.reached); press += 1) {
        await page.keyboard.press('Shift+Tab');
        keyboardStar = await page.evaluate(() => {
          const active = document.activeElement;
          const star = active instanceof HTMLElement && active.classList.contains('assistant-star-toggle') ? active : null;
          return star
            ? { reached: true, entryId: star.getAttribute('data-entry-id') || '', focusVisible: star.matches(':focus-visible') }
            : { reached: false };
        });
      }
      if (!keyboardStar || !keyboardStar.reached || !keyboardStar.focusVisible || keyboardStar.entryId === '') {
        fail(`stars: real Shift+Tab keyboard never landed :focus-visible on an assistant star (${JSON.stringify(keyboardStar)})`);
      }
      /* Keyboard focus must produce REAL feedback — the ring itself — on a
       * star that was already visible at rest (opacity 1). Revealing a
       * hidden control on focus is NOT an accepted focus feedback. */
      const keyboardFeedback = await page.evaluate((entryId) => {
        const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
          .find((candidate) => (candidate.getAttribute('data-entry-id') || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === entryId);
        if (!button) return null;
        const style = getComputedStyle(button);
        return {
          active: document.activeElement === button,
          focusVisible: button.matches(':focus-visible'),
          opacity: parseFloat(style.opacity),
          outlineStyle: style.outlineStyle,
          outlineWidth: style.outlineWidth,
          outlineColor: style.outlineColor,
          outlineOffset: style.outlineOffset,
        };
      }, keyboardStar.entryId);
      if (!keyboardFeedback || !keyboardFeedback.active || !keyboardFeedback.focusVisible) {
        fail(`stars: the focused star did not stay the :focus-visible activeElement (${JSON.stringify(keyboardFeedback)})`);
      }
      if (keyboardFeedback.opacity !== 1) {
        fail(`stars: keyboard focus must not be the moment a hidden star appears — resting opacity must already be 1 (got ${keyboardFeedback.opacity})`);
      }
      if (keyboardFeedback.outlineStyle !== 'solid'
        || keyboardFeedback.outlineWidth !== '1px'
        || /^(transparent|rgba\(0, 0, 0, 0\))$/.test(keyboardFeedback.outlineColor)
        || keyboardFeedback.outlineOffset !== '-1px') {
        fail(`stars: keyboard :focus-visible must draw its real 1px accent ring (${JSON.stringify(keyboardFeedback)})`);
      }
      /* Feedback independence: real-Tab moves focus OFF the star, which must
       * drop the ring and return to fully-visible resting chrome before the
       * hover gate — hover must never pass on :focus-visible residue. */
      await page.keyboard.press('Tab');
      await waitFor(page, (entryId) => {
        const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
          .find((candidate) => (candidate.getAttribute('data-entry-id') || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === entryId);
        return !!button
          && document.activeElement !== button
          && !button.matches(':focus-visible')
          && getComputedStyle(button).outlineStyle === 'none'
          && parseFloat(getComputedStyle(button).opacity) === 1;
      }, 'stars: the star kept its focus ring after real Tab moved focus off it', 5000, keyboardStar.entryId);
      await waitFor(page, (entryId) => {
        const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
          .find((candidate) => (candidate.getAttribute('data-entry-id') || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === entryId);
        if (!button) return false;
        // Force the style/transition flush, then require it to be settled.
        getComputedStyle(button).opacity;
        return button.getAnimations().every((animation) => animation.playState !== 'running');
      }, 'stars: the star never settled after focus moved off it', 5000, keyboardStar.entryId);
      const restingChrome = await page.evaluate((entryId) => {
        const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
          .find((candidate) => (candidate.getAttribute('data-entry-id') || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === entryId);
        if (!button) return null;
        const style = getComputedStyle(button);
        return {
          opacity: parseFloat(style.opacity),
          backgroundColor: style.backgroundColor,
          color: style.color,
          hovered: button.matches(':hover'),
        };
      }, early.entryId);
      if (!restingChrome || restingChrome.hovered || restingChrome.opacity !== 1) {
        fail(`stars: the parked star is not in its fully-visible resting state (${JSON.stringify(restingChrome)})`);
      }
      /* bg1914 body-hover gate: the resting star is already visible, so a
       * real pointer on the BODY — never the button — must leave it fully
       * visible with no hover fill (body hover can never hide or fake chrome). */
      const bodyPt = await page.evaluate((m) => {
        const row = [...document.querySelectorAll('main#transcript .msg--assistant')].filter((r) => (r.querySelector('.assistant-text')?.textContent || '').includes(m)).pop();
        const btn = row && row.querySelector('button.assistant-star-toggle');
        const text = row && row.querySelector('.assistant-text');
        if (!btn || !text) return { missing: 'body-target' };
        const t = text.getBoundingClientRect(), b = btn.getBoundingClientRect(), x = t.left + 8, y = t.top + 8;
        if ((x > b.left - 1 && x < b.right + 1 && y > b.top - 1 && y < b.bottom + 1) || x < 1 || y < 1 || x > innerWidth - 2 || y > innerHeight - 2) return { missing: 'body-point' };
        return { x, y };
      }, earlyMarker);
      if (bodyPt.missing) fail(`stars: body-hover probe has no usable body point (${JSON.stringify(bodyPt)})`);
      await page.mouse.move(bodyPt.x, bodyPt.y);
      await waitFor(page, ({ m, x, y, entryId, restBackground, restColor }) => {
        const row = [...document.querySelectorAll('main#transcript .msg--assistant')].filter((r) => (r.querySelector('.assistant-text')?.textContent || '').includes(m)).pop();
        const btn = row?.querySelector('button.assistant-star-toggle');
        const hit = document.elementFromPoint(x, y);
        if (!btn || !hit || !hit.closest('.assistant-text') || !row.matches(':hover') || btn.matches(':hover')) return false;
        const style = getComputedStyle(btn);
        return (btn.getAttribute('data-entry-id') || btn.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === entryId
          && parseFloat(style.opacity) === 1
          && style.backgroundColor === restBackground
          && style.color === restColor;
      }, 'stars: BODY hover must leave the resting star fully visible with no button hover fill (bg1914)', 5000, { m: earlyMarker, x: bodyPt.x, y: bodyPt.y, entryId: early.entryId, restBackground: restingChrome.backgroundColor, restColor: restingChrome.color });
      await page.mouse.move(0, 0);
      /* Real pointer hover must paint the bg-hover fill — the hover feedback
       * contract — while the star stays at opacity 1. The pressed (gold)
       * foreground is owned by [aria-pressed="true"], so only the background
       * fill is the hover signal here. */
      await earlyButton.hover();
      await waitFor(page, ({ entryId, restBackground }) => {
        const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
          .find((candidate) => (candidate.getAttribute('data-entry-id') || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === entryId);
        if (!button || !button.matches(':hover')) return false;
        const box = button.getBoundingClientRect();
        const hit = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
        const style = getComputedStyle(button);
        return parseFloat(style.opacity) === 1
          && style.backgroundColor !== restBackground
          && !/^(transparent|rgba\(0, 0, 0, 0\))$/.test(style.backgroundColor)
          && !!hit && (hit === button || button.contains(hit));
      }, 'stars: real pointer hover must paint the bg-hover fill at opacity 1', 5000, { entryId: early.entryId, restBackground: restingChrome.backgroundColor });
      await page.mouse.move(0, 0);
      await waitFor(page, ({ entryId, restBackground, restColor }) => {
        const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
          .find((candidate) => (candidate.getAttribute('data-entry-id') || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === entryId);
        if (!button || button.matches(':hover')) return false;
        const style = getComputedStyle(button);
        return parseFloat(style.opacity) === 1
          && style.backgroundColor === restBackground
          && style.color === restColor;
      }, 'stars: pointer leaving the star must restore its resting chrome', 5000, { entryId: early.entryId, restBackground: restingChrome.backgroundColor, restColor: restingChrome.color });
    }
    const railRowGeometry = await page.evaluate((entryId) => {
      const jump = document.querySelector(`.starred-reply-minimap .starred-reply-minimap__jump[data-entry-id="${entryId}"]`);
      const item = jump?.closest('li');
      const unstar = item?.querySelector('.starred-reply-minimap__unstar');
      if (!jump || !item || !unstar) return null;
      const itemBox = item.getBoundingClientRect();
      const jumpBox = jump.getBoundingClientRect();
      const unstarBox = unstar.getBoundingClientRect();
      const glyph = unstar.querySelector('span');
      return {
        rowHeight: itemBox.height,
        jumpHeight: jumpBox.height,
        jumpEntryId: jump.getAttribute('data-entry-id'),
        jumpTitle: jump.getAttribute('title'),
        jumpLabel: jump.getAttribute('aria-label'),
        unstarWidth: unstarBox.width,
        unstarHeight: unstarBox.height,
        unstarLabel: unstar.getAttribute('aria-label'),
        unstarTitle: unstar.getAttribute('title'),
        unstarText: (unstar.textContent || '').trim(),
        unstarBorderLeftWidth: parseFloat(getComputedStyle(unstar).borderLeftWidth),
        unstarBorderLeftColor: getComputedStyle(unstar).borderLeftColor,
        unstarStamped: unstar.hasAttribute('data-entry-id'),
        unstarGlyphFontSize: glyph ? parseFloat(getComputedStyle(glyph).fontSize) : -1,
        sameRow: jump.parentElement === unstar.parentElement,
      };
    }, early.entryId);
    if (!railRowGeometry) {
      fail('stars: the rail row did not expose separate jump and unstar controls');
    } else {
      if (railRowGeometry.rowHeight < 44 || railRowGeometry.jumpHeight < 44 || railRowGeometry.unstarWidth < 44 || railRowGeometry.unstarHeight < 44) {
        fail(`stars: rail row hits are below 44px (${JSON.stringify(railRowGeometry)})`);
      }
      if (!railRowGeometry.sameRow || railRowGeometry.jumpEntryId !== early.entryId || railRowGeometry.jumpLabel !== `Jump to ${railRowGeometry.jumpTitle}`) {
        fail(`stars: rail jump control lost its row identity (${JSON.stringify(railRowGeometry)})`);
      }
      if (railRowGeometry.unstarStamped || railRowGeometry.unstarLabel !== `Unstar ${railRowGeometry.jumpTitle}` || railRowGeometry.unstarTitle !== 'Unstar reply') {
        fail(`stars: rail unstar control is mislabelled or stamped as a jump entry (${JSON.stringify(railRowGeometry)})`);
      }
      if (railRowGeometry.unstarText !== '×'
        || railRowGeometry.unstarBorderLeftWidth !== 1
        || /^(transparent|rgba\(0, 0, 0, 0\))$/.test(railRowGeometry.unstarBorderLeftColor)) {
        fail(`stars: rail unstar is not the divided delete glyph (${JSON.stringify(railRowGeometry)})`);
      }
      if (railRowGeometry.unstarGlyphFontSize !== 16) {
        fail(`stars: rail unstar glyph is ${railRowGeometry.unstarGlyphFontSize}px (must be 16px)`);
      }
    }
    const authority = new URL(url).host;
    const storedKey = replyStarsStorageKey(authority, sessionA);
    const storedAfterStar = await page.evaluate((key) => window.localStorage.getItem(key), storedKey);
    if (!storedAfterStar || !storedAfterStar.includes(early.entryId)) {
      fail(`stars: scoped store ${storedKey} did not keep ${early.entryId} (${storedAfterStar})`);
    }
    await page.reload({ waitUntil: 'domcontentloaded' });
    await waitFor(page, (sid) => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId === sid, 'stars: reload did not restore the primary', 30000, sessionA);
    await waitFor(page, (entryId) => {
      const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
        .find((candidate) => (candidate.getAttribute('data-entry-id') || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === entryId);
      return button?.getAttribute('aria-pressed') === 'true' && button.getAttribute('aria-label') === 'Unstar';
    }, 'stars: reload did not preserve the starred reply', 30000, early.entryId);
    await earlyButton.click();
    await waitFor(page, (entryId) => {
      const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
        .find((candidate) => (candidate.getAttribute('data-entry-id') || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === entryId);
      const stillListed = [...document.querySelectorAll('.starred-reply-minimap button[data-entry-id]')]
        .some((candidate) => candidate.getAttribute('data-entry-id') === entryId);
      return button?.getAttribute('aria-pressed') === 'false' && button.getAttribute('aria-label') === 'Star' && !stillListed && document.querySelector('.starred-reply-minimap') === null;
    }, 'stars: Unstar did not remove the current minimap', 10000, early.entryId);
    await page.reload({ waitUntil: 'domcontentloaded' });
    await waitFor(page, ({ entryId, marker }) => {
      const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
        .find((candidate) => (candidate.getAttribute('data-entry-id') || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === entryId);
      const row = button?.closest('.msg--assistant');
      const text = row?.querySelector('.assistant-text')?.textContent || '';
      return !!button && text.includes(marker) && button.getAttribute('aria-pressed') === 'false' && button.getAttribute('aria-label') === 'Star' && document.querySelector('.starred-reply-minimap') === null;
    }, 'stars: reload after Unstar did not restore the early reply unstarred without a minimap', 30000, { entryId: early.entryId, marker: earlyMarker });
    await earlyButton.click();
    await waitFor(page, (entryId) => {
      const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
        .find((candidate) => (candidate.getAttribute('data-entry-id') || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === entryId);
      return button?.getAttribute('aria-pressed') === 'true'
        && button.getAttribute('aria-label') === 'Unstar'
        && [...document.querySelectorAll('.starred-reply-minimap button[data-entry-id]')].some((candidate) => candidate.getAttribute('data-entry-id') === entryId);
    }, 'stars: re-star before the long history did not restore the minimap entry', 10000, early.entryId);
    await waitForIdle(page, 'stars: primary idle before the identical pair');
    await promptAndWait(page, 'star-twin-body', twinMarker, 'stars: first identical reply never settled');
    await waitForSettledStar(page, twinMarker, 'stars: first identical reply never exposed Star');
    await promptAndWait(page, 'star-twin-body', twinMarker, 'stars: second identical reply never settled');
    await waitFor(page, (want) => [...document.querySelectorAll('main#transcript [data-runtime-key] .transcript-content .msg--assistant')]
      .filter((row) => (row.querySelector('.assistant-text')?.textContent || '').includes(want)
        && row.querySelector('button.assistant-star-toggle')?.getAttribute('aria-label') === 'Star').length >= 2, 'stars: identical replies did not both settle a Star', 30000, twinMarker);
    stars = await readReplyStarSnapshot(page);
    const twins = stars.stars.filter((star) => star.text.includes(twinMarker));
    if (twins.length < 2 || new Set(twins.map((star) => star.entryId)).size < 2) {
      fail(`stars: identical reply text did not keep distinct entry ids (${JSON.stringify(twins)})`);
    }
    const [firstTwin, secondTwin] = twins;
    await page.locator(`${STAR_SURFACE} button.assistant-star-toggle[data-entry-id="${firstTwin.entryId}"], ${STAR_SURFACE} [data-entry-id="${firstTwin.entryId}"] button.assistant-star-toggle`).first().click();
    await waitFor(page, (ids) => {
      const state = (entryId) => {
        const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
          .find((candidate) => (candidate.getAttribute('data-entry-id') || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === entryId);
        return button?.getAttribute('aria-pressed') || '';
      };
      return state(ids.early) === 'true' && state(ids.first) === 'true' && state(ids.second) === 'false';
    }, 'stars: starring one identical reply starred the other', 10000, { early: early.entryId, first: firstTwin.entryId, second: secondTwin.entryId });
    await waitForIdle(page, 'stars: primary idle before the off-window reply');
    const midPadPrompt = `star-pad-${String(STAR_MID_TURN).padStart(4, '0')}`;
    const midPadMarker = `sessions-reply: ${midPadPrompt}`;
    let midPad = null;
    for (let index = 0; index < LONG_STAR_TURNS; index += 1) {
      const prompt = `star-pad-${String(index).padStart(4, '0')}`;
      await promptAndWait(page, prompt, `sessions-reply: ${prompt}`, `stars: pad ${index} never echoed`);
      if (index === STAR_MID_TURN) {
        // Mid-pad REAL star: by turn 100 the SAME session already mounts
        // >120 projected items; capture THIS pad reply's REAL entry id from
        // the live DOM (never a synthetic id), star it through the real
        // toggle, then keep padding to the tail.
        // Reused settle gate (same as early/twin/late): the reply text echo
        // can beat the entryId stamp — wait for the stamped, unpressed Star.
        await waitForSettledStar(page, prompt, `stars: mid-pad reply never exposed a settleable Star`);
        const midSnapshot = await readReplyStarSnapshot(page);
        if (midSnapshot.mounted <= 120) {
          fail(`stars: mid-pad star expected >120 mounted items at pad ${index} (got ${midSnapshot.mounted})`);
        }
        const mid = starForMarker(midSnapshot, prompt);
        if (!mid || mid.entryId === '' || mid.pressed !== 'false') {
          fail(`stars: mid-pad reply has no settleable Star at pad ${index} (${JSON.stringify(mid)})`);
        }
        await page.locator(`${STAR_SURFACE} .msg--assistant`, { hasText: prompt }).locator('button.assistant-star-toggle').click();
        await waitFor(page, (id) => {
          const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
            .find((candidate) => (candidate.getAttribute('data-entry-id')
              || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === id);
          return button?.getAttribute('aria-pressed') === 'true'
            && [...document.querySelectorAll('.starred-reply-minimap button[data-entry-id]')]
              .some((candidate) => candidate.getAttribute('data-entry-id') === id);
        }, 'stars: mid-pad star did not settle pressed with a minimap entry', 10000, mid.entryId);
        midPad = mid;
      }
    }
    await promptAndWait(page, 'star-late-marker', lateMarker, 'stars: off-window reply never settled');
    await waitForSettledStar(page, lateMarker, 'stars: off-window reply never exposed Star');
    stars = await readReplyStarSnapshot(page);
    const late = starForMarker(stars, lateMarker);
    if (!late || late.entryId === '' || late.entryId === early.entryId) {
      fail(`stars: off-window reply has no distinct entry id (${JSON.stringify(late)})`);
    }
    const transcriptBox = await page.locator('main#transcript').boundingBox();
    if (!transcriptBox) fail('stars: transcript viewport missing before the bottom walk');
    await page.mouse.move(transcriptBox.x + transcriptBox.width / 2, transcriptBox.y + Math.min(80, transcriptBox.height / 2));
    for (let step = 0; step < 40; step += 1) {
      await page.mouse.wheel(0, 4000);
      const parked = await page.evaluate((marker) => {
        const root = document.querySelector('main#transcript [data-runtime-key] .transcript-content');
        return [...(root?.querySelectorAll('.msg--assistant .assistant-text') || [])]
          .some((node) => (node.textContent || '').includes(marker));
      }, earlyMarker);
      if (!parked) break;
    }
    const tailHasEarly = await page.evaluate((marker) => {
      const surface = document.querySelector('main#transcript');
      const root = document.querySelector('main#transcript [data-runtime-key] .transcript-content');
      return {
        present: [...(root?.querySelectorAll('.msg--assistant .assistant-text') || [])]
          .some((node) => (node.textContent || '').includes(marker)),
        mounted: root?.children.length ?? -1,
        follow: surface?.querySelector('[data-runtime-key]')?.getAttribute('data-live-follow') || surface?.getAttribute('data-live-follow') || '',
        windowStart: surface?.querySelector('[data-runtime-key]')?.getAttribute('data-window-start') || '',
        remaining: surface ? surface.scrollHeight - surface.scrollTop - surface.clientHeight : -1,
      };
    }, earlyMarker);
    if (tailHasEarly.present) {
      fail(`stars: the early reply stayed mounted after the user scrolled to the bottom (${JSON.stringify(tailHasEarly)})`);
    }
    const beforeJump = await readReplyStarSnapshot(page);
    if (beforeJump.mounted > PROJECTION_BOUND) {
      fail(`stars: transcript mounted ${beforeJump.mounted} items after the bottom walk (bound ${PROJECTION_BOUND})`);
    }
    const preserved = [...beforeJump.entries].some((entry) => entry.entryId === early.entryId);
    if (!preserved) fail(`stars: the early star did not survive the long history (${JSON.stringify(beforeJump.entries)})`);
    /* Rail unstar is a pure toggle, NEVER a jump. The pinned twin is far
     * outside the mounted window here, so an accidental jump would re-anchor
     * and scroll the transcript. Assert the store + rail lose ONLY that entry
     * while scrollTop, live-follow, the mounted window, and the off-window
     * early reply stay unchanged; then reload to prove the removal persisted
     * (no local fake delete). */
    const twinEntry = twins[0];
    const readStarUnstarState = () => page.evaluate((want) => {
      const surface = document.querySelector('main#transcript');
      const root = document.querySelector('main#transcript [data-runtime-key] .transcript-content');
      const jump = [...document.querySelectorAll('.starred-reply-minimap .starred-reply-minimap__jump')]
        .find((candidate) => candidate.getAttribute('data-entry-id') === want.twin);
      const item = jump?.closest('li');
      const assistantTexts = [...(root?.querySelectorAll('.msg--assistant .assistant-text') || [])];
      return {
        scrollTop: surface ? surface.scrollTop : -1,
        follow: document.querySelector('main#transcript [data-runtime-key]')?.getAttribute('data-live-follow') ?? '',
        mounted: root ? root.children.length : -1,
        earlyMounted: assistantTexts.some((node) => (node.textContent || '').includes(want.earlyText)),
        twinMounted: assistantTexts.filter((node) => (node.textContent || '').includes('sessions-reply: star-twin-body')).length,
        listed: [...document.querySelectorAll('.starred-reply-minimap .starred-reply-minimap__jump[data-entry-id]')]
          .map((candidate) => candidate.getAttribute('data-entry-id')),
        unstarInRow: item ? item.querySelectorAll('.starred-reply-minimap__unstar').length : -1,
      };
    }, { twin: twinEntry.entryId, earlyText: earlyMarker });
    // User-visible contract ONLY: scroll position, live-follow stamp, and the
    // TOP VISIBLE transcript row (assistant preferred, any stamped row
    // otherwise) intersected with the REAL scroll viewport (#transcript rect,
    // NOT the full-height surface/content box), stable data-item-id +
    // viewport-relative top. Content height is NOT asserted.
    const sampleStarViewport = () => page.evaluate(() => {
      const scroll = document.querySelector('main#transcript');
      const surface = scroll?.querySelector('[data-runtime-key]');
      const viewport = scroll?.getBoundingClientRect();
      const pick = (selector) => {
        let top = null;
        for (const row of surface?.querySelectorAll(selector) || []) {
          const id = row.getAttribute('data-item-id') || '';
          const box = row.getBoundingClientRect();
          if (id === '' || !viewport || box.bottom <= viewport.top || box.top >= viewport.bottom) continue;
          const relativeTop = box.top - viewport.top;
          if (top === null || relativeTop < top.top) top = { id, top: relativeTop };
        }
        return top;
      };
      const anchor = pick('.transcript-content .msg--assistant') || pick('.transcript-content [data-item-id]');
      return {
        scrollTop: scroll ? scroll.scrollTop : -1,
        follow: surface?.getAttribute('data-live-follow') ?? '',
        anchorId: anchor ? anchor.id : 'none',
        anchorTop: anchor ? anchor.top : -1,
        remaining: scroll ? scroll.scrollHeight - scroll.scrollTop - scroll.clientHeight : -1,
      };
    });
    const nextFrame = () => page.evaluate(() => new Promise((resolve) => requestAnimationFrame(resolve)));
    // User contract: the SAME top-visible row must stay put (±1px) and the
    // live-follow stamp must not flip. scrollTop is intentionally NOT compared:
    // at the pinned bottom an offscreen content-height re-measure clamps it.
    const assertStarViewportHeld = (label, before, after) => {
      if (after.follow !== before.follow
        || after.anchorId !== before.anchorId || Math.abs(after.anchorTop - before.anchorTop) > 1) {
        fail(`stars: ${label} Unstar moved the visible transcript (${JSON.stringify({ before, after })})`);
      }
    };
    const beforeMinimapUnstar = await readStarUnstarState();
    if (beforeMinimapUnstar.unstarInRow !== 1
      || beforeMinimapUnstar.listed.length !== 3
      || !beforeMinimapUnstar.listed.includes(twinEntry.entryId)
      || !beforeMinimapUnstar.listed.includes(early.entryId)
      || !beforeMinimapUnstar.listed.includes(midPad.entryId)) {
      fail(`stars: the pinned twin's rail row did not expose exactly one unstar among {early,mid-pad,twin} (${JSON.stringify(beforeMinimapUnstar)})`);
    }
    // One real journey: the inherited view is whatever it is (parked or
    // pinned); the REAL rail unstar must not move the same top-visible row or
    // flip the live-follow stamp. The next frame is the immediate proof;
    // scrollTop/mounted are projection artifacts, not the user contract.
    const viewportBefore = await sampleStarViewport();
    if (viewportBefore.anchorId === 'none') {
      fail(`stars: no visible transcript row before the rail Unstar (${JSON.stringify(viewportBefore)})`);
    }
    const preClickState = await readStarUnstarState();
    // Baseline at the REAL twin rail Unstar pointerdown (capture), before React onClick.
    await page.evaluate((twinId) => {
      const onDown = (event) => { const button = event.target?.closest?.('.starred-reply-minimap__unstar');
        if (!button || button.closest('.starred-reply-minimap__item')?.querySelector('.starred-reply-minimap__jump[data-entry-id]')?.getAttribute('data-entry-id') !== twinId) return;
        document.removeEventListener('pointerdown', onDown, true);
        const s = document.querySelector('main#transcript'), vp = s?.getBoundingClientRect();
        let pick = null; for (const row of s?.querySelectorAll('.transcript-content .msg--assistant') || []) {
          const box = row.getBoundingClientRect(), id = row.getAttribute('data-item-id') || '';
          if (id === '' || !vp || box.bottom <= vp.top || box.top >= vp.bottom) continue;
          const rel = box.top - vp.top;
          if (pick === null || rel < pick.top) pick = { id, top: rel };
        }
        window.__railBaseline = { t: performance.now(), follow: s?.querySelector('[data-runtime-key]')?.getAttribute('data-live-follow') ?? '', anchorId: pick ? pick.id : 'none', anchorTop: pick ? pick.top : -1 };
      };
      document.addEventListener('pointerdown', onDown, true); window.__railBaselineCleanup = () => { document.removeEventListener('pointerdown', onDown, true); delete window.__railBaselineCleanup; };
    }, twinEntry.entryId);
    await page.locator('.starred-reply-minimap__item', { has: page.locator(`.starred-reply-minimap__jump[data-entry-id="${twinEntry.entryId}"]`) })
      .locator('.starred-reply-minimap__unstar').click();
    await nextFrame();
    const railBaseline = await page.evaluate(() => { window.__railBaselineCleanup?.(); return window.__railBaseline; });
    if (!railBaseline || railBaseline.anchorId === 'none') fail(`stars: rail Unstar pointerdown baseline missing (${JSON.stringify({ railBaseline, installSample: viewportBefore })})`);
    const viewportAfter = await sampleStarViewport();
    assertStarViewportHeld('rail', { ...railBaseline, installSample: viewportBefore }, viewportAfter);
    await waitFor(page, (want) => {
      let ids = [];
      const stored = window.localStorage.getItem(want.key);
      try { ids = stored ? JSON.parse(stored) : []; } catch { return false; }
      const listed = [...document.querySelectorAll('.starred-reply-minimap .starred-reply-minimap__jump[data-entry-id]')]
        .map((candidate) => candidate.getAttribute('data-entry-id'));
      // Only `early`, the mid-pad reply, and the pinned twin were ever
      // starred: the late reply only proved a Star control exists
      // (waitForSettledStar), it is never clicked. Un-starring the twin must
      // therefore leave exactly {early, mid-pad} in the store AND the rail.
      // The rail selector targets the JUMP button only; the unstar button
      // intentionally has no data-entry-id, so it can never be counted as an
      // entry snapshot here.
      return !ids.includes(want.twin) && !listed.includes(want.twin)
        && ids.length === 2 && ids.includes(want.early) && ids.includes(want.mid)
        && listed.length === 2 && listed.includes(want.early) && listed.includes(want.mid);
    }, 'stars: rail Unstar did not remove just that entry from the store and the rail', 10000, {
      key: storedKey, twin: twinEntry.entryId, early: early.entryId, mid: midPad.entryId,
    }, (want) => ({
      activeSessionId: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
      scopedStore: window.localStorage.getItem(want.key),
      replyStarKeys: Object.keys(window.localStorage)
        .filter((key) => key.startsWith('rpi-web-reply-stars:'))
        .map((key) => ({ key, value: window.localStorage.getItem(key) })),
      railPresent: document.querySelector('.starred-reply-minimap') !== null,
      railItems: document.querySelectorAll('.starred-reply-minimap__item').length,
      railJumps: [...document.querySelectorAll('.starred-reply-minimap__jump[data-entry-id]')]
        .map((button) => button.getAttribute('data-entry-id')),
      railUnstars: document.querySelectorAll('.starred-reply-minimap__unstar').length,
      pressed: [want.twin, want.early].map((entryId) => {
        const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
          .find((candidate) => (candidate.getAttribute('data-entry-id') || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === entryId);
        return { entryId, pressed: button?.getAttribute('aria-pressed') ?? 'not-mounted' };
      }),
    }));
    const afterMinimapUnstar = await readStarUnstarState();
    // Lazy-render contract: the off-window early reply must stay unmounted
    // (the twin count must not change either); the visible position/follow is
    // asserted by the instantaneous anchor check above, and store/rail by the
    // waitFor. scrollTop/mounted are projection artifacts, not the user
    // contract, so they are not compared.
    if (afterMinimapUnstar.earlyMounted || afterMinimapUnstar.twinMounted !== preClickState.twinMounted) {
      fail(`stars: rail Unstar re-mounted an off-window reply instead of staying put (${JSON.stringify({ before: preClickState, after: afterMinimapUnstar })})`);
    }
    await page.reload({ waitUntil: 'domcontentloaded' });
    await waitFor(page, (sid) => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId === sid, 'stars: reload after rail Unstar did not restore the primary', 30000, sessionA);
    await waitFor(page, (want) => {
      let ids = [];
      try { ids = JSON.parse(window.localStorage.getItem(want.key) || '[]'); } catch { return false; }
      const listed = [...document.querySelectorAll('.starred-reply-minimap .starred-reply-minimap__jump[data-entry-id]')]
        .map((candidate) => candidate.getAttribute('data-entry-id'));
      return ids.length === 2 && ids.includes(want.early) && ids.includes(want.mid)
        && listed.length === 2 && listed.includes(want.early) && listed.includes(want.mid);
    }, 'stars: reload after rail Unstar did not restore exactly the remaining pinned replies', 30000, {
      key: storedKey, early: early.entryId, mid: midPad.entryId,
    });
    const jumpDraft = 'star-jump-unsent-draft-must-survive';
    const jumpAttachment = 'star-jump-unsent.txt';
    await page.fill('#prompt-input', jumpDraft);
    await page.setInputFiles('input[type=file]', {
      name: jumpAttachment,
      mimeType: 'text/plain',
      buffer: Buffer.from('star jump attachment must survive'),
    });
    await waitFor(page, (expected) => document.getElementById('prompt-input')?.value === expected.draft
      && document.querySelectorAll('.composer-attachment').length === 1
      && (document.querySelector('.composer-attachment__name')?.textContent || '') === expected.name,
    'stars: unsent draft and attachment were not ready before the jump', 10000, { draft: jumpDraft, name: jumpAttachment });
    const attachmentBeforeJump = await page.locator('.composer-attachment').elementHandle();
    if (!attachmentBeforeJump) fail('stars: unsent attachment node was not held before the jump');
    const composerBeforeJump = await page.evaluate(() => ({
      draft: document.getElementById('prompt-input')?.value ?? '',
      names: [...document.querySelectorAll('.composer-attachment__name')].map((el) => el.textContent || ''),
      count: document.querySelectorAll('.composer-attachment').length,
    }));
    await sampledMinimapJumpClick(page, early.entryId, 'stars: post-reload minimap jump');
    await waitFor(page, (marker) => {
      const viewport = document.querySelector('main#transcript')?.getBoundingClientRect();
      const root = document.querySelector('main#transcript [data-runtime-key] .transcript-content');
      const row = [...(root?.querySelectorAll('.msg--assistant') || [])]
        .find((candidate) => (candidate.querySelector('.assistant-text')?.textContent || '').includes(marker));
      if (!row || !root || !viewport) return false;
      const rowBox = row.getBoundingClientRect();
      const visibleTop = Math.max(rowBox.top, viewport.top);
      const visibleBottom = Math.min(rowBox.bottom, viewport.bottom);
      const follow = document.querySelector('main#transcript [data-runtime-key]')?.getAttribute('data-live-follow');
      return visibleBottom - visibleTop >= 24 && follow === 'false' && root.children.length <= 240 && root.children.length > 0;
    }, 'stars: minimap jump did not show the early reply inside the transcript viewport', 10000, earlyMarker);
    const composerAfterJump = await page.evaluate(() => ({
      draft: document.getElementById('prompt-input')?.value ?? '',
      names: [...document.querySelectorAll('.composer-attachment__name')].map((el) => el.textContent || ''),
      count: document.querySelectorAll('.composer-attachment').length,
    }));
    const sameAttachment = await attachmentBeforeJump.evaluate((node) => node.isSameNode(document.querySelector('.composer-attachment')));
    if (composerAfterJump.draft !== composerBeforeJump.draft
      || composerAfterJump.count !== 1
      || composerAfterJump.names[0] !== jumpAttachment
      || !sameAttachment) {
      fail(`stars: minimap jump changed the unsent composer (${JSON.stringify({ composerBeforeJump, composerAfterJump, sameAttachment })})`);
    }
    const jumped = await readReplyStarSnapshot(page);
    const visibleEarly = starForMarker(jumped, earlyMarker);
    if (!visibleEarly || visibleEarly.entryId !== early.entryId) {
      fail(`stars: jump mounted a different reply (${JSON.stringify({ visibleEarly, expected: early.entryId, mounted: jumped.mounted, follow: jumped.follow })})`);
    }
    if (jumped.mounted > PROJECTION_BOUND || jumped.follow !== 'false') {
      fail(`stars: jump broke the projection (${JSON.stringify({ mounted: jumped.mounted, follow: jumped.follow })})`);
    }
    // Mid-history REAL jump: the SAME rail, the mid-pad entry (starred at
    // pad 100 with >120 items already mounted); the early jump above stays
    // the boundary contract.
    await sampledMinimapJumpClick(page, midPad.entryId, 'stars: post-reload midPad jump');
    await waitFor(page, (marker) => {
      const viewport = document.querySelector('main#transcript')?.getBoundingClientRect();
      const root = document.querySelector('main#transcript [data-runtime-key] .transcript-content');
      const row = [...(root?.querySelectorAll('.msg--assistant') || [])]
        .find((candidate) => (candidate.querySelector('.assistant-text')?.textContent || '').includes(marker));
      if (!row || !root || !viewport) return false;
      const rowBox = row.getBoundingClientRect();
      const visibleTop = Math.max(rowBox.top, viewport.top);
      const visibleBottom = Math.min(rowBox.bottom, viewport.bottom);
      const follow = document.querySelector('main#transcript [data-runtime-key]')?.getAttribute('data-live-follow');
      return visibleBottom - visibleTop >= 24 && follow === 'false' && root.children.length <= 240 && root.children.length > 0;
    }, 'stars: post-reload midPad jump did not show the mid-pad reply inside the transcript viewport', 10000, midPadMarker);
    const jumpedMid = await readReplyStarSnapshot(page);
    const visibleMid = starForMarker(jumpedMid, midPadPrompt);
    if (!visibleMid || visibleMid.entryId !== midPad.entryId) {
      fail(`stars: midPad jump mounted a different reply (${JSON.stringify({ visibleMid, expected: midPad.entryId, mounted: jumpedMid.mounted, follow: jumpedMid.follow })})`);
    }
    const baselineJump = await starJumpGeometry(page, early.entryId);
    const baselineJumpMid = await starJumpGeometry(page, midPad.entryId);
    await page.screenshot({ path: `${evidence}/sessions-star-desktop-jump.png`, fullPage: false });
    const desktopFit = await page.evaluate(() => {
      const boxes = (selector) => [...document.querySelectorAll(selector)].map((el) => {
        const r = el.getBoundingClientRect();
        return { top: r.top, right: r.right, bottom: r.bottom, left: r.left, width: r.width, height: r.height };
      });
      const one = (selector) => boxes(selector).find((box) => box.width > 0 && box.height > 0) || null;
      return {
        header: one('header'),
        footer: one('.app-main > footer'),
        transcript: one('main#transcript'),
        minimap: one('.starred-reply-minimap'),
        scrollWidth: document.documentElement.scrollWidth,
        innerWidth: window.innerWidth,
        innerHeight: window.innerHeight,
      };
    });
    if (!desktopFit.minimap || !desktopFit.transcript || !desktopFit.header || !desktopFit.footer) {
      fail(`stars: desktop chrome missing (${JSON.stringify(desktopFit)})`);
    }
    if (desktopFit.minimap.left < desktopFit.transcript.right - 1 || desktopFit.minimap.top < desktopFit.header.bottom - 1 || desktopFit.minimap.bottom > desktopFit.footer.top + 1) {
      fail(`stars: desktop minimap overlaps transcript, header, or footer (${JSON.stringify(desktopFit)})`);
    }
    if (desktopFit.scrollWidth > desktopFit.innerWidth + 1) fail(`stars: desktop horizontal overflow ${desktopFit.scrollWidth} > ${desktopFit.innerWidth}`);
    /* Unrecorded hard-fail: minimap append regression. The static jump
     * above proved ONE click after the 260 pad turns; the reported bug is
     * that the SAME minimap entry, clicked again after MORE turns appended
     * to the SAME session, lands at the wrong offset. Baseline first (the
     * jump above just landed), then real appended turns through the same
     * pad construction — never a new session — then the same entryId. */
    // Baselines (early + midPad) were captured right after each jump landed above.
    // The jump contract above left its unsent attachment chip behind; that
    // contract is complete — clear it so the appended turns are pure typed
    // prompts.
    await page.locator('.composer-attachment__remove').click();
    await waitFor(page, () => document.querySelectorAll('.composer-attachment').length === 0,
      'stars: leftover jump attachment chip never cleared before the append turns', 10000);
    // Resume live follow with the lane's wheel gesture — parked windows
    // legitimately never mount the offscreen tail the appended turns need.
    await page.mouse.move(640, 400);
    for (let step = 0; step < 40; step += 1) await page.mouse.wheel(0, 4000);
    await waitFor(page, () => document.querySelector('main#transcript [data-runtime-key]')?.getAttribute('data-live-follow') === 'true',
      'stars: wheel-to-bottom never resumed live follow before the append turns', 10000);
    // Bounded scroll/input telemetry (diagnosis only): input type/time (never
    // key content) + scroll metrics in event order. isTrusted is recorded as
    //-is and is NOT treated as user-vs-programmatic proof.
    await page.evaluate(() => {
      window.__starScrollLog = []; const last = { type: 'none', at: 0 };
      for (const [t, ev] of [['wheel', 'wheel'], ['pointer', 'pointerdown'], ['key', 'keydown']]) document.addEventListener(ev, (e) => { last.type = t; last.at = e.timeStamp; }, true);
      document.querySelector('main#transcript')?.addEventListener('scroll', (e) => {
        if (window.__starScrollLog.length < 120) { const s = e.currentTarget, r = s.querySelector('[data-runtime-key]'); window.__starScrollLog.push({ input: last.type, inputAt: Math.round(last.at), at: Math.round(e.timeStamp), trusted: e.isTrusted, scrollTop: Math.round(s.scrollTop), sh: s.scrollHeight, ch: s.clientHeight, follow: r?.getAttribute('data-live-follow') || '', anchor: r?.getAttribute('data-live-anchor') || '' }); }
      }, true);
    });
    for (let index = 0; index < APPEND_STAR_TURNS; index += 1) {
      const prompt = `star-append-${String(index).padStart(4, '0')}`;
      await page.evaluate((i) => { window.__starScrollLog.push({ turn: i, phase: 'pre', follow: document.querySelector('main#transcript [data-runtime-key]')?.getAttribute('data-live-follow') || '', anchor: document.querySelector('main#transcript [data-runtime-key]')?.getAttribute('data-live-anchor') || '', scrollTop: Math.round(document.querySelector('main#transcript')?.scrollTop ?? -1), sh: document.querySelector('main#transcript')?.scrollHeight ?? -1, ch: document.querySelector('main#transcript')?.clientHeight ?? -1 }); }, index);
      await promptAndWait(page, prompt, `sessions-reply: ${prompt}`, `stars: append ${index} never echoed`);
      await page.evaluate((i) => { window.__starScrollLog.push({ turn: i, phase: 'post', follow: document.querySelector('main#transcript [data-runtime-key]')?.getAttribute('data-live-follow') || '', anchor: document.querySelector('main#transcript [data-runtime-key]')?.getAttribute('data-live-anchor') || '', scrollTop: Math.round(document.querySelector('main#transcript')?.scrollTop ?? -1), sh: document.querySelector('main#transcript')?.scrollHeight ?? -1, ch: document.querySelector('main#transcript')?.clientHeight ?? -1 }); }, index);
    }
    await waitFor(page, (entryId) => {
      const rail = document.querySelector('.starred-reply-minimap');
      return !!rail && [...rail.querySelectorAll('button[data-entry-id]')]
        .some((candidate) => candidate.getAttribute('data-entry-id') === entryId);
    }, 'stars: the appended session lost the minimap entry', 10000, early.entryId);
    // The SAME-entry jump target: click it directly when visible. Desktop
    // keeps the collapsed rail's list visible (styles.css:9038 hides the
    // expand control OUTSIDE the mobile breakpoint), so no expand is needed;
    // when the target is not visible, use the real expand control's actual
    // accessible label; a rail without either is a failure with typed
    // attributes only (never content).
    const jumpTarget = page.locator(`.starred-reply-minimap button[data-entry-id="${early.entryId}"]`);
    if (!(await jumpTarget.isVisible())) {
      const expand = await page.evaluate(() => { const b = document.querySelector('.starred-reply-minimap__expand'); return b ? { label: b.getAttribute('aria-label') || '', expanded: b.getAttribute('aria-expanded') || '', display: getComputedStyle(b).display } : null; });
      if (!expand || expand.display === 'none' || expand.expanded !== 'false') fail(`stars: the same-entry jump is not visible and the rail exposes no usable expand (${JSON.stringify(expand)})`);
      await page.getByRole('button', { name: expand.label, exact: true }).click();
      if (!(await jumpTarget.isVisible())) fail(`stars: the expand click did not reveal the same-entry jump (${JSON.stringify(expand)})`);
    }
    await sampledMinimapJumpClick(page, early.entryId, 'stars: after-append minimap jump');
    try {
      await page.waitForFunction((want) => {
        const scroll = document.querySelector('main#transcript');
        const root = document.querySelector('main#transcript [data-runtime-key] .transcript-content');
        const row = [...(root?.querySelectorAll('.msg--assistant') || [])]
          .find((candidate) => (candidate.querySelector('.assistant-text')?.textContent || '').includes(want.marker));
        if (!row || !root || !scroll) return false;
        const entryId = row.querySelector('button.assistant-star-toggle')?.getAttribute('data-entry-id')
          || row.getAttribute('data-entry-id') || '';
        if (entryId !== want.entryId) return false;
        const rowBox = row.getBoundingClientRect();
        const box = scroll.getBoundingClientRect();
        // 24 = the static jump contract's minimum visible overlap (defined
        // above), reused — not a new tolerance.
        return Math.min(rowBox.bottom, box.bottom) - Math.max(rowBox.top, box.top) >= 24
          && document.querySelector('main#transcript [data-runtime-key]')?.getAttribute('data-live-follow') === 'false';
      }, { entryId: early.entryId, marker: earlyMarker }, { timeout: 10000 });
    } catch {
      await dumpStarJumpRegression('after-append-remount-timeout', page, await starJumpGeometry(page, early.entryId).catch(() => null));
      fail('stars: append-turn minimap jump did not remount the same starred reply');
    }
    const appendedJump = await starJumpGeometry(page, early.entryId);
    // Mid-history jump after the SAME appends: the mid-pad entry, same
    // remount contract as the early boundary above.
    await sampledMinimapJumpClick(page, midPad.entryId, 'stars: after-append midPad jump');
    try {
      await page.waitForFunction((want) => {
        const scroll = document.querySelector('main#transcript');
        const root = document.querySelector('main#transcript [data-runtime-key] .transcript-content');
        const row = [...(root?.querySelectorAll('.msg--assistant') || [])]
          .find((candidate) => (candidate.querySelector('.assistant-text')?.textContent || '').includes(want.marker));
        if (!row || !root || !scroll) return false;
        const entryId = row.querySelector('button.assistant-star-toggle')?.getAttribute('data-entry-id')
          || row.getAttribute('data-entry-id') || '';
        if (entryId !== want.entryId) return false;
        const rowBox = row.getBoundingClientRect();
        const box = scroll.getBoundingClientRect();
        return Math.min(rowBox.bottom, box.bottom) - Math.max(rowBox.top, box.top) >= 24
          && document.querySelector('main#transcript [data-runtime-key]')?.getAttribute('data-live-follow') === 'false';
      }, { entryId: midPad.entryId, marker: midPadMarker }, { timeout: 10000 });
    } catch {
      await dumpStarJumpRegression('after-append-midpad-remount-timeout', page, await starJumpGeometry(page, midPad.entryId).catch(() => null));
      fail('stars: append-turn midPad minimap jump did not remount the mid-pad starred reply');
    }
    const appendedJumpMid = await starJumpGeometry(page, midPad.entryId);
    // Root-cause evidence ONLY, never an assertion: the restore anchors on
    // the internal heights model, so the absolute offset is incidental.
    fs.writeFileSync(
      path.join(evidence, 'sessions-star-append-jump-geometry.json'),
      JSON.stringify({ baseline: baselineJump, baselineMidPad: baselineJumpMid, afterAppend: appendedJump, afterAppendMidPad: appendedJumpMid }, null, 2),
    );
    // Three CONSECUTIVE sampled jumps on desktop: the same helper, the same
    // real rail button, back to back — each click must land the target at
    // top<=1px and hold it for 120 frames with no drift or unmount.
    for (let click = 1; click <= 3; click += 1) {
      await sampledMinimapJumpClick(page, midPad.entryId, `stars: desktop midPad consecutive jump ${click}/3`);
    }
    // User-wheel takeover (existing fixture path, minimal): a jumped/parked
    // view (data-live-follow=false) must hand control back to the user's
    // real wheel gesture — live follow resumes.
    await page.mouse.move(640, 400);
    for (let step = 0; step < 40; step += 1) await page.mouse.wheel(0, 4000);
    await waitFor(page, () => document.querySelector('main#transcript [data-runtime-key]')?.getAttribute('data-live-follow') === 'true',
      'stars: user wheel never took over the parked view after the sampled jumps', 10000);
    await page.setViewportSize({ width: 390, height: 844 });
    await waitFor(page, () => {
      const rail = document.querySelector('.starred-reply-minimap');
      const box = rail?.getBoundingClientRect();
      return !!rail && rail.classList.contains('is-collapsed') && !!box && box.width <= 44.5 && box.width >= 44;
    }, 'stars: 390x844 minimap did not settle as the 44px reserved column', 10000);
    const expand = page.getByRole('button', { name: 'Show starred replies', exact: true });
    const expandBox = await expand.boundingBox();
    if (!expandBox || expandBox.width < 44 || expandBox.height < 44) {
      fail(`stars: mobile minimap expand target is below 44px (${JSON.stringify(expandBox)})`);
    }
    await expand.click();
    await waitFor(page, () => {
      const rail = document.querySelector('.starred-reply-minimap');
      const box = rail?.getBoundingClientRect();
      return !!rail && !rail.classList.contains('is-collapsed') && !!box && box.width > 44;
    }, 'stars: mobile minimap did not expand', 10000);
    const mobileFit = await page.evaluate(() => {
      const rect = (selector) => {
        const matches = [...document.querySelectorAll(selector)].map((el) => {
          const r = el.getBoundingClientRect();
          return { top: r.top, right: r.right, bottom: r.bottom, left: r.left, width: r.width, height: r.height };
        }).filter((box) => box.width > 0 && box.height > 0);
        return matches[0] || null;
      };
      return {
        header: rect('header'),
        footer: rect('.app-main > footer'),
        transcript: rect('main#transcript'),
        minimap: rect('.starred-reply-minimap'),
        scrollWidth: document.documentElement.scrollWidth,
        innerWidth: window.innerWidth,
        innerHeight: window.innerHeight,
      };
    });
    if (!mobileFit.minimap || !mobileFit.transcript || !mobileFit.header || !mobileFit.footer) fail(`stars: mobile chrome missing (${JSON.stringify(mobileFit)})`);
    if (mobileFit.minimap.left < mobileFit.transcript.right - 1 || mobileFit.minimap.top < mobileFit.header.bottom - 1 || mobileFit.minimap.bottom > mobileFit.footer.top + 1) {
      fail(`stars: expanded mobile minimap overlaps transcript, header, or footer (${JSON.stringify(mobileFit)})`);
    }
    if (mobileFit.scrollWidth > mobileFit.innerWidth + 1) fail(`stars: mobile horizontal overflow ${mobileFit.scrollWidth} > ${mobileFit.innerWidth}`);
    await page.screenshot({ path: `${evidence}/sessions-star-mobile-expanded.png`, fullPage: false });
    // Three CONSECUTIVE sampled jumps on mobile: the same helper; each jump
    // folds the minimap back to the 44px column, the next click re-expands
    // through the real fixture control.
    for (let click = 1; click <= 3; click += 1) {
      await sampledMinimapJumpClick(page, midPad.entryId, `stars: mobile midPad consecutive jump ${click}/3`, { mayExpand: true });
    }
    await waitFor(page, () => {
      const rail = document.querySelector('.starred-reply-minimap');
      const box = rail?.getBoundingClientRect();
      return !!rail && rail.classList.contains('is-collapsed') && !!box && box.width <= 44.5;
    }, 'stars: mobile jump did not collapse the minimap', 10000);
    // Fold no-rollback (REAL mobile fold fixture): the fold back to the 44px
    // column must not move the landed jump target — the same entry-id row
    // stays at top<=1px in the transcript viewport after the collapse.
    const afterFold = await starJumpGeometry(page, midPad.entryId);
    if (!afterFold.found || afterFold.rowTopInViewport === null || afterFold.rowTopInViewport > 1) {
      fail(`stars: the mobile fold rolled back the landed jump target (${JSON.stringify(afterFold)})`);
    }
    // Real fold toggle (existing fixture control, no invented elements):
    // re-expand through the rail's real expand toggle, hold it OPEN across
    // 30 animation frames, then the user's real wheel takes over the parked
    // view (live follow resumes).
    await page.getByRole('button', { name: 'Show starred replies', exact: true }).click();
    await waitFor(page, () => {
      const rail = document.querySelector('.starred-reply-minimap');
      const box = rail?.getBoundingClientRect();
      return !!rail && !rail.classList.contains('is-collapsed') && !!box && box.width > 44;
    }, 'stars: mobile fold did not re-expand through the real toggle', 10000);
    await page.evaluate(() => new Promise((resolve) => {
      let frames = 0;
      window.__foldHold = null;
      const step = () => {
        frames += 1;
        const rail = document.querySelector('.starred-reply-minimap');
        const open = !!rail && !rail.classList.contains('is-collapsed') && rail.getBoundingClientRect().width > 44;
        if (!open || frames >= 30) { window.__foldHold = { frames, open }; resolve(); return; }
        requestAnimationFrame(step);
      };
      requestAnimationFrame(step);
    }));
    const foldHold = await page.evaluate(() => window.__foldHold);
    if (!foldHold || !foldHold.open || foldHold.frames < 30) {
      fail(`stars: the re-expanded mobile fold did not stay open across 30 frames (${JSON.stringify(foldHold)})`);
    }
    await page.mouse.move(195, 400);
    for (let step = 0; step < 40; step += 1) await page.mouse.wheel(0, 4000);
    await waitFor(page, () => document.querySelector('main#transcript [data-runtime-key]')?.getAttribute('data-live-follow') === 'true',
      'stars: user wheel never took over the parked view after the mobile fold', 10000);
    await page.setViewportSize({ width: 1280, height: 800 });
    if (!(await page.evaluate(() => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true))) {
      await page.click('#rail-reopen-btn');
    }
    await waitFor(page, () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true && Math.abs((document.querySelector('.session-sidebar')?.getBoundingClientRect().width ?? 0) - 195.5) <= 1, 'stars: desktop rail did not reopen before the session switch', 10000);
    await clickRow(page, sessionB, 'stars: switch away to prove session isolation');
    await waitFor(page, (sid) => {
      const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId;
      const foreign = [...document.querySelectorAll('.starred-reply-minimap button[data-entry-id]')];
      return active === sid && foreign.length === 0;
    }, 'stars: the other session rendered the primary minimap entries', 30000, sessionB);
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

    console.log(`web-sessions: PASSED (${executed.size}/${DOCUMENTED_IDS.length} assertions, T0 boot, T1 concurrent streaming + unread + restore, T3 lifecycle attachment isolation, T2 abort/toast isolation, T6 rail collapse/reopen + header, T8 materialized fork ancestry + composer-only cut, T4 16-runtime cap + idle LRU replacement, T9 selected-path focus default + refresh persistence + A→B→A non-resurrection, T5 Todo/Goal/Workflow completed DAG + fail-closed isolation, T7 390x844 drawer failure-retain + success-close) — evidence at ${coveragePath}`);
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-sessions: playwright crashed: ${err && err.stack ? err.stack : err}`);
  process.exit(2);
});
