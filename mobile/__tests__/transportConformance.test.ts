// Transport conformance with the /web RPC contract
// (crates/pi-cli/web/src/{protocol,pending,socket}.ts): reload-safe command ids,
// per-command timeout classes, the response-ownership gate (command name +
// socket generation + session), the bounded ready gate, and the heartbeat that
// treats ANY inbound frame as liveness and retires only on real silence.
import {
  commandTimeoutMs,
  createClientNamespace,
  DEFAULT_COMMAND_TIMEOUT_MS,
  LONG_OP_TIMEOUT_MS,
  nextCommandId,
  RESPONSE_OWNERSHIP_REJECTIONS,
  timeoutErrorMessage,
} from '../src/protocol';
import {
  HEARTBEAT_PING_FRAME,
  HEARTBEAT_PING_INTERVAL_MS,
  HEARTBEAT_PONG_DEADLINE_MS,
  HEARTBEAT_TIMEOUT_MS,
  planHeartbeatPing,
  READY_GATE_TIMEOUT_MS,
  ReadyGate,
  RUNNER_RECONNECT_INITIAL_MS,
  type ReadyGateScheduler,
  type TransportTimerHandle,
} from '../src/reconnect';
import { RunnerClient } from '../src/runnerClient';

const ENDPOINT = { origin: 'http://runner.local', authority: 'runner.local', webSocketUrl: 'ws://runner.local/ws' };

/** Stand-in for the React Native WebSocket global: records outbound frames and
 *  lets the test drive the handshake, inbound frames, and closure. */
class FakeSocket {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;
  static instances: FakeSocket[] = [];

  readyState: number = FakeSocket.CONNECTING;
  readonly sent: string[] = [];
  /** When set, the next `send` throws this message (a transport-level failure
   *  surfacing while a command is in flight). */
  failNextSend: string | null = null;
  onopen: (() => void) | null = null;
  onmessage: ((event: { data: string }) => void) | null = null;
  onclose: ((event: { code: number; reason: string }) => void) | null = null;
  onerror: (() => void) | null = null;

  constructor(readonly url: string, readonly protocols: string[] = []) {
    FakeSocket.instances.push(this);
  }

  send(payload: string): void {
    if (this.failNextSend !== null) {
      const message = this.failNextSend;
      this.failNextSend = null;
      throw new Error(message);
    }
    this.sent.push(payload);
  }

  close(): void {
    this.readyState = FakeSocket.CLOSED;
  }

  /** Handshake completes: the client starts its heartbeat and bootstraps. */
  open(): void {
    this.readyState = FakeSocket.OPEN;
    this.onopen?.();
  }

  /** One inbound frame; every frame slides the silence window. */
  deliver(frame: unknown): void {
    this.onmessage?.({ data: JSON.stringify(frame) });
  }

  /** Parsed outbound command frames, ignoring the raw heartbeat ping. */
  frames(): Array<Record<string, unknown>> {
    return this.sent.filter((payload) => payload.startsWith('{')).map((payload) => JSON.parse(payload));
  }

  pings(): number {
    return this.sent.filter((payload) => payload === HEARTBEAT_PING_FRAME).length;
  }
}

const transportGlobal = globalThis as { WebSocket?: unknown };
let realWebSocket: unknown;

beforeAll(() => {
  realWebSocket = transportGlobal.WebSocket;
  transportGlobal.WebSocket = FakeSocket;
});

afterAll(() => {
  transportGlobal.WebSocket = realWebSocket;
});

beforeEach(() => {
  FakeSocket.instances = [];
  jest.useFakeTimers();
});

afterEach(() => {
  jest.useRealTimers();
});

/** Promise jobs here are microtask-only, so awaiting a chain suffices to let
 *  every `gate.wait()` / dispatch continuation run under fake timers. */
async function flushMicrotasks(rounds = 12): Promise<void> {
  for (let i = 0; i < rounds; i += 1) await Promise.resolve();
}

function newClient(): RunnerClient {
  return new RunnerClient(ENDPOINT, 'token', () => {});
}

function latestSocket(): FakeSocket {
  const socket = FakeSocket.instances[FakeSocket.instances.length - 1];
  if (!socket) throw new Error('the client opened no socket');
  return socket;
}

