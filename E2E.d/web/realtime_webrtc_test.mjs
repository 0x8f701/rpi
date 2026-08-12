// Web realtime WebRTC protocol-level E2E lane (playwright half of
// E2E.d/web/realtime_webrtc.sh).
//
// Environment:
//   RPI_URL         http://127.0.0.1:<port>/web  (page context only; WS unused)
//   RPI_RT_HELPERS  path to the esbuild IIFE bundle of src/realtime.ts
//                   (exposes window.__rtHelpers)
//   RPI_CHROME      executable path of the system Chrome (optional)
//   RPI_EVIDENCE    evidence dir for the result JSON + screenshot
//
// This is the protocol-level REAL WebRTC scenario the acceptance requires
// (NOT FakePeerConnection): a real Chromium RTCPeerConnection loopback drives
// the same browser WebRTC + SDP/datachannel/audio-track code path the
// production realtime call uses, plus the REAL src/realtime.ts helpers
// (waitForIceGatheringComplete, classifyRealtimeConnectionState,
// setupRealtimeCall) bundled into the page. It proves the ICE-gather-then-POST
// fix works against the platform stack.
//
// Exit: 0 = the loopback + helper assertions held; 1 = setup failure
// (playwright/chromium/WebRTC API unavailable); 2 = assertion failure.

import { chromium } from 'playwright';
import fs from 'node:fs';

const url = process.env.RPI_URL;
const helpersPath = process.env.RPI_RT_HELPERS || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

function fail(message) {
  console.error(`web-realtime-webrtc: FAIL: ${message}`);
  process.exit(2);
}

function setupFail(message) {
  console.error(`web-realtime-webrtc: SETUP FAILED: ${message}`);
  process.exit(1);
}

