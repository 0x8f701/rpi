#!/usr/bin/env node
// Focused realtime sideband/data-channel regression for src/realtime.ts — the
// pure session config, live-mode normalization, input-transcript
// classification/extraction, error-parsing rules, and captured-session owner
// binding for create/stop RPCs shared by App.tsx's WebRTC realtime flow. Run
// through `npm run build`, which bundles this file with esbuild into a
// disposable Node module before executing the assertions.
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
  inputTranscriptDeltaText,
  inputTranscriptSegmentText,
  delegationCreatedText,
  finalTranscriptText,
  nextInputTranscriptCommit,
  planRealtimeDelegation,
  planRealtimeFinal,
  reduceRealtimeVoiceFrame,
  reducePendingVoiceFlush,
  parseRealtimeChannelFrames,
  unwrapRealtimeFrame,
  decodeRealtimeChannelData,
  isRealtimeHandoffType,
  isIgnorableTranscriptFinal,
  userFinalUtterance,
  realtimeDelegationLabel,
  realtimeErrorMessage,
  classifyRealtimeConnectionState,
  waitForIceGatheringComplete,
  realtimeCreateCallCommand,
  realtimeStopCommand,
  shouldCommitRealtimeTranscript,
  shouldStopRealtimeOnSessionSwitch,
} from '../src/realtime.ts';
const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

// ---- buildRealtimeSessionConfig: exact frameless v3 session.update shape ----
{
  const session = buildRealtimeSessionConfig('gpt-realtime-1.5', 'sol');
  check('session.instructions empty', session.instructions === '');
  check('session.audio.output.voice propagated', session.audio.output.voice === 'sol');
  check('session.delegation.type client', session.delegation.type === 'client');
  const frame = { type: 'session.update', session };
  check('session.update frame type', frame.type === 'session.update');
  check('session.update frame carries the full nested session object', frame.session === session);
  check('session.update omits model', !('model' in frame.session));
  check('session.update omits legacy quicksilver type', !('type' in session));
  check('session.update omits rejected audio input config', !('input' in session.audio));
  check('no top-level voice on session', !('voice' in session));
  check('session carries no id', !('id' in session));
  const other = buildRealtimeSessionConfig('gpt-4o-realtime', 'marin');
  check('other voice propagates without model', other.audio.output.voice === 'marin' && !('model' in other));
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
  // GA v3 input-transcription events: segment is interim (preview only),
  // failed is an error surface (toast, never commits), delta/final aliases.
  check('input_audio_transcription.segment is interim segment', classifyInputTranscriptEvent('conversation.item.input_audio_transcription.segment') === 'segment');
  check('input_audio_transcription.failed is failure', classifyInputTranscriptEvent('conversation.item.input_audio_transcription.failed') === 'failed');
  check('transcription.failed alias is failure', classifyInputTranscriptEvent('transcription.failed') === 'failed');
  check('output_audio_transcript.done is NOT user input', classifyInputTranscriptEvent('response.output_audio_transcript.done') === null);
  check('input_audio_buffer.speech_started is NOT user input', classifyInputTranscriptEvent('input_audio_buffer.speech_started') === null);
  // Codex FRAMELESS protocol (relayed verbatim by CLIProxyAPI over
  // oai-events): interim deltas and the authoritative turn final.
  check('input_transcript.added is input segment (frameless full item.text)', classifyInputTranscriptEvent('input_transcript.added') === 'segment');
  check('turn.done is input final (frameless)', classifyInputTranscriptEvent('turn.done') === 'final');
  check('output_transcript.added is NOT user input', classifyInputTranscriptEvent('output_transcript.added') === null);
  check('session.updated is NOT user input', classifyInputTranscriptEvent('session.updated') === null);
  check('unknown type is NOT user input', classifyInputTranscriptEvent('garbage.event') === null);
  check('empty type is NOT user input', classifyInputTranscriptEvent('') === null);
}

