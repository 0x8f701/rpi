// Web session restore + persistence E2E lane (playwright half of
// E2E.d/web/session_restore.sh).
//
// Proves the Web-only listener contract for transcript/session parity:
//   R0  a persisted fixture restores legitimate assistant/IRC content,
//       paints stored [text, thinking] thinking-first, hides display:false
//       internal customs, and bounds bash/tool output
//   R1  a Web prompt round-trips and the new session's sidebar row appears
//       (the prompt is written to the normal recorder)
//   R2  switching away/back to a LOADED session restores its prior
//       transcript from the authoritative backend snapshot
//   R4  after the listener process is restarted through its SIGTERM restart authority, the Web
//       auto-reconnects, re-binds to the authoritative primary, and
//       switching to the recorded session restores its history from disk
//   R5  after a page reload, the fresh load boots the SERVING page authority
//       (never a persisted host value) and restores the PAGE-authority saved
//       session `rpi-web-session:<encodeURIComponent(pageAuthority)>` with its
//       transcript, while the session chosen through a host-switch cutover
//       stays stored under the connection authority's own key (per-authority
//       preference isolation; no token or preference crosses authorities).
//   R6  a saved preference naming a nonexistent session falls back to the
//       FIRST catalog row, which becomes active with its transcript loaded
//
// Every R4.x/R5.x/R6.x contract records a machine-readable ID (record(id) at
// the assertion site — never implicitly). This driver STAGES ONLY: every
// artifact (screenshots, the FULL exact ledger coverage-assertions.json with
// { "documented": [...], "executed": [...Map insertion order...], "counts":
// {...} } — the same documented/executed/counts convention as every other
// coverage producer — and the playwright-assertions.json manifest) is written
// into the ONE run-scoped staging dir the shell handed over via RPI_STAGING,
// registered only after its write succeeded. This script NEVER renames the
// staging set, NEVER writes a run directory, and NEVER touches the current
// pointer: session_restore.sh validates the exact staged set/ledger and
// performs the single stage->run rename + pointer commit after the HTTP
// sanity and listener-close gates pass. A failed run removes ONLY its
// staging dir; published evidence and the prior pointer are unreachable
// from here.
//
// Environment:
//   RPI_URL            http://127.0.0.1:<port>/web
//   RPI_TOKEN          token file content (rpi-auth.<token> subprotocol)
//   RPI_REPLY          expected instant reply marker (default "sessions-reply:")
//   RPI_STAGING        the run-scoped staging dir handed over by the shell
//   RPI_LANE_NONCE     this invocation's nonce (must match the shell's)
//   RPI_WORK           shared dir for the kill/restart handshake:
//                        test writes kill-server.marker, the lane respawns
//                        the listener and writes server-up.marker
//
// The lane FAILS (exit 2) on any assertion failure. No agent-browser
// fallback, no skip.

import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';
import { assertSidebarNewActionAbsent, createFreshSessionViaManage } from './lib/session-lifecycle.mjs';

const listenerUrl = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const replyMarker = process.env.RPI_REPLY || 'sessions-reply:';
const paritySession = process.env.RPI_PARITY_SESSION || 'web-transcript-parity';
const baseSession = process.env.RPI_BASE_SESSION || 'web-transcript-a';
const chromePath = process.env.RPI_CHROME || '';
const stagingDir = process.env.RPI_STAGING || '';
const laneNonce = process.env.RPI_LANE_NONCE || '';
const work = process.env.RPI_WORK || '.';

const KILL_MARKER = path.join(work, 'kill-server.marker');
const UP_MARKER = path.join(work, 'server-up.marker');
// Documented in the lane's ACTUAL execution order (R5 -> R6 -> R4; within
// the restart phase every contract completes strictly in ID order
// R4.1..R4.5 — R4.3 records at its actual contract site, the disk-backed
// parity history restore), so the Map's insertion order can be compared
// elementwise against it.
const DOCUMENTED_IDS = [
  'R5.1', 'R5.2', 'R5.3',
  'R6.1', 'R6.2', 'R6.3',
  'R4.1', 'R4.2', 'R4.3', 'R4.4', 'R4.5',
];
// Explicit per-ID execution counts: record(id) is called ONLY at assertion
// sites, hard-fails on any id outside DOCUMENTED_IDS, and never allows a
// duplicate — every documented id must land at exactly 1.
const executedCounts = new Map();
function record(id) {
  if (!DOCUMENTED_IDS.includes(id)) {
    fail(`record: unexpected assertion id ${JSON.stringify(id)} (not in DOCUMENTED_IDS)`);
  }
  const count = (executedCounts.get(id) || 0) + 1;
  executedCounts.set(id, count);
  if (count !== 1) {
    fail(`record: assertion id ${id} recorded ${count}x — every documented id must execute exactly once`);
  }
  console.log(`[web-session-restore:assert] ${id}`);
}

