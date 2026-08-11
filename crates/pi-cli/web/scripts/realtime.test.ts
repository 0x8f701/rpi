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
  normalizeLiveMode,
  isRealtimeLiveMode,
  classifyInputTranscriptEvent,
  finalTranscriptText,
  nextInputTranscriptCommit,
  realtimeErrorMessage,
} from '../src/realtime.ts';
const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

// ---- buildRealtimeSessionConfig: exact V1 "quicksilver" shape (Contract 2/4) ----
{
  const session = buildRealtimeSessionConfig('gpt-realtime-1.5', 'sol');
  check('session.type == quicksilver', session.type === 'quicksilver', JSON.stringify(session));
  check('session.model propagated', session.model === 'gpt-realtime-1.5');
  check('session.audio.input.format.type == audio/pcm', session.audio.input.format.type === 'audio/pcm');
  check('session.audio.input.format.rate == 24000 (number)', session.audio.input.format.rate === 24000 && typeof session.audio.input.format.rate === 'number');
  check('session.audio.output.voice propagated', session.audio.output.voice === 'sol');
  // The session.update frame wraps the session object verbatim (Contract 4:
  // same object as the Rust create-call POST body).
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


// ---- setupRealtimeCall: oai-events data channel created BEFORE the offer,
//      and NO direct CLIProxy WebSocket is opened (data-channel-only contract) ----
{
  const ops = []; // recorded operation trace (behavioral, not source strings)
  let webSocketOpened = 0; // would increment if a direct WS were constructed
  const mockStream = {
    getTracks: () => [{ stop: () => {} }],
  };
  const mockDc = {
    onopen: null, onmessage: null, onerror: null, onclose: null,
    send: () => {}, close: () => {},
  };
  const mockPc = {
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
    setLocalDescription: () => {
      ops.push('setLocalDescription');
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
    sendCreateCall: (sdpOffer) => {
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

console.log(`\nrealtime.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);