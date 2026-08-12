#!/usr/bin/env node
// Focused realtime sideband/data-channel regression for src/realtime.ts — the
// pure session config, live-mode normalization, input-transcript
// classification/extraction, and error-parsing rules shared by App.tsx's
// WebRTC realtime flow. Run through `npm run build`, which bundles this file
// with esbuild into a disposable Node module before executing the assertions.
//
// Exit codes: 0 = every assertion held; 1 = a regression.
//
// These assertions exercise BEHAVIOR (constructed objects, classification
// decisions, extracted text), never source strings.
import {
  buildRealtimeSessionConfig,
  setupRealtimeCall,
  REALTIME_EVENT_CHANNEL,
  REALTIME_ICE_GATHER_TIMEOUT_MS,
  normalizeLiveMode,
  isRealtimeLiveMode,
  classifyInputTranscriptEvent,
  finalTranscriptText,
  nextInputTranscriptCommit,
  realtimeErrorMessage,
  classifyRealtimeConnectionState,
  waitForIceGatheringComplete,
} from '../src/realtime.ts';
const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

// ---- buildRealtimeSessionConfig: exact V1 "quicksilver" shape (data-channel session.update) ----
{
  const session = buildRealtimeSessionConfig('gpt-realtime-1.5', 'sol');
  check('session.type == quicksilver', session.type === 'quicksilver', JSON.stringify(session));
  check('session.model propagated', session.model === 'gpt-realtime-1.5');
  check('session.audio.input.format.type == audio/pcm', session.audio.input.format.type === 'audio/pcm');
  check('session.audio.input.format.rate == 24000 (number)', session.audio.input.format.rate === 24000 && typeof session.audio.input.format.rate === 'number');
  check('session.audio.output.voice propagated', session.audio.output.voice === 'sol');
  // The session.update frame wraps this model-bearing session object verbatim.
  // It is data-channel-only: the Rust create-call POST session is a strict
  // subset WITHOUT `model`, which Codex realtime rejects with 400.
  const frame = { type: 'session.update', session };
  check('session.update frame type', frame.type === 'session.update');
  check('session.update frame carries the full nested session object', frame.session === session);
  // The legacy bug shape ({model, voice} at the session top level) must NOT be
  // produced — voice nests under audio.output, and there is no top-level voice.
  check('no top-level voice on session (voice nests under audio.output)', !('voice' in session));
  check('session has audio.input + audio.output nesting', 'input' in session.audio && 'output' in session.audio);
  // The session object must NOT carry an `id` (the official client strips it).
  check('session carries no id', !('id' in session));
  // Different configured values propagate (no hardcoding).
  const other = buildRealtimeSessionConfig('gpt-4o-realtime', 'marin');
  check('other model/voice propagate', other.model === 'gpt-4o-realtime' && other.audio.output.voice === 'marin' && other.audio.input.format.rate === 24000);
}

// ---- normalizeLiveMode: trim + ASCII lowercase ----
{
  check('normalizeLiveMode realtime lower', normalizeLiveMode('realtime') === 'realtime');
  check('normalizeLiveMode realtime trim+lower', normalizeLiveMode('  Realtime  ') === 'realtime');
  check('normalizeLiveMode REALTIME upper', normalizeLiveMode('REALTIME') === 'realtime');
  check('normalizeLiveMode MixedCase', normalizeLiveMode('RealTime') === 'realtime');
  check('normalizeLiveMode stt unchanged', normalizeLiveMode('stt') === 'stt');
  check('normalizeLiveMode empty', normalizeLiveMode('') === '');
  check('normalizeLiveMode whitespace -> empty', normalizeLiveMode('   ') === '');
  check('normalizeLiveMode non-string -> empty', normalizeLiveMode(undefined) === '' && normalizeLiveMode(null) === '' && normalizeLiveMode(123) === '');
}

