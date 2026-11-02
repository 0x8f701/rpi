// Session management Move/Delete lane — PLAYWRIGHT-ONLY.
//
// Playwright half of E2E.d/web/session_move_delete.sh. Guards the shared
// product contract for the SessionSidebar Move/Delete menu actions:
//
//   SMD1  boot lists the four seeded rows; the resumed A row is ACTIVE
//   SMD2  ACTIVE row: Delete AND Move disabled, both titled with the exact
//         switch-away guidance; foreign row menu stays export-only
//   SMD3  non-active row (unloaded): Delete and Move enabled; the enabled
//         Move title explains saved-session relocation (never a cwd change)
//   SMD4  BUSY loaded secondary: switch to B, hold its turn on the mock
//         (`sessions-slow-b3`), switch back to A, then Delete B — the
//         backend refuses with EXACTLY ONE bounded error toast, exactly one
//         session_delete frame, and ZERO catalog mutation (row retained)
//   SMD5  idle (post-release) Delete: exactly one session_delete, row
//         removal, one Undo click inside the 5s window sends exactly one
//         session_restore and the row returns
//   SMD6  Move: the empty picker is a disabled submit (Enter with a blank
//         value sends nothing), the explicit absolute target sends exactly
//         one session_move with the trimmed target, the response toast is
//         the fixed `Session moved`, the catalog regroups the destination
//         row under the target project while the SOURCE group loses it, the
//         active session is retained (zero switch_session frames), and an
//         explicit click on the destination row restores its transcript
//   SMD7  mobile viewport: drawer + touch long-press eligibility — active
//         row disabled, non-active (now idle-loaded) row enabled
//
// Evidence (all labels synthetic): smd-*.png screenshots plus
// session-move-delete-assertions.json ({ executed: [...] }).
//
// Exit codes: 0 = PASS, 2 = deterministic assertion failure.

import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const mockPort = process.env.MOCK_PORT || '';
const TARGET_CWD = process.env.SMD_TARGET_CWD || '';
const SOURCE_PROJECT = process.env.SMD_SOURCE_PROJECT || 'workspace';
const TARGET_PROJECT = process.env.SMD_TARGET_PROJECT || 'beta';

const SID_A = 'smd-active-a';
const SID_B = 'smd-idle-b';
const SID_C = 'smd-unloaded-c';
const SID_F = 'codex-ext-smd';
const ACTIVE_GUIDANCE = 'Switch away from this session first';
// Must mirror MOVE_SESSION_TITLE in SessionSidebar.tsx.
const MOVE_TITLE =
  'Move this saved session under another project directory; the moved session’s cwd becomes the target (the active session stays active)';

const executed = [];
function record(id) {
  if (!executed.includes(id)) executed.push(id);
}

