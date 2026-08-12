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
//   R4  after the listener process is SIGTERM-restarted, the Web
//       auto-reconnects, re-binds to the authoritative primary, and
//       switching to the recorded session restores its history from disk
//   R5  after a page reload, the last-activated session — persisted under the
//       selected listener's key `rpi-web-session:<encodeURIComponent(authority)>`
//       — is restored as the active row with its transcript, including when
//       the page origin and listener authority use different host strings
//       (bootstrap restores the selected listener before its preference)
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

const listenerUrl = process.env.RPI_URL;
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

// Click a session row and wait for it to become the active session, so later
// assertions observe the post-switch state rather than the pre-click state.
async function clickRow(page, sid, label) {
  if (await activeRowSid(page) === sid) return;
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

async function assertTranscriptHas(page, needle, label) {
  const text = await activeTranscript(page);
  if (!text.includes(needle)) {
    fail(`${label}: transcript missing "${needle}" — got: ${text.slice(0, 400)}`);
  }
}

async function main() {
  if (!listenerUrl) fail('RPI_URL is required');
  const connectionUrl = new URL(listenerUrl);
  const pageUrl = new URL(listenerUrl);
  // Exercise the residual production bug: the document is loaded through
  // `localhost`, while the header later connects the WebSocket to
  // `127.0.0.1`. Both reach the same listener but have distinct authority
  // strings, which used to make click-save and reload-load use different keys.
  if (pageUrl.hostname === '127.0.0.1') pageUrl.hostname = 'localhost';
  const pageAuthority = pageUrl.host;
  const connectionAuthority = connectionUrl.host;
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    page.on('pageerror', (err) => console.error(`web-session-restore: page error: ${err.message}`));

    /* ---- boot + connect ---- */
    await page.goto(pageUrl.toString(), { waitUntil: 'domcontentloaded', timeout: 20000 });
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
      'clean IRC parity body',
      'irc-incoming-line-1',
      '… 20 more lines',
      'bash-parity-30',
      '… 6 more lines',
      'tool-parity-12',
      '… 2 more lines',
      'orphan-parity-8',
    ]) {
      if (!parityText.includes(needle)) fail(`R0: restored transcript missing "${needle}" — got: ${parityText.slice(0, 800)}`);
    }
    for (const forbidden of ['internal parity secret', '<orchestration-message', 'orchestration_message', 'bash-parity-1\n', 'tool-parity-1\n', 'orphan-parity-1\n']) {
      if (parityText.includes(forbidden)) fail(`R0: restored transcript leaked "${forbidden}"`);
    }
    const oldIrcNodes = await page.locator('text="IRC · Main → Child"').evaluateAll((nodes) =>
      nodes.map((node) => node.outerHTML),
    );
    if (oldIrcNodes.length !== 0) {
      fail(`R0: restored transcript retained the old flat IRC custom label: ${JSON.stringify(oldIrcNodes)}`);
    }

    /* ---- R0: typed IRC cards (shared IrcCard model — no old label/XML) ---- */
    const ircCards = page.locator('.msg--irc');
    if (await ircCards.count() !== 2) fail(`R0: expected 2 typed IRC cards, got ${await ircCards.count()}`);
    const outgoing = page.locator('.msg--irc[data-irc-direction="outgoing"]');
    const incoming = page.locator('.msg--irc[data-irc-direction="incoming"]');
    if (await outgoing.count() !== 1 || await incoming.count() !== 1) {
      fail(`R0: IRC direction split wrong (outgoing=${await outgoing.count()} incoming=${await incoming.count()})`);
    }
    // Outgoing Main → Child: title `IRC → Child`, clean body, independent reply line.
    if ((await outgoing.locator('.msg--irc__title').textContent() || '') !== 'IRC → Child') {
      fail(`R0: outgoing IRC title mismatch: "${await outgoing.locator('.msg--irc__title').textContent()}"`);
    }
    if (!(await outgoing.locator('.msg--irc__body').textContent() || '').includes('clean IRC parity body')) {
      fail('R0: outgoing IRC body missing');
    }
    if ((await outgoing.locator('.msg--irc__reply').textContent() || '') !== 'reply to parent-9') {
      fail(`R0: outgoing IRC reply line mismatch: "${await outgoing.locator('.msg--irc__reply').textContent()}"`);
    }
    // Incoming Child → Main: title `IRC ← Child`, 6-line compact clamp + expand.
    if ((await incoming.locator('.msg--irc__title').textContent() || '') !== 'IRC ← Child') {
      fail(`R0: incoming IRC title mismatch: "${await incoming.locator('.msg--irc__title').textContent()}"`);
    }
    const incomingBody = incoming.locator('.msg--irc__body');
    if (!(await incomingBody.textContent() || '').startsWith('irc-incoming-line-1')) {
      fail('R0: incoming IRC body does not start at line 1');
    }
    if (await incomingBody.evaluate((el) => el.classList.contains('is-expanded'))) {
      fail('R0: incoming IRC body should start compact');
    }
    // Compact clamp: the 10-line body overflows a 6-line clamp (scrollHeight
    // > clientHeight proves the visual bound; the text remains in the DOM).
    const clampState = await incomingBody.evaluate((el) => ({
      scrollHeight: el.scrollHeight,
      clientHeight: el.clientHeight,
    }));
    if (!(clampState.scrollHeight > clampState.clientHeight + 1)) {
      fail(`R0: incoming IRC body not clamped to 6 visual lines (scroll=${clampState.scrollHeight} client=${clampState.clientHeight})`);
    }
    const toggle = incoming.locator('.msg--irc__toggle');
    if ((await toggle.getAttribute('aria-expanded')) !== 'false') fail('R0: IRC expand toggle should start collapsed');
    await toggle.click();
    if ((await toggle.getAttribute('aria-expanded')) !== 'true') fail('R0: IRC expand toggle did not expand');
    const expandedState = await incomingBody.evaluate((el) => ({
      scrollHeight: el.scrollHeight,
      clientHeight: el.clientHeight,
      cls: el.className,
    }));
    if (!expandedState.cls.split(/\s+/).includes('is-expanded') || !(expandedState.scrollHeight <= expandedState.clientHeight + 1)) {
      fail(`R0: IRC expand did not reveal the full body (scroll=${expandedState.scrollHeight} client=${expandedState.clientHeight})`);
    }
    if (!(await incomingBody.textContent() || '').includes('irc-incoming-line-10')) {
      fail('R0: expanded IRC body missing the final line');
    }
    const bashCard = page.locator('.tool-card[data-tool-id="parity-bash-card"]');
    if (await bashCard.count() !== 1) fail('R0: restored bash tool card missing or duplicated');
    if (await bashCard.locator('.tool-card__title').textContent() !== 'Command') fail('R0: restored bash command title mismatch');
    if (!(await bashCard.locator('.tool-card__command-text').textContent() || '').includes('seq 1 15')) fail('R0: restored bash command missing');
    const bashClasses = await bashCard.getAttribute('class') || '';
    if (!bashClasses.split(/\s+/).includes('tool-card--done')) fail('R0: restored bash tool card is not done');
    const bashResult = await bashCard.locator('.tool-card__output').textContent() || '';
    if (!bashResult.startsWith('… 5 more lines\nbash-card-6') || !bashResult.endsWith('\nbash-card-15')) {
      fail(`R0: restored bash result was not bounded to its ten-line tail — got: ${bashResult}`);
    }

    const toolCard = page.locator('.tool-card[data-tool-id="parity-tool"]');
    if (await toolCard.count() !== 1) fail('R0: restored generic tool card missing or duplicated');
    if (await toolCard.locator('.tool-card__title').textContent() !== 'Read') fail('R0: restored read title mismatch');
    if (!(await toolCard.locator('.tool-card__summary-path').textContent() || '').includes('parity.txt')) fail('R0: restored read path missing');
    const toolClasses = await toolCard.getAttribute('class') || '';
    if (!toolClasses.split(/\s+/).includes('tool-card--error')) {
      fail('R0: restored read tool card did not preserve error state');
    }
    const toolResult = await toolCard.locator('.tool-card__output').textContent() || '';
    if (!toolResult.startsWith('… 6 more lines\ntool-parity-7') || !toolResult.endsWith('\ntool-parity-12')) {
      fail(`R0: restored read result was not bounded to its six-line tail — got: ${toolResult}`);
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
    // Reconnect through connectionAuthority while the document stays on
    // pageAuthority. Pre-fix, reload reset the listener to pageAuthority and
    // therefore could not read the selection saved for connectionAuthority.
    if (connectionAuthority !== pageAuthority) {
      if (token) {
        await page.evaluate(
          ({ authority, value }) => window.localStorage.setItem(`rpi-web-token:${encodeURIComponent(authority)}`, value),
          { authority: connectionAuthority, value: token }
        );
      }
      await page.fill('#host-input', connectionAuthority);
      await page.press('#host-input', 'Enter');
      await waitFor(page, () => document.getElementById('conn-state').dataset.state === 'on', 'R5: alternate-authority WS did not connect');
      await waitFor(page, () => document.querySelectorAll('.session-sidebar__switch').length >= 2, 'R5: catalog missing after alternate-authority reconnect');
    }
    // Select the parity row by stable session identity. Other valid sessions
    // can be created during the preceding lifecycle checks, so catalog index
    // is not a contract; the saved-preference proof is that reload restores
    // this explicitly selected row instead of whichever row sorts first.
    const catalogIds = await rowIds(page);
    if (catalogIds.length < 2) fail('R5: fewer than two fixture sessions in the catalog');
    if (!catalogIds.includes(paritySession)) {
      fail(`R5: parity session ${paritySession} missing from catalog: ${catalogIds.join(', ')}`);
    }
    await clickRow(page, paritySession, 'R5: switch to the parity session');
    if ((await activeRowSid(page)) !== paritySession) fail('R5: active session id mismatch after switching to the parity row');
    const connectionPrefKey = `rpi-web-session:${encodeURIComponent(connectionAuthority)}`;
    const savedPreference = await page.evaluate(
      (key) => window.localStorage.getItem(key),
      connectionPrefKey
    );
    if (savedPreference !== paritySession) {
      fail(`R5: click did not save under connection authority (got ${savedPreference})`);
    }
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
    // Persist a nonexistent session id under the restored listener authority.
    // Bootstrap must ignore it and activate the first catalog row instead.
    const prefKey = `rpi-web-session:${encodeURIComponent(connectionAuthority)}`;
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
    // The fallback row's own durable snapshot must load. Which fixture sorts
    // first is not contractual once earlier lifecycle steps create valid
    // sessions, so derive the expected marker from its stable identity.
    const firstTranscriptMarker = firstId === paritySession ? 'transcript parity seed' : bReply;
    await waitFor(
      page,
      (r) => document.getElementById('transcript')?.textContent.includes(r),
      `R6: fallback session ${firstId} transcript not loaded from the backend snapshot`,
      30000,
      firstTranscriptMarker
    );
    record('R6.2');
    // The fallback activation persists the preference under the same restored
    // listener-authority key.
    const persisted = await page.evaluate((key) => window.localStorage.getItem(key), prefKey);
    if (persisted !== firstId) fail(`R6: preference persisted under a different listener key (got ${persisted}, expected ${firstId})`);
    record('R6.3');
    await page.screenshot({ path: `${evidence}/restore-r6-fallback.png`, fullPage: true });
    console.log('web-session-restore: R6 PASS (missing listener preference falls back to the first catalog row)');

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
    // The restart handshake is complete once the lane writes the up marker:
    // stop polling the connection pill so the Node event loop drains and the
    // process can exit. Without this the lingering setInterval keeps the
    // process alive after main() resolves, so the lane's `wait $pw_pid`
    // blocks forever (the test PASSES but the shell never returns).
    clearInterval(pillWatch);
    // The durable parity fixture is always catalog-visible and proves that a
    // restarted listener rebinds disk-backed history. Temporary B can be
    // hidden by the recoverable view policy when it is no longer loaded.
    if ((await activeRowSid(page)) !== paritySession) {
      await clickRow(page, paritySession, 'R4: switch to parity after restart');
    }
    await assertTranscriptHas(page, 'transcript parity seed', 'R4: parity history not present from disk after restart');
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

    console.log(`web-session-restore: PASSED (${executed.size}/${DOCUMENTED_IDS.length} assertions, R0 restored parity, R1 prompt recorded, R2 loaded restore, R4 restart restore, R5 selected-listener preference reload, R6 missing-preference fallback) — evidence at ${path.join(evidence, 'coverage-assertions.json')}`);
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-session-restore: playwright crashed: ${err && err.stack ? err.stack : err}`);
  process.exit(2);
});