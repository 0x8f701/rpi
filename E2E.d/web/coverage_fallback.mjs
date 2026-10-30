// Fallback coverage matrix driver — closes the zero-hit margin on the panels
// the core steering driver (coverage_test.mjs) does not exhaustively exercise:
// SessionPanel refresh / rename-via-Enter / clone / fork / switch-row;
// GoalPanel unpin; SideChatPanel Enter-prompt / tab switch / tab close;
// redact.redactSecrets credential-shape branches through real panel safeText
// rendering; and the App panel close callbacks (onClose / onClosePanel ->
// setActivePanel('')).
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
import { createFreshSessionViaManage } from './lib/session-lifecycle.mjs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

const executed = new Map(); // id -> exact execution count (per-ID ledger, no Set erasure)
// The documented contract this driver must execute EXACTLY once — IN THE
// ACTUAL ASSERTION-SITE (runtime) ORDER (the matrix requires the published
// executed array to equal the documented array in order).
const DOCUMENTED_IDS = [
  'fallback.session-refresh',
  'fallback.session-rename-enter',
  'fallback.redact-credential-panel',
  'fallback.session-fork',
  'fallback.session-clone',
  'fallback.session-switch-row',
  'fallback.app-close-session',
  'fallback.goal-unpin',
  'fallback.app-close-goal',
  'fallback.sidechat-enter-prompt',
  'fallback.sidechat-tab-switch',
  'fallback.sidechat-tab-close',
  'fallback.app-close-sidechat',
];
function record(id) {
  executed.set(id, (executed.get(id) || 0) + 1);
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
  if (token) {
    await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
  }
  await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
  await waitFor(page, () => document.title === 'rpi web', 'page title missing');
  await waitFor(page, () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true, 'WS did not reach "connected"');
}

// Wait for the main session to be idle (unified action back to Send) — the
// turn-gate rejects a prompt issued while a run is in flight, so every
// priming prompt must only be sent once the previous turn finished.
async function waitForIdle(page, label, timeoutMs = 30000) {
  await waitFor(page, () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message', `${label}: main session still streaming`, timeoutMs);
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
        document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message'
      );
    },
    `${label}: priming turn never completed`,
    40000,
    before
  );
}


