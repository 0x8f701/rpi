/**
 * Realtime data-channel pure helpers — session config, live-mode
 * normalization, event-frame classification, error/transcript extraction,
 * and captured-session owner binding for create/stop RPCs.
 *
 * Shared by App.tsx's WebRTC realtime flow and the node-runnable regression
 * test (scripts/realtime.test.ts). DELIBERATELY has no DOM/browser/WebSocket
 * dependency so `esbuild --platform=node` can bundle the test in isolation
 * (mirroring src/transcript.ts + scripts/transcript.test.ts).
 *
 * The session object here is the frameless v3 shape used by both the Rust
 * create-call POST and the web `session.update` event over the `oai-events`
 * RTCDataChannel. It carries instructions, output voice, and client
 * delegation; model selection stays server/proxy-owned.
 *
 * Create/stop/cleanup RPCs always stamp the session captured at call start,
 * never the post-switch current session — switching A→B must not leak A's
 * call onto B or stop B's runtime.
 */

import { withResolvers } from './withResolvers';


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

/** The frameless v3 realtime session object used by both create-call and the
 *  retained idempotent `session.update` data-channel frame. */
export interface RealtimeSessionConfig {
  instructions: string;
  audio: {
    output: { voice: string };
  };
  delegation: {
    type: 'client';
  };
}


/** Build the frameless realtime session config. Model selection stays
 *  proxy-owned; the create-call endpoint accepts output voice and client
 *  delegation but rejects `audio.input` in AVAS session data. */
