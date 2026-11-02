// Thinking-level persistence web E2E (playwright half of thinking_level.sh):
// TL1 catalog populates/enables; TL2/TL3 an idle or streaming selection sends
// exactly ONE set_thinking_level, succeeds with the effective level and stays
// displayed, and the next prompt runs on the live level; TL4/TL5 per-session
// levels survive switches and reloads restore them via bootstrap get_state.

import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';
import { createFreshSessionViaManage } from './lib/session-lifecycle.mjs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const slowPrefix = process.env.RPI_SLOW_PREFIX || 'steer-1-';
const fastReply = process.env.RPI_FAST_REPLY || 'thinking-followup-reply';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const pageErrors = [];
const consoleErrors = [];

const DOCUMENTED_IDS = [
  'tl.catalog-populated',
  'tl.session-identity',
  'tl.initial-level-displayed',
  'tl.idle-exactly-one-frame',
  'tl.idle-response-effective',
  'tl.idle-authoritative-state',
  'tl.idle-remains-selected',
  'tl.idle-no-error-toast',
  'tl.stream-first-delta',
  'tl.stream-exactly-one-frame',
  'tl.stream-response-effective',
  'tl.stream-remains-selected',
  'tl.stream-no-error-toast',
  'tl.stream-turn-settles',
  'tl.stream-authoritative-state',
  'tl.stream-followup-exactly-one-frame',
  'tl.stream-followup-response-effective',
  'tl.stream-followup-authoritative-state',
  'tl.stream-followup-no-error-toast',
  'tl.next-prompt-roundtrip',
  'tl.next-prompt-live-level',
  'tl.next-prompt-no-error-toast',
  'tl.reload-restores-level',
  'tl.fresh-session-created',
  'tl.fresh-session-level-loaded',
  'tl.session-b-exactly-one-frame',
  'tl.session-b-response-effective',
  'tl.session-b-authoritative-state',
  'tl.switch-back-a-preserved',
  'tl.switch-back-b-preserved',
  'tl.switch-no-error-toast',
  'tl.reload-restores-b',
  'tl.switch-a-after-reload-restored',
  'tl.no-browser-errors',
];
const executed = new Map();
function record(id) {
  executed.set(id, (executed.get(id) || 0) + 1);
}