function fail(message) {
  console.error(`session-move-delete: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 30000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch (err) {
    fail(`${label}: ${String(err.message).split('\n')[0]}`);
  }
}

async function sidebarState(page) {
  return page.evaluate(() => ({
    active: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
    rows: [...document.querySelectorAll('.session-sidebar__switch')].map((r) => ({
      sid: r.dataset.sessionId || '',
      loaded: r.closest('.session-sidebar__row')?.dataset.loaded === 'true',
    })),
  }));
}

function requireRow(state, sid, label) {
  const row = state.rows.find((r) => r.sid === sid);
  if (!row) fail(`${label}: sidebar row ${sid} missing (rows: ${state.rows.map((r) => r.sid).join(', ')})`);
  return row;
}

async function menuItems(page) {
  return page.evaluate(() =>
    [...document.querySelectorAll('.session-sidebar__menu [role="menuitem"]')].map((el) => ({
      label: (el.textContent || '').trim(),
      disabled: el.disabled === true,
      title: el.getAttribute('title') || '',
    })),
  );
}

async function openRowMenu(page, sid, label) {
  const row = page.locator(`.session-sidebar__switch[data-session-id="${sid}"]`);
  await row.click({ button: 'right' });
  await waitFor(page, () => document.querySelector('.session-sidebar__menu') !== null,
    `${label}: context menu never opened`, 10000);
}

async function clickMenuItem(page, label) {
  const clicked = await page.evaluate((want) => {
    const item = [...document.querySelectorAll('.session-sidebar__menu [role="menuitem"]')]
      .find((el) => (el.textContent || '').trim() === want && el.disabled !== true);
    if (!item) return false;
    item.click();
    return true;
  }, label);
  if (!clicked) fail(`menu item '${label}' missing or disabled at click time`);
}

async function clickConfirmDelete(page, label) {
  await clickMenuItem(page, 'Delete');
  await waitFor(
    page,
    () => [...document.querySelectorAll('.session-sidebar__menu [role="menuitem"]')]
      .some((el) => (el.textContent || '').trim() === 'Confirm delete'),
    `${label}: delete confirmation never appeared`,
    10000,
  );
  await clickMenuItem(page, 'Confirm delete');
}

async function closeRowMenu(page) {
  for (let attempt = 0; attempt < 2; attempt += 1) {
    await page.keyboard.press('Escape');
    if (await page.evaluate(() => document.querySelector('.session-sidebar__menu') === null)) return;
  }
  await waitFor(page, () => document.querySelector('.session-sidebar__menu') === null,
    'context menu never closed', 10000);
}

async function toasts(page) {
  return page.evaluate(() =>
    [...document.querySelectorAll('#toasts .toast')].map((t) => ({
      message: (t.querySelector('.toast__message')?.textContent || '').trim(),
      error: t.classList.contains('toast--error'),
      action: (t.querySelector('.toast__action')?.textContent || '').trim(),
    })),
  );
}

async function waitForToast(page, pred, label, timeoutMs = 8000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const list = await toasts(page);
    const hit = list.find(pred);
    if (hit) return list;
    await page.waitForTimeout(100);
  }
  fail(`${label}: expected toast never appeared (toasts: ${JSON.stringify(await toasts(page))})`);
}

const frameCount = (page, type) =>
  page.evaluate((t) => (window.__rpiSmd?.frames || []).filter((e) => e.frame.type === t).length, type);

async function switchTo(page, sid, label) {
  await waitFor(
    page,
    (target) => {
      const row = [...document.querySelectorAll('.session-sidebar__switch')]
        .find((candidate) => candidate.dataset.sessionId === target);
      return row !== undefined && !row.disabled;
    },
    `${label}: row ${sid} never became clickable`,
    15000,
    sid,
  );
  await page.evaluate((target) => {
    [...document.querySelectorAll('.session-sidebar__switch')]
      .find((candidate) => candidate.dataset.sessionId === target)?.click();
  }, sid);
  await waitFor(
    page,
    (target) =>
      document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId === target,
    `${label}: switch to ${sid} never settled`,
    30000,
    sid,
  );
}

async function sendPrompt(page, text, label) {
  // Production idle gate (App.tsx): only click when the unified action reads
  // Send — while streaming the same button is Stop, so a missing idle gate
  // would silently abort instead of send. Missing element fails the gate.
  await waitFor(
    page,
    () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message',
    `${label}: composer never returned to the idle Send action`,
    15000,
  );
  await page.fill('#prompt-input', text);
  await page.click('#send-btn');
  await waitFor(
    page,
    (needle) => (document.getElementById('transcript')?.textContent || '').includes(needle),
    `${label}: prompt turn never started`,
    30000,
    text,
  );
}

async function releaseHeldStream(label) {
  if (!mockPort) fail(`${label}: MOCK_PORT missing — cannot release the held mock stream`);
  let res;
  try {
    res = await fetch(`http://127.0.0.1:${mockPort}/__release-session`, { method: 'POST' });
  } catch (err) {
    fail(`${label}: release POST failed: ${err}`);
  }
  if (!res.ok) fail(`${label}: release POST returned ${res.status}`);
}

/** Touch long-press (pointerdown held past SESSION_MENU_LONG_PRESS_MS=500). */
async function longPressRow(page, sid, label) {
  await page.evaluate((target) => {
    const row = [...document.querySelectorAll('.session-sidebar__switch')]
      .find((candidate) => candidate.dataset.sessionId === target);
    if (!row) throw new Error(`row ${target} missing`);
    const rect = row.getBoundingClientRect();
    row.dispatchEvent(new PointerEvent('pointerdown', {
      bubbles: true, cancelable: true, pointerId: 7, pointerType: 'touch', isPrimary: true,
      clientX: rect.left + 10, clientY: rect.top + 10,
    }));
    window.__smdLongPressRow = row;
  }, sid);
  await page.waitForTimeout(700); // past the 500ms long-press threshold
  await page.evaluate(() => {
    window.__smdLongPressRow?.dispatchEvent(new PointerEvent('pointerup', {
      bubbles: true, cancelable: true, pointerId: 7, pointerType: 'touch', isPrimary: true,
    }));
  });
  await waitFor(page, () => document.querySelector('.session-sidebar__menu') !== null,
    `${label}: long-press menu for ${sid} never opened`, 10000);
}

