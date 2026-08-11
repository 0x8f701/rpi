/**
 * Realtime data-channel pure helpers — session config, live-mode
 * normalization, event-frame classification, and error/transcript extraction.
 *
 * Shared by App.tsx's WebRTC realtime flow and the node-runnable regression
 * test (scripts/realtime.test.ts). DELIBERATELY has no DOM/browser/WebSocket
 * dependency so `esbuild --platform=node` can bundle the test in isolation
 * (mirroring src/transcript.ts + scripts/transcript.test.ts).
 *
 * The session object shape matches the CLIProxyAPI/chatgpt.com V1 "quicksilver"
 * contract exactly (Contract 2): the SAME object rides the Rust create-call
 * POST body AND the web `session.update` event frame sent over the
 * `oai-events` RTCDataChannel (Contract 4), so the two never drift.
 */

/** PCM input sample rate the V1 session expects (24 kHz mono). A JSON number,
 *  not a string, so it serializes as `24000`. */
export const REALTIME_AUDIO_SAMPLE_RATE = 24000;

/** First non-empty trimmed string among `values` (empty strings and
 *  non-strings are skipped) — used to read defensive field variants off
 *  realtime event frames without guessing the exact key the backend chose. Pure so
 *  the transcript/error extractors below stay testable. */
export function firstString(...values: unknown[]): string {
  for (const value of values) {
    if (typeof value === 'string' && value.trim()) return value.trim();
  }
  return '';
}

/** The V1 "quicksilver" realtime session object. Carried in the create-call
 *  POST body (`{sdp, session}`) and re-sent verbatim as the `session.update`
 *  event frame's `session` field over the `oai-events` data channel. `voice`
 *  nests under `audio.output.voice` (the legacy top-level `voice` was silently
 *  ignored by the V1 parser, so the configured voice never took effect). */
export interface RealtimeSessionConfig {
  type: 'quicksilver';
  model: string;
  audio: {
    input: { format: { type: 'audio/pcm'; rate: number } };
    output: { voice: string };
  };
}

/** Build the V1 "quicksilver" session config. Shape matches the contract
 *  exactly: `{type:'quicksilver', model, audio:{input:{format:{type:'audio/pcm',
 *  rate:24000}}, output:{voice}}}`. The caller defaults `model`/`voice` before
 *  calling, so both are always non-empty; the object never carries an `id`
 *  (the official client strips it pre-send). */
export function buildRealtimeSessionConfig(model: string, voice: string): RealtimeSessionConfig {
  return {
    type: 'quicksilver',
    model,
    audio: {
      input: { format: { type: 'audio/pcm', rate: REALTIME_AUDIO_SAMPLE_RATE } },
      output: { voice },
    },
  };
}

/** Normalize a live mode string for comparison: trim + ASCII lowercase.
 *  Non-string / whitespace → ''. Guards mode switching against accidental
 *  casing or surrounding whitespace from the settings wire. */
export function normalizeLiveMode(mode: unknown): string {
  if (typeof mode !== 'string') return '';
  return mode.trim().toLowerCase();
}

/** True only when the advertised live settings are enabled and select realtime
 *  mode (after trim + ASCII lowercase). False for missing / disabled / 'stt' /
 *  unknown — it NEVER assumes realtime when the backend omits `live`, so a
 *  stale or absent runtimeSettings.live cannot leave the composer stuck in
 *  realtime mode. */
export function isRealtimeLiveMode(live: { enabled?: unknown; mode?: unknown } | null | undefined): boolean {
  return live?.enabled === true && normalizeLiveMode(live.mode) === 'realtime';
}

/** Parse a realtime `error` event frame into a bounded user-facing message. Reads
 *  the nested `error.message` / `error.code` (V1 shape) first, falling back to
 *  the top-level `message` / `code` (CLIProxyAPI alias). Never throws; returns
 *  a bounded default when no detail is present so the user always sees
 *  *something* actionable instead of a silent failure. */
export function realtimeErrorMessage(frame: Record<string, unknown>): string {
  const err = frame.error;
  const src = err && typeof err === 'object' ? (err as Record<string, unknown>) : {};
  const message = firstString(src.message, frame.message);
  const code = firstString(src.code, frame.code);
  if (message && code) return `realtime error [${code}]: ${message}`;
  if (message) return `realtime error: ${message}`;
  if (code) return `realtime error [${code}]`;
  return 'realtime session error';
}

/** Classify a realtime event type as a USER input transcript delta, final, or
 *  neither. The CLIProxyAPI aliases (`transcript.delta`/`transcript.done`) are
 *  kept for compatibility. Assistant OUTPUT transcript events
 *  (`conversation.output_transcript.*`, `response.output_audio_transcript.*`)
 *  classify as `null` so they can never be committed to the composer as a user
 *  draft. */
