// Web multi-session E2E lane — PLAYWRIGHT-ONLY (E2E.d/web/sessions.sh).
//
// Environment:
//   RPI_URL         http://127.0.0.1:<port>/web
//   RPI_TOKEN       token file content (served via rpi-auth.<token> subprotocol)
//   RPI_MOCK_CONTROL_URL loopback mock origin for the T3 release barrier
//   RPI_CHROME      executable path of the system Chrome (optional)
//   RPI_EVIDENCE    evidence dir for screenshots + executed-assertion evidence
//
// REQUIRES the MultiSessionRuntimeManager backend: top-level `sessionId`
// command routing, lifecycle responses `{sessionId,state,messages}`, every
// event tagged with the owning `sessionId`, close_session idle/busy
// semantics, MAX_LOADED_SESSIONS=8 with no eviction, and the `loaded` overlay
// on session_list rows. The lane FAILS (exit 2) whenever any assertion fails;
// there is no agent-browser fallback and no skip.
//
// Mock scenario: `sessions` (E2E.d/lib/user_mock_server.py) — replies are
// routed by exact prompt text. `sessions-slow-b3` sends its first delta, then
// waits for the test-owned release endpoint; anything else follows the normal
// slow/echo fixture behavior.
//
// Assertion matrix (feature -> IDs, used by the coverage report):
//   T0  boot + token connect + primary session row
//   T1  T1.1 slow A stream starts
//       T1.2 New while A streams -> B active with empty new-session view
//       T1.3 B prompt round-trips while A keeps running
//       T1.4 A background events bump A's unread badge (B stays 0)
//       T1.5 switch back to A: full stream text, unread cleared
//       T1.6 A authoritative transcript restore (history preserved)
//   T2  T2.1 second slow stream on A
//       T2.2 New -> C; C slow stream starts
//       T2.3 abort C -> neutral "run aborted" toast
//       T2.4 aborted C text never contains the final tail chunks
//       T2.5 A unaffected: completes fully after C's abort
//   T6  T6.1 header has NO feature buttons (they live in the sidebar nav)
//       T6.2 desktop collapse -> compact rail with reopen control
//       T6.3 rail reopen restores the sidebar
//       T6.4 sidebar Manage opens the session panel
//   T3  T3.1 close busy session -> refusal toast surfaced, row/loaded stays
//       T3.2 close after idle -> loaded marker drops (close succeeded)
//   T4  T4.1 create sessions to the 8-session cap
//       T4.2 9th create refused with the cap error surfaced
//       T4.3 no eviction: earlier session switches back, loaded count == 8
//   T5  T5.1 Todo state never leaks A<->B
//       T5.2 Goal state never leaks A<->B
//       T5.3 Workflow state never leaks A<->B
//   T7  T7.1 Android 390x844: hamburger visible, drawer opens
//       T7.2 session pick closes the drawer
//       T7.3 picked session's transcript restores from the backend
//       T7.4 header has no feature buttons on mobile either
//
// Every passing contract records its machine-readable ID (T0.1..T7.4); on
// full success the lane writes $RPI_EVIDENCE/coverage-assertions.json
// ({ "executed": [...] }). The Web coverage matrix
// (E2E.d/web/coverage_matrix.mjs, feature "multi-session") requires ALL of
// DOCUMENTED_IDS below: the matrix fails when any named contract is absent,
// and this lane fails — before writing evidence — unless every documented ID
// actually executed, so the 10th lane is quantitatively gated, never just a
// shell PASS.

import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const mockControlUrl = process.env.RPI_MOCK_CONTROL_URL || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

// Machine-readable executed-assertion evidence for the coverage matrix: every
// passing T<x>.<y> contract is recorded here (a Set, so repeated waits under
// one contract dedupe) and written to $RPI_EVIDENCE/coverage-assertions.json
// only after the FULL documented matrix below has executed.
const DOCUMENTED_IDS = [
  'T0.1', 'T0.2', 'T0.3',
  'T1.1', 'T1.2', 'T1.3', 'T1.4', 'T1.5', 'T1.6',
  'T2.1', 'T2.2', 'T2.3', 'T2.4', 'T2.5',
  'T3.1', 'T3.2',
  'T4.1', 'T4.2', 'T4.3',
  'T5.1', 'T5.2', 'T5.3',
  'T6.1', 'T6.2', 'T6.3', 'T6.4',
  'T7.1', 'T7.2', 'T7.3', 'T7.4',
];
const executed = new Set();
function record(id) {
  executed.add(id);
  console.log(`[web-sess:assert] ${id}`);
}

