// Goal panel E2E (playwright lane of E2E.d/web/goal.sh).
//
// Environment:
//   RPI_URL         http://127.0.0.1:<port>/web
//   RPI_TOKEN       token file content (served via rpi-auth.<token> subprotocol)
//   RPI_EVIDENCE    evidence dir for screenshots
//
// Asserts:
//   1. page loads and WS connects (subprotocol)
//   2. Goal panel opens; empty state shows before any goal exists
//   3. creating a goal through the panel form updates the panel (objective,
//      lifecycle, budget + usage) and appends a `created` journal entry
//   4. pinning through the panel form shows the pin and a `pins_updated` entry
//   5. a goal_pause issued by a SECOND WS client (raw `ws`, live event
//      stream, stamped with the page's bound sessionId) flips the panel
//      to `paused` without any page interaction — proving goal events
//      refresh the panel live
//   6. Resume via the panel button returns to `active`; journal replays the
//      full history (created → pins_updated → paused → resumed)

import { chromium } from 'playwright';
import WebSocket from 'ws';
import fs from 'node:fs';
import { clickFeature, openSessionsDrawer } from './lib/feature-menu.mjs';
import { createFreshSessionViaManage } from './lib/session-lifecycle.mjs';
const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

function fail(message) {
  // Exit 2 (not 1): 1 means "playwright setup failure (node/chromium/npm)",
  // which must NOT be confused with an assertion failure (the mock's request
  // counter is stateful and a rerun would see shifted replies — a fake pass).
  console.error(`web-goal: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

/** Raw RPC client over the SAME listener (second WS session). */
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

/** Permanent mid-scroll regression for the Goal head/strip bug (real
 *  failure 2680: with the old sticky head inside the padded drawer
 *  scroller, journal rows painted through the padding strip above the
 *  head). Browser assertions only — no CSS-source or wiring test:
 *    - .goal-panel__body is the ONLY scroll surface and really scrolls
 *      (scrollHeight > clientHeight, scrollTop > 0 at mid-50%),
 *    - the head holds its position across the scroll (±1px),
 *    - all three in-head hit points resolve inside the head,
 *    - the strip above the head never resolves to journal content.
 *  Runs on both the desktop and the phone page. */
async function assertMidScrollHead(pg, failFn, tag) {
  const mid = await pg.evaluate(() => new Promise((resolve) => {
    const panel = document.getElementById('goal-panel');
    const body = panel ? panel.querySelector('.goal-panel__body') : null;
    const head = panel ? panel.querySelector('.goal-panel__head') : null;
    const journal = document.getElementById('goal-journal');
    const round = (n) => Math.round(n * 10) / 10;
    if (!panel || !body || !head) {
      resolve({ ok: false, reason: 'panel/body/head missing' });
      return;
    }
    if (!(body.scrollHeight > body.clientHeight + 1)) {
      resolve({ ok: false, reason: `body is not scrollable (scrollHeight ${round(body.scrollHeight)} <= clientHeight ${round(body.clientHeight)})` });
      return;
    }
    const headBefore = round(head.getBoundingClientRect().top);
    body.scrollTop = Math.round((body.scrollHeight - body.clientHeight) / 2);
    requestAnimationFrame(() => requestAnimationFrame(() => {
      const headRect = head.getBoundingClientRect();
      const panelRect = panel.getBoundingClientRect();
      const cx = Math.round(headRect.left + headRect.width / 2);
      const cy = Math.round(headRect.top + headRect.height / 2);
      const describe = (el) => el
        ? { inHead: head.contains(el), inJournal: journal.contains(el), tag: el.tagName.toLowerCase() }
        : null;
      const hit = (x, y) => describe(document.elementFromPoint(x, y));
      resolve({
        ok: true,
        scrollTop: round(body.scrollTop),
        headBefore,
        headAfter: round(headRect.top),
        headHits: [
          ['left', Math.round(headRect.left + 8), cy],
          ['center', cx, cy],
          ['right', Math.round(headRect.right - 8), cy],
        ].map(([name, x, y]) => ({ name, hit: hit(x, y) })),
        strip: { gapPx: round(headRect.top - panelRect.top), hit: hit(cx, Math.round(panelRect.top + 3)) },
      });
    }));
  }));
  if (!mid.ok) {
    await failFn(`goal-scroll-head: ${tag}: ${mid.reason}`);
    return;
  }
  if (!(mid.scrollTop > 0)) {
    await failFn(`goal-scroll-head: ${tag}: body did not scroll at mid-50% (scrollTop ${mid.scrollTop})`);
    return;
  }
  if (Math.abs(mid.headAfter - mid.headBefore) > 1) {
    await failFn(`goal-scroll-head: ${tag}: head moved across the mid-50% scroll (top ${mid.headBefore} -> ${mid.headAfter})`);
    return;
  }
  for (const point of mid.headHits) {
    if (!(point.hit && point.hit.inHead)) {
      await failFn(`goal-scroll-head: ${tag}: ${point.name} hit outside the head (${point.hit ? point.hit.tag : 'none'})`);
      return;
    }
  }
  if (mid.strip.gapPx > 4 && mid.strip.hit && mid.strip.hit.inJournal) {
    await failFn(`goal-scroll-head: ${tag}: strip above the head hit journal content (${mid.strip.hit.tag})`);
  }
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const wsUrl = url.replace(/^http/, 'ws').replace(/\/web\/?$/, '/ws');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const desktopConsoleErrors = [];
    const phoneConsoleErrors = [];
    const desktopPageErrors = [];
    const phonePageErrors = [];
    const phoneWire = [];
    const page = await browser.newPage();
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    page.on('console', (msg) => {
      if (msg.type() === 'error') desktopConsoleErrors.push(msg.text());
    });
    page.on('pageerror', (err) => {
      desktopPageErrors.push(err.message);
      console.error(`web-goal: page error: ${err.message}`);
    });

    // 1. Page loads; WS connects via the rpi-auth.<token> subprotocol (Settings panel).
    // Capture the page's BOUND sessionId the same way the UI does: listen.rs
    // grants each connection exactly one sessionId-less get_state (bootstrap),
    // and the response's `sessionId` is the source App.tsx `applyState` uses
    // for its cold bind. After bootstrap every command must carry sessionId
    // ("sessionId is required after WebSocket bootstrap"), so the raw
    // goal_pause below is stamped with this captured value.
    let pageSessionId = '';
    page.on('websocket', (ws) => {
      ws.on('framereceived', (frame) => {
        try {
          const msg = JSON.parse(String(frame.payload));
          if (!msg || msg.type !== 'response' || msg.success === false) return;
          const data = msg.data || {};
          const sid = typeof data.sessionId === 'string' && data.sessionId !== ''
            ? data.sessionId
            : (data.state && typeof data.state.sessionId === 'string' ? data.state.sessionId : '');
          if (sid) pageSessionId = sid;
        } catch {
          /* non-JSON frame (ping/pong or binary): not a state snapshot */
        }
      });
    });
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'WS did not reach "connected"'
    );

    // 2. Open the Goal panel; empty state must render before any goal exists.
    await clickFeature(page, '#goal-panel-btn');
    await waitFor(page, () => document.getElementById('goal-panel') !== null, 'goal panel missing');
    await waitFor(
      page,
      () => document.getElementById('goal-panel').dataset.hasGoal === 'false',
      'goal panel should start with no goal'
    );

    // 3. Create a goal through the panel form.
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
    const budgetText = await page.textContent('#goal-budget');
    if (!budgetText.includes('100 token budget')) {
      fail(`budget line wrong: ${budgetText}`);
    }
    const usageText = await page.textContent('#goal-usage');
    if (!usageText.includes('0/100 tokens')) {
      fail(`usage line wrong: ${usageText}`);
    }
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('#goal-journal li')).some(
          (li) => li.dataset.kind === 'created'
        ),
      'journal never recorded the created event'
    );

    // 4. Pin through the panel form.
    const pinText = 'stay focused';
    await page.fill('#goal-pin-input', pinText);
    await page.click('#goal-pin-btn');
    await waitFor(
      page,
      (pin) =>
        Array.from(document.querySelectorAll('#goal-pins li .goal-pin__text')).some(
          (el) => el.textContent === pin
        ),
      'pinned text never appeared in the panel',
      30000,
      pinText
    );
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('#goal-journal li')).some(
          (li) => li.dataset.kind === 'pins_updated'
        ),
      'journal never recorded the pins_updated event'
    );
    await page.screenshot({ path: `${evidence}/goal-created-pinned.png`, fullPage: true });

    // 5. Live update: a SECOND WS client pauses the goal. The panel must
    //    reflect the paused state purely from the pushed goal_updated event.
    const rpc = rpcClient(wsUrl);
    if (pageSessionId === '') {
      fail('page sessionId is missing after WebSocket bootstrap; cannot stamp goal_pause');
    }
    const paused = await rpc.call({ type: 'goal_pause', sessionId: pageSessionId });
    if (!paused || paused.lifecycle !== 'paused') {
      fail(`raw goal_pause returned ${JSON.stringify(paused)}`);
    }
    await waitFor(
      page,
      () => document.getElementById('goal-panel').dataset.lifecycle === 'paused',
      'panel never flipped to paused from the live event'
    );
    const statusText = await page.textContent('#goal-status');
    if (!statusText.includes('paused')) {
      fail(`status line wrong: ${statusText}`);
    }
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('#goal-journal li')).some(
          (li) => li.dataset.kind === 'paused'
        ),
      'journal never recorded the paused event'
    );
    await page.screenshot({ path: `${evidence}/goal-paused-live.png`, fullPage: true });

    // 6. Resume via the panel's own action button.
    await page.click('#goal-resume-btn');
    await waitFor(
      page,
      () => document.getElementById('goal-panel').dataset.lifecycle === 'active',
      'panel never returned to active after resume'
    );
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('#goal-journal li')).some(
          (li) => li.dataset.kind === 'resumed'
        ),
      'journal never recorded the resumed event'
    );

    // Journal replay order: created → pins_updated → paused → resumed.
    const kinds = await page.$$eval('#goal-journal li', (lis) => lis.map((li) => li.dataset.kind));
    const expected = ['created', 'pins_updated', 'paused', 'resumed'];
    for (const kind of expected) {
      if (!kinds.includes(kind)) {
        fail(`journal replay missing ${kind}; got ${JSON.stringify(kinds)}`);
      }
    }
    const firstCreated = kinds.indexOf('created');
    const firstPinned = kinds.indexOf('pins_updated');
    const firstPaused = kinds.indexOf('paused');
    const firstResumed = kinds.indexOf('resumed');
    if (!(firstCreated < firstPinned && firstPinned < firstPaused && firstPaused < firstResumed)) {
      fail(`journal replay out of order: ${JSON.stringify(kinds)}`);
    }
    await page.screenshot({ path: `${evidence}/goal-resumed-journal.png`, fullPage: true });

    // 7. Layout regression ('goal-layout-regression:*' labels only). Source
    //    contract: a dropped goal STAYS current and a goal is NEVER replaced,
    //    so the multiline fixture gets a NEW session via the real
    //    New-session journey.
    await page.click('#goal-drop-btn');
    await waitFor(
      page,
      () =>
        document.getElementById('goal-panel').dataset.lifecycle === 'dropped' &&
        document.getElementById('goal-panel').dataset.hasGoal === 'true',
      'dropped goal did not stay current with lifecycle dropped'
    );
    let replaceRejected = false;
    try {
      await rpc.call({ type: 'goal_create', sessionId: pageSessionId, objective: 'replace attempt' });
    } catch {
      replaceRejected = true;
    }
    if (!replaceRejected) {
      fail('goal_create replaced the dropped goal (never-replace contract)');
    }
    const oldSessionId = pageSessionId;
    await openSessionsDrawer(page);
    const layoutSession = await createFreshSessionViaManage(page, waitFor, 'goal layout fixture session', { closePanel: true });
    const layoutSessionId = layoutSession.sessionId;
    if (!layoutSessionId || layoutSessionId === oldSessionId) {
      fail('fresh layout session identity missing after the New-session journey');
    }
    await clickFeature(page, '#goal-panel-btn');
    await waitFor(page, () => document.getElementById('goal-panel')?.dataset.hasGoal === 'false', 'fresh session goal panel not empty');

    // Short prefix lines (no natural wrap) + one long unbroken TOKEN line.
    const layoutObjective = [
      'layout regression objective',
      'Constraints: short line two.',
      'Tasks: short line three.',
      'TOKEN_' + 'A'.repeat(300),
    ].join('\n');
    await rpc.call({ type: 'goal_create', sessionId: layoutSessionId, objective: layoutObjective, tokenBudget: 500 });
    await waitFor(
      page,
      (obj) => document.getElementById('goal-objective')?.textContent === obj,
      'multiline layout goal never appeared',
      30000,
      layoutObjective
    );
    await rpc.call({ type: 'goal_pin', sessionId: layoutSessionId, text: 'layout regression pin' });
    await waitFor(page, () => document.querySelectorAll('#goal-pins li .goal-pin__text').length === 1, 'layout pin never appeared');

    // Close the panel for the usage seeding: goal_usage_charged events do
    // not refresh the journal while the panel is open (App refreshes on
    // goal_state only — run 2710 diag: rows=2 with 32 accepted ACKs, active
    // lifecycle), so seeding while open would never publish the usage rows.
    // The reopen below follows the real user menu path whose fresh
    // goal_journal fetch publishes every seeded row. Live-event tests above
    // are untouched; no pin/refresh masking is added.
    await page.locator('#goal-close-btn').click();
    await waitFor(page, () => document.getElementById('goal-panel') === null, 'layout goal panel never closed for seeding');

    // Seed ONLY this layout block with enough usage rows for a real
    // scrollable body, without the slow 253-row fixture: tokens:1 per call.
    // ACK tally is safe metadata (counts only) consumed by the row-count
    // failure diagnostic below; a rejection still propagates unchanged.
    const LAYOUT_USAGE_ROWS = 32;
    let usageAckAccepted = 0;
    let usageAckRejected = 0;
    for (let i = 0; i < LAYOUT_USAGE_ROWS; i++) {
      try {
        await rpc.call({ type: 'goal_update_usage', sessionId: layoutSessionId, tokens: 1, activeTimeSeconds: 0 });
        usageAckAccepted += 1;
      } catch (err) {
        usageAckRejected += 1;
        throw err;
      }
    }

    // Reopen through the normal feature-menu journey (the REAL user path):
    // the open must send a fresh goal_journal fetch that publishes the full
    // seeded journal. goal_usage_charged does NOT refresh the journal while
    // the panel is open (App refreshes on goal_state only — run 2710 diag:
    // rows=2 with 32 accepted ACKs), so the usage rows can only arrive via
    // this fetch. No artificial pin/refresh trigger is added to mask it.
    await clickFeature(page, '#goal-panel-btn');
    await waitFor(page, () => document.getElementById('goal-panel') !== null, 'layout goal panel never reopened');
    await waitFor(
      page,
      (obj) => document.getElementById('goal-objective')?.textContent === obj,
      'layout objective never reappeared after reopen',
      30000,
      layoutObjective
    );

    // Per-NEWLINE positions: the char before and after every source newline
    // must land on DIFFERENT visual lines (wrap cannot fake this).
    const assertBreaks = (tops, where) => {
      for (let i = 0; i + 1 < tops.length; i += 2) {
        if (!(tops[i] < tops[i + 1])) {
          fail(`goal-layout-regression: ${where} source newline ${i / 2} collapsed (tops ${JSON.stringify(tops)})`);
        }
      }
    };
    assertBreaks(
      await page.evaluate((obj) => {
        const el = document.getElementById('goal-objective');
        const node = el.firstChild;
        const tops = [];
        let offset = 0;
        for (const line of obj.split('\n').slice(0, -1)) {
          const before = document.createRange();
          before.setStart(node, offset + line.length - 1);
          before.setEnd(node, offset + line.length);
          tops.push(Math.round(before.getBoundingClientRect().top));
          const after = document.createRange();
          after.setStart(node, offset + line.length + 1);
          after.setEnd(node, offset + line.length + 2);
          tops.push(Math.round(after.getBoundingClientRect().top));
          offset += line.length + 1;
        }
        return tops;
      }, layoutObjective),
      'objective'
    );
    const tokenOverflow = await page.evaluate(() => {
      const el = document.getElementById('goal-objective');
      return el.scrollWidth > el.clientWidth + 1;
    });
    if (tokenOverflow) fail('goal-layout-regression: long unbroken TOKEN overflows the objective');

    // The journal DOM is populated by refreshGoal RESPONSES — from goal_updated
    // events OR the settled bind/switch fetch (App.tsx:6435/:6442, settled
    // effect -> :3781) — which can lag the pins/goal waits. Wait for the REAL
    // publication condition (the
    // fold carrying the exact fixture objective) before any fold assert —
    // no sleeps, no fixed timeouts as proof.
    await waitFor(
      page,
      (obj) => {
        const folds = Array.from(document.querySelectorAll('#goal-journal li details.goal-journal__objective'));
        return folds.some((d) => d.querySelector('.goal-journal__objective-full')?.textContent === obj);
      },
      'layout journal fold never published (goal_updated -> goal_journal refresh)',
      30000,
      layoutObjective
    );
    // Kind bound kept in sync with the seeded fixture (1 created +
    // 1 pins_updated + LAYOUT_USAGE_ROWS usage_updated): the publication
    // wait above can fire on the FIRST refresh, so require the full seeded
    // set before any row/scroll assertion below.
    const rowWait = async () => {
      try {
        await page.waitForFunction(
          (n) => document.querySelectorAll('#goal-journal li').length === n,
          1 + 1 + LAYOUT_USAGE_ROWS,
          { timeout: 30000 },
        );
      } catch {
        // Safe failure diagnostics: counts and lifecycle labels only — no
        // budget/usage text, no objective text. The ACK tally is recorded
        // while seeding above.
        const diag = await page.evaluate(() => {
          const lis = Array.from(document.querySelectorAll('#goal-journal li'));
          const counts = {};
          for (const li of lis) {
            const k = li.dataset.kind || 'unknown';
            counts[k] = (counts[k] || 0) + 1;
          }
          const panel = document.getElementById('goal-panel');
          return {
            journalRows: lis.length,
            kindCounts: counts,
            foldCount: lis.filter((li) => li.querySelector('details.goal-journal__objective')).length,
            panelHasGoal: panel ? panel.dataset.hasGoal ?? null : null,
            panelLifecycle: panel ? panel.dataset.lifecycle ?? null : null,
          };
        }).catch(() => null);
        fail(
          `layout journal never reached the seeded row count (expected ${1 + 1 + LAYOUT_USAGE_ROWS}, ` +
            `usageAck accepted ${usageAckAccepted}/rejected ${usageAckRejected}; ` +
            `safe diag ${JSON.stringify(diag)})`,
        );
      }
    };
    await rowWait();
    const layoutKinds = await page.evaluate(() =>
      Array.from(document.querySelectorAll('#goal-journal li')).map((li) => li.dataset.kind || 'unknown'),
    );
    if (layoutKinds.filter((k) => k === 'usage_updated').length !== LAYOUT_USAGE_ROWS) {
      const counts = {};
      for (const k of layoutKinds) counts[k] = (counts[k] || 0) + 1;
      fail(`goal-layout-regression: usage_updated rows out of bound with the seeded fixture (${JSON.stringify(counts)}, expected ${LAYOUT_USAGE_ROWS})`);
    }

    // Journal folds: locate the layout fold by FULL text (never .first()).
    const foldDiag = await page.evaluate((obj) => ({
      folds: document.querySelectorAll('#goal-journal li details.goal-journal__objective').length,
      journalRows: document.querySelectorAll('#goal-journal li').length,
      kinds: Array.from(document.querySelectorAll('#goal-journal li')).map((li) => li.dataset.kind || 'unknown'),
      currentObjectiveLength: (document.getElementById('goal-objective')?.textContent || '').length,
      expectedLength: obj.length,
    }), layoutObjective);
    const foldIndex = await page.evaluate((obj) => {
      return Array.from(document.querySelectorAll('#goal-journal li details.goal-journal__objective')).findIndex(
        (d) => d.querySelector('.goal-journal__objective-full')?.textContent === obj,
      );
    }, layoutObjective);
    if (foldIndex < 0) {
      fail(
        `goal-layout-regression: layout fold not in journal (published folds ${foldDiag.folds}, rows ${foldDiag.journalRows}, ` +
          `kinds ${JSON.stringify(foldDiag.kinds)}, current objective length ${foldDiag.currentObjectiveLength}, expected ${foldDiag.expectedLength})`,
      );
    }

    const rows = await page.evaluate(() => {
      const lis = Array.from(document.querySelectorAll('#goal-journal li'));
      return lis.map((li) => {
        const d = li.querySelector('details.goal-journal__objective');
        if (!d) return { hasDetails: false };
        const summary = d.querySelector('summary');
        const lineHeight = parseFloat(getComputedStyle(summary).lineHeight) || 18;
        return {
          hasDetails: true,
          open: d.open,
          detailsHeight: Math.round(d.getBoundingClientRect().height * 10) / 10,
          summaryHeight: Math.round(summary.getBoundingClientRect().height * 10) / 10,
          twoLineMax: Math.round(2 * lineHeight + 1),
          rowHeight: Math.round(li.getBoundingClientRect().height * 10) / 10,
        };
      });
    });
    if (rows.some((r) => !r.hasDetails)) {
      fail(`goal-layout-regression: journal row without the details inspect-full fold (rows ${rows.length}, without fold ${rows.filter((r) => !r.hasDetails).length})`);
    }
    if (rows.some((r) => r.open)) {
      fail('goal-layout-regression: journal folds not collapsed by default');
    }
    if (rows.some((r) => r.detailsHeight > r.summaryHeight + 1)) {
      fail(`goal-layout-regression: collapsed fold body contributes layout (details ${JSON.stringify(rows.map((r) => r.detailsHeight))} vs summary ${JSON.stringify(rows.map((r) => r.summaryHeight))})`);
    }
    if (rows.some((r) => r.summaryHeight > r.twoLineMax)) {
      fail('goal-layout-regression: collapsed row exceeds 2 lines (heights ' + JSON.stringify(rows.map((r) => r.summaryHeight)) + ')');
    }
    const foldSummary = page.locator('#goal-journal li details.goal-journal__objective').nth(foldIndex).locator('summary');
    const foldOpen = () =>
      page.evaluate((idx) => document.querySelectorAll('#goal-journal li details.goal-journal__objective')[idx].open, foldIndex);
    await foldSummary.click();
    if (!(await foldOpen())) fail('goal-layout-regression: first click did not open the fold');
    const expanded = await page.evaluate((idx) => {
      const li = document.querySelectorAll('#goal-journal li')[idx];
      const d = li.querySelector('details.goal-journal__objective');
      const body = d.querySelector('.goal-journal__objective-full');
      return { open: d.open, visible: body.getBoundingClientRect().height > 0, full: body.textContent, rowHeight: Math.round(li.getBoundingClientRect().height * 10) / 10, tops: (() => {
        const node = body.firstChild;
        const obj = body.textContent;
        const t = [];
        let offset = 0;
        for (const line of obj.split('\n').slice(0, -1)) {
          const before = document.createRange();
          before.setStart(node, offset + line.length - 1);
          before.setEnd(node, offset + line.length);
          t.push(Math.round(before.getBoundingClientRect().top));
          const after = document.createRange();
          after.setStart(node, offset + line.length + 1);
          after.setEnd(node, offset + line.length + 2);
          t.push(Math.round(after.getBoundingClientRect().top));
          offset += line.length + 1;
        }
        return t;
      })() };
    }, foldIndex);
    if (!expanded.open || !expanded.visible || expanded.full !== layoutObjective) fail('goal-layout-regression: opened fold does not show the full objective');
    if (expanded.rowHeight <= rows[foldIndex].rowHeight) {
      fail(`goal-layout-regression: expanding did not grow the row (closed ${rows[foldIndex].rowHeight} -> open ${expanded.rowHeight})`);
    }
    for (let i = 0; i + 1 < expanded.tops.length; i += 2) {
      if (!(expanded.tops[i] < expanded.tops[i + 1])) {
        fail(`goal-layout-regression: expanded fold source newline ${i / 2} collapsed`);
      }
    }
    await foldSummary.click();
    if (await foldOpen()) fail('goal-layout-regression: second click did not close the fold');
    await foldSummary.focus();
    await page.keyboard.press('Enter');
    if (!(await foldOpen())) fail('goal-layout-regression: keyboard Enter did not open the fold');
    await foldSummary.focus();
    await page.keyboard.press(' ');
    if (await foldOpen()) fail('goal-layout-regression: keyboard Space did not close the fold');

    // Desktop resizer: same width and same right edge as the drawer.
    const widths = await page.evaluate(() => {
      const p = document.getElementById('goal-panel').getBoundingClientRect();
      const r = document.getElementById('panel-drawer-resizer').getBoundingClientRect();
      return { pw: p.width, rw: r.width, pr: p.right, rr: r.right };
    });
    if (Math.abs(widths.rw - widths.pw) > 1 || Math.abs(widths.rr - widths.pr) > 1) {
      fail(`goal-layout-regression: resizer/drawer edge mismatch (resizer ${widths.rw} vs drawer ${widths.pw})`);
    }

    // Permanent same-bug regression (real failure 2680: a sticky head inside
    // the padded drawer scroller let journal rows paint through the padding
    // strip above the head). Mid-50% scroll on the REAL body scroller.
    await assertMidScrollHead(page, fail, 'desktop');

    // 8. Phone 390x844: 44px unpin tap target without overflow, resizer
    //    hidden, summary tap reveals the SAME full fixture objective, and
    //    the panel survives close/reopen.
    // Phone boot mirrors the established mobile lane exactly: plain
    // newPage({viewport}) (no isMobile/hasTouch variant), token, connected
    // gate, feature-menu panel toggle, #goal-close-btn close. On ANY phone
    // wait failure: failure screenshot + safe mini-DOM (no user text), then
    // the labeled fail.
    const phoneFail = async (message) => {
      try {
        const shotPath = `${evidence}/goal-phone-failure.png`;
        await ppage.screenshot({ path: shotPath });
        fs.chmodSync(shotPath, 0o600);
        const mini = await ppage.evaluate(() => {
          const panel = document.getElementById('goal-panel');
          const pr = panel ? panel.getBoundingClientRect() : null;
          const sidebar = document.querySelector('.session-sidebar');
          const sr = sidebar ? sidebar.getBoundingClientRect() : null;
          const overlaps = (a, b) => a !== null && b !== null && a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom;
          return {
            goalPanelPresent: panel !== null,
            panelHasGoal: panel ? panel.dataset.hasGoal ?? null : null,
            panelLifecycle: panel ? panel.dataset.lifecycle ?? null : null,
            panelRect: pr ? { left: Math.round(pr.left), top: Math.round(pr.top), width: Math.round(pr.width), height: Math.round(pr.height) } : null,
            panelOccludedBySidebar: overlaps(pr, sr),
            goalPanelBtnAriaExpanded: document.getElementById('goal-panel-btn')?.getAttribute('aria-expanded') ?? null,
            goalPanelBtnDisplay: document.getElementById('goal-panel-btn') ? getComputedStyle(document.getElementById('goal-panel-btn')).display : null,
            featureMenuBtnExpanded: document.getElementById('feature-menu-btn')?.getAttribute('aria-expanded') ?? null,
            sidebarDrawerOpen: document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true,
            sidebarRect: sr ? { left: Math.round(sr.left), top: Math.round(sr.top), width: Math.round(sr.width), height: Math.round(sr.height) } : null,
            connected: document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
            sendBtnLabel: document.getElementById('send-btn')?.getAttribute('aria-label') ?? null,
            viewport: { width: window.innerWidth, height: window.innerHeight },
            mountedPanels: ['#goal-panel', '#session-panel', '#todo-panel', '#workflow-panel', '#settings-panel', '#subagents-panel'].filter((s) => document.querySelector(s) !== null),
            journalRows: document.querySelectorAll('#goal-journal li').length,
            journalKinds: Array.from(document.querySelectorAll('#goal-journal li')).map((li) => li.dataset.kind || 'unknown'),
            journalFolds: document.querySelectorAll('#goal-journal li details.goal-journal__objective').length,
            journalBodyLengths: Array.from(document.querySelectorAll('#goal-journal li .goal-journal__objective-full')).map((b) => b.textContent.length),
            currentObjectiveLength: (document.getElementById('goal-objective')?.textContent || '').length,
          };
        });
        mini.wireTail = phoneWire.slice(-40).map((e) => ({
          dir: e.dir, seq: e.seq, type: e.type, ownerMatch: e.ownerMatch,
          success: e.success, isArray: e.isArray, arrayLen: e.arrayLen,
          lastObjectiveLen: e.lastObjectiveLen, objectiveExact: e.objectiveExact,
        }));
        mini.wireNote = 'wire entries are safe metadata only (dir/seq/type/ownerMatch/success/arrayLen/lastObjectiveLen/objectiveExact); rpc ids omitted; no text';
        console.error(`web-goal: phone failure diagnostics: ${JSON.stringify(mini)}`);
      } catch (diagErr) {
        console.error(`web-goal: phone failure diagnostics unavailable: ${diagErr.message}`);
      }
      fail(message);
    };
    const phoneWait = async (fn, label, arg, timeoutMs = 25000) => {
      try {
        await ppage.waitForFunction(fn, arg, { timeout: timeoutMs });
      } catch (err) {
        if (err && err.name === 'TimeoutError') {
          await phoneFail(`${label} (timeout ${timeoutMs}ms)`);
        }
        throw err;
      }
    };
    const ppage = await browser.newPage({ viewport: { width: 390, height: 844 } });
    if (token) {
      await ppage.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    ppage.on('console', (msg) => {
      if (msg.type() === 'error') phoneConsoleErrors.push(msg.text());
    });
    ppage.on('pageerror', (err) => phonePageErrors.push(err.message));
    // Wire recorder (SAFE OUTPUT ONLY: rpc ids are kept INTERNALLY for
    // request/response pairing and never printed; printed fields are
    // dir/seq/type/ownerMatch/success/arrayLen/objectiveExact booleans).
    ppage.on('websocket', (ws) => {
      let seq = 0;
      const record = (dir, frame) => {
        try {
          const msg = JSON.parse(String(frame.payload));
          if (!msg || typeof msg !== 'object') return;
          const entry = {
            dir,
            seq: ++seq,
            id: typeof msg.id === 'string' ? msg.id : null,
            type: typeof msg.type === 'string' ? msg.type : null,
            ownerMatch: msg.sessionId == null ? null : msg.sessionId === layoutSessionId,
          };
          if (msg.type === 'response') {
            entry.success = msg.success === true;
            entry.isArray = Array.isArray(msg.data);
            entry.arrayLen = Array.isArray(msg.data) ? msg.data.length : null;
            const arr = Array.isArray(msg.data) ? msg.data : [];
            entry.objectiveExact = arr.length > 0 && arr.every((e) => e && e.goal && e.goal.objective === layoutObjective);
          }
          phoneWire.push(entry);
          if (phoneWire.length > 400) phoneWire.shift();
        } catch { /* non-JSON frame */ }
      };
      ws.on('framesent', (f) => record('sent', f));
      ws.on('framereceived', (f) => record('recv', f));
    });
    await ppage.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await phoneWait(() => document.getElementById('send-btn')?.hasAttribute('disabled') !== true, 'phone WS did not reach connected');
    // The phone page bootstraps onto the listener's CURRENT session — the
    // OLD one with the dropped goal (bg2258 mini-DOM: lifecycle DROPPED).
    // Switch to the layout session through the REAL user session-select
    // path (exact sidebar row click), verify the authoritative sidebar
    // binding, THEN open the Goal panel and require the same objective.
    await openSessionsDrawer(ppage);
    await ppage.locator(`.session-sidebar__switch[data-session-id="${layoutSessionId}"]`).first().click();
    await phoneWait(
      (sid) => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId === sid,
      'phone sidebar never activated the layout session',
      layoutSessionId,
    );
    await clickFeature(ppage, '#goal-panel-btn');
    await phoneWait(() => document.getElementById('goal-panel')?.dataset.lifecycle === 'active', 'phone layout session goal panel never showed the active goal');
    await phoneWait((obj) => document.getElementById('goal-objective')?.textContent === obj, 'phone goal panel never showed the layout objective', layoutObjective);
    // The journal must arrive via the settled-authority effect's OWN owner
    // goal_journal fetch — no pin mutation forces it. TRUE PAIR: the sent
    // goal_journal and its RESPONSE must share the rpc id (response strictly
    // after the request on the same socket ordinal), succeed for the exact
    // owner, and carry the EXACT fixture objective internally (bool only;
    // never printed).
    {
      const deadline = Date.now() + 30000;
      let pairOk = false;
      while (Date.now() < deadline) {
        pairOk = phoneWire.some((s) =>
          s.dir === 'sent' && s.type === 'goal_journal' && s.ownerMatch === true &&
          phoneWire.some((r) =>
            r.dir === 'recv' && r.type === 'response' && r.id !== null && r.id === s.id &&
            r.seq > s.seq && r.success === true && r.ownerMatch === true &&
            r.isArray === true && (r.arrayLen ?? 0) >= 1 && r.objectiveExact === true));
        if (pairOk) break;
        await ppage.waitForTimeout(150);
      }
      if (!pairOk) {
        await phoneFail(`phone owner goal_journal request/response pair never arrived after the switch (frames ${phoneWire.length}, journal sends ${phoneWire.filter((e) => e.dir === 'sent' && e.type === 'goal_journal').length})`);
      }
    }
    const phoneState = await ppage.evaluate(() => {
      const unpin = document.querySelector('.goal-pin__unpin').getBoundingClientRect();
      const panel = document.getElementById('goal-panel').getBoundingClientRect();
      const resizer = document.getElementById('panel-drawer-resizer');
      return {
        unpinMin: Math.min(unpin.width, unpin.height),
        unpinOverflows: unpin.right > panel.right + 1,
        resizerVisible: resizer !== null && resizer.getBoundingClientRect().width > 0,
      };
    });
    if (phoneState.unpinMin < 44) fail(`goal-layout-regression: phone unpin tap target ${phoneState.unpinMin}px < 44px`);
    if (phoneState.unpinOverflows) fail('goal-layout-regression: phone unpin button overflows the drawer');
    if (phoneState.resizerVisible) fail('goal-layout-regression: phone must hide the panel resizer');

    // Same publication wait on the phone page: the journal arrives via the
    // settled-authority fetch (or a goal_updated event), not with the lifecycle.
    await phoneWait(
      (obj) => {
        const folds = Array.from(document.querySelectorAll('#goal-journal li details.goal-journal__objective'));
        return folds.some((d) => d.querySelector('.goal-journal__objective-full')?.textContent === obj);
      },
      'phone layout journal fold never published (settled-authority goal_journal fetch)',
      layoutObjective,
      30000
    );
    const phoneFoldDiag = await ppage.evaluate((obj) => ({
      folds: document.querySelectorAll('#goal-journal li details.goal-journal__objective').length,
      journalRows: document.querySelectorAll('#goal-journal li').length,
      currentObjectiveLength: (document.getElementById('goal-objective')?.textContent || '').length,
      expectedLength: obj.length,
    }), layoutObjective);
    const phoneFoldIndex = await ppage.evaluate((obj) => {
      return Array.from(document.querySelectorAll('#goal-journal li details.goal-journal__objective')).findIndex(
        (d) => d.querySelector('.goal-journal__objective-full')?.textContent === obj,
      );
    }, layoutObjective);
    if (phoneFoldIndex < 0) fail(`goal-layout-regression: phone journal lacks the full layout objective fold (folds ${phoneFoldDiag.folds}, rows ${phoneFoldDiag.journalRows}, current objective length ${phoneFoldDiag.currentObjectiveLength}, expected ${phoneFoldDiag.expectedLength})`);
    await ppage.locator('#goal-journal li details.goal-journal__objective').nth(phoneFoldIndex).locator('summary').click();
    const phoneFold = await ppage.evaluate((idx) => {
      const d = document.querySelectorAll('#goal-journal li details.goal-journal__objective')[idx];
      const body = d.querySelector('.goal-journal__objective-full');
      return { open: d.open, visible: body.getBoundingClientRect().height > 0, full: body.textContent };
    }, phoneFoldIndex);
    if (!phoneFold.open || !phoneFold.visible || phoneFold.full !== layoutObjective) {
      fail('goal-layout-regression: phone fold tap did not reveal the full fixture objective');
    }
    // Permanent same-bug regression on the phone page: mid-50% scroll must
    // live on .goal-panel__body with the head fixed above it and no journal
    // row in the strip above the head.
    await assertMidScrollHead(ppage, phoneFail, 'phone');
    await ppage.locator('#goal-close-btn').click();
    await phoneWait(() => document.getElementById('goal-panel') === null, 'phone goal panel never closed');
    await clickFeature(ppage, '#goal-panel-btn');
    await phoneWait(() => document.getElementById('goal-panel')?.dataset.lifecycle === 'active', 'phone goal panel never reopened');
    const unpinReopened = await ppage.evaluate(() => {
      const r = document.querySelector('.goal-pin__unpin').getBoundingClientRect();
      return Math.min(r.width, r.height);
    });
    if (unpinReopened < 44) fail(`goal-layout-regression: phone unpin tap target after reopen ${unpinReopened}px < 44px`);
    await ppage.close();


    // Strict gate: ANY console error or page error on the desktop or the
    // phone page fails the lane (previously only logged — never failed).
    if (desktopConsoleErrors.length > 0 || phoneConsoleErrors.length > 0 || desktopPageErrors.length > 0 || phonePageErrors.length > 0) {
      fail(
        `console/page errors: desktop console ${desktopConsoleErrors.length}, phone console ${phoneConsoleErrors.length}, ` +
          `desktop page ${desktopPageErrors.length}, phone page ${phonePageErrors.length}` +
          `; first: ${(desktopConsoleErrors[0] ?? phoneConsoleErrors[0] ?? desktopPageErrors[0] ?? phonePageErrors[0] ?? '').slice(0, 160)}`,
      );
    }
    rpc.close();
    console.log('web-goal: playwright PASSED (empty state, create, pin, live pause event, resume, journal replay)');
  } finally {
    await browser.close();
  }
}

main().catch((err) => {
  console.error(`web-goal: FATAL: ${err.stack || err.message}`);
  process.exit(2);
});
