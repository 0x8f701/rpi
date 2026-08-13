#!/usr/bin/env node
// Collaboration VIEW-role browser guest (coverage driver phase of
// E2E.d/collab/collab_scenario.sh, gated by COLLAB_VIEW_GUEST=1).
//
// Opens the /web client at the collab VIEW join link in a real Chromium
// browser and asserts the view-only guest contract: view-only role badge,
// history + live stream rendering, read-only view notice (no composer / no
// send control), read-only approval notices, host-path absence, and a stable
// close on host stop. Complements the control-role browser guest
// (collab_browser_test.mjs) so CollabGuestView's view branches are exercised
// through the real rendering path.
//
// Environment:
//   RPI_URL           Full collab view join link (#v=<key>)
//   RPI_EVIDENCE      Evidence directory for screenshots + results JSON
//   RPI_CHROME        System Chrome/Chromium executable path (optional)
//   RPI_EVENT_TEXT    Comma-separated text expected in the live stream after
//                     the control prompt
//   RPI_CONNECTED_MARKER  written after VG-01..VG-03 pass
//   RPI_READY_MARKER  written after the pre-stop assertions pass
//   RPI_STOP_MARKER   written by the shell after collab_stop
//   RPI_STOP_TIMEOUT  seconds to wait for the host-stop close (default 30)
//
// Exit: 0 = all assertions passed; 1 = setup failure; 2 = assertion failure.

import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';

const url = process.env.RPI_URL;
const evidence = process.env.RPI_EVIDENCE || '.';
const chromePath = process.env.RPI_CHROME || '';
const connectedMarker = process.env.RPI_CONNECTED_MARKER || '';
const readyMarker = process.env.RPI_READY_MARKER || '';
const stopMarker = process.env.RPI_STOP_MARKER || '';
const stopTimeout = parseInt(process.env.RPI_STOP_TIMEOUT || '30', 10) * 1000;
const eventTexts = (process.env.RPI_EVENT_TEXT || '').split(',').map((text) => text.trim()).filter(Boolean);
const roleKeyMarker = url ? new URL(url).hash.slice(3) : '';

const results = [];

function sanitizeDetails(details) {
  const text = String(details);
  return roleKeyMarker ? text.split(roleKeyMarker).join('[REDACTED]') : text;
}

function assert(id, description, condition, details) {
  const passed = !!condition;
  const entry = { id, description, passed };
  const safeDetails = details ? sanitizeDetails(details) : '';
  if (!passed) entry.details = safeDetails || null;
  results.push(entry);
  const tag = passed ? 'PASS' : 'FAIL';
  const suffix = (!passed && safeDetails) ? ' — ' + safeDetails.slice(0, 200) : '';
  console.error(`[collab-view] ${tag} ${id}: ${description}${suffix}`);
}

function fail(message) {
  console.error(`collab-view: FAIL: ${sanitizeDetails(message)}`);
  writeResults();
  process.exit(2);
}

