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
//   R5  after a page reload, the last-activated session — persisted under the
//       per-authority key `rpi-web-session:<encodeURIComponent(authority)>` —
//       is restored as the active row with its transcript (bootstrap reads
//       the preference, not just the first catalog row)
//   R6  a saved preference naming a nonexistent session falls back to the
//       FIRST catalog row, which becomes active with its transcript loaded
//
// Every R5.x/R6.x contract records a machine-readable ID; on full success the
// lane writes $RPI_EVIDENCE/coverage-assertions.json ({ "executed": [...] }),
// the same executed-assertion evidence convention as the sessions lane, so
// the assertions enter the existing coverage counting/reporting.
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

// Machine-readable executed-assertion evidence for the coverage suite: every
// passing R5.x/R6.x contract is recorded here (a Set, so repeated waits under
// one contract dedupe) and written to $RPI_EVIDENCE/coverage-assertions.json
// only after the FULL documented matrix below has executed, matching the
// sessions lane's evidence convention.
const DOCUMENTED_IDS = ['R5.1', 'R5.2', 'R5.3', 'R6.1', 'R6.2', 'R6.3'];
const executed = new Set();
function record(id) {
  executed.add(id);
  console.log(`[web-session-restore:assert] ${id}`);
}

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
      const btn = document.querySelector(`.session-sidebar__close[data-session-id="${s}"]`);
      return !!btn && !btn.disabled;
    },
    `${label}: close button for ${sid} not available`,
    30000,
    sid
  );
  await page.click(`.session-sidebar__close[data-session-id="${sid}"]`);
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
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    page.on('pageerror', (err) => console.error(`web-session-restore: page error: ${err.message}`));

    /* ---- boot + connect ---- */
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
        await waitFor(page, () => document.getElementById('conn-state').dataset.state === 'on', 'WS did not connect');
    await waitFor(
      page,
      () =>
        [...document.querySelectorAll('.session-sidebar__switch')].every((r) => (r.dataset.sessionId || '') !== '') &&
        document.querySelectorAll('.session-sidebar__switch').length >= 2,
      'primary and persisted parity session rows never appeared'
    );
    const activeAtBoot = await activeRowSid(page);
    if (!activeAtBoot) fail('could not read the active session id at boot');
    const knownIds = await rowIds(page);
    if (!knownIds.includes(paritySession)) fail(`persisted parity session row missing: ${paritySession}`);
    // A fresh page now intentionally selects the catalog's first row when no
    // preference exists, which may already be the parity fixture. Keep the
    // other fixture as A so the legacy R0/R2 switch-away assertions remain
    // meaningful without assuming which row bootstrap activates.
    const sessionA = knownIds.find((id) => id !== paritySession);
    if (!sessionA) fail('could not identify the non-parity primary session');
    await page.screenshot({ path: `${evidence}/restore-t0-boot.png`, fullPage: true });

    /* ---- R0: restored transcript visibility and output bounding ---- */
    if (activeAtBoot !== paritySession) {
      await clickRow(page, paritySession, 'R0: switch to persisted transcript parity fixture');
    }
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

    /* ---- R5: the last-activated session restores after a page reload ---- */
    // Catalog order after R2: B (newest, prompted in R1), the persisted
    // parity fixture, then the never-prompted primary (live overlay row). The
    // NON-FIRST row with real content is the parity fixture, so switching to
    // it and reloading must re-activate the SAME session id with its
    // transcript — proving bootstrap read the per-authority preference
    // instead of just keeping the first catalog row. The active-row wait
    // observes the post-bootstrap state (conn-state reaches "on" only after
    // the preference restore + switch_session chain settles).
    const catalogIds = await rowIds(page);
    if (catalogIds.length < 2) fail('R5: fewer than two fixture sessions in the catalog');
    if (catalogIds[1] !== paritySession) {
      fail(`R5: expected the non-first catalog row to be ${paritySession}, got ${catalogIds.join(', ')}`);
    }
    await clickRow(page, paritySession, 'R5: switch to the non-first session');
    if ((await activeRowSid(page)) !== paritySession) fail('R5: active session id mismatch after switching to the non-first row');
    record('R5.1');
    await page.screenshot({ path: `${evidence}/restore-r5-switched.png`, fullPage: true });
    await page.reload({ waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'R5: page title missing after reload');
    await waitFor(page, () => document.getElementById('conn-state').dataset.state === 'on', 'R5: WS did not connect after reload');
    await waitFor(
      page,
      (s) => {
        const row = document.querySelector('.session-sidebar__row--active');
        const btn = row?.querySelector('.session-sidebar__switch');
        return btn?.dataset.sessionId === s;
      },
      'R5: reload never restored the saved session as active',
      30000,
      paritySession
    );
    record('R5.2');
    // The restored view must be the parity session's backend snapshot (the
    // bootstrap switch_session result was consumed), not an empty transcript.
    await waitFor(
      page,
      (n) => document.getElementById('transcript')?.textContent.includes(n),
      'R5: reloaded transcript missing the parity seed',
      30000,
      'transcript parity seed'
    );
    if (!(await activeTranscript(page)).includes('system-reminder wording is legitimate assistant text')) {
      fail('R5: reloaded parity transcript not restored from the backend snapshot');
    }
    record('R5.3');
    await page.screenshot({ path: `${evidence}/restore-r5-restored.png`, fullPage: true });
    console.log('web-session-restore: R5 PASS (per-authority preference restores the last session after reload)');

    /* ---- R6: a missing saved preference falls back to the first catalog row ---- */
    // Persist a NONEXISTENT session id under the CURRENT page authority's
    // scoped key (constructed from location.host — never a hardcoded port).
    // Bootstrap must ignore the stale id and activate the first catalog row
    // instead, loading that session's transcript from the switch snapshot.
    const prefKey = await page.evaluate(() => 'rpi-web-session:' + encodeURIComponent(window.location.host));
    await page.evaluate((key) => window.localStorage.setItem(key, `no-such-session-${Date.now()}`), prefKey);
    await page.reload({ waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'R6: page title missing after reload');
    await waitFor(page, () => document.getElementById('conn-state').dataset.state === 'on', 'R6: WS did not connect after reload');
    const firstId = (await rowIds(page))[0];
    if (!firstId) fail('R6: could not read the first catalog row after reload');
    await waitFor(
      page,
      (s) => {
        const row = document.querySelector('.session-sidebar__row--active');
        const btn = row?.querySelector('.session-sidebar__switch');
        return btn?.dataset.sessionId === s;
      },
      'R6: the first catalog row never became active (no fallback for a missing preference)',
      30000,
      firstId
    );
    record('R6.1');
    // The first row is B: its round-trip reply must load from the snapshot.
    await waitFor(
      page,
      (r) => document.getElementById('transcript')?.textContent.includes(r),
      'R6: fallback session transcript not loaded from the backend snapshot',
      30000,
      bReply
    );
    record('R6.2');
    // The fallback activation persists the preference under the SAME
    // page-authority key — proving the product's key convention matches the
    // page authority (a hardcoded or differently-derived authority key would
    // leave the stale nonexistent value unread and untouched).
    const persisted = await page.evaluate((key) => window.localStorage.getItem(key), prefKey);
    if (persisted !== firstId) fail(`R6: preference persisted under a different authority key (got ${persisted}, expected ${firstId})`);
    record('R6.3');
    await page.screenshot({ path: `${evidence}/restore-r6-fallback.png`, fullPage: true });
    console.log('web-session-restore: R6 PASS (missing preference falls back to the first catalog row)');

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
    // The preference survives listener restart. Bootstrap now restores B
    // automatically when it remains in the catalog; older/rebind-only paths
    // may still land on another primary, in which case switch explicitly.
    if ((await activeRowSid(page)) !== sessionB) {
      await clickRow(page, sessionB, 'R4: switch to B (resume from disk after restart)');
    }
    await assertTranscriptHas(page, bReply, 'R4: B history not present from disk after restart');
    await page.screenshot({ path: `${evidence}/restore-r4-restarted.png`, fullPage: true });
    console.log('web-session-restore: R4 PASS (restart re-bind + disk restore)');

    // The lane may only report PASS (and may only write evidence) once the
    // FULL documented matrix executed: a renamed/renumbered contract that no
    // longer records its ID fails the lane instead of silently shrinking the
    // evidence the coverage suite quantifies.
    const missing = DOCUMENTED_IDS.filter((id) => !executed.has(id));
    if (missing.length > 0) {
      fail(`session-preference evidence incomplete: ${missing.join(', ')} never executed (coverage contract)`);
    }
    fs.mkdirSync(evidence, { recursive: true });
    fs.writeFileSync(path.join(evidence, 'coverage-assertions.json'), JSON.stringify({ executed: [...executed] }, null, 2));

    console.log(`web-session-restore: PASSED (${executed.size}/${DOCUMENTED_IDS.length} assertions, R0 restored parity, R1 prompt recorded, R2 loaded restore, R3 disk-resume restore, R4 restart restore, R5 per-authority preference reload, R6 missing-preference fallback) — evidence at ${path.join(evidence, 'coverage-assertions.json')}`);
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-session-restore: playwright crashed: ${err && err.stack ? err.stack : err}`);
  process.exit(2);
});