function response(command: string, id: string, data?: unknown, sessionId = 'session-1'): Record<string, unknown> {
  return { type: 'response', command, id, sessionId, success: true, data };
}

/** Connect a socket and complete its authoritative bootstrap (ready gate open). */
async function connectAndBootstrap(client: RunnerClient): Promise<FakeSocket> {
  client.connect();
  const socket = latestSocket();
  socket.open();
  const bootstrap = socket.frames()[0];
  socket.deliver(response('get_state', String(bootstrap.id), { sessionId: 'session-1', runtimeIncarnation: 'inc-1', cwd: '/workspace' }));
  await flushMicrotasks();
  return socket;
}

function commandFrame(socket: FakeSocket, type: string): Record<string, unknown> {
  const frame = socket.frames().filter((candidate) => candidate.type === type).pop();
  if (!frame) throw new Error(`no ${type} frame was sent`);
  return frame;
}

/** Advance the fake clock in chunks short enough that the heartbeat always sees
 *  an inbound frame first: this keeps the transport alive for a long wait.
 *  Promise settlement happens in microtasks, so drain them before asserting. */
async function advanceKeepingAlive(socket: FakeSocket, ms: number): Promise<void> {
  const step = 4_000;
  let remaining = ms;
  while (remaining > 0) {
    const chunk = Math.min(step, remaining);
    socket.deliver({ type: 'pong' });
    jest.advanceTimersByTime(chunk);
    remaining -= chunk;
  }
  await flushMicrotasks();
}

function outcomeOf(promise: Promise<unknown>): { state: () => 'pending' | 'resolved' | 'rejected'; error: () => Error | null } {
  let state: 'pending' | 'resolved' | 'rejected' = 'pending';
  let rejection: Error | null = null;
  promise.then(
    () => { state = 'resolved'; },
    (error: Error) => { state = 'rejected'; rejection = error; },
  );
  return { state: () => state, error: () => rejection };
}

describe('reload-safe command ids', () => {
  it('namespaces ids per client generation so a reload cannot reuse them', async () => {
    const namespace = createClientNamespace(() => Uint8Array.from([1, 2, 3, 4, 5, 6]));
    expect(namespace).toBe('010203040506');
    expect(nextCommandId(namespace, 1)).toBe(`c${namespace}-1`);
    expect(nextCommandId(namespace, 2)).not.toBe(nextCommandId(namespace, 1));
    const other = createClientNamespace(() => Uint8Array.from([9, 9, 9, 9, 9, 9]));
    expect(other).not.toBe(namespace);
    expect(nextCommandId(other, 1)).not.toBe(nextCommandId(namespace, 1));
  });

  it('stays monotonic within a generation and never collides across generations', async () => {
    const first = newClient();
    const second = newClient();
    const firstSocket = await connectAndBootstrap(first);
    const secondSocket = await connectAndBootstrap(second);
    // Sent, deliberately unanswered: only the wire ids matter here.
    void first.command({ type: 'get_state' });
    void first.command({ type: 'get_state' });
    void second.command({ type: 'get_state' });
    await flushMicrotasks();

    const ids = (socket: FakeSocket) => socket.frames().map((frame) => String(frame.id));
    const firstIds = ids(firstSocket);
    const secondIds = ids(secondSocket);
    expect(firstIds).toHaveLength(3);
    expect(secondIds).toHaveLength(2);
    for (const id of [...firstIds, ...secondIds]) {
      expect(id).toMatch(/^c[0-9a-z]{12}-\d+$/);
    }
    const sequenceOf = (id: string) => Number(id.slice(id.indexOf('-') + 1));
    expect(sequenceOf(firstIds[1])).toBe(sequenceOf(firstIds[0]) + 1);
    expect(sequenceOf(firstIds[2])).toBe(sequenceOf(firstIds[1]) + 1);
    expect(sequenceOf(secondIds[1])).toBe(sequenceOf(secondIds[0]) + 1);
    // The reload case: a fresh client generation restarts its counter, and the
    // namespace keeps seq 1 from colliding with the retained ledger entry.
    expect(firstIds[0]).not.toBe(secondIds[0]);
    expect(firstIds[0].split('-')[0]).not.toBe(secondIds[0].split('-')[0]);
  });
});