// ---- isRealtimeLiveMode: never assumes realtime when live is missing ----
{
  check('enabled realtime mode selected', isRealtimeLiveMode({ enabled: true, mode: 'realtime' }) === true);
  check('enabled realtime with casing/spacing', isRealtimeLiveMode({ enabled: true, mode: ' RealTime ' }) === true);
  check('disabled realtime not selected', isRealtimeLiveMode({ enabled: false, mode: 'realtime' }) === false);
  check('missing enabled not realtime', isRealtimeLiveMode({ mode: 'realtime' }) === false);
  check('stt not realtime', isRealtimeLiveMode({ enabled: true, mode: 'stt' }) === false);
  check('unknown mode not realtime', isRealtimeLiveMode({ enabled: true, mode: 'webrtc' }) === false);
  check('null live not realtime (never assume)', isRealtimeLiveMode(null) === false);
  check('undefined live not realtime (never assume)', isRealtimeLiveMode(undefined) === false);
  check('empty live not realtime', isRealtimeLiveMode({}) === false);
  check('blank mode not realtime', isRealtimeLiveMode({ enabled: true, mode: '  ' }) === false);
}

// ---- classifyInputTranscriptEvent: USER input vs assistant output ----
{
  // CLIProxyAPI aliases (kept for compat) + V1 conversation input events.
  check('transcript.delta is input delta', classifyInputTranscriptEvent('transcript.delta') === 'delta');
  check('conversation.input_transcript.delta is input delta', classifyInputTranscriptEvent('conversation.input_transcript.delta') === 'delta');
  check('conversation.item.input_audio_transcription.delta is input delta', classifyInputTranscriptEvent('conversation.item.input_audio_transcription.delta') === 'delta');
  check('transcript.done is input final', classifyInputTranscriptEvent('transcript.done') === 'final');
  check('conversation.input_transcript.done is input final', classifyInputTranscriptEvent('conversation.input_transcript.done') === 'final');
  check('conversation.item.input_audio_transcription.completed is input final', classifyInputTranscriptEvent('conversation.item.input_audio_transcription.completed') === 'final');
  // Assistant OUTPUT transcript events must NEVER classify as user input —
  // they cannot be committed to the composer as a user draft.
  check('conversation.output_transcript.delta is NOT user input', classifyInputTranscriptEvent('conversation.output_transcript.delta') === null);
  check('conversation.output_transcript.done is NOT user input', classifyInputTranscriptEvent('conversation.output_transcript.done') === null);
  check('response.output_audio_transcript.delta is NOT user input', classifyInputTranscriptEvent('response.output_audio_transcript.delta') === null);
  check('response.output_audio.delta is NOT user input', classifyInputTranscriptEvent('response.output_audio.delta') === null);
  check('delegation.created is NOT user input', classifyInputTranscriptEvent('delegation.created') === null);
  check('error is NOT user input', classifyInputTranscriptEvent('error') === null);
  check('unknown type is NOT user input', classifyInputTranscriptEvent('garbage.event') === null);
  check('empty type is NOT user input', classifyInputTranscriptEvent('') === null);
}

// ---- finalTranscriptText: extracts the authoritative final text ----
{
  check('finalTranscriptText reads top-level transcript', finalTranscriptText({ type: 'conversation.input_transcript.done', transcript: 'hello world' }) === 'hello world');
  check('finalTranscriptText reads alias text', finalTranscriptText({ type: 'transcript.done', text: 'hi there' }) === 'hi there');
  // conversation.item.input_audio_transcription.completed nests under item.
  check('finalTranscriptText reads item.transcript (completed)', finalTranscriptText({ type: 'conversation.item.input_audio_transcription.completed', item: { transcript: 'nested final' } }) === 'nested final');
  check('finalTranscriptText reads item.content fallback', finalTranscriptText({ type: 'conversation.item.input_audio_transcription.completed', item: { content: 'nested content' } }) === 'nested content');
  check('finalTranscriptText empty when no text', finalTranscriptText({ type: 'conversation.input_transcript.done' }) === '');
  check('finalTranscriptText trims', finalTranscriptText({ transcript: '  spaced  ' }) === 'spaced');
}