// Wait until a fork/clone lifecycle action is OBSERVABLE under the CURRENT
// panel contract — the panel never renders an in-panel status surface:
//   * SUCCESS — the keyed panel REMOUNTS and its "Session id" row changes
//     (the runLifecycle status is set on the unmounted instance; the
//     session-id/active identity change is the observable, mirroring the
//     core driver's session.switch-new-id);
//   * REFUSAL — exactly ONE new bottom-right error toast names the action
//     ("<label> failed: ...") and the panel renders NO status element.
// Either outcome proves onFork/onClone + runLifecycle + the RPC executed.
// `previousId` (panel session id) and `toastsBefore` (toast count) are
// captured by the CALLER strictly BEFORE the action click — a fast refusal
// or cutover must never fall outside the baseline. The helper never
// baselines internally after the action. Returns 'identity' or 'refused'.
async function waitForLifecycleOutcome(page, label, previousId, toastsBefore, timeoutMs = 30000) {
  await waitFor(
    page,
    ({ prevId, prevToasts }) => {
      const dts = [...document.querySelectorAll('#session-panel dl dt')];
      const idx = dts.findIndex((dt) => dt.textContent === 'Session id');
      const id = idx >= 0 ? (dts[idx]?.nextElementSibling?.textContent || '').trim() : '';
      const identityChanged = id !== '' && id !== '—' && id !== prevId;
      const fresh = [...document.querySelectorAll('#toasts .toast')].slice(prevToasts)
        .map((t) => t.textContent || '');
      return identityChanged || fresh.some((t) => t.includes(`${label} failed:`));
    },
    `session lifecycle "${label}" never became observable (identity change or refusal toast)`,
    timeoutMs,
    { prevId: previousId, prevToasts: toastsBefore },
  );
  const outcome = await page.evaluate((prevToasts) => {
    const dts = [...document.querySelectorAll('#session-panel dl dt')];
    const idx = dts.findIndex((dt) => dt.textContent === 'Session id');
    const id = idx >= 0 ? (dts[idx]?.nextElementSibling?.textContent || '').trim() : '';
    return {
      id,
      freshToasts: [...document.querySelectorAll('#toasts .toast')].slice(prevToasts).map((t) => t.textContent || ''),
      panelStatus: document.querySelector('#session-panel .panel__status'),
    };
  }, toastsBefore);
  if (outcome.id !== '' && outcome.id !== '—' && outcome.id !== previousId) return 'identity';
  if (outcome.freshToasts.length !== 1 || !outcome.freshToasts[0].includes(`${label} failed:`)) {
    fail(`session lifecycle "${label}": expected a session-id cutover or exactly one refusal toast (got ${JSON.stringify(outcome.freshToasts)})`);
  }
  if (outcome.panelStatus !== null) {
    fail(`session lifecycle "${label}": panel rendered an in-panel status surface`);
  }
  return 'refused';
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
    // target below). The panel remounts for the fork target, so the
    // session-id/active identity CHANGE is the observable (the panel never
    // renders an in-panel status).
    const forkPreviousId = await readSessionId(page);
    const forkToastsBefore = await page.evaluate(() => document.querySelectorAll('#toasts .toast').length);
    await waitFor(
      page,
      () => {
        // The fork candidate window is the accessible bounded listbox
        // (#session-fork-list): a selectable option row exists and the Fork
        // button is enabled without manual selection (auto-selected first
        // forkable message).
        const list = document.getElementById('session-fork-list');
        const selectable = list
          ? [...list.querySelectorAll('[role="option"]')]
            .some((option) => option.id.startsWith('session-fork-option-')
              && option.getAttribute('aria-disabled') !== 'true')
          : false;
        const active = list?.getAttribute('aria-activedescendant') || '';
        return selectable && active !== '' && !document.getElementById('session-fork-btn')?.disabled;
      },
      'fork listbox never populated with a selectable forkable message'
    );
    await page.click('#session-fork-btn');
    await waitForLifecycleOutcome(page, 'fork', forkPreviousId, forkToastsBefore);
    record('fallback.session-fork');

    // Clone -> onClone -> clone RPC -> onLifecycleResult. clone_from needs the
    // active session's recorder leaf; the fixture's session tree has no
    // selected leaf, so the RPC refuses: the observable is EXACTLY ONE
    // bottom-right error toast naming "clone failed: ..." and ZERO in-panel
    // status — but onClone + runLifecycle + the clone RPC still executed.
    // Should a fixture allow clone, the helper accepts the identity-change
    // outcome instead (never an in-panel status).
    const clonePreviousId = await readSessionId(page);
    const cloneToastsBefore = await page.evaluate(() => document.querySelectorAll('#toasts .toast').length);
    await page.click('#session-clone-btn');
    await waitForLifecycleOutcome(page, 'clone', clonePreviousId, cloneToastsBefore);
    record('fallback.session-clone');
    // Create a second session via new_session (the core driver proves this
    // path works in the fixture; fork/clone above are refused by the fixture's
    // spawner, so new_session is the reliable way to leave a non-current saved
    // row to switch back to). The snapshot switches the active session to the
    // fresh one; the original (renamed above) becomes a non-current saved row
    // with a Switch button. A successful cutover REMOUNTS the keyed panel, so
    // the runLifecycle status is set on the unmounted instance — assert the
    // session-id change instead (mirrors the core driver).
    const { beforeSessionId: originalId } = await createFreshSessionViaManage(
      page,
      waitFor,
      'fallback session lifecycle',
      { closePanel: false, timeoutMs: 30000 },
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
    // Exact per-ID ledger gate BEFORE any write: every documented id must
    // have executed EXACTLY once and nothing undocumented may execute; the
    // published executed array is the documented order (ordered equality).
    const missing = DOCUMENTED_IDS.filter((id) => !executed.has(id));
    if (missing.length > 0) {
      fail(`fallback coverage evidence incomplete: ${missing.join(', ')} never executed (coverage matrix contract)`);
    }
    const overExecuted = [...executed.keys()].filter((id) => !DOCUMENTED_IDS.includes(id));
    if (overExecuted.length > 0) {
      fail(`fallback coverage evidence drift: executed-but-undocumented ids: ${overExecuted.join(', ')}`);
    }
    const repeatedIds = [...executed].filter(([, count]) => count !== 1).map(([id]) => id);
    if (repeatedIds.length > 0) {
      fail(`fallback coverage evidence exact-once violation: ${repeatedIds.join(', ')}`);
    }
    // Publish the REAL ledger: executed is the Map's insertion order (actual
    // execution order — never a reconstruction of DOCUMENTED_IDS) and counts
    // the real per-ID ledger. The gate above proved documented == executed
    // as exact-once sets; the matrix's ordered gate additionally proves the
    // runtime order matches the documented order.
    fs.writeFileSync(path.join(evidence, 'coverage-assertions.json'), JSON.stringify({
      documented: [...DOCUMENTED_IDS],
      executed: [...executed.keys()],
      counts: Object.fromEntries(executed),
    }, null, 2));
    console.log(`web-cov-fallback: PASSED (${executed.size} assertions) — evidence at ${path.join(evidence, 'coverage-assertions.json')}`);
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-cov-fallback: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});