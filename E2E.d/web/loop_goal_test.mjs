// Web composer /loop + /goal + /ps E2E lane (playwright half of
// loop_goal.sh).
//
// Environment:
//   RPI_URL          http://127.0.0.1:<port>/web
//   RPI_TOKEN        token file content (served via rpi-auth.<token> subprotocol)
//   RPI_CHROME       executable path of the system Chrome (optional)
//   RPI_EVIDENCE     evidence dir for screenshots
//
// Asserts the Web composer command surface for the backend-builtin /loop,
// /goal, and /ps commands against the REAL listener:
//   - the command picker lists /loop + /goal + /ps (get_commands catalog
//     authority)
//   - choosing /loop drafts "/loop " and choosing /goal drafts "/goal" —
//     neither auto-submits (no user bubble, no summary bubble, no RPC);
//     choosing /ps drafts "/ps" (bare) the same way
//   - bare /ps dispatches process_list against the real listener and renders
//     the bounded TUI-parity "No supervised processes" summary bubble in the
//     empty fixture; /ps extra fails LOCALLY with the usage toast, preserves
//     the draft, and dispatches NO RPC
//   - Enter dispatches the typed RPCs (loop_create/list/update/delete/cancel,
//     goal_create/get/pin/pause/resume/complete/drop) and renders bounded
//     TUI-parity summary bubbles (create/list/update/delete succeed; cancel
//     after delete surfaces the actionable "no active loop" error; drop after
//     complete surfaces the actionable invalid-transition error)
//   - /goal create|resume dispatch `activate: true` (TUI parity: they start
//     or queue goal work) and render "Goal work started|queued|already
//     active · {state}" from the chained goal_get
//   - TUI parity streaming guard: /loop update while a turn is running is
//     rejected locally with "/loop is unavailable while another turn is
//     running" and dispatches NO RPC; it succeeds once the session is idle
//   - malformed arguments fail LOCALLY with TUI-equivalent usage toasts and
//     dispatch NO RPC (bare /loop, /goal unpin nope, /goal pin, /ps extra)
//   - intercepted commands never create an optimistic user bubble
//   - every loop_*/goal_*/process_list frame carries the same non-empty
//     top-level sessionId as the session's own frames (no cross-session
//     routing)
import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

// Documented contracts for the Web coverage matrix (feature "loop + goal
// composer"). Every passing contract records its machine-readable ID; on full
// success the lane writes $RPI_EVIDENCE/coverage-assertions.json
// ({ "executed": [...] }) — the same executed-assertion evidence convention
// as the sessions/scroll/projects lanes, so the assertions enter the existing
// coverage counting/reporting. The matrix fails when any named contract did
// not execute and when any executed ID is not declared in the matrix.
const DOCUMENTED_IDS = [
  'lg.picker-lists-loop-goal',
  'lg.loop-draft-no-submit',
  'lg.goal-draft-no-submit',
  'lg.goal-panel-open',
  'lg.ps-picker-listed',
  'lg.ps-draft-no-submit',
  'lg.ps-empty-list',
  'lg.ps-args-local-reject',
  'lg.ps-session-routed',
  'lg.loop-create',
  'lg.loop-streaming-guard',
  'lg.loop-list',
  'lg.loop-update',
  'lg.loop-delete',
  'lg.loop-cancel-error',
  'lg.goal-create-activate',
  'lg.goal-show',
  'lg.goal-pin',
  'lg.goal-pins',
  'lg.goal-pause',
  'lg.goal-resume-activate',
  'lg.goal-complete',
  'lg.goal-drop-error',
  'lg.usage-error-no-rpc',
  'lg.draft-preserved-on-error',
  'lg.alias-no-prompt',
  'lg.no-user-bubbles',
  'lg.session-routed',
];
const executed = new Set();
function record(id) {
  executed.add(id);
}