// The browser-side loopback. Runs in the page (real chromium WebRTC). It uses
// real platform timers for the WebRTC event waits — this is the rule's
// integration-test exception (deterministic time control cannot drive a real
// ICE/DTLS stack); the node-side test file uses NO real timers.
const LOOPBACK = async () => {
  const H = window.__rtHelpers;
  const out = { steps: [], errors: [] };
  const log = (s) => out.steps.push(s);
  try {
    if (!H || typeof H.waitForIceGatheringComplete !== 'function') {
      out.errors.push('__rtHelpers missing or incomplete');
      return out;
    }

    // 1. Real getUserMedia (synthetic mic via --use-fake-device-for-media-stream)
    //    -> a real MediaStreamTrack the peer connection will negotiate.
    const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    out.tracks = stream.getTracks().length;
    out.trackKind = stream.getTracks()[0] && stream.getTracks()[0].kind;
    log('getUserMedia ok; tracks=' + out.tracks + ' kind=' + out.trackKind);

    // 2. Two REAL RTCPeerConnections (caller + answerer) on loopback. No fake.
    const caller = new RTCPeerConnection();
    const answerer = new RTCPeerConnection();
    caller.addTrack(stream.getTracks()[0], stream);
    const dc = caller.createDataChannel(H.REALTIME_EVENT_CHANNEL);
    out.channelLabel = dc.label;

    // 3. Real offer + the ICE-gather wait (the fix). Reading localDescription
    //    BEFORE the gather wait yields a candidate-less SDP (the old silent-
    //    failure root cause); AFTER the wait it carries a=candidate lines.
    const offer = await caller.createOffer();
    await caller.setLocalDescription(offer);
    const bareOfferHadCandidates = /a=candidate:/.test(caller.localDescription.sdp || '');
    await H.waitForIceGatheringComplete(caller, H.REALTIME_ICE_GATHER_TIMEOUT_MS);
    const gatheredSdp = caller.localDescription.sdp || '';
    out.bareOfferHadCandidates = bareOfferHadCandidates;
    out.gatheredOfferHasCandidates = /a=candidate:/.test(gatheredSdp);
    out.iceGatheringState = caller.iceGatheringState;
    log('ICE gather: bareHadCandidates=' + bareOfferHadCandidates +
        ' gatheredHasCandidates=' + out.gatheredOfferHasCandidates +
        ' state=' + caller.iceGatheringState);

    // 5. Wire event-driven completion: datachannel open (caller side),
    //    answerer ondatachannel, remote audio track arrival, connected state.
    //    Attach handlers BEFORE the answerer SRD so no async event is missed.
    out._dcOpenFired = false;
    out._ondatachannelFired = false;
    out._ontrackFired = false;
    out._connectedFired = false;
    const dcOpen = Promise.withResolvers();
    dc.onopen = () => { out._dcOpenFired = true; dcOpen.resolve('caller-open'); };
    let answererDc = null;
    const answererDcReady = Promise.withResolvers();
    answerer.ondatachannel = (e) => { answererDc = e.channel; out._ondatachannelFired = true; answererDcReady.resolve('answerer-dc'); };
    const remoteTrack = Promise.withResolvers();
    answerer.ontrack = (e) => {
      out._ontrackFired = true;
      // Keep the actual remote MediaStream for the real play() evidence.
      out._remoteStream = e.streams[0] || null;
      remoteTrack.resolve({ kind: e.track.kind, streams: e.streams.length, trackEnabled: e.track.enabled });
    };
    const connected = Promise.withResolvers();
    const settleConn = () => {
      const s = H.classifyRealtimeConnectionState(caller.connectionState);
      if (s === 'connected') { out._connectedFired = true; connected.resolve(s); }
    };
    settleConn();
    caller.onconnectionstatechange = settleConn;
    // 4. Real SDP exchange: answerer consumes the GATHERED offer (with
    //    candidates), produces an answer, gathers its own candidates.
    await answerer.setRemoteDescription({ type: 'offer', sdp: gatheredSdp });
    const answer = await answerer.createAnswer();
    await answerer.setLocalDescription(answer);
    await H.waitForIceGatheringComplete(answerer, H.REALTIME_ICE_GATHER_TIMEOUT_MS);
    await caller.setRemoteDescription({ type: 'answer', sdp: answerer.localDescription.sdp });

    // Safety bound so a stuck loopback rejects instead of hanging the lane.
    const safety = new Promise((_, rej) => setTimeout(() => {
      out.diag = {
        callerConn: caller.connectionState,
        callerIce: caller.iceConnectionState,
        callerSig: caller.signalingState,
        callerIceGather: caller.iceGatheringState,
        answererConn: answerer.connectionState,
        answererIce: answerer.iceConnectionState,
        answererSig: answerer.signalingState,
        dcReadyState: dc.readyState,
        answererDcPresent: !!answererDc,
        dcOpenFired: out._dcOpenFired,
        ondatachannelFired: out._ondatachannelFired,
        ontrackFired: out._ontrackFired,
        connectedFired: out._connectedFired,
      };
      rej(new Error('loopback timeout'));
    }, 10000));
    const [openRes, adRes, trackRes, connRes] = await Promise.race([
      Promise.all([dcOpen.promise, answererDcReady.promise, remoteTrack.promise, connected.promise]),
      safety,
    ]);
    out.datachannelOpen = openRes === 'caller-open';
    out.answererDc = adRes === 'answerer-dc';
    out.remoteTrack = trackRes;
    out.callerConnState = connRes;
    out.classifiedConn = H.classifyRealtimeConnectionState(caller.connectionState);
    log('loopback connected: dcOpen=' + out.datachannelOpen +
        ' answererDc=' + out.answererDc + ' remoteTrack=' + JSON.stringify(out.remoteTrack) +
        ' conn=' + out.callerConnState);

    // 6. Round-trip a message over the oai-events datachannel (the transport
    //    session.update + server events ride in production).
    const msgArrived = Promise.withResolvers();
    answererDc.onmessage = (e) => msgArrived.resolve(e.data);
    dc.send('oai-events-ping');
    const echoed = await Promise.race([
      msgArrived.promise,
      new Promise((_, rej) => setTimeout(() => rej(new Error('dc roundtrip timeout')), 5000)),
    ]);
    out.datachannelRoundtrip = echoed === 'oai-events-ping';
    log('dc roundtrip=' + out.datachannelRoundtrip + ' echoed=' + echoed);

    // 7. Remote audio element play (autoplay allowed via launch flag).
    //    Verifies the production ontrack path end to end: the remote track is
    //    attached to a real MediaStream, an <audio> element plays it, and the
    //    playback is OBSERVABLY progressing (readyState >= HAVE_CURRENT_DATA,
    //    not paused, currentTime advancing) — never a boolean placeholder.
    const remoteStream = out._remoteStream;
    if (!remoteStream) {
      out.errors.push('audio: no remote MediaStream captured on ontrack');
    } else {
      const audio = document.createElement('audio');
      audio.srcObject = remoteStream;
      document.body.appendChild(audio);
      try {
        await audio.play();
        await new Promise((resolve, reject) => {
          const deadline = setTimeout(
            () => reject(new Error('audio.play() never produced data (readyState/currentTime stalled)')),
            5000
          );
          const poll = setInterval(() => {
            if (audio.readyState >= 2 && !audio.paused && audio.currentTime > 0) {
              clearInterval(poll);
              clearTimeout(deadline);
              resolve();
            }
          }, 50);
        });
        out.audioPlayed = true;
        out.audioReadyState = audio.readyState;
        out.audioCurrentTime = audio.currentTime;
        out.remoteTrackPlayable = out.remoteTrack && out.remoteTrack.kind === 'audio';
      } catch (e) {
        out.errors.push('audio: ' + String(e));
      }
      audio.srcObject = null;
      audio.remove();
    }

    caller.close(); answerer.close();
    stream.getTracks().forEach((t) => t.stop());
    out.ok = true;
  } catch (e) {
    out.ok = false;
    out.errors.push(String(e && e.message ? e.message : e));
  }
  return out;
};

