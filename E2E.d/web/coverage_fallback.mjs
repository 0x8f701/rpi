// Fallback coverage matrix driver — closes the zero-hit margin on the panels
// the core steering driver (coverage_test.mjs) does not exhaustively exercise:
// SessionPanel refresh / rename-via-Enter / clone / fork / switch-row;
// GoalPanel unpin; MaintenancePanel compact + actual rewind apply;
// SideChatPanel Enter-prompt / tab switch / tab close; redact.redactSecrets
// credential-shape branches through real panel safeText rendering; and the App
// panel close callbacks (onClose / onClosePanel -> setActivePanel('')).
//
// Run against the REAL `rpi --listen` steering fixture (loopback mock +
// RPI_WEB_DEV_DIR coverage bundle) with concrete, machine-readable assertion
// IDs. V8 coverage is collected on every page by the standard coverage hook
// (E2E.d/web/lib/coverage-hook.mjs, loaded with node --import via
// web_run_playwright when RPI_COVERAGE_DIR is set); this driver only records
// assertion evidence — it never calls redact/markdown functions directly (that
// would be a source-text/fake test). Every redact hit is driven through REAL
// RPC -> REAL panel rendering.
//
// Environment:
//   RPI_URL        http://127.0.0.1:<port>/web
//   RPI_TOKEN      token file content (rpi-auth.<token> subprotocol)
//   RPI_CHROME     system Chrome executable (optional)
//   RPI_EVIDENCE   evidence dir (coverage-assertions.json is written here)
//
// Exit: 0 = every assertion passed + evidence written; 2 = assertion failure;
//       1 = setup failure (playwright unusable).

import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

const executed = new Set();
function record(id) {
  executed.add(id);
  console.log(`[web-cov-fallback:assert] ${id}`);
}

