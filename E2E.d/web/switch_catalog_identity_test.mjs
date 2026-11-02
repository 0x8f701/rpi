// Negotiated catalog-identity switch E2E lane (playwright half of
// E2E.d/web/switch_catalog_identity.sh): three 1500-message sessions,
// one correlated switch_session command + one post-response subscribe +
// one authoritative full cut per COLD leg, trusted-click milestones,
// bounded per-surface demand window, header label removal.
import fs from 'node:fs';

import { chromium } from 'playwright';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

const SESSION_A = 'switch-catalog-a';
const SESSION_B = 'switch-catalog-b';
const SESSION_C = 'switch-catalog-c';
const TAIL_A = `${SESSION_A}-tail`;
const TAIL_B = `${SESSION_B}-tail`;
const TAIL_C = `${SESSION_C}-tail`;
const SEEDED_MESSAGES = 1500;
const TRANSCRIPT_WINDOW_MAX = 240;
const MAX_SWITCH_MS = 1000;

function switchDurations(timing) {
  return {
    activeMs: Math.round(timing.activeAt - timing.clickAt),
    transcriptMs: Math.round(timing.transcriptAt - timing.clickAt),
    readyMs: Math.round(timing.readyAt - timing.clickAt),
    pendingClearMs: Math.round(timing.pendingClearAt - timing.clickAt),
  };
}