// A second loopback that drives the REAL setupRealtimeCall orchestration
// end-to-end (getUserMedia -> pc -> addTrack -> createDataChannel('oai-events')
// -> createOffer -> setLocalDescription -> waitForIceGathering -> sendCreateCall
// (the GATHERED sdp) -> setRemoteDescription) with a sendCreateCall that
// forwards the offer to a real answerer peer and returns its answer. Proves the
// whole fix path with real WebRTC, not just the helper in isolation.
const SETUP_LOOPBACK = async () => {
  const H = window.__rtHelpers;
  const out = { steps: [], errors: [] };
  const log = (s) => out.steps.push(s);
  try {
    const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    const answerer = new RTCPeerConnection();
    let answererDc = null;
    const sessionUpdate = Promise.withResolvers();
    answerer.ondatachannel = (e) => {
      answererDc = e.channel;
      answererDc.onmessage = (event) => sessionUpdate.resolve(event.data);
    };
    const remoteTrack = Promise.withResolvers();
    answerer.ontrack = (e) => remoteTrack.resolve({ kind: e.track.kind });

    const result = await H.setupRealtimeCall({
      getUserMedia: () => Promise.resolve(stream),
      createPeerConnection: () => {
        const pc = new RTCPeerConnection();
        // Wire the answerer side via the sendCreateCall closure below; the pc
        // is the caller.
        return pc;
      },
      sendCreateCall: async (sdpOffer) => {
        out.postedSdpHasCandidates = /a=candidate:/.test(sdpOffer || '');
        // Informational only: end-of-candidates may be omitted by some
        // browser builds even when gathering completed; the asserted
        // contract is the presence of a=candidate lines (out.postedSdpHasCandidates).
        out.postedSdpHasEndOfCandidates = /a=end-of-candidates/.test(sdpOffer || '');
        log('setupRealtimeCall posted SDP with candidates=' + out.postedSdpHasCandidates);
        await answerer.setRemoteDescription({ type: 'offer', sdp: sdpOffer });
        const ans = await answerer.createAnswer();
        await answerer.setLocalDescription(ans);
        await H.waitForIceGatheringComplete(answerer, H.REALTIME_ICE_GATHER_TIMEOUT_MS);
        return { sdp: answerer.localDescription.sdp, callId: 'rtc_loopback_setup' };
      },
      onPeerConnection: (pc) => {
        pc.onconnectionstatechange = () => {};
      },
      onDataChannel: (_pc, dcArg) => {
        // Mirror the production wiring: send session.update on open.
        dcArg.onopen = () => {
          try { dcArg.send(JSON.stringify({ type: 'session.update', session: H.buildRealtimeSessionConfig('gpt-realtime-1.5', 'sol') })); }
          catch (e) { out.errors.push('session.update send: ' + String(e)); }
        };
      },
    });

    out.callId = result.callId;
    out.dcLabel = result.dc.label;
    log('setupRealtimeCall returned callId=' + out.callId + ' dcLabel=' + out.dcLabel);

    // Wait for the answerer datachannel + remote track + caller connected.
    const adReady = Promise.withResolvers();
    const checkAd = setInterval(() => { if (answererDc) { clearInterval(checkAd); adReady.resolve(true); } }, 30);
    const safety = new Promise((_, rej) => setTimeout(() => rej(new Error('setup loopback timeout')), 10000));
    const [adRes, trackRes] = await Promise.race([
      Promise.all([adReady.promise, remoteTrack.promise]),
      safety,
    ]);
    out.answererDcOpened = adRes === true;
    out.remoteTrack = trackRes;

    // session.update listener is installed synchronously in ondatachannel so
    // the caller's immediate onopen send cannot race past the assertion.
    const frame = await Promise.race([
      sessionUpdate.promise,
      new Promise((_, rej) => setTimeout(() => rej(new Error('session.update timeout')), 5000)),
    ]);
    let parsed = null;
    try { parsed = JSON.parse(frame); } catch (e) { out.errors.push('parse session.update: ' + String(e)); }
    out.sessionUpdateType = parsed && parsed.type;
    out.sessionUpdateModel = parsed && parsed.session && parsed.session.model;
    out.sessionUpdateVoice = parsed && parsed.session && parsed.session.audio && parsed.session.audio.output && parsed.session.audio.output.voice;
    log('session.update arrived type=' + out.sessionUpdateType + ' model=' + out.sessionUpdateModel + ' voice=' + out.sessionUpdateVoice);

    result.pc.close(); answerer.close();
    stream.getTracks().forEach((t) => t.stop());
    out.ok = true;
  } catch (e) {
    out.ok = false;
    out.errors.push(String(e && e.message ? e.message : e));
  }
  return out;
};