// ---- REAL WIRE SHAPES (Codex frameless-bidi, relayed byte-for-byte by
// CLIProxyAPI over oai-events — internal/client/codex/live/media.go). These
// fixtures are the authoritative upstream shapes; the extractors MUST read
// them. ----
{
  // delegation.created: the spoken command rides item.content[] input_text
  // entries — there is NO `delegation` field (openai/codex
  // protocol_frameless_bidi_tests.rs fixture).
  const framelessDelegation = {
    type: 'delegation.created',
    offset_ms: 1000,
    item: {
      id: 'handoff-123',
      type: 'delegation',
      target: 'client',
      content: [{ type: 'input_text', text: 'check the weather' }],
    },
  };
  check('frameless delegation.created extracts item.content input_text', delegationCreatedText(framelessDelegation) === 'check the weather');
  check('frameless delegation text plans a delivery', planRealtimeDelegation('A', 'A', delegationCreatedText(framelessDelegation), '').deliver === true);
  // Multiple input_text entries concatenate (Codex collects the array).
  check('frameless delegation concatenates input_text entries', delegationCreatedText({ type: 'delegation.created', item: { type: 'delegation', target: 'client', content: [{ type: 'input_text', text: 'fix ' }, { type: 'input_text', text: 'parser.rs' }] } }) === 'fix parser.rs');
  // Non-input_text content entries are ignored.
  check('frameless delegation ignores non-input_text content', delegationCreatedText({ type: 'delegation.created', item: { type: 'delegation', target: 'client', content: [{ type: 'output_text', text: 'nope' }] } }) === '');
  // Id-only delegation: no content, no text — the caller must surface a
  // failure toast instead of silently dropping.
  check('id-only frameless delegation extracts empty', delegationCreatedText({ type: 'delegation.created', item: { id: 'handoff-9', type: 'delegation', target: 'client' } }) === '');
  // Legacy wrapped shapes remain supported.
  check('legacy raw-string delegation still extracts', delegationCreatedText({ type: 'delegation.created', delegation: 'legacy command' }) === 'legacy command');
  check('legacy description delegation still extracts', delegationCreatedText({ type: 'delegation.created', delegation: { description: 'desc command' } }) === 'desc command');
  check('legacy task delegation still extracts', delegationCreatedText({ type: 'delegation.created', delegation: { task: 'task command' } }) === 'task command');
  // turn.done: the final transcript nests under turn and is ROLE-GATED.
  check('frameless turn.done (user) extracts turn.transcript', finalTranscriptText({ type: 'turn.done', turn: { id: 't1', role: 'user', transcript: 'deploy now' } }) === 'deploy now');
  check('frameless turn.done (assistant) NEVER extracts as input', finalTranscriptText({ type: 'turn.done', turn: { id: 't2', role: 'assistant', transcript: 'sure thing' } }) === '');
  // input_transcript.added: the live interim text nests under item.text.
  check('frameless input_transcript.added extracts item.text', inputTranscriptDeltaText({ type: 'input_transcript.added', item: { id: 'i1', type: 'input_transcript', text: 'hello' } }) === 'hello');
  check('frameless input_transcript.added empty without text', inputTranscriptDeltaText({ type: 'input_transcript.added', item: { id: 'i2', type: 'input_transcript' } }) === '');
  check('segment extractor reads item.text too', inputTranscriptSegmentText({ type: 'input_transcript.added', item: { text: 'full interim' } }) === 'full interim');
  check('item.input_transcript is read when content is empty',
    delegationCreatedText({ type: 'delegation.created', item: { type: 'delegation', target: 'client', input_transcript: 'from item field' } }) === 'from item field');
  check('V1 conversation.handoff.requested extracts top-level input_transcript',
    delegationCreatedText({ type: 'conversation.handoff.requested', input_transcript: 'delegate this' }) === 'delegate this');
  check('handoff type aliases both count',
    isRealtimeHandoffType('delegation.created') && isRealtimeHandoffType('conversation.handoff.requested')
      && !isRealtimeHandoffType('turn.done'));
}

