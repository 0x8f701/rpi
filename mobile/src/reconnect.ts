export const RUNNER_RECONNECT_INITIAL_MS = 500;
export const RUNNER_RECONNECT_MAX_MS = 15_000;

export function shouldResumeFromBackground(readyState: number | null): boolean {
  return readyState !== WebSocket.OPEN;
}

export function nextReconnectDelay(current: number): number {
  return Math.min(current * 2, RUNNER_RECONNECT_MAX_MS);
}

export function runtimeIdentityKey(sessionId: string, runtimeIncarnation: string): string {
  return `${sessionId}:${runtimeIncarnation}`;
}

export function readFrameSequence(frame: unknown): number | null {
  if (!frame || typeof frame !== 'object') return null;
  const value = frame as Record<string, unknown>;
  if (typeof value.sequence !== 'number' || !Number.isFinite(value.sequence)) return null;
  return value.sequence;
}

export function readFrameType(frame: unknown): string {
  if (!frame || typeof frame !== 'object') return '';
  const value = frame as Record<string, unknown>;
  return typeof value.type === 'string' ? value.type : '';
}

// ---- Socket liveness + readiness, ported from crates/pi-cli/web/src/socket.ts ----

/** Cadence for the application `{type:"ping"}` probe. */
export const HEARTBEAT_PING_INTERVAL_MS = 5_000;
/** Maximum wait for inbound traffic after a ping before the socket is stale. */
export const HEARTBEAT_PONG_DEADLINE_MS = 10_000;
/** Maximum age of the most recent inbound frame before the socket is silent. */
export const HEARTBEAT_TIMEOUT_MS = 60_000;

export const HEARTBEAT_PING_FRAME = '{"type":"ping"}';

/** ReadyState values every transport (WebSocket / RTCDataChannel) shares. */
const TRANSPORT_READY_CONNECTING = 0;
const TRANSPORT_READY_OPEN = 1;
const TRANSPORT_READY_CLOSING = 2;
const TRANSPORT_READY_CLOSED = 3;

export type HeartbeatPingPlan = 'idle' | 'send' | 'retire' | 'wait-pong';

/** What the heartbeat tick does next. Command latency is never a liveness
 *  signal: only an outstanding ping with no inbound traffic behind it retires
 *  the socket, and ANY inbound frame (pong or not) counts as the answer. */
export function planHeartbeatPing(
  readyState: number | null,
  pingSentAtMs: number | null,
  nowMs: number,
  lastMessageAtMs = 0,
): HeartbeatPingPlan {
  if (readyState === null || readyState === TRANSPORT_READY_CONNECTING) return 'idle';
  if (readyState === TRANSPORT_READY_CLOSING || readyState === TRANSPORT_READY_CLOSED) return 'retire';
  if (pingSentAtMs === null) return 'send';
  if (lastMessageAtMs >= pingSentAtMs) return 'send';
  if (nowMs - pingSentAtMs >= HEARTBEAT_PONG_DEADLINE_MS) return 'retire';
  return 'wait-pong';
}

/** Bounded wait for an auto/background command that races the connection's
 *  authoritative bootstrap. Raw transport OPEN is not readiness: the runner
 *  grants one session-less `get_state` bootstrap and then requires a bound
 *  session, so a command admitted mid-bootstrap would fail against the first
 *  snapshot. Only the current socket's successful bootstrap releases waiters. */
export const READY_GATE_TIMEOUT_MS = 15_000;

/** Timer handle returned by the host scheduler (a number in React Native, a
 *  Timeout object under Node/Jest). */
export type TransportTimerHandle = ReturnType<typeof setTimeout>;

/** Scheduler injected so the gate has no timer dependency and tests drive it. */
export interface ReadyGateScheduler {
  setTimeout: (fn: () => void, ms: number) => TransportTimerHandle;
  clearTimeout: (timer: TransportTimerHandle) => void;
}

export class ReadyGate {
  private ready = false;
  private waiters: Array<{
    resolve: () => void;
    reject: (error: Error) => void;
    timer: TransportTimerHandle | null;
  }> = [];

  constructor(private readonly scheduler: ReadyGateScheduler) {}

  /** Number of queued waiters (test hook). */
  size(): number {
    return this.waiters.length;
  }

  /** Resolve immediately once authoritative bootstrap has bound the runtime;
   *  otherwise queue one bounded waiter for this or the replacement socket. */
  wait(): Promise<void> {
    if (this.ready) return Promise.resolve();
    return new Promise<void>((resolve, reject) => {
      const waiter: { resolve: () => void; reject: (error: Error) => void; timer: TransportTimerHandle | null } = {
        resolve,
        reject,
        timer: null,
      };
      waiter.timer = this.scheduler.setTimeout(() => {
        this.waiters = this.waiters.filter((queued) => queued !== waiter);
        reject(new Error('not connected'));
      }, READY_GATE_TIMEOUT_MS);
      this.waiters.push(waiter);
    });
  }

  /** Mark the transport not authoritative-ready WITHOUT dropping queued
   *  waiters, which migrate to the replacement socket's successful bootstrap. */
  markNotReady(): void {
    this.ready = false;
  }

  /** Mark the current socket authoritative-ready and release every waiter. */
  markReady(): void {
    this.ready = true;
    const queued = this.waiters;
    this.waiters = [];
    for (const waiter of queued) {
      this.clearWaiterTimer(waiter.timer);
      waiter.resolve();
    }
  }

  /** Reject every queued waiter with the given failure — the transport's typed
   *  disconnected error when its socket is gone — clear their timers, and reset
   *  readiness. */
  clear(error: Error = new Error('not connected')): void {
    this.ready = false;
    const queued = this.waiters;
    this.waiters = [];
    for (const waiter of queued) {
      this.clearWaiterTimer(waiter.timer);
      waiter.reject(error);
    }
  }

  private clearWaiterTimer(timer: TransportTimerHandle | null): void {
    if (timer === null) return;
    this.scheduler.clearTimeout(timer);
  }
}
