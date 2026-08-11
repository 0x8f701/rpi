/**
 * Socket-generation pure helpers for the reconnect state machine — shared by
 * App.tsx's connect/onOpen/onResponse/sendCommand flow and the node-runnable
 * regression test (scripts/socket.test.ts). No DOM/browser/WebSocket
 * dependency so `esbuild --platform=node` can bundle the test in isolation.
 *
 * Problem this encodes: a socket (A) superseded mid-bootstrap (dropped, B
 * reconnected healthy) used to leave its pending commands in the global map
 * until their 30s timeout; the timeout/late response then continued A's dead
 * bootstrap (applyState / setConnState / refreshGoal) or scheduled a reconnect
 * that replaced the healthy B. The fix ties every pending command and every
 * bootstrap continuation to the socket GENERATION it was sent on, so a stale
 * socket can neither settle a newer socket's pending nor reconnect on its
 * behalf.
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