import { AppState, type AppStateStatus } from 'react-native';
import {
  classifyResponseOwnership,
  commandTimeoutMs,
  createClientNamespace,
  isRpcResponse,
  nextCommandId,
  parseRunnerState,
  RESPONSE_OWNERSHIP_REJECTIONS,
  timeoutErrorMessage,
  type RpcResponse,
  type RunnerCommand,
  type RunnerEndpoint,
  type RunnerStateData,
  type RuntimeIdentity,
} from './protocol';
import {
  HEARTBEAT_PING_FRAME,
  HEARTBEAT_PING_INTERVAL_MS,
  HEARTBEAT_TIMEOUT_MS,
  nextReconnectDelay,
  planHeartbeatPing,
  readFrameSequence,
  readFrameType,
  ReadyGate,
  RUNNER_RECONNECT_INITIAL_MS,
  runtimeIdentityKey,
  shouldResumeFromBackground,
  type ReadyGateScheduler,
  type TransportTimerHandle,
} from './reconnect';
import {
  RunnerDisconnectedError,
  parseTransportFailure,
  transportFailureMessage,
  type TransportFailureKind,
} from './transportError';

const AUTH_PROTOCOL_PREFIX = 'rpi-auth.';

const TRANSPORT_TIMERS: ReadyGateScheduler = {
  setTimeout: (fn, ms) => setTimeout(fn, ms),
  clearTimeout: (timer) => clearTimeout(timer),
};

interface PendingCommand {
  resolve: (response: RpcResponse) => void;
  reject: (error: Error) => void;
  timeout: TransportTimerHandle;
  /** Wire command name, for the timeout message and the ownership gate. */
  type: string;
  /** Session the command was sent for ('' when the runner has no bound session). */
  sessionId: string;
  /** Socket generation this command was sent on; a response from a superseded
   *  generation is dropped, never settled. */
  generation: number;
}

export type RunnerConnectionState = 'connecting' | 'connected' | 'reconnecting' | 'token_required' | 'offline';

export interface RunnerProbe {
  sessionId: string | null;
  runtimeIncarnation: string | null;
  cwd: string;
}

export class RunnerClient {
  private socket: WebSocket | null = null;
  /** Monotonic command sequence, namespaced by `clientNamespace` so ids can
   *  never collide with the runner's retained command ledger after a reload. */
  private sequence = 0;
  private readonly clientNamespace = createClientNamespace();
  private generation = 0;
  private stopped = false;
  private reconnectDelay = RUNNER_RECONNECT_INITIAL_MS;
  private reconnectTimer: TransportTimerHandle | null = null;
  private heartbeatTimer: TransportTimerHandle | null = null;
  private pingSentAt: number | null = null;
  private lastFrameAt = 0;
  private readonly gate = new ReadyGate(TRANSPORT_TIMERS);
  private readonly pending = new Map<string, PendingCommand>();
  private readonly subscriptions = new Map<string, RuntimeIdentity>();
  private readonly sessionCursors = new Map<string, number>();
  private readonly reconnectListeners = new Set<() => void>();
  private appStateSub: { remove: () => void } | null = null;
  state: RunnerConnectionState = 'connecting';
  errorKind: TransportFailureKind | null = null;
  probe: RunnerProbe | null = null;
  error = '';
  private readonly onState: (state: RunnerConnectionState, error: string) => void;
  private readonly frameListeners = new Set<(frame: unknown) => void>();

  constructor(
    private readonly endpoint: RunnerEndpoint,
    private readonly token: string,
    onState: (state: RunnerConnectionState, error: string) => void,
  ) {
    this.onState = onState;
    this.appStateSub = AppState.addEventListener('change', (next: AppStateStatus) => {
      if (next === 'active') this.resumeFromBackground();
    });
  }

  hasOpenSocket(): boolean {
    return this.socket !== null && this.socket.readyState === WebSocket.OPEN;
  }