describe('per-command timeout classes', () => {
  it('gives long operations the long bound and unknown types the fast-ack bound', () => {
    expect(DEFAULT_COMMAND_TIMEOUT_MS).toBe(30_000);
    expect(LONG_OP_TIMEOUT_MS).toBe(600_000);
    for (const type of ['compact', 'snapcompact', 'get_messages', 'code_review_snapshot', 'new_session']) {
      expect(commandTimeoutMs(type)).toBe(LONG_OP_TIMEOUT_MS);
    }
    expect(commandTimeoutMs('prompt')).toBe(DEFAULT_COMMAND_TIMEOUT_MS);
    // A command type nobody has classified yet must NOT inherit the long bound.
    expect(commandTimeoutMs('future_streaming_command')).toBe(DEFAULT_COMMAND_TIMEOUT_MS);
    expect(timeoutErrorMessage('compact', 0, 300_500)).toBe('compact timed out after 301s (no response from server)');
  });

  it('arms the class-specific bound on the wire', async () => {
    const client = newClient();
    const socket = await connectAndBootstrap(client);
    const compact = outcomeOf(client.command({ type: 'compact' }));
    const prompt = outcomeOf(client.command({ type: 'prompt', message: 'hi' }));
    await flushMicrotasks();

    await advanceKeepingAlive(socket, DEFAULT_COMMAND_TIMEOUT_MS + 1);
    expect(prompt.state()).toBe('rejected');
    expect(prompt.error()?.message).toMatch(/^prompt timed out after \d+s \(no response from server\)$/);
    // The long operation is still bounded but alive past the fast-ack window.
    expect(compact.state()).toBe('pending');

    await advanceKeepingAlive(socket, LONG_OP_TIMEOUT_MS - DEFAULT_COMMAND_TIMEOUT_MS - 1);
    expect(compact.state()).toBe('rejected');
    expect(compact.error()?.message).toBe('compact timed out after 600s (no response from server)');
  });
});

describe('superseded socket generation', () => {
  it('retires pending commands immediately and ignores their late responses', async () => {
    const client = newClient();
    const first = await connectAndBootstrap(client);
    const retired = outcomeOf(client.command({ type: 'compact' }));
    await flushMicrotasks();
    const retiredFrame = commandFrame(first, 'compact');
    expect(retired.state()).toBe('pending');

    // The replacement generation retires the command at once: a 600s long-op
    // bound must not be waited out by a command whose socket is gone.
    const second = await connectAndBootstrap(client);
    expect(retired.state()).toBe('rejected');
    expect(retired.error()?.message).toBe('Runner is not connected.');

    // A late frame carrying the retired id is dropped by the generation guard:
    // it can neither settle nor re-reject the retired command.
    second.deliver(response('compact', String(retiredFrame.id), {}));
    await flushMicrotasks();
    expect(retired.state()).toBe('rejected');
    expect(retired.error()?.message).toBe('Runner is not connected.');

    // The live generation is unaffected.
    const live = outcomeOf(client.command({ type: 'compact' }));
    await flushMicrotasks();
    expect(live.state()).toBe('pending');
    const liveFrame = commandFrame(second, 'compact');
    second.deliver(response('compact', String(liveFrame.id), {}));
    await flushMicrotasks();
    expect(live.state()).toBe('resolved');
  });
});