function fail(message) {
  // Exit 2 (not 1): 1 means "playwright setup failure (node/chromium/npm)";
  // 2+ is an assertion failure (the lane reports it distinctly).
  console.error(`web-loop-goal: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** Poll transcript summaries created after `startIndex`; returns the first
 *  new bubble whose full text satisfies `matches`. */
async function waitForSummary(page, startIndex, matches, label, timeoutMs = 25000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const hit = await page.evaluate(({ start, source, flags }) => {
      const matcher = new RegExp(source, flags);
      const texts = Array.from(document.querySelectorAll('.msg--summary'))
        .slice(start)
        .map((el) => el.textContent || '');
      return texts.find((text) => matcher.test(text)) ?? null;
    }, { start: startIndex, source: matches.source, flags: matches.flags });
    if (hit !== null) return hit;
    await sleep(150);
  }
  const dump = await page.evaluate(() => ({
    summaries: Array.from(document.querySelectorAll('.msg--summary')).map((el) => el.textContent || ''),
    toasts: Array.from(document.querySelectorAll('#toasts .toast')).map((el) => el.textContent || ''),
    users: Array.from(document.querySelectorAll('.msg--user')).map((el) => (el.textContent || '').slice(0, 120)),
    assistants: Array.from(document.querySelectorAll('.msg--assistant')).map((el) => (el.textContent || '').slice(0, 120)),
  }));
  const frameTypes = sentFrames
    .map((payload) => { try { const frame = JSON.parse(payload); return typeof frame.type === 'string' ? frame.type : '?'; } catch { return null; } })
    .filter(Boolean);
  fail(`${label}: no new summary bubble matching ${matches} (summaries=${JSON.stringify(dump.summaries)} toasts=${JSON.stringify(dump.toasts)} users=${JSON.stringify(dump.users)} assistants=${JSON.stringify(dump.assistants)} frames=${JSON.stringify(frameTypes)})`);
}

/** Poll the transcript for ANY assistant/user text containing `substr`
 *  (used to wait for the mock's streamed turn text). */
async function waitForTranscriptText(page, substr, label, timeoutMs = 25000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const hit = await page.evaluate((want) => {
      const text = document.querySelector('#transcript, .transcript, main')?.textContent || document.body.textContent || '';
      return text.includes(want);
    }, substr);
    if (hit) return;
    await sleep(150);
  }
  fail(`${label}: transcript never contained ${JSON.stringify(substr)}`);
}

/** Count occurrences of `substr` in the transcript text. */
async function countInTranscript(page, substr) {
  return page.evaluate((want) => {
    const text = document.querySelector('#transcript, .transcript, main')?.textContent || document.body.textContent || '';
    return text.split(want).length - 1;
  }, substr);
}

/** Poll until `substr` appears at least `minCount` times. */
async function waitForTranscriptCount(page, substr, minCount, label, timeoutMs = 25000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if ((await countInTranscript(page, substr)) >= minCount) return;
    await sleep(150);
  }
  fail(`${label}: transcript never contained ${minCount}x ${JSON.stringify(substr)}`);
}

/** Poll the toast region for a toast containing `substr`. */
async function waitForToast(page, substr, label, timeoutMs = 15000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const hit = await page.evaluate((want) => {
      const texts = Array.from(document.querySelectorAll('#toasts .toast')).map((el) => el.textContent || '');
      return texts.some((t) => t.includes(want));
    }, substr);
    if (hit) return;
    await sleep(100);
  }
  fail(`${label}: no toast containing ${JSON.stringify(substr)}`);
}

/** Open the command picker and wait for its popover. */
async function openPicker(page) {
  await page.click('#command-btn');
  await waitFor(page, () => document.querySelector('.command-picker__popover') !== null, 'command picker popover did not open');
}

/** Names (with leading '/') currently shown in the picker list. */
async function pickerNames(page) {
  return page.evaluate(() =>
    Array.from(document.querySelectorAll('.command-picker__option .command-picker__name')).map((el) => el.textContent.trim())
  );
}

/** Wait until the picker renders the option `/${name}`, then click it. */
async function chooseCommand(page, name) {
  await waitFor(
    page,
    (want) => Array.from(document.querySelectorAll('.command-picker__option')).some((li) =>
      li.querySelector('.command-picker__name')?.textContent.trim() === `/${want}`
    ),
    `command picker option /${name} did not appear`,
    25000,
    name
  );
  const clicked = await page.evaluate((want) => {
    const opt = Array.from(document.querySelectorAll('.command-picker__option')).find((li) =>
      li.querySelector('.command-picker__name')?.textContent.trim() === `/${want}`
    );
    if (!opt) return false;
    opt.click();
    return true;
  }, name);
  if (!clicked) fail(`command picker option /${name} not found`);
}

/** Fill the composer with a typed command and press Enter (real dispatch).
 *  Returns the new summary bubble text matching `expected`. */
async function runCommand(page, text, expected, label) {
  const summaryCount = await page.evaluate(() => document.querySelectorAll('.msg--summary').length);
  const matches = expected instanceof RegExp
    ? expected
    : new RegExp(expected.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'));
  await page.fill('#prompt-input', text);
  await page.press('#prompt-input', 'Enter');
  const summary = await waitForSummary(page, summaryCount, matches, label);
  // The intercepted command never leaves optimistic residue in the composer.
  const value = await page.evaluate(() => document.getElementById('prompt-input')?.value || '');
  if (value.trim() !== '') fail(`${label}: composer not cleared after dispatch (value=${JSON.stringify(value)})`);
  return summary;
}

/** Count of outgoing WS frames whose type is a loop, goal, or process RPC
 *  (the intercepted composer command surface: loop_*, goal_*, process_list).
 *  The /ps draft, /ps extra rejection, and the streaming guard must not add
 *  any frame; bare /ps adds exactly one process_list frame. */
function commandFrameCount() {
  return sentFrames.filter((payload) => {
    try {
      const parsed = JSON.parse(payload);
      return parsed && typeof parsed.type === 'string' && /^(loop_|goal_|process_)/.test(parsed.type);
    } catch {
      return false;
    }
  }).length;
}

let sentFrames = [];

async function main() {
  if (!url) fail('RPI_URL is required');
  const browser = await chromium.launch(chromePath ? { executablePath: chromePath } : {});
  try {
    const page = await browser.newPage();
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    page.on('pageerror', (err) => {
      console.error(`web-loop-goal: page error: ${err.message}`);
    });

    // Capture outgoing WS frames so the dispatched RPC types + sessionId
    // stamping can be asserted deterministically.
    page.on('websocket', (ws) => {
      ws.on('framesent', (frame) => {
        const payload = typeof frame.payload === 'string' ? frame.payload : '';
        if (!payload) return;
        sentFrames.push(payload);
      });
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(page, () => document.getElementById('conn-state') !== null, 'conn-state missing');
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'WS did not reach "connected"'
    );

    const userBubbles = () => page.evaluate(() => document.querySelectorAll('.msg--user').length);
    const summaryBubbles = () => page.evaluate(() => document.querySelectorAll('.msg--summary').length);
    const initialUser = await userBubbles();
    const initialSummary = await summaryBubbles();
    const initialFrames = await commandFrameCount();

    /* ---------------- picker: /loop + /goal + /ps from the backend catalog ---------------- */
    await openPicker(page);
    await waitFor(
      page,
      () => {
        const names = Array.from(document.querySelectorAll('.command-picker__option .command-picker__name')).map((el) => el.textContent.trim());
        return names.includes('/loop') && names.includes('/goal') && names.includes('/ps');
      },
      'command picker did not list /loop + /goal + /ps'
    );
    const names = await pickerNames(page);
    for (const required of ['/loop', '/goal', '/ps', '/compact', '/skill', '/code-review']) {
      if (!names.includes(required)) fail(`command picker missing ${required} (got ${names.join(', ')})`);
    }
    record('lg.picker-lists-loop-goal');
    record('lg.ps-picker-listed');
    await page.screenshot({ path: `${evidence}/loop-goal-picker-open.png`, fullPage: true });

    /* ---------------- draft-only selection: no auto-submit ---------------- */
    await chooseCommand(page, 'loop');
    await waitFor(
      page,
      () => (document.getElementById('prompt-input')?.value || '') === '/loop ',
      'choosing /loop did not draft "/loop " (trailing space from requiresArguments)'
    );
    await page.screenshot({ path: `${evidence}/loop-goal-draft-loop.png`, fullPage: true });
    if ((await userBubbles()) !== initialUser) fail('choosing /loop auto-submitted (user bubble appeared)');
    if ((await summaryBubbles()) !== initialSummary) fail('choosing /loop auto-submitted (summary bubble appeared)');
    record('lg.loop-draft-no-submit');

    await page.fill('#prompt-input', '');
    await openPicker(page);
    await chooseCommand(page, 'goal');
    await waitFor(
      page,
      () => (document.getElementById('prompt-input')?.value || '') === '/goal',
      'choosing /goal did not draft "/goal" (bare)'
    );
    await page.screenshot({ path: `${evidence}/loop-goal-draft-goal.png`, fullPage: true });
    if ((await userBubbles()) !== initialUser) fail('choosing /goal auto-submitted (user bubble appeared)');
    if ((await summaryBubbles()) !== initialSummary) fail('choosing /goal auto-submitted (summary bubble appeared)');
    record('lg.goal-draft-no-submit');
    await page.fill('#prompt-input', '');

    /* ---------------- draft-only selection: /ps drafts bare, no submit ---------------- */
    // ps (requiresArguments=false) drafts exactly "/ps" — no trailing space —
    // and never auto-submits (no user bubble, no summary bubble, no RPC).
    await openPicker(page);
    await chooseCommand(page, 'ps');
    await waitFor(
      page,
      () => (document.getElementById('prompt-input')?.value || '') === '/ps',
      'choosing /ps did not draft "/ps" (bare, requiresArguments=false)'
    );
    await page.screenshot({ path: `${evidence}/loop-goal-draft-ps.png`, fullPage: true });
    if ((await userBubbles()) !== initialUser) fail('choosing /ps auto-submitted (user bubble appeared)');
    if ((await summaryBubbles()) !== initialSummary) fail('choosing /ps auto-submitted (summary bubble appeared)');
    if ((await commandFrameCount()) !== initialFrames) fail('choosing /ps dispatched an RPC');
    record('lg.ps-draft-no-submit');
    await page.fill('#prompt-input', '');

    /* ---------------- bare /ps: process_list -> empty list summary bubble ---------------- */
    // The fresh fixture owns no supervised processes, so the real listener
    // resolves `[]` and the bounded TUI-parity formatter renders the exact
    // "No supervised processes" marker. Exactly one process_list frame must
    // leave the client, stamped with the active sessionId.
    const framesBeforePs = await commandFrameCount();
    const psBubble = await runCommand(page, '/ps', 'No supervised processes', 'ps empty list bubble');
    if ((await userBubbles()) !== initialUser) fail('bare /ps created a user bubble');
    if ((await commandFrameCount()) !== framesBeforePs + 1) {
      fail(`bare /ps must dispatch exactly one process_list frame (frames ${framesBeforePs} -> ${await commandFrameCount()})`);
    }
    record('lg.ps-empty-list');
    await page.screenshot({ path: `${evidence}/loop-goal-ps-empty.png`, fullPage: true });

    /* ---------------- bare /goal opens the Goal panel (TUI parity) ---------------- */
    // The TUI maps a BARE /goal to its goal panel; the Web composer does the
    // same (explicit /goal show|get|inspect dispatch the goal_get RPC).
    await page.fill('#prompt-input', '/goal');
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, () => document.getElementById('goal-panel') !== null, 'bare /goal did not open the Goal panel');
    const panelState = await page.evaluate(() => ({
      hasGoal: document.getElementById('goal-panel')?.getAttribute('data-has-goal'),
      composer: document.getElementById('prompt-input')?.value || '',
    }));
    if (panelState.hasGoal !== 'false') fail(`bare /goal panel should show the no-goal create form (data-has-goal=${panelState.hasGoal})`);
    if (panelState.composer !== '') fail(`bare /goal accepted dispatch must clear the composer (value=${JSON.stringify(panelState.composer)})`);
    if ((await userBubbles()) !== initialUser) fail('bare /goal created a user bubble');
    record('lg.goal-panel-open');
    await page.screenshot({ path: `${evidence}/loop-goal-goal-panel-open.png`, fullPage: true });
    await page.click('#goal-close-btn');
    await waitFor(page, () => document.getElementById('goal-panel') === null, 'Goal panel did not close');

    /* ---------------- /loop: create (fire streams on the mock) ---------------- */
    const created = await runCommand(
      page,
      '/loop create 1h e2e loop probe',
      'every 1 hour',
      'loop create bubble'
    );
    const idMatch = created.match(/scheduled (\S+)/);
    if (!idMatch) fail(`loop create bubble missing task id (text=${JSON.stringify(created)})`);
    const loopId = idMatch[1];
    // The TUI create line is `scheduled {id} · {schedule} · expires {ts}` —
    // no prompt (the prompt is asserted in the list row below).
    record('lg.loop-create');
    await page.screenshot({ path: `${evidence}/loop-goal-loop-create.png`, fullPage: true });

    /* ---------------- TUI parity guard: update rejected while streaming ---------------- */
    // The immediate fire streams the mock's slow first request ("steer-1-…");
    // once its first chunk is visible the session is streaming, and /loop
    // update must be rejected LOCALLY with the TUI-equivalent message and NO
    // RPC (mirrors the TUI loop dispatch guard).
    await waitForTranscriptText(page, 'steer-', 'loop fire stream visible');
    const framesBeforeGuard = await commandFrameCount();
    await page.fill('#prompt-input', `/loop update ${loopId} 10s probe updated`);
    await page.press('#prompt-input', 'Enter');
    const guardToastSeen = await (async () => {
      const deadline = Date.now() + 4000;
      while (Date.now() < deadline) {
        const hit = await page.evaluate((want) => {
          const texts = Array.from(document.querySelectorAll('#toasts .toast')).map((el) => el.textContent || '');
          return texts.some((t) => t.includes(want));
        }, '/loop is unavailable while another turn is running');
        if (hit) return true;
        await sleep(100);
      }
      return false;
    })();
    if (!guardToastSeen) {
      // Diagnostic dump for a guard bypass (never a silent failure).
      const diag = await page.evaluate(() => ({
        composer: document.getElementById('prompt-input')?.value || '',
        summaries: Array.from(document.querySelectorAll('.msg--summary')).map((el) => el.textContent || ''),
        assistants: Array.from(document.querySelectorAll('.msg--assistant')).length,
        bodyTail: (document.body.textContent || '').slice(-400),
      }));
      const frameTypes = sentFrames
        .map((p) => { try { return JSON.parse(p).type; } catch { return null; } })
        .filter((t) => t && /^(loop_|goal_)/.test(t));
      fail(`loop streaming guard bypassed: no guard toast (composer=${JSON.stringify(diag.composer)} summaries=${JSON.stringify(diag.summaries)} assistants=${diag.assistants} frames=${JSON.stringify(frameTypes)} bodyTail=${JSON.stringify(diag.bodyTail)})`);
    }
    await sleep(400);
    if ((await commandFrameCount()) !== framesBeforeGuard) {
      fail('/loop update while streaming dispatched an RPC (guard bypassed)');
    }
    record('lg.loop-streaming-guard');

    // Let the loop fire's slow stream finish before the goal ops so the goal
    // work turns run deterministically (request #2 instant, request #3 slow —
    // their mock replies are known, no queued-behind-fire races).
    await waitForTranscriptText(page, 'chunk-four-done', 'loop fire stream completed');

    /* ---------------- /goal operations (session idle at create) ---------------- */
    // create with activate:true mirrors TUI /goal create: the goal work turn
    // starts immediately (gate free), so the bubble carries the activation
    // prefix + the chained goal_get state line.
    const goalCreated = await runCommand(
      page,
      '/goal create --tokens 42 ship the web goal e2e',
      'ship the web goal e2e',
      'goal create bubble'
    );
    if (!/Goal work (started|queued|already active)/.test(goalCreated)) {
      fail(`goal create bubble missing activation prefix (text=${JSON.stringify(goalCreated)})`);
    }
    if (!/active · \d+\/42 tokens · ship the web goal e2e/.test(goalCreated)) {
      fail(`goal create bubble missing state line (text=${JSON.stringify(goalCreated)})`);
    }
    record('lg.goal-create-activate');
    await page.screenshot({ path: `${evidence}/loop-goal-goal-create.png`, fullPage: true });
    await runCommand(page, '/goal show', 'ship the web goal e2e', 'goal show bubble');
    record('lg.goal-show');
    await runCommand(page, '/goal pin stay focused', 'ship the web goal e2e', 'goal pin bubble');
    record('lg.goal-pin');
    // The pins listing renders through MarkdownBody: "1. stay focused" is an
    // ordered list, so the DOM text of the row is "stay focused" (the "1. "
    // becomes the <ol> marker). Assert the rendered row text — uniquely the
    // pins bubble, since the pin bubble is a state line without the pin text.
    await runCommand(page, '/goal pins', 'stay focused', 'goal pins bubble');
    const pinsMarkup = await page.evaluate(() => {
      const summaries = Array.from(document.querySelectorAll('.msg--summary'));
      const pins = summaries.find((el) => (el.textContent || '').includes('stay focused'));
      if (!pins) return null;
      const items = Array.from(pins.querySelectorAll('ol > li')).map((li) => li.textContent || '');
      return { items, listCount: pins.querySelectorAll('ol').length };
    });
    if (!pinsMarkup || pinsMarkup.items.length !== 1 || pinsMarkup.items[0] !== 'stay focused') {
      fail(`goal pins bubble must render an ordered list row (got ${JSON.stringify(pinsMarkup)})`);
    }
    record('lg.goal-pins');
    await runCommand(page, '/goal pause', 'paused (manually paused)', 'goal pause bubble');
    record('lg.goal-pause');
    const goalResumed = await runCommand(
      page,
      '/goal resume',
      /Goal work (started|queued|already active) · active · \d+\/42 tokens · ship the web goal e2e/,
      'goal resume bubble'
    );
    if (!/Goal work (started|queued|already active)/.test(goalResumed)) {
      fail(`goal resume bubble missing activation prefix (text=${JSON.stringify(goalResumed)})`);
    }
    record('lg.goal-resume-activate');
    await runCommand(page, '/goal complete', /completed · \d+\/42 tokens · ship the web goal e2e/, 'goal complete bubble');
    record('lg.goal-complete');
    // Drop after complete is an invalid transition -> actionable RPC error.
    await runCommand(page, '/goal drop', 'cannot drop a goal', 'goal drop error bubble');
    await waitForToast(page, 'cannot drop a goal', 'goal drop error toast');
    record('lg.goal-drop-error');
    await page.screenshot({ path: `${evidence}/loop-goal-goal-drop-error.png`, fullPage: true });

    /* ---------------- /loop ops once every turn has settled ---------------- */
    // Both goal-work turns must finish: the create's work replies instantly
    // ("steering-followup-reply") and the resume's work streams slowly
    // (a second "chunk-four-done"). With a 1h cadence no further loop fires
    // occur, so the session is idle and update is allowed again.
    await waitForTranscriptCount(page, 'chunk-four-done', 2, 'goal work stream completed');
    await waitForTranscriptText(page, 'steering-followup-reply', 'goal work turn completed');
    await sleep(700);
    const listed = await runCommand(page, '/loop list', `${loopId}  every 1 hour`, 'loop list bubble');
    if (!listed.includes('e2e loop probe')) fail(`loop list bubble missing prompt (text=${JSON.stringify(listed)})`);
    record('lg.loop-list');
    const updated = await runCommand(
      page,
      `/loop update ${loopId} 10s probe updated`,
      `updated loop ${loopId}`,
      'loop update bubble'
    );
    if (!updated.includes('every 10 seconds') || !updated.includes('probe updated')) {
      fail(`loop update bubble missing schedule/prompt (text=${JSON.stringify(updated)})`);
    }
    record('lg.loop-update');
    await page.screenshot({ path: `${evidence}/loop-goal-loop-updated.png`, fullPage: true });
    await runCommand(page, `/loop delete ${loopId}`, `deleted loop ${loopId}`, 'loop delete bubble');
    record('lg.loop-delete');
    // Cancel after delete: the backend resolves false -> actionable error.
    await runCommand(
      page,
      `/loop cancel ${loopId}`,
      `no active loop with id ${loopId}`,
      'loop cancel error bubble'
    );
    await waitForToast(page, `no active loop with id ${loopId}`, 'loop cancel error toast');
    record('lg.loop-cancel-error');

    /* ---------------- malformed args: local usage errors, NO RPC, draft kept ---------------- */
    const framesBeforeUsage = await commandFrameCount();
    await page.fill('#prompt-input', '/loop');
    await page.press('#prompt-input', 'Enter');
    await waitForToast(page, 'usage: /loop', 'bare /loop usage toast');
    if ((await userBubbles()) !== initialUser) fail('bare /loop created a user bubble');
    let value = await page.evaluate(() => document.getElementById('prompt-input')?.value || '');
    if (value !== '/loop') fail(`bare /loop must preserve the draft after the usage error (value=${JSON.stringify(value)})`);

    await page.fill('#prompt-input', '/goal unpin nope');
    await page.press('#prompt-input', 'Enter');
    await waitForToast(page, 'usage: /goal unpin <index>', '/goal unpin nope usage toast');
    value = await page.evaluate(() => document.getElementById('prompt-input')?.value || '');
    if (value !== '/goal unpin nope') fail(`/goal unpin nope must preserve the draft (value=${JSON.stringify(value)})`);

    await page.fill('#prompt-input', '/goal pin');
    await page.press('#prompt-input', 'Enter');
    await waitForToast(page, 'usage: /goal pin <text>', '/goal pin usage toast');
    value = await page.evaluate(() => document.getElementById('prompt-input')?.value || '');
    if (value !== '/goal pin') fail(`/goal pin must preserve the draft (value=${JSON.stringify(value)})`);

    // /ps is a bare-only surface: an argument tail is a LOCAL usage error —
    // no process_list frame leaves the client and the draft is preserved.
    await page.fill('#prompt-input', '/ps extra');
    await page.press('#prompt-input', 'Enter');
    await waitForToast(page, 'usage: /ps', '/ps extra usage toast');
    value = await page.evaluate(() => document.getElementById('prompt-input')?.value || '');
    if (value !== '/ps extra') fail(`/ps extra must preserve the draft (value=${JSON.stringify(value)})`);
    if ((await userBubbles()) !== initialUser) fail('/ps extra created a user bubble');
    record('lg.ps-args-local-reject');
    record('lg.draft-preserved-on-error');

    // TUI loop aliases (loops/loop-update/loop-delete/loop-cancel) are NOT
    // wired in the Web composer — they must be intercepted with an actionable
    // error and NEVER fall through as a model prompt.
    await page.fill('#prompt-input', '/loops');
    await page.press('#prompt-input', 'Enter');
    await waitForToast(page, 'alias of /loop: use /loop list', '/loops alias toast');
    value = await page.evaluate(() => document.getElementById('prompt-input')?.value || '');
    if (value !== '/loops') fail(`/loops must preserve the draft (value=${JSON.stringify(value)})`);
    if ((await userBubbles()) !== initialUser) fail('/loops fell through as a prompt (user bubble appeared)');
    record('lg.alias-no-prompt');

    await sleep(500);
    const framesAfterUsage = await commandFrameCount();
    if (framesAfterUsage !== framesBeforeUsage) {
      fail(`malformed args/aliases dispatched RPCs or prompts (frames ${framesBeforeUsage} -> ${framesAfterUsage})`);
    }
    record('lg.usage-error-no-rpc');

    /* ---------------- invariants: no prompt bubbles, sessionId stamping ---------------- */
    if ((await userBubbles()) !== initialUser) {
      fail(`intercepted commands created user bubbles (${initialUser} -> ${await userBubbles()})`);
    }
    record('lg.no-user-bubbles');
    // Every loop_*/goal_* frame carries the same non-empty top-level
    // sessionId as the session's own command frames — the multi-session
    // routing contract (no cross-session routing in a single-session lane).
    const parsedFrames = sentFrames
      .map((payload) => {
        try { return JSON.parse(payload); } catch { return null; }
      })
      .filter((f) => f && typeof f.type === 'string');
    const commandFrames = parsedFrames.filter((f) => /^(loop_|goal_|process_)/.test(f.type));
    const expectedTypes = [
      'loop_create', 'loop_list', 'loop_update', 'loop_delete', 'loop_cancel',
      'goal_create', 'goal_get', 'goal_pin', 'goal_pause', 'goal_resume',
      'goal_complete', 'goal_drop', 'process_list',
    ];
    for (const type of expectedTypes) {
      if (!commandFrames.some((f) => f.type === type)) fail(`no ${type} frame observed (got ${commandFrames.map((f) => f.type).join(',')})`);
    }
    // sessionId stamping: goal create/resume each chain a goal_get, so two
    // goal_get frames are expected (show + the activation state chain).
    const goalGetFrames = commandFrames.filter((f) => f.type === 'goal_get').length;
    if (goalGetFrames < 3) fail(`expected >=3 goal_get frames (show, create chain, resume chain); got ${goalGetFrames}`);
    const sids = commandFrames.map((f) => f.sessionId);
    if (sids.length === 0) fail('no loop_*/goal_* frames observed');
    if (!sids.every((sid) => typeof sid === 'string' && sid.length > 0)) {
      fail(`a loop_*/goal_* frame lacks a sessionId (sids=${JSON.stringify(sids)})`);
    }
    if (!sids.every((sid) => sid === sids[0])) {
      fail(`loop_*/goal_* frames routed to different sessions (sids=${JSON.stringify(sids)})`);
    }
    // The active session's own frames (get_commands etc.) carry the SAME
    // sessionId — the commands were routed to the active session.
    const sessionSids = parsedFrames.filter((f) => f.sessionId != null).map((f) => f.sessionId);
    if (sessionSids.length === 0 || !sids.every((sid) => sessionSids.includes(sid))) {
      fail(`loop_/goal_ frames not routed to the active session (command sids=${JSON.stringify(sids)}, session sids=${JSON.stringify(sessionSids)})`);
    }
    record('lg.session-routed');
    // The process_list frame (bare /ps) must carry the same active-session
    // sessionId as the loop/goal frames — the multi-session routing contract
    // covers the /ps surface too.
    const psFrames = commandFrames.filter((f) => f.type === 'process_list');
    if (psFrames.length === 0) fail('no process_list frame observed for session routing');
    if (!psFrames.every((f) => typeof f.sessionId === 'string' && f.sessionId.length > 0)) {
      fail(`process_list frame lacks a sessionId (sids=${JSON.stringify(psFrames.map((f) => f.sessionId))})`);
    }
    if (!psFrames.every((f) => f.sessionId === sids[0])) {
      fail(`process_list frame routed to a different session (ps sids=${JSON.stringify(psFrames.map((f) => f.sessionId))}, command sids=${JSON.stringify(sids)})`);
    }
    record('lg.ps-session-routed');
    await page.screenshot({ path: `${evidence}/loop-goal-final.png`, fullPage: true });

    // Evidence for the Web coverage matrix: every documented contract must
    // have executed; the file is written only on full success.
    const missingContracts = DOCUMENTED_IDS.filter((id) => !executed.has(id));
    if (missingContracts.length > 0) {
      fail(`documented contracts never executed: ${missingContracts.join(', ')}`);
    }
    fs.mkdirSync(evidence, { recursive: true });
    fs.writeFileSync(
      path.join(evidence, 'coverage-assertions.json'),
      JSON.stringify({ executed: [...executed] }, null, 2),
    );
    console.log(`web-loop-goal: PASSED (${executed.size}/${DOCUMENTED_IDS.length} assertions) — evidence at ${path.join(evidence, 'coverage-assertions.json')}`);
  } finally {
    await browser.close();
  }
}

main().catch((err) => {
  console.error(`web-loop-goal: unexpected error: ${err && err.stack ? err.stack : err}`);
  process.exit(2);
});
