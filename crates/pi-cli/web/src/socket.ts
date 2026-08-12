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

/** Bounded wait for an auto/background load that fires before the socket has
 *  reached OPEN — the sidebar and session panel both `load()` on mount, so on
 *  a fresh page the effect runs while `connect()` is still in CONNECTING and
 *  `sendCommand` would reject with `not connected`, surfacing a persistent
 *  `load failed: not connected` error. The gate lets that auto-load WAIT for
 *  the current socket's open (bounded, so a long outage never hangs) instead
 *  of failing immediately.
 *
 *  Generation safety: `notifyOpen` is only ever called from the App's `onOpen`
 *  handler, which `connect()`/unmount detach from any superseded socket first
 *  (detachTransportHandlers), so a STALE socket's open can never resolve a
 *  waiter. A waiter resolved on the newer socket's open proceeds to
 *  `sendCommand`, which re-checks `wsRef.readyState === OPEN` at send time, so
 *  even a defensive late open cannot trigger a load on a stale transport. The
 *  gate itself is therefore generation-agnostic — the invariant is enforced by
 *  the existing detach + readyState guards, not by tracking a gen per waiter.
 *
 *  Bounded: each waiter arms a `READY_GATE_TIMEOUT_MS` timer so a permanently
 *  dead transport rejects (and the auto-load's catch swallows it silently —
 *  the next poll/reconnect re-arms). No unbounded wait, no permanent error.
 *
 *  Active actions (sidebar New / Switch, panel rename) do NOT use the gate —
 *  they call `sendCommand` directly and fail fast with `not connected` while
 *  disconnected, exactly as before. */
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
 *  No DOM/WebSocket dependency — the App wires the real scheduler; the node
 *  test passes a fake. See `READY_GATE_TIMEOUT_MS` doc for the full contract. */
export class ReadyGate {
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

  /** Register a bounded waiter. The caller MUST first check `ws.readyState ===
   *  OPEN` and only call `wait()` when the socket is NOT open; an already-open
   *  socket never registers a waiter. Resolves on the next `notifyOpen`, or
   *  rejects with `not connected` after `READY_GATE_TIMEOUT_MS`. */
  wait(): Promise<void> {
    const { promise, resolve, reject } = Promise.withResolvers<void>();
    const entry = { resolve, reject, timer: null as ReadyGateTimer };
    entry.timer = this.scheduler.setTimeout(() => {
      this.waiters = this.waiters.filter((w) => w !== entry);
      reject(new Error('not connected'));
    }, READY_GATE_TIMEOUT_MS);
    this.waiters.push(entry);
    return promise;
  }

  /** Resolve every pending waiter. Called from the App's `onOpen` (the current
   *  socket's open only — superseded sockets' `onopen` is detached first, so a
   *  stale socket can never trigger a load through the gate). */
  notifyOpen(): void {
    const pending = this.waiters;
    this.waiters = [];
    for (const w of pending) {
      this.scheduler.clearTimeout(w.timer);
      w.resolve();
    }
  }

  /** Reject every pending waiter with `reason` and clear their timers. Called
   *  on unmount so a torn-down app does not leak waiter timers or settle a
   *  promise into an unmounted component. */
  clear(reason = 'not connected'): void {
    const pending = this.waiters;
    this.waiters = [];
    for (const w of pending) {
      this.scheduler.clearTimeout(w.timer);
      w.reject(new Error(reason));
    }
  }
}
