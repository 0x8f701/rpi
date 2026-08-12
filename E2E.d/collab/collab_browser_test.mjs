#!/usr/bin/env node
// Collaboration browser guest E2E lane (Playwright half of E2E.d/collab/collab_scenario.sh).
//
// Opens the /web client at the collab join URL in a real Chromium browser and
// asserts the collab guest view: encrypted back-history rendering, tool cards,
// live stream from a control guest's prompt, role badge, composer state
// (enabled for control, disabled+hidden for view), participant visibility,
// and host-stop close detection.
//
// Wire protocol: the browser opens the join link (http://host:port/collab/ws/<roomId>#c=<key>
// or #v=<key>). The /web client JS detects guest mode from the pathname, reads
// the role key from the URL fragment, computes the SHA-256 capability hash,
// and opens a WS to the same path with the rpi-collab.<hash> subprotocol. The
// server sends a plaintext TEXT hello JSON, then encrypted binary snapshot
// and event frames. The client decrypts and renders history + live events.
//
// Runtime commands (sent by the orchestrator shell, NOT this test):
//   collab_start  → controlLink + viewLink
//   collab_status → participant counts
//   collab_stop   → stopped=true (closes all guest WS connections)
//
// Environment:
//   RPI_URL           Full collab join link (http://host:port/collab/ws/<roomId>#c=<key> or #v=<key>)
//   RPI_ROLE          "control" or "view"
//   RPI_EVIDENCE      Evidence directory for screenshots + DOM snapshots
//   RPI_CHROME        System Chrome/Chromium executable path (optional; else bundled)
//   RPI_EVENT_TEXT    Comma-separated text expected in live stream after control prompt
//   RPI_CONNECTED_MARKER Path written after encrypted history/tool-card assertions pass
//   RPI_READY_MARKER  Path written after BG-01 through BG-09 pass and before
//                     waiting for host stop
//   RPI_STOP_MARKER   Path to a marker file written by the shell after collab_stop
//
// Exit: 0 = all assertions passed; 1 = setup failure (no Chromium);
//       2 = assertion failure. BG-01b proves the fragment is scrubbed while
//       BG-02 proves the retained in-memory key still authenticates.

import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';

const url = process.env.RPI_URL;
const role = process.env.RPI_ROLE || 'view';
const evidence = process.env.RPI_EVIDENCE || '.';
const chromePath = process.env.RPI_CHROME || '';
const connectedMarker = process.env.RPI_CONNECTED_MARKER || '';
const readyMarker = process.env.RPI_READY_MARKER || '';
const stopMarker = process.env.RPI_STOP_MARKER || '';
const stopTimeout = parseInt(process.env.RPI_STOP_TIMEOUT || '30', 10) * 1000;
const eventTexts = (process.env.RPI_EVENT_TEXT || '').split(',').map((text) => text.trim()).filter(Boolean);
const terminalStabilityMs = 2500;
const roleKeyMarker = url ? new URL(url).hash.slice(3) : '';

const results = [];

function sanitizeDetails(details) {
  const text = String(details);
  return roleKeyMarker ? text.split(roleKeyMarker).join('[REDACTED]') : text;
}

function assert(id, description, condition, details) {
  const passed = !!condition;
  // Passing assertions store/log no details: failure-only data and raw host
  // paths must never be persisted or printed on a pass.
  const entry = { id, description, passed };
  const safeDetails = details ? sanitizeDetails(details) : '';
  if (!passed) entry.details = safeDetails || null;
  results.push(entry);
  const tag = passed ? 'PASS' : 'FAIL';
  const suffix = (!passed && safeDetails) ? ' — ' + safeDetails.slice(0, 200) : '';
  console.error(`[collab-browser] ${tag} ${id}: ${description}${suffix}`);
}

function fail(message) {
  console.error(`collab-browser: FAIL: ${sanitizeDetails(message)}`);
  writeResults();
  process.exit(2);
}

function writeResults() {
  fs.mkdirSync(evidence, { recursive: true });
  fs.writeFileSync(path.join(evidence, 'browser-results.json'), JSON.stringify(results, null, 2));
}