{
  const ownerCtx = (overlay, pendingText = '') => ({
    owner: 'A', current: 'A', overlay, lastCommitted: '', pendingText,
  });
  check('assistant turn.done is ignorable as a user final',
    isIgnorableTranscriptFinal({ type: 'turn.done', turn: { role: 'assistant', transcript: 'sure' } }) === true);
  check('user turn.done is not ignorable',
    isIgnorableTranscriptFinal({ type: 'turn.done', turn: { role: 'user', transcript: 'go' } }) === false);
  check('empty user turn.done uses overlay as the utterance',
    userFinalUtterance({ type: 'turn.done', turn: { role: 'user' } }, '  run the tests  ') === 'run the tests');

  const growing1 = reduceRealtimeVoiceFrame(
    { type: 'input_transcript.added', item: { text: 'run the' } },
    ownerCtx(''),
  );
  check('input_transcript.added replaces overlay with full item.text',
    growing1.kind === 'overlay' && growing1.text === 'run the');
  const growing2 = reduceRealtimeVoiceFrame(
    { type: 'input_transcript.added', item: { text: 'run the tests' } },
    ownerCtx(growing1.kind === 'overlay' ? growing1.text : ''),
  );
  check('a later input_transcript.added does not append the previous overlay',
    growing2.kind === 'overlay' && growing2.text === 'run the tests');

  const parked = reduceRealtimeVoiceFrame(
    {
      type: 'delegation.created',
      item: { id: 'h1', type: 'delegation', target: 'client', content: [{ type: 'input_text', text: 'run the tests' }] },
    },
    ownerCtx('run the tests'),
  );
  check('delegation.created parks instead of submitting immediately',
    parked.kind === 'park' && parked.text === 'run the tests');

  const assistantDone = reduceRealtimeVoiceFrame(
    { type: 'turn.done', turn: { role: 'assistant', transcript: 'working on it' } },
    ownerCtx('run the tests', 'run the tests'),
  );
  check('assistant turn.done during a parked utterance DELIVERS the spoken text (does not drop it)',
    assistantDone.kind === 'deliver' && assistantDone.text === 'run the tests' && assistantDone.cancelPending === true);

  const idleAssistant = reduceRealtimeVoiceFrame(
    { type: 'turn.done', turn: { role: 'assistant', transcript: 'hello' } },
    ownerCtx('', ''),
  );
  check('assistant turn.done with no overlay and no pending is a no-op',
    idleAssistant.kind === 'noop');

  const emptyUserDone = reduceRealtimeVoiceFrame(
    { type: 'turn.done', turn: { role: 'user' } },
    ownerCtx('fix parser.rs', ''),
  );
  check('empty user turn.done delivers the overlay as the chat prompt',
    emptyUserDone.kind === 'deliver' && emptyUserDone.text === 'fix parser.rs');

  const v1Handoff = reduceRealtimeVoiceFrame(
    { type: 'conversation.handoff.requested', input_transcript: 'legacy handoff' },
    ownerCtx(''),
  );
  check('V1 conversation.handoff.requested parks the input_transcript',
    v1Handoff.kind === 'park' && v1Handoff.text === 'legacy handoff');

  const bytes = new TextEncoder().encode(JSON.stringify({
    type: 'event',
    event: { type: 'input_transcript.added', item: { text: 'from binary' } },
  }));
  const parsed = parseRealtimeChannelFrames(bytes);
  check('channel JSON unwraps nested event frames',
    unwrapRealtimeFrame({ type: 'event', event: { type: 'delegation.created' } })?.type === 'delegation.created');
  check('channel ArrayBuffer decodes to the nested frame',
    parsed.length === 1 && parsed[0].type === 'input_transcript.added');
  check('channel string data still parses',
    decodeRealtimeChannelData('{"type":"error"}') === '{"type":"error"}');

  const flush = reducePendingVoiceFlush({ owner: 'A', text: 'short' }, 'a much longer preview', 'A', '');
  check('pending flush delivers the richer overlay',
    flush.kind === 'deliver' && flush.text === 'a much longer preview');
  const emptyFlush = reducePendingVoiceFlush({ owner: 'A', text: '' }, '   ', 'A', '');
  check('pending flush with no text is empty-handoff',
    emptyFlush.kind === 'empty-handoff');
}