function fail(message) {
  console.error(`web-switch-catalog-identity: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 30000, arg) {

  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

// Take the evidence screenshot AND prove it actually landed on disk.
async function assertScreenshot(page, path, label) {
  await page.screenshot({ path, fullPage: true });
  if (!fs.existsSync(path) || fs.statSync(path).size === 0) {
    fail(`${label}: evidence screenshot was not written: ${path}`);
  }
}

async function activeSessionId(page) {
  return page.evaluate(
    () =>
      document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset
        .sessionId || ''
  );
}

async function assertHeaderHasNoSessionLabel(page, label) {
  const state = await page.evaluate(() => ({
    sessionNameElement: document.getElementById('session-name'),
    visibleSessionLabel: [...document.querySelectorAll('header *')].some(
      (node) =>
        (node.textContent || '').trim().toLowerCase().startsWith('session:') &&
        node.children.length === 0
    ),
  }));
  if (state.sessionNameElement !== null || state.visibleSessionLabel) {
    fail(`${label}: header rendered a session label (${JSON.stringify(state)})`);
  }
}

async function waitForIdle(page, label, timeoutMs = 30000) {
  await waitFor(
    page,
    // The unified composer action is the production streaming gate: idle
    // turns it back to Send. Missing element fails the gate — never passes
    // silently.
    () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message',
    `${label}: composer never returned to the idle Send action`,
    timeoutMs
  );
}

async function promptAndWait(page, prompt, reply, label, timeoutMs = 30000) {
  await waitForIdle(page, label, timeoutMs);
  await page.fill('#prompt-input', prompt);
  await page.press('#prompt-input', 'Enter');
  await waitFor(page, (r) => document.body.textContent.includes(r), label, timeoutMs, reply);
}

async function waitForClickableRow(page, sid, label) {
  await waitFor(
    page,
    (target) => {
      const row = [...document.querySelectorAll('.session-sidebar__switch')].find(
        (candidate) => candidate.dataset.sessionId === target
      );
      return row !== undefined && !row.disabled;
    },
    `${label}: session ${sid} row never became clickable`,
    30000,
    sid
  );
}

function parsedFrames(entries) {
  return entries.map(({ order, direction, gen, url: socketUrl, raw }) => {
    try {
      return { order, direction, gen, socketUrl, ...JSON.parse(raw) };
    } catch {
      return { order, direction, gen, socketUrl, invalid: true, raw };
    }
  });
}

// One correlated cold leg: exactly ONE switch_session frame; with `bootstrap` the source is DERIVED from the boot authority.
async function assertColdSwitchLeg(page, socketFrames, baseline, sourceSessionId, targetSessionId, label, bootstrap = false) {
  const sent = parsedFrames(socketFrames.sent.slice(baseline.sent));
  const received = parsedFrames(socketFrames.received.slice(baseline.received));
  const switchCommands = sent.filter((frame) => frame.type === 'switch_session');
  if (switchCommands.length !== 1) {
    fail(`${label}: expected exactly one switch_session frame in the leg, got ${switchCommands.length}`);
  }
  const command = switchCommands[0];
  if (command.catalogSessionId !== targetSessionId) {
    fail(`${label}: switch_session targets ${JSON.stringify(command.catalogSessionId)}, expected ${targetSessionId}`);
  }
  if (typeof command.sessionId !== 'string' || command.sessionId === '') {
    fail(`${label}: switch command sessionId must be a nonempty source, got ${JSON.stringify(command.sessionId)}`);
  }
  if (bootstrap) {
    // Exact boot authority: the owner-less get_state (id present, no session
    // owner) and its single id-correlated get_state response bind the primary.
    const bootStateCommands = sent.filter(
      (frame) => frame.type === 'get_state' && typeof frame.id === 'string' && frame.id !== ''
        && !('sessionId' in frame) && frame.order < command.order
    );
    if (bootStateCommands.length !== 1) {
      fail(`${label}: expected exactly one owner-less get_state discovering the boot authority, got ${bootStateCommands.length}`);
    }
    const bindCandidates = received.filter(
      (frame) => frame.type === 'response' && frame.id === bootStateCommands[0].id
    );
    if (bindCandidates.length !== 1) {
      fail(`${label}: expected exactly one response to the boot get_state id (success or failure), got ${bindCandidates.length}`);
    }
    const bindResponse = bindCandidates[0];
    if (bindResponse.command !== 'get_state' || bindResponse.success !== true
      || bindResponse.gen !== bootStateCommands[0].gen || bindResponse.gen !== command.gen
      || typeof bindResponse.data?.sessionId !== 'string' || bindResponse.data.sessionId === ''
      || typeof bindResponse.sessionId !== 'string' || bindResponse.sessionId !== bindResponse.data.sessionId) {
      fail(`${label}: the boot get_state response did not bind a boot authority primary`);
    }
    sourceSessionId = bindResponse.data.sessionId;
  }
  if (typeof sourceSessionId !== 'string' || sourceSessionId === '') {
    fail(`${label}: expected source must be the exact known boot/command authority session id`);
  }
  if (command.sessionId !== sourceSessionId) {
    fail(`${label}: switch command sessionId ${JSON.stringify(command.sessionId)} !== expected source ${sourceSessionId}`);
  }
  // Path authority: the causally LAST source-owned same-gen session_list
  // RESPONSE completed before the switch; in-flight later requests are ignored.
  const completedLists = parsedFrames(socketFrames.received).filter(
    (frame) => frame.type === 'response' && frame.command === 'session_list'
      && frame.success === true && frame.order < command.order
      && frame.gen === command.gen
      && typeof frame.sessionId === 'string' && frame.sessionId === sourceSessionId
      && Array.isArray(frame.data?.sessions)
  );
  const pathBySessionId = {};
  if (completedLists.length === 0) {
    fail(`${label}: no source-owned session_list response completed before the switch command`);
  }
  const listResponse = completedLists[completedLists.length - 1];
  const listCommand = parsedFrames(socketFrames.sent).find(
    (frame) => frame.type === 'session_list' && frame.id === listResponse.id
      && typeof frame.sessionId === 'string' && frame.sessionId === sourceSessionId
      && frame.gen === listResponse.gen && frame.order < listResponse.order
  );
  if (!listCommand) {
    fail(`${label}: the authoritative session_list response has no source-owned same-gen request`);
  }
  for (const row of listResponse.data.sessions) {
    if (row && typeof row.sessionId === 'string' && row.sessionId !== '' && typeof row.path === 'string' && row.path !== '') {
      pathBySessionId[row.sessionId] = row.path;
    }
  }
  if (command.responseMode !== 'subscribe-handoff-v1') {
    fail(`${label}: switch_session responseMode missing: ${JSON.stringify(command.responseMode)}`);
  }
  const expectedPath = pathBySessionId[targetSessionId];
  if (typeof expectedPath !== 'string' || expectedPath === '') {
    fail(`${label}: no authoritative session_list row bound a path for ${targetSessionId}`);
  }
  if (command.sessionPath !== expectedPath) {
    fail(`${label}: switch_session sessionPath ${JSON.stringify(command.sessionPath)} !== authoritative target row path ${JSON.stringify(expectedPath)}`);
  }
  if (command.catalogSource !== 'rpi') {
    fail(`${label}: catalogSource should be "rpi", got ${JSON.stringify(command.catalogSource)}`);
  }
  if (command.catalogStatus !== 'native') {
    fail(`${label}: catalogStatus should be "native", got ${JSON.stringify(command.catalogStatus)}`);
  }
  if ('runtimeIncarnation' in command || 'catalogRuntimeIncarnation' in command) {
    fail(`${label}: switch_session must not carry an incarnation field (advisory hint only)`);
  }
  const responses = received.filter(
    (frame) => frame.type === 'response' && frame.id === command.id
  );
  if (responses.length !== 1) {
    fail(`${label}: expected exactly one correlated response (success or failure), got ${responses.length}`);
  }
  const response = responses[0];
  if (response.command !== 'switch_session' || response.success !== true) {
    fail(`${label}: the single correlated response must be the exact successful switch_session response`);
  }
  if (response.sessionId !== command.sessionId || response.data?.sessionId !== targetSessionId) {
    fail(
      `${label}: response owner/target mismatch: envelope ${JSON.stringify(
        response.sessionId
      )} payload ${JSON.stringify(response.data?.sessionId)}`
    );
  }
  if ('sessionPath' in (response.data ?? {}) && response.data.sessionPath !== expectedPath) {
    fail(`${label}: handoff response sessionPath ${JSON.stringify(response.data.sessionPath)} !== authoritative target row path`);
  }
  const subscribes = sent.filter((frame) => frame.type === 'subscribe');
  const targetSubscribes = subscribes.filter(
    (frame) => frame.sessionId === targetSessionId
      && typeof frame.runtimeIncarnation === 'string'
      && frame.runtimeIncarnation !== ''
  );
  let bootSubscribe = null;
  if (bootstrap) {
    const sourceSubscribes = subscribes.filter(
      (frame) => frame.sessionId === sourceSessionId
        && frame.cursor === 0
        && typeof frame.runtimeIncarnation === 'string'
        && frame.runtimeIncarnation !== ''
        && frame.order < command.order
    );
    if (sourceSubscribes.length !== 1) {
      fail(`${label}: expected exactly one boot-authority subscribe (cursor 0, nonempty incarnation) before the switch command, got ${sourceSubscribes.length}`);
    }
    bootSubscribe = sourceSubscribes[0];
  }
  const straySubscribes = subscribes.filter(
    (frame) => frame !== targetSubscribes[0] && frame !== bootSubscribe
  );
  if (targetSubscribes.length !== 1 || straySubscribes.length > 0) {
    fail(
      `${label}: expected exactly one target subscribe and no stray subscribes, got ${
        targetSubscribes.length
      } target + ${straySubscribes.length} stray (${JSON.stringify(subscribes.map((frame) => frame.sessionId))})`
    );
  }
  const subscribe = targetSubscribes[0];
  if (subscribe.order <= response.order) {
    fail(`${label}: subscribe must occur after the handoff response`);
  }
  if (subscribe.runtimeIncarnation !== response.data?.runtimeIncarnation || typeof subscribe.runtimeIncarnation !== 'string' || subscribe.runtimeIncarnation === '') {
    fail(`${label}: subscribe incarnation must match the handoff response payload`);
  }
  if (subscribe.cursor !== 0) {
    fail(`${label}: cold subscribe cursor must be exactly 0, got ${JSON.stringify(subscribe.cursor)}`);
  }
  if (response.data?.cursor !== 0 || response.data?.cursor !== subscribe.cursor) {
    fail(`${label}: cold handoff cursor must be exactly 0 and equal the subscribe cursor`);
  }
  const allCuts = received.filter(
    (frame) => frame.type === 'subscription_snapshot' || frame.type === 'session_resync'
  );
  const targetCuts = allCuts.filter((frame) => frame.sessionId === targetSessionId);
  if (targetCuts.some((frame) => frame.order <= subscribe.order)) {
    fail(`${label}: subscription cut for ${targetSessionId} arrived before/without the subscribe`);
  }
  let bootCut = null;
  if (bootstrap) {
    const sourceCuts = allCuts.filter(
      (frame) => frame.sessionId === sourceSessionId && frame.order < command.order
    );
    if (sourceCuts.length !== 1) {
      fail(`${label}: expected exactly one boot-authority cut before the switch command, got ${sourceCuts.length}`);
    }
    bootCut = sourceCuts[0];
    if (bootCut.reason !== 'cold-full' || !Array.isArray(bootCut.messages) || bootCut.messages.length !== 0) {
      fail(`${label}: boot-authority cut must be a cold-full snapshot carrying exactly 0 messages, got ${JSON.stringify(bootCut.messages?.length)}`);
    }
    if (bootCut.sessionId !== bootSubscribe.sessionId
      || bootCut.runtimeIncarnation !== bootSubscribe.runtimeIncarnation
      || bootCut.watermark !== bootSubscribe.cursor) {
      fail(`${label}: boot-authority cut identity/watermark must match its cursor-0 subscribe`);
    }
    if (bootCut.order <= bootSubscribe.order) {
      fail(`${label}: boot-authority cut must arrive after its subscribe`);
    }
  }
  const strayCuts = allCuts.filter(
    (frame) => frame !== targetCuts[0] && frame !== bootCut
  );
  if (targetCuts.length !== 1 || strayCuts.length > 0) {
    fail(
      `${label}: expected exactly one authoritative cut for ${targetSessionId} and no stray cuts, got ${
        targetCuts.length
      } target + ${strayCuts.length} stray (${JSON.stringify(allCuts.map((frame) => `${frame.type}:${frame.sessionId}`))})`
    );
  }
  const handoff = targetCuts[0] ?? {};
  if (handoff.reason !== 'cold-full' || !Array.isArray(handoff.messages) || handoff.messages.length !== SEEDED_MESSAGES) {
    fail(`${label}: cold cut must be a single cold-full snapshot carrying exactly ${SEEDED_MESSAGES} messages, got ${JSON.stringify(handoff.messages?.length)}`);
  }
  if (handoff.sessionId !== targetSessionId || handoff.runtimeIncarnation !== subscribe.runtimeIncarnation) {
    fail(`${label}: cut runtime identity must match the subscribed target runtime`);
  }
  if (handoff.watermark !== subscribe.cursor) {
    fail(`${label}: cut watermark ${JSON.stringify(handoff.watermark)} must equal the subscribe cursor ${subscribe.cursor}`);
  }
  const handoffFieldsOk = response.data !== null && typeof response.data === 'object'
    && response.data.sessionId === targetSessionId
    && response.data.runtimeIncarnation === subscribe.runtimeIncarnation
    && response.data.catalogSource === 'rpi'
    && response.data.catalogSessionId === targetSessionId
    && response.data.catalogStatus === 'native'
    && response.data.subscriptionRequired === true
    && response.data.lifecycleResponseVersion === 1
    && response.data.cursor === 0
    && response.data.cursor === subscribe.cursor;
  if (!handoffFieldsOk) {
    fail(`${label}: handoff response fields (source/target/catalog tuple/cursor/incarnation) drifted`);
  }
  const genSet = new Set([command.gen, response.gen, subscribe.gen, handoff.gen]);
  if (bootstrap) genSet.add(bootSubscribe.gen), genSet.add(bootCut.gen);
  if (genSet.size !== 1) {
    fail(
      `${label}: command/response/subscribe/cut crossed WebSocket generations: ${
        JSON.stringify([...genSet])
      }`
    );
  }
  return {
    switchSessionFrames: switchCommands.length,
    subscribeFrames: targetSubscribes.length,
    authoritativeCuts: targetCuts.length,
    cutMessages: handoff.messages.length,
    socketGeneration: command.gen,
  };
}

function assertSubsecond(label, timing) {
  if (timing.clickAt === null || !Number.isFinite(timing.clickAt)) {
    fail(`${label}: no trusted pointerdown was observed for the click`);
  }
  if (timing.untrustedClick === true) {
    fail(`${label}: a click landed on the row without a preceding trusted pointerdown`);
  }
  for (const [milestone, at] of Object.entries({
    activeAt: timing.activeAt,
    transcriptAt: timing.transcriptAt,
    readyAt: timing.readyAt,
    pendingClearAt: timing.pendingClearAt,
  })) {
    if (at === null) fail(`${label}: ${milestone} was never observed`);
    const ms = at - timing.clickAt;
    if (!Number.isFinite(ms) || ms < 0) {
      fail(`${label}: ${milestone} produced a non-finite or negative duration (${String(ms)})`);
    }
    if (ms >= MAX_SWITCH_MS) {
      fail(`${label}: ${milestone} completed in ${Math.round(ms)}ms (>= ${MAX_SWITCH_MS}ms hard gate)`);
    }
  }
  // Milestone order: click <= {active, transcript} <= ready <= pendingClear.
  const readyFloor = Math.max(timing.activeAt, timing.transcriptAt);
  if (timing.readyAt < readyFloor) {
    fail(`${label}: ready (${Math.round(timing.readyAt - timing.clickAt)}ms) fired before active/transcript settled`);
  }
  if (timing.pendingClearAt < timing.readyAt) {
    fail(`${label}: pending cleared before authoritative ready`);
  }
}

// In-page recorder installed BEFORE the trusted click, keyed to the target
function installSwitchTimingRecorder({ sid, tail }) {
  const RECORDER_TIMEOUT_MS = 3000;
  const marker = () => document.getElementById('session-lifecycle')?.dataset || {};
  const windowBounds = (surface) => {
    if (!surface) return null;
    const items = [...surface.querySelectorAll('[data-item-id]')];
    const first = items[0] ?? null;
    const last = items[items.length - 1] ?? null;
    return {
      count: items.length,
      firstId: first?.getAttribute('data-item-id') || '',
      lastId: last?.getAttribute('data-item-id') || '',
      firstText: (first?.textContent || '').slice(0, 48),
      lastText: (last?.textContent || '').slice(0, 48),
    };
  };
  const sample = () => {
    const data = marker();
    const surface = document.querySelector('#transcript .transcript-surface:not([hidden])');
    return {
      active:
        document.querySelector('.session-sidebar__row--active .session-sidebar__switch')
          ?.dataset.sessionId || '',
      transcriptOwned: surface?.textContent?.includes(tail) === true,
      switchingMarker: document.querySelector('.session-sidebar__row--switching') !== null,
      markerDisplayed: data.displayedSessionId || '',
      pendingGeneration: data.pendingGeneration ?? null,
      authoritativeReady: data.authoritativeReady === 'true',
      window: windowBounds(surface),
      scrollTop: Math.round(document.getElementById('transcript')?.scrollTop ?? -1),
    };
  };
  const recorder = {
    clickAt: null,
    untrustedClick: false,
    sync: null,
    firstFrame: null,
    activeAt: null,
    transcriptAt: null,
    readyAt: null,
    pendingClearAt: null,
    settled: false,
    timedOut: false,
    // Bounded observed-state trail (relative ms since the trusted pointerdown)
    // so a milestone that never completes is diagnosable from the sequence the
    // page actually displayed — the state flip that never happened is visible
    // as the attribute that stays constant to the end of the trail.
    samples: [],
  };
  window.__rpiSwitchTiming = recorder;
  const complete = () =>
    recorder.activeAt !== null
    && recorder.transcriptAt !== null
    && recorder.readyAt !== null
    && recorder.pendingClearAt !== null;
  const record = (now) => {
    if (recorder.clickAt === null) return;
    const s = sample();
    if (recorder.samples.length < 400) {
      recorder.samples.push({ at: Math.round(now - recorder.clickAt), ...s });
    }
    if (recorder.activeAt === null && s.active === sid) recorder.activeAt = now;
    if (recorder.transcriptAt === null && s.transcriptOwned) recorder.transcriptAt = now;
    if (recorder.readyAt === null && s.authoritativeReady && s.markerDisplayed === sid) {
      recorder.readyAt = now;
    }
    if (recorder.pendingClearAt === null && s.pendingGeneration === '' && s.active === sid) {
      recorder.pendingClearAt = now;
    }
  };
  const observer = new MutationObserver(() => {
    if (recorder.clickAt === null) return;
    const now = performance.now();
    if (recorder.sync === null) recorder.sync = { ...sample(), at: now };
    record(now);
    if (complete()) {
      recorder.settled = true;
      observer.disconnect();
    }
  });
  observer.observe(document.documentElement, {
    childList: true,
    subtree: true,
    attributes: true,
    characterData: true,
  });
  document.addEventListener(
    'pointerdown',
    (event) => {
      if (event.isTrusted !== true) return;
      const row = event.target instanceof Element ? event.target.closest('.session-sidebar__switch') : null;
      if (row && row.dataset.sessionId === sid && recorder.clickAt === null) {
        recorder.clickAt = performance.now();
      }
    },
    { capture: true, once: true }
  );
  let started = false;
  document.addEventListener(
    'click',
    (event) => {
      const row = event.target instanceof Element ? event.target.closest('.session-sidebar__switch') : null;
      if (started || !row || row.dataset.sessionId !== sid) return;
      started = true;
      if (recorder.clickAt === null || event.isTrusted !== true) {
        recorder.untrustedClick = true;
        recorder.settled = true;
        return;
      }
      window.requestAnimationFrame(function step(now) {
        if (recorder.firstFrame === null) recorder.firstFrame = { ...sample(), at: now };
        record(now);
        if (complete()) {
          recorder.settled = true;
          observer.disconnect();
          return;
        }
        if (now - recorder.clickAt > RECORDER_TIMEOUT_MS) {
          recorder.timedOut = true;
          recorder.settled = true;
          observer.disconnect();
          return;
        }
        window.requestAnimationFrame(step);
      });
    },
    { capture: true, once: true }
  );
}

// Runtime evidence on a milestone timeout: the recorder's observed-state
// trail (which attribute never flipped), the first-frame/sync snapshots,
// per-milestone latches, the final lifecycle-marker dataset, both mounted
// transcript surfaces' visibility + tail ownership, the active/switching
// rows, and every wire frame sent after the trusted pointerdown (fast vs
// negotiated discriminator). The contract is unchanged — this only makes the
// deterministic failure reportable.
async function measureSwitchTiming(page, sid, tail, label, socketFrames) {
  await page.evaluate(installSwitchTimingRecorder, { sid, tail });
  const baseline = {
    sent: socketFrames.sent.length,
    received: socketFrames.received.length,
  };
  await page.click(`.session-sidebar__switch[data-session-id="${sid}"]`, { timeout: 20000 });
  await waitFor(
    page,
    () => window.__rpiSwitchTiming?.settled === true,
    `${label}: switch recorder never settled`,
    10000
  );
  const timing = await page.evaluate(() => JSON.parse(JSON.stringify(window.__rpiSwitchTiming)));
  if (timing.timedOut) {
    const dom = await page.evaluate((marker) => {
      const data = document.getElementById('session-lifecycle')?.dataset || {};
      return {
        lifecycle: {
          displayed: data.displayedSessionId || '',
          command: data.commandSessionId || '',
          pendingGeneration: data.pendingGeneration ?? '',
          pendingPhase: data.pendingPhase ?? '',
          authoritativeReady: data.authoritativeReady === 'true',
          stateMessages: data.stateMessages ?? '',
          projectedDom: data.projectedDom ?? '',
        },
        activeRow: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
        switchingRow: document.querySelector('.session-sidebar__row--switching .session-sidebar__switch')?.dataset.sessionId || '',
        surfaces: [...document.querySelectorAll('#transcript .transcript-surface')].map((surface) => {
          const items = [...surface.querySelectorAll('[data-item-id]')];
          const first = items[0] ?? null;
          const last = items[items.length - 1] ?? null;
          return {
            rk: surface.dataset.runtimeKey || '',
            hidden: surface.hidden,
            inert: surface.hasAttribute('inert'),
            hasTail: (surface.textContent || '').includes(marker),
            itemCount: items.length,
            firstText: (first?.textContent || '').slice(0, 48),
            lastText: (last?.textContent || '').slice(0, 48),
          };
        }),
        tailOnVisible: document.querySelector('#transcript .transcript-surface:not([hidden])')?.textContent?.includes(marker) === true,
        scroll: (() => {
          const container = document.getElementById('transcript');
          return container ? { top: Math.round(container.scrollTop), height: container.scrollHeight, client: container.clientHeight } : null;
        })(),
        toasts: (window.__rpiSwitchIdentityTestToasts || []).length,
      };
    }, String(tail ?? ''));
    const report = {
      label,
      timing: {
        clickAt: timing.clickAt,
        untrustedClick: timing.untrustedClick,
        activeAt: timing.activeAt,
        transcriptAt: timing.transcriptAt,
        readyAt: timing.readyAt,
        pendingClearAt: timing.pendingClearAt,
        sync: timing.sync,
        firstFrame: timing.firstFrame,
      },
      trail: timing.samples || [],
      dom,
      wire: {
        sentTypes: parsedFrames(socketFrames.sent.slice(baseline.sent)).map((frame) => frame.type),
        receivedTypes: parsedFrames(socketFrames.received.slice(baseline.received)).map((frame) => frame.type),
      },
    };
    // Persist the full dump before failing: a truncated console line must
    // never lose the evidence for the deterministic retention failure.
    try {
      fs.mkdirSync(evidence, { recursive: true });
      fs.writeFileSync(`${evidence}/switch-catalog-milestone-timeout.json`, JSON.stringify(report, null, 2));
    } catch {
      /* evidence write is best-effort; the failure message still carries it */
    }
    fail(
      `${label}: switch milestones never completed within 3000ms; timing=${JSON.stringify(report.timing)} trailTail=${JSON.stringify((timing.samples || []).slice(-12))} dom=${JSON.stringify(dom)} wire=${JSON.stringify(report.wire)}`
    );
  }
  assertSubsecond(label, timing);
  return timing;
}

async function readDisplayedSurfaceState(page) {
  return page.evaluate(() => {
    const data = document.getElementById('session-lifecycle')?.dataset || {};
    const surface = document.querySelector('#transcript .transcript-surface:not([hidden])');
    return {
      stateMessages: Number(data.stateMessages ?? '0'),
      projectedDom: Number(data.projectedDom ?? '0'),
      domItems: surface ? surface.querySelectorAll('[data-item-id]').length : -1,
      displayed: data.displayedSessionId || '',
      commandSessionId: data.commandSessionId || '',
      pendingGeneration: data.pendingGeneration ?? '',
      pendingPhase: data.pendingPhase ?? '',
      ready: data.authoritativeReady === 'true',
    };
  });
}

// Exact ready-marker gate: the displayed session owns command authority, the
async function assertFullStateBoundedDom(page, label, sid) {
  const state = await readDisplayedSurfaceState(page);
  if (state.displayed !== (sid ?? state.displayed) || state.displayed === '') {
    fail(`${label}: marker displayedSessionId is ${JSON.stringify(state.displayed)}`);
  }
  if (state.commandSessionId !== state.displayed) {
    fail(`${label}: commandSessionId ${JSON.stringify(state.commandSessionId)} != displayed ${JSON.stringify(state.displayed)}`);
  }
  if (state.pendingGeneration !== '' || state.pendingPhase !== '') {
    fail(`${label}: pending not empty at ready: generation ${JSON.stringify(state.pendingGeneration)} phase ${JSON.stringify(state.pendingPhase)}`);
  }
  if (!state.ready) fail(`${label}: surface is not authoritative-ready`);
  const fullStateBoundedDom =
    state.stateMessages === SEEDED_MESSAGES &&
    state.projectedDom <= TRANSCRIPT_WINDOW_MAX &&
    state.projectedDom > 0 &&
    state.domItems <= TRANSCRIPT_WINDOW_MAX &&
    state.domItems > 0;
  if (!fullStateBoundedDom) {
    fail(
      `${label}: full state vs bounded DOM violated: stateMessages === SEEDED_MESSAGES expected, got ${
        state.stateMessages
      }; projectedDom <= TRANSCRIPT_WINDOW_MAX expected, got ${state.projectedDom}; dom items ${state.domItems}`
    );
  }
  return state;
}


function expectedText(sid, index) {
  return index === SEEDED_MESSAGES - 1 ? `${sid}-tail` : `${sid}-msg-${String(index + 1).padStart(4, '0')}`;
}

function assertWindowExact(texts, sid, from, label) {
  if (texts.length === 0 || texts.length > TRANSCRIPT_WINDOW_MAX) {
    fail(`${label}: rendered window size ${texts.length} outside (0, ${TRANSCRIPT_WINDOW_MAX}]`);
  }
  if (new Set(texts).size !== texts.length) {
    fail(`${label}: duplicate ids inside one DOM window`);
  }
  for (let index = 0; index < texts.length; index += 1) {
    const expected = expectedText(sid, from + index);
    if (texts[index] !== expected) {
      fail(
        `${label}: transcript order broke at position ${index}: expected ${JSON.stringify(
          expected
        )}, got ${JSON.stringify(texts[index])}`
      );
    }
  }
}

async function readDisplayedWindow(page) {
  return page.evaluate(() => {
    const transcript = document.getElementById('transcript');
    const surface = document.querySelector('#transcript .transcript-surface:not([hidden])');
    return {
      scrollTop: transcript.scrollTop,
      scrollHeight: transcript.scrollHeight,
      clientHeight: transcript.clientHeight,
      windowStart: Number(surface?.dataset.windowStart ?? '-1'),
      liveFollow: surface?.dataset.liveFollow ?? '',
      liveAnchor: (surface?.dataset.liveAnchor || '').slice(0, 24),
      texts: surface
        ? [...surface.querySelectorAll('.msg--user__text')].map((node) => (node.textContent || '').trim())
        : [],
      domItems: surface ? surface.querySelectorAll('[data-item-id]').length : -1,
    };
  });
}

async function assertDemandHeadTailJourney(page, sid, label) {
  const frameSample = await page.evaluate(
    () =>
      new Promise((resolve) => {
        const sizes = [];
        const step = () => {
          const surface = document.querySelector('#transcript .transcript-surface:not([hidden])');
          sizes.push(surface ? surface.querySelectorAll('[data-item-id]').length : -1);
          if (sizes.length < 40) window.requestAnimationFrame(step);
          else resolve(sizes);
        };
        window.requestAnimationFrame(step);
      })
  );
  if (frameSample.some((size) => size <= 0 || size > TRANSCRIPT_WINDOW_MAX)) {
    fail(`${label}: DOM window not bounded across 40 idle animation frames: ${JSON.stringify(frameSample)}`);
  }
  // Diagnostic scroll recorder: the demand walk depends on real scroll
  // events reaching the app's note/decide pair. Record every scroll event on
  // #transcript (bounded) plus the pre-walk scroll state, so a walk that
  // never shifts is attributable to missing events vs missing shifts.
  await page.evaluate(() => {
    const recorder = { events: [], writes: [] };
    window.__rpiScrollTrace = recorder;
    const transcript = document.getElementById('transcript');
    if (transcript) {
      transcript.addEventListener('scroll', () => {
        if (recorder.events.length < 200) {
          recorder.events.push({ at: Math.round(performance.now()), scrollTop: transcript.scrollTop });
        }
      }, { passive: true, capture: true });
    }
  });
  const preWalk = await page.evaluate(() => ({
    scrollTop: document.getElementById('transcript')?.scrollTop ?? -1,
    scrollHeight: document.getElementById('transcript')?.scrollHeight ?? -1,
    clientHeight: document.getElementById('transcript')?.clientHeight ?? -1,
  }));
  const walkTrail = [{ preWalk, note: 'scroll events recorded on #transcript' }];
  for (let attempt = 0; attempt < 24; attempt += 1) {
    await page.evaluate(() => {
      document.getElementById('transcript').scrollTop = 0;
    });
    await page.waitForTimeout(120);
    const window = await readDisplayedWindow(page);
    walkTrail.push({
      attempt,
      scrollTop: Math.round(window.scrollTop),
      scrollHeight: window.scrollHeight,
      windowStart: window.windowStart,
      liveFollow: window.liveFollow,
      liveAnchor: window.liveAnchor,
      domItems: window.domItems,
      first: window.texts[0] || '',
      last: window.texts[window.texts.length - 1] || '',
    });
    if (window.domItems <= 0 || window.domItems > TRANSCRIPT_WINDOW_MAX) {
      fail(`${label}: DOM window out of bound during head walk: ${window.domItems}`);
    }
    if (window.texts[0] === expectedText(sid, 0)) {
      assertWindowExact(window.texts, sid, 0, `${label}: head window`);
      break;
    }
    if (attempt === 23) {
      const trace = await page.evaluate(() => window.__rpiScrollTrace || null);
      fail(`${label}: demand scroll never reached the exact head; walkTrail=${JSON.stringify(walkTrail)} scrollTrace=${JSON.stringify(trace)}`);
    }
  }
  const tailTrail = [];
  for (let attempt = 0; attempt < 24; attempt += 1) {
    await page.evaluate(() => {
      const transcript = document.getElementById('transcript');
      transcript.scrollTop = transcript.scrollHeight;
    });
    await page.waitForTimeout(120);
    const window = await readDisplayedWindow(page);
    tailTrail.push({
      attempt,
      scrollTop: Math.round(window.scrollTop),
      scrollHeight: window.scrollHeight,
      windowStart: window.windowStart,
      domItems: window.domItems,
      first: window.texts[0] || '',
      last: window.texts[window.texts.length - 1] || '',
    });
    if (window.domItems <= 0 || window.domItems > TRANSCRIPT_WINDOW_MAX) {
      fail(`${label}: DOM window out of bound during tail walk: ${window.domItems}`);
    }
    if (window.texts[0] === expectedText(sid, SEEDED_MESSAGES - TRANSCRIPT_WINDOW_MAX)) {
      assertWindowExact(window.texts, sid, SEEDED_MESSAGES - TRANSCRIPT_WINDOW_MAX, `${label}: tail window`);
      break;
    }
    if (attempt === 23) {
      const trace = await page.evaluate(() => window.__rpiScrollTrace || null);
      fail(`${label}: demand scroll never returned to the exact tail; tailTrail=${JSON.stringify(tailTrail)} scrollTrace=${JSON.stringify(trace)}`);
    }
  }
  const inactive = await page.evaluate(() =>
    [...document.querySelectorAll('#transcript .transcript-surface[hidden]')].map(
        (surface) => surface.querySelectorAll('[data-item-id]').length
      ),
    TRANSCRIPT_WINDOW_MAX
  );
  if (inactive.some((size) => size > TRANSCRIPT_WINDOW_MAX)) {
    fail(`${label}: inactive surfaces grew beyond the bound: ${JSON.stringify(inactive)}`);
  }
}

async function captureRetainedProbe(page) {
  await page.evaluate(() => {
    const transcript = document.getElementById('transcript');
    const surface = document.querySelector('#transcript .transcript-surface:not([hidden])');
    const fold = surface.querySelector('details[data-fold-key]');
    const rect = transcript.getBoundingClientRect();
    let item = null;
    for (const candidate of surface.querySelectorAll('[data-item-id]')) {
      const top = candidate.getBoundingClientRect().top;
      if (top >= rect.top && top < rect.bottom) {
        item = candidate;
        break;
      }
    }
    window.__rpiRetainedProbe = {
      surface,
      item,
      fold,
      scrollTop: transcript.scrollTop,
      itemTop: item ? item.getBoundingClientRect().top : null,
    };
  });
}

async function assertRetainedIdentity(page, label) {
  const retained = await page.evaluate(() => {
    const probe = window.__rpiRetainedProbe;
    if (!probe) return null;
    const transcript = document.getElementById('transcript');
    const surface = document.querySelector('#transcript .transcript-surface:not([hidden])');
    return {
      sameSurface: surface === probe.surface,
      sameItem: probe.item !== null && probe.item.isConnected && surface.contains(probe.item),
      foldStillOpen: probe.fold !== null && probe.fold.isConnected && probe.fold.open === true,
      itemTopDeltaPx: probe.item ? Math.abs(probe.item.getBoundingClientRect().top - probe.itemTop) : -1,
      scrollDeltaPx: Math.abs(transcript.scrollTop - probe.scrollTop),
    };
  });
  if (!retained) fail(`${label}: retained-surface probe was never captured`);
  if (!retained.sameSurface) fail(`${label}: displayed surface wrapper was remounted across A→B→A`);
  if (!retained.sameItem) fail(`${label}: displayed item DOM node was remounted across A→B→A`);
  if (!retained.foldStillOpen) fail(`${label}: open fold did not survive the A→B→A round trip`);
  if (retained.itemTopDeltaPx > 2) {
    fail(`${label}: anchor geometry moved ${retained.itemTopDeltaPx}px across A→B→A`);
  }
  if (retained.scrollDeltaPx > 2) {
    fail(`${label}: scroll position moved ${retained.scrollDeltaPx}px across A→B→A`);
  }
  return retained;
}

async function assertInactiveSurfaceHidden(page, label) {
  const state = await page.evaluate(() => {
    const probe = window.__rpiRetainedProbe;
    if (!probe) return null;
    return {
      stillMounted: probe.surface.isConnected,
      hidden: probe.surface.hidden === true,
      inert: probe.surface.hasAttribute('inert'),
      ariaHidden: probe.surface.getAttribute('aria-hidden') === 'true',
    };
  });
  if (!state) fail(`${label}: retained probe missing`);
  if (!state.stillMounted || !state.hidden || !state.inert || !state.ariaHidden) {
    fail(
      `${label}: inactive surface must stay mounted hidden+inert+aria-hidden, got ${JSON.stringify(state)}`
    );
  }
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
    const page = await context.newPage();
    if (token) {
      // Current production token storage is per-authority
      // the legacy global `rpi-web-token` key is NOT seeded.
      await page.addInitScript(
        (t) => {
          window.localStorage.setItem(
            `rpi-web-token:${encodeURIComponent(window.location.host.trim())}`,
            t
          );
        },
        token
      );
    }
    // Seed the page-authority session preference to session B ONLY when the
    // key is absent: the seeded preference drives the COLD negotiated
    // bootstrap leg on the FIRST load, but the runtime preference saved by
    // later exact-ready switches (the mobile reload restores the LAST picked
    // session) must survive a reload — an unconditional re-seed would force
    // every reload back to B and the reload restore could never observe the
    // runtime-saved preference.
    await page.addInitScript((sid) => {
      const key = `rpi-web-session:${encodeURIComponent(window.location.host.trim())}`;
      if (window.localStorage.getItem(key) === null) {
        window.localStorage.setItem(key, sid);
      }
    }, SESSION_B);
    // Passive toast recorder: records every toast element (text + error
    // class) the moment it enters #toasts, so the zero-toast contract on a
    // successful switch is asserted race-free even after auto-dismiss.
    await page.addInitScript(() => {
      const toasts = [];
      new MutationObserver(() => {
        for (const toast of document.querySelectorAll('#toasts .toast')) {
          toasts.push({
            text: toast.textContent || '',
            error: toast.classList.contains('toast--error'),
          });
        }
      }).observe(document, { childList: true, subtree: true });
      window.__rpiSwitchIdentityTestToasts = toasts;
    });
    const browserErrors = [];
    page.on('pageerror', (err) => browserErrors.push(err.message));
    // Ordered wire recorder for BOTH directions: the cold legs assert exact
    // frame correlation (command id -> response -> subscribe -> cut); the
    // exact-ready legs assert ZERO switch_session/subscribe frames after a
    // pre-click baseline.
    const socketFrames = { nextGen: 0, nextOrder: 0, sent: [], received: [] };
    page.on('websocket', (socket) => {
      const gen = ++socketFrames.nextGen;
      socket.on('framesent', (frame) => {
        if (typeof frame.payload !== 'string') return;
        socketFrames.sent.push({ order: ++socketFrames.nextOrder, gen, url: socket.url(), raw: frame.payload });
      });
      socket.on('framereceived', (frame) => {
        if (typeof frame.payload !== 'string') return;
        socketFrames.received.push({ order: ++socketFrames.nextOrder, gen, url: socket.url(), raw: frame.payload });
      });
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'WS did not reach "connected"'
    );

    // 1. Desktop header: no #session-name element, no visible "session:" label.
    await assertHeaderHasNoSessionLabel(page, 'desktop boot header');

    // 2. COLD bootstrap restore leg: the seeded preference resolves to B, so
    //    the bootstrap must negotiate ONE correlated switch to B and bind it
    //    through one subscribe + one full authoritative cut.
    await waitFor(
      page,
      (sid) =>
        (document.querySelector('.session-sidebar__row--active .session-sidebar__switch') || {})
          .dataset?.sessionId === sid,
      'bootstrap restore never activated the seeded preferred session B',
      30000,
      SESSION_B
    );
    await waitFor(
      page,
      (tail) => document.querySelector('#transcript .transcript-surface:not([hidden])')?.textContent?.includes(tail) === true,
      'bootstrap cold cut never displayed the B transcript tail',
      30000,
      TAIL_B
    );
    const bootCold = await assertColdSwitchLeg(
      page,
      socketFrames,
      { sent: 0, received: 0 },
      null,
      SESSION_B,
      'bootstrap cold restore of B',
      true
    );
    await waitFor(
      page,
      () => document.querySelector('.session-sidebar__row--switching') === null,
      'switching marker never cleared after the bootstrap cold cut',
      20000
    );
    const bootToasts = await page.evaluate(() => window.__rpiSwitchIdentityTestToasts || []);
    if (bootToasts.length > 0) {
      fail(`bootstrap cold restore surfaced toasts: ${JSON.stringify(bootToasts)}`);
    }
    await assertHeaderHasNoSessionLabel(page, 'desktop after bootstrap cold restore');
    await assertScreenshot(page, `${evidence}/switch-catalog-identity-boot-cold.png`, 'boot cold');


    // 3. FIRST USER CLICK (cold B→A): exactly one correlated switch_session
    //    carrying the SOURCE sessionId (B) and the TARGET native catalog
    await waitForClickableRow(page, SESSION_A, 'first cold click');
    const coldBaseline = {
      sent: socketFrames.sent.length,
      received: socketFrames.received.length,
    };
    const coldTiming = await measureSwitchTiming(page, SESSION_A, TAIL_A, 'first cold user click B→A', socketFrames);
    assertSubsecond('first cold user click B→A', coldTiming);
    const firstCold = await assertColdSwitchLeg(
      page,
      socketFrames,
      coldBaseline,
      SESSION_B,
      SESSION_A,
      'first cold user click B→A'
    );
    const desktopColdState = await assertFullStateBoundedDom(page, 'first cold user click B→A', SESSION_A);
    if (await page.evaluate(() => document.querySelector('.session-sidebar__row--switching') !== null)) {
      fail('first cold click: switching marker still rendered after authoritative ready');
    }
    const coldToasts = await page.evaluate(() => window.__rpiSwitchIdentityTestToasts || []);
    if (coldToasts.length > 0) {
      fail(`first cold click surfaced toasts (success and error must stay silent): ${JSON.stringify(coldToasts)}`);
    }
    await assertHeaderHasNoSessionLabel(page, 'desktop after first cold click');
    // 4. EXACT-READY repeated A→B→A: every same-socket repeated click must
    //    flip the active row + transcript ownership synchronously or by the
    //    next animation frame with ZERO switch_session/subscribe frames, no
    //    switching marker, and no toast.
    const fastSwitches = [];
    const runFastSwitch = async (target, tailMarker, label) => {
      const from = await activeSessionId(page);
      if (from === target) {
        fail(`${label}: pre-click active row already equals the target ${target}`);
      }
      await waitForClickableRow(page, target, label);
      const baseline = {
        sent: socketFrames.sent.length,
        received: socketFrames.received.length,
      };
      const timing = await measureSwitchTiming(page, target, tailMarker, label, socketFrames);
      const complete = (s) =>
        s !== null && s.active === target && s.transcriptOwned && s.authoritativeReady && !s.switchingMarker;
      if (!complete(timing.sync) && !complete(timing.firstFrame)) {
        fail(
          `${label}: exact-ready cutover did not complete within one animation frame`
        );
      }
      const paint = complete(timing.sync) ? 'sync' : 'next-frame';
      await page.waitForTimeout(300);
      const sentAfter = parsedFrames(socketFrames.sent.slice(baseline.sent));
      const wire = {
        switchSession: sentAfter.filter((frame) => frame.type === 'switch_session').length,
        subscribe: sentAfter.filter((frame) => frame.type === 'subscribe').length,
      };
      if (wire.switchSession !== 0 || wire.subscribe !== 0) {
        fail(`${label}: exact-ready switch emitted wire frames: ${JSON.stringify(wire)}`);
      }
      const toasts = await page.evaluate(() => window.__rpiSwitchIdentityTestToasts || []);
      if (toasts.length > 0) {
        fail(`${label}: exact-ready switch surfaced toasts: ${JSON.stringify(toasts)}`);
      }
      if ((await activeSessionId(page)) !== target) {
        fail(`${label}: active row regressed after the exact-ready switch`);
      }
      const record = {
        from,
        target,
        paint,
        durations: switchDurations(timing),
        wire,
      };
      fastSwitches.push(record);
      return record;
    };

    // First exact-ready repeat: A→B, with the composer-routing proof bound to
    // the SAME fast switch (the prompt must ride the fast-switched command
    // target without any switch_session wire frame).
    await runFastSwitch(SESSION_B, TAIL_B, 'exact-ready A→B (repeat 1)');
    const promptBaseline = { sent: socketFrames.sent.length, received: socketFrames.received.length };
    await promptAndWait(
      page,
      'switch-catalog-fast-route',
      'sessions-reply: switch-catalog-fast-route',
      'composer route probe on B never round-tripped after the fast switch'
    );
    const promptFrames = parsedFrames(socketFrames.sent.slice(promptBaseline.sent)).filter(
      (frame) => frame.type === 'prompt'
    );
    if (promptFrames.length !== 1 || promptFrames[0].sessionId !== SESSION_B) {
      fail(
        `composer route probe must dispatch exactly one prompt frame owned by B, got ${JSON.stringify(
          promptFrames.map((frame) => frame.sessionId)
        )}`
      );
    }
    const fastRouteExtraSwitches = parsedFrames(socketFrames.sent.slice(promptBaseline.sent)).filter(
      (frame) => frame.type === 'switch_session'
    ).length;
    if (fastRouteExtraSwitches !== 0) {
      fail(`composer route probe dispatched ${fastRouteExtraSwitches} switch_session frames`);
    }
    if ((await activeSessionId(page)) !== SESSION_B) {
      fail('composer route probe moved the active session away from B');
    }
    await runFastSwitch(SESSION_A, TAIL_A, 'exact-ready B→A (repeat 2)');
    const preferenceAfterFastA = await page.evaluate(
      () => window.localStorage.getItem(`rpi-web-session:${encodeURIComponent(window.location.host.trim())}`)
    );
    if (preferenceAfterFastA !== SESSION_A) {
      fail(
        `exact-ready switch did not save the per-authority preference to the target (got ${JSON.stringify(
          preferenceAfterFastA
        )})`
      );
    }
    await runFastSwitch(SESSION_B, TAIL_B, 'exact-ready A→B (repeat 3)');
    await runFastSwitch(SESSION_A, TAIL_A, 'exact-ready B→A (repeat 4)');

    await page.click('#transcript .transcript-surface:not([hidden]) .msg--user__large-text > summary');
    await page.waitForTimeout(150);
    await page.evaluate(() => {
      const transcript = document.getElementById('transcript');
      transcript.scrollTop = Math.floor((transcript.scrollHeight - transcript.clientHeight) / 2);
    });
    await page.waitForTimeout(200);
    await captureRetainedProbe(page);
    await runFastSwitch(SESSION_B, TAIL_B, 'exact-ready A→B (retention leg)');
    await assertInactiveSurfaceHidden(page, 'inactive A surface during B');
    await runFastSwitch(SESSION_A, TAIL_A, 'exact-ready B→A (retention leg)');
    const retained = await assertRetainedIdentity(page, 'desktop A→B→A retained identity');

    await assertHeaderHasNoSessionLabel(page, 'desktop after exact-ready repeats');
    await assertScreenshot(page, `${evidence}/switch-catalog-identity-desktop.png`, 'desktop');

    await page.setViewportSize({ width: 390, height: 844 });
    // The crossing itself must collapse the inherited-open desktop rail into
    // the closed mobile drawer (breakpoint-owned visibility default), so the
    // FIRST hamburger press below OPENS instead of closing a leaked-open
    // drawer.
    await waitFor(
      page,
      () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === false,
      'desktop-open rail never collapsed on the desktop→mobile crossing'
    );
    await page.click('#sidebar-toggle-btn');
    await waitFor(
      page,
      () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true,
      'mobile drawer never opened for the cold + exact-ready proofs'
    );
    await waitForClickableRow(page, SESSION_C, 'mobile cold click');
    const mobileColdBaseline = {
      sent: socketFrames.sent.length,
      received: socketFrames.received.length,
    };
    const mobileColdTiming = await measureSwitchTiming(page, SESSION_C, TAIL_C, 'mobile cold user click A→C', socketFrames);
    assertSubsecond('mobile cold user click A→C', mobileColdTiming);
    const mobileCold = await assertColdSwitchLeg(
      page,
      socketFrames,
      mobileColdBaseline,
      SESSION_A,
      SESSION_C,
      'mobile cold user click A→C'
    );
    const mobileColdState = await assertFullStateBoundedDom(page, 'mobile cold user click A→C', SESSION_C);
    const mobileToastsPre = await page.evaluate(() => window.__rpiSwitchIdentityTestToasts || []);
    if (mobileToastsPre.length > 0) {
      fail(`mobile cold click surfaced toasts: ${JSON.stringify(mobileToastsPre)}`);
    }
    await assertDemandHeadTailJourney(page, SESSION_C, 'mobile C demand head/tail window walk');
    // Every mobile pick closes the drawer (App onSwitchComplete contract),
    // so each subsequent fast-switch row click re-opens it with exactly ONE
    // hamburger press — never a double-press workaround.
    const openMobileDrawer = async (label) => {
      await page.click('#sidebar-toggle-btn');
      await waitFor(
        page,
        () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true,
        label
      );
    };
    await openMobileDrawer('mobile drawer never reopened for the mobile exact-ready C→A pick');
    await runFastSwitch(SESSION_A, TAIL_A, 'mobile exact-ready C→A');
    await openMobileDrawer('mobile drawer never reopened for the mobile exact-ready A→C pick');
    await runFastSwitch(SESSION_C, TAIL_C, 'mobile exact-ready A→C');
    await openMobileDrawer('mobile drawer never reopened for the mobile exact-ready preference pick');
    await runFastSwitch(SESSION_A, TAIL_A, 'mobile exact-ready C→A (preference leg)');
    const mobileFastSwitches = fastSwitches.slice(-3);
    // The last pick itself closed the drawer (onSwitchComplete, mobile); the
    // post-pick close is asserted directly — no extra toggle press here.
    await waitFor(
      page,
      () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === false,
      'mobile drawer never closed after the exact-ready pick'
    );
    const preReload = await page.evaluate(() => ({
      preference: window.localStorage.getItem(`rpi-web-session:${encodeURIComponent(window.location.host.trim())}`),
      activeRow: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
    }));
    const preReloadSent = parsedFrames(socketFrames.sent.slice(-40))
      .filter((frame) => frame.type === 'switch_session' || frame.type === 'prompt')
      .map((frame) => ({ type: frame.type, sessionId: frame.sessionId || frame.catalogSessionId }));
    await page.reload({ waitUntil: 'domcontentloaded' });
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'mobile reload did not reach "connected"'
    );
    try {
      await page.waitForFunction(
        (sid) =>
          (document.querySelector('.session-sidebar__row--active .session-sidebar__switch') || {})
            .dataset?.sessionId === sid,
        SESSION_A,
        { timeout: 30000 }
      );
    } catch {
      const dump = await page.evaluate(() => ({
        activeRow: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
        preference: window.localStorage.getItem(`rpi-web-session:${encodeURIComponent(window.location.host.trim())}`),
        lifecycle: (() => {
          const data = document.getElementById('session-lifecycle')?.dataset || {};
          return {
            displayed: data.displayedSessionId || '',
            pendingGeneration: data.pendingGeneration ?? '',
            pendingPhase: data.pendingPhase ?? '',
            authoritativeReady: data.authoritativeReady === 'true',
          };
        })(),
        switching: document.querySelector('.session-sidebar__row--switching') !== null,
        toasts: (window.__rpiSwitchIdentityTestToasts || []).map((toast) => toast.text.slice(0, 80)),
      }));
      const receivedTail = parsedFrames(socketFrames.received.slice(-120))
        .filter((frame) => frame.type !== 'response')
        .map((frame) => ({ type: frame.type, sessionId: frame.sessionId || undefined, reason: frame.reason || undefined }));
      fail(`mobile reload never restored session A as the active row; preReload=${JSON.stringify(preReload)} preReloadSent=${JSON.stringify(preReloadSent)} dump=${JSON.stringify({ ...dump, receivedTail })}`);
    }
    await assertHeaderHasNoSessionLabel(page, 'mobile header');
    const mobileToasts = await page.evaluate(() => window.__rpiSwitchIdentityTestToasts || []);
    const mobileErrorToasts = mobileToasts.filter((toast) => toast.error);
    if (mobileErrorToasts.length > 0) {
      fail(`mobile reload surfaced error toasts: ${JSON.stringify(mobileErrorToasts)}`);
    }
    await assertScreenshot(page, `${evidence}/switch-catalog-identity-mobile.png`, 'mobile');

    if (browserErrors.length > 0) {
      fail(`browser errors observed: ${JSON.stringify(browserErrors)}`);
    }

    //    assertion above passed. Counts, ids, and paint provenance only.
    fs.mkdirSync(evidence, { recursive: true });
    fs.writeFileSync(
      `${evidence}/switch-catalog-fast-metrics.json`,
      JSON.stringify(
        {
          seeded: {
            messagesPerSession: SEEDED_MESSAGES,
            sessionIds: [SESSION_A, SESSION_B, SESSION_C],
            projectionWindowMax: TRANSCRIPT_WINDOW_MAX,
          },
          maxSwitchMs: MAX_SWITCH_MS,
          coldLegs: {
            bootstrapRestoreB: bootCold,
            firstUserClickBtoA: {
              ...firstCold,
              durations: switchDurations(coldTiming),
              stateMessages: desktopColdState.stateMessages,
              projectedDom: desktopColdState.projectedDom,
            },
            mobileColdClickAtoC: {
              ...mobileCold,
              durations: switchDurations(mobileColdTiming),
              stateMessages: mobileColdState.stateMessages,
              projectedDom: mobileColdState.projectedDom,
            },
          },
          desktopExactReadySwitches: fastSwitches.slice(0, 4),
          retainedSurfaceIdentity: retained,
          mobileExactReadySwitches: mobileFastSwitches,
          composerRoute: {
            promptFrames: 1,
            routedSessionId: SESSION_B,
            extraSwitchFrames: 0,
          },
          preferenceAfterFastSwitch: preferenceAfterFastA,
        },
        null,
        2
      )
    );
    console.log('web-switch-catalog-identity: PASSED (cold bootstrap + cold B→A + never-loaded mobile cold C correlated cuts, retained-surface A-B-A identity with scroll/fold retention, fixed bounded demand-window head/tail walk, repeated exact-ready zero-wire one-frame cutover with composer/preference ownership, desktop+mobile, zero toasts, header session label absent)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-switch-catalog-identity: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});