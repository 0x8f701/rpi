// Web realtime RPC-level start/error/stop regression lane (playwright half
// of E2E.d/web/realtime_rpc.sh).
//
// Environment:
//   RPI_URL             http://127.0.0.1:<port>/web
//   RPI_TOKEN           token file content (served via rpi-auth.<token> subprotocol)
//   RPI_PHASE           "ok" | "error"
//   RPI_PROXY_EVIDENCE  mock-persisted proxy evidence JSON ({callId,
//                       openaiAlpha, authPresent, sessionHasModel})
//                       written when the Rust realtime_create_call proxy
//                       reached the mock
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
//       OpenAI-Alpha quicksilver=v2 + a Bearer token, and the create-call
//       session is model-free (sessionHasModel must be exactly false)
//   R3  the live overlay renders (#realtime-transcript with the
//       .realtime-transcript__label "realtime voice") and
//       #realtime-conn-state exposes a non-empty state bucket
//   R5  clicking #mic-btn again dispatches realtime_stop and the overlay
//       disappears
// error phase:
//   R4  the mock rejects with 500; the page surfaces the "realtime call
//       failed" toast and #realtime-transcript never renders

import { chromium } from 'playwright';
import fs from 'node:fs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const phase = process.env.RPI_PHASE || 'ok';
const proxyEvidence = process.env.RPI_PROXY_EVIDENCE || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

function fail(message) {
  console.error(`web-realtime-rpc (${phase}): FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await page.evaluate(fn)) return;
    await page.waitForTimeout(120);
  }
  fail(`${label} (timeout ${timeoutMs}ms)`);
}

/** Deterministic in-page RTCPeerConnection stub. iceGatheringState is
 *  'complete' from construction so waitForIceGatheringComplete resolves
 *  immediately; localDescription carries gathered a=candidate lines so the
 *  POSTed offer looks gathered; setRemoteDescription resolves (the real
 *  answer correctness is covered by the realtime_webrtc lane). */
const FAKE_RTC = `(() => {
  class FakeDataChannel {
    constructor(label) {
      this.label = label;
      this.readyState = 'open';
      this.onopen = null;
      this.onmessage = null;
      this.onerror = null;
      this.onclose = null;
    }
    send() {}
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
    page.on('websocket', (ws) => {
      ws.on('framesent', (frame) => {
        const payload = typeof frame.payload === 'string' ? frame.payload : '';
        if (payload) sentFrames.push(payload);
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
        connState: document.getElementById('conn-state')?.dataset?.state || null,
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
    // R2 also requires the create-call session to be model-free: the proxy
    // selects the model server-side, so a request carrying session.model is
    // the exact regression this lane guards. The mock rejects such requests
    // with a 400 and records sessionHasModel=true; a successful call must
    // record exactly false. Requiring `=== false` (not merely falsy) means a
    // stale mock that never writes the field cannot silently pass.
    if (proxy.sessionHasModel === true) {
      fail('R2: proxy request carried session.model (create-call session must be model-free)');
    }
    if (proxy.sessionHasModel !== false) {
      fail(`R2: proxy evidence missing sessionHasModel=false (got ${proxy.sessionHasModel}) — stale mock without the model guard`);
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

    // ---- R5: click again -> realtime_stop + overlay gone ----
    const sentBeforeStop = sentFrames.length;
    await page.click('#mic-btn');
    await waitFor(
      page,
      () => document.getElementById('realtime-transcript') === null,
      'R5: the realtime overlay did not disappear after the second click'
    );
    let sawStop = false;
    const stopDeadline = Date.now() + 15000;
    while (Date.now() < stopDeadline && !sawStop) {
      for (let i = sentBeforeStop; i < sentFrames.length; i++) {
        try {
          if (JSON.parse(sentFrames[i]).type === 'realtime_stop') { sawStop = true; break; }
        } catch { /* non-JSON frame */ }
      }
      if (!sawStop) await page.waitForTimeout(200);
    }
    if (!sawStop) {
      fail(`R5: realtime_stop RPC never dispatched (frames after stop: ${sentFrames.slice(sentBeforeStop).join(' | ')})`);
    }
    const micIdle = await page.evaluate(() => document.getElementById('mic-btn')?.getAttribute('aria-pressed'));
    if (micIdle !== 'false') fail(`R5: mic did not return to the idle state after stop (aria-pressed=${micIdle})`);
    await page.screenshot({ path: `${evidence}/realtime-stopped.png`, fullPage: true });

    console.log('web-realtime-rpc (ok): PASSED (realtime_create_call RPC + real proxy quicksilver evidence + live overlay + conn-state bucket + realtime_stop RPC + overlay gone)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-realtime-rpc (${phase}): FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