describe('response ownership gate', () => {
  it('settles a command answered by its own socket generation', async () => {
    const client = newClient();
    const socket = await connectAndBootstrap(client);
    const first = outcomeOf(client.command({ type: 'get_messages' }));
    const second = outcomeOf(client.command({ type: 'get_messages' }));
    await flushMicrotasks();
    const frames = socket.frames().filter((frame) => frame.type === 'get_messages');
    expect(frames).toHaveLength(2);
    socket.deliver(response('get_messages', String(frames[1].id), { messages: [] }));
    await flushMicrotasks();
    expect(second.state()).toBe('resolved');
    expect(first.state()).toBe('pending');
    socket.deliver(response('get_messages', String(frames[0].id), { messages: [] }));
    await flushMicrotasks();
    expect(first.state()).toBe('resolved');
  });

  it('rejects a frame that does not name the pending command', async () => {
    const client = newClient();
    const socket = await connectAndBootstrap(client);
    const named = client.command({ type: 'prompt', message: 'hi' });
    const observed = outcomeOf(named);
    await flushMicrotasks();
    const frame = commandFrame(socket, 'prompt');
    socket.deliver(response('abort', String(frame.id), {}));
    await flushMicrotasks();
    expect(observed.state()).toBe('rejected');
    expect(observed.error()?.message).toBe(RESPONSE_OWNERSHIP_REJECTIONS['command-mismatch']);
  });

  it('rejects a response owned by another session', async () => {
    const client = newClient();
    const socket = await connectAndBootstrap(client);
    const sourced = client.command({ type: 'get_state' }, 'session-1');
    const observed = outcomeOf(sourced);
    await flushMicrotasks();
    const frame = commandFrame(socket, 'get_state');
    socket.deliver(response('get_state', String(frame.id), { sessionId: 'session-9' }, 'session-9'));
    await flushMicrotasks();
    expect(observed.state()).toBe('rejected');
    expect(observed.error()?.message).toBe(RESPONSE_OWNERSHIP_REJECTIONS['source-ownership-mismatch']);

    const matched = client.command({ type: 'get_state' }, 'session-1');
    const matchedOutcome = outcomeOf(matched);
    await flushMicrotasks();
    const matchedFrame = commandFrame(socket, 'get_state');
    socket.deliver(response('get_state', String(matchedFrame.id), { sessionId: 'session-1' }, 'session-1'));
    await flushMicrotasks();
    expect(matchedOutcome.state()).toBe('resolved');
  });

  it('accepts only a target-owned success envelope for a lifecycle command', async () => {
    const client = newClient();
    const socket = await connectAndBootstrap(client);
    const created = client.command({ type: 'new_session', cwd: '/workspace' }, 'session-1');
    const createdOutcome = outcomeOf(created);
    await flushMicrotasks();
    const createdFrame = commandFrame(socket, 'new_session');
    // The runner tags a successful new_session with the NEW runner session.
    socket.deliver(response('new_session', String(createdFrame.id), { sessionId: 'session-2', runtimeIncarnation: 'inc-2', cwd: '/workspace' }, 'session-2'));
    await flushMicrotasks();
    expect(createdOutcome.state()).toBe('resolved');

    const forged = client.command({ type: 'new_session', cwd: '/workspace' }, 'session-1');
    const forgedOutcome = outcomeOf(forged);
    await flushMicrotasks();
    const forgedFrame = commandFrame(socket, 'new_session');
    socket.deliver(response('new_session', String(forgedFrame.id), { sessionId: 'session-2', runtimeIncarnation: 'inc-2', cwd: '/workspace' }, 'session-9'));
    await flushMicrotasks();
    expect(forgedOutcome.state()).toBe('rejected');
    expect(forgedOutcome.error()?.message).toBe(RESPONSE_OWNERSHIP_REJECTIONS['target-ownership-mismatch']);
  });
});

