// Web hold-to-talk STT RPC lane (playwright half of E2E.d/web/stt_rpc.sh).
//
// Environment:
//   RPI_URL            http://127.0.0.1:<port>/web
//   RPI_TOKEN          token file content (served via rpi-auth.<token> subprotocol)
//   RPI_PHASE          "ok" | "error"
//   RPI_STT_EVIDENCE   mock-persisted STT evidence JSON ({authPresent,
//                      contentType, filePresent, wavPresent, modelPresent})
//                      written when the Rust stt_transcribe proxy reached the
//                      mock — metadata only, never the key or the audio body
//   RPI_FIXTURE_KEY    the fixture STT key placeholder (asserted to NEVER
//                      appear in the DOM or the evidence)
//   RPI_CHROME         executable path of the system Chrome (optional)
//   RPI_EVIDENCE       evidence dir for screenshots
//
// ok phase:
//   S1  #mic-btn is in hold-to-talk (STT) mode; a pointer hold records via
//       the synthetic mic and release dispatches stt_transcribe on the WS
//       with ONLY {audioBase64, mimeType: "audio/wav"}
//   S2  the Rust proxy reached the mock /v1/audio/transcriptions with the
//       server-held Bearer + multipart WAV + model (evidence metadata only)
//   S3  the returned transcript lands in the composer (#prompt-input)
//   S4  the fixture key appears nowhere in the page DOM or the evidence
// error phase (MOCK_STT_ERROR=1):
//   S5  the mock 500 surfaces the bounded "transcription failed" toast and
//       no transcript lands in the composer; the key is still absent

import { chromium } from 'playwright';
import fs from 'node:fs';
import { createFreshSessionViaManage } from './lib/session-lifecycle.mjs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const phase = process.env.RPI_PHASE || 'ok';
const sttEvidence = process.env.RPI_STT_EVIDENCE || '';
const fixtureKey = process.env.RPI_FIXTURE_KEY || 'stt-rpc-fixture-key';
const sttBaseUrlFixture = process.env.RPI_STT_BASE_URL || 'http://127.0.0.1:0';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const releaseUrl = process.env.RPI_STT_RELEASE_URL || '';

