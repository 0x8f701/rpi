/**
 * Socket-generation pure helpers for the reconnect state machine — shared by
 * App.tsx's connect/onOpen/onResponse/sendCommand flow and the node-runnable
 * regression test (scripts/socket.test.ts). No DOM/browser/WebSocket
 * dependency so `esbuild --platform=node` can bundle the test in isolation.
 *
 * Problem this encodes: a socket (A) superseded mid-bootstrap (dropped, B
 * reconnected healthy) used to leave its pending commands in the global map
 * until their bounded timeout; the timeout/late response then continued A's dead
 * bootstrap (applyState / setConnState / refreshGoal) or scheduled a reconnect
 * that replaced the healthy B. The fix ties every pending command and every
 * bootstrap continuation to the socket GENERATION it was sent on, so a stale
 * socket can neither settle a newer socket's pending nor reconnect on its
 * behalf. (Timeout bounds themselves are per-command classes in ./pending.ts;
 * a dropped socket drains its pending immediately in App's onclose with the
 * connection reason, never as a timeout.)
 */

import { withResolvers } from './withResolvers';

/** Unique sentinel thrown to abort a superseded socket's bootstrap
 *  continuation WITHOUT scheduling a reconnect (the newer, healthy socket
 *  owns the reconnect path). Distinct from a real error so the `.catch` can
 *  no-op on it instead of reconnecting. */
export const STALE_ABORT = Symbol('stale-socket-abort');

/** True only for the STALE_ABORT sentinel (a real Error is never a stale
 *  abort). Used by the bootstrap `.catch` to distinguish "bail, don't
 *  reconnect" from "real bootstrap failure, do reconnect". */
export function isStaleAbort(err: unknown): boolean {
  return err === STALE_ABORT;
}

/** Whether a pending command's settlement should be APPLIED: only when the
 *  command was sent on the CURRENT socket generation. A late response from a
 *  superseded socket (pendingGen !== currentGen) must be DROPPED so it can
 *  neither resolve the dead bootstrap's promise nor settle a newer socket's
 *  pending. */
export function isCurrentPending(pendingGen: number, currentGen: number): boolean {
  return pendingGen === currentGen;
}

/** Whether a bootstrap failure should schedule a reconnect. A superseded
 *  socket (alive === false) must NOT reconnect on behalf of the newer, healthy
 *  socket, and an explicit STALE abort never reconnects. Only a LIVE socket's
 *  real bootstrap error (e.g. messages missing, state did not bind) reconnects. */
export function shouldScheduleReconnect(err: unknown, alive: boolean): boolean {
  if (isStaleAbort(err)) return false;
  if (!alive) return false;
  return true;
}

/** Cadence for the application `{type:"ping"}` probe. */
export const HEARTBEAT_PING_INTERVAL_MS = 5_000;
/** Maximum wait for a pong when no inbound frame has arrived after the ping. */
export const HEARTBEAT_PONG_DEADLINE_MS = 10_000;
/** Maximum age of the most recent inbound frame before the socket is stale. */
export const HEARTBEAT_TIMEOUT_MS = 60_000;

export const HEARTBEAT_PING_FRAME = '{"type":"ping"}';

/** Application heartbeat may retire a socket only while the page is visible.
 *  Hidden-tab timer throttling freezes lastMessageAt and can age an
 *  outstanding ping past the pong deadline while the browser still
 *  auto-pongs server RFC6455 pings. Death while hidden is not a liveness
 *  signal — wake probes an OPEN socket and reconnects only if it is gone. */
export function heartbeatMayRetire(pageVisible: boolean): boolean {
  return pageVisible;
}

/** Only frame silence, not command latency, may classify an open socket dead. */
export function shouldCloseForSilence(lastMessageAtMs: number, nowMs: number): boolean {
  return nowMs - lastMessageAtMs >= HEARTBEAT_TIMEOUT_MS;
}

/** An outstanding ping dies only when no inbound frame arrives before the deadline. */
export function shouldCloseForMissedPong(
  pingSentAtMs: number | null,
  nowMs: number,
  lastMessageAtMs: number,
): boolean {
  if (pingSentAtMs === null) return false;
  if (lastMessageAtMs >= pingSentAtMs) return false;
  return nowMs - pingSentAtMs >= HEARTBEAT_PONG_DEADLINE_MS;
}

export const TRANSPORT_READY_CONNECTING = 0;
export const TRANSPORT_READY_OPEN = 1;
export const TRANSPORT_READY_CLOSING = 2;
export const TRANSPORT_READY_CLOSED = 3;

export type HeartbeatPingAction = 'idle' | 'send' | 'retire';

export function heartbeatPingAction(readyState: number | null): HeartbeatPingAction {
  if (readyState === null) return 'idle';
  if (readyState === TRANSPORT_READY_OPEN) return 'send';
  if (readyState === TRANSPORT_READY_CLOSING || readyState === TRANSPORT_READY_CLOSED) {
    return 'retire';
  }
  return 'idle';
}

export type HeartbeatPingPlan = 'idle' | 'send' | 'retire' | 'wait-pong';

export function planHeartbeatPing(
  readyState: number | null,
  pingSentAtMs: number | null,
  nowMs: number,
  lastMessageAtMs = 0,
): HeartbeatPingPlan {
  const ping = heartbeatPingAction(readyState);
  if (ping === 'idle') return 'idle';
  if (ping === 'retire') return 'retire';
  if (pingSentAtMs !== null) {
    if (lastMessageAtMs >= pingSentAtMs) return 'send';
    if (shouldCloseForMissedPong(pingSentAtMs, nowMs, lastMessageAtMs)) return 'retire';
    return 'wait-pong';
  }
  return 'send';
}

export const TRANSPORT_HANDSHAKE_DEADLINE_MS = 10_000;