describe('ready gate', () => {
  it('queues a command issued during bootstrap and admits it exactly once', async () => {
    const client = newClient();
    client.connect();
    const socket = latestSocket();
    socket.open();
    expect(socket.frames().map((frame) => frame.type)).toEqual(['get_state']);

    const admitted = outcomeOf(client.command({ type: 'session_list', scope: 'all_projects' }));
    await flushMicrotasks();
    expect(admitted.state()).toBe('pending');
    // Nothing was admitted while the bootstrap was unfinished.
    expect(socket.frames().map((frame) => frame.type)).toEqual(['get_state']);

    const bootstrap = socket.frames()[0];
    socket.deliver(response('get_state', String(bootstrap.id), { sessionId: 'session-1', runtimeIncarnation: 'inc-1', cwd: '/workspace' }));
    await flushMicrotasks();
    const queued = socket.frames().filter((frame) => frame.type === 'session_list');
    expect(queued).toHaveLength(1);

    socket.deliver(response('session_list', String(queued[0].id), { sessions: [] }));
    await flushMicrotasks();
    expect(admitted.state()).toBe('resolved');
  });

  it('rejects a queued command exactly once when the client closes', async () => {
    const client = newClient();
    client.connect();
    const socket = latestSocket();
    socket.open();
    const queued = outcomeOf(client.command({ type: 'session_list', scope: 'all_projects' }));
    await flushMicrotasks();
    client.close();
    await flushMicrotasks();
    expect(queued.state()).toBe('rejected');
    expect(queued.error()?.message).toBe('Runner is not connected.');
    expect(socket.frames().filter((frame) => frame.type === 'session_list')).toHaveLength(0);
  });

  it('bounds the wait and releases every waiter once', async () => {
    const armed: Array<{ fn: () => void; ms: number; cleared: boolean }> = [];
    const scheduler: ReadyGateScheduler = {
      // Index-as-handle fake: the gate only round-trips the value.
      setTimeout: (fn, ms) => {
        armed.push({ fn, ms, cleared: false });
        return (armed.length - 1) as unknown as TransportTimerHandle;
      },
      clearTimeout: (handle) => {
        const entry = armed[handle as unknown as number];
        if (entry) entry.cleared = true;
      },
    };
    const gate = new ReadyGate(scheduler);
    const first = gate.wait();
    const second = gate.wait();
    expect(gate.size()).toBe(2);
    expect(armed[0].ms).toBe(READY_GATE_TIMEOUT_MS);
    // A replaced transport keeps queued waiters for the replacement bootstrap.
    gate.markNotReady();
    expect(gate.size()).toBe(2);
    gate.markReady();
    await Promise.all([first, second]);
    expect(gate.size()).toBe(0);
    expect(armed[0].cleared).toBe(true);
    expect(armed[1].cleared).toBe(true);
    await gate.wait();

    const dead = new ReadyGate(scheduler);
    const bounded = dead.wait();
    const observed = outcomeOf(bounded);
    await Promise.resolve();
    armed[armed.length - 1].fn();
    await flushMicrotasks();
    expect(observed.state()).toBe('rejected');
    expect(observed.error()?.message).toBe('not connected');
    expect(dead.size()).toBe(0);
  });
});

describe('heartbeat liveness', () => {
  it('treats any inbound frame as an answer and retires only on real silence', async () => {
    const client = newClient();
    const socket = await connectAndBootstrap(client);
    expect(socket.pings()).toBe(0);

    jest.advanceTimersByTime(HEARTBEAT_PING_INTERVAL_MS);
    expect(socket.pings()).toBe(1);
    expect(client.state).toBe('connected');

    // Any inbound frame — not only a pong — keeps the socket alive.
    socket.deliver({ type: 'pong' });
    jest.advanceTimersByTime(HEARTBEAT_PING_INTERVAL_MS);
    expect(socket.pings()).toBe(2);
    expect(client.state).toBe('connected');

    socket.deliver({ type: 'message_start', sessionId: 'session-1', runtimeIncarnation: 'inc-1' });
    jest.advanceTimersByTime(HEARTBEAT_PING_INTERVAL_MS);
    expect(socket.pings()).toBe(3);
    expect(client.state).toBe('connected');

    // Now the peer goes silent: the unanswered ping retires the socket.
    jest.advanceTimersByTime(HEARTBEAT_PONG_DEADLINE_MS + HEARTBEAT_PING_INTERVAL_MS);
    expect(client.state).toBe('reconnecting');
    expect(socket.readyState).toBe(FakeSocket.CLOSED);
    const pingsAtRetirement = socket.pings();
    jest.advanceTimersByTime(HEARTBEAT_PING_INTERVAL_MS * 4);
    expect(socket.pings()).toBe(pingsAtRetirement);
  });

  it('plans the ping from inbound traffic, never from command latency', () => {
    const now = 100_000;
    expect(HEARTBEAT_TIMEOUT_MS).toBe(60_000);
    expect(planHeartbeatPing(FakeSocket.OPEN, null, now, now)).toBe('send');
    expect(planHeartbeatPing(FakeSocket.OPEN, now - 1_000, now, now - 500)).toBe('send');
    expect(planHeartbeatPing(FakeSocket.OPEN, now - 1_000, now, now - 2_000)).toBe('wait-pong');
    expect(planHeartbeatPing(FakeSocket.OPEN, now - HEARTBEAT_PONG_DEADLINE_MS, now, now - HEARTBEAT_PONG_DEADLINE_MS - 1)).toBe('retire');
    expect(planHeartbeatPing(FakeSocket.CONNECTING, null, now)).toBe('idle');
    expect(planHeartbeatPing(FakeSocket.CLOSED, null, now)).toBe('retire');
    expect(planHeartbeatPing(null, null, now)).toBe('idle');
  });
});