  resumeFromBackground(): void {
    if (this.stopped) return;
    const readyState = this.socket ? this.socket.readyState : null;
    if (!shouldResumeFromBackground(readyState)) return;
    this.reconnectDelay = RUNNER_RECONNECT_INITIAL_MS;
    this.clearReconnectTimer();
    this.connect();
  }

  connect(): void {
    if (this.stopped) return;
    this.generation += 1;
    const generation = this.generation;
    // Every pending command is retired with the socket it was sent on: their
    // responses can never arrive, so they must reject now instead of waiting
    // out their bounded timeout. No command can carry the new generation yet,
    // so this drains exactly the superseded set; the generation guard in
    // settle() stays as the backstop for any entry that outlives its socket.
    this.rejectPending();
    // Raw transport OPEN is not readiness: queued commands must wait for this
    // socket's authoritative bootstrap (or the gate's bounded timeout).
    this.gate.markNotReady();
    this.dropSocket();
    this.setState(this.probe ? 'reconnecting' : 'connecting', '');
    const protocols = this.token ? [`${AUTH_PROTOCOL_PREFIX}${this.token}`] : [];
    let socket: WebSocket;
    try {
      socket = new WebSocket(this.endpoint.webSocketUrl, protocols);
    } catch (error) {
      this.fail(error instanceof Error ? error.message : String(error), generation);
      return;
    }
    this.socket = socket;
    socket.onopen = () => {
      if (generation !== this.generation) return;
      this.lastFrameAt = Date.now();
      this.pingSentAt = null;
      this.startHeartbeat();
      void this.bootstrap(generation);
    };
    socket.onmessage = (event) => {
      if (generation !== this.generation) return;
      // ANY inbound frame — pong or not — answers an outstanding ping and
      // slides the silence window: only real silence retires the socket.
      this.lastFrameAt = Date.now();
      this.pingSentAt = null;
      if (typeof event.data !== 'string') return;
      let frame: unknown;
      try {
        frame = JSON.parse(event.data);
      } catch {
        return;
      }
      this.trackCursor(frame);
      for (const listener of this.frameListeners) listener(frame);
      if (isRpcResponse(frame)) this.settle(frame);
    };
    socket.onclose = (event) => {
      if (generation !== this.generation) return;
      if (this.stopped) return;
      this.fail(event.reason || 'Runner connection closed.', generation);
    };
    socket.onerror = () => {
      if (generation !== this.generation) return;
      this.fail('Runner connection failed.', generation);
    };
  }

  /** Application heartbeat: ping on the cadence, retire the socket only when
   *  real silence (or an unanswered ping) proves the peer is gone. Command
   *  latency is never a liveness signal. */
  private startHeartbeat(): void {
    if (this.heartbeatTimer !== null) return;
    this.heartbeatTimer = setInterval(() => this.heartbeatTick(), HEARTBEAT_PING_INTERVAL_MS);
  }

  private stopHeartbeat(): void {
    if (this.heartbeatTimer === null) return;
    clearInterval(this.heartbeatTimer);
    this.heartbeatTimer = null;
    this.pingSentAt = null;
  }

  private heartbeatTick(): void {
    const now = Date.now();
    if (now - this.lastFrameAt >= HEARTBEAT_TIMEOUT_MS) {
      this.fail('Runner connection is silent.', this.generation);
      return;
    }
    const socket = this.socket;
    const plan = planHeartbeatPing(socket ? socket.readyState : null, this.pingSentAt, now, this.lastFrameAt);
    if (plan === 'idle' || plan === 'wait-pong') return;
    if (plan === 'retire' || !socket) {
      this.fail('Runner connection is unresponsive.', this.generation);
      return;
    }
    try {
      socket.send(HEARTBEAT_PING_FRAME);
    } catch {
      this.fail('Runner connection failed.', this.generation);
      return;
    }
    this.pingSentAt = now;
  }

