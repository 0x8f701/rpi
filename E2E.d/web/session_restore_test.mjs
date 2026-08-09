// Web session restore + persistence E2E lane (playwright half of
// E2E.d/web/session_restore.sh).
//
// Proves the Web-only listener contract for transcript/session parity:
//   R0  a persisted fixture restores legitimate assistant/IRC content,
//       hides display:false internal customs, and bounds bash/tool output
//   R1  a Web prompt round-trips and the new session's sidebar row appears
//       (the prompt is written to the normal recorder)
//   R2  switching away/back to a LOADED session restores its prior
//       transcript from the authoritative backend snapshot
//   R3  closing a settled session then switching back to it RESUMES from
//       disk and restores the same transcript (backend-authoritative,
//       not a stale frontend cache)
//   R4  after the listener process is SIGTERM-restarted, the Web
//       auto-reconnects, re-binds to the authoritative primary, and
//       switching to the recorded session restores its history from disk
//
// Environment:
//   RPI_URL            http://127.0.0.1:<port>/web
//   RPI_TOKEN          token file content (rpi-auth.<token> subprotocol)
//   RPI_REPLY          expected instant reply marker (default "sessions-reply:")
//   RPI_CHROME         executable path of the system Chrome (optional)
//   RPI_EVIDENCE       evidence dir for screenshots
//   RPI_WORK           shared dir for the kill/restart handshake:
//                        test writes kill-server.marker, the lane respawns
//                        the listener and writes server-up.marker
//
// The lane FAILS (exit 2) on any assertion failure. No agent-browser
// fallback, no skip.

import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const replyMarker = process.env.RPI_REPLY || 'sessions-reply:';
const paritySession = process.env.RPI_PARITY_SESSION || 'web-transcript-parity';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const work = process.env.RPI_WORK || '.';

const KILL_MARKER = path.join(work, 'kill-server.marker');
const UP_MARKER = path.join(work, 'server-up.marker');

function fail(message) {
  console.error(`web-session-restore: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 30000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

async function waitForFile(file, label, timeoutMs = 60000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (fs.existsSync(file)) return;
    await new Promise((r) => setTimeout(r, 100));
  }
  fail(`${label} (marker never appeared: ${file})`);
}

async function activeTranscript(page) {
  return page.evaluate(() => document.getElementById('transcript')?.textContent || '');
}

async function rowIds(page) {
  return page.evaluate(() =>
    [...document.querySelectorAll('.session-sidebar__switch')]
      .map((r) => r.dataset.sessionId || '')
      .filter(Boolean)
  );
}

async function activeRowSid(page) {
  return page.evaluate(() => {
    const row = document.querySelector('.session-sidebar__row--active');
    if (!row) return null;
    const btn = row.querySelector('.session-sidebar__switch');
    return btn?.dataset.sessionId || null;
  });
}

async function rowLoaded(page, sid) {
  return page.evaluate((s) =>
    !!document.querySelector(`.session-sidebar__loaded[data-loaded-for="${s}"]`)
  , sid);
}

// Click a session row and wait for it to become the active session, so later
// assertions observe the post-switch state rather than the pre-click state.
async function clickRow(page, sid, label) {
  await waitFor(
    page,
    (s) => {
      const btn = document.querySelector(`.session-sidebar__switch[data-session-id="${s}"]`);
      return !!btn && !btn.disabled;
    },
    `${label}: row ${sid} not clickable`,
    30000,
    sid
  );
  await page.click(`.session-sidebar__switch[data-session-id="${sid}"]`);
  await waitFor(
    page,
    (s) => {
      const row = document.querySelector('.session-sidebar__row--active');
      const btn = row?.querySelector('.session-sidebar__switch');
      return btn?.dataset.sessionId === s;
    },
    `${label}: active row never became ${sid}`,
    30000,
    sid
  );
}

async function waitForNewRow(page, knownIds, label, timeoutMs = 30000) {
  await waitFor(
    page,
    (known) => {
      const ids = [...document.querySelectorAll('.session-sidebar__switch')]
        .map((r) => r.dataset.sessionId || '')
        .filter(Boolean);
      return ids.some((id) => !known.includes(id));
    },
    label,
    timeoutMs,
    knownIds
  );
  const ids = await rowIds(page);
  return ids.find((id) => !knownIds.includes(id));
}

async function promptAndWait(page, prompt, reply, label, timeoutMs = 30000) {
  await page.fill('#prompt-input', prompt);
  await page.press('#prompt-input', 'Enter');
  await waitFor(page, (r) => document.body.textContent.includes(r), label, timeoutMs, reply);
}

async function closeRow(page, sid, label) {
  await waitFor(
    page,
    (s) => {
      const btn = document.querySelector(`#session-row-close-btn-${s}`);
      return !!btn && !btn.disabled;
    },
    `${label}: close button for ${sid} not available`,
    30000,
    sid
  );
  await page.click(`#session-row-close-btn-${sid}`);
}