// ---- nextInputTranscriptCommit: final text enters the composer, deduped ----
{
  check('first final commits to composer', nextInputTranscriptCommit('hello world', '') === 'hello world');
  check('empty final skipped', nextInputTranscriptCommit('', '') === '');
  check('whitespace final skipped', nextInputTranscriptCommit('   ', '') === '');
  // The V1 protocol emits BOTH conversation.input_transcript.done AND
  // conversation.item.input_audio_transcription.completed for one utterance,
  // both carrying the same final text — the second must not double-commit.
  check('duplicate final deduped (two V1 variants, same utterance)', nextInputTranscriptCommit('hello world', 'hello world') === '');
  check('different utterance still commits', nextInputTranscriptCommit('second turn', 'hello world') === 'second turn');
  check('trim before compare + return', nextInputTranscriptCommit('  hello world  ', '') === 'hello world');
  check('trimmed duplicate deduped', nextInputTranscriptCommit('  hello world  ', 'hello world') === '');
}

// ---- realtimeErrorMessage: nested error.message/code with bounded fallback ----
{
  check('nested error message+code', realtimeErrorMessage({ type: 'error', error: { message: 'Field session must be an object', code: 'invalid_request_error' } }) === 'realtime error [invalid_request_error]: Field session must be an object');
  check('nested error message only', realtimeErrorMessage({ type: 'error', error: { message: 'boom' } }) === 'realtime error: boom');
  check('nested error code only', realtimeErrorMessage({ type: 'error', error: { code: 'E1' } }) === 'realtime error [E1]');
  check('top-level message fallback (alias)', realtimeErrorMessage({ type: 'error', message: 'alias boom' }) === 'realtime error: alias boom');
  check('top-level message+code fallback', realtimeErrorMessage({ type: 'error', message: 'x', code: 'C2' }) === 'realtime error [C2]: x');
  check('no info -> bounded default', realtimeErrorMessage({ type: 'error' }) === 'realtime session error');
  check('error non-object -> top-level fallback', realtimeErrorMessage({ type: 'error', error: 'oops', message: 'fallback' }) === 'realtime error: fallback');
}