describe('auth failure lifecycle', () => {
  it('retires the socket and clears ready waiters when the runner rejects the token mid-bootstrap', async () => {
    const client = newClient();
    client.connect();
    const socket = latestSocket();
    socket.open();
    const queued = outcomeOf(client.command({ type: 'session_list', scope: 'all_projects' }));
    await flushMicrotasks();
    expect(queued.state()).toBe('pending');

    // The runner refuses the token while the bootstrap is still in flight.
    socket.onclose?.({ code: 4001, reason: 'Token required' });
    await flushMicrotasks();
    expect(client.state).toBe('token_required');
    expect(socket.readyState).toBe(FakeSocket.CLOSED);
    expect(queued.state()).toBe('rejected');
    expect(queued.error()?.message).toBe('Runner is not connected.');

    // The bootstrap continuation must not downgrade the verdict into a
    // reconnect, and no heartbeat interval may survive the retirement: past the
    // ping cadence a leaked interval would retire the transport again.
    jest.advanceTimersByTime(HEARTBEAT_PING_INTERVAL_MS * 2);
    expect(client.state).toBe('token_required');
    expect(FakeSocket.instances).toHaveLength(1);
  });

  it('routes a bootstrap auth failure through the auth path instead of reconnecting', async () => {
    const client = newClient();
    client.connect();
    const socket = latestSocket();
    socket.failNextSend = 'Token required';
    socket.open();
    await flushMicrotasks();

    expect(client.state).toBe('token_required');
    expect(socket.readyState).toBe(FakeSocket.CLOSED);
    jest.advanceTimersByTime(HEARTBEAT_PING_INTERVAL_MS * 2);
    expect(FakeSocket.instances).toHaveLength(1);
    expect(client.state).toBe('token_required');
  });

  it('still reconnects on a non-auth bootstrap failure', async () => {
    const client = newClient();
    client.connect();
    const socket = latestSocket();
    socket.open();
    const bootstrap = socket.frames()[0];
    socket.deliver({ type: 'response', command: 'get_state', id: String(bootstrap.id), success: false, error: 'Runner state is unavailable.' });
    await flushMicrotasks();
    expect(client.state).toBe('reconnecting');
    jest.advanceTimersByTime(RUNNER_RECONNECT_INITIAL_MS);
    expect(FakeSocket.instances).toHaveLength(2);
  });
});

describe('reconnect backoff', () => {
  it('doubles while the connection is unstable and resets only after a stable bootstrap', async () => {
    const client = newClient();
    const first = await connectAndBootstrap(client);

    first.onclose?.({ code: 1006, reason: 'lost' });
    jest.advanceTimersByTime(RUNNER_RECONNECT_INITIAL_MS);
    expect(FakeSocket.instances).toHaveLength(2);

    // A handshake that never bootstraps is not stability: the wait doubles.
    const second = latestSocket();
    second.open();
    second.onclose?.({ code: 1006, reason: 'lost' });
    jest.advanceTimersByTime(RUNNER_RECONNECT_INITIAL_MS);
    expect(FakeSocket.instances).toHaveLength(2);
    jest.advanceTimersByTime(RUNNER_RECONNECT_INITIAL_MS);
    expect(FakeSocket.instances).toHaveLength(3);

    // A stable bootstrap resets the backoff to the initial delay.
    const third = latestSocket();
    third.open();
    third.deliver(response('get_state', String(commandFrame(third, 'get_state').id), { sessionId: 'session-1', runtimeIncarnation: 'inc-1', cwd: '/workspace' }));
    await flushMicrotasks();
    expect(client.state).toBe('connected');
    third.onclose?.({ code: 1006, reason: 'lost' });
    jest.advanceTimersByTime(RUNNER_RECONNECT_INITIAL_MS);
    expect(FakeSocket.instances).toHaveLength(4);
  });
});