async function assertTranscriptHas(page, needle, label) {
  const text = await activeTranscript(page);
  if (!text.includes(needle)) {
    fail(`${label}: transcript missing "${needle}" — got: ${text.slice(0, 400)}`);
  }
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
    page.on('pageerror', (err) => console.error(`web-session-restore: page error: ${err.message}`));

    /* ---- boot + connect ---- */
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await page.fill('#token-input', token);
    await page.click('#connect-btn');
    await waitFor(page, () => document.getElementById('conn-state').dataset.state === 'on', 'WS did not connect');
    await waitFor(
      page,
      () =>
        [...document.querySelectorAll('.session-sidebar__switch')].every((r) => (r.dataset.sessionId || '') !== '') &&
        document.querySelectorAll('.session-sidebar__switch').length >= 2,
      'primary and persisted parity session rows never appeared'
    );
    const sessionA = await activeRowSid(page);
    if (!sessionA) fail('could not read the active primary session id');
    const knownIds = await rowIds(page);
    if (!knownIds.includes(paritySession)) fail(`persisted parity session row missing: ${paritySession}`);
    await page.screenshot({ path: `${evidence}/restore-t0-boot.png`, fullPage: true });

    /* ---- R0: restored transcript visibility and output bounding ---- */
    await clickRow(page, paritySession, 'R0: switch to persisted transcript parity fixture');
    const parityText = await activeTranscript(page);
    for (const needle of [
      'system-reminder wording is legitimate assistant text',
      'IRC · Main → Child',
      'clean IRC parity body',
      '… 20 more lines',
      'bash-parity-30',
      '… 6 more lines',
      'tool-parity-12',
      '… 2 more lines',
      'orphan-parity-8',
    ]) {
      if (!parityText.includes(needle)) fail(`R0: restored transcript missing "${needle}" — got: ${parityText.slice(0, 800)}`);
    }
    for (const forbidden of ['internal parity secret', '<orchestration-message', 'bash-parity-1\n', 'tool-parity-1\n', 'orphan-parity-1\n']) {
      if (parityText.includes(forbidden)) fail(`R0: restored transcript leaked "${forbidden}"`);
    }
    const bashCard = page.locator('.tool-card[data-tool-id="parity-bash-card"]');
    if (await bashCard.count() !== 1) fail('R0: restored bash tool card missing or duplicated');
    if (await bashCard.locator('.tool-card__name').textContent() !== 'bash') fail('R0: restored bash tool name mismatch');
    if (!(await bashCard.locator('.tool-card__args').textContent() || '').includes('seq 1 15')) fail('R0: restored bash command missing from args');
    if (!(await bashCard.locator('.tool-card__state--done').count())) fail('R0: restored bash tool card is not done');
    const bashResult = await bashCard.locator('.tool-card__result').textContent() || '';
    if (!bashResult.startsWith('… 5 more lines\nbash-card-6') || !bashResult.endsWith('\nbash-card-15')) {
      fail(`R0: restored bash result was not bounded to its ten-line tail — got: ${bashResult}`);
    }

    const toolCard = page.locator('.tool-card[data-tool-id="parity-tool"]');
    if (await toolCard.count() !== 1) fail('R0: restored generic tool card missing or duplicated');
    if (await toolCard.locator('.tool-card__name').textContent() !== 'read') fail('R0: restored generic tool name mismatch');
    if (!(await toolCard.locator('.tool-card__args').textContent() || '').includes('parity.txt')) fail('R0: restored generic tool args missing');
    const toolClasses = await toolCard.getAttribute('class') || '';
    if (!(await toolCard.locator('.tool-card__state--error').count()) || !toolClasses.split(/\s+/).includes('tool-card--error')) {
      fail('R0: restored generic tool card did not preserve error state');
    }
    const toolResult = await toolCard.locator('.tool-card__result').textContent() || '';
    if (!toolResult.startsWith('… 6 more lines\ntool-parity-7') || !toolResult.endsWith('\ntool-parity-12')) {
      fail(`R0: restored generic result was not bounded to its six-line tail — got: ${toolResult}`);
    }
    await page.screenshot({ path: `${evidence}/restore-r0-transcript-parity.png`, fullPage: true });
    await clickRow(page, sessionA, 'R0: return to primary');
    console.log('web-session-restore: R0 PASS (metadata visibility + bounded restored output)');

    /* ---- R1: prompt writes the recorder; B created ---- */
    await page.click('#sidebar-new-session-btn');
    const sessionB = await waitForNewRow(page, knownIds, 'R1: B row never appeared');
    knownIds.push(sessionB);
    await waitFor(
      page,
      (s) => {
        const row = document.querySelector('.session-sidebar__row--active');
        return row?.querySelector('.session-sidebar__switch')?.dataset.sessionId === s;
      },
      'R1: B never became the active session after New',
      20000,
      sessionB
    );
    const bPrompt = 'restore-b-1';
    const bReply = `${replyMarker} ${bPrompt}`;
    await promptAndWait(page, bPrompt, bReply, 'R1: B prompt never round-tripped');
    await page.screenshot({ path: `${evidence}/restore-r1-b.png`, fullPage: true });

    /* ---- R2: switch away (A) and back (B, LOADED) restores B transcript ---- */
    await clickRow(page, sessionA, 'R2: switch to A');
    // A is the fresh primary (empty): confirm B's text left the active view.
    await waitFor(page, (r) => !document.body.textContent.includes(r), 'R2: B reply leaked into A view', 10000, bReply);
    await clickRow(page, sessionB, 'R2: switch back to B (loaded)');
    await assertTranscriptHas(page, bReply, 'R2: B transcript not restored from backend (loaded)');
    await page.screenshot({ path: `${evidence}/restore-r2-loaded.png`, fullPage: true });
    console.log('web-session-restore: R1+R2 PASS (prompt recorded + loaded switch restore)');

    /* ---- R3: close B (idle) then switch back to it (resume from disk) ---- */
    await clickRow(page, sessionA, 'R3: switch to A before closing B');
    if (!(await rowLoaded(page, sessionB))) fail('R3: B lost its loaded marker before close');
    await closeRow(page, sessionB, 'R3: close idle B');
    await waitFor(
      page,
      (s) => !document.querySelector(`.session-sidebar__loaded[data-loaded-for="${s}"]`),
      'R3: B loaded marker never dropped after close',
      30000,
      sessionB
    );
    await page.screenshot({ path: `${evidence}/restore-r3-closed.png`, fullPage: true });
    // B is no longer loaded: switching back to it resumes from disk.
    await clickRow(page, sessionB, 'R3: switch back to B (resume from disk)');
    await assertTranscriptHas(page, bReply, 'R3: B transcript not restored from disk after close/resume');
    if (!(await rowLoaded(page, sessionB))) fail('R3: B not marked loaded after disk resume');
    await page.screenshot({ path: `${evidence}/restore-r3-resumed.png`, fullPage: true });
    console.log('web-session-restore: R3 PASS (disk-resume restore)');

    /* ---- R4: restart the listener, reopen, switch to B, history present ---- */
    let sawReconnecting = false;
    const pillWatch = setInterval(async () => {
      try {
        const state = await page.evaluate(() => document.getElementById('conn-state')?.dataset.state || '');
        if (state === 'reconnecting') sawReconnecting = true;
      } catch {
        /* page mid-close */
      }
    }, 100);
    fs.writeFileSync(KILL_MARKER, 'kill now\n');
    await waitForFile(UP_MARKER, 'R4: lane never respawned the listener');
    clearInterval(pillWatch);
    if (!sawReconnecting) fail('R4: conn-state never entered "reconnecting" after the restart kill');
    await waitFor(page, () => document.getElementById('conn-state').dataset.state === 'on', 'R4: never auto-reconnected after restart');
    await page.screenshot({ path: `${evidence}/restore-r4-reconnected.png`, fullPage: true });
    // After restart the pre-restart primary is gone (it was never prompted, so
    // its lazy recorder never flushed a file); the Web re-binds to the fresh
    // primary on reconnect. The persisted session B is still in the catalog,
    // so switching to it resumes it from disk and restores its transcript.
    await clickRow(page, sessionB, 'R4: switch to B (resume from disk after restart)');
    await assertTranscriptHas(page, bReply, 'R4: B history not present from disk after restart');
    await page.screenshot({ path: `${evidence}/restore-r4-restarted.png`, fullPage: true });
    console.log('web-session-restore: R4 PASS (restart re-bind + disk restore)');

    console.log('web-session-restore: PASSED (R1 prompt recorded, R2 loaded restore, R3 disk-resume restore, R4 restart restore)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-session-restore: playwright crashed: ${err && err.stack ? err.stack : err}`);
  process.exit(2);
});