function fail(message) {
  // Exit 2 (not 1): run.sh treats 1 as "npm install unavailable -> fall
  // back", which is FORBIDDEN for this lane. Any non-zero exit fails it.
  console.error(`web-sessions: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, id, timeoutMs = 30000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${id} (timeout ${timeoutMs}ms)`);
  }
  // Every label in this lane starts with its contract's T<x>.<y> ID; record
  // it on success so the coverage matrix can quantify this lane. Direct
  // (non-waitFor) checks record explicitly at their own sites.
  const tid = typeof id === 'string' ? (id.match(/^T\d+\.\d+/) || [null])[0] : null;
  if (tid) record(tid);
}

async function activeTranscript(page) {
  return page.evaluate(() => document.getElementById('transcript')?.textContent || '');
}

async function rowIds(page) {
  return page.evaluate(() =>
    [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId || '')
  );
}

async function rowExists(page, sid) {
  return page.evaluate((s) => {
    return [...document.querySelectorAll('.session-sidebar__switch')].some((r) => r.dataset.sessionId === s);
  }, sid);
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

async function rowLoaded(page, sid) {
  return page.evaluate((s) => {
    return document.querySelector(`.session-sidebar__loaded[data-loaded-for="${s}"]`) !== null;
  }, sid);
}

async function clickRowClose(page, sid, id) {
  await waitFor(
    page,
    (s) => {
      const btn = document.querySelector(`.session-sidebar__close[data-session-id="${s}"]`);
      return btn !== null && !btn.disabled;
    },
    `${id}: close button for session ${sid} never became clickable`,
    30000,
    sid
  );
  await page.evaluate((s) => {
    document.querySelector(`.session-sidebar__close[data-session-id="${s}"]`).click();
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

async function promptAndWait(page, prompt, reply, id, timeoutMs = 30000) {
  await page.fill('#prompt-input', prompt);
  await page.press('#prompt-input', 'Enter');
  await waitFor(page, (r) => document.body.textContent.includes(r), id, timeoutMs, reply);
}

async function waitForBadge(page, visible, id, timeoutMs = 30000) {
  await waitFor(
    page,
    (v) => document.getElementById('stream-badge')?.hidden !== v,
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
  'todos-toggle-btn',
  'goal-panel-btn',
  'workflow-toggle-btn',
  'sidechat-toggle-btn',
  'maintenance-toggle-btn',
  'subagents-toggle-btn',
  'session-toggle-btn',
  'settings-toggle-btn',
];

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
  record(id);
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
    page.on('pageerror', (err) => {
      console.error(`web-sessions: page error: ${err.message}`);
    });

    /* ---------------- T0: boot + connect ---------------- */
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'T0.1: page title missing');
    await waitFor(page, () => document.querySelector('#conn-state') !== null, 'T0.1: conn-state missing');
    await page.click('#settings-toggle-btn');
    await waitFor(page, () => document.querySelector('#settings-token-input') !== null, 'settings token input missing');
    await page.fill('#settings-token-input', token);
    await page.click('#settings-token-save-btn');
    await page.click('#settings-close-btn');
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'T0.2: WS did not reach "connected"'
    );
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
    const knownIds = [sessionA];
    await page.screenshot({ path: `${evidence}/sessions-t0-boot.png`, fullPage: true });

    /* ---------------- T1: slow A continues while B is active ---------------- */
    // T1.1: instant round-trip persists A, then a slow stream starts on A.
    await promptAndWait(page, 'a-prompt-0', 'sessions-reply: a-prompt-0', 'T1.1: A initial prompt never round-tripped');
    await promptAndWait(page, 'sessions-slow-a', 'sessions-slow-a-1/', 'T1.1: A slow stream never started');
    await waitForBadge(page, true, 'T1.1: A streaming badge never appeared');

    // T1.2: New while A is mid-stream; the fresh session B shows the empty view.
    await page.click('#sidebar-new-session-btn');
    await waitForEmptyView(page, 'T1.2: session B did not show the empty new-session view');
    const bTranscript = await activeTranscript(page);
    if (bTranscript.includes('slow-a-')) {
      fail("T1.2: session B's empty view leaked session A's streaming text");
    }

    // T1.3: B's prompt completes while A's stream keeps running (the prompt
    // also flushes B's recorder so its sidebar row appears deterministically).
    await promptAndWait(page, 'b-prompt-1', 'sessions-reply: b-prompt-1', 'T1.3: B prompt never round-tripped while A streams');
    const sessionB = await waitForNewRow(page, knownIds, 'T1.2: session B row never appeared in the sidebar');
    knownIds.push(sessionB);

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
    await page.screenshot({ path: `${evidence}/sessions-t1-background.png`, fullPage: true });

    // T1.5/T1.6: switch back to A — full stream text restored, unread cleared.
    await clickRow(page, sessionA, 'T1.5: switch back to session A');
    await waitFor(page, (s) => document.body.textContent.includes('slow-a-done'), 'T1.5: A\'s slow stream never completed (while B was active)', 40000);
    await waitForBadge(page, false, 'T1.5: A streaming badge did not clear after the stream completed');
    if ((await rowUnread(page, sessionA)) !== 0) {
      fail("T1.5: A's unread badge never cleared after switching back");
    }
    const aTranscript = await activeTranscript(page);
    if (!aTranscript.includes('sessions-reply: a-prompt-0') || !aTranscript.includes('slow-a-done')) {
      fail("T1.6: A's authoritative transcript was not restored (history missing)");
    }
    record('T1.6');
    await page.screenshot({ path: `${evidence}/sessions-t1-restored.png`, fullPage: true });

    /* ---------------- T2: abort/toast isolation ---------------- */
    await promptAndWait(page, 'sessions-slow-a2', 'sessions-slow-a2-1/', 'T2.1: A second slow stream never started');
    await waitForBadge(page, true, 'T2.1: A streaming badge missing for the second stream');

    await page.click('#sidebar-new-session-btn');
    await waitForEmptyView(page, 'T2.2: session C did not show the empty new-session view');
    await promptAndWait(page, 'sessions-slow-b', 'sessions-slow-b-1/', 'T2.2: C slow stream never started');
    const sessionC = await waitForNewRow(page, knownIds, 'T2.2: session C row never appeared');
    knownIds.push(sessionC);
    await waitForBadge(page, true, 'T2.2: C streaming badge missing');

    // T2.3: abort C -> neutral toast; T2.4: tail chunks never render.
    await page.click('#abort-btn');
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
    await page.screenshot({ path: `${evidence}/sessions-t2-abort-isolation.png`, fullPage: true });

    /* ---------------- T6: desktop rail collapse/reopen + sidebar controls ---------------- */
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
        if (layout.classList.contains('app-layout--drawer-open')) return false;
        if (getComputedStyle(reopen).display === 'none') return false;
        if (nav && getComputedStyle(nav).display !== 'none') return false;
        return sidebar.getBoundingClientRect().width <= 60;
      },
      'T6.2: desktop rail never collapsed to the compact strip',
      15000
    );
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
    // T6.4: Manage opens the session panel.
    await page.click('#sidebar-manage-btn');
    await waitFor(page, () => document.getElementById('session-panel') !== null, 'T6.4: session panel never opened from Manage');
    await page.click('#session-close-btn');
    await waitFor(page, () => document.getElementById('session-panel') === null, 'T6.4: session panel never closed');
    // T6.4 (open+close the session panel via Manage) collapses the desktop
    // rail; re-open it before the T3/T4 New actions. Wait on the exact
    // control we are about to click (the New button, which lives in the
    // sidebar header), not on the nav, so the next click is observable.
    await page.click('#rail-reopen-btn');
    await waitFor(
      page,
      () => {
        const button = document.getElementById('sidebar-new-session-btn');
        return button !== null && getComputedStyle(button).display !== 'none';
      },
      'T6.4: New session control never restored after the session panel close'
    );
    /* ---------------- T3: close busy refusal then idle success ---------------- */
    await page.click('#sidebar-new-session-btn');
    await waitForEmptyView(page, 'T3.1: session D did not show the empty new-session view');
    // D's provider sends one non-terminal delta, then blocks on the test-owned
    // release barrier. Observing that exact delta proves D entered its turn;
    // the mock cannot emit its final tail or [DONE] before we release it.
    await promptAndWait(page, 'sessions-slow-b3', 'sessions-slow-b3-1/', 'T3.1: D held stream never entered its turn');
    const sessionD = await waitForNewRow(page, knownIds, 'T3.1: session D row never appeared');
    knownIds.push(sessionD);
    // Switch back to A so D runs in the background. Its first provider delta
    // already proves entry into D's turn; the provider remains held regardless
    // of client event timing until the explicit release below.
    await clickRow(page, sessionA, 'T3.1: switch back to A (D now background)');
    const dUnreadBeforeRelease = await rowUnread(page, sessionD);
    const dLoadedBefore = await rowLoaded(page, sessionD);
    if (!dLoadedBefore) fail('T3.1: D row lacks the loaded marker while it is running');
    // T3.1: close a BUSY session while the provider is still held. Because
    // release occurs only after the refusal assertions below, the close is
    // causally ordered before provider completion rather than timed by luck.
    await clickRowClose(page, sessionD, 'T3.1: close button for held busy D');
    await waitFor(
      page,
      () => {
        const toasts = [...document.querySelectorAll('#toasts .toast--error')];
        return toasts.some(
          (t) => (t.textContent || '').includes('close_session failed') && (t.textContent || '').includes('busy')
        );
      },
      'T3.1: busy-close refusal never surfaced (expected "session is busy" toast)',
      15000
    );
    if (!(await rowExists(page, sessionD))) fail('T3.1: busy-close refusal removed the session row (must be non-destructive)');
    if (!(await rowLoaded(page, sessionD))) fail('T3.1: busy-close refusal unloaded the session (must be non-destructive)');
    await page.screenshot({ path: `${evidence}/sessions-t3-busy-refused.png`, fullPage: true });

    // T3.2: only after busy refusal/non-destruction is observed do we release
    // D's provider. Require an HTTP 200 acknowledgement, then require its two
    // terminal background events (message_end + agent_settled), activate D to
    // prove the authoritative final tail and idle badge, then close it from A.
    if (!mockControlUrl) fail('T3.2: RPI_MOCK_CONTROL_URL is required for the session release barrier');
    let releaseResponse;
    try {
      releaseResponse = await fetch(`${mockControlUrl}/__release-session`, { method: 'POST', body: '{}' });
    } catch (error) {
      fail(`T3.2: session provider release request failed: ${error}`);
    }
    if (!releaseResponse.ok) {
      fail(`T3.2: session provider release returned HTTP ${releaseResponse.status}`);
    }
    const releaseBody = await releaseResponse.json().catch(() => null);
    if (releaseBody?.released !== true) fail('T3.2: session provider release acknowledgement was invalid');
    await waitFor(
      page,
      ({ sid, before }) => {
        const badge = document.querySelector(`.session-sidebar__unread[data-unread-for="${sid}"]`);
        return badge !== null && Number(badge.dataset.unread || 0) >= before + 2;
      },
      'T3.2: released D never emitted message_end and agent_settled',
      30000,
      { sid: sessionD, before: dUnreadBeforeRelease }
    );
    await clickRow(page, sessionD, 'T3.2: activate released D to prove its final transcript');
    await waitFor(
      page,
      () => document.body.textContent.includes('slow-b3-done')
        && document.getElementById('stream-badge')?.hidden === true,
      'T3.2: released D never rendered its final tail and idle state',
      30000
    );
    await clickRow(page, sessionA, 'T3.2: switch back to A before closing idle D');
    await clickRowClose(page, sessionD, 'T3.2: close button for settled D');
    await waitFor(
      page,
      (s) => !document.querySelector(`.session-sidebar__loaded[data-loaded-for="${s}"]`),
      'T3.2: close of the idle session never succeeded (loaded marker still present)',
      30000,
      sessionD
    );
    await page.screenshot({ path: `${evidence}/sessions-t3-closed.png`, fullPage: true });

    /* ---------------- T4: 8-session cap, no eviction ---------------- */
    await clickRow(page, sessionA, 'T4.1: switch back to A before creating more sessions');
    const created = [];
    for (let i = 1; i <= 5; i++) {
      await page.click('#sidebar-new-session-btn');
      await waitForEmptyView(page, `T4.1: session S${i} did not show the empty new-session view`);
      // The prompt flushes the recorder so the catalog row appears.
      await promptAndWait(page, `persist-${i}`, `sessions-reply: persist-${i}`, `T4.1: persist-${i} never round-tripped`);
      const sid = await waitForNewRow(page, knownIds, `T4.1: session S${i} row never appeared`, 30000);
      knownIds.push(sid);
      created.push(sid);
    }
    // T4.2: the 9th loaded session (8 = cap, counting the primary) is refused.
    await page.click('#sidebar-new-session-btn');
    await waitFor(
      page,
      () => (document.querySelector('.session-sidebar__error')?.textContent || '').includes('too many concurrent sessions'),
      'T4.2: the 9th session create was not refused with the cap error',
      20000
    );
    await page.screenshot({ path: `${evidence}/sessions-t4-cap.png`, fullPage: true });
    // T4.3: no eviction — an earlier child session still switches back with
    // its transcript, and all 8 loaded sessions keep the loaded marker.
    const earlier = created[3];
    await clickRow(page, earlier, 'T4.3: switch back to an earlier created session');
    await waitFor(
      page,
      (r) => document.body.textContent.includes(r),
      'T4.3: earlier session transcript lost after the cap refusal',
      30000,
      'sessions-reply: persist-4'
    );
    await waitFor(
      page,
      (s) => !!document.querySelector(`.session-sidebar__loaded[data-loaded-for="${s}"]`),
      'T4.3: earlier session was evicted by the cap refusal',
      20000,
      earlier
    );
    const loadedCount = await page.evaluate(() => document.querySelectorAll('.session-sidebar__loaded').length);
    if (loadedCount !== 8) {
      fail(`T4.3: expected exactly 8 loaded sessions after the cap refusal, found ${loadedCount} (eviction?)`);
    }
    await page.screenshot({ path: `${evidence}/sessions-t4-no-eviction.png`, fullPage: true });

    /* ---------------- T5: Todo/Goal/Workflow isolation ---------------- */
    // T5.1 Todo: create in A, verify absent in B and back in A.
    await clickRow(page, sessionA, 'T5.1: switch to A for the todo leak test');
    await page.click('#todos-toggle-btn');
    await waitFor(page, () => document.getElementById('todo-panel') !== null, 'T5.1: todo panel did not open on A');
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
    await page.click('#todos-toggle-btn');
    await waitFor(page, () => document.getElementById('todo-panel') !== null, 'T5.1: todo panel did not open on B');
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
    await page.click('#todos-toggle-btn');
    await waitFor(page, () => document.getElementById('todo-panel') !== null, 'T5.1: todo panel did not reopen on A');
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

    // T5.3 Workflow: create in A, verify B's list stays empty of it.
    await clickRow(page, sessionA, 'T5.3: switch to A for the workflow leak test');
    await page.click('#workflow-toggle-btn');
    await waitFor(page, () => document.getElementById('workflow-panel') !== null, 'T5.3: workflow panel did not open on A');
    await page.fill('#workflow-create-name', 'a-wf');
    await page.fill('#workflow-create-objective', 'a-wf-objective');
    await page.click('#workflow-create-btn');
    await waitFor(
      page,
      () => [...document.querySelectorAll('.workflow-row')].some((r) => (r.textContent || '').includes('a-wf'))
        || document.querySelector('.workflow-panel__error') !== null,
      'T5.3: workflow create returned neither a row nor an error',
      30000
    );
    const workflowCreateError = await page.evaluate(
      () => document.querySelector('.workflow-panel__error')?.textContent || ''
    );
    if (workflowCreateError !== '') fail(`T5.3: workflow create failed on A: ${workflowCreateError}`);
    await page.click('#workflow-close-btn');
    await waitFor(page, () => document.getElementById('workflow-panel') === null, 'T5.3: workflow panel did not close on A');

    await clickRow(page, sessionB, 'T5.3: switch to B for the workflow leak check');
    await page.click('#workflow-toggle-btn');
    await waitFor(page, () => document.getElementById('workflow-panel') !== null, 'T5.3: workflow panel did not open on B');
    await waitFor(
      page,
      () => {
        const rows = [...document.querySelectorAll('.workflow-row')];
        return !rows.some((r) => (r.textContent || '').includes('a-wf'));
      },
      "T5.3: session A's workflow leaked into session B's list",
      30000
    );
    await page.screenshot({ path: `${evidence}/sessions-t5-isolation.png`, fullPage: true });
    await page.click('#workflow-close-btn');
    await waitFor(page, () => document.getElementById('workflow-panel') === null, 'T5.3: workflow panel did not close on B');

    /* ---------------- T7: Android 390x844 drawer ---------------- */
    const mobile = await browser.newPage({ viewport: { width: 390, height: 844 } });
    mobile.on('pageerror', (err) => {
      console.error(`web-sessions: mobile page error: ${err.message}`);
    });
    await mobile.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(mobile, () => document.title === 'rpi web', 'T7.1: mobile page title missing');
    await mobile.click('#settings-toggle-btn');
    await waitFor(mobile, () => document.querySelector('#settings-token-input') !== null, 'settings token input missing');
    await mobile.fill('#settings-token-input', token);
    await mobile.click('#settings-token-save-btn');
    await mobile.click('#settings-close-btn');
    await waitFor(
      mobile,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'T7.1: mobile WS did not reach "connected"'
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
    // T7.2/T7.3: picking session B closes the drawer and restores B's
    // transcript straight from the backend history.
    await clickRow(mobile, sessionB, 'T7.2: pick session B from the drawer');
    await waitFor(
      mobile,
      () => !document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open'),
      'T7.2: drawer never closed after the session pick',
      20000
    );
    await waitFor(
      mobile,
      (r) => document.body.textContent.includes(r),
      'T7.3: picked session B transcript never restored from the backend',
      30000,
      'sessions-reply: b-prompt-1'
    );
    await mobile.screenshot({ path: `${evidence}/sessions-t7-picked.png`, fullPage: true });
    await mobile.close();

    // The lane may only report PASS (and may only write evidence) once the
    // FULL documented matrix executed: a renamed/renumbered contract that no
    // longer records its ID fails the lane instead of silently shrinking the
    // evidence the coverage matrix quantifies.
    const missing = DOCUMENTED_IDS.filter((id) => !executed.has(id));
    if (missing.length > 0) {
      fail(`T-lane evidence incomplete: ${missing.join(', ')} never executed (coverage matrix contract)`);
    }
    fs.mkdirSync(evidence, { recursive: true });
    fs.writeFileSync(path.join(evidence, 'coverage-assertions.json'), JSON.stringify({ executed: [...executed] }, null, 2));

    console.log(`web-sessions: PASSED (${executed.size}/${DOCUMENTED_IDS.length} assertions, T0 boot, T1 concurrent streaming + unread + restore, T2 abort/toast isolation, T6 rail collapse/reopen + header, T3 close busy/idle, T4 8-session cap + no eviction, T5 Todo/Goal/Workflow isolation, T7 390x844 drawer pick-close) — evidence at ${path.join(evidence, 'coverage-assertions.json')}`);
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-sessions: playwright crashed: ${err && err.stack ? err.stack : err}`);
  process.exit(2);
});
