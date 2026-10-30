// Web session UX E2E lane — PLAYWRIGHT-ONLY (E2E.d/web/session_ux.sh).
//
// Environment:
//   RPI_URL         http://127.0.0.1:<port>/web
//   RPI_TOKEN       token file content (served via rpi-auth.<token> subprotocol)
//   RPI_CHROME      executable path of the system Chrome (optional)
//   RPI_EVIDENCE    evidence dir for screenshots + executed-assertion evidence
//   RPI_SEED_META   JSON fixture metadata written by the lane's seeder
//
// Fixture contract (see session_ux.sh): the listener resumes the seeded
// native `session-ux-rename-seed` as its primary; the sidebar additionally
// lists the native `session-ux-context-seed` (conversation + active goal +
// todo snapshot), the native `session-ux-truncate-seed` (~73KiB user turn),
// and a foreign Pi row.
//
// Assertion matrix (feature -> IDs):
//   SUX0.1  boot lists all seeded rows; the active session is the rename seed
//   SUX0.2  native saved row context menu = exact native idle item set,
//           Copy context first
//   SUX1.1  Session panel opens from Manage Session
//   SUX1.2  rename input accumulates the typed multi-character name PER
//           CHARACTER via keyboard events (never fill)
//   SUX1.3  Enter immediately sends exactly one set_session_name frame with
//           the exact typed name and no sessionPath
//   SUX1.4  panel Name updates to the typed name; the header renders no
//           duplicate session label (no #session-name, no `session:` text)
//   SUX1.5  sidebar row title updates to the typed name, re-requested
//           through the authoritative boot session_list refresh (one page
//           reload), never the 8s sidebar catalog poll
//   SUX2.1  exactly one session_copy_context frame carrying the captured
//           row path
//   SUX2.2  clipboard starts with the fixed format headers and stays within
//           the 64KiB artifact bound
//   SUX2.3  clipboard contains the seeded conversation, goal, and todo,
//           sanitized: credential-shaped fixture text is [REDACTED], local
//           absolute paths are [PATH]
//   SUX2.4  copy never switches the active session and sends no
//           switch_session frame
//   SUX2.5  success toast is exactly "Copied context brief"
//   SUX3.1  oversized conversation copies as a bounded brief with clamped exchanges
//   SUX3.2  fresh completion toast is exactly "Copied context brief"
//   SUX4.1  foreign row menu never exposes Copy context (Export HTML only)
//   SUX4.2  zero page/console errors across the whole lane
//
// On full success the lane writes $RPI_EVIDENCE/coverage-assertions.json
// ({ "executed": [...] }) and the captured wire frames, with every
// sessionPath reduced to its basename (no absolute paths in evidence).

import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';
import { openManageSession } from './lib/session-lifecycle.mjs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const metaPath = process.env.RPI_SEED_META || '';
const coveragePath = path.join(evidence, 'coverage-assertions.json');

const DOCUMENTED_IDS = [
  'SUX0.1', 'SUX0.2',
  'SUX1.1', 'SUX1.2', 'SUX1.3', 'SUX1.4', 'SUX1.5',
  'SUX2.1', 'SUX2.2', 'SUX2.3', 'SUX2.4', 'SUX2.5',
  'SUX3.1', 'SUX3.2',
  'SUX4.1', 'SUX4.2',
];

const RENAME_NAME = 'Renamed Session UX 42';
// Exact native idle item set (SessionSidebar contextMenuItems): Move
// session… sits directly before Delete.
const NATIVE_IDLE_MENU = 'Copy context|Export HTML|Fork latest|Fork from…|Clone|Rename|Move session…|Delete';

const executed = new Set();
function record(id) {
  executed.add(id);
  console.log(`[web-session-ux:assert] ${id}`);
}

