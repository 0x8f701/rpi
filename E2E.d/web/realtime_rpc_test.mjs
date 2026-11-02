// Web realtime RPC-level start/error/stop regression lane (playwright half
// of E2E.d/web/realtime_rpc.sh).
//
// Environment:
//   RPI_URL             http://127.0.0.1:<port>/web
//   RPI_TOKEN           token file content (served via rpi-auth.<token> subprotocol)
//   RPI_PHASE           "ok" | "error"
//   RPI_PROXY_EVIDENCE  mock-persisted proxy evidence JSON ({callId,
//                       openaiAlpha, authPresent, sessionPresent, sessionValid,
//                       sessionInputOmitted}) written when the Rust proxy reached the mock
//   RPI_CHROME          executable path of the system Chrome (optional)
//   RPI_EVIDENCE        evidence dir for screenshots
//
// The in-page RTCPeerConnection is stubbed BEFORE page load (deterministic
// lifecycle; the real transport is covered by the realtime_webrtc lane).
//
// ok phase:
//   R1  #mic-btn is in realtime mode; clicking dispatches the real
//       realtime_create_call frame on the WS
//   R2  the Rust proxy reached the mock: the evidence file records
//       OpenAI-Alpha quicksilver=v2, a Bearer token, and an AVAS-compatible
//       session (`sessionPresent`/`sessionValid` true, `audio.input` omitted)
//   R3  the live overlay renders (#realtime-transcript with the
//       .realtime-transcript__label "realtime voice") and
//       #realtime-conn-state exposes a non-empty state bucket
//   R5  clicking #mic-btn again dispatches realtime_stop and the overlay
//       disappears
// error phase:
//   R4  the mock rejects with 500; the page surfaces the "realtime call
//       failed" toast and #realtime-transcript never renders

import { chromium } from 'playwright';
import { createFreshSessionViaManage } from './lib/session-lifecycle.mjs';
import fs from 'node:fs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const phase = process.env.RPI_PHASE || 'ok';
const proxyEvidence = process.env.RPI_PROXY_EVIDENCE || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const realtimeReleaseUrl = process.env.RPI_REALTIME_RELEASE_URL || '';