function fail(message) {
  console.error(`web-stt-rpc (${phase}): FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await page.evaluate(fn, arg)) return;
    await page.waitForTimeout(120);
  }
  throw new Error(`${label} (timeout ${timeoutMs}ms)`);
}

/** Node-side poll for conditions over captured WS frames/DOM — never runs in
 *  the browser context, so closures may reference Node variables like the
 *  receivedFrames array without a page-side ReferenceError. */
async function waitForNode(fn, label, timeoutMs = 25000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await fn()) return;
    await new Promise((resolve) => setTimeout(resolve, 120));
  }
  fail(`${label} (timeout ${timeoutMs}ms)`);
}

async function releaseStt() {
  if (!releaseUrl) fail('RPI_STT_RELEASE_URL is required for the owner-isolation phase');
  const response = await fetch(releaseUrl, { method: 'POST' });
  if (!response.ok) fail(`STT release endpoint returned ${response.status}`);
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = {
    args: [
      '--use-fake-device-for-media-stream',
      '--use-fake-ui-for-media-stream',
      '--autoplay-policy=no-user-gesture-required',
    ],
  };
  if (chromePath) launchOptions.executablePath = chromePath;
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage();
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    page.on('pageerror', (err) => {
      console.error(`web-stt-rpc: page error: ${err.message}`);
    });

    // Capture outgoing WS frames so the real stt_transcribe payload can be
    // observed on the wire (deterministic — never a source-text assertion).
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
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'WS did not reach "connected"'
    );
    await waitFor(page, () => document.getElementById('mic-btn') !== null, 'mic button missing');

    // ---- S0: the browser's runtime state projects ONLY the safe live
    // fields — enabled/mode/sttConfigured/realtimeConfigured/model/voice.
    // Endpoint URLs, API keys, and secret-bearing settings never cross the
    // wire (the fixture endpoint/key must appear nowhere in any frame). ----
    await waitForNode(
      () =>
        receivedFrames.some((f) => {
          try {
            const parsed = JSON.parse(f);
            return (
              parsed &&
              typeof parsed === 'object' &&
              parsed.type === 'response' &&
              parsed.data &&
              parsed.data.runtimeSettings &&
              parsed.data.runtimeSettings.live
            );
          } catch {
            return false;
          }
        }),
      'S0: a get_state frame with runtimeSettings.live was never received',
      15000
    );
    const liveBlocks = receivedFrames
      .map((f) => {
        try {
          return JSON.parse(f);
        } catch {
          return null;
        }
      })
      .filter(
        (p) =>
          p &&
          typeof p === 'object' &&
          p.type === 'response' &&
          p.data &&
          p.data.runtimeSettings &&
          p.data.runtimeSettings.live &&
          typeof p.data.runtimeSettings.live === 'object'
      )
      .map((p) => p.data.runtimeSettings.live);
    const live = liveBlocks[liveBlocks.length - 1];
    if (!live) fail('S0: no live settings block captured');
    if (live.mode !== 'stt' || live.enabled !== true) fail(`S0: live block wrong (${JSON.stringify(live)})`);
    if (live.sttConfigured !== true) fail(`S0: live.sttConfigured not projected true (${JSON.stringify(live)})`);
    if (live.realtimeConfigured !== false) fail(`S0: live.realtimeConfigured not projected false (${JSON.stringify(live)})`);
    if (typeof live.realtimeModel !== 'string') fail(`S0: live model missing (${JSON.stringify(live)})`);
    if (typeof live.voice !== 'string') fail(`S0: live voice missing (${JSON.stringify(live)})`);
    const wireText = receivedFrames.join('\n');
    for (const banned of ['sttBaseUrl', 'sttApiKey', 'realtimeBaseUrl', 'realtimeApiKey']) {
      if (wireText.includes(banned)) fail(`S0: banned wire field ${banned} reached the browser`);
    }
    if (wireText.includes(fixtureKey)) fail('S0: the fixture key reached the browser');
    // The bare mock base URL is shared with the steering provider (a
    // legitimate wire value); the STT-specific shape is the endpoint WITH
    // its path — that must never reach the browser.
    if (wireText.includes(`${sttBaseUrlFixture}/v1/audio/transcriptions`)) {
      fail(`S0: the fixture STT endpoint (with path) reached the browser`);
    }

    // Hold-to-talk: press and hold the mic (synthetic device records), then
    // release; the recorder stops, the capture is converted to WAV, and
    // stt_transcribe is dispatched on the WS.
    const box = await page.locator('#mic-btn').boundingBox();
    if (!box) fail('mic button has no bounding box');
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
    await page.mouse.down();
    await page.waitForTimeout(900);
    await page.mouse.up();

    // ---- S1: stt_transcribe RPC with ONLY {audioBase64, mimeType} ----
    let sawTranscribe = false;
    let transcribePayload = null;
    const deadline = Date.now() + 20000;
    while (Date.now() < deadline && !sawTranscribe) {
      for (const frame of sentFrames) {
        try {
          const parsed = JSON.parse(frame);
          if (parsed.type === 'stt_transcribe') {
            transcribePayload = parsed;
            sawTranscribe = true;
            break;
          }
        } catch { /* non-JSON frame */ }
      }
      if (!sawTranscribe) await page.waitForTimeout(200);
    }
    if (!sawTranscribe) {
      fail(`S1: stt_transcribe RPC never dispatched (sent frames: ${sentFrames.slice(-5).join(' | ')})`);
    }
    if (transcribePayload.mimeType !== 'audio/wav') {
      fail(`S1: stt_transcribe mimeType not audio/wav (${JSON.stringify(transcribePayload)})`);
    }
    if (typeof transcribePayload.audioBase64 !== 'string' || transcribePayload.audioBase64.length === 0) {
      fail(`S1: stt_transcribe missing non-empty audioBase64 (${JSON.stringify(transcribePayload)})`);
    }
    // The browser sends ONLY the audio + MIME — never an endpoint URL or key.
    const payloadKeys = Object.keys(transcribePayload).sort();
    if (payloadKeys.some((key) => /url|key|token/i.test(key))) {
      fail(`S1: stt_transcribe payload carries a URL/key-shaped field (${JSON.stringify(transcribePayload)})`);
    }


    if (phase === 'owner-error') {
      const evidenceDeadline = Date.now() + 15000;
      while (Date.now() < evidenceDeadline && !fs.existsSync(sttEvidence)) {
        await page.waitForTimeout(200);
      }
      if (!fs.existsSync(sttEvidence)) fail('S5: delayed-error proxy evidence never appeared');
      const ownerA = transcribePayload.sessionId;
      const created = await createFreshSessionViaManage(page, waitFor, 'S5: create session B during delayed transcription failure');
      if (created.beforeSessionId !== ownerA) {
        fail(`S5: delayed STT owner ${ownerA} did not match session A ${created.beforeSessionId}`);
      }
      await page.fill('#prompt-input', 'session B error draft');
      await releaseStt();
      const responseDeadline = Date.now() + 15000;
      while (Date.now() < responseDeadline && !receivedFrames.some((raw) => {
        try { const frame = JSON.parse(raw); return frame.type === 'response' && frame.id === transcribePayload.id; } catch { return false; }
      })) {
        await page.waitForTimeout(100);
      }
      if (!receivedFrames.some((raw) => {
        try { const frame = JSON.parse(raw); return frame.type === 'response' && frame.id === transcribePayload.id; } catch { return false; }
      })) fail('S5: delayed STT command never settled after release');
      const composer = await page.inputValue('#prompt-input');
      if (composer !== 'session B error draft') {
        fail(`S5: late A failure changed B composer (${JSON.stringify(composer)})`);
      }
      const toasts = await page.locator('.toast').allTextContents();
      if (toasts.length > 0) {
        fail(`S5: late A transcription failure surfaced toast(s) on B (${JSON.stringify(toasts)})`);
      }
      await page.screenshot({ path: `${evidence}/stt-session-switch-error-isolated.png`, fullPage: true });

      console.log('web-stt-rpc (owner-error): PASSED (delayed A failure stays silent after A→B switch)');
      return;
    }
    if (phase === 'error') {
      // ---- S5: mock 500 -> bounded transcription-failed toast, no
      // transcript, but the proxy path is still PROVEN: the evidence (written
      // before the error knob) records the server-held Bearer + multipart
      // WAV + model metadata, and contains no key or audio body ----
      const evidenceDeadline = Date.now() + 15000;
      let meta = null;
      while (Date.now() < evidenceDeadline) {
        try {
          meta = JSON.parse(fs.readFileSync(sttEvidence, 'utf8'));
          break;
        } catch {
          await page.waitForTimeout(200);
        }
      }
      if (!meta) fail(`S5: STT evidence never appeared (proxy never reached the mock): ${sttEvidence}`);
      if (meta.authPresent !== true) fail(`S5: the backend did not send the server-held Bearer (${JSON.stringify(meta)})`);
      if (!meta.contentType || !meta.contentType.startsWith('multipart/form-data')) {
        fail(`S5: the backend did not forward the multipart form (${JSON.stringify(meta)})`);
      }
      if (meta.filePresent !== true || meta.wavPresent !== true || meta.modelPresent !== true) {
        fail(`S5: forwarded form metadata incomplete (${JSON.stringify(meta)})`);
      }
      const evidenceText = fs.readFileSync(sttEvidence, 'utf8');
      if (evidenceText.includes(fixtureKey)) fail('S5: the fixture key leaked into the evidence file');

      await waitFor(
        page,
        () =>
          Array.from(document.querySelectorAll('.toast')).some((el) =>
            (el.textContent || '').includes('transcription failed')
          ),
        'S5: the transcription-failed toast never appeared after the mock 500',
        20000
      );
      const composer = await page.evaluate(() => document.getElementById('prompt-input')?.value || '');
      if (composer.includes('web stt transcript')) fail('S5: transcript landed despite the mock error');
      const domText = await page.evaluate(() => document.body.innerText || '');
      if (domText.includes(fixtureKey)) fail('S5: the fixture key leaked into the page DOM');
      const toastText = await page.evaluate(() =>
        Array.from(document.querySelectorAll('.toast'))
          .map((el) => el.textContent || '')
          .join(' | ')
      );
      if (toastText.includes(fixtureKey)) fail(`S5: the fixture key leaked into the toast (${toastText})`);
      if (toastText.length > 2000) fail(`S5: toast unbounded (${toastText.length} chars)`);
      await page.screenshot({ path: `${evidence}/stt-error-toast.png`, fullPage: true });
      console.log('web-stt-rpc (error): PASSED (proxy evidence shows server-held Bearer + multipart WAV + model; mock 500 -> bounded transcription-failed toast, no transcript, key absent from DOM/evidence/toast)');
      return;
    }

    // ---- S2: the Rust proxy reached the mock (metadata-only evidence) ----
    const evidenceDeadline = Date.now() + 15000;
    let meta = null;
    while (Date.now() < evidenceDeadline) {
      try {
        meta = JSON.parse(fs.readFileSync(sttEvidence, 'utf8'));
        break;
      } catch {
        await page.waitForTimeout(200);
      }
    }
    if (!meta) fail(`S2: STT evidence file never appeared at ${sttEvidence}`);
    if (meta.authPresent !== true) fail(`S2: the backend did not send the server-held Bearer (${JSON.stringify(meta)})`);
    if (!meta.contentType || !meta.contentType.startsWith('multipart/form-data')) {
      fail(`S2: the backend did not forward the multipart form (${JSON.stringify(meta)})`);
    }
    if (meta.filePresent !== true) fail(`S2: the file field is missing (${JSON.stringify(meta)})`);
    if (meta.wavPresent !== true) fail(`S2: the WAV bytes are missing from the forwarded form (${JSON.stringify(meta)})`);
    if (meta.modelPresent !== true) fail(`S2: the model field is missing (${JSON.stringify(meta)})`);

    // ---- S3: switch A→B while the backend transcription is still held.
    // The stt_transcribe command must stay targeted at A, and its late result
    // must not mutate B's composer. ----
    const transcribeSession = transcribePayload.sessionId;
    if (typeof transcribeSession !== 'string' || transcribeSession === '') {
      fail(`S3: stt_transcribe missing its captured session owner (${JSON.stringify(transcribePayload)})`);
    }
    const framesBeforeSessionPanel = sentFrames.length;
    let created;
    try {
      created = await createFreshSessionViaManage(page, waitFor, 'S3: create session B during transcription');
    } catch (error) {
      const frames = sentFrames.slice(framesBeforeSessionPanel).map((raw) => {
        try {
          const frame = JSON.parse(raw);
          return { type: frame.type, id: frame.id, sessionId: frame.sessionId };
        } catch {
          return { type: 'non-json' };
        }
      });
      fail(`S3: Session panel lifecycle failed: ${error instanceof Error ? error.message : String(error)}; frames=${JSON.stringify(frames)}`);
    }
    if (created.beforeSessionId !== transcribeSession) {
      fail(`S3: STT command owner ${transcribeSession} did not match session A ${created.beforeSessionId}`);
    }
    await page.fill('#prompt-input', 'session B draft');
    await releaseStt();
    await page.waitForTimeout(800);
    const bComposer = await page.inputValue('#prompt-input');
    if (bComposer !== 'session B draft') {
      fail(`S3: late A transcript polluted B composer (${JSON.stringify(bComposer)})`);
    }
    if (bComposer.includes('web stt transcript')) fail('S3: session A transcript landed in session B composer');
    await page.screenshot({ path: `${evidence}/stt-session-switch-isolated.png`, fullPage: true });

    // ---- S4: the fixture key never appears in the DOM or the evidence ----
    const domText = await page.evaluate(() => document.body.innerText || '');
    if (domText.includes(fixtureKey)) fail('S4: the fixture key leaked into the page DOM');
    const evidenceText = fs.readFileSync(sttEvidence, 'utf8');
    if (evidenceText.includes(fixtureKey)) fail('S4: the fixture key leaked into the evidence file');

    console.log('web-stt-rpc (ok): PASSED (synthetic mic -> owner-stamped STT RPC/proxy + A→B switch drops late A transcript + key absent)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-stt-rpc (${phase}): FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