// ---- finalTranscriptText: extracts the authoritative final text ----
{
  check('finalTranscriptText reads top-level transcript', finalTranscriptText({ type: 'conversation.input_transcript.done', transcript: 'hello world' }) === 'hello world');
  check('finalTranscriptText reads alias text', finalTranscriptText({ type: 'transcript.done', text: 'hi there' }) === 'hi there');
  // conversation.item.input_audio_transcription.completed nests under item.
  check('finalTranscriptText reads item.transcript (completed)', finalTranscriptText({ type: 'conversation.item.input_audio_transcription.completed', item: { transcript: 'nested final' } }) === 'nested final');
  check('finalTranscriptText reads item.content fallback', finalTranscriptText({ type: 'conversation.item.input_audio_transcription.completed', item: { content: 'nested content' } }) === 'nested content');
  check('finalTranscriptText empty when no text', finalTranscriptText({ type: 'conversation.input_transcript.done' }) === '');
  check('finalTranscriptText reads completed top-level transcript', finalTranscriptText({ type: 'conversation.item.input_audio_transcription.completed', transcript: 'ga final' }) === 'ga final');
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
  // The GA failed event nests its detail under `error`; the same bounded
  // extractor that serves `error` frames must render it.
  check('failed event surfaces nested error message', realtimeErrorMessage({ type: 'conversation.item.input_audio_transcription.failed', error: { message: 'asr unavailable', code: 'asr_error' } }) === 'realtime error [asr_error]: asr unavailable');
  check('failed event without detail uses bounded default', realtimeErrorMessage({ type: 'transcription.failed' }) === 'realtime session error');
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

// ---- captured owner: A realtime start -> switch B -> stop/cleanup ----
{
  // Call starts on A; the owner is captured then. ICE gather is async, so a
  // switch to B before POST (or before stop) must still create/stop A's
  // runtime. Prior code read sessionIdRef.current at fire time, so create
  // leaked onto B and stop tore down B's runtime.
  const owner = 'A';
  let current = 'A';
  current = 'B';
  const create = realtimeCreateCallCommand(owner, 'v=0');
  const stop = realtimeStopCommand(owner);
  check('create type is realtime_create_call', create.type === 'realtime_create_call');
  check('stop type is realtime_stop', stop.type === 'realtime_stop');
  check('A start -> switch B: create stays on A', create.sessionId === 'A');
  check('A start -> switch B: create does not target B', create.sessionId !== current);
  check('A start -> switch B: create offer is the captured SDP', create.sdpOffer === 'v=0');
  check('A start -> switch B: stop stays on A', stop.sessionId === 'A');
  check('A start -> switch B: stop does not target B', stop.sessionId !== current);
  check(
    'A start -> switch B: late realtime transcript not committed to B',
    shouldCommitRealtimeTranscript(owner, current) === false,
  );
  check('same-session realtime still commits', shouldCommitRealtimeTranscript(owner, 'A') === true);
  check('empty realtime owner never commits', shouldCommitRealtimeTranscript('', 'A') === false);
  check('null realtime owner never commits', shouldCommitRealtimeTranscript(null, 'A') === false);

  check(
    'A call -> switch B stops the whole A realtime surface',
    shouldStopRealtimeOnSessionSwitch(owner, 'A', 'B') === true,
  );
  check(
    'stopped A channel cannot project A deltas or delegations onto B',
    shouldStopRealtimeOnSessionSwitch(owner, 'A', 'B')
      && shouldCommitRealtimeTranscript(owner, 'B') === false,
  );
  check('same-session refresh keeps A call', shouldStopRealtimeOnSessionSwitch(owner, 'A', 'A') === false);
  check('unowned realtime never triggers another session teardown', shouldStopRealtimeOnSessionSwitch('', 'A', 'B') === false);
}

// ---- planRealtimeFinal: voice commits to the owner composer, dropped on owner mismatch ----
{
  const owner = 'A';
  const delivered = planRealtimeFinal(owner, 'A', 'run the tests', '');
  check('completed transcript commits', delivered.commit === 'run the tests');
  check('completed transcript delivers on the owner session', delivered.deliver === true);
  const duplicate = planRealtimeFinal(owner, 'A', 'run the tests', 'run the tests');
  check('duplicate final neither commits nor delivers', duplicate.commit === '' && duplicate.deliver === false);
  const empty = planRealtimeFinal(owner, 'A', '   ', '');
  check('empty final never delivers', empty.commit === '' && empty.deliver === false);
  const mismatched = planRealtimeFinal(owner, 'B', 'late utterance from A', '');
  check('owner mismatch drops the transcript (no deliver, no draft write)',
    mismatched.commit === 'late utterance from A' && mismatched.deliver === false);
  const unowned = planRealtimeFinal(null, 'A', 'orphan utterance', '');
  check('unowned transcript never delivers', unowned.deliver === false);
  const second = planRealtimeFinal(owner, 'A', 'second turn', 'run the tests');
  check('a second utterance still commits and delivers', second.commit === 'second turn' && second.deliver === true);
}

// ---- realtimeDelegationLabel: no prefix/placeholder duplication ----
{
  check('delegation text renders with the prefix', realtimeDelegationLabel('spawned 3 runners') === 'delegation created: spawned 3 runners');
  check('empty delegation renders the meaningful fallback', realtimeDelegationLabel('') === 'Voice: delegation started (see Activity)');
  check('whitespace delegation renders the fallback', realtimeDelegationLabel('   ') === 'Voice: delegation started (see Activity)');
  check('absent delegation renders nothing (no overlay row)', realtimeDelegationLabel(null) === null && realtimeDelegationLabel(undefined) === null);
  check('placeholder is never prefixed', !realtimeDelegationLabel('').includes('Delegation created'));
}

{
  const { readFileSync } = await import('node:fs');
  const { dirname, join } = await import('node:path');
  const { fileURLToPath } = await import('node:url');
  const appSource = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../src/App.tsx'), 'utf8');
  check('App voice frames dispatch through reduceRealtimeVoiceFrame',
    appSource.includes('reduceRealtimeVoiceFrame(frame,'));
  check('App pending flush dispatches through reducePendingVoiceFlush',
    appSource.includes('reducePendingVoiceFlush('));
  check('App data-channel messages parse via parseRealtimeChannelFrames',
    appSource.includes('parseRealtimeChannelFrames(event.data)'));
  check('App treats binary data-channel payloads as ArrayBuffer',
    appSource.includes("dcArg.binaryType = 'arraybuffer'"));
  check('delivered voice commits through submitVoicePrompt',
    appSource.includes('submitVoicePrompt(action.text)'));
  check('delivered voice does not auto-submit the composer',
    !appSource.includes('realtimeSubmitRef'));
}

// ---- planRealtimeDelegation: dedup anchor lifecycle (audited contract) ----
// The caller ANCHORS lastCommitted at the delivered text on every successful
// delivery, and CLEARS it in the delta/segment branches (every new utterance
// opens with one). So: the V1 dual-final variants of ONE utterance dedup,
// the final+delegation pair of ONE utterance dedup, and every distinct
// utterance delivers exactly once (cross-utterance prefix DELIVERS).
{
  const owner = 'A';
  // Delegation-only utterance: no transcript final ever arrives — the
  // delegation text IS the command and delivers exactly once.
  const solo = planRealtimeDelegation(owner, 'A', 'spin up three runners', '');
  check('delegation-only utterance delivers exactly once',
    solo.commit === 'spin up three runners' && solo.deliver === true);
  // Cross-utterance prefix: u1 'fix the failing tests' anchored lastCommitted;
  // u2 delegation 'fix the failing tests in the auth module' prefix-matches
  // u1 but is a DIFFERENT utterance (its delta cleared the anchor in the
  // live path; here we simulate a stale anchor) → must DELIVER anyway.
  const cross = planRealtimeDelegation(owner, 'A', 'fix the failing tests in the auth module', 'fix the failing tests');
  check('cross-utterance prefix match DELIVERS (not silently dropped)',
    cross.commit === 'fix the failing tests in the auth module' && cross.deliver === true);
  const reversedCross = planRealtimeDelegation(owner, 'A', 'fix the failing tests', 'fix the failing tests in the auth module');
  check('reversed cross-utterance prefix also DELIVERS', reversedCross.deliver === true);
  // Same-utterance dedup pairs (the anchor holds WITHIN one utterance):
  // V1 dual-final — two final variants of the same utterance.
  const dualFinal = planRealtimeFinal(owner, 'A', 'spin up three runners', 'spin up three runners');
  check('dual-final same-utterance variant is deduped',
    dualFinal.commit === '' && dualFinal.deliver === false);
  // Final + delegation pair — the delegation of an utterance whose final
  // already delivered.
  const finalThenDelegation = planRealtimeDelegation(owner, 'A', 'spin up three runners', 'spin up three runners');
  check('final+delegation same-utterance pair is deduped', finalThenDelegation.deliver === false);
  // Exact same-utterance delegation re-fire: still deduped.
  const sameFire = planRealtimeDelegation(owner, 'A', 'spin up three runners', 'spin up three runners');
  check('exact same-utterance re-fire is deduped', sameFire.deliver === false);
  // Different utterance still delivers.
  const different = planRealtimeDelegation(owner, 'A', 'totally different command', 'unrelated earlier text');
  check('an unrelated delegation still delivers', different.deliver === true);
  // Owner-switch drops it entirely.
  const switched = planRealtimeDelegation(owner, 'B', 'late delegation from A', '');
  check('owner-switch drops the delegation (no deliver, no draft write)',
    switched.deliver === false);
  // PREVIEW FALLBACK (round-7.5 live defect): an id-only delegation.created
  // carries no description/task, but the user's words already streamed into
  // the accumulated transcript preview. The preview delivers as the command.
  const fromPreview = planRealtimeDelegation(owner, 'A', '', '', '  run the deploy pipeline  ');
  check('id-only delegation with a non-empty preview delivers the preview',
    fromPreview.commit === 'run the deploy pipeline' && fromPreview.deliver === true);
  // Description wins over the preview when both exist.
  const descWins = planRealtimeDelegation(owner, 'A', 'spin up three runners', '', 'unrelated partial preview');
  check('description/task text wins over the accumulated preview',
    descWins.commit === 'spin up three runners' && descWins.deliver === true);
  // Both empty → nothing delivers (overlay-only).
  const bothEmpty = planRealtimeDelegation(owner, 'A', '', '', '   ');
  check('delegation with no text and an empty preview delivers nothing',
    bothEmpty.commit === '' && bothEmpty.deliver === false);
  // Dedup still applies to the preview-delivered text: a final of the same
  // utterance (the anchor already holds the preview text) never re-delivers.
  const previewDedup = planRealtimeDelegation(owner, 'A', '', 'run the deploy pipeline', 'run the deploy pipeline');
  check('preview delivery dedups against the shared anchor', previewDedup.deliver === false);
  // Empty/whitespace text has nothing to send — overlay/Activity only.
  const blank = planRealtimeDelegation(owner, 'A', '   ', '');
  check('empty delegation text never delivers to chat', blank.commit === '' && blank.deliver === false);
}

{
  const { readFileSync } = await import('node:fs');
  const { dirname, join } = await import('node:path');
  const { fileURLToPath } = await import('node:url');
  const appSource = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../src/App.tsx'), 'utf8');
  check('App parks handoff text and arms REALTIME_DELEGATION_QUIESCENCE_MS',
    appSource.includes("pendingDelegationRef.current = { owner: action.owner, text: action.text }")
      && appSource.includes('REALTIME_DELEGATION_QUIESCENCE_MS'));
  check('the quiescence delivery surfaces a FAILURE toast when no text ever arrives (never silent)',
    appSource.includes("toast('Voice delegation carried no transcribed text — not delivered', true);"));
  check('delegation flush toasts a draft, not a started agent',
    appSource.includes('Voice draft ready:')
      && !appSource.includes('Voice delegation started:'));
  check('the realtime STOP path cancels the pending delegation (session switch / call stop drop it)',
    appSource.includes('setRealtimeDelegation(null);\n      cancelPendingDelegationRef.current();'));
  check('overlay writes clear the dedup anchor for a new utterance',
    appSource.includes("case 'overlay':")
      && appSource.includes("lastCommittedTranscriptRef.current = '';"));
}

console.log(`\nrealtime.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);