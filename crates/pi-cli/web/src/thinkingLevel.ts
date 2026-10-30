// Pure thinking-level selection state machine for the Web `#thinking-select`
// (mirror of the goal.ts pure-helper pattern): each distinct selection sends
// exactly one set_thinking_level at the SYNCHRONOUS commandTarget; the chosen
// level displays optimistically and settles to the backend's effective
// (possibly clamped) level; failures revert; state is per-session.
export interface ThinkingCommandAuthority {
  commandTargetSessionId?: string | null;
  displayedSessionId?: string | null;
}

export function thinkingCommandTargetSessionId(
  authority: ThinkingCommandAuthority | null | undefined,
  fallback: string | null | undefined,
): string {
  const candidates = [
    authority?.commandTargetSessionId,
    authority?.displayedSessionId,
    fallback,
  ];
  for (const sid of candidates) {
    if (typeof sid === 'string' && sid !== '') return sid;
  }
  return '';
}

export function setThinkingLevelCommand(
  sid: string,
  level: string,
): { type: 'set_thinking_level'; level: string; sessionId: string } {
  return { type: 'set_thinking_level', level, sessionId: sid };
}

export interface ThinkingLevelRequest {
  readonly seq: number;
  readonly level: string;
}

export interface ThinkingLevelState {
  readonly settled: Readonly<Record<string, string>>;
  readonly pending: ReadonlyMap<string, ThinkingLevelRequest>;
  readonly nextSeq: number;
}

export const EMPTY_THINKING_LEVEL_STATE: ThinkingLevelState = {
  settled: {},
  pending: new Map(),
  nextSeq: 1,
};

export function thinkingLevelView(state: ThinkingLevelState): Record<string, string> {
  const view: Record<string, string> = { ...state.settled };
  for (const [sid, request] of state.pending) view[sid] = request.level;
  return view;
}

/** Message to surface when the backend CLAMPED a set_thinking_level change:
 *  the wire envelope is `{requested, level, clamped, message}` (rpc.rs
 *  SetThinkingLevel) and a clamp silently reverts the select — the message is
 *  the user-facing explanation. Returns null for every non-clamp or
 *  malformed envelope (missing/non-true `clamped`, missing/empty/non-string
 *  `message`), so a bare ack never toasts. */
export function thinkingLevelClampMessage(response: unknown): string | null {
  if (!response || typeof response !== 'object' || Array.isArray(response)) return null;
  if (!('clamped' in response) || response.clamped !== true) return null;
  if (!('message' in response) || typeof response.message !== 'string' || response.message === '') return null;
  return response.message;
}

export interface ThinkingLevelDecision {
  readonly state: ThinkingLevelState;
  readonly send: boolean;
}

export function requestThinkingLevel(
  state: ThinkingLevelState,
  sid: string,
  level: string,
  availableLevels?: readonly string[],
): ThinkingLevelDecision {
  if (sid === '' || level === '') return { state, send: false };
  if (availableLevels && availableLevels.length > 0 && !availableLevels.includes(level)) {
    return { state, send: false };
  }
  const pending = state.pending.get(sid);
  if (pending) {
    if (pending.level === level) return { state, send: false };
  } else if (state.settled[sid] === level) {
    return { state, send: false };
  }
  const next: ThinkingLevelState = {
    settled: state.settled,
    pending: new Map(state.pending).set(sid, { seq: state.nextSeq, level }),
    nextSeq: state.nextSeq + 1,
  };
  return { state: next, send: true };
}

export function settleThinkingRequest(
  state: ThinkingLevelState,
  sid: string,
  seq: number,
  ok: boolean,
  effectiveLevel?: string | null,
): ThinkingLevelState {
  if (sid === '') return state;
  const pending = state.pending.get(sid);
  if (!pending || pending.seq !== seq) {
    if (!ok || typeof effectiveLevel !== 'string' || effectiveLevel === '') return state;
    return { settled: { ...state.settled, [sid]: effectiveLevel }, pending: state.pending, nextSeq: state.nextSeq };
  }
  const nextPending = new Map(state.pending);
  nextPending.delete(sid);
  if (ok && typeof effectiveLevel === 'string' && effectiveLevel !== '') {
    return { settled: { ...state.settled, [sid]: effectiveLevel }, pending: nextPending, nextSeq: state.nextSeq };
  }
  return { settled: state.settled, pending: nextPending, nextSeq: state.nextSeq };
}

export function reconcileThinkingLevel(
  state: ThinkingLevelState,
  sid: string,
  level: string,
): ThinkingLevelState {
  if (sid === '' || level === '' || state.pending.has(sid)) return state;
  if (state.settled[sid] === level) return state;
  return { settled: { ...state.settled, [sid]: level }, pending: state.pending, nextSeq: state.nextSeq };
}

export function selectionAppliesNextTurn(state: ThinkingLevelState, sid: string): boolean {
  const pending = state.pending.get(sid);
  return pending !== undefined && pending.level !== state.settled[sid];
}