export function buildRealtimeSessionConfig(_model: string, voice: string): RealtimeSessionConfig {
  return {
    instructions: '',
    audio: { output: { voice } },
    delegation: { type: 'client' },
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

/** Classify a realtime event type within the USER input-transcription
 *  category. The CLIProxyAPI aliases (`transcript.delta`/`transcript.done`)
 *  are kept for compatibility. GA v3 events:
 *  `conversation.item.input_audio_transcription.delta` (incremental text),
 *  `.segment` (a full interim segment that SUPERSEDES the running preview),
 *  `.completed` (the authoritative final), and `.failed` (transcription
 *  error — surfaces a toast, never commits). The Codex FRAMELESS protocol
 *  (what CLIProxyAPI relays verbatim over the `oai-events` data channel —
 *  `internal/client/codex/live/media.go` bridges it byte-for-byte) uses
 *  `input_transcript.added` (full live text at `item.text`, segment replace) and
 *  `turn.done` (authoritative final at `turn.transcript`, role-gated).
 *  Assistant OUTPUT transcript events (`conversation.output_transcript.*`,
 *  `response.output_audio_transcript.*`, `output_transcript.added`) classify
 *  as `null` so they can never be committed to the composer as a user
 *  draft. */
export function classifyInputTranscriptEvent(
  type: string
): 'delta' | 'segment' | 'final' | 'failed' | null {
  switch (type) {
    case 'transcript.delta':
    case 'conversation.input_transcript.delta':
    case 'conversation.item.input_audio_transcription.delta':
      return 'delta';
    case 'conversation.item.input_audio_transcription.segment':
    case 'input_transcript.added':
      return 'segment';
    case 'transcript.done':
    case 'conversation.input_transcript.done':
    case 'conversation.item.input_audio_transcription.completed':
    case 'turn.done':
      return 'final';
    case 'conversation.item.input_audio_transcription.failed':
    case 'transcription.failed':
      return 'failed';
    default:
      return null;
  }
}

/** Extract the interim USER transcript text from a delta event. The
 *  CLIProxyAPI/GA v3 aliases carry `delta`/`transcript` at the top level;
 *  the frameless `input_transcript.added` nests the live text under
 *  `item.text`. The frameless fragment is consumed RAW — each fragment is
 *  appended verbatim, so trimming here would eat the significant whitespace
 *  BETWEEN fragments. Returns '' when no text is present. */
export function inputTranscriptDeltaText(frame: Record<string, unknown>): string {
  const item = frame.item;
  const rawItemText = item && typeof item === 'object' ? (item as Record<string, unknown>).text : undefined;
  if (typeof rawItemText === 'string') return rawItemText;
  return firstString(frame.delta, frame.transcript);
}

/** Extract the FULL interim segment text from a supersede-semantics segment
 *  event (GA `…transcription.segment`; the frameless protocol's
 *  `input_transcript.added` also carries the full live text of the segment).
 *  Reads the full-text fields first, falling back to the delta aliases.
 *  Returns '' when no text is present. */
export function inputTranscriptSegmentText(frame: Record<string, unknown>): string {
  const item = frame.item;
  return firstString(
    frame.text,
    frame.transcript,
    item && typeof item === 'object' ? (item as Record<string, unknown>).text : undefined,
    frame.delta
  );
}

/** Extract the finalized user-input transcript text from a final event. The
 *  frameless `turn.done` nests `transcript` under `turn` and is ROLE-GATED:
 *  only a `user` turn is input text — an assistant turn is OUTPUT and must
 *  never commit to the composer. The V1/alias finals read the canonical
 *  `transcript` field, the nested `item.transcript` / `item.content`
 *  (`conversation.item.input_audio_transcription.completed`), and the
 *  `text` / `delta` alias fallbacks. Returns '' when no text is present. */
export function finalTranscriptText(frame: Record<string, unknown>): string {
  const turn = frame.turn;
  if (turn && typeof turn === 'object') {
    const t = turn as Record<string, unknown>;
    if (t.role !== undefined && t.role !== 'user') return '';
    return firstString(t.transcript, frame.transcript, frame.text, frame.delta);
  }
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

/** `realtime_create_call` targeted at the session captured when the call
 *  started. ICE gather is async, so reading the active session at POST time
 *  would create the call on a session the user switched to mid-setup. */
export function realtimeCreateCallCommand(
  owner: string,
  sdpOffer: string,
): { type: 'realtime_create_call'; sdpOffer: string; sessionId: string } {
  return { type: 'realtime_create_call', sdpOffer, sessionId: owner };
}

/** `realtime_stop` targeted at the session captured when the call started.
 *  Cleanup after an A→B switch must stop A's runtime, never the current
 *  session's. */
export function realtimeStopCommand(owner: string): { type: 'realtime_stop'; sessionId: string } {
  return { type: 'realtime_stop', sessionId: owner };
}

/** True when a captured realtime owner may mutate the active composer. A
 *  late input-transcript final is dropped unless the owner is still the
 *  current session — switching A→B must never write A's utterance into B. */
export function shouldCommitRealtimeTranscript(owner: string | null | undefined, current: string | null | undefined): boolean {
  return typeof owner === 'string' && owner !== '' && owner === current;
}

/** Plan for one finalized input transcript: the deduped utterance (`commit`,
 *  empty = drop) and whether it may be written to the owner session's
 *  composer as a draft. Delivery is owner-gated: switching A→B must never
 *  write A's utterance into B — the late transcript is dropped entirely. */
export interface RealtimeFinalPlan {
  readonly commit: string;
  readonly deliver: boolean;
}
export function planRealtimeFinal(
  owner: string | null | undefined,
  current: string | null | undefined,
  finalText: string,
  lastCommitted: string,
): RealtimeFinalPlan {
  const commit = nextInputTranscriptCommit(finalText, lastCommitted);
  return { commit, deliver: commit !== '' && shouldCommitRealtimeTranscript(owner, current) };
}

/** Overlay label for a `delegation.created` frame. No delegation text renders
 *  a meaningful bounded label — never the meaningless duplication
 *  'delegation created: Delegation created'. */
export function realtimeDelegationLabel(text: string | null | undefined): string | null {
  if (text === null || text === undefined) return null;
  const trimmed = text.trim();
  if (trimmed === '') return 'Voice: delegation started (see Activity)';
  return `delegation created: ${trimmed}`;
}

/** Extract the spoken command from a `delegation.created` frame. The REAL
 *  wire shape (Codex frameless-bidi protocol, relayed byte-for-byte by
 *  CLIProxyAPI over the `oai-events` data channel) carries NO `delegation`
 *  field — the command rides `item.content[]` entries of type
 *  `input_text`:
 *  `{ type: 'delegation.created', item: { id, type: 'delegation',
 *  target: 'client', content: [{ type: 'input_text', text }] } }`.
 *  Legacy wrapped shapes (a raw string `delegation`, or
 *  `delegation.description`/`.task`) are retained defensively. An
 *  id-only/untextable delegation returns '' — the caller must surface a
 *  failure instead of silently dropping the utterance. */
export function delegationCreatedText(frame: Record<string, unknown>): string {
  const item = frame.item && typeof frame.item === 'object'
    ? (frame.item as Record<string, unknown>)
    : null;
  if (item && Array.isArray(item.content)) {
    const text = (item.content as unknown[])
      .filter(
        (c): c is Record<string, unknown> =>
          !!c && typeof c === 'object' && (c as Record<string, unknown>).type === 'input_text'
      )
      .map((c) => (typeof c.text === 'string' ? c.text : ''))
      .join('');
    if (text.trim() !== '') return text.trim();
  }
  const d = frame.delegation;
  return firstString(
    item ? item.input_transcript : undefined,
    frame.input_transcript,
    typeof d === 'string' ? d : undefined,
    d && typeof d === 'object' ? (d as Record<string, unknown>).description : undefined,
    d && typeof d === 'object' ? (d as Record<string, unknown>).task : undefined,
  );
}

/** Plan for a `delegation.created` voice flow: the delegation text IS the
 *  user's spoken command, so it delivers to chat exactly like a transcript
 *  final — owner-gated, deduped by EXACT match against the shared
 *  lastCommitted text (same-utterance re-fire and the V1 dual-final
 *  variants of ONE utterance dedup; the caller anchors lastCommitted at the
 *  delivered text and clears it in the delta/segment branches where every
 *  new utterance opens). Text resolution order: (1) the extracted delegation
 *  text (`delegationCreatedText`); (2) the ACCUMULATED transcript preview
 *  the caller passes — the spoken words streamed into the overlay before
 *  the delegation arrived; (3) both empty → deliver=false; the caller MUST
 *  surface a failure toast for (3) — an untextable delegation with no
 *  preview is a dropped utterance, never silence. */
export interface RealtimeDelegationPlan {
  readonly commit: string;
  readonly deliver: boolean;
}
export function planRealtimeDelegation(
  owner: string | null | undefined,
  current: string | null | undefined,
  delegationText: string,
  lastCommitted: string,
  previewText: string = '',
): RealtimeDelegationPlan {
  const text = (typeof delegationText === 'string' && delegationText.trim() !== ''
    ? delegationText
    : typeof previewText === 'string' && previewText.trim() !== ''
      ? previewText
      : '').trim();
  if (text === '') return { commit: '', deliver: false };
  if (!shouldCommitRealtimeTranscript(owner, current)) return { commit: text, deliver: false };
  if (text === lastCommitted.trim()) return { commit: text, deliver: false };
  return { commit: text, deliver: true };
}

/** Bounded quiescence window (ms) for delegation.created voice delivery: the
 *  delegation can arrive BEFORE/WHILE the input-transcript deltas stream, so
 *  delivery waits this long for the preview to catch up, then delivers the
 *  RICHER of (delegation text, preview). A transcript final arriving first
 *  delivers through the normal path and cancels the pending delegation. */
export const REALTIME_DELEGATION_QUIESCENCE_MS = 1500;

export type RealtimeVoiceAction =
  | { kind: 'noop' }
  | { kind: 'overlay'; text: string }
  | { kind: 'failed'; message: string }
  | { kind: 'deliver'; text: string; cancelPending: boolean }
  | { kind: 'consume-final' }
  | { kind: 'park'; owner: string; text: string }
  | { kind: 'error'; message: string }
  | { kind: 'unhandled'; type: string }
  | { kind: 'empty-handoff' };

export interface RealtimeVoiceContext {
  owner: string | null | undefined;
  current: string | null | undefined;
  overlay: string;
  lastCommitted: string;
  pendingText: string;
}

export function isRealtimeHandoffType(type: string): boolean {
  return type === 'delegation.created' || type === 'conversation.handoff.requested';
}

export function isIgnorableTranscriptFinal(frame: Record<string, unknown>): boolean {
  const type = typeof frame.type === 'string' ? frame.type : '';
  if (type !== 'turn.done') return false;
  const turn = frame.turn;
  if (!turn || typeof turn !== 'object') return false;
  const role = (turn as Record<string, unknown>).role;
  return typeof role === 'string' && role !== 'user';
}

export function userFinalUtterance(frame: Record<string, unknown>, preview: string): string {
  const fromFrame = finalTranscriptText(frame);
  if (fromFrame) return fromFrame;
  return typeof preview === 'string' ? preview.trim() : '';
}

function deliverUtterance(
  owner: string | null | undefined,
  current: string | null | undefined,
  text: string,
  lastCommitted: string,
): RealtimeVoiceAction {
  const plan = planRealtimeFinal(owner, current, text, lastCommitted);
  if (plan.deliver) return { kind: 'deliver', text: plan.commit, cancelPending: true };
  return { kind: 'consume-final' };
}

export function reduceRealtimeVoiceFrame(
  frame: Record<string, unknown>,
  ctx: RealtimeVoiceContext,
): RealtimeVoiceAction {
  if (!shouldCommitRealtimeTranscript(ctx.owner, ctx.current)) return { kind: 'noop' };
  const type = typeof frame.type === 'string' ? frame.type : '';
  const cls = classifyInputTranscriptEvent(type);
  if (cls === 'delta') {
    const delta = inputTranscriptDeltaText(frame);
    if (!delta) return { kind: 'noop' };
    return { kind: 'overlay', text: `${ctx.overlay}${delta}` };
  }
  if (cls === 'segment') {
    const segment = inputTranscriptSegmentText(frame);
    if (!segment) return { kind: 'noop' };
    return { kind: 'overlay', text: segment };
  }
  if (cls === 'failed') {
    return { kind: 'failed', message: realtimeErrorMessage(frame) };
  }
  if (cls === 'final') {
    if (isIgnorableTranscriptFinal(frame)) {
      const fallback = ctx.overlay.trim() || ctx.pendingText.trim();
      if (!fallback) return { kind: 'noop' };
      return deliverUtterance(ctx.owner, ctx.current, fallback, ctx.lastCommitted);
    }
    const text = userFinalUtterance(frame, ctx.overlay);
    if (!text) return { kind: 'noop' };
    return deliverUtterance(ctx.owner, ctx.current, text, ctx.lastCommitted);
  }
  if (isRealtimeHandoffType(type)) {
    return { kind: 'park', owner: ctx.owner ?? '', text: delegationCreatedText(frame) };
  }
  if (type === 'error') {
    return { kind: 'error', message: realtimeErrorMessage(frame) };
  }
  if (type === 'output_audio.delta') return { kind: 'noop' };
  return { kind: 'unhandled', type };
}

export function reducePendingVoiceFlush(
  pending: { owner: string; text: string } | null,
  overlay: string,
  current: string | null | undefined,
  lastCommitted: string,
): RealtimeVoiceAction {
  if (!pending) return { kind: 'noop' };
  const richer = overlay.trim().length > pending.text.trim().length ? overlay : pending.text;
  if (richer.trim() === '') return { kind: 'empty-handoff' };
  const plan = planRealtimeDelegation(pending.owner, current, richer, lastCommitted);
  if (!plan.deliver) return { kind: 'noop' };
  return { kind: 'deliver', text: plan.commit, cancelPending: false };
}

export function unwrapRealtimeFrame(raw: unknown): Record<string, unknown> | null {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return null;
  const frame = raw as Record<string, unknown>;
  const nested = frame.event;
  if (nested && typeof nested === 'object' && !Array.isArray(nested)) {
    const inner = nested as Record<string, unknown>;
    if (typeof inner.type === 'string' && inner.type !== '') {
      const outerType = typeof frame.type === 'string' ? frame.type : '';
      if (outerType === '' || outerType === 'event' || outerType === 'server_event') {
        return inner;
      }
    }
  }
  return frame;
}

export function decodeRealtimeChannelData(data: unknown): string | null {
  if (typeof data === 'string') return data;
  if (data instanceof ArrayBuffer) return new TextDecoder().decode(data);
  if (ArrayBuffer.isView(data)) {
    const view = data as ArrayBufferView;
    return new TextDecoder().decode(new Uint8Array(view.buffer, view.byteOffset, view.byteLength));
  }
  return null;
}

export function parseRealtimeChannelFrames(data: unknown): Record<string, unknown>[] {
  const text = decodeRealtimeChannelData(data);
  if (text == null || text.trim() === '') return [];
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return [];
  }
  const items = Array.isArray(parsed) ? parsed : [parsed];
  const frames: Record<string, unknown>[] = [];
  for (const item of items) {
    const frame = unwrapRealtimeFrame(item);
    if (frame) frames.push(frame);
  }
  return frames;
}

/** A realtime call is stopped when its owning session is replaced by another
 * active session. This single-owner semantic prevents every late delta,
 * delegation, connection state, and toast from projecting onto the new page. */
export function shouldStopRealtimeOnSessionSwitch(
  owner: string | null | undefined,
  previous: string | null | undefined,
  next: string | null | undefined,
): boolean {
  return typeof owner === 'string'
    && owner !== ''
    && owner === previous
    && typeof next === 'string'
    && next !== ''
    && next !== previous;
}

/** The OpenAI Realtime WebRTC event data channel label. Created on the
 *  RTCPeerConnection BEFORE the SDP offer so it is negotiated in the answer;
 *  session.update + server events (transcript/delegation/error) flow over it
 *  instead of a direct sideband WebSocket. */
export const REALTIME_EVENT_CHANNEL = 'oai-events';


/** Bounded wait for ICE candidate gathering before the offer is POSTed. The
 *  OpenAI/CLIProxy realtime `/v1/realtime/calls` endpoint is a single HTTP
 *  round trip with NO trickle-ICE sideband: the offer MUST carry gathered ICE
 *  candidates or the server's answer cannot connect (the data channel never
 *  opens, `session.update` never fires, and the user sees nothing — the
 *  long-standing "realtime doesn't work" failure). The FakePeerConnection
 *  used by the existing E2E/TS mocks never gathers ICE, so this bug is
 *  invisible to those tests. Five seconds is the OpenAI realtime WebRTC
 *  reference client's gather bound; on a loopback/LAN only host candidates
 *  are gathered, which completes in well under a second. */
export const REALTIME_ICE_GATHER_TIMEOUT_MS = 5000;

/** Normalize an `RTCPeerConnectionState` (or `iceConnectionState`) into a
 *  small UI bucket. `disconnected` is distinguished from `failed` because the
 *  former is recoverable (a transient ICE path loss) while the latter is
 *  terminal. Pure so the connection-status overlay and the toast/teardown
 *  decision are testable without a browser. */
export function classifyRealtimeConnectionState(state: unknown): 'new' | 'connecting' | 'connected' | 'disconnected' | 'failed' | 'closed' | 'unknown' {
  if (typeof state !== 'string') return 'unknown';
  switch (state) {
    case 'new':
    case 'connecting':
    case 'connected':
    case 'disconnected':
    case 'failed':
    case 'closed':
      return state;
    // iceConnectionState aliases that map onto the pc.connectionState buckets.
    case 'checking':
      return 'connecting';
    case 'completed':
      return 'connected';
    default:
      return 'unknown';
  }
}

/** A minimal scheduler seam around `setTimeout`/`clearTimeout` so the gather
 *  wait's timeout path is testable deterministically (no real wall-clock
 *  timer). Defaults to the platform globals. The timer id type is opaque
 *  (`unknown`) because browser `setTimeout` returns `number` while Node
 *  returns a `Timeout` handle — both round-trip through the matching
 *  `clearTimeout`. */
export interface RealtimeScheduler {
  setTimeout: (fn: () => void, ms: number) => unknown;
  clearTimeout: (id: unknown) => void;
}

/** Wait until `pc.iceGatheringState === 'complete'` (all candidates gathered),
 *  or until `timeoutMs` elapses — whichever is first. Resolves (never rejects):
 *  a gather timeout is a recoverable fallback that posts whatever candidates
 *  have been gathered so far rather than wedging the call setup forever. Uses
 *  the standard `icegatheringstatechange` event; safe to call when gathering
 *  is already complete (resolves immediately). `scheduler` is a test seam that
 *  defaults to the platform `setTimeout`/`clearTimeout`. */
export async function waitForIceGatheringComplete(
  pc: RTCPeerConnection,
  timeoutMs: number = REALTIME_ICE_GATHER_TIMEOUT_MS,
  scheduler: RealtimeScheduler = {
    setTimeout: (fn, ms) => setTimeout(fn, ms),
    clearTimeout: (id) => clearTimeout(id as number),
  },
): Promise<void> {
  if (pc.iceGatheringState === 'complete') return;
  const { promise, resolve } = withResolvers<void>();
  let done = false;
  const finish = () => {
    if (done) return;
    done = true;
    pc.removeEventListener('icegatheringstatechange', onChange);
    scheduler.clearTimeout(timer);
    resolve();
  };
  const onChange = () => {
    if (pc.iceGatheringState === 'complete') finish();
  };
  const timer = scheduler.setTimeout(finish, timeoutMs);
  pc.addEventListener('icegatheringstatechange', onChange);
  await promise;
}
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
  /** Override the ICE-gathering wait bound (default
   *  [`REALTIME_ICE_GATHER_TIMEOUT_MS`]). The wait happens after
   *  `setLocalDescription` and before `sendCreateCall` so the POSTed offer
   *  carries gathered candidates. */
  iceGatheringTimeoutMs?: number;
  /** Override the gather-wait implementation (test seam: the node mock has no
   *  real ICE layer). Defaults to [`waitForIceGatheringComplete`]. */
  waitForIceGathering?: (pc: RTCPeerConnection, timeoutMs: number) => Promise<void>;
}

/** Orchestrate the realtime call setup in the contract-mandated order:
 *  getUserMedia -> RTCPeerConnection -> addTrack -> wire pc handlers ->
 *  createDataChannel('oai-events') -> wire channel handlers -> createOffer ->
 *  setLocalDescription -> WAIT FOR ICE GATHERING -> realtime_create_call ->
 *  setRemoteDescription.
 *
 *  The ICE-gathering wait is the fix for the long-standing "realtime doesn't
 *  work" failure: the `/v1/realtime/calls` endpoint is a single HTTP round
 *  trip with no trickle sideband, so the offer MUST carry gathered ICE
 *  candidates. Reading `offer.sdp` immediately after `setLocalDescription`
 *  posts an offer with zero candidates — the server's answer cannot connect,
 *  the `oai-events` data channel never opens, `session.update` never fires,
 *  and the user sees no transcript/error (silent failure). After the gather
 *  wait, `pc.localDescription.sdp` carries the gathered `a=candidate` lines
 *  and is what gets POSTed.
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
  // Wait for ICE candidate gathering BEFORE posting: the realtime
  // create-call endpoint does not trickle, so the offer must carry gathered
  // candidates or the answer cannot connect. pc.localDescription.sdp is
  // updated in place as candidates are gathered, so read it AFTER the wait.
  const wait = deps.waitForIceGathering ?? waitForIceGatheringComplete;
  await wait(pc, deps.iceGatheringTimeoutMs ?? REALTIME_ICE_GATHER_TIMEOUT_MS);
  const gatheredSdp = pc.localDescription?.sdp ?? offer.sdp ?? '';
  const result = await deps.sendCreateCall(gatheredSdp);
  if (!result || !result.sdp || !result.callId) {
    throw new Error('realtime_create_call returned no SDP answer or call id');
  }
  await pc.setRemoteDescription({ type: 'answer', sdp: result.sdp });
  return { pc, dc, callId: result.callId };
}