function fail(message) {
  // Exit 2 (not 1): run.sh treats 1 as "npm install unavailable -> fall
  // back", which is FORBIDDEN for this lane. Any non-zero exit fails it.
  console.error(`web-session-ux: FAIL: ${message}`);
  process.exit(2);
}

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
  const tid = typeof id === 'string' ? (id.match(/^SUX\d+\.\d+/) || [null])[0] : null;
  if (tid) record(tid);
}

async function outgoingFrames(page) {
  return page.evaluate(() =>
    ((window.__rpiSessionUxTest && window.__rpiSessionUxTest.frames) || []).map((entry) => entry.frame)
  );
}

async function activeSessionId(page) {
  return page.evaluate(() =>
    (document.querySelector('.session-sidebar__row--active .session-sidebar__switch') || {}).dataset?.sessionId || ''
  );
}

async function openRowMenu(page, sessionId, label) {
  const selector = `.session-sidebar__switch[data-session-id="${sessionId}"]`;
  const row = page.locator(selector);
  if ((await row.count()) !== 1) fail(`${label}: session row not found`);
  // A stray open menu would capture this right-click's mousedown; Escape
  // closes any previous menu before this row's contextmenu event.
  const stray = await page.evaluate(() => document.querySelector('.session-sidebar__menu') !== null);
  if (stray) {
    await page.keyboard.press('Escape');
    await waitFor(page, () => document.querySelector('.session-sidebar__menu') === null, `${label}: previous menu never closed`);
  }
  await row.click({ button: 'right' });
  await waitFor(page, () => document.querySelector('.session-sidebar__menu') !== null, `${label}: context menu never opened`);
}

async function menuLabels(page) {
  return page.evaluate(() =>
    [...document.querySelectorAll('.session-sidebar__menu [role="menuitem"]')].map((el) =>
      (el.textContent || '').trim()
    )
  );
}

async function clickMenuItem(page, wanted, label) {
  const clicked = await page.evaluate((target) => {
    const items = [...document.querySelectorAll('.session-sidebar__menu [role="menuitem"]')];
    const item = items.find((el) => (el.textContent || '').trim() === target && el.disabled !== true);
    if (!item) return false;
    item.click();
    return true;
  }, wanted);
  if (!clicked) fail(`${label}: menu item '${wanted}' missing or disabled`);
}

async function closeRowMenu(page, label) {
  await page.keyboard.press('Escape');
  await waitFor(page, () => document.querySelector('.session-sidebar__menu') === null, `${label}: context menu never closed`);
}

async function clipboardText(page) {
  return page.evaluate(() => navigator.clipboard.readText());
}

/** Evidence redaction: captured wire frames must never carry local absolute
 *  paths. sessionPath keeps its command semantics but is reduced to the
 *  session file basename; session ids and every other field pass through. */
function redactFrameForEvidence(frame) {
  if (!frame || typeof frame.sessionPath !== 'string' || frame.sessionPath === '') return frame;
  return { ...frame, sessionPath: path.basename(frame.sessionPath) };
}

