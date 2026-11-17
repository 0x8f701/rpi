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
//   S1  #mic-btn is Hold to talk. While its getUserMedia is pending,
//       #live-btn stays Start live voice and disabled; a click sends no
//       realtime_create_call. Pointer release BEFORE that acquisition
//       resolves must stop every track of the old stream and must not start
//       MediaRecorder or dispatch stt_transcribe. The second hold records;
//       pointerup then immediate pointerleave, while the proxy still holds
//       stt_transcribe, submits that transcript exactly once and re-enables
//       mic and live. The composer is cleared by that existing auto-submit.
//   S2  the Rust proxy reached the mock /v1/audio/transcriptions with the
//       server-held Bearer + multipart WAV + model (evidence metadata only)
//   S3  switching A→B while transcription is held keeps the late result off B
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

    const idleVoice = await page.evaluate(() => {
      const describe = (node) => node ? {
        label: node.getAttribute('aria-label'),
        pressed: node.getAttribute('aria-pressed'),
        disabled: node.disabled === true,
      } : null;
      return {
        mic: describe(document.getElementById('mic-btn')),
        live: describe(document.getElementById('live-btn')),
      };
    });
    if (!idleVoice.mic) fail('S1: #mic-btn missing');
    if (!idleVoice.live) fail('S1: #live-btn missing');
    if (idleVoice.mic.label !== 'Hold to talk' || idleVoice.mic.pressed !== 'false') {
      fail(`S1: #mic-btn is not idle Hold to talk (${JSON.stringify(idleVoice.mic)})`);
    }
    if (idleVoice.live.label !== 'Start live voice' || idleVoice.live.pressed !== 'false') {
      fail(`S1: #live-btn is not idle Start live voice (${JSON.stringify(idleVoice.live)})`);
    }
    if (idleVoice.live.disabled) {
      fail('S1: #live-btn was already disabled before STT getUserMedia (cannot prove the pending-setup block)');
    }
    const gumInstalled = await page.evaluate(() => {
      const devices = navigator.mediaDevices;
      if (!devices || typeof devices.getUserMedia !== 'function') return 'missing';
      const native = devices.getUserMedia.bind(devices);
      const state = {
        calls: 0,
        held: false,
        released: false,
        recordersStarted: 0,
        recordersStopped: 0,
        recorderOnStops: 0,
        recorderErrors: 0,
        dataSizes: [],
        recorderStateAtStop: null,
        tracksStopped: 0,
        tracksLive: 0,
        acquisitions: [],
      };
      const NativeMediaRecorder = window.MediaRecorder;
      if (typeof NativeMediaRecorder === 'function' && !NativeMediaRecorder.__sttObserved) {
        const ObservedMediaRecorder = function ObservedMediaRecorder(stream, options) {
          const recorder = new NativeMediaRecorder(stream, options);
          const nativeStart = recorder.start.bind(recorder);
          const nativeStop = recorder.stop.bind(recorder);
          recorder.addEventListener('dataavailable', (event) => {
            state.dataSizes.push(event.data?.size ?? 0);
          });
          recorder.addEventListener('stop', () => {
            state.recorderOnStops += 1;
            state.recorderStateAtStop = recorder.state;
          });
          recorder.addEventListener('error', () => {
            state.recorderErrors += 1;
          });
          recorder.start = (...args) => {
            state.recordersStarted += 1;
            state.recorderStartedAt = performance.now();
            return nativeStart(...args);
          };
          recorder.stop = (...args) => {
            state.recordersStopped += 1;
            return nativeStop(...args);
          };
          return recorder;
        };
        ObservedMediaRecorder.prototype = NativeMediaRecorder.prototype;
        ObservedMediaRecorder.isTypeSupported = NativeMediaRecorder.isTypeSupported.bind(NativeMediaRecorder);
        ObservedMediaRecorder.__sttObserved = true;
        window.MediaRecorder = ObservedMediaRecorder;
      }
      const wrapped = (constraints) => {
        const call = ++state.calls;
        const acquisition = {
          call,
          settled: false,
          trackCount: 0,
          stopped: 0,
          run: null,
          stream: null,
          releaseStream: null,
        };
        state.acquisitions.push(acquisition);
        state.held = true;
        return new Promise((resolve, reject) => {
          acquisition.run = () => native(constraints).then((stream) => {
            acquisition.stream = stream;
            for (const track of stream.getTracks()) {
              state.tracksLive += 1;
              acquisition.trackCount += 1;
              const nativeStop = track.stop.bind(track);
              track.stop = () => {
                if (track.readyState !== 'ended') {
                  acquisition.stopped += 1;
                  state.tracksStopped += 1;
                  state.tracksLive -= 1;
                }
                nativeStop();
              };
            }
            return new Promise((releaseStream) => {
              acquisition.releaseStream = () => {
                acquisition.settled = true;
                acquisition.releaseStream = null;
                releaseStream();
                resolve(stream);
              };
            });
          }, reject);
        });
      };
      try {
        devices.getUserMedia = wrapped;
      } catch {
        try {
          Object.defineProperty(devices, 'getUserMedia', { configurable: true, writable: true, value: wrapped });
        } catch {
          return 'unpatchable';
        }
      }
      window.__sttGumState = state;
      window.__sttGumRelease = (call) => {
        const queued = state.acquisitions.filter((entry) => (
          typeof entry.run === 'function' && entry.stream === null && (call == null || entry.call === call)
        ));
        if (queued.length === 0) return false;
        for (const entry of queued) entry.run();
        return true;
      };
      window.__sttGumDeliver = (call) => {
        const entry = state.acquisitions.find((candidate) => candidate.call === call && typeof candidate.releaseStream === 'function');
        if (!entry) return false;
        entry.releaseStream();
        const stillPending = state.acquisitions.some((candidate) => candidate.settled !== true);
        state.held = stillPending;
        state.released = !stillPending;
        return true;
      };
      return 'ok';
    });
    if (gumInstalled !== 'ok') fail(`S1: could not hold getUserMedia (${gumInstalled})`);
    const framesBeforeHold = sentFrames.length;
    const box = await page.locator('#mic-btn').boundingBox();
    if (!box) fail('S1: #mic-btn has no bounding box');
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
    await page.mouse.down();
    await waitFor(
      page,
      () => window.__sttGumState?.held === true && window.__sttGumState?.calls >= 1,
      'S1: Hold to talk never called getUserMedia',
    );
    await waitFor(
      page,
      () => {
        const live = document.getElementById('live-btn');
        return live?.disabled === true && live.getAttribute('aria-label') === 'Start live voice';
      },
      'S1: Live stayed enabled while STT getUserMedia was pending',
    );
    const callsBeforeLive = await page.evaluate(() => window.__sttGumState?.calls ?? 0);
    await page.evaluate(() => document.getElementById('live-btn')?.click());
    const callsAfterLive = await page.evaluate(() => window.__sttGumState?.calls ?? 0);
    if (callsAfterLive !== callsBeforeLive) {
      fail(`S1: Live click started another getUserMedia while STT setup was pending (${callsBeforeLive} -> ${callsAfterLive})`);
    }
    const voiceDuringHold = sentFrames.slice(framesBeforeHold).some((frame) => {
      try {
        const type = JSON.parse(frame).type;
        return type === 'realtime_create_call' || type === 'stt_transcribe';
      } catch { return false; }
    });
    if (voiceDuringHold) fail('S1: voice RPC dispatched while STT getUserMedia was pending');
    const framesAtPointerRelease = sentFrames.length;
    await page.mouse.up();
    const pointerReleased = await page.evaluate(() => {
      const mic = document.getElementById('mic-btn');
      const live = document.getElementById('live-btn');
      return mic?.getAttribute('aria-pressed') !== 'true'
        && live?.disabled !== true
        && live?.getAttribute('aria-label') === 'Start live voice';
    });
    if (!pointerReleased) fail('S1: pointer release did not clear STT busy before getUserMedia resolved');
    if (await page.evaluate(() => window.__sttGumRelease?.(1)) !== true) {
      fail('S1: the deferred first getUserMedia acquisition could not be continued');
    }
    await waitFor(
      page,
      () => {
        const first = window.__sttGumState?.acquisitions?.find((entry) => entry.call === 1);
        return first?.stream !== null && first.trackCount > 0 && first.settled !== true && typeof first.releaseStream === 'function';
      },
      'S1: deferred getUserMedia never produced the old stream',
    );
    const callsBeforeSecond = await page.evaluate(() => window.__sttGumState?.calls ?? 0);
    const secondBox = await page.locator('#mic-btn').boundingBox();
    if (!secondBox) fail('S1: #mic-btn has no bounding box for the second hold');
    await page.mouse.move(secondBox.x + secondBox.width / 2, secondBox.y + secondBox.height / 2);
    await page.mouse.down();
    await waitFor(
      page,
      () => window.__sttGumState?.calls === 2
        && window.__sttGumState?.acquisitions?.some((entry) => entry.call === 2 && entry.stream === null),
      'S1: second hold never started its own pending getUserMedia',
    );
    if (callsBeforeSecond !== 1) {
      fail(`S1: first hold did not account for exactly one getUserMedia before the second hold (${callsBeforeSecond})`);
    }
    if (await page.evaluate(() => window.__sttGumDeliver?.(1)) !== true) {
      fail('S1: old getUserMedia stream could not be delivered during the second hold');
    }
    await waitFor(
      page,
      () => {
        const first = window.__sttGumState?.acquisitions?.find((entry) => entry.call === 1);
        const second = window.__sttGumState?.acquisitions?.find((entry) => entry.call === 2);
        const live = document.getElementById('live-btn');
        return first?.settled === true
          && first.trackCount > 0
          && first.stopped === first.trackCount
          && second?.settled !== true
          && second?.stream === null
          && live?.disabled === true
          && (window.__sttGumState?.recordersStarted ?? -1) === 0
          && (window.__sttGumState?.tracksLive ?? -1) === 0;
      },
      'S1: old acquisition completion cleared the second hold, left tracks live, or started a recorder',
    );
    if (sentFrames.slice(framesAtPointerRelease).some((frame) => {
      try { return JSON.parse(frame).type === 'stt_transcribe'; } catch { return false; }
    })) {
      fail('S1: old stream delivery dispatched stt_transcribe');
    }
    if (await page.evaluate(() => window.__sttGumRelease?.(2)) !== true) {
      fail('S1: the second getUserMedia acquisition could not be continued');
    }
    await waitFor(
      page,
      () => {
        const second = window.__sttGumState?.acquisitions?.find((entry) => entry.call === 2);
        return second?.stream !== null && second.trackCount > 0 && typeof second.releaseStream === 'function';
      },
      'S1: second getUserMedia never produced its stream',
    );
    if (await page.evaluate(() => window.__sttGumDeliver?.(2)) !== true) {
      fail('S1: second getUserMedia stream could not be delivered');
    }
    await waitFor(
      page,
      () => {
        const second = window.__sttGumState?.acquisitions?.find((entry) => entry.call === 2);
        return second?.settled === true
          && (window.__sttGumState?.recordersStarted ?? 0) >= 1
          && document.getElementById('mic-btn')?.getAttribute('aria-pressed') === 'true';
      },
      'S1: second hold never started recording',
    );
    await page.waitForTimeout(900);
    const captureMs = await page.evaluate(() => performance.now() - (window.__sttGumState?.recorderStartedAt ?? performance.now()));
    if (!(captureMs >= 300)) fail(`S1: second recording was released before a real capture (${captureMs}ms)`);
    await page.mouse.up();
    await page.locator('#mic-btn').dispatchEvent('pointerleave');
    await waitFor(
      page,
      () => document.getElementById('mic-btn')?.getAttribute('aria-pressed') !== 'true',
      'S1: pointerup did not release the second recording',
    );

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
      const micTrace = await page.evaluate(() => {
        const mic = document.getElementById('mic-btn');
        const live = document.getElementById('live-btn');
        const gum = window.__sttGumState;
        return {
          recordersStarted: gum?.recordersStarted ?? null,
          recordersStopped: gum?.recordersStopped ?? null,
          recorderOnStops: gum?.recorderOnStops ?? null,
          recorderErrors: gum?.recorderErrors ?? null,
          recorderStateAtStop: gum?.recorderStateAtStop ?? null,
          dataSizes: gum?.dataSizes ?? null,
          tracksStopped: gum?.tracksStopped ?? null,
          tracksLive: gum?.tracksLive ?? null,
          acquisitions: (gum?.acquisitions ?? []).map((entry) => ({
            call: entry.call,
            settled: entry.settled,
            trackCount: entry.trackCount,
            stopped: entry.stopped,
            hasStream: entry.stream !== null,
          })),
          mic: mic ? {
            pressed: mic.getAttribute('aria-pressed'),
            disabled: mic.disabled === true,
            className: mic.className,
            title: mic.getAttribute('title'),
          } : null,
          live: live ? {
            label: live.getAttribute('aria-label'),
            disabled: live.disabled === true,
          } : null,
          toasts: Array.from(document.querySelectorAll('.toast')).map((node) => node.textContent || ''),
        };
      });
      fail(`S1: stt_transcribe RPC never dispatched (sent frames: ${sentFrames.slice(-5).join(' | ')}; mic=${JSON.stringify(micTrace)})`);
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
    if (sentFrames.some((frame) => {
      try { return JSON.parse(frame).type === 'realtime_create_call'; } catch { return false; }
    })) {
      fail('S1: hold-to-talk dispatched realtime_create_call');
    }

    if (phase === 'ok') {
      await waitForNode(
        () => fs.existsSync(sttEvidence),
        'S1: held STT request never reached the proxy before pointerleave recovery',
      );
      await releaseStt();
      const framesBeforeTranscript = sentFrames.length;
      await waitForNode(
        () => sentFrames.slice(framesBeforeTranscript).filter((raw) => {
          try {
            const frame = JSON.parse(raw);
            return (frame.type === 'prompt' || frame.type === 'steer') && frame.message === 'web stt transcript';
          } catch { return false; }
        }).length === 1,
        'S1: held STT transcript was not submitted exactly once after pointerleave',
      );
      const submitted = sentFrames.slice(framesBeforeTranscript).filter((raw) => {
        try {
          const frame = JSON.parse(raw);
          return (frame.type === 'prompt' || frame.type === 'steer') && frame.message === 'web stt transcript';
        } catch { return false; }
      });
      if (submitted.length !== 1) fail(`S1: held STT transcript submissions=${submitted.length}`);
      const composer = await page.inputValue('#prompt-input');
      if (composer !== '') fail(`S1: auto-submitted STT transcript remained in the composer (${JSON.stringify(composer)})`);
      await waitFor(
        page,
        () => {
          const mic = document.getElementById('mic-btn');
          const live = document.getElementById('live-btn');
          return mic?.disabled !== true
            && mic?.getAttribute('aria-pressed') !== 'true'
            && live?.disabled !== true
            && live?.getAttribute('aria-label') === 'Start live voice';
        },
        'S1: mic or live stayed disabled after pointerleave during the held STT response',
      );
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

    // ---- S3: a new hold after S1 is still owned by A. Its late result
    // must not mutate B. ----
    const s1Evidence = fs.statSync(sttEvidence);
    const framesBeforeThird = sentFrames.length;
    const thirdBox = await page.locator('#mic-btn').boundingBox();
    if (!thirdBox) fail('S3: #mic-btn has no bounding box for the third hold');
    await page.mouse.move(thirdBox.x + thirdBox.width / 2, thirdBox.y + thirdBox.height / 2);
    await page.mouse.down();
    await waitFor(
      page,
      () => window.__sttGumState?.calls === 3
        && window.__sttGumState?.acquisitions?.some((entry) => entry.call === 3 && entry.stream === null),
      'S3: third hold never started its own pending getUserMedia',
    );
    if (await page.evaluate(() => window.__sttGumRelease?.(3)) !== true) {
      fail('S3: the third getUserMedia acquisition could not be continued');
    }
    await waitFor(
      page,
      () => {
        const third = window.__sttGumState?.acquisitions?.find((entry) => entry.call === 3);
        return third?.stream !== null && third.trackCount > 0 && typeof third.releaseStream === 'function';
      },
      'S3: third getUserMedia never produced its stream',
    );
    if (await page.evaluate(() => window.__sttGumDeliver?.(3)) !== true) {
      fail('S3: third getUserMedia stream could not be delivered');
    }
    await waitFor(
      page,
      () => (window.__sttGumState?.recordersStarted ?? 0) >= 2
        && document.getElementById('mic-btn')?.getAttribute('aria-pressed') === 'true',
      'S3: third hold never started recording',
    );
    await page.waitForTimeout(900);
    const thirdCaptureMs = await page.evaluate(() => performance.now() - (window.__sttGumState?.recorderStartedAt ?? performance.now()));
    if (!(thirdCaptureMs >= 300)) fail(`S3: third recording was released before a real capture (${thirdCaptureMs}ms)`);
    await page.mouse.up();
    let thirdPayload = null;
    await waitForNode(
      () => {
        thirdPayload = sentFrames.slice(framesBeforeThird).map((raw) => {
          try { return JSON.parse(raw); } catch { return null; }
        }).find((frame) => frame?.type === 'stt_transcribe') ?? null;
        return thirdPayload?.id && thirdPayload.id !== transcribePayload.id;
      },
      'S3: third hold never dispatched its own stt_transcribe',
    );
    await waitForNode(
      () => {
        try { return fs.statSync(sttEvidence).mtimeMs > s1Evidence.mtimeMs; } catch { return false; }
      },
      'S3: third STT request never registered a new proxy hold',
    );
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
    if (created.beforeSessionId !== thirdPayload.sessionId) {
      fail(`S3: third STT owner ${thirdPayload.sessionId} did not match session A ${created.beforeSessionId}`);
    }
    await page.fill('#prompt-input', 'session B draft');
    const framesBeforeLateRelease = sentFrames.length;
    await releaseStt();
    await waitForNode(
      () => receivedFrames.some((raw) => {
        try { return JSON.parse(raw).id === thirdPayload.id; } catch { return false; }
      }),
      'S3: third STT command never settled after release',
    );
    const lateSubmits = sentFrames.slice(framesBeforeLateRelease).filter((raw) => {
      try {
        const frame = JSON.parse(raw);
        return (frame.type === 'prompt' || frame.type === 'steer') && frame.message === 'web stt transcript';
      } catch { return false; }
    });
    if (lateSubmits.length !== 0) fail(`S3: late A transcript submitted on B (${lateSubmits.length})`);
    const bComposer = await page.inputValue('#prompt-input');
    if (bComposer !== 'session B draft') {
      fail(`S3: late A transcript polluted B composer (${JSON.stringify(bComposer)})`);
    }

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