export function classifyInputTranscriptEvent(type: string): 'delta' | 'final' | null {
  switch (type) {
    case 'transcript.delta':
    case 'conversation.input_transcript.delta':
    case 'conversation.item.input_audio_transcription.delta':
      return 'delta';
    case 'transcript.done':
    case 'conversation.input_transcript.done':
    case 'conversation.item.input_audio_transcription.completed':
      return 'final';
    default:
      return null;
  }
}

/** Extract the finalized user-input transcript text from a V1/alias final
 *  event. Reads the canonical `transcript` field, the nested
 *  `item.transcript` / `item.content`
 *  (`conversation.item.input_audio_transcription.completed`), and the
 *  `text` / `delta` alias fallbacks. Returns '' when no text is present. */
export function finalTranscriptText(frame: Record<string, unknown>): string {
  const item = frame.item;
  const src = item && typeof item === 'object' ? (item as Record<string, unknown>) : {};
  return firstString(frame.transcript, src.transcript, src.content, frame.text, frame.delta);
}

/** Decide whether a finalized input transcript should be committed to the
 *  composer. Returns the trimmed text to commit, or '' when it is empty or
 *  identical to the last committed transcript. The V1 protocol emits BOTH
 *  `conversation.input_transcript.done` AND
 *  `conversation.item.input_audio_transcription.completed` for one utterance,
 *  both carrying the same final text — the second must not double-commit, and
 *  a re-fire of an already-committed utterance is suppressed. */
export function nextInputTranscriptCommit(finalText: string, lastCommitted: string): string {
  const text = typeof finalText === 'string' ? finalText.trim() : '';
  if (!text) return '';
  if (text === lastCommitted) return '';
  return text;
}

/** The OpenAI Realtime WebRTC event data channel label. Created on the
 *  RTCPeerConnection BEFORE the SDP offer so it is negotiated in the answer;
 *  session.update + server events (transcript/delegation/error) flow over it
 *  instead of a direct sideband WebSocket. */
export const REALTIME_EVENT_CHANNEL = 'oai-events';

/** Dependencies injected into `setupRealtimeCall` so the call-setup ORDER is
 *  testable with recorded mocks (no browser/WebSocket needed). The types are
 *  erased by esbuild for the node test, which supplies plain mock objects. */
export interface RealtimeCallDeps {
  getUserMedia: () => Promise<MediaStream>;
  createPeerConnection: () => RTCPeerConnection;
  sendCreateCall: (sdpOffer: string) => Promise<{ sdp: string; callId: string }>;
  /** Wire pc.ontrack / pc.onconnectionstatechange right after addTrack. */
  onPeerConnection?: (pc: RTCPeerConnection) => void;
  /** Wire the data channel handlers right after createDataChannel (before
   *  createOffer), so onopen/onmessage are in place before the channel opens. */
  onDataChannel?: (pc: RTCPeerConnection, dc: RTCDataChannel) => void;
}

/** Orchestrate the realtime call setup in the contract-mandated order:
 *  getUserMedia -> RTCPeerConnection -> addTrack -> wire pc handlers ->
 *  createDataChannel('oai-events') -> wire channel handlers -> createOffer ->
 *  setLocalDescription -> realtime_create_call -> setRemoteDescription.
 *
 *  The `oai-events` data channel is created BEFORE createOffer so it is
 *  negotiated in the SDP; NO direct WebSocket to the CLIProxy realtime
 *  endpoint is opened (events ride the WebRTC DTLS transport — no browser-side
 *  Bearer header, no HTTPS/http mixed-content block, which is why the direct
 *  sideband WebSocket was removed). Returns the peer connection, data channel,
 *  and call id; the caller owns event-handler wiring via the callbacks and
 *  teardown via stopRealtime. */
export async function setupRealtimeCall(
  deps: RealtimeCallDeps,
): Promise<{ pc: RTCPeerConnection; dc: RTCDataChannel; callId: string }> {
  const stream = await deps.getUserMedia();
  const pc = deps.createPeerConnection();
  for (const track of stream.getTracks()) pc.addTrack(track, stream);
  deps.onPeerConnection?.(pc);
  const dc = pc.createDataChannel(REALTIME_EVENT_CHANNEL);
  deps.onDataChannel?.(pc, dc);
  const offer = await pc.createOffer();
  await pc.setLocalDescription(offer);
  const result = await deps.sendCreateCall(offer.sdp ?? '');
  if (!result || !result.sdp || !result.callId) {
    throw new Error('realtime_create_call returned no SDP answer or call id');
  }
  await pc.setRemoteDescription({ type: 'answer', sdp: result.sdp });
  return { pc, dc, callId: result.callId };
}