async function main() {
  if (!url) fail('RPI_URL is required');
  if (!TARGET_CWD.startsWith('/')) fail(`SMD_TARGET_CWD must be an absolute path (got '${TARGET_CWD}')`);

  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
    const page = await context.newPage();
    if (token) {
      await page.addInitScript((t) => {
        window.localStorage.setItem('rpi-web-token', t);
      }, token);
    }
    page.on('pageerror', (err) => console.error(`session-move-delete: pageerror: ${err.message}`));

    // Capture every outbound WebSocket frame so exact-one wire assertions
    // (session_delete / session_move / session_restore / switch_session)
    // are provable, not assumed.
    await page.addInitScript(() => {
      const control = { frames: [] };
      window.__rpiSmd = control;
      const NativeWebSocket = window.WebSocket;
      class SmdWebSocket extends NativeWebSocket {
        send(data) {
          if (typeof data === 'string') {
            try {
              const frame = JSON.parse(data);
              if (frame && typeof frame.type === 'string') {
                control.frames.push({ t: Date.now(), frame });
                if (control.frames.length > 2000) control.frames.shift();
              }
            } catch { /* non-JSON passes through */ }
          }
          return super.send(data);
        }
      }
      for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
        Object.defineProperty(SmdWebSocket, key, { value: NativeWebSocket[key] });
      }
      window.WebSocket = SmdWebSocket;
    });

    await page.goto(url, { waitUntil: 'domcontentloaded' });

    // SMD1 — boot: all four rows listed, A active (resumed primary).
    await waitFor(
      page,
      (sids) => {
        const present = new Set(
          [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId),
        );
        return sids.every((sid) => present.has(sid));
      },
      'seeded rows never appeared after boot',
      30000,
      [SID_A, SID_B, SID_C, SID_F],
    );
    let state = await sidebarState(page);
    if (state.active !== SID_A) fail(`SMD1: active row is '${state.active}' (expected '${SID_A}')`);
    await page.screenshot({ path: path.join(evidence, 'smd-0-boot.png'), fullPage: true });
    record('SMD1.1');

    // Active idle sessions can move without changing their identity; Delete stays gated.
    await openRowMenu(page, SID_A, 'SMD2 active menu');
    let items = await menuItems(page);
    const activeDelete = items.find((i) => i.label === 'Delete');
    const activeMove = items.find((i) => i.label === 'Move session…');
    if (!activeDelete || activeDelete.disabled !== true) fail('SMD2.1: active row Delete is not disabled');
    if (activeDelete.title !== ACTIVE_GUIDANCE) {
      fail(`SMD2.1: active Delete title '${activeDelete.title}' is not the switch-away guidance`);
    }
    if (!activeMove || activeMove.disabled === true) fail('SMD2.2: active idle row Move is disabled');
    await page.screenshot({ path: path.join(evidence, 'smd-1-active-menu.png'), fullPage: true });
    await closeRowMenu(page);

    await openRowMenu(page, SID_F, 'SMD2 foreign menu');
    items = await menuItems(page);
    const foreignLabels = items.map((i) => i.label);
    if (foreignLabels.join('|') !== 'Export HTML') {
      fail(`SMD2.3: foreign menu is not export-only: ${foreignLabels.join(' | ')}`);
    }
    await closeRowMenu(page);
    record('SMD2.1');
    record('SMD2.2');
    record('SMD2.3');

    // SMD3 — non-active unloaded row: Delete and Move enabled; enabled Move
    // carries the relocation title (not the switch-away guidance).
    await openRowMenu(page, SID_C, 'SMD3 unloaded menu');
    items = await menuItems(page);
    const unloadedDelete = items.find((i) => i.label === 'Delete');
    const unloadedMove = items.find((i) => i.label === 'Move session…');
    if (!unloadedDelete || unloadedDelete.disabled === true) fail('SMD3.1: non-active unloaded Delete is not enabled');
    if (!unloadedMove || unloadedMove.disabled === true) fail('SMD3.1: non-active unloaded Move is not enabled');
    if (unloadedMove.title !== MOVE_TITLE) {
      fail(`SMD3.2: enabled Move title '${unloadedMove.title}' does not explain saved-session relocation`);
    }
    await closeRowMenu(page);

    await openRowMenu(page, SID_B, 'SMD3 idle-b menu');
    items = await menuItems(page);
    const bDelete = items.find((i) => i.label === 'Delete');
    const bMove = items.find((i) => i.label === 'Move session…');
    if (!bDelete || bDelete.disabled === true) fail('SMD3.2: non-active unloaded B Delete is not enabled');
    if (!bMove || bMove.disabled === true) fail('SMD3.2: non-active unloaded B Move is not enabled');
    await closeRowMenu(page);
    record('SMD3.1');
    record('SMD3.2');

    // SMD4 — busy loaded secondary: hold B's turn, switch back to A, delete
    // B -> backend refusal, one bounded error toast, zero catalog mutation.
    await switchTo(page, SID_B, 'SMD4 load B');
    await sendPrompt(page, 'sessions-slow-b3', 'SMD4 hold B busy');
    // Exact busy evidence: the mock's FIRST slow delta is authoritative —
    // the turn is genuinely in flight and held on the release POST. Hard
    // fail otherwise (no soft notes): a delete attempt without a held turn
    // would prove nothing.
    await waitFor(
      page,
      () => (document.getElementById('transcript')?.textContent || '').includes('sessions-slow-b3-1/'),
      'SMD4: held stream first delta (sessions-slow-b3-1/) never appeared',
      20000,
    );
    state = await sidebarState(page);
    if (requireRow(state, SID_B, 'SMD4 pre-switch').loaded !== true) {
      fail('SMD4: B row is not loaded (data-loaded) while its turn is streaming');
    }
    await switchTo(page, SID_A, 'SMD4 return to A');
    state = await sidebarState(page);
    requireRow(state, SID_B, 'SMD4 pre-delete');

    await openRowMenu(page, SID_B, 'SMD4 busy delete menu');
    const busyMove = (await menuItems(page)).find((item) => item.label === 'Move session…');
    if (!busyMove || busyMove.disabled !== true) fail('SMD2.2: busy session Move still clickable');
    const deletesBefore = await frameCount(page, 'session_delete');
    await clickMenuItem(page, 'Delete');
    await waitFor(
      page,
      () => [...document.querySelectorAll('.session-sidebar__menu [role="menuitem"]')]
        .some((el) => (el.textContent || '').trim() === 'Confirm delete'),
      'SMD4: delete confirmation never appeared',
      10000,
    );
    await clickMenuItem(page, 'Confirm delete');
    await waitForToast(
      page,
      (t) => t.error === true && /busy/i.test(t.message),
      'SMD4: busy refusal toast',
      10000,
    );
    await page.screenshot({ path: path.join(evidence, 'smd-2-busy-refusal.png'), fullPage: true });
    const deletesAfter = await frameCount(page, 'session_delete');
    if (deletesAfter - deletesBefore !== 1) {
      fail(`SMD4.1: busy refusal sent ${deletesAfter - deletesBefore} session_delete frames (expected exactly 1)`);
    }
    // One bounded toast, not a toast storm.
    await page.waitForTimeout(1500);
    const busyToasts = (await toasts(page)).filter((t) => t.error && /busy/i.test(t.message));
    if (busyToasts.length !== 1) fail(`SMD4.1: ${busyToasts.length} busy refusal toasts visible (expected exactly 1)`);
    // The refused delete is NOT a success: the confirm menu stays open and
    // no archived toast appears.
    const confirmStillOpen = await page.evaluate(
      () => [...document.querySelectorAll('.session-sidebar__menu [role="menuitem"]')]
        .some((el) => (el.textContent || '').trim() === 'Confirm delete'),
    );
    if (!confirmStillOpen) fail('SMD4.1: refused delete closed the confirm menu (refusal must keep it open)');
    const archivedOnRefusal = (await toasts(page)).some((t) => t.message === 'Session archived');
    if (archivedOnRefusal) fail('SMD4.1: refused delete produced a Session archived toast');
    await closeRowMenu(page);
    state = await sidebarState(page);
    requireRow(state, SID_B, 'SMD4 post-refusal');
    if (state.active !== SID_A) fail(`SMD4.2: active changed after busy refusal ('${state.active}')`);
    record('SMD4.1');
    record('SMD4.2');

    // Release the held stream and settle DETERMINISTICALLY: view B and wait
    // for the mock's exact slow tail marker (never a fixed sleep), then
    // return to A — B's turn has finished streaming before the idle delete.
    await releaseHeldStream('SMD4 release');
    await switchTo(page, SID_B, 'SMD4 settle: view B after release');
    await waitFor(
      page,
      () => (document.getElementById('transcript')?.textContent || '').includes('slow-b3-done'),
      'SMD4: slow tail (slow-b3-done) never completed after release',
      20000,
    );
    await switchTo(page, SID_A, 'SMD4 settle: back to A');

    // SMD5 — idle (loaded) delete: exact one session_delete, row removal,
    // Undo sends exactly one session_restore and restores the row.
    await openRowMenu(page, SID_B, 'SMD5 idle delete menu');
    items = await menuItems(page);
    const idleDelete = items.find((i) => i.label === 'Delete');
    if (!idleDelete || idleDelete.disabled === true) {
      fail('SMD5.1: idle loaded B Delete is not enabled (loaded guard regressed)');
    }
    const idleDeletesBefore = await frameCount(page, 'session_delete');
    const restoresBefore = await frameCount(page, 'session_restore');
    await clickMenuItem(page, 'Delete');
    await waitFor(
      page,
      () => [...document.querySelectorAll('.session-sidebar__menu [role="menuitem"]')]
        .some((el) => (el.textContent || '').trim() === 'Confirm delete'),
      'SMD5: delete confirmation never appeared',
      10000,
    );
    await clickMenuItem(page, 'Confirm delete');
    await waitFor(
      page,
      (target) => ![...document.querySelectorAll('.session-sidebar__switch')]
        .some((r) => r.dataset.sessionId === target),
      'SMD5.1: deleted B row never left the catalog',
      15000,
      SID_B,
    );
    const idleDeletesAfter = await frameCount(page, 'session_delete');
    if (idleDeletesAfter - idleDeletesBefore !== 1) {
      fail(`SMD5.1: delete sent ${idleDeletesAfter - idleDeletesBefore} session_delete frames (expected exactly 1)`);
    }
    const archivedToasts = await waitForToast(
      page,
      (t) => t.message === 'Session archived' && t.action === 'Undo',
      'SMD5.2: archive+Undo toast',
      8000,
    );
    if (archivedToasts.filter((t) => t.message === 'Session archived').length !== 1) {
      fail('SMD5.2: more than one Session archived toast visible');
    }
    await page.screenshot({ path: path.join(evidence, 'smd-3-deleted.png'), fullPage: true });
    // Undo within the 5s window.
    await page.evaluate(() => {
      const toast = [...document.querySelectorAll('#toasts .toast')]
        .find((t) => (t.querySelector('.toast__message')?.textContent || '').trim() === 'Session archived');
      const action = toast?.querySelector('.toast__action');
      if (!action) throw new Error('Undo action missing');
      action.click();
    });
    await waitFor(
      page,
      (before) => (window.__rpiSmd?.frames || []).filter((e) => e.frame.type === 'session_restore').length > before,
      'SMD5.3: Undo never sent a session_restore frame',
      8000,
      restoresBefore,
    );
    const restoresAfter = await frameCount(page, 'session_restore');
    if (restoresAfter - restoresBefore !== 1) {
      fail(`SMD5.2: Undo sent ${restoresAfter - restoresBefore} session_restore frames (expected exactly 1)`);
    }
    await waitFor(
      page,
      (target) => [...document.querySelectorAll('.session-sidebar__switch')]
        .some((r) => r.dataset.sessionId === target),
      'SMD5.2: undone B row never returned to the catalog',
      15000,
      SID_B,
    );
    await page.screenshot({ path: path.join(evidence, 'smd-4-undone.png'), fullPage: true });
    record('SMD5.1');
    record('SMD5.2');

    // SMD6 — Move C to the absolute target project directory.
    await openRowMenu(page, SID_C, 'SMD6 move menu');
    await clickMenuItem(page, 'Move session…');
    await waitFor(page, () => document.getElementById('session-move-cwd-input') !== null,
      'SMD6: move target input never appeared', 10000);
    await page.fill('#session-move-cwd-input', '');
    // Empty picker: a disabled submit — the Move button is disabled until a
    // target is typed, blank Enter sends nothing, and the form stays open.
    const emptyDisabled = await page.evaluate(() =>
      document.getElementById('session-move-submit-btn')?.disabled === true);
    if (!emptyDisabled) fail('SMD6.1: Move button enabled with an empty target (must be a disabled submit)');
    const movesBeforeEmpty = await frameCount(page, 'session_move');
    await page.keyboard.press('Enter');
    await page.waitForTimeout(300);
    if ((await frameCount(page, 'session_move')) !== movesBeforeEmpty) {
      fail('SMD6.1: Enter with an empty picker sent a session_move frame (must be a disabled submit)');
    }
    const stillMoveMode = await page.evaluate(() => document.getElementById('session-move-cwd-input') !== null);
    if (!stillMoveMode) fail('SMD6.1: empty-picker Enter closed the move form (expected it to stay open)');
    // Explicit absolute target: exactly one session_move with the trimmed
    // absolute target. PathAutocomplete owns Enter while its suggestion menu
    // is open (first Enter may accept the identical suggestion), so the
    // lane presses Enter, polls, and only re-presses while NO frame has
    // been sent and the move form is still open.
    await page.fill('#session-move-cwd-input', TARGET_CWD);
    const movesBefore = await frameCount(page, 'session_move');
    const switchBefore = await frameCount(page, 'switch_session');
    const targetEnabled = await page.evaluate(() =>
      document.getElementById('session-move-submit-btn')?.disabled === false);
    if (!targetEnabled) fail('SMD6.2: Move button stayed disabled after an explicit target was typed');
    await page.keyboard.press('Enter');
    let moveSent = false;
    for (let attempt = 0; attempt < 3 && !moveSent; attempt += 1) {
      const deadline = Date.now() + 2000;
      while (Date.now() < deadline) {
        if ((await frameCount(page, 'session_move')) > movesBefore) {
          moveSent = true;
          break;
        }
        const formOpen = await page.evaluate(() => document.getElementById('session-move-cwd-input') !== null);
        if (!formOpen && !moveSent) fail('SMD6.2: move form closed without sending session_move');
        await page.waitForTimeout(100);
      }
      if (!moveSent) await page.keyboard.press('Enter');
    }
    if (!moveSent) fail('SMD6.2: move target never sent a session_move frame');
    await page.waitForTimeout(500); // bound the exactly-one window
    const movesAfter = await frameCount(page, 'session_move');
    if (movesAfter - movesBefore !== 1) {
      fail(`SMD6.2: move sent ${movesAfter - movesBefore} session_move frames (expected exactly 1)`);
    }
    const moveFrame = await page.evaluate(
      (before) => (window.__rpiSmd?.frames || []).filter((e) => e.frame.type === 'session_move').slice(-1)[0].frame,
      movesBefore,
    );
    if (moveFrame.targetCwd !== TARGET_CWD) fail(`SMD6.2: session_move targetCwd '${moveFrame.targetCwd}' != '${TARGET_CWD}'`);
    if (typeof moveFrame.sessionPath !== 'string' || moveFrame.sessionPath.length === 0) {
      fail('SMD6.2: session_move carried no sessionPath');
    }
    await waitForToast(page, (t) => t.message === 'Session moved', 'SMD6.3: move toast', 8000);
    await page.screenshot({ path: path.join(evidence, 'smd-5-moved.png'), fullPage: true });

    // Destination row regrouped under the target project; source group lost
    // the row; active retained with zero switch_session frames.
    await waitFor(
      page,
      ({ source, target, moved }) => {
        const seq = (() => {
          const lis = [...document.querySelectorAll('.session-sidebar__list > li')];
          return lis.map((li) => {
            if (li.classList.contains('session-sidebar__group')) {
              return {
                kind: 'group',
                groupKind: li.dataset.groupKind || '',
                label: (li.querySelector('.session-sidebar__group-label')?.textContent || '').trim(),
              };
            }
            return { kind: 'row', sid: li.querySelector('.session-sidebar__switch')?.dataset.sessionId || '' };
          });
        })();
        const slice = (label) => {
          const start = seq.findIndex((e) => e.kind === 'group' && e.groupKind === 'project' && e.label === label);
          if (start < 0) return null;
          let end = seq.length;
          for (let i = start + 1; i < seq.length; i += 1) {
            if (seq[i].kind === 'group') {
              end = i;
              break;
            }
          }
          return seq.slice(start + 1, end).filter((e) => e.kind === 'row').map((e) => e.sid);
        };
        return (slice(target) || []).includes(moved) && !(slice(source) || []).includes(moved);
      },
      'SMD6.3: destination row never regrouped under the target project',
      15000,
      { source: SOURCE_PROJECT, target: TARGET_PROJECT, moved: SID_C },
    );
    state = await sidebarState(page);
    requireRow(state, SID_C, 'SMD6 post-move');
    if (state.active !== SID_A) {
      fail(`SMD6.4: active changed after move ('${state.active}', expected retained '${SID_A}')`);
    }
    const switchAfter = await frameCount(page, 'switch_session');
    if (switchAfter - switchBefore !== 0) {
      fail(`SMD6.4: move triggered ${switchAfter - switchBefore} switch_session frames (expected 0 — no auto-switch)`);
    }

    // Explicit click restores the moved session's transcript.
    await switchTo(page, SID_C, 'SMD6.5 explicit switch to moved session');
    await waitFor(
      page,
      () => (document.querySelector('#transcript .transcript-surface:not([hidden])')?.textContent || '').includes('smd-c-seed-reply'),
      'SMD6.5: moved session transcript never restored after the explicit click',
      15000,
    );
    await page.screenshot({ path: path.join(evidence, 'smd-6-moved-transcript.png'), fullPage: true });
    record('SMD6.1');
    record('SMD6.2');
    record('SMD6.3');
    record('SMD6.4');
    record('SMD6.5');

    // SMD7 — mobile viewport: drawer + touch long-press eligibility. The
    // active row (C) stays disabled; the idle-LOADED A row is eligible
    // (its loaded state is asserted explicitly below, never assumed).
    await page.setViewportSize({ width: 390, height: 667 });
    // Drawer pre-state: only click the hamburger from a CLOSED drawer, then
    // assert the closed→open transition; an already-open pre-state skips
    // the click but must still end open.
    const drawerWasOpen = await page.evaluate(
      () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true,
    );
    if (!drawerWasOpen) {
      await page.click('#sidebar-toggle-btn');
      await waitFor(
        page,
        () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true,
        'SMD7: hamburger click never opened the drawer (closed→open)',
        10000,
      );
    }
    state = await sidebarState(page);
    if (requireRow(state, SID_A, 'SMD7 idle A').loaded !== true) {
      fail('SMD7: idle A row is not loaded (data-loaded=true) — mobile eligibility context invalid');
    }
    await longPressRow(page, SID_C, 'SMD7.1 active long-press');
    items = await menuItems(page);
    const mobileActiveDelete = items.find((i) => i.label === 'Delete');
    if (!mobileActiveDelete || mobileActiveDelete.disabled !== true) fail('SMD7.1: mobile ACTIVE Delete is not disabled');
    await closeRowMenu(page);
    await longPressRow(page, SID_A, 'SMD7.2 idle-loaded long-press');
    items = await menuItems(page);
    const mobileLoadedDelete = items.find((i) => i.label === 'Delete');
    const mobileLoadedMove = items.find((i) => i.label === 'Move session…');
    if (!mobileLoadedDelete || mobileLoadedDelete.disabled === true) {
      fail('SMD7.2: mobile non-active LOADED Delete is not enabled');
    }
    if (!mobileLoadedMove || mobileLoadedMove.disabled === true) {
      fail('SMD7.2: mobile non-active LOADED Move is not enabled');
    }
    await page.screenshot({ path: path.join(evidence, 'smd-7-mobile.png'), fullPage: true });
    record('SMD7.1');
    record('SMD7.2');

    fs.writeFileSync(
      path.join(evidence, 'session-move-delete-assertions.json'),
      JSON.stringify({ executed }, null, 2),
    );
    console.log(`[session-move-delete] PASS: ${executed.join(', ')}`);
  } finally {
    await browser.close();
  }
}

main().catch((err) => fail(err && err.stack ? err.stack : String(err)));