function fail(message) {
  console.error(`web-thinking-level: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

async function countSetFrames(page, sid, level) {
  return (await parseFrames(page, 'sent'))
    .filter((frame) => frame.type === 'set_thinking_level' && frame.sessionId === sid && frame.level === level)
    .length;
}

async function installSocketHarness(page) {
  await page.addInitScript(() => {
    const NativeWebSocket = window.WebSocket;
    const frames = { sent: [], received: [] };
    class TracedWebSocket extends NativeWebSocket {
      constructor(...args) {
        super(...args);
        this.addEventListener('message', (event) => {
          if (typeof event.data === 'string') frames.received.push(event.data);
        });
      }
      send(data) {
        if (typeof data === 'string') frames.sent.push(data);
        return super.send(data);
      }
    }
    for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
      Object.defineProperty(TracedWebSocket, key, { value: NativeWebSocket[key] });
    }
    window.WebSocket = TracedWebSocket;
    window.__rpiThinkingFrames = frames;
  });
}

function parseFrames(page, kind) {
  return page.evaluate((frameKind) => window.__rpiThinkingFrames[frameKind]
    .map((raw) => { try { return JSON.parse(raw); } catch { return null; } })
    .filter((frame) => frame !== null), kind);
}

async function activeSessionId(page) {
  return page.evaluate(() =>
    document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '');
}

async function waitSelectValue(page, expected, label, timeoutMs = 25000) {
  await waitFor(
    page,
    (want) => {
      const select = document.getElementById('thinking-select');
      if (!select || select.disabled) return false;
      return want === null ? select.value !== '' : select.value === want;
    },
    label,
    timeoutMs,
    expected,
  );
}

async function assertOneNewSetFrame(page, sid, level, baseline, label) {
  await waitFor(
    page,
    ({ target, want, before }) => window.__rpiThinkingFrames.sent
      .map((raw) => { try { return JSON.parse(raw); } catch { return null; } })
      .filter((frame) => frame && frame.type === 'set_thinking_level'
        && frame.sessionId === target && frame.level === want).length === before + 1,
    `${label}: exactly one new set_thinking_level frame`,
    25000,
    { target: sid, want: level, before: baseline },
  );
  const frames = (await parseFrames(page, 'sent'))
    .filter((frame) => frame.type === 'set_thinking_level' && frame.sessionId === sid && frame.level === level);
  if (frames.length !== baseline + 1) {
    fail(`${label}: expected exactly ${baseline + 1} set_thinking_level frame(s) for ${sid}/${level}, saw ${frames.length}`);
  }
  const frame = frames[frames.length - 1];
  if (!frame.id) fail(`${label}: set_thinking_level request id missing: ${JSON.stringify(frame)}`);
  return frame;
}

async function assertSetResponse(page, requestId, expectedLevel, label) {
  await waitFor(
    page,
    ({ want }) => window.__rpiThinkingFrames.received
      .map((raw) => { try { return JSON.parse(raw); } catch { return null; } })
      .some((frame) => frame && frame.type === 'response' && frame.id === want
        && frame.success === true),
    `${label}: correlated set_thinking_level response`,
    25000,
    { want: requestId },
  );
  const frames = (await parseFrames(page, 'received'))
    .filter((frame) => frame.type === 'response' && frame.id === requestId);
  const response = frames[frames.length - 1];
  if (response.command !== 'set_thinking_level') {
    fail(`${label}: response command mismatch: ${JSON.stringify(response)}`);
  }
  if (response.data?.level !== expectedLevel) {
    fail(`${label}: response effective level ${JSON.stringify(response.data?.level)} != ${JSON.stringify(expectedLevel)}`);
  }
}

async function waitAuthoritativeState(page, sid, expectedLevel, label) {
  await waitFor(
    page,
    ({ target, want }) => window.__rpiThinkingFrames.received
      .map((raw) => { try { return JSON.parse(raw); } catch { return null; } })
      .some((frame) => frame && frame.type === 'response' && frame.command === 'get_state'
        && frame.success === true && frame.sessionId === target && frame.data?.thinkingLevel === want),
    `${label}: get_state never reflected thinkingLevel=${expectedLevel}`,
    25000,
    { target: sid, want: expectedLevel },
  );
}

async function assertNoErrorToast(page, label) {
  const bad = await page.evaluate(() =>
    [...document.querySelectorAll('#toasts .toast')].some((t) => t.classList.contains('toast--error')));
  if (bad) fail(`${label}: an error toast appeared during the thinking-level flow`);
}

async function selectLevel(page, level, label) {
  await waitFor(page, () => {
    const select = document.getElementById('thinking-select');
    return select !== null && !select.disabled && select.value !== '';
  }, `${label}: #thinking-select never became selectable`);
  await page.selectOption('#thinking-select', level);
}

async function clickRow(page, sid, label) {
  await waitFor(
    page,
    (target) => {
      const row = [...document.querySelectorAll('.session-sidebar__switch')]
        .find((candidate) => candidate.dataset.sessionId === target);
      return row !== undefined && !row.disabled;
    },
    `${label}: session row ${sid} never became clickable`,
    30000,
    sid
  );
  await page.evaluate((target) => {
    const row = [...document.querySelectorAll('.session-sidebar__switch')]
      .find((candidate) => candidate.dataset.sessionId === target);
    row.click();
  }, sid);
  await waitFor(
    page,
    (target) => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId === target,
    `${label}: session ${sid} never became active`,
    30000,
    sid
  );
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage();
    await installSocketHarness(page);
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    page.on('pageerror', (err) => pageErrors.push(err.message));
    page.on('console', (message) => {
      if (message.type() === 'error') consoleErrors.push(message.text());
    });
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'WS did not reach "connected"'
    );

    await waitFor(
      page,
      () => document.querySelectorAll('#thinking-select option').length >= 2,
      'TL1: #thinking-select never populated',
    );
    const levels = await page.evaluate(() => {
      const select = document.getElementById('thinking-select');
      return select ? [...select.querySelectorAll('option')].map((o) => o.value) : [];
    });
    record('tl.catalog-populated');
    const sidA = await activeSessionId(page);
    if (!sidA) fail('TL1: active session id missing');
    record('tl.session-identity');
    await waitSelectValue(page, null, 'TL1: select never showed the session level');
    record('tl.initial-level-displayed');

    const initialA = await page.inputValue('#thinking-select');
    const targetA = levels.find((l) => l !== initialA && l !== '') ?? '';
    if (!targetA) fail(`TL2: no alternative level to select (options: ${JSON.stringify(levels)})`);
    const baselineIdle = await countSetFrames(page, sidA, targetA);
    await selectLevel(page, targetA, 'TL2');
    const setFrameA = await assertOneNewSetFrame(page, sidA, targetA, baselineIdle, 'TL2');
    record('tl.idle-exactly-one-frame');
    await assertSetResponse(page, setFrameA.id, targetA, 'TL2');
    record('tl.idle-response-effective');
    await waitAuthoritativeState(page, sidA, targetA, 'TL2');
    record('tl.idle-authoritative-state');
    const staysA = await page.inputValue('#thinking-select');
    if (staysA !== targetA) fail(`TL2: select snapped back after the round trip: ${JSON.stringify(staysA)} != ${JSON.stringify(targetA)}`);
    record('tl.idle-remains-selected');
    await assertNoErrorToast(page, 'TL2');
    record('tl.idle-no-error-toast');
    await page.screenshot({ path: `${evidence}/thinking-idle-selected.png`, fullPage: true });

    await page.fill('#prompt-input', 'write a very long answer');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (prefix) => document.body.textContent.includes(prefix),
      'TL3: slow stream never started (first delta missing)',
      30000,
      slowPrefix
    );
    record('tl.stream-first-delta');
    const initialStreaming = await page.inputValue('#thinking-select');
    const targetStreaming = levels.find((l) => l !== initialStreaming && l !== '') ?? '';
    if (!targetStreaming) fail(`TL3: no alternative level to select (options: ${JSON.stringify(levels)})`);
    const baselineStreaming = await countSetFrames(page, sidA, targetStreaming);
    await selectLevel(page, targetStreaming, 'TL3');
    const setFrameStream = await assertOneNewSetFrame(page, sidA, targetStreaming, baselineStreaming, 'TL3');
    record('tl.stream-exactly-one-frame');
    await assertSetResponse(page, setFrameStream.id, targetStreaming, 'TL3');
    record('tl.stream-response-effective');
    const staysStreaming = await page.inputValue('#thinking-select');
    if (staysStreaming !== targetStreaming) fail(`TL3: streaming selection did not stay: ${JSON.stringify(staysStreaming)}`);
    record('tl.stream-remains-selected');
    await assertNoErrorToast(page, 'TL3');
    record('tl.stream-no-error-toast');
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message',
      'TL3: streaming turn never settled after the mid-stream selection',
      40000
    );
    record('tl.stream-turn-settles');
    await waitAuthoritativeState(page, sidA, targetStreaming, 'TL3');
    record('tl.stream-authoritative-state');
    const baselineAfter = await countSetFrames(page, sidA, targetA);
    await selectLevel(page, targetA, 'TL3');
    const frameAfter = await assertOneNewSetFrame(page, sidA, targetA, baselineAfter, 'TL3');
    record('tl.stream-followup-exactly-one-frame');
    await assertSetResponse(page, frameAfter.id, targetA, 'TL3');
    record('tl.stream-followup-response-effective');
    await waitAuthoritativeState(page, sidA, targetA, 'TL3');
    record('tl.stream-followup-authoritative-state');
    await assertNoErrorToast(page, 'TL3');
    record('tl.stream-followup-no-error-toast');
    await page.screenshot({ path: `${evidence}/thinking-stream-selected.png`, fullPage: true });

    const levelA = await page.inputValue('#thinking-select');
    await page.fill('#prompt-input', 'next turn after selection');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (reply) => document.body.textContent.includes(reply),
      'TL3: next prompt never round-tripped with the selected level',
      30000,
      fastReply
    );
    record('tl.next-prompt-roundtrip');
    await waitSelectValue(page, levelA, 'TL3: next prompt changed the selected level');
    record('tl.next-prompt-live-level');
    await assertNoErrorToast(page, 'TL3');
    record('tl.next-prompt-no-error-toast');

    await page.reload({ waitUntil: 'domcontentloaded' });
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'TL5: WS did not reconnect after reload'
    );
    await waitSelectValue(page, levelA, 'TL5: reload never restored the persisted level');
    record('tl.reload-restores-level');

    const created = await createFreshSessionViaManage(page, waitFor, 'TL4', { closePanel: true });
    const sidB = created.sessionId;
    if (!sidB || sidB === sidA) fail(`TL4: fresh session identity invalid: ${JSON.stringify(created)}`);
    record('tl.fresh-session-created');
    await waitSelectValue(page, null, 'TL4: fresh session B select never loaded a level');
    record('tl.fresh-session-level-loaded');
    const initialB = await page.inputValue('#thinking-select');
    const targetB = levels.find((l) => l !== initialB && l !== '') ?? '';
    if (!targetB) fail('TL4: no level available for session B');
    const baselineB = await countSetFrames(page, sidB, targetB);
    await selectLevel(page, targetB, 'TL4');
    const setFrameB = await assertOneNewSetFrame(page, sidB, targetB, baselineB, 'TL4');
    record('tl.session-b-exactly-one-frame');
    await assertSetResponse(page, setFrameB.id, targetB, 'TL4');
    record('tl.session-b-response-effective');
    await waitAuthoritativeState(page, sidB, targetB, 'TL4');
    record('tl.session-b-authoritative-state');
    await clickRow(page, sidA, 'TL4: switch back to A');
    await waitSelectValue(page, levelA, 'TL4: A level not preserved across the exact-ready switch');
    record('tl.switch-back-a-preserved');
    await clickRow(page, sidB, 'TL4: switch back to B');
    await waitSelectValue(page, targetB, 'TL4: B level not preserved on re-activation');
    record('tl.switch-back-b-preserved');
    await assertNoErrorToast(page, 'TL4');
    record('tl.switch-no-error-toast');
    await page.screenshot({ path: `${evidence}/thinking-per-session.png`, fullPage: true });

    await page.reload({ waitUntil: 'domcontentloaded' });
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'TL5: WS did not reconnect after the second reload'
    );
    await waitSelectValue(page, targetB, 'TL5: reload never restored the persisted B level');
    record('tl.reload-restores-b');
    await clickRow(page, sidA, 'TL5: switch to A after reload');
    await waitSelectValue(page, levelA, 'TL5: A level not restored from the backend after reload');
    record('tl.switch-a-after-reload-restored');

    if (pageErrors.length || consoleErrors.length) {
      fail(`browser errors: ${JSON.stringify({ pageErrors, consoleErrors })}`);
    }
    record('tl.no-browser-errors');

    const missingContracts = DOCUMENTED_IDS.filter((id) => !executed.has(id));
    if (missingContracts.length > 0) {
      fail(`documented contracts never executed: ${missingContracts.join(', ')}`);
    }
    const overExecuted = [...executed.keys()].filter((id) => !DOCUMENTED_IDS.includes(id));
    if (overExecuted.length > 0) {
      fail(`executed-but-undocumented contract ids: ${overExecuted.join(', ')}`);
    }
    const repeatedIds = [...executed].filter(([, count]) => count !== 1).map(([id]) => id);
    if (repeatedIds.length > 0) {
      fail(`exact-once violation (duplicate execution): ${repeatedIds.join(', ')}`);
    }
    await page.screenshot({ path: `${evidence}/thinking-restored.png`, fullPage: true });
    fs.writeFileSync(
      path.join(evidence, 'coverage-assertions.json'),
      JSON.stringify({
        documented: [...DOCUMENTED_IDS],
        executed: [...executed.keys()],
        counts: Object.fromEntries(executed),
      }, null, 2),
    );
    console.log(`web-thinking-level: PASSED (${executed.size}/${DOCUMENTED_IDS.length} assertions) — evidence at ${path.join(evidence, 'coverage-assertions.json')}`);
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-thinking-level: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});