function fail(message) {
  // Throwing (not exiting) keeps every failure on main's single failure
  // route, which closes the browser before exiting — no leaked Chromium.
  throw new Error(message);
}

// Run-scoped STAGING ONLY: every artifact is written into the ONE staging
// dir the shell claimed for this invocation and REGISTERED only after its
// write/screenshot succeeded.
const laneArtifacts = []; // staged artifact names, AFTER their write succeeded

function cleanupStaging() {
  fs.rmSync(stagingDir, { recursive: true, force: true });
}

async function laneShot(page, name, opts = {}) {
  fs.mkdirSync(stagingDir, { recursive: true });
  await page.screenshot({ path: path.join(stagingDir, name), ...opts });
  laneArtifacts.push(name);
  return path.join(stagingDir, name);
}

// Stage the FULL exact ledger — executed is the Map's actual insertion order
// and counts the real per-ID ledger, never a reconstruction of
// DOCUMENTED_IDS — after the close/error gates, plus the manifest carrying
// the exact registered artifact list the shell's commit gate validates.
function stageLedgerAndManifest() {
  const executedOrder = [...executedCounts.keys()];
  if (executedOrder.length !== DOCUMENTED_IDS.length
    || executedOrder.some((id, index) => id !== DOCUMENTED_IDS[index])) {
    fail(`session restore evidence order mismatch: executed ${JSON.stringify(executedOrder)} != documented ${JSON.stringify([...DOCUMENTED_IDS])}`);
  }
  fs.mkdirSync(stagingDir, { recursive: true });
  fs.writeFileSync(
    path.join(stagingDir, 'coverage-assertions.json'),
    JSON.stringify({
      documented: [...DOCUMENTED_IDS],
      executed: executedOrder,
      counts: Object.fromEntries(executedCounts),
    }, null, 2),
  );
  laneArtifacts.push('coverage-assertions.json');
  fs.writeFileSync(path.join(stagingDir, 'playwright-assertions.json'), JSON.stringify({
    lane: 'web-session-restore',
    nonce: laneNonce,
    artifacts: laneArtifacts,
  }, null, 2));
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
  return page.evaluate(() =>
    document.querySelector('#transcript .transcript-surface:not([hidden])')?.textContent || '');
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

// Failure diagnostics: dump the exact restore-relevant client state so a
// failed reload-restore assertion is attributable (which authority booted,
// what the catalog showed, which keys were saved, what the socket exchanged).
async function dumpRestoreDiagnostics(page, socketFrames, label) {
  const snapshot = await page.evaluate(() => ({
    pageAuthority: window.location.host,
    activeRow: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || null,
    rows: [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId || ''),
    sendDisabled: document.getElementById('send-btn')?.hasAttribute('disabled') === true,
    sendLabel: document.getElementById('send-btn')?.getAttribute('aria-label') || '',
    switchPending: document.getElementById('transcript')?.dataset.switchPending || '',
    overlay: document.querySelector('#reconnect-overlay .reconnect-overlay__text')?.textContent || '',
    toasts: [...document.querySelectorAll('.toast, [role="status"]')].map((node) => (node.textContent || '').slice(0, 160)),
    storage: Object.fromEntries(Object.keys(window.localStorage)
      .filter((k) => k.startsWith('rpi-web-'))
      .map((k) => [k, (window.localStorage.getItem(k) || '').slice(0, 80)])),
  })).catch((error) => ({ dumpError: String(error) }));
  console.error(`[diag:${label}]`, JSON.stringify(snapshot));
  const latestGen = socketFrames.sent.reduce((max, frame) => Math.max(max, frame.gen), 0);
  const summarize = (frame) => {
    try {
      const parsed = JSON.parse(frame.raw);
      return {
        gen: frame.gen,
        type: parsed.type,
        id: parsed.id,
        command: parsed.command,
        sessionId: parsed.sessionId,
        success: parsed.success,
      };
    } catch {
      return { gen: frame.gen, type: '?' };
    }
  };
  const sent = socketFrames.sent.filter((frame) => frame.gen === latestGen).map(summarize);
  const received = socketFrames.received.filter((frame) => frame.gen === latestGen).map(summarize);
  console.error(`[diag:${label}] gen ${latestGen} sent:`, JSON.stringify(sent));
  console.error(`[diag:${label}] gen ${latestGen} received:`, JSON.stringify(received));
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
  if (!stagingDir) fail('RPI_STAGING is required (the shell hands over the run-scoped staging dir)');
  if (!laneNonce) fail('RPI_LANE_NONCE is required (must match the shell invocation nonce)');
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
    // Explicit context so the R5 connection-authority probe can open a second
    // page in the SAME context (shared localStorage per origin) — a fresh
    // context would have empty storage and prove nothing.
    const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
    const page = await context.newPage();
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    const browserErrors = [];
    // Error monitors attached to the CONTEXT (not one page): the primary
    // page AND the R5 connection-authority probe page — every page this
    // run ever opens — feed the SAME browserErrors union, so an error on
    // the probe page can never escape the lane-final gate. The gate itself
    // evaluates only after every page/context/browser closed.
    context.on('pageerror', (err) => browserErrors.push(`page: ${err.message}`));
    context.on('console', (message) => {
      if (message.type() === 'error') browserErrors.push(`console: ${message.text()}`);
    });
    const socketFrames = { nextGen: 0, sent: [], received: [], closes: [], current: null };
    page.on('websocket', (socket) => {
      const gen = ++socketFrames.nextGen;
      socketFrames.current = socket;
      socket.on('framesent', (frame) => {
        if (typeof frame.payload === 'string') socketFrames.sent.push({ gen, raw: frame.payload });
      });
      socket.on('framereceived', (frame) => {
        if (typeof frame.payload === 'string') socketFrames.received.push({ gen, raw: frame.payload });
      });
      socket.on('close', () => socketFrames.closes.push({ gen, at: Date.now() }));
    });

    /* ---- boot + connect ---- */
    await page.goto(pageUrl.toString(), { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(page, () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true, 'WS did not connect');
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
    const sessionA = knownIds.find((id) => id === baseSession);
    if (!sessionA) fail(`could not identify the durable base session ${baseSession}`);
    await laneShot(page, 'restore-t0-boot.png', { fullPage: true });

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
    const thinkOrder = await page.evaluate(() => {
      const msgs = [...document.querySelectorAll('.msg--assistant')];
      const hit = msgs.find((node) => (node.textContent || '').includes('system-reminder wording is legitimate assistant text'));
      if (!hit) return { ok: false, reason: 'assistant missing' };
      if (!hit.querySelector('.thinking')) return { ok: false, reason: 'thinking fold missing' };
      const html = hit.innerHTML;
      const thinkAt = html.indexOf('THINKPROBE_EN restored chain of thought');
      const answerAt = html.indexOf('system-reminder wording is legitimate assistant text');
      return { ok: thinkAt >= 0 && answerAt >= 0 && thinkAt < answerAt, thinkAt, answerAt };
    });
    if (!thinkOrder.ok) fail(`R0: restored [text, thinking] did not paint thinking first: ${JSON.stringify(thinkOrder)}`);
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
    const bashCard = page.locator('.tool-card[data-tool-call-id="parity-bash-card"]');
    if (await bashCard.count() !== 1) fail(`R0: restored bash tool card missing or duplicated (${await bashCard.count()})`);
    if (await bashCard.locator('.tool-card__title').textContent() !== 'Command') fail('R0: restored bash command title mismatch');
    if (!(await bashCard.locator('.tool-card__command-text').textContent() || '').includes('seq 1 15')) fail('R0: restored bash command missing');
    const bashClasses = await bashCard.getAttribute('class') || '';
    if (!bashClasses.split(/\s+/).includes('tool-card--done')) fail('R0: restored bash tool card is not done');
    const bashResult = await bashCard.locator('.tool-card__output').textContent() || '';
    if (!bashResult.startsWith('… 5 more lines\nbash-card-6') || !bashResult.endsWith('\nbash-card-15')) {
      fail(`R0: restored bash result was not bounded to its ten-line tail — got: ${bashResult}`);
    }

    const toolCard = page.locator('.tool-card[data-tool-call-id="parity-tool"]');
    if (await toolCard.count() !== 1) fail(`R0: restored generic tool card missing or duplicated (${await toolCard.count()})`);
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
    await laneShot(page, 'restore-r0-transcript-parity.png', { fullPage: true });
    await clickRow(page, sessionA, 'R0: return to primary');
    console.log('web-session-restore: R0 PASS (metadata visibility + bounded restored output)');

    /* ---- R1: prompt writes the recorder; B created ---- */
    await assertSidebarNewActionAbsent(page, 'R1: obsolete sidebar New action rendered');
    const { sessionId: sessionB } = await createFreshSessionViaManage(page, waitFor, 'R1', {
      closePanel: true,
      timeoutMs: 30000,
    });
    await waitForNewRow(page, knownIds, 'R1: B row never appeared');
    knownIds.push(sessionB);
    const bPrompt = 'restore-b-1';
    const bReply = `${replyMarker} ${bPrompt}`;
    await promptAndWait(page, bPrompt, bReply, 'R1: B prompt never round-tripped');
    await laneShot(page, 'restore-r1-b.png', { fullPage: true });

    /* ---- R2: switch away (A) and back (B, LOADED) restores B transcript ---- */
    await clickRow(page, sessionA, 'R2: switch to A');
    // A is the fresh primary (empty): confirm B's text left the VISIBLE
    // transcript surface. Hidden parked surfaces and sidebar previews still
    // contain B's reply in document.body.
    await waitFor(page, (r) => !(document.querySelector('#transcript .transcript-surface:not([hidden])')?.textContent || '').includes(r), 'R2: B reply leaked into A view', 10000, bReply);
    await clickRow(page, sessionB, 'R2: switch back to B (loaded)');
    await assertTranscriptHas(page, bReply, 'R2: B transcript not restored from backend (loaded)');
    await laneShot(page, 'restore-r2-loaded.png', { fullPage: true });
    console.log('web-session-restore: R1+R2 PASS (prompt recorded + loaded switch restore)');


    /* ---- R5: the last-activated session restores after a page reload ---- */
    if (connectionAuthority !== pageAuthority) {
      if (token) {
        await page.evaluate(
          ({ authority, value }) => window.localStorage.setItem(`rpi-web-token:${encodeURIComponent(authority)}`, value),
          { authority: connectionAuthority, value: token }
        );
      }
      // Directly open the connection-authority listener URL in a fresh page.
      // The page's own authority contract connects the WebSocket to its
      // serving host (the header host-input was removed).
      await page.goto(listenerUrl, { waitUntil: 'domcontentloaded', timeout: 20000 });
      await waitFor(page, () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true, 'R5: alternate-authority WS did not connect');
      await waitFor(page, () => document.querySelectorAll('.session-sidebar__switch').length >= 2, 'R5: catalog missing after alternate-authority reconnect');
    }
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
    await laneShot(page, 'restore-r5-switched.png', { fullPage: true });
    // Fresh-load authority contract: navigate BACK to the SERVING page URL
    // (never a persisted host value — the header host-input is removed). The
    // page boots on the page authority and restores the PAGE-authority saved
    // session, while the connection-authority preference stays stored under
    // its own origin-scoped key (per-authority isolation; no token or
    // preference value crosses authorities on boot).
    await page.goto(pageUrl.toString(), { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'R5: page title missing after reload');
    const bootAuthority = await page.evaluate(() => window.location.host);
    if (bootAuthority !== pageAuthority) {
      fail(`R5: fresh load booted authority ${bootAuthority}, expected page authority ${pageAuthority}`);
    }
    await waitFor(page, () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true, 'R5: WS did not connect after reload');
    const pagePrefKey = `rpi-web-session:${encodeURIComponent(pageAuthority)}`;
    const pagePreference = await page.evaluate((key) => window.localStorage.getItem(key), pagePrefKey);
    if (!pagePreference) fail('R5: page authority lost its saved session preference before reload');
    try {
      await page.waitForFunction(
        (s) => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId === s,
        pagePreference,
        { timeout: 30000 },
      );
    } catch (error) {
      await dumpRestoreDiagnostics(page, socketFrames, 'R5');
      fail(`R5: reload never restored the page-authority saved session as active (timeout 30000ms): ${error?.message || error}`);
    }
    // The connection-authority preference lives in the 127-origin localStorage
    // (saved during the alternate-authority visit above) — verify it through
    // a SEPARATE page in the SAME browser context (a fresh context would have
    // empty storage) so the per-origin isolation is proven, not assumed.
    const connectionProbe = await context.newPage();
    try {
      await connectionProbe.goto(listenerUrl, { waitUntil: 'domcontentloaded', timeout: 20000 });
      const connectionPreferenceAfterReload = await connectionProbe.evaluate(
        (key) => window.localStorage.getItem(key),
        connectionPrefKey
      );
      if (connectionPreferenceAfterReload !== paritySession) {
        fail(`R5: reload clobbered the connection-authority preference (got ${connectionPreferenceAfterReload}, expected ${paritySession})`);
      }
    } finally {
      await connectionProbe.close().catch(() => {});
    }
    record('R5.2');
    await assertTranscriptHas(page, bReply, 'R5: reloaded page-authority session transcript missing from the backend snapshot');
    record('R5.3');
    await laneShot(page, 'restore-r5-restored.png', { fullPage: true });
    console.log('web-session-restore: R5 PASS (fresh load boots the serving page authority and restores its own saved session)');

    /* ---- R6: a missing saved preference falls back to the first catalog row ---- */
    const prefKey = `rpi-web-session:${encodeURIComponent(pageAuthority)}`;
    await page.evaluate((key) => window.localStorage.setItem(key, `no-such-session-${Date.now()}`), prefKey);
    await page.reload({ waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'R6: page title missing after reload');
    await waitFor(page, () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true, 'R6: WS did not connect after reload');
    // The catalog rows arrive via a session_list response AFTER the socket
    // turns on; read them only once the sidebar has rendered its first row.
    await waitFor(
      page,
      () => document.querySelector('.session-sidebar__switch') !== null,
      'R6: sidebar catalog never rendered after reload',
      30000,
    );
    const firstId = (await rowIds(page))[0];
    if (!firstId) fail('R6: could not read the first catalog row after reload');
    await waitFor(
      page,
      (s) => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId === s,
      'R6: the first catalog row never became active (no fallback for a missing preference)',
      30000,
      firstId
    );
    record('R6.1');
    const firstTranscriptMarker = firstId === paritySession
      ? 'transcript parity seed'
      : firstId === sessionA
        ? 'transcript A seed'
        : bReply;
    await waitFor(
      page,
      (r) => document.getElementById('transcript')?.textContent.includes(r),
      `R6: fallback session ${firstId} transcript not loaded from the backend snapshot`,
      30000,
      firstTranscriptMarker
    );
    record('R6.2');
    const persisted = await page.evaluate((key) => window.localStorage.getItem(key), prefKey);
    if (persisted !== firstId) fail(`R6: fallback preference persisted under a different listener key (got ${persisted}, expected ${firstId})`);
    record('R6.3');
    await laneShot(page, 'restore-r6-fallback.png', { fullPage: true });
    console.log('web-session-restore: R6 PASS (missing listener preference falls back to the first catalog row)');
    /* ---- R4: restart the listener, reopen, switch to parity, settle a prompt ---- */
    // Event-gated: register the current socket's close promise and the
    // successor-generation waitForEvent BEFORE the kill, so the restart is
    // observed from the transport lifecycle itself — no polling, no sleeps.
    const currentSocket = socketFrames.current;
    if (!currentSocket) fail('R4: no live WebSocket to watch for the restart');
    let currentSocketClosedResolve;
    const currentSocketClosed = new Promise((resolve) => { currentSocketClosedResolve = resolve; });
    currentSocket.once('close', () => currentSocketClosedResolve());
    const successorOpened = page.waitForEvent('websocket', { timeout: 30000 });
    fs.writeFileSync(KILL_MARKER, 'kill now\n');
    record('R4.1');
    await waitForFile(UP_MARKER, 'R4: lane never respawned the listener');
    try {
      await Promise.race([
        currentSocketClosed,
        new Promise((_, reject) => setTimeout(() => reject(new Error('killed generation never closed within 30s')), 30000)),
      ]);
      await successorOpened;
    } catch (error) {
      await dumpRestoreDiagnostics(page, socketFrames, 'R4');
      fail(`R4: restart transport cycle not observed: ${error?.message || error}`);
    }
    record('R4.2');
    try {
      await page.waitForFunction(
        () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
        { timeout: 30000 },
      );
    } catch (error) {
      await dumpRestoreDiagnostics(page, socketFrames, 'R4-connected');
      fail(`R4: WebSocket never returned to authoritative connected state after listener restart (timeout 30000ms): ${error?.message || error}`);
    }
    // Prove a real post-reconnect switch_session on the successor generation:
    // move to the base session first, then switch back to parity — an
    // A→parity round trip whose exact command id, response session owner, and
    // applied active row are all correlated below.
    await clickRow(page, sessionA, 'R4: post-restart switch away to the base session');
    const sentBeforeSwitch = socketFrames.sent.length;
    await page.click(`.session-sidebar__switch[data-session-id="${paritySession}"]`);
    try {
      await page.waitForFunction(
        (s) => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId === s,
        paritySession,
        { timeout: 30000 },
      );
    } catch (error) {
      const sent = socketFrames.sent.slice(sentBeforeSwitch).map(({ gen, raw }) => {
        try {
          const frame = JSON.parse(raw);
          return { gen, type: frame.type, id: frame.id, sessionId: frame.sessionId, sessionPath: frame.sessionPath };
        } catch {
          return { gen, invalid: true };
        }
      });
      const switchCommand = sent.find((frame) => frame.type === 'switch_session');
      const received = socketFrames.received.map(({ gen, raw }) => {
        try {
          const frame = JSON.parse(raw);
          return { gen, type: frame.type, id: frame.id, success: frame.success, sessionId: frame.sessionId, error: frame.error };
        } catch {
          return null;
        }
      }).filter((frame) => frame && switchCommand && frame.id === switchCommand.id);
      const diagnostic = await page.evaluate(() => ({
        sendDisabled: document.getElementById('send-btn')?.hasAttribute('disabled') === true,
        active: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || null,
        rows: [...document.querySelectorAll('.session-sidebar__switch')].map((row) => ({
          id: row.dataset.sessionId || '',
          disabled: row.disabled,
          text: row.textContent || '',
        })),
        toasts: [...document.querySelectorAll('.toast, .error-toast, [role="alert"]')].map((node) => node.textContent || ''),
      }));
      await dumpRestoreDiagnostics(page, socketFrames, 'R4-switch');
      fail(`R4: parity switch failed after restart: ${error?.message || error} ${JSON.stringify({ diagnostic, sent, matchingResponses: received })}`);
    }
    // Correlate the post-restart switch on the successor generation: the
    // exact switch_session command id, its response session owner, and the
    // applied active row must all name the parity session.
    const switchCommand = socketFrames.sent.slice(sentBeforeSwitch)
      .map((frame) => {
        try {
          const parsed = JSON.parse(frame.raw);
          return { gen: frame.gen, ...parsed };
        } catch {
          return null;
        }
      })
      .filter((frame) => frame && frame.type === 'switch_session')
      .pop();
    if (!switchCommand || (switchCommand.sessionPath && !switchCommand.sessionPath.includes(paritySession))) {
      fail(`R4: no switch_session command targeting parity on the successor generation: ${JSON.stringify(switchCommand || null)}`);
    }
    const switchResponse = [...socketFrames.received]
      .map((frame) => {
        try {
          return { gen: frame.gen, ...JSON.parse(frame.raw) };
        } catch {
          return null;
        }
      })
      .filter((frame) => frame && frame.id === switchCommand.id)
      .pop();
    // Wire contract (MultiSessionRuntimeManager): the response ENVELOPE
    // sessionId names the routing owner — the session the command was sent
    // from (the base session, since the click switch routes to the active
    // session) — while the snapshot payload `data.sessionId` names the
    // TARGET. The web client rejects any response whose envelope sessionId
    // differs from the routed-from session, so both fields are asserted.
    if (
      !switchResponse || switchResponse.success !== true
      || switchResponse.sessionId !== sessionA
      || switchResponse.data?.sessionId !== paritySession
    ) {
      fail(`R4: switch_session response owner mismatch: ${JSON.stringify({
        envelopeSessionId: switchResponse?.sessionId ?? null,
        dataSessionId: switchResponse?.data?.sessionId ?? null,
        success: switchResponse?.success ?? null,
      })}`);
    }
    if (switchCommand.gen !== switchResponse.gen) {
      fail(`R4: switch command/response crossed socket generations (sent gen ${switchCommand.gen}, response gen ${switchResponse.gen})`);
    }
    if ((await activeRowSid(page)) !== paritySession) {
      fail(`R4: active session ${await activeRowSid(page)} does not match the switch response owner ${paritySession}`);
    }
    await assertTranscriptHas(page, 'transcript parity seed', 'R4: parity history not present from disk after restart');
    // R4.3's contract site: the post-reconnect switch correlation above plus
    // the disk-backed parity history restore just proven. The record lands
    // exactly here — never deferred to phase completion — so the executed
    // order is the real contract order R4.1..R4.5.
    record('R4.3');
    // Cold-cache omission-settle: the parity fixture's recorded Task spawn is
    // still queued/running in the disk transcript, but the session has no
    // orchestration runtime, so the authoritative job_list after reconnect is
    // empty and the historical child MUST be settled (never a stale Running
    // card). DOM provenance keeps the exact backend jobId.
    await waitFor(
      page,
      () => {
        const row = document.querySelector('.tool-card__child[data-job-id="job-restore-parity"]');
        return !!row && row.dataset.status === 'cancelled';
      },
      'R4.4: historical running Task child not omission-settled after restart restore (stale Running card)',
      30000,
    );
    record('R4.4');
    // Post-reconnect event delivery: send ONE unique prompt on the successor
    // generation and require its assistant reply to settle exactly once in
    // the transcript DOM (the recorded post-recovery delivery gap).
    const r4Prompt = `restore-r4-prompt-${Date.now()}`;
    const r4Reply = `${replyMarker} ${r4Prompt}`;
    await page.fill('#prompt-input', r4Prompt);
    await page.press('#prompt-input', 'Enter');
    try {
      await page.waitForFunction(
        (r) => document.getElementById('transcript')?.textContent.includes(r),
        r4Reply,
        { timeout: 30000 },
      );
    } catch (error) {
      await dumpRestoreDiagnostics(page, socketFrames, 'R4-prompt');
      fail(`R4: post-reconnect prompt reply never settled in the transcript DOM: ${error?.message || error}`);
    }
    const replyOccurrences = await page.evaluate(
      (r) => document.getElementById('transcript').textContent.split(r).length - 1,
      r4Reply,
    );
    if (replyOccurrences !== 1) {
      fail(`R4: post-reconnect reply settled ${replyOccurrences} times (expected exactly once)`);
    }
    record('R4.5');
    await laneShot(page, 'restore-r4-restarted.png', { fullPage: true });
    // Close EVERY browser surface (primary page, any residual probe page,
    // the context, the browser) BEFORE the final error union, so close-time
    // errors land in the same browserErrors array and the gate evaluates
    // the complete error set — never a mid-run snapshot.
    await page.close().catch((error) => browserErrors.push(`page close: ${error?.message || error}`));
    await context.close().catch((error) => browserErrors.push(`context close: ${error?.message || error}`));
    await browser.close().catch((error) => browserErrors.push(`browser close: ${error?.message || error}`));
    // Final error union gate over the context-wide monitor: every
    // pageerror and console.error from the primary page and the R5
    // connectionProbe (and any other page) under ONE policy.
    if (browserErrors.length > 0) {
      fail(`browser errors observed: ${browserErrors.join(' | ')}`);
    }
    console.log(`web-session-restore: all assertions executed (${executedCounts.size}/${DOCUMENTED_IDS.length}) with a zero browser-error union`);
    // Ledger + manifest stage AFTER the close/error gates: staged artifacts
    // only — the shell validates the exact set and commits the run dir +
    // pointer after its own HTTP sanity and listener-close gates pass.
    stageLedgerAndManifest();
    console.log(`web-session-restore: STAGED ${stagingDir}`);
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  cleanupStaging();
  if (fs.existsSync(stagingDir)) {
    console.error(`web-session-restore: FAIL: staging dir survived failure cleanup: ${stagingDir}`);
    process.exit(3);
  }
  console.error(`web-session-restore: FAIL: ${err && err.stack ? err.stack : err}`);
  process.exit(2);
});