export function shouldRetireHandshake(
  readyState: number | null,
  startedAtMs: number | null,
  nowMs: number,
): boolean {
  if (readyState === null || startedAtMs === null) return false;
  if (readyState === TRANSPORT_READY_OPEN) return false;
  return nowMs - startedAtMs >= TRANSPORT_HANDSHAKE_DEADLINE_MS;
}

/** `{type:"pong"}` is one inbound live signal; any post-ping frame is equivalent. */
export function isPongFrame(raw: string): boolean {
  if (raw === '{"type":"pong"}') return true;
  try {
    const frame = JSON.parse(raw) as { type?: unknown };
    return frame != null && frame.type === 'pong';
  } catch {
    return false;
  }
}


/** The four event-handler slots a transport (WebSocket / RTCDataChannel)
 *  exposes. Generic so the helper has no DOM type dependency and the node test
 *  can pass a plain mock. */
export interface TransportHandlers {
  onopen: unknown;
  onmessage: unknown;
  onerror: unknown;
  onclose: unknown;
}

/** Null the four event-handler slots of a transport being replaced/closed, so
 *  a CONNECTING old socket's late `onopen` (or a late `onmessage`/`onerror`/
 *  `onclose`) can never fire against the NEW generation / wsRef — e.g. a late
 *  old `onopen` would otherwise run `onOpen` against the new socket's
 *  generation and bootstrap a second time. Called by connect() before closing
 *  the old socket and by the unmount cleanup before closing the main socket. */
export function detachTransportHandlers<T extends TransportHandlers>(t: T): void {
  t.onopen = null;
  t.onmessage = null;
  t.onerror = null;
  t.onclose = null;
}

/* ------------------------------------------------------------------ *
 * Bounded ready gate (mount-before-WebSocket-OPEN fix)
 * ------------------------------------------------------------------ */

/** Bounded wait for an auto/background load that fires before the connection
 *  has completed its authoritative bootstrap. The sidebar and session panel
 *  both `load()` on mount; raw WebSocket OPEN is too early because the server
 *  grants one sessionId-less `get_state` bootstrap command, then requires a
 *  sessionId. App therefore calls `markReady` only after state/messages bind
 *  `sessionIdRef`, preventing a racing session_list from surfacing a persistent
 *  `sessionId is required after WebSocket bootstrap` error.
 *
 *  Generation safety: App calls `markNotReady` before replacing a transport,
 *  and only the current socket's successful bootstrap continuation calls
 *  `markReady`. Superseded continuations fail their `alive()` checks, so a
 *  stale socket cannot release a waiter. `sendCommand` still re-checks the
 *  live transport at send time.
 *
 *  Bounded: each waiter arms a `READY_GATE_TIMEOUT_MS` timer so a permanently
 *  dead transport/bootstrap rejects; auto-load callers swallow that transient
 *  error and retry on the next poll/reconnect. Active actions bypass the gate
 *  and fail fast while disconnected. */
export const READY_GATE_TIMEOUT_MS = 15000;

/** A scheduled timer handle as returned by the injected scheduler (the App
 *  passes `window.setTimeout`/`window.clearTimeout`; the node test passes a
 *  fake). Opaque to the gate. */
export type ReadyGateTimer = unknown;

/** Scheduler injected into `ReadyGate` so the pure class has no DOM/timer
 *  dependency and the node-runnable unit test can drive time deterministically. */
export interface ReadyGateScheduler {
  setTimeout: (fn: () => void, ms: number) => ReadyGateTimer;
  clearTimeout: (timer: ReadyGateTimer) => void;
}

/** Bounded, generation-safe ready gate for auto/background `sendCommand` loads.
 *  The gate itself owns authoritative-bootstrap readiness: raw transport OPEN
 *  never makes `wait()` succeed. No DOM/WebSocket dependency — the App wires
 *  the real scheduler; the node test passes a fake. */
export class ReadyGate {
  private isReady = false;
  private waiters: Array<{
    resolve: () => void;
    reject: (err: Error) => void;
    timer: ReadyGateTimer;
  }> = [];

  constructor(private readonly scheduler: ReadyGateScheduler) {}

  /** Number of waiters currently pending (test hook). */
  size(): number {
    return this.waiters.length;
  }

  /** Resolve immediately only after authoritative bootstrap; otherwise
   *  register a bounded waiter for the current or replacement connection. */
  wait(): Promise<void> {
    if (this.isReady) return Promise.resolve();
    const { promise, resolve, reject } = withResolvers<void>();
    const entry = { resolve, reject, timer: null as ReadyGateTimer };
    entry.timer = this.scheduler.setTimeout(() => {
      this.waiters = this.waiters.filter((w) => w !== entry);
      reject(new Error('not connected'));
    }, READY_GATE_TIMEOUT_MS);
    this.waiters.push(entry);
    return promise;
  }

  /** Mark the transport not authoritative-ready without dropping waiters that
   *  may migrate to the replacement connection's successful bootstrap. */
  markNotReady(): void {
    this.isReady = false;
  }

  /** Mark the current connection authoritative-ready and resolve every waiter. */
  markReady(): void {
    this.isReady = true;
    const pending = this.waiters;
    this.waiters = [];
    for (const w of pending) {
      this.scheduler.clearTimeout(w.timer);
      w.resolve();
    }
  }

  /** Reject every pending waiter with `reason`, clear their timers, and reset
   *  readiness. Called on unmount so a torn-down app cannot settle later. */
  clear(reason = 'not connected'): void {
    this.isReady = false;
    const pending = this.waiters;
    this.waiters = [];
    for (const w of pending) {
      this.scheduler.clearTimeout(w.timer);
      w.reject(new Error(reason));
    }
  }
}