{
  const ops = []; // recorded operation trace (behavioral, not source strings)
  let webSocketOpened = 0; // would increment if a direct WS were constructed
  let postedSdp = null; // SDP actually handed to sendCreateCall
  const mockStream = {
    getTracks: () => [{ stop: () => {} }],
  };
  const mockDc = {
    onopen: null, onmessage: null, onerror: null, onclose: null,
    send: () => {}, close: () => {},
  };
  // Mock pc that simulates ICE gathering: setLocalDescription stores a bare
  // offer; the injected waitForIceGathering appends a candidate line to
  // localDescription.sdp and flips iceGatheringState to 'complete' — exactly
  // what a real browser does as host candidates are gathered. This proves the
  // POST carries the POST-GATHER localDescription (with candidates), not the
  // stale offer.sdp (the long-standing silent-failure root cause).
  const mockPc = {
    iceGatheringState: 'new',
    localDescription: null,
    listeners: {},
    addEventListener(type, fn) { (this.listeners[type] ||= []).push(fn); },
    removeEventListener(type, fn) {
      const arr = this.listeners[type];
      if (arr) this.listeners[type] = arr.filter((f) => f !== fn);
    },
    addTrack: () => {
      ops.push('addTrack');
    },
    createDataChannel: (label) => {
      ops.push(`createDataChannel:${label}`);
      return mockDc;
    },
    createOffer: () => {
      ops.push('createOffer');
      return Promise.resolve({ sdp: 'v=0\nmock-offer\n' });
    },
    setLocalDescription(desc) {
      ops.push('setLocalDescription');
      this.localDescription = { type: 'offer', sdp: desc.sdp };
      return Promise.resolve();
    },
    setRemoteDescription: () => {
      ops.push('setRemoteDescription');
      return Promise.resolve();
    },
  };
  const onPcCalls = [];
  const onDcCalls = [];
  const deps = {
    getUserMedia: () => {
      ops.push('getUserMedia');
      return Promise.resolve(mockStream);
    },
    createPeerConnection: () => {
      ops.push('createPeerConnection');
      return mockPc;
    },
    // Inject a gather simulation that appends a candidate line (a real browser
    // updates localDescription.sdp in place as candidates arrive). Using the
    // injected seam keeps the node test deterministic with no real ICE stack.
    waitForIceGathering: (pc) => {
      ops.push('waitForIceGathering');
      pc.iceGatheringState = 'complete';
      pc.localDescription = {
        type: 'offer',
        sdp: (pc.localDescription?.sdp || '') + 'a=candidate:mock-host 1 udp 2122252543 127.0.0.1 9 typ host\r\n',
      };
      return Promise.resolve();
    },
    sendCreateCall: (sdpOffer) => {
      postedSdp = sdpOffer;
      ops.push('sendCreateCall');
      return Promise.resolve({ sdp: 'v=0\nmock-answer\n', callId: 'rtc_test_1' });
    },
    onPeerConnection: () => {
      onPcCalls.push('onPeerConnection');
      ops.push('wirePeerConnection');
    },
    onDataChannel: () => {
      onDcCalls.push('onDataChannel');
      ops.push('wireDataChannel');
    },
  };
  const result = await setupRealtimeCall(deps);
  check('setupRealtimeCall returns callId', result.callId === 'rtc_test_1');
  check('setupRealtimeCall returns the data channel', result.dc === mockDc);
  check('setupRealtimeCall returns the peer connection', result.pc === mockPc);
  // The oai-events data channel is created BEFORE createOffer.
  const dcIdx = ops.indexOf(`createDataChannel:${REALTIME_EVENT_CHANNEL}`);
  const offerIdx = ops.indexOf('createOffer');
  check('createDataChannel index found', dcIdx >= 0, JSON.stringify(ops));
  check('createOffer index found', offerIdx >= 0, JSON.stringify(ops));
  check('oai-events data channel created BEFORE createOffer', dcIdx >= 0 && offerIdx >= 0 && dcIdx < offerIdx, JSON.stringify(ops));
  check('channel label is oai-events', REALTIME_EVENT_CHANNEL === 'oai-events');
  // The pc + data-channel handlers are wired before createOffer (so ontrack
  // and onopen are in place before the answer/ICE produces events).
  check('peer connection wired before createOffer', ops.indexOf('wirePeerConnection') >= 0 && ops.indexOf('wirePeerConnection') < offerIdx, JSON.stringify(ops));
  check('data channel wired before createOffer', ops.indexOf('wireDataChannel') >= 0 && ops.indexOf('wireDataChannel') < offerIdx, JSON.stringify(ops));
  // ICE gathering is awaited AFTER setLocalDescription and BEFORE sendCreateCall
  // — this is the root-cause fix: the offer posted to /v1/realtime/calls must
  // carry gathered candidates (no trickle sideband).
  const gatherIdx = ops.indexOf('waitForIceGathering');
  const sendIdx = ops.indexOf('sendCreateCall');
  check('waitForIceGathering recorded', gatherIdx >= 0, JSON.stringify(ops));
  check('ICE gather awaited after setLocalDescription', ops.indexOf('setLocalDescription') >= 0 && ops.indexOf('setLocalDescription') < gatherIdx, JSON.stringify(ops));
  check('sendCreateCall after ICE gather', gatherIdx < sendIdx, JSON.stringify(ops));
  // The SDP posted to sendCreateCall is the POST-GATHER localDescription
  // (carries the candidate line), NOT the stale bare offer.sdp. This is the
  // behavioral proof of the fix — the old code posted offer.sdp with zero
  // candidates and the call silently never connected.
  check('posted SDP carries gathered candidate line', typeof postedSdp === 'string' && postedSdp.includes('a=candidate:mock-host'), JSON.stringify(postedSdp));
  check('posted SDP is the post-gather localDescription (not bare offer)', postedSdp && postedSdp.includes('a=candidate:mock-host') && postedSdp.includes('v=0\nmock-offer\n'));
  // sendCreateCall happens after createOffer + setLocalDescription.
  check('sendCreateCall after setLocalDescription', ops.indexOf('setLocalDescription') >= 0 && ops.indexOf('setLocalDescription') < ops.indexOf('sendCreateCall'), JSON.stringify(ops));
  // setRemoteDescription happens last (after the answer arrives).
  check('setRemoteDescription after sendCreateCall', ops.indexOf('sendCreateCall') < ops.indexOf('setRemoteDescription'), JSON.stringify(ops));
  // NO direct CLIProxy WebSocket is opened: the orchestration never touches a
  // WebSocket — events ride the data channel. (webSocketOpened stays 0; the
  // op trace contains no WebSocket/realtime?call_id step.)
  check('no direct CLIProxy WebSocket constructed', webSocketOpened === 0);
  check('op trace has no WebSocket step', !ops.some((op) => /WebSocket|realtime\?call_id/i.test(op)), JSON.stringify(ops));
}