function writeResults() {
  fs.mkdirSync(evidence, { recursive: true });
  fs.writeFileSync(path.join(evidence, 'view-browser-results.json'), JSON.stringify(results, null, 2));
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg = null) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
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
    console.error(`collab-view: SETUP FAILED: chromium launch failed: ${sanitizeDetails(e.message)}`);
    process.exit(1);
  }

  try {
    const page = await browser.newPage();
    page.on('pageerror', (err) => {
      console.error(`collab-view: page error: ${sanitizeDetails(err.message)}`);
    });
    await page.exposeFunction('recordCollabStreamSeen', () => {
      // The view guest records the streaming badge render (live stream path).
      globalThis.__collabViewStreamSeen = true;
    });
    await page.addInitScript(() => {
      let streamReported = false;
      const checkStream = () => {
        if (streamReported) return;
        const badge = document.getElementById('stream-badge');
        if (badge && !badge.hidden) {
          globalThis.__collabViewStreamSeen = true;
          streamReported = true;
          window.recordCollabStreamSeen();
        }
      };
      const observer = new MutationObserver(() => {
        checkStream();
      });
      observer.observe(document, { subtree: true, childList: true, attributes: true, attributeFilter: ['hidden'] });
      checkStream();
      const poll = setInterval(checkStream, 50);
      setTimeout(() => clearInterval(poll), 15000);
    });

    // VG-01: the view join link loads the guest view in view role.
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => {
      const root = document.getElementById('collab-guest');
      return root !== null && root.getAttribute('data-role') === 'view';
    }, 'VG-01: #collab-guest view container exists');
    const visibleHash = await page.evaluate(() => window.location.hash);
    assert('VG-01b', 'role-key fragment is scrubbed from the address bar after parse',
      visibleHash === '', `hashLength=${visibleHash.length}`);
    assert('VG-01', 'collab guest view loaded with role=view', true);

    // VG-02: WS connects.
    await waitFor(page,
      () => document.getElementById('conn-state')?.dataset?.state === 'connected',
      'VG-02: conn-state reaches "connected"');
    assert('VG-02', 'WS connected (conn-state=connected)', true);

    // VG-03: role badge shows view-only.
    await waitFor(page, () => {
      const badge = document.getElementById('collab-role-badge');
      return badge !== null && badge.textContent.trim().length > 0;
    }, 'VG-03: role badge visible');
    const badgeText = await page.evaluate(() =>
      document.getElementById('collab-role-badge')?.textContent?.trim() || ''
    );
    assert('VG-03', 'role badge shows "view-only"',
      badgeText.includes('view-only'), `badge="${badgeText}"`);

    // VG-04: history transcript renders.
    await waitFor(page,
      () => document.querySelectorAll('#transcript .msg').length > 0,
      'VG-04: transcript has history messages');
    assert('VG-04', 'transcript renders back-history messages', true);

    await page.screenshot({ path: path.join(evidence, 'view-connected.png'), fullPage: true });
    fs.writeFileSync(connectedMarker, 'connected');

    // VG-05: view-only notice renders; no composer / no send control.
    await waitFor(page,
      () => document.querySelector('.collab-viewonly-notice') !== null,
      'VG-05: view-only notice never rendered');
    const noticeText = await page.evaluate(() =>
      document.querySelector('.collab-viewonly-notice')?.textContent || ''
    );
    assert('VG-05', 'view-only notice explains disabled prompting',
      noticeText.includes('View-only guest'), `notice="${noticeText.slice(0, 80)}"`);
    const composerState = await page.evaluate(() => ({
      promptInput: document.getElementById('prompt-input') !== null,
      sendBtn: document.getElementById('send-btn') !== null,
      composerButtons: document.getElementById('composer-buttons') !== null,
    }));
    assert('VG-05b', 'view guest has no composer/send controls',
      !composerState.promptInput && !composerState.sendBtn && !composerState.composerButtons,
      JSON.stringify(composerState));

    // VG-06: tool card from back-history renders read-only.
    await waitFor(page,
      () => document.querySelectorAll('#transcript .tool-card').length > 0,
      'VG-06: tool cards exist in the transcript', 15000);
    assert('VG-06', 'tool card renders in the view transcript', true);

    // VG-07: live stream from the control prompt renders (event text lands).
    if (eventTexts.length > 0) {
      await waitFor(page,
        (texts) => texts.every((t) => document.body.textContent.includes(t)),
        'VG-07: live control-prompt text never rendered in the view transcript',
        30000,
        eventTexts);
      assert('VG-07', 'live stream renders every expected control-prompt marker', true);
      await page.waitForTimeout(400);
      const streamSeen = await page.evaluate(() => globalThis.__collabViewStreamSeen === true);
      assert('VG-07b', 'streaming badge became visible during the live run', streamSeen);
    }

    await page.screenshot({ path: path.join(evidence, 'view-live.png'), fullPage: true });

    // VG-08: the DOM contains no host workspace path and no fixture secrets.
    const hostPath = process.env.RPI_HOST_PATH || '';
    const domState = await page.evaluate((hp) => ({
      text: document.body.textContent || '',
      html: document.documentElement.innerHTML,
      hp,
    }), hostPath);
    assert('VG-08', 'DOM does not contain the host workspace path',
      hostPath === '' || !domState.text.includes(hostPath), `hostPath=${hostPath}`);
    assert('VG-08b', 'DOM contains no raw credential-shaped secrets',
      !/(sk-[A-Za-z0-9]{16,}|mock-realtime-key|mock-realtime-key)/.test(domState.html));

    // VG-09: pre-stop assertions complete; wait for the host stop close.
    fs.writeFileSync(readyMarker, 'ready');
    const stopDeadline = Date.now() + stopTimeout;
    let markerSeen = false;
    while (Date.now() < stopDeadline) {
      if (fs.existsSync(stopMarker)) {
        markerSeen = true;
        break;
      }
      await new Promise((resolve) => setTimeout(resolve, 200));
    }
    let reachedClosed = false;
    let closedLabel = '';
    if (markerSeen) {
      try {
        await page.waitForFunction(
          () =>
            document.getElementById('conn-state')?.dataset?.state === 'closed' &&
            !document.body.textContent.includes('reconnecting'),
          null,
          { timeout: stopTimeout },
        );
        await page.waitForTimeout(500);
        const closedState = await page.evaluate(() => {
          const el = document.getElementById('conn-state');
          return {
            state: el ? el.dataset.state || '' : '',
            label: el ? el.textContent?.trim() || '' : '',
            reconnecting: document.body.textContent.includes('reconnecting'),
          };
        });
        reachedClosed = closedState.state === 'closed' && !closedState.reconnecting;
        closedLabel = closedState.label;
      } catch {
        reachedClosed = false;
      }
    }
    assert('VG-09', 'host stop leaves the view WS stably closed with no reconnect status',
      markerSeen && reachedClosed, `stopMarkerSeen=${markerSeen} reachedClosed=${reachedClosed}`);
    assert('VG-09b', 'the closed conn pill renders the "offline" label (never "error" or "reconnecting")',
      markerSeen && reachedClosed && closedLabel === 'offline', `label=${JSON.stringify(closedLabel)}`);

    await page.screenshot({ path: path.join(evidence, 'view-stopped.png'), fullPage: true });

    // VG-10: malformed /collab/ws links are a hard ERROR BOUNDARY. The
    // listener serves the web client ONLY at validated room paths — a broken
    // path (empty room id, room id containing '/') is rejected with 404 and
    // never serves the app. A valid room path carrying a malformed capability
    // fragment parses to null but the document is STILL a collab guest route,
    // so the web client mounts the nullable-link guest view and renders its
    // safe malformed-link error — never the host App shell, never a broken
    // guest mount, never the capability bytes lingering in the address bar
    // (the fragment is scrubbed even when it fails to parse). The fragment
    // variants drive the parseCollabLocation rejection branches AND the guest
    // view's null-link early return (the useScrollPin null-ref guard) through
    // the real render path: missing role fragment, unknown role prefix, empty
    // key, '&' in the encoded key, non-base64url key characters, and a key of
    // the wrong length. These phases are room-independent and run AFTER the
    // host stop so they never delay the pre-stop ready markers the CLI guests'
    // stop-close windows depend on.
    const origin = new URL(url).origin;
    // One shared probe page navigated across every variant. The coverage hook
    // segments JS coverage at navigation boundaries (dumps the current
    // document's coverage on each goto), so every variant's rendered app code
    // is captured before the next navigation; the FINAL segment (wrong-key
    // guest) is dumped at browser close. The probe page is intentionally
    // never closed first — a closed page's coverage cannot be dumped.
    const probePage = await browser.newPage();
    try {
      // Path-malformed variants: the listener serves the client ONLY at
      // validated room paths — a broken path (empty room id, room id
      // containing '/') is rejected with 404 and never presents the app.
      const malformedPathVariants = ['/collab/ws/', '/collab/ws/room/x'];
      for (let vi = 0; vi < malformedPathVariants.length; vi++) {
        const resp = await probePage.goto(origin + malformedPathVariants[vi], { waitUntil: 'domcontentloaded', timeout: 20000 });
        assert(`VG-10.${vi}`, `malformed collab path ${malformedPathVariants[vi]} is rejected by the listener (404)`,
          resp !== null && resp.status() === 404,
          `status=${resp ? resp.status() : 'no response'}`);
      }
      // Fragment-malformed variants: a valid room path carrying a malformed
      // capability fragment parses to null, but the document is still a
      // collab guest route, so the web client mounts the nullable-link guest
      // view and renders its safe malformed-link error — never the host App
      // shell, never a broken guest mount, never the capability bytes
      // lingering in the address bar (the fragment is scrubbed even when it
      // fails to parse). These drive the parseCollabLocation rejection
      // branches AND the guest view's null-link early return (the useScrollPin
      // null-ref guard) through the real render path: missing role fragment,
      // unknown role prefix, empty key, '&' in the encoded key, non-base64url
      // key characters, and a key of the wrong length.
      // Every variant uses a DISTINCT room id: a hash-only change on the same
      // path is a same-document navigation (no reload), which would never
      // re-run parseCollabLocation — each variant must be a full document
      // load to exercise its parse branch.
      const malformedFragmentVariants = [
        '/collab/ws/roomA',
        '/collab/ws/roomB#x=AAAA',
        '/collab/ws/roomC#v=',
        '/collab/ws/roomD#v=AAAA&x=y',
        '/collab/ws/roomE#v=!!!!',
        '/collab/ws/roomF#v=short',
      ];
      for (let vi = 0; vi < malformedFragmentVariants.length; vi++) {
        await probePage.goto(origin + malformedFragmentVariants[vi], { waitUntil: 'domcontentloaded', timeout: 20000 });
        // The guest view's null-link early return renders the safe
        // malformed-link error; the collab guest root must never mount and
        // the capability fragment must be scrubbed from the address bar.
        await waitFor(probePage, () => document.querySelector('.collab-error') !== null,
          `VG-10p.${vi}: malformed-link error never rendered for ${malformedFragmentVariants[vi]}`);
        // waitForFunction resolves the moment the error node COMMITS, but
        // React flushes passive effects (the useScrollPin useEffect whose
        // null-ref guard this variant drives) only AFTER paint. If the next
        // navigation dumped this document's coverage segment now, that effect
        // would record count=0 and the null-guard branch would be lost. Give
        // React one deterministic flush window: yield past the next frame
        // (rAF, so the commit is painted) and then one macrotask (so any
        // scheduler-queued effect flush drains) before asserting / navigating
        // on. This is an event-loop yield, not a fixed sleep — it costs at
        // most a frame plus one task per variant.
        await probePage.evaluate(() => new Promise((resolve) => {
          requestAnimationFrame(() => setTimeout(resolve, 0));
        }));
        const malformedDom = await probePage.evaluate(() => ({
          guest: document.getElementById('collab-guest') !== null,
          error: document.querySelector('.collab-error') !== null,
          hash: window.location.hash,
        }));
        assert(`VG-10p.${vi}`, `malformed fragment ${malformedFragmentVariants[vi]} renders the malformed-link error with the fragment scrubbed (no guest mount, no capability fragment in the address bar)`,
          !malformedDom.guest && malformedDom.error && malformedDom.hash === '',
          JSON.stringify(malformedDom));
      }

      // VG-11: a SYNTACTICALLY VALID join link carrying a WRONG capability
      // key must never reach 'connected' — the guest surfaces the connection
      // failure state (reconnecting/offline pill), never a transcript and
      // never a composer. The subprotocol handshake cannot authenticate the
      // key, so the socket is rejected and the guest retries with backoff.
      // The room id used here never existed, so this is independent of the
      // stopped room.
      const wrongKey = 'A'.repeat(43); // 32 bytes base64url, no padding
      const probeErrors = [];
      probePage.on('pageerror', (err) => probeErrors.push(`pageerror: ${err && err.message ? err.message : err}`));
      probePage.on('console', (m) => {
        if (m.type() === 'error') probeErrors.push(`console: ${m.text().slice(0, 160)}`);
      });
      await probePage.goto(`${origin}/collab/ws/roomG#v=${wrongKey}`, { waitUntil: 'domcontentloaded', timeout: 20000 });
      // Sample the page state until the guest mounts (or the window elapses),
      // collecting the exact DOM/error state for the assertion details.
      const wrongKeyProbe = [];
      const wrongKeyMountDeadline = Date.now() + 25000;
      while (Date.now() < wrongKeyMountDeadline) {
        const st = await probePage.evaluate(() => ({
          title: document.title,
          href: window.location.href,
          hash: window.location.hash,
          path: window.location.pathname,
          guest: document.getElementById('collab-guest') !== null,
          conn: document.getElementById('conn-state')?.dataset.state || '',
          rootLen: (document.getElementById('root')?.innerHTML || '').length,
        }));
        wrongKeyProbe.push(st);
        if (st.guest) break;
        await probePage.waitForTimeout(400);
      }
      if (!wrongKeyProbe.some((s) => s.guest)) {
        assert('VG-11-mount', 'wrong-key link mounts the collab guest view',
          false, JSON.stringify({ probe: wrongKeyProbe.slice(-3), errors: probeErrors.slice(-5) }));
      }
      // Observe several reconnect cycles; the state must never flip to
      // 'connected' and the pill must show the failure state at some point.
      const wrongKeyStates = [];
      const wrongKeyDeadline = Date.now() + 8000;
      while (Date.now() < wrongKeyDeadline) {
        const st = await probePage.evaluate(() => {
          const el = document.getElementById('conn-state');
          const root = document.getElementById('collab-guest');
          return {
            state: el ? el.dataset.state || '' : '',
            label: el ? el.textContent || '' : '',
            transcriptMsgs: document.querySelectorAll('#transcript .msg').length,
            composer: document.getElementById('prompt-input') !== null,
          };
        });
        wrongKeyStates.push(st);
        if (st.state === 'connected') break;
        await probePage.waitForTimeout(400);
      }
      const everConnected = wrongKeyStates.some((s) => s.state === 'connected');
      const sawFailureState = wrongKeyStates.some(
        (s) => s.state === 'reconnecting' || s.state === 'connecting' || s.state === 'error' || s.state === 'closed'
      );
      const sawTranscript = wrongKeyStates.some((s) => s.transcriptMsgs > 0);
      assert('VG-11', 'wrong-key guest never reaches connected and surfaces the failure state (no transcript, no composer)',
        !everConnected && sawFailureState && !sawTranscript,
        JSON.stringify(wrongKeyStates.slice(0, 3)));
      await probePage.screenshot({ path: path.join(evidence, 'view-wrong-key.png'), fullPage: true });
    } finally {
      // NOTE: probePage stays open until browser.close() so the coverage hook
      // can dump its final segment (closed pages cannot be dumped).
    }

    console.log('collab-view: PASSED (view role badge, history + live stream, no composer, read-only notices, host-path absence, stable stop close, malformed-link error boundary, wrong-key rejection)');
  } finally {
    writeResults();
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`collab-view: FAIL: ${err && err.message ? err.message : err}`);
  writeResults();
  process.exit(2);
});