async function main() {
  if (!url) fail('RPI_URL is required');
  if (!helpersPath) setupFail('RPI_RT_HELPERS is required (esbuild bundle of src/realtime.ts)');
  let helpersCode;
  try {
    helpersCode = fs.readFileSync(helpersPath, 'utf8');
  } catch (e) {
    setupFail(`could not read rt-helpers bundle at ${helpersPath}: ${e.message}`);
    }
    // esbuild --format=iife emits `var __rtHelpers = (() => {...})()`; Playwright's
    // addInitScript wraps the snippet in a function so the top-level `var` stays
    // function-scoped and never reaches `window`. Re-export it onto globalThis in
    // the same scope so page.evaluate can read window.__rtHelpers.
    helpersCode += '\nglobalThis.__rtHelpers = __rtHelpers;\n';
  // Launch real Chromium with the synthetic mic + auto-approve permission +
  // autoplay-no-gesture flags so getUserMedia/audio.play work headless.
  const launchOptions = {
    args: [
      '--use-fake-device-for-media-stream',
      '--use-fake-ui-for-media-stream',
      '--autoplay-policy=no-user-gesture-required',
    ],
  };
  if (chromePath) launchOptions.executablePath = chromePath;

  let browser;
  try {
    browser = await chromium.launch(launchOptions);
  } catch (e) {
    setupFail(`chromium launch failed: ${e.message}`);
  }

  try {
    const page = await browser.newPage();
    // Inject the REAL src/realtime.ts helpers (esbuild IIFE) before any page
    // script so window.__rtHelpers is available to page.evaluate.
    await page.addInitScript(helpersCode);
    page.on('pageerror', (err) => {
      console.error(`web-realtime-webrtc: page error: ${err.message}`);
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });

    // ---- Loopback 1: real ICE gather + datachannel + audio track ----
    const r1 = await page.evaluate(LOOPBACK);
    fs.writeFileSync(`${evidence}/loopback1.json`, JSON.stringify(r1, null, 2));
    if (!r1 || r1.ok !== true) {
      fail(`loopback1 did not complete: ${JSON.stringify(r1)}`);
    }
    if (r1.tracks !== 1 || r1.trackKind !== 'audio') fail(`loopback1: getUserMedia did not yield one audio track: ${JSON.stringify(r1)}`);
    if (r1.channelLabel !== 'oai-events') fail(`loopback1: datachannel label not oai-events: ${r1.channelLabel}`);
    // The root-cause fix: the offer posted AFTER the ICE-gather wait carries
    // a=candidate lines. Before the wait it does not (on a real stack).
    if (r1.gatheredOfferHasCandidates !== true) fail(`loopback1: gathered offer has no ICE candidates: ${JSON.stringify(r1)}`);
    if (r1.iceGatheringState !== 'complete') fail(`loopback1: iceGatheringState not complete after wait: ${r1.iceGatheringState}`);
    if (r1.datachannelOpen !== true) fail(`loopback1: caller datachannel never opened: ${JSON.stringify(r1)}`);
    if (r1.answererDc !== true) fail(`loopback1: answerer ondatachannel never fired: ${JSON.stringify(r1)}`);
    if (!r1.remoteTrack || r1.remoteTrack.kind !== 'audio') fail(`loopback1: remote audio track never arrived: ${JSON.stringify(r1)}`);
    if (r1.callerConnState !== 'connected') fail(`loopback1: caller never reached connected: ${r1.callerConnState}`);
    if (r1.classifiedConn !== 'connected') fail(`loopback1: classifyRealtimeConnectionState not connected: ${r1.classifiedConn}`);
    if (r1.datachannelRoundtrip !== true) fail(`loopback1: oai-events datachannel round-trip failed: ${JSON.stringify(r1)}`);
    if (r1.remoteTrackPlayable !== true) fail(`loopback1: remote audio track not playable: ${JSON.stringify(r1)}`);
    if (r1.audioPlayed !== true) fail(`loopback1: remote audio did not observably play (no real MediaStream/play evidence): ${JSON.stringify(r1)}`);
    console.log('web-realtime-webrtc: loopback1 (real ICE gather + oai-events datachannel + audio track) PASSED');

    // ---- Loopback 2: real setupRealtimeCall end-to-end ----
    const r2 = await page.evaluate(SETUP_LOOPBACK);
    fs.writeFileSync(`${evidence}/loopback2.json`, JSON.stringify(r2, null, 2));
    if (!r2 || r2.ok !== true) fail(`loopback2 (setupRealtimeCall) did not complete: ${JSON.stringify(r2)}`);
    if (r2.callId !== 'rtc_loopback_setup') fail(`loopback2: callId mismatch: ${JSON.stringify(r2)}`);
    if (r2.dcLabel !== 'oai-events') fail(`loopback2: dc label not oai-events: ${r2.dcLabel}`);
    // The orchestration POSTed the GATHERED SDP (with candidates) — the fix.
    if (r2.postedSdpHasCandidates !== true) fail(`loopback2: setupRealtimeCall posted a candidate-less SDP: ${JSON.stringify(r2)}`);
    if (r2.answererDcOpened !== true) fail(`loopback2: answerer datachannel never opened: ${JSON.stringify(r2)}`);
    if (!r2.remoteTrack || r2.remoteTrack.kind !== 'audio') fail(`loopback2: remote audio track never arrived: ${JSON.stringify(r2)}`);
    // session.update rode the oai-events datachannel with the V1 quicksilver
    // session shape (voice nested under audio.output.voice).
    if (r2.sessionUpdateType !== 'session.update') fail(`loopback2: session.update type wrong: ${r2.sessionUpdateType}`);
    if (r2.sessionUpdateModel !== 'gpt-realtime-1.5') fail(`loopback2: session.update model not propagated: ${r2.sessionUpdateModel}`);
    if (r2.sessionUpdateVoice !== 'sol') fail(`loopback2: session.update voice not nested under audio.output.voice: ${r2.sessionUpdateVoice}`);
    console.log('web-realtime-webrtc: loopback2 (real setupRealtimeCall + session.update over oai-events) PASSED');

    await page.screenshot({ path: `${evidence}/realtime-webrtc.png`, fullPage: true });
    console.log('web-realtime-webrtc: PASSED (REAL RTCPeerConnection loopback, no FakePeerConnection)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-realtime-webrtc: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});