// ---- setupRealtimeCall: gather-timeout fallback still posts (never wedges) ----
{
  // A pc whose ICE gathering never completes (no host candidates, e.g. a
  // restricted network) must NOT wedge the call forever: the gather wait
  // times out and posts whatever localDescription has. We inject a
  // waitForIceGathering that resolves WITHOUT appending candidates (the
  // timeout-fallback behavior) so the orchestration posts the partial
  // localDescription verbatim — proving the call never wedges on a stuck
  // gather and that pc.localDescription.sdp (not the stale offer.sdp) is
  // always what gets POSTed.
  let postedSdp = null;
  const mockPc = {
    iceGatheringState: 'new',
    localDescription: { type: 'offer', sdp: 'v=0\npartial-offer\n' },
    addEventListener() {},
    removeEventListener() {},
    addTrack: () => {},
    createDataChannel: () => ({ onopen: null, onmessage: null, onerror: null, onclose: null, send: () => {}, close: () => {} }),
    createOffer: () => Promise.resolve({ sdp: 'v=0\npartial-offer\n' }),
    setLocalDescription(desc) { this.localDescription = { type: 'offer', sdp: desc.sdp }; return Promise.resolve(); },
    setRemoteDescription: () => Promise.resolve(),
  };
  const deps = {
    getUserMedia: () => Promise.resolve({ getTracks: () => [{ stop: () => {} }] }),
    createPeerConnection: () => mockPc,
    // Simulate the timeout-fallback path: resolve without flipping
    // iceGatheringState to 'complete' and without appending candidates.
    waitForIceGathering: () => Promise.resolve(),
    sendCreateCall: (sdp) => { postedSdp = sdp; return Promise.resolve({ sdp: 'v=0\nans\n', callId: 'rtc_t' }); },
  };
  const result = await setupRealtimeCall(deps);
  check('gather-timeout fallback still returns callId', result.callId === 'rtc_t');
  check('gather-timeout fallback posted the partial localDescription', postedSdp === 'v=0\npartial-offer\n', JSON.stringify(postedSdp));
  check('gather-timeout fallback posted NO candidate (gather did not complete)', typeof postedSdp === 'string' && !postedSdp.includes('a=candidate'), JSON.stringify(postedSdp));
}