function fail(message) {
  console.error(`web-realtime-rpc (${phase}): FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await page.evaluate(fn, arg)) return;
    await page.waitForTimeout(120);
  }
  fail(`${label} (timeout ${timeoutMs}ms)`);
}

/** Bounded Node-side await for one already-sent WS frame of `type`, scanning
 *  the captured array from `fromIndex` and sleeping between scans — never a
 *  busy loop and never a sleep-only success. Timeout fails with bounded frame
 *  evidence. */
async function waitForSentFrame(frames, type, fromIndex, timeoutMs, label) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    for (let i = fromIndex; i < frames.length; i++) {
      try {
        const parsed = JSON.parse(frames[i]);
        if (parsed.type === type) return parsed;
      } catch { /* non-JSON frame */ }
    }
    await new Promise((resolve) => setTimeout(resolve, 120));
  }
  fail(`${label} (timeout ${timeoutMs}ms); frames=${frames.slice(fromIndex).slice(-5).join(' | ')}`);
}

/** Deterministic in-page RTCPeerConnection stub. iceGatheringState is
 *  'complete' from construction so waitForIceGatheringComplete resolves
 *  immediately; localDescription carries gathered a=candidate lines so the
 *  POSTed offer looks gathered; setRemoteDescription resolves (the real
 *  answer correctness is covered by the realtime_webrtc lane). */
const FAKE_RTC = `(() => {
  const channels = [];
  class FakeDataChannel {
    constructor(label) {
      this.label = label;
      this.readyState = 'open';
      this.onopen = null;
      this.onmessage = null;
      this.onerror = null;
      this.onclose = null;
      this.sent = [];
      channels.push(this);
    }
    send(data) { this.sent.push(data); }
    close() { this.readyState = 'closed'; }
  }
  class FakeRTCPeerConnection {
    constructor() {
      this.iceGatheringState = 'complete';
      this.connectionState = 'connected';
      this.signalingState = 'stable';
      this.localDescription = null;
      this.remoteDescription = null;
      this.ontrack = null;
      this.onconnectionstatechange = null;
      this._listeners = {};
    }
    addTrack() {}
    createDataChannel(label) { this.dc = new FakeDataChannel(label); return this.dc; }
    async createOffer() {
      return { type: 'offer', sdp: 'v=0\\r\\no=- 0 0 IN IP4 127.0.0.1\\r\\ns=realtime-rpc-fake-offer\\r\\n' };
    }
    async setLocalDescription(desc) {
      this.localDescription = {
        type: desc.type,
        sdp: (desc.sdp || '') + 'a=candidate:1 1 udp 1 127.0.0.1 9 typ host\\r\\na=end-of-candidates\\r\\n',
      };
    }
    async setRemoteDescription(desc) { this.remoteDescription = desc; }
    addEventListener(name, fn) {
      (this._listeners[name] = this._listeners[name] || []).push(fn);
    }
    close() { this.connectionState = 'closed'; }
  }
  window.__realtimeOwnerTest = {
    // Fires the App's onopen handler (which sends session.update) — mirrors
    // the coverage lane's __rtStub.openChannel.
    openChannel() {
      const channel = channels[channels.length - 1];
      if (channel) {
        channel.readyState = 'open';
        channel.onopen?.({});
      }
    },
    // Frames the live oai-events channel SENT (session.update lives here).
    sentFrames() {
      const channel = channels[channels.length - 1];
      return channel ? channel.sent : [];
    },
    sendFrame(obj) {
      const channel = channels[channels.length - 1];
      channel?.onmessage?.({ data: JSON.stringify(obj) });
    },
    sendLateFinal(text) {
      window.__realtimeOwnerTest.sendFrame({
        type: 'conversation.input_transcript.done',
        transcript: text,
      });
    },
  };
  window.RTCPeerConnection = FakeRTCPeerConnection;
})();`;

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = {
    args: [
      '--use-fake-device-for-media-stream',
      '--use-fake-ui-for-media-stream',
    ],
  };
  if (chromePath) launchOptions.executablePath = chromePath;
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage();
    await page.addInitScript(FAKE_RTC);
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    const pageErrors = [];
    page.on('pageerror', (err) => {
      pageErrors.push(String(err && err.message ? err.message : err));
      console.error(`web-realtime-rpc: page error: ${err.message}`);
    });

    // Capture outgoing WS frames so the real create-call / stop controls can
    // be observed on the wire (deterministic — never a source-text assertion).
    const sentFrames = [];
    const receivedFrames = [];
    page.on('websocket', (ws) => {
      ws.on('framesent', (frame) => {
        const payload = typeof frame.payload === 'string' ? frame.payload : '';
        if (payload) sentFrames.push(payload);
      });
      ws.on('framereceived', (frame) => {
        const payload = typeof frame.payload === 'string' ? frame.payload : '';
        if (payload) receivedFrames.push(payload);
      });
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'WS did not reach "connected"'
    );

    // The backend advertises realtime live mode -> the mic is in realtime
    // mode (aria-pressed reflects realtimeActive, not recording).
    await waitFor(
      page,
      () => {
        const btn = document.getElementById('mic-btn');
        return btn !== null && btn.getAttribute('aria-pressed') === 'false';
      },
      'mic button did not enter realtime mode (aria-pressed false idle)'
    );

    if (phase === 'error') {
      // ---- R4: mock rejects -> user-visible "realtime call failed" toast,
      // overlay never renders ----
      await page.click('#mic-btn');
      await waitFor(
        page,
        () =>
          Array.from(document.querySelectorAll('.toast')).some((el) =>
            (el.textContent || '').includes('realtime call failed')
          ),
        'R4: the "realtime call failed" toast never appeared after the mock 500',
        20000
      );
      const overlay = await page.evaluate(() => document.getElementById('realtime-transcript') !== null);
      if (overlay) fail('R4: the realtime overlay rendered despite the failed call');
      const sentStart = sentFrames.some((f) => {
        try { return JSON.parse(f).type === 'realtime_create_call'; } catch { return false; }
      });
      if (!sentStart) fail('R4: realtime_create_call RPC never dispatched');
      await page.screenshot({ path: `${evidence}/realtime-error-toast.png`, fullPage: true });
      console.log('web-realtime-rpc (error): PASSED (mock-500 -> "realtime call failed" toast, overlay down, realtime_create_call dispatched)');
      return;
    }

    if (phase === 'owner-delay') {
      await page.click('#mic-btn');
      const createDeadline = Date.now() + 15000;
      let createFrame = null;
      while (Date.now() < createDeadline && !createFrame) {
        for (const raw of sentFrames) {
          try {
            const frame = JSON.parse(raw);
            if (frame.type === 'realtime_create_call') { createFrame = frame; break; }
          } catch { /* non-JSON frame */ }
        }
        if (!createFrame) await page.waitForTimeout(100);
      }
      if (!createFrame?.id) fail('R6: held realtime setup never dispatched create-call');
      const created = await createFreshSessionViaManage(page, waitFor, 'R6: create session B during held realtime setup');
      if (!realtimeReleaseUrl) fail('R6: RPI_REALTIME_RELEASE_URL is required');
      const release = await fetch(realtimeReleaseUrl, { method: 'POST' });
      if (!release.ok) fail(`R6: realtime release endpoint returned ${release.status}`);
      const responseDeadline = Date.now() + 15000;
      while (Date.now() < responseDeadline && !receivedFrames.some((raw) => {
        try { return JSON.parse(raw).type === 'response' && JSON.parse(raw).id === createFrame.id; } catch { return false; }
      })) {
        await page.waitForTimeout(100);
      }
      if (!receivedFrames.some((raw) => {
        try { const frame = JSON.parse(raw); return frame.type === 'response' && frame.id === createFrame.id; } catch { return false; }
      })) fail('R6: held realtime create-call never settled after release');
      if (await page.locator('#realtime-transcript').count()) fail('R6: stale A setup resurrected realtime overlay on B');
      const staleToast = await page.locator('.toast').allTextContents();
      if (staleToast.length > 0) {
        fail(`R6: stale A setup surfaced toast(s) on B (${JSON.stringify(staleToast)})`);
      }
      await page.fill('#prompt-input', 'session B setup draft');
      if (await page.inputValue('#prompt-input') !== 'session B setup draft') {
        fail(`R6: session B composer unavailable after stale A setup (${created.sessionId})`);
      }
      await page.screenshot({ path: `${evidence}/realtime-setup-switch-isolated.png`, fullPage: true });
      console.log('web-realtime-rpc (owner-delay): PASSED (held A setup cannot resurrect or toast after A→B switch)');
      return;
    }

    // ---- R1: click #mic-btn -> realtime_create_call on the WS ----
    await page.click('#mic-btn');
    const r1Deadline = Date.now() + 20000;
    let r1Pressed = false;
    while (Date.now() < r1Deadline) {
      r1Pressed = await page.evaluate(
        () => document.getElementById('mic-btn')?.getAttribute('aria-pressed') === 'true'
      );
      if (r1Pressed) break;
      await page.waitForTimeout(120);
    }
    if (!r1Pressed) {
      // Surface the page's own view of the failure (toasts, mic state,
      // frames) instead of a bare timeout.
      const diag = await page.evaluate(() => ({
        toasts: Array.from(document.querySelectorAll('.toast')).map((el) => el.textContent || ''),
        micPressed: document.getElementById('mic-btn')?.getAttribute('aria-pressed') || null,
        micTitle: document.getElementById('mic-btn')?.getAttribute('title') || null,
      }));
      fail(
        `R1: mic did not enter the active (pressed) state (timeout 20000ms) | ` +
          `pageErrors=${JSON.stringify(pageErrors)} | ${JSON.stringify(diag)} | ` +
          `frames=${sentFrames.slice(-5).join(' | ')}`
      );
    }
    let sawStart = false;
    const deadline = Date.now() + 15000;
    while (Date.now() < deadline && !sawStart) {
      sawStart = sentFrames.some((f) => {
        try { return JSON.parse(f).type === 'realtime_create_call'; } catch { return false; }
      });
      if (!sawStart) await page.waitForTimeout(200);
    }
    if (!sawStart) {
      fail(`R1: realtime_create_call RPC never dispatched (sent frames: ${sentFrames.slice(-5).join(' | ')})`);
    }

    // ---- R2: the Rust proxy reached the mock (quicksilver + Bearer) ----
    const evidenceDeadline = Date.now() + 15000;
    let proxy = null;
    while (Date.now() < evidenceDeadline) {
      try {
        proxy = JSON.parse(fs.readFileSync(proxyEvidence, 'utf8'));
        break;
      } catch {
        await page.waitForTimeout(200);
      }
    }
    if (!proxy) fail(`R2: proxy evidence file never appeared at ${proxyEvidence}`);
    if (!proxy.openaiAlpha || !proxy.openaiAlpha.includes('quicksilver=v2')) {
      fail(`R2: proxy request missing OpenAI-Alpha quicksilver=v2 (got ${proxy.openaiAlpha})`);
    }
    if (proxy.authPresent !== true) fail('R2: proxy request missing the Bearer token');
    if (!proxy.callId || !proxy.callId.startsWith('rtc_')) fail(`R2: unexpected callId (${proxy.callId})`);
    // R2 requires the complete create-call session object. Exact booleans
    // prevent a stale mock that never writes the fields from silently passing.
    if (proxy.sessionPresent !== true) {
      fail(`R2: proxy evidence missing sessionPresent=true (got ${proxy.sessionPresent})`);
    }
    if (proxy.sessionInputOmitted !== true) {
      fail(`R2: proxy evidence did not prove audio.input omission (got ${proxy.sessionInputOmitted})`);
    }
    if (proxy.sessionValid !== true) {
      fail(`R2: proxy evidence missing sessionValid=true (got ${proxy.sessionValid})`);
    }

    // ---- R3: live overlay + connection-state bucket ----
    await waitFor(
      page,
      () => document.getElementById('realtime-transcript') !== null,
      'R3: the realtime overlay (#realtime-transcript) never rendered'
    );
    const overlayState = await page.evaluate(() => ({
      label: document.querySelector('.realtime-transcript__label')?.textContent?.trim() || '',
      connState: document.getElementById('realtime-conn-state')?.dataset?.state || '',
      dot: document.querySelector('.realtime-transcript__dot') !== null,
    }));
    if (overlayState.label !== 'realtime voice') {
      fail(`R3: overlay label wrong (got "${overlayState.label}")`);
    }
    if (!overlayState.dot) fail('R3: overlay is missing the live dot');
    if (!overlayState.connState) fail('R3: #realtime-conn-state missing its state bucket');


    await page.screenshot({ path: `${evidence}/realtime-active.png`, fullPage: true });

    // ---- R3b: realtime event journey over the oai-events data channel.
    // The post-connect session.update must preserve the same AVAS-compatible
    // shape as create-call: audio.input absent, output voice present.
    await page.evaluate(() => window.__realtimeOwnerTest.openChannel());
    const sessionUpdate = await page.evaluate(() => {
      const frames = [];
      for (const raw of window.__realtimeOwnerTest.sentFrames()) {
        try { frames.push(JSON.parse(raw)); } catch { /* non-JSON */ }
      }
      return frames.find((f) => f?.type === 'session.update') || null;
    });
    if (!sessionUpdate) fail('R3b: no session.update frame was sent on the oai-events channel');
    if ('input' in (sessionUpdate.session?.audio || {})) {
      fail(`R3b: session.update sent rejected audio.input (${JSON.stringify(sessionUpdate.session?.audio)})`);
    }
    if ('model' in sessionUpdate.session || 'type' in sessionUpdate.session) {
      fail('R3b: session.update re-introduced legacy model/type fields');
    }
    await page.evaluate(() => window.__realtimeOwnerTest.sendFrame({ type: 'conversation.item.input_audio_transcription.delta', delta: 'realtime ' }));
    await waitFor(
      page,
      () => (document.querySelector('.realtime-transcript__text')?.textContent || '').includes('realtime'),
      'R3b: GA delta never reached the overlay preview'
    );
    await page.evaluate(() =>
      window.__realtimeOwnerTest.sendFrame({
        type: 'conversation.item.input_audio_transcription.segment',
        text: 'realtime segment preview',
      })
    );
    await waitFor(
      page,
      () => (document.querySelector('.realtime-transcript__text')?.textContent || '').trim() === 'realtime segment preview',
      'R3b: GA segment never superseded the overlay preview'
    );
    const userBubblesBefore = await page.evaluate(() => document.querySelectorAll('.msg--user').length);
    await page.evaluate(() =>
      window.__realtimeOwnerTest.sendFrame({
        type: 'conversation.item.input_audio_transcription.completed',
        transcript: 'realtime hello world',
      })
    );
    await waitFor(
      page,
      () => (document.getElementById('prompt-input')?.value || '').includes('realtime hello world'),
      'R3b: GA final transcript never committed to the composer'
    );
    const userBubblesAfter = await page.evaluate(() => document.querySelectorAll('.msg--user').length);
    if (userBubblesAfter !== userBubblesBefore) {
      fail(`R3b: final transcript auto-sent a message (${userBubblesBefore} -> ${userBubblesAfter} user bubbles)`);
    }
    if ((await page.evaluate(() => (document.querySelector('.realtime-transcript__text')?.textContent || ''))) !== '') {
      fail('R3b: overlay preview was not cleared after the final commit');
    }
    await page.evaluate(() =>
      window.__realtimeOwnerTest.sendFrame({ type: 'conversation.input_transcript.done', transcript: 'realtime hello world' })
    );
    await page.waitForTimeout(300);
    const commitCount = await page.evaluate(
      () => (document.getElementById('prompt-input')?.value.match(/realtime hello world/g) || []).length
    );
    if (commitCount !== 1) fail(`R3b: final transcript double-committed (count ${commitCount})`);
    await page.evaluate(() =>
      window.__realtimeOwnerTest.sendFrame({
        type: 'conversation.item.input_audio_transcription.failed',
        error: { message: 'mock asr rejected', code: 'asr_down' },
      })
    );
    await waitFor(
      page,
      () => document.body.textContent.includes('realtime error [asr_down]: mock asr rejected'),
      'R3b: transcription.failed never surfaced the bounded toast'
    );
    if ((await page.evaluate(() => (document.getElementById('prompt-input')?.value.match(/realtime hello world/g) || []).length)) !== 1) {
      fail('R3b: transcription.failed mutated the composer');
    }

    // Inspected rendered geometry/state of the exact final surface (desktop
    // 1280x720): composer rect + value, overlay rect, toasts, mic state.
    const finalGeometry = await page.evaluate(() => {
      const rect = (sel) => {
        const el = document.querySelector(sel);
        if (!el) return null;
        const r = el.getBoundingClientRect();
        return { x: Math.round(r.x), y: Math.round(r.y), w: Math.round(r.width), h: Math.round(r.height) };
      };
      return {
        viewport: { w: window.innerWidth, h: window.innerHeight },
        composer: rect('#prompt-input'),
        composerValue: document.getElementById('prompt-input')?.value || '',
        overlay: rect('#realtime-transcript'),
        previewText: document.querySelector('.realtime-transcript__text')?.textContent || '',
        toasts: Array.from(document.querySelectorAll('.toast')).map((el) => el.textContent || ''),
        micPressed: document.getElementById('mic-btn')?.getAttribute('aria-pressed'),
      };
    });
    console.log(`R3b geometry: ${JSON.stringify(finalGeometry)}`);
    await page.screenshot({ path: `${evidence}/realtime-composer-final.png`, fullPage: true });

    // ---- R5: switching A→B through the real Session panel tears the active
    // call down, dispatches realtime_stop to A, and blocks a late A transcript
    // from B's composer. Keep failure evidence bounded to command metadata.
    const sentBeforeSwitch = sentFrames.length;
    let created;
    try {
      created = await createFreshSessionViaManage(page, waitFor, 'R5: create session B during realtime call');
    } catch (error) {
      const frames = sentFrames.slice(sentBeforeSwitch).map((raw) => {
        try {
          const frame = JSON.parse(raw);
          return { type: frame.type, id: frame.id, sessionId: frame.sessionId };
        } catch {
          return { type: 'non-json' };
        }
      });
      fail(`R5: Session panel lifecycle failed: ${error instanceof Error ? error.message : String(error)}; frames=${JSON.stringify(frames)}`);
    }
    if (!created.sessionId || created.sessionId === created.beforeSessionId) {
      fail('R5: fresh session B identity missing');
    }
    await waitFor(
      page,
      () => document.getElementById('realtime-transcript') === null
        && document.getElementById('mic-btn')?.getAttribute('aria-pressed') === 'false',
      'R5: A→B switch did not tear down the realtime overlay and mic state'
    );
    const stopFrame = await waitForSentFrame(
      sentFrames,
      'realtime_stop',
      sentBeforeSwitch,
      15000,
      'R5: session switch never dispatched realtime_stop'
    );
    if (stopFrame.sessionId !== created.beforeSessionId) {
      fail(`R5: realtime_stop targeted ${stopFrame.sessionId}, expected owner A ${created.beforeSessionId}`);
    }
    await page.fill('#prompt-input', 'session B draft');
    await page.evaluate(() => window.__realtimeOwnerTest.sendLateFinal('late session A realtime transcript'));
    await page.waitForTimeout(300);
    const bComposer = await page.inputValue('#prompt-input');
    if (bComposer !== 'session B draft') {
      fail(`R5: late A realtime final polluted B composer (${JSON.stringify(bComposer)})`);
    }
    await page.screenshot({ path: `${evidence}/realtime-session-switch-isolated.png`, fullPage: true });
    console.log('web-realtime-rpc (ok): PASSED (realtime RPC/proxy lifecycle + GA transcription session.update + delta/segment/final/failed journey into the composer with no auto-send + A→B teardown targets A + late A transcript cannot pollute B)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-realtime-rpc (${phase}): FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