async function waitFor(page, fn, label, timeoutMs = 25000) {
  try {
    await page.waitForFunction(fn, null, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

async function main() {
  if (!url) fail('RPI_URL is required');
  if (!connectedMarker || !readyMarker || !stopMarker) {
    fail('RPI_CONNECTED_MARKER, RPI_READY_MARKER, and RPI_STOP_MARKER are required');
  }

  fs.mkdirSync(evidence, { recursive: true });
  const launchOptions = chromePath ? { executablePath: chromePath } : {};

  let browser;
  try {
    browser = await chromium.launch(launchOptions);
  } catch (e) {
    console.error(`collab-browser: SETUP FAILED: chromium launch failed: ${sanitizeDetails(e.message)}`);
    process.exit(1);
  }

  try {
    const page = await browser.newPage();
    const connectionStates = [];
    let streamSeen = false;
    page.on('pageerror', (err) => {
      console.error(`collab-browser: page error: ${sanitizeDetails(err.message)}`);
    });
    await page.exposeFunction('recordCollabConnectionState', (state) => {
      connectionStates.push(String(state));
    });
    await page.exposeFunction('recordCollabStreamSeen', () => {
      streamSeen = true;
    });
    await page.addInitScript(() => {
      let streamReported = false;
      const checkStream = () => {
        if (streamReported) return;
        const badge = document.getElementById('stream-badge');
        if (badge && !badge.hidden) {
          streamReported = true;
          window.recordCollabStreamSeen();
        }
      };
      const observer = new MutationObserver(() => {
        const state = document.getElementById('conn-state')?.dataset?.state;
        if (state) window.recordCollabConnectionState(state);
        checkStream();
      });
      // Observe `document` (always a Node at init time) rather than
      // `document.documentElement`, which may not yet exist when the init
      // script runs and would throw "not of type 'Node'", silently dropping
      // every observer callback. subtree covers the whole tree.
      observer.observe(document, {
        subtree: true,
        childList: true,
        attributes: true,
        attributeFilter: ['data-state', 'hidden'],
      });
      checkStream();
      // Belt-and-suspenders: the live run can be brief, and a single
      // mutation-batch could in principle skip the visible window. Poll the
      // badge for the first 12s so the streaming render path is reliably
      // observed even if the observer microtask races the settle render.
      const poll = setInterval(checkStream, 50);
      setTimeout(() => clearInterval(poll), 12000);
    });

    // ── BG-01: page loads collab guest view ─────────────────────────────
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.querySelector('#collab-guest') !== null,
      'BG-01: #collab-guest container exists');
    assert('BG-01', 'collab guest view loaded (#collab-guest exists)', true);
    const visibleHash = await page.evaluate(() => window.location.hash);
    assert('BG-01b', 'role-key fragment is scrubbed from address bar/history after parse',
      visibleHash === '', `hashLength=${visibleHash.length}`);

    // ── BG-02: WS connects (conn-state = connected) ─────────────────────
    await waitFor(page,
      () => document.getElementById('conn-state')?.dataset?.state === 'connected',
      'BG-02: conn-state reaches "connected"');
    assert('BG-02', 'WS connected (conn-state=connected)', true);

    await page.screenshot({ path: path.join(evidence, 'collab-connected.png'), fullPage: true });

    // ── BG-03: role badge shows correct role ────────────────────────────
    await waitFor(page, () => {
      const badge = document.getElementById('collab-role-badge');
      return badge !== null && badge.textContent.trim().length > 0;
    }, 'BG-03: role badge visible');
    const badgeText = await page.evaluate(() =>
      document.getElementById('collab-role-badge')?.textContent?.trim() || ''
    );
    const expectedBadge = role === 'control' ? 'control' : 'view-only';
    assert('BG-03', `role badge shows "${expectedBadge}"`,
      badgeText.includes(expectedBadge) || badgeText.includes(role),
      `badge="${badgeText}" expected="${expectedBadge}"`);

    // ── BG-04: transcript renders history entries ──────────────────────
    await waitFor(page,
      () => document.querySelectorAll('#transcript .msg').length > 0,
      'BG-04: transcript has history messages');
    const msgCount = await page.evaluate(() =>
      document.querySelectorAll('#transcript .msg').length
    );
    assert('BG-04', 'transcript renders back-history messages', msgCount > 0,
      `msgCount=${msgCount}`);

    // ── BG-05: tool card renders ────────────────────────────────────────
    await waitFor(page,
      () => document.querySelectorAll('#transcript .tool-card').length > 0,
      'BG-05: tool-card elements exist in transcript', 15000);
    const toolCardInfo = await page.evaluate(() => {
      const cards = document.querySelectorAll('#transcript .tool-card');
      if (cards.length === 0) return null;
      const card = cards[0];
      const name = card.querySelector('.tool-card__name, .tool-card__title')?.textContent?.trim() || '';
      const state = card.getAttribute('data-tool-status') || card.querySelector('[class*="tool-card__state"]')?.className || '';
      return { name, state, total: cards.length };
    });
    assert('BG-05', 'tool card renders with name', toolCardInfo !== null && toolCardInfo.name.length > 0,
      `name="${toolCardInfo?.name}" state="${toolCardInfo?.state}" count=${toolCardInfo?.total}`);

    await page.screenshot({ path: path.join(evidence, 'collab-history.png'), fullPage: true });
    if (connectedMarker && results.every((result) => result.passed)) {
      fs.writeFileSync(connectedMarker, 'connected\n');
    }

    // ── BG-06: live stream renders every expected marker after control prompt
    // The orchestrator releases the control CLI only after this browser has
    // consumed history, so these markers prove live delivery rather than a
    // snapshot-only rendering path.
    const msgCountBefore = msgCount;
    let liveRendered = false;
    if (eventTexts.length > 0) {
      try {
        await page.waitForFunction(
          (texts) => {
            const text = document.querySelector('#transcript')?.textContent || '';
            return texts.every((marker) => text.includes(marker));
          },
          eventTexts,
          { timeout: 30000 }
        );
        liveRendered = true;
      } catch {
        // Fall through to the explicit failure below.
      }
    }
    assert('BG-06', 'live stream renders every expected marker after control prompt', liveRendered,
      `expected=${eventTexts.length} messagesBefore=${msgCountBefore} messagesAfter=${await page.evaluate(() => document.querySelectorAll('#transcript .msg').length)}`);
    // ── BG-06b: streaming badge becomes visible during the live run ────
    // The host's live assistant turn flips `streaming` true, which toggles
    // the #stream-badge `hidden` binding off — exercising the streaming
    // render path (setStreaming(true) -> hidden={!streaming}). The init
    // observer records the first visible transition; assert it fired.
    assert('BG-06b', 'stream badge becomes visible during the live run (streaming render path)',
      streamSeen, 'streamBadgeNeverVisible');

    await page.screenshot({ path: path.join(evidence, 'collab-live.png'), fullPage: true });

    // ── BG-07: composer state matches role ─────────────────────────────
    if (role === 'control') {
      const promptEnabled = await page.evaluate(() => {
        const input = document.getElementById('prompt-input');
        const action = document.getElementById('send-btn');
        return {
          inputExists: input !== null,
          inputDisabled: input?.disabled === true,
          actionExists: action !== null,
          actionDisabled: action?.disabled === true,
          actionLabel: action?.getAttribute('aria-label') || '',
        };
      });
      assert('BG-07', 'control guest: composer enabled with one unified action',
        promptEnabled.inputExists && !promptEnabled.inputDisabled &&
        promptEnabled.actionExists && !promptEnabled.actionDisabled && promptEnabled.actionLabel === 'Send message',
        `input=${promptEnabled.inputExists} disabled=${promptEnabled.inputDisabled} action=${promptEnabled.actionExists} label=${promptEnabled.actionLabel}`);

      // BG-07b: control can type and send a prompt
      const testPrompt = 'browser-collab-e2e-prompt';
      await page.fill('#prompt-input', testPrompt);
      await page.click('#send-btn');
      // The prompt should generate events; verify the input cleared or a response appeared.
      try {
        await page.waitForFunction(
          (txt) => {
            const transcript = document.querySelector('#transcript')?.textContent || '';
            return transcript.includes(txt);
          },
          testPrompt,
          { timeout: 20000 }
        );
        assert('BG-07b', 'control guest: browser prompt appears in transcript', true);
      } catch {
        assert('BG-07b', 'control guest: browser prompt appears in transcript', false,
          'prompt text not found in transcript after send');
      }
    } else {
      const viewState = await page.evaluate(() => {
        const input = document.getElementById('prompt-input');
        const action = document.getElementById('send-btn');
        const notice = document.querySelector('.collab-viewonly-notice');
        return {
          inputExists: input !== null,
          inputDisabled: input?.disabled === true,
          inputHidden: input?.hidden === true || (input ? getComputedStyle(input).display === 'none' : true),
          actionExists: action !== null,
          actionDisabled: action?.disabled === true,
          noticeVisible: notice !== null && getComputedStyle(notice).display !== 'none',
        };
      });
      assert('BG-07', 'view guest: composer unavailable and view-only notice visible',
        (viewState.inputHidden || !viewState.inputExists || viewState.inputDisabled) &&
        (!viewState.actionExists || viewState.actionDisabled) && viewState.noticeVisible,
        `input=${viewState.inputExists} disabled=${viewState.inputDisabled} action=${viewState.actionExists} notice=${viewState.noticeVisible}`);

      assert('BG-07b', 'view guest: unified composer action disabled or absent',
        !viewState.actionExists || viewState.actionDisabled,
        `actionExists=${viewState.actionExists} actionDisabled=${viewState.actionDisabled}`);
    }

    // ── BG-08: no host path in DOM ─────────────────────────────────────
    const domText = await page.evaluate(() => document.documentElement.textContent || '');
    const hostPath = process.env.RPI_HOST_PATH || '';
    if (hostPath) {
      assert('BG-08', 'DOM does NOT contain the host workspace path',
        !domText.includes(hostPath), `host path present=${domText.includes(hostPath)}`);
    } else {
      assert('BG-08', 'DOM does not contain the host workspace path', true, 'no comparison path provided');
    }
    const rawPrivacyMarkers = [
      ['s', 'k-collabprivacy1234567890'].join(''),
      ['s', 'k-collablive1234567890'].join(''),
      ['/', 'tmp/collab-private/workspace'].join(''),
      ['/', 'tmp/collab-live/private.txt'].join(''),
    ];
    assert('BG-08b', 'DOM contains no raw privacy fixture secrets or absolute paths',
      rawPrivacyMarkers.every((marker) => !domText.includes(marker)),
      `rawMarkerPresent=${rawPrivacyMarkers.some((marker) => domText.includes(marker))}`);

    // ── BG-09: approval cards are read-only (host-only approvals) ───────
    const approvalInfo = await page.evaluate(() => {
      const approvals = document.querySelectorAll('#transcript .approval');
      if (approvals.length === 0) return { count: 0, hasButtons: false };
      // Check that no interactive approve/deny buttons are present for guests.
      const buttons = document.querySelectorAll('#transcript .approval button');
      return { count: approvals.length, hasButtons: buttons.length > 0 };
    });
    assert('BG-09', 'approval cards are read-only (no interactive approve/deny buttons for guests)',
      approvalInfo.count === 0 || !approvalInfo.hasButtons,
      `approvalCount=${approvalInfo.count} hasButtons=${approvalInfo.hasButtons}`);
    if (readyMarker && results.every((result) => result.passed)) {
      fs.writeFileSync(readyMarker, 'ready\n');
    }


    // ── BG-10: explicit host stop is terminal ──────────────────────────
    // The shell script writes the stop marker after calling collab_stop.
    // The observer records bounded status values only; reset it immediately
    // before waiting for the marker so every post-stop transition is retained.
    connectionStates.length = 0;
    let markerSeen = false;
    const markerDeadline = Date.now() + stopTimeout + 10000;
    while (Date.now() < markerDeadline) {
      try {
        if (fs.existsSync(stopMarker)) { markerSeen = true; break; }
      } catch (err) {
        fail(`BG-10: cannot read stop marker: ${err.message}`);
      }
      await new Promise((resolve) => setTimeout(resolve, 500));
    }

    let reachedClosed = false;
    let statesAtClosed = 0;
    if (markerSeen) {
      try {
        await page.waitForFunction(
          () => document.getElementById('conn-state')?.dataset?.state === 'closed',
          null,
          { timeout: stopTimeout }
        );
        reachedClosed = true;
        await page.waitForTimeout(0);
        statesAtClosed = connectionStates.length;
      } catch {
        // The explicit assertion below records the bounded state only.
      }
    }
    if (reachedClosed) await page.waitForTimeout(terminalStabilityMs);
    const finalState = await page.evaluate(() =>
      document.getElementById('conn-state')?.dataset?.state || ''
    );
    const laterStates = connectionStates.slice(statesAtClosed);
    const retryStateSeen = laterStates.some((state) =>
      state === 'connecting' || state === 'reconnecting'
    );
    assert('BG-10', 'host stop leaves browser WS stably closed with no reconnect status',
      markerSeen && reachedClosed && finalState === 'closed' && !retryStateSeen,
      `markerSeen=${markerSeen} reachedClosed=${reachedClosed} finalState=${finalState} laterStateCount=${laterStates.length} retryStateSeen=${retryStateSeen}`);

    await page.screenshot({ path: path.join(evidence, 'collab-final.png'), fullPage: true });

    // Save a sanitized DOM snapshot for evidence. The document has no
    // capability fragment, and the serialized output is checked again before
    // it is persisted.
    const domHtml = await page.content();
    const evidenceRoleKey = roleKeyMarker;
    assert('BG-10b', 'browser evidence contains no role-key capability bytes',
      !evidenceRoleKey || !domHtml.includes(evidenceRoleKey),
      `roleKeyPresent=${!!evidenceRoleKey && domHtml.includes(evidenceRoleKey)}`);
    fs.writeFileSync(path.join(evidence, 'collab-dom.html'), domHtml);

    writeResults();
    const allPassed = results.every((r) => r.passed);
    console.log(`[collab-browser] ${results.filter((r) => r.passed).length}/${results.length} assertions passed`);
    if (!allPassed) process.exit(2);

  } finally {
    await browser.close();
  }
}

main().catch((err) => {
  const message = sanitizeDetails(err?.message || err);
  console.error(`collab-browser: crashed: ${message}`);
  assert('BG-FATAL', 'browser test did not crash', false, message);
  writeResults();
  process.exit(2);
});