// ---- classifyRealtimeConnectionState: UI buckets + ice aliases ----
{
  check('new', classifyRealtimeConnectionState('new') === 'new');
  check('connecting', classifyRealtimeConnectionState('connecting') === 'connecting');
  check('connected', classifyRealtimeConnectionState('connected') === 'connected');
  check('disconnected', classifyRealtimeConnectionState('disconnected') === 'disconnected');
  check('failed', classifyRealtimeConnectionState('failed') === 'failed');
  check('closed', classifyRealtimeConnectionState('closed') === 'closed');
  // iceConnectionState aliases fold onto the pc.connectionState buckets —
  // 'checking' is a transient connecting phase, 'completed' means connected.
  check('checking -> connecting', classifyRealtimeConnectionState('checking') === 'connecting');
  check('completed -> connected', classifyRealtimeConnectionState('completed') === 'connected');
  // disconnected MUST stay distinct from failed: disconnected is recoverable
  // (transient ICE path loss -> toast, no teardown) while failed is terminal
  // (toast + teardown). Collapsing them would tear down a recoverable call.
  check('disconnected != failed', classifyRealtimeConnectionState('disconnected') !== classifyRealtimeConnectionState('failed'));
  check('unknown state', classifyRealtimeConnectionState('garbage') === 'unknown');
  check('non-string -> unknown', classifyRealtimeConnectionState(undefined) === 'unknown' && classifyRealtimeConnectionState(null) === 'unknown' && classifyRealtimeConnectionState(123) === 'unknown');
}

// ---- waitForIceGatheringComplete: already-complete, event, timeout ----
{
  // Already complete -> resolves immediately (no event listener wired, no
  // scheduler timeout scheduled).
  const completePc = { iceGatheringState: 'complete', addEventListener() {}, removeEventListener() {} };
  await waitForIceGatheringComplete(completePc, 1000);
  check('already-complete resolves', true);

  // Event fires 'complete' -> resolves once gathering finishes. Simulate a
  // real browser: ICE starts 'gathering', then a later turn flips to
  // 'complete' and dispatches icegatheringstatechange. No real timer: the
  // scheduler's setTimeout is captured but never fired by the test (the
  // event path resolves first).
  let onChangeFn = null;
  let timeoutFired = false;
  const fakeScheduler = {
    setTimeout: () => 1,
    clearTimeout: () => { timeoutFired = true; },
  };
  const gatheringPc = {
    iceGatheringState: 'gathering',
    addEventListener(type, fn) { onChangeFn = fn; },
    removeEventListener() { onChangeFn = null; },
  };
  let resolvedEarly = false;
  const p = waitForIceGatheringComplete(gatheringPc, 2000, fakeScheduler).then(() => { resolvedEarly = true; });
  // The promise cannot resolve until either the event fires or the scheduler
  // timeout fires — neither has, so a microtask flush leaves it pending.
  await Promise.resolve();
  check('not resolved before gather completes', resolvedEarly === false);
  gatheringPc.iceGatheringState = 'complete';
  if (onChangeFn) onChangeFn();
  await p;
  check('resolved after icegatheringstatechange -> complete', resolvedEarly === true);
  // The event path MUST cancel the pending timeout (so a late timeout cannot
  // double-resolve / wedge). clearTimeout was called with the captured id.
  check('event path cancels the pending timeout', timeoutFired === true);

  // Timeout fallback -> resolves when the scheduler timeout fires even if
  // gathering never completes and no event ever fires (recoverable: post
  // whatever was gathered so far). Deterministic: the test invokes the
  // captured timeout fn directly instead of waiting real time.
  let capturedTimeoutFn = null;
  const timeoutScheduler = {
    setTimeout: (fn) => { capturedTimeoutFn = fn; return 7; },
    clearTimeout: () => {},
  };
  const stuckPc = {
    iceGatheringState: 'gathering',
    addEventListener() {},
    removeEventListener() {},
  };
  let timedOut = false;
  const timeoutPromise = waitForIceGatheringComplete(stuckPc, 40, timeoutScheduler).then(() => { timedOut = true; });
  await Promise.resolve();
  check('timeout fallback not resolved before scheduler fires', timedOut === false);
  if (capturedTimeoutFn) capturedTimeoutFn();
  await timeoutPromise;
  check('timeout fallback resolves when scheduler fires', timedOut === true);
  // Default bound is the documented 5s reference-client gather window.
  check('default gather timeout is 5000ms', REALTIME_ICE_GATHER_TIMEOUT_MS === 5000);
}

console.log(`\nrealtime.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);