  private async bootstrap(generation: number): Promise<void> {
    try {
      const response = await this.dispatch({ type: 'get_state' }, '');
      if (generation !== this.generation) return;
      const parsed: RunnerStateData | null = parseRunnerState(response.data);
      if (!response.success || !parsed) throw new Error('Runner state is unavailable.');
      this.probe = { sessionId: parsed.sessionId, runtimeIncarnation: parsed.runtimeIncarnation, cwd: parsed.cwd };
      // Backoff resets only after the connection is provably stable: an
      // authoritative bootstrap, never a bare socket OPEN.
      this.reconnectDelay = RUNNER_RECONNECT_INITIAL_MS;
      this.setState('connected', '');
      this.gate.markReady();
      this.resubscribe();
      for (const listener of this.reconnectListeners) listener();
    } catch (error) {
      if (generation !== this.generation) return;
      // A socket that is no longer OPEN already failed through its own
      // close/error path (an auth rejection included): this continuation must
      // not overwrite that verdict — e.g. downgrade token_required into a
      // reconnect — because the transport it bootstrapped is already retired.
      if (!this.hasOpenSocket()) return;
      // `fail` owns every close-out: it rejects pending commands, retires the
      // socket (stopping the heartbeat), and on an auth failure clears the
      // ready waiters and enters token_required instead of reconnecting.
      this.fail(error instanceof Error ? error.message : String(error), generation);
    }
  }

  private fail(message: string, generation: number): void {
    if (this.stopped || generation !== this.generation) return;
    const kind = parseTransportFailure(message);
    const display = transportFailureMessage(kind, message);
    this.rejectPending();
    this.dropSocket();
    if (kind === 'auth') {
      // A rejected token cannot become ready without user action: queued
      // commands must fail fast instead of waiting out the gate timeout.
      this.gate.clear(new RunnerDisconnectedError());
      this.setState('token_required', display);
      return;
    }
    this.setState('reconnecting', display);
    this.scheduleReconnect();
  }