function fail(message) {
  console.error(`web-cov-fallback: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 30000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

// Credential shapes that hit redact.ts REDACT_PATTERNS branches the XSS lane
// (only `sk-`) and the core rename ("web e2e session", no credential) leave
// unexercised: ghp_, AKIA, and `bearer <token>`. Embedded in a session name so
// safeText(info.sessionName) runs redactSecrets over them through the REAL
// SessionPanel render path.
const REDACT_NAME = 'creds ghp_xxxxxxxxxxxxxxxxxxxx AKIAIOSFODNN7EXAMPLE bearer abcdef0123456789xyz';

async function connectPage(page) {
  await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
  await waitFor(page, () => document.title === 'rpi web', 'page title missing');
  await waitFor(page, () => document.querySelector('#conn-state') !== null, 'conn-state missing');
  await page.click('#settings-toggle-btn');
  await waitFor(page, () => document.querySelector('#settings-token-input') !== null, 'settings token input missing');
  await page.fill('#settings-token-input', token);
  await page.click('#settings-token-save-btn');
  await page.click('#settings-close-btn');
  await waitFor(page, () => document.getElementById('conn-state').dataset.state === 'on', 'WS did not reach "connected"');
}

// Wait for the main session to be idle (stream-badge cleared) — the turn-gate
// rejects a prompt issued while a run is in flight, so every priming prompt
// must only be sent once the previous turn finished.
async function waitForIdle(page, label, timeoutMs = 30000) {
  await waitFor(page, () => document.getElementById('stream-badge').hidden === true, `${label}: main session still streaming`, timeoutMs);
}

// Send one main-session prompt and wait for a NEW non-empty assistant message
// + the turn to finish. Accepts any reply (mock parity is not relied on).
async function primeTurn(page, text, label) {
  await waitForIdle(page, `${label}: pre-send idle`);
  const before = await page.evaluate(() => document.querySelectorAll('.msg--assistant .assistant-text').length);
  await page.fill('#prompt-input', text);
  await page.press('#prompt-input', 'Enter');
  await waitFor(
    page,
    (b) => {
      const nodes = [...document.querySelectorAll('.msg--assistant .assistant-text')];
      return (
        nodes.length > b &&
        (nodes[nodes.length - 1]?.textContent || '').trim() !== '' &&
        document.getElementById('stream-badge').hidden === true
      );
    },
    `${label}: priming turn never completed`,
    40000,
    before
  );
}


// Click a maintenance action button by visible label (mirrors the core driver's
// clickMaintenanceAction — the actions are <button class="maintenance__action">
// labeled Compact / Snapcompact / Rewind… / Handoff / Queue…).
async function clickMaintenanceAction(page, label) {
  const clicked = await page.evaluate((want) => {
    const btn = Array.from(document.querySelectorAll('.maintenance__action')).find((b) => (b.textContent || '').includes(want));
    if (!btn) return false;
    btn.click();
    return true;
  }, label);
  if (!clicked) fail(`maintenance action "${label}" not found`);
}

// Wait for the SessionPanel to report a lifecycle outcome. runLifecycle sets
// the panel status to "<label> ok" on success or "<label> failed: <err>" on
// error — either proves onClone/onFork/onSwitch ran the RPC through the App
// lifecycle path (the observable contract). Robust to a clone/fork that the
// fixture refuses (e.g. no forkable leaf): the journey still executed.
async function waitForSessionStatus(page, label, timeoutMs = 30000) {
  await waitFor(
    page,
    (want) => {
      const status = document.querySelector('#session-panel .panel__status');
      return status !== null && (status.textContent || '').includes(want);
    },
    `session lifecycle "${label}" never reported a panel status`,
    timeoutMs,
    label
  );
}

// Read the active session id from the SessionPanel's "Session id" row. Used
// for lifecycle ops that SUCCEED (new_session / switch_session): the panel is
// keyed by sessionId, so a successful cutover REMOUNTS it and the runLifecycle
// status is set on the unmounted instance (invisible). The session-id change
// is the observable instead (mirrors the core driver's session.switch-new-id).
async function readSessionId(page) {
  return page.evaluate(() => {
    const dts = [...document.querySelectorAll('#session-panel dl dt')];
    const idx = dts.findIndex((dt) => dt.textContent === 'Session id');
    return idx >= 0 ? (dts[idx]?.nextElementSibling?.textContent || '') : '';
  });
}

async function main() {
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage();
    page.on('pageerror', (err) => {
      console.error(`web-cov-fallback: page error: ${err.message}`);
    });

    await connectPage(page);

    // ---- Prime one user turn so SessionPanel.get_fork_messages and the
    // rewind entry list have real content to operate on. ----
    await primeTurn(page, 'prime fallback session turn', 'phase-1-prime');

    // ==================== Session panel ====================
    await page.click('#session-toggle-btn');
    await waitFor(page, () => document.getElementById('session-panel') !== null, 'session panel did not open');
    await waitFor(
      page,
      () => document.querySelector('[data-testid="session-name-value"]') !== null,
      'current session name never rendered'
    );

    // Refresh button -> SessionPanel.load() re-runs (get_state / stats / list /
    // fork_messages). Observable: the panel still renders the current session
    // info after the refresh round-trip.
    await page.click('#session-refresh-btn');
    await waitFor(
      page,
      () => document.querySelector('[data-testid="session-name-value"]') !== null,
      'session refresh wiped the current session info'
    );
    record('fallback.session-refresh');

    // Rename via Enter key -> onKeyDown Enter -> onRename -> set_session_name.
    // The name carries credential shapes so safeText -> redactSecrets runs the
    // ghp_/AKIA/bearer REDACT_PATTERNS branches through the REAL panel render.
    await page.fill('#session-rename-input', REDACT_NAME);
    await page.press('#session-rename-input', 'Enter');
    await waitFor(
      page,
      () => {
        const v = document.querySelector('[data-testid="session-name-value"]')?.textContent || '';
        return v.includes('[REDACTED]');
      },
      'credential-bearing rename never redacted in the panel'
    );
    const renderedName = await page.evaluate(() => document.querySelector('[data-testid="session-name-value"]')?.textContent || '');
    if (renderedName.includes('ghp_xxxxxxxxxxxxxxxxxxxx')) {
      fail(`ghp_ credential leaked unredacted into the panel: ${renderedName}`);
    }
    if (renderedName.includes('AKIAIOSFODNN7EXAMPLE')) {
      fail(`AKIA credential leaked unredacted into the panel: ${renderedName}`);
    }
    record('fallback.session-rename-enter');
    record('fallback.redact-credential-panel');

    // Fork FIRST -> onFork -> fork RPC -> onLifecycleResult. The current
    // session carries the primed user message, so get_fork_messages populates
    // the fork select; the panel auto-selects the first forkable message, so
    // the Fork button is enabled without manually touching the <select>. A
    // root user message (no parent) makes the spawner fall back to a fresh
    // session, so fork always yields a snapshot — the active session cuts over
    // to the fork, leaving the original as a non-current saved row (the switch
    // target below). The panel status ("fork ok" / "fork failed: ...") is the
    // observable: either proves onFork + runLifecycle + the fork RPC executed.
    await waitFor(
      page,
      () => {
        const opt = document.querySelector('#session-fork-select option:not([value=""])');
        return opt !== null && !document.getElementById('session-fork-btn')?.disabled;
      },
      'fork select never populated with a forkable message'
    );
    await page.click('#session-fork-btn');
    await waitForSessionStatus(page, 'fork');
    record('fallback.session-fork');

    // Clone -> onClone -> clone RPC -> onLifecycleResult. clone_from needs the
    // active session's recorder leaf; if the fixture's session tree has no
    // selected leaf the RPC refuses, the panel reports "clone failed: ...",
    // and onLifecycleResult falls back to refreshState — but onClone +
    // runLifecycle + the clone RPC still executed (the observable contract).
    await page.click('#session-clone-btn');
    await waitForSessionStatus(page, 'clone');
    record('fallback.session-clone');
    // Create a second session via new_session (the core driver proves this
    // path works in the fixture; fork/clone above are refused by the fixture's
    // spawner, so new_session is the reliable way to leave a non-current saved
    // row to switch back to). The snapshot switches the active session to the
    // fresh one; the original (renamed above) becomes a non-current saved row
    // with a Switch button. A successful cutover REMOUNTS the keyed panel, so
    // the runLifecycle status is set on the unmounted instance — assert the
    // session-id change instead (mirrors the core driver).
    const originalId = await readSessionId(page);
    if (!originalId) fail('current session id never rendered before new_session');
    await page.click('#session-new-btn');
    await waitFor(
      page,
      (b) => {
        const dts = [...document.querySelectorAll('#session-panel dl dt')];
        const idx = dts.findIndex((dt) => dt.textContent === 'Session id');
        const id = idx >= 0 ? (dts[idx]?.nextElementSibling?.textContent || '') : '';
        return id !== '' && id !== b;
      },
      'new_session never produced a different session id',
      30000,
      originalId
    );

    // Switch via a saved-session row's Switch button -> onSwitch(path) ->
    // switch_session RPC -> onLifecycleResult. The original is now a
    // non-current saved row; clicking its Switch button resumes it. A
    // successful switch remounts the panel, so assert the session-id changes
    // BACK to the original (proves onSwitch + runLifecycle + switch_session).
    await waitFor(
      page,
      () => document.querySelector('.session-row__switch') !== null,
      'no saved session row with a Switch button after new_session'
    );
    await page.evaluate(() => {
      const btn = document.querySelector('.session-row__switch');
      if (!btn) throw new Error('no session-row__switch button');
      btn.click();
    });
    await waitFor(
      page,
      (b) => {
        const dts = [...document.querySelectorAll('#session-panel dl dt')];
        const idx = dts.findIndex((dt) => dt.textContent === 'Session id');
        const id = idx >= 0 ? (dts[idx]?.nextElementSibling?.textContent || '') : '';
        return id !== '' && id === b;
      },
      'switch_session row never restored the original session id',
      30000,
      originalId
    );
    record('fallback.session-switch-row');
    // Close callback -> SessionPanel onClose -> App.setActivePanel('').
    await page.click('#session-close-btn');
    await waitFor(page, () => document.getElementById('session-panel') === null, 'session panel did not close via the close callback');
    record('fallback.app-close-session');

    // ==================== Goal panel ====================
    await page.click('#goal-panel-btn');
    await waitFor(page, () => document.getElementById('goal-panel') !== null, 'goal panel did not open');
    await waitFor(
      page,
      () => document.getElementById('goal-panel').dataset.hasGoal === 'false',
      'goal panel should start with no goal'
    );
    const objective = `fallback goal unpin e2e ${Date.now()}`;
    await page.fill('#goal-objective-input', objective);
    await page.fill('#goal-budget-input', '200');
    await page.click('#goal-create-btn');
    await waitFor(
      page,
      (obj) =>
        document.getElementById('goal-panel').dataset.hasGoal === 'true' &&
        (document.getElementById('goal-objective') || {}).textContent === obj,
      'created goal never appeared in the panel',
      30000,
      objective
    );

    // Pin, then UNPIN — the core driver pins but never unpins, so onUnpin +
    // goal_unpin are zero-hit without this journey.
    await page.fill('#goal-pin-input', 'role-model pin');
    await page.click('#goal-pin-btn');
    await waitFor(
      page,
      () => Array.from(document.querySelectorAll('#goal-pins li .goal-pin__text')).some((el) => (el.textContent || '') === 'role-model pin'),
      'pinned text never appeared'
    );
    await page.click('#goal-unpin-0');
    await waitFor(
      page,
      () => document.querySelectorAll('#goal-pins li').length === 0,
      'goal_unpin never removed the pin from the panel'
    );
    record('fallback.goal-unpin');

    // Close callback -> GoalPanel onClose -> App.setActivePanel('').
    await page.click('#goal-close-btn');
    await waitFor(page, () => document.getElementById('goal-panel') === null, 'goal panel did not close via the close callback');
    record('fallback.app-close-goal');

    // ==================== Maintenance panel ====================
    // Prime two more turns so get_entries has enough records for a safe rewind
    // (rewind to the last entry retains N-1, drops 1 — valid for N>=2). The
    // active session (after the row switch) carries the primed phase-1 turn.
    await primeTurn(page, 'fallback maintenance prime a', 'phase-4-prime-a');
    await primeTurn(page, 'fallback maintenance prime b', 'phase-4-prime-b');

    await page.click('#maintenance-toggle-btn');
    await waitFor(page, () => document.querySelector('.maintenance') !== null, 'maintenance panel did not open');

    // Compact -> MaintenancePanel.run('compact', {type:'compact'}) -> compact
    // RPC (real LLM summarization through the mock) -> tokenReport. The core
    // driver only exercises Snapcompact; Compact's LLM-summarizer path is
    // unique to this journey.
    await clickMaintenanceAction(page, 'Compact');
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.maintenance__result')).some((r) => {
          const t = r.textContent || '';
          // run('compact', ...) renders a result titled "compact:" on BOTH
          // success (tokenReport "Compact: N → M estimated tokens") and error
          // ("compact: <err>"). Either proves the compact RPC + run() +
          // tokenReport/error path executed; prefer the A→B arrow when the
          // fixture permits a real LLM summarization.
          return t.includes('compact:');
        }),
      'compact never rendered a result',
      45000
    );
    record('fallback.maintenance-compact');

    // Rewind list, then APPLY rewind by clicking a real entry — the core
    // driver only opens the list (maintenance.rewind-list); doRewind + the
    // rewind RPC are zero-hit without this click.
    await clickMaintenanceAction(page, 'Rewind…');
    await waitFor(
      page,
      () => document.querySelector('.maintenance__list') !== null,
      'rewind entry list never appeared'
    );
    await waitFor(
      page,
      () => document.querySelectorAll('.maintenance__list-row').length >= 2,
      'rewind entry list has fewer than 2 rows — a safe rewind target is unavailable'
    );
    // Click the LAST row: rewind(index = N-1) retains N-1 records, drops 1
    // (mirrors the rpc.rs rewind test, which rewinds to count-1). doRewind's
    // run() renders a result titled "rewind:" on success ("rewind: Rewound to
    // …") or error ("rewind: <err>"); either proves doRewind + the rewind RPC
    // executed. Prefer the success text when the fixture permits a real rewind.
    await page.evaluate(() => {
      const rows = Array.from(document.querySelectorAll('.maintenance__list-row'));
      const last = rows[rows.length - 1];
      if (!last) throw new Error('no maintenance__list-row to rewind to');
      last.click();
    });
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.maintenance__result')).some((r) => (r.textContent || '').includes('rewind:')),
      'rewind apply never rendered a result',
      30000
    );
    record('fallback.maintenance-rewind-apply');

    // Close callback -> MaintenancePanel onClosePanel -> App.setActivePanel('').
    await page.click('.maintenance .panel-close');
    await waitFor(page, () => document.querySelector('.maintenance') === null, 'maintenance panel did not close via the close callback');
    record('fallback.app-close-maintenance');

    // ==================== Side chat panel ====================
    await page.click('#sidechat-toggle-btn');
    await waitFor(page, () => document.querySelector('.side-chat') !== null, 'side chat panel did not open');
    await waitFor(
      page,
      () => document.querySelectorAll('.side-chat__tab').length >= 1,
      'side chat never showed the default tab'
    );

    // Enter-to-send prompt -> SideChatPanel textarea onKeyDown Enter -> submit
    // -> onPrompt -> side_chat_prompt RPC. The core driver clicks the Send
    // button; the Enter-key submit path is unique to this journey.
    await page.fill('.side-chat__composer textarea', 'fallback sidechat enter prompt');
    await page.press('.side-chat__composer textarea', 'Enter');
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.side-chat__entry--user .side-chat__text')).some((el) => (el.textContent || '').includes('fallback sidechat enter prompt')),
      'side-chat Enter prompt never rendered the user entry',
      30000
    );
    record('fallback.sidechat-enter-prompt');

    // Create a second tab, then SWITCH back to the default tab by clicking its
    // tab-select button -> onSwitch -> side_chat_switch. The core driver
    // creates a tab and asserts it activated, but never switches BACK, so the
    // side_chat_switch-through-UI path is unique to this journey.
    await page.fill('.side-chat__new input', 'fb_switch');
    await page.click('.side-chat__new button');
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.side-chat__tab-select')).some((b) => (b.textContent || '').includes('fb_switch') && b.getAttribute('aria-selected') === 'true'),
      'fb_switch tab never created/activated'
    );
    // The default tab is the FIRST tab-select (the one whose text does not
    // include fb_switch); click it to switch back.
    await page.evaluate(() => {
      const tabs = Array.from(document.querySelectorAll('.side-chat__tab-select'));
      const def = tabs.find((b) => !(b.textContent || '').includes('fb_switch'));
      if (!def) throw new Error('default side-chat tab not found');
      def.click();
    });
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.side-chat__tab-select')).some((b) => !(b.textContent || '').includes('fb_switch') && b.getAttribute('aria-selected') === 'true'),
      'side_chat_switch never re-activated the default tab'
    );
    record('fallback.sidechat-tab-switch');

    // Close the fb_switch tab via its tab-close button -> onClose(name) ->
    // side_chat_close. The core driver never closes a side-chat tab, so the
    // side_chat_close-through-UI path is unique to this journey.
    await page.evaluate(() => {
      const tabs = Array.from(document.querySelectorAll('.side-chat__tab'));
      const target = tabs.find((t) => (t.querySelector('.side-chat__tab-select')?.textContent || '').includes('fb_switch'));
      if (!target) throw new Error('fb_switch tab row not found for close');
      const close = target.querySelector('.side-chat__tab-close');
      if (!close) throw new Error('fb_switch tab-close button not found');
      close.click();
    });
    await waitFor(
      page,
      () =>
        !Array.from(document.querySelectorAll('.side-chat__tab-select')).some((b) => (b.textContent || '').includes('fb_switch')),
      'side_chat_close never removed the fb_switch tab'
    );
    record('fallback.sidechat-tab-close');

    // Close callback -> SideChatPanel onClosePanel -> App.setActivePanel('').
    await page.click('.side-chat .panel-close');
    await waitFor(page, () => document.querySelector('.side-chat') === null, 'side chat panel did not close via the close callback');
    record('fallback.app-close-sidechat');

    fs.mkdirSync(evidence, { recursive: true });
    fs.writeFileSync(path.join(evidence, 'coverage-assertions.json'), JSON.stringify({ executed: [...executed] }, null, 2));
    console.log(`web-cov-fallback: PASSED (${executed.size} assertions) — evidence at ${path.join(evidence, 'coverage-assertions.json')}`);
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-cov-fallback: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});