async function main() {
  if (!url) fail('RPI_URL is required');
  if (!metaPath || !fs.existsSync(metaPath)) fail('RPI_SEED_META fixture metadata is required');
  const meta = JSON.parse(fs.readFileSync(metaPath, 'utf8'));
  // Fake credential-shaped strings, assembled exactly like the seeder: the
  // clipboard must carry their redacted form and never the raw fixture text.
  const fakeSk = 'sk-' + 'fakekey0123456789abcdef';
  const fakeApiValue = 'fake-cred-value-123456';

  fs.mkdirSync(evidence, { recursive: true });
  fs.rmSync(coveragePath, { force: true });

  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const context = await browser.newContext({
      viewport: { width: 1280, height: 800 },
      permissions: ['clipboard-read', 'clipboard-write'],
    });
    const page = await context.newPage();
    if (token) {
      await page.addInitScript((t) => {
        window.localStorage.setItem('rpi-web-token', t);
      }, token);
    }
    await page.addInitScript((sid) => {
      window.localStorage.setItem(
        `rpi-web-session:${encodeURIComponent(window.location.host.trim())}`,
        sid,
      );
    }, meta.activeId);
    const browserErrors = monitorBrowserErrors(page, 'session-ux');
    // Passive outgoing-frame recorder: wraps WebSocket.send and records every
    // string frame so the exact rename/copy RPC frames can be asserted
    // without altering any app behavior.
    await page.addInitScript(() => {
      const NativeWebSocket = window.WebSocket;
      const control = { frames: [] };
      class SessionUxTestWebSocket extends NativeWebSocket {
        send(data) {
          if (typeof data === 'string') {
            try {
              const frame = JSON.parse(data);
              if (frame && typeof frame.type === 'string') {
                control.frames.push({ t: Date.now(), frame });
                if (control.frames.length > 500) control.frames.shift();
              }
            } catch {
              // Non-JSON frames pass through unchanged.
            }
          }
          return super.send(data);
        }
      }
      for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
        Object.defineProperty(SessionUxTestWebSocket, key, { value: NativeWebSocket[key] });
      }
      window.WebSocket = SessionUxTestWebSocket;
      window.__rpiSessionUxTest = control;
    });

    await page.goto(url, { waitUntil: 'domcontentloaded' });

    // SUX0.1 — boot: all seeded rows listed, the active session is the
    // resumed native rename seed.
    await waitFor(
      page,
      (ids) => {
        const rendered = [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId || '');
        return ids.every((id) => rendered.includes(id));
      },
      'SUX0.1: seeded native + foreign rows never appeared after boot',
      30000,
      [meta.activeId, meta.contextId, meta.truncateId, meta.foreignId],
      (ids) => [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId || '')
    );
    await waitFor(
      page,
      (expected) => {
        const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch');
        return (active?.dataset?.sessionId || '') === expected;
      },
      'SUX0.1: active session after boot never became the rename seed',
      20000,
      meta.activeId,
      () => (document.querySelector('.session-sidebar__row--active .session-sidebar__switch') || {}).dataset?.sessionId || ''
    );
    await page.screenshot({ path: `${evidence}/session-ux-boot.png`, fullPage: true });
    record('SUX0.1');

    // SUX0.2 — native saved row context menu: exact native idle item set,
    // Copy context first.
    await openRowMenu(page, meta.contextId, 'SUX0.2: native saved row');
    const nativeMenu = await menuLabels(page);
    if (nativeMenu.join('|') !== NATIVE_IDLE_MENU) {
      fail(`SUX0.2: native menu items diverged: ${JSON.stringify(nativeMenu)}`);
    }
    record('SUX0.2');
    await closeRowMenu(page, 'SUX0.2');

    // ---------------- Rename (Session panel) ----------------
    await openManageSession(page, waitFor, 'SUX1.1: open the Session panel');

    // SUX1.2 — type the multi-character name char-by-char with keyboard
    // events and assert the input value accumulates after every character.
    await page.click('#session-rename-input');
    const typed = [];
    for (const ch of RENAME_NAME) {
      await page.keyboard.type(ch, { delay: 20 });
      typed.push(ch);
      const expected = typed.join('');
      const value = await page.evaluate(() => {
        const input = document.getElementById('session-rename-input');
        return input ? input.value : null;
      });
      if (value !== expected) {
        fail(`SUX1.2: rename input did not accumulate '${expected}' (value '${value}')`);
      }
    }
    record('SUX1.2');

    // SUX1.3 — Enter immediately: exactly one set_session_name frame with the
    // exact typed name and no sessionPath (rename of the current session).
    await page.keyboard.press('Enter');
    await waitFor(
      page,
      ({ name }) => {
        const frames = ((window.__rpiSessionUxTest && window.__rpiSessionUxTest.frames) || [])
          .map((entry) => entry.frame);
        const renames = frames.filter((frame) => frame.type === 'set_session_name');
        return renames.length === 1
          && renames[0].name === name
          && !('sessionPath' in renames[0]);
      },
      'SUX1.3: exact set_session_name frame never sent',
      20000,
      { name: RENAME_NAME },
      () => ((window.__rpiSessionUxTest && window.__rpiSessionUxTest.frames) || [])
        .map((e) => e.frame)
        .map((f) => (f && typeof f.sessionPath === 'string' && f.sessionPath !== ''
          ? { ...f, sessionPath: f.sessionPath.split('/').pop() }
          : f))
    );
    record('SUX1.3');

    // SUX1.4 — the panel Name reflects the typed name; the header renders
    // no duplicate session label beside the model selector (permanent
    // absence: the element is gone, not merely empty).
    await waitFor(
      page,
      ({ name }) =>
        (document.querySelector('[data-testid="session-name-value"]')?.textContent || '').trim() === name
        && document.getElementById('session-name') === null
        && !(document.getElementById('model-select')?.closest('header')?.textContent || '').includes('session:'),
      'SUX1.4: panel Name never updated to the typed name or the header still renders a duplicate session label',
      20000,
      { name: RENAME_NAME },
      () => ({
        panel: document.querySelector('[data-testid="session-name-value"]')?.textContent || '',
        headerSessionName: document.getElementById('session-name') === null ? null : 'present',
      })
    );
    record('SUX1.4');
    await page.screenshot({ path: `${evidence}/session-ux-renamed.png`, fullPage: true });

    // Stash the exact rename frames for evidence BEFORE the reload below —
    // the passive recorder state is per-navigation and resets at reload.
    const renameEvidenceFrames = (await outgoingFrames(page)).filter((f) => f.type === 'set_session_name');

    // SUX1.5 — the sidebar row title follows the rename. The sidebar catalog
    // refreshes on mount, on an active-session change, and on an 8s poll; the
    // panel rename drives none of those, so the former blind 25s DOM wait
    // merely rode the 8s poll. Instead the wait re-requests the authoritative
    // session_list through the explicit existing boot refresh path: one page
    // reload forces a fresh session_list round-trip and the rows re-render
    // from its response. The row title can only come from that fresh
    // authoritative catalog (no client-side catalog cache), so a rename that
    // never persisted server-side cannot false-pass on a blind poll.
    await page.reload({ waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'rename reload: WS did not reconnect after the rename reload',
      20000
    );
    await waitFor(
      page,
      (ids) => {
        const rendered = [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId || '');
        return ids.every((id) => rendered.includes(id));
      },
      'rename reload: seeded rows never re-rendered after the rename reload',
      30000,
      [meta.activeId, meta.contextId, meta.truncateId, meta.foreignId],
      (ids) => [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId || '')
    );
    await waitFor(
      page,
      (expected) => {
        const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch');
        return (active?.dataset?.sessionId || '') === expected;
      },
      'rename reload: active session after the reload never became the rename seed again',
      20000,
      meta.activeId,
      () => (document.querySelector('.session-sidebar__row--active .session-sidebar__switch') || {}).dataset?.sessionId || ''
    );
    await waitFor(
      page,
      ({ sid, name }) =>
        [...document.querySelectorAll(`.session-sidebar__switch[data-session-id="${sid}"] .session-sidebar__name`)]
          .some((el) => (el.textContent || '').trim() === name),
      'SUX1.5: sidebar row title never updated to the typed name from the fresh authoritative catalog',
      20000,
      { sid: meta.activeId, name: RENAME_NAME },
      ({ sid }) => ({
        row: document.querySelector(`.session-sidebar__switch[data-session-id="${sid}"] .session-sidebar__name`)?.textContent || '',
      })
    );
    record('SUX1.5');

    // ---------------- Copy context (native saved row) ----------------
    const switchFramesBefore = (await outgoingFrames(page)).filter((f) => f.type === 'switch_session').length;
    await openRowMenu(page, meta.contextId, 'SUX2: native saved context row');
    await clickMenuItem(page, 'Copy context', 'SUX2.1: copy context click');

    // The unloaded saved row produces a bounded handoff brief.
    await waitFor(
      page,
      () => [...document.querySelectorAll('#toasts .toast')].some(
        (t) => !t.classList.contains('toast--error') && (t.textContent || '').trim() === 'Copied context brief'
      ),
      'SUX2.5: "Copied context brief" toast never appeared',
      20000,
      null,
      () => [...document.querySelectorAll('#toasts .toast')].map((t) => (t.textContent || '').trim())
    );
    record('SUX2.5');

    // SUX2.1 — exactly one session_copy_context frame carrying the captured
    // row path.
    const framesAfterCopy = await outgoingFrames(page);
    const copyFrames = framesAfterCopy.filter((f) => f.type === 'session_copy_context');
    if (copyFrames.length !== 1) {
      fail(`SUX2.1: expected exactly one session_copy_context frame, got ${copyFrames.length}`);
    }
    if (copyFrames[0].sessionPath !== meta.contextPath) {
      fail('SUX2.1: session_copy_context frame did not carry the captured row path');
    }
    record('SUX2.1');

    // SUX2.4 — copy never switches the active session.
    const switchFramesAfter = framesAfterCopy.filter((f) => f.type === 'switch_session').length;
    if (switchFramesAfter !== switchFramesBefore) {
      fail('SUX2.4: copy context sent a switch_session frame');
    }
    const activeAfterCopy = await activeSessionId(page);
    if (activeAfterCopy !== meta.activeId) {
      fail(`SUX2.4: copy context switched the active session to '${activeAfterCopy}'`);
    }
    record('SUX2.4');

    // The brief retains sanitized recent exchanges and explicit session metadata.
    const copied = await clipboardText(page);
    fs.writeFileSync(path.join(evidence, 'copy-context-clipboard.txt'), copied);
    const lines = copied.split(/\r?\n/);
    if (lines[0] !== '# Handoff brief' || lines[1] !== 'Session: Context Seed Beta' || lines[2] !== `Working directory: ${meta.cwd}`) {
      fail(`SUX2.2: clipboard header lines diverged: ${JSON.stringify(lines.slice(0, 3))}`);
    }
    if (Buffer.byteLength(copied, 'utf8') > 65536) {
      fail(`SUX2.2: clipboard exceeded the 65536-byte bound (${Buffer.byteLength(copied, 'utf8')})`);
    }
    if (lines.slice(3).some((l) => l.startsWith('Format:') || l.startsWith('Truncated:'))) {
      fail('SUX2.2: fixed header labels recur after the first three lines');
    }
    record('SUX2.2');

    const required = [
      '## Model',
      '## Recent exchanges',
      '[User]: session-ux-context user turn marker: inspect [PATH] then ship.',
      '[Assistant]: session-ux-context assistant turn marker: plan committed',
    ];
    for (const marker of required) {
      if (!copied.includes(marker)) fail(`SUX2.3: clipboard is missing ${JSON.stringify(marker)}`);
    }
    if (!copied.includes('api_key=[REDACTED]')) fail('SUX2.3: credential-shaped api_key value was not redacted');
    if (copied.includes(fakeSk) || copied.includes(fakeApiValue)) {
      fail('SUX2.3: raw credential-shaped fixture text leaked into the clipboard');
    }
    if (copied.split('\n').filter((line) => !line.startsWith('Working directory: ')).join('\n').includes(meta.cwd)) fail('SUX2.3: local path leaked outside the explicit working-directory metadata');
    if (!copied.includes('[PATH]')) fail('SUX2.3: absolute path in the conversation was not replaced with [PATH]');
    record('SUX2.3');
    await page.click('#toasts .toast');
    await page.screenshot({ path: `${evidence}/session-ux-copy-context.png`, fullPage: true });

    // ---------------- Truncation fixture ----------------
    await openRowMenu(page, meta.truncateId, 'SUX3: native truncation row');
    await clickMenuItem(page, 'Copy context', 'SUX3: copy context click');

    // A large saved transcript is copied as the same bounded brief.
    await waitFor(
      page,
      () => [...document.querySelectorAll('#toasts .toast')].some(
        (t) => !t.classList.contains('toast--error') && (t.textContent || '').trim() === 'Copied context brief'
      ),
      'SUX3.2: "Copied context brief" toast never appeared',
      20000,
      null,
      () => [...document.querySelectorAll('#toasts .toast')].map((t) => (t.textContent || '').trim())
    );
    await waitFor(page, async () => (await navigator.clipboard.readText()).includes('session-ux-truncate marker'), 'truncation copy did not replace clipboard');
    record('SUX3.2');

    const truncated = await clipboardText(page);
    fs.writeFileSync(path.join(evidence, 'copy-context-truncated.txt'), truncated);
    const tLines = truncated.split(/\r?\n/);
    if (tLines[0] !== '# Handoff brief' || !truncated.includes('## Recent exchanges')) {
      fail(`SUX3.1: truncated clipboard header lines diverged: ${JSON.stringify(tLines.slice(0, 3))}`);
    }
    if (!truncated.includes('…')) fail('SUX3.1: large exchange lacks the brief truncation marker');
    if (!truncated.includes('session-ux-truncate marker')) {
      fail('SUX3.1: truncated clipboard lost the conversation head');
    }
    if (Buffer.byteLength(truncated, 'utf8') > 65536) {
      fail(`SUX3.1: truncated clipboard exceeded the 65536-byte bound (${Buffer.byteLength(truncated, 'utf8')})`);
    }
    record('SUX3.1');
    await page.screenshot({ path: `${evidence}/session-ux-truncated.png`, fullPage: true });

    // ---------------- Foreign row must stay export-only ----------------
    await openRowMenu(page, meta.foreignId, 'SUX4.1: foreign row');
    const foreignMenu = await menuLabels(page);
    if (foreignMenu.join('|') !== 'Export HTML') {
      fail(`SUX4.1: foreign menu must be Export HTML only, got ${JSON.stringify(foreignMenu)}`);
    }
    record('SUX4.1');
    await closeRowMenu(page, 'SUX4.1');
    await page.screenshot({ path: `${evidence}/session-ux-foreign-menu.png`, fullPage: true });

    // SUX4.2 — zero page/console errors across the whole lane.
    if (browserErrors.length > 0) {
      fail(`unexpected browser errors: ${browserErrors.join(' | ')}`);
    }
    record('SUX4.2');

    // Evidence: executed matrix + the captured rename/copy frames. Absolute
    // sessionPath values are redacted to their basenames before the frames
    // land in the evidence dir (the live frames kept their exact paths for
    // the SUX2.1 assertion above; no local absolute path is printed to
    // stdout either).
    const copyEvidenceFrames = (await outgoingFrames(page)).filter((f) => f.type === 'session_copy_context');
    const interesting = [...renameEvidenceFrames, ...copyEvidenceFrames].map(redactFrameForEvidence);
    fs.writeFileSync(path.join(evidence, 'frames.json'), JSON.stringify(interesting, null, 2));

    const missing = DOCUMENTED_IDS.filter((id) => !executed.has(id));
    if (missing.length > 0) {
      fail(`lane evidence incomplete: ${missing.join(', ')} never executed`);
    }
    const coverageTmp = `${coveragePath}.tmp`;
    fs.writeFileSync(coverageTmp, JSON.stringify({ executed: [...executed] }, null, 2));
    fs.renameSync(coverageTmp, coveragePath);

    console.log(`web-session-ux: PASSED (${executed.size}/${DOCUMENTED_IDS.length} assertions, SUX0 boot + native menu, SUX1 per-char panel rename + exact frame + panel/header/sidebar update, SUX2 copy context frame/bounded sanitized clipboard/no switch/toast, SUX3 truncation fixture, SUX4 foreign export-only + zero errors) — evidence at ${coveragePath}`);
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-session-ux: playwright crashed: ${err && err.stack ? err.stack : err}`);
  process.exit(2);
});