  private scheduleReconnect(): void {
    if (this.stopped || this.reconnectTimer !== null) return;
    const delay = this.reconnectDelay;
    this.reconnectDelay = nextReconnectDelay(delay);
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      if (this.stopped) return;
      this.connect();
    }, delay);
  }

  private clearReconnectTimer(): void {
    if (this.reconnectTimer === null) return;
    clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
  }

  private rejectPending(): void {
    for (const [, pending] of this.pending) {
      clearTimeout(pending.timeout);
      pending.reject(new RunnerDisconnectedError());
    }
    this.pending.clear();
  }

  private dropSocket(): void {
    this.stopHeartbeat();
    const socket = this.socket;
    this.socket = null;
    if (!socket) return;
    socket.onopen = null;
    socket.onmessage = null;
    socket.onclose = null;
    socket.onerror = null;
    try {
      socket.close();
    } catch {
      return;
    }
  }

  private setState(state: RunnerConnectionState, error: string): void {
    this.state = state;
    this.error = error;
    this.errorKind = error ? parseTransportFailure(error) : null;
    if (state === 'connected') this.errorKind = null;
    this.onState(state, error);
  }

  private trackCursor(frame: unknown): void {
    const type = readFrameType(frame);
    const rec = frame && typeof frame === 'object' ? frame as Record<string, unknown> : {};
    const sessionId = typeof rec.sessionId === 'string' ? rec.sessionId : '';
    const runtimeIncarnation = typeof rec.runtimeIncarnation === 'string' ? rec.runtimeIncarnation : '';
    if (!sessionId || !runtimeIncarnation) return;
    const key = runtimeIdentityKey(sessionId, runtimeIncarnation);
    if (type === 'session_resync_required') {
      this.sessionCursors.set(key, 0);
      return;
    }
    const sequence = readFrameSequence(frame);
    if (sequence === null) return;
    const current = this.sessionCursors.get(key) ?? 0;
    if (sequence > current) this.sessionCursors.set(key, sequence);
  }

  private resubscribe(): void {
    for (const identity of this.subscriptions.values()) {
      this.sendSubscribe(identity);
    }
  }

  cursorFor(identity: RuntimeIdentity): number {
    return this.sessionCursors.get(runtimeIdentityKey(identity.sessionId, identity.runtimeIncarnation)) ?? 0;
  }

  /** Admit a command once the socket's authoritative bootstrap has settled: a
   *  command sent mid-bootstrap would otherwise race the first snapshot. */
  command(command: RunnerCommand, sourceSessionId?: string): Promise<RpcResponse> {
    if (!this.hasOpenSocket()) return Promise.reject(new RunnerDisconnectedError());
    return this.gate.wait().then(() => this.dispatch(command, sourceSessionId));
  }

  private dispatch(command: RunnerCommand, sourceSessionId?: string): Promise<RpcResponse> {
    const socket = this.socket;
    if (!socket || socket.readyState !== WebSocket.OPEN) {
      return Promise.reject(new RunnerDisconnectedError());
    }
    const generation = this.generation;
    const id = nextCommandId(this.clientNamespace, ++this.sequence);
    const sessionId = (sourceSessionId ?? this.probe?.sessionId ?? '').trim();
    const frame = sessionId ? { ...command, id, sessionId } : { ...command, id };
    const payload = JSON.stringify(frame);
    const timeoutMs = commandTimeoutMs(command.type);
    return new Promise<RpcResponse>((resolve, reject) => {
      const sentAt = Date.now();
      const timeout = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(timeoutErrorMessage(command.type, sentAt, Date.now())));
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, timeout, type: command.type, sessionId, generation });
      try {
        socket.send(payload);
      } catch (error) {
        clearTimeout(timeout);
        this.pending.delete(id);
        reject(error instanceof Error ? error : new Error(String(error)));
      }
    });
  }

  private settle(response: RpcResponse): void {
    const id = response.id;
    if (!id) return;
    const pending = this.pending.get(id);
    if (!pending) return;
    // A response for a command sent on a superseded socket generation is
    // DROPPED: it can neither settle a newer socket's pending command nor
    // consume this entry, which stays bounded by its own timeout.
    if (pending.generation !== this.generation) return;
    // Ownership gate: a frame that does not NAME the pending command, or that
    // answers on a session the command was not sent for, is rejected once.
    const ownership = classifyResponseOwnership(response, pending);
    this.pending.delete(id);
    clearTimeout(pending.timeout);
    if (ownership !== 'accept') {
      pending.reject(new Error(RESPONSE_OWNERSHIP_REJECTIONS[ownership]));
      return;
    }
    pending.resolve(response);
  }

  subscribe(identity: RuntimeIdentity, cursor?: number): void {
    const key = runtimeIdentityKey(identity.sessionId, identity.runtimeIncarnation);
    this.subscriptions.set(key, identity);
    if (cursor !== undefined) this.sessionCursors.set(key, cursor);
    this.sendSubscribe(identity);
  }

  private sendSubscribe(identity: RuntimeIdentity): void {
    const socket = this.socket;
    if (!socket || socket.readyState !== WebSocket.OPEN) return;
    const key = runtimeIdentityKey(identity.sessionId, identity.runtimeIncarnation);
    const cursor = this.sessionCursors.get(key) ?? 0;
    socket.send(JSON.stringify({
      type: 'subscribe',
      sessionId: identity.sessionId,
      runtimeIncarnation: identity.runtimeIncarnation,
      cursor,
    }));
  }

  close(): void {
    this.stopped = true;
    this.appStateSub?.remove();
    this.appStateSub = null;
    this.clearReconnectTimer();
    this.generation += 1;
    this.rejectPending();
    // A closed client can never become ready again: queued commands fail now.
    this.gate.clear(new RunnerDisconnectedError());
    this.dropSocket();
    this.setState('offline', '');
  }

  subscribeFrames(listener: (frame: unknown) => void): () => void {
    this.frameListeners.add(listener);
    return () => {
      this.frameListeners.delete(listener);
    };
  }

  subscribeReconnect(listener: () => void): () => void {
    this.reconnectListeners.add(listener);
    return () => {
      this.reconnectListeners.delete(listener);
    };
  }
}
