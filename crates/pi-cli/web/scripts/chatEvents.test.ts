#!/usr/bin/env node
import { hostTokenKey, type StorageLike } from '../src/hostToken.ts';
import {
  MAX_RUNNER_EVENT_LISTENERS,
  dispatchRunnerEvent,
  runnerEventListenerCount,
  subscribeRunnerEvents,
} from '../src/chat/events.ts';
import {
  isRpcResponse,
  isRunnerEventFrame,
  type RunnerCommand,
} from '../src/chat/protocol.ts';
import { RunnerRegistry } from '../src/chat/runners.ts';
import { planReconnectDisplayedCatchup, reconnectSubscribeCursor } from '../src/protocol.ts';
import { planReconnectTurn, planStreamStallReconcile, planLiveSequenceGap, planLiveGapCatchUp, planStreamCatchUpToast } from '../src/transcript.ts';
import { AUTHORITATIVE_CUT_TIMEOUT_MS, planAuthoritativeCutTick } from '../src/sessionSwitch.ts';
import {
  SESSION_PROCESSING_CODE,
  composerSubmitWire,
  dispatchComposerSubmit,
  planComposerSubmitKind,
  planPromptBusySteerRetry,
  rpcFailureToastSuppressed,
} from '../src/composerSubmit.ts';
import { isBusyCapacityError, rpcErrorCode } from '../src/types.ts';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  CJK_SHEET_TASK,
  phoneSheetRects,
  rectsIntersect,
  subagentSheetSectionRects,
  subagentSheetTaskShrinks,
} from '../src/subagentSheetLayout.ts';

const failures: string[] = [];
let assertions = 0;
let activeBlock = 'module initialization';
const RUNNER_ORIGIN = 'https://runner-a.example';
const RUNNER_AUTHORITY = 'runner-a.example';
const AUTH_MARKER = 'auth-marker-a';
const SAFE_CWD = '<workspace>/runner-a';

function incarnationOf(sessionId: string): string {
  return `inc-${sessionId}`;
}

function stateOf(sessionId: string, extra: Record<string, unknown> = {}): Record<string, unknown> {
  return { sessionId, runtimeIncarnation: incarnationOf(sessionId), ...extra };
}

function loadedRow(sessionId: string, extra: Record<string, unknown> = {}): Record<string, unknown> {
  return { sessionId, loaded: true, runtimeIncarnation: incarnationOf(sessionId), ...extra };
}

function check(name: string, condition: boolean, detail = ''): void {
  assertions += 1;
  if (!condition) failures.push(`${activeBlock}: ${name}${detail ? ` (${detail})` : ''}`);
}

function storage(): StorageLike & { values: Map<string, string> } {
  const values = new Map<string, string>();
  return {
    values,
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => { values.set(key, value); },
    removeItem: (key) => { values.delete(key); },
  };
}

interface FakeTimer {
  id: number;
  delay: number;
  handler: () => void;
}

function timers() {
  let nextId = 0;
  const scheduled = new Map<number, FakeTimer>();
  return {
    scheduled,
    setTimeoutFn: (handler: () => void, delay: number) => {
      const id = ++nextId;
      scheduled.set(id, { id, delay, handler });
      return id;
    },
    clearTimeoutFn: (id: number) => { scheduled.delete(id); },
    runDelay: (delay: number) => {
      const timer = [...scheduled.values()].find((candidate) => candidate.delay === delay);
      if (!timer) return false;
      scheduled.delete(timer.id);
      timer.handler();
      return true;
    },
  };
}

class FakeWebSocket {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;
  readyState = FakeWebSocket.CONNECTING;
  protocol = '';
  onopen: ((event: Event) => void) | null = null;
  onmessage: ((event: MessageEvent) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  onclose: ((event: CloseEvent) => void) | null = null;
  sent: string[] = [];
  closed = false;

  constructor(readonly url: string, readonly protocols: string[]) {}

  open(): void {
    this.readyState = FakeWebSocket.OPEN;
    this.protocol = this.protocols[0] ?? '';
    this.onopen?.({ target: this } as unknown as Event);
  }

  send(frame: string): void {
    if (this.readyState !== FakeWebSocket.OPEN) throw new Error('not open');
    this.sent.push(frame);
  }

  respond(command: string, success: boolean, data?: unknown, error?: string): void {
    const sent = this.sent.map((frame) => JSON.parse(frame) as { id?: string; type?: string; sessionId?: string })
      .find((frame) => frame.type === command);
    if (!sent?.id) throw new Error(`missing ${command} command on ${this.url}`);
    const bootstrapSessionId = command === 'get_state'
      && data && typeof data === 'object'
      && typeof (data as { sessionId?: unknown }).sessionId === 'string'
      ? (data as { sessionId: string }).sessionId
      : '';
    this.onmessage?.({
      target: this,
      data: JSON.stringify({
        type: 'response',
        id: sent.id,
        command,
        ...((sent.sessionId || bootstrapSessionId)
          ? { sessionId: sent.sessionId || bootstrapSessionId }
          : {}),
        success,
        data,
        error,
      }),
    } as MessageEvent);
  }

  emit(frame: unknown): void {
    this.onmessage?.({
      target: this,
      data: JSON.stringify(frame),
    } as MessageEvent);
  }

  close(code = 1000, reason = ''): void {
    this.closed = true;
    this.readyState = FakeWebSocket.CLOSED;
    this.onclose?.({ target: this, code, reason } as CloseEvent);
  }
}

function installWebSocketConstants(): void {
  Object.defineProperty(globalThis, 'WebSocket', {
    configurable: true,
    value: FakeWebSocket,
  });
}

function deterministicBytes(length: number): Uint8Array {
  return Uint8Array.from({ length }, (_, index) => index + 1);
}

async function flush(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
}

function wire(command: RunnerCommand): string {
  return JSON.stringify(command);
}

async function probe(socket: FakeWebSocket): Promise<void> {
  socket.open();
  socket.respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD }));
  await flush();
  socket.respond('session_list', true, { sessions: [loadedRow('session-a'), loadedRow('child-a')] });
  await flush();
}

async function main(): Promise<void> {
  installWebSocketConstants();

  activeBlock = 'command serialization';
  {
    const commands: RunnerCommand[] = [
      { type: 'prompt', message: 'hello' },
      { type: 'abort' },
      { type: 'goal_create', objective: 'ship' },
      { type: 'goal_create', objective: 'ship', tokenBudget: 12 },
      { type: 'goal_pause' },
      { type: 'goal_resume' },
      { type: 'goal_complete' },
      { type: 'goal_drop' },
      { type: 'goal_get' },
      { type: 'goal_journal' },
      { type: 'job_list' },
      { type: 'task_spawn', args: { task: 'inspect source' } },
      { type: 'task_spawn', args: { task: 'inspect source', agent: 'writer' } },
      { type: 'job_cancel', jobIds: ['job-1'] },
      { type: 'job_output', jobId: 'job-1' },
      { type: 'hub_send', to: 'writer', body: 'ping' },
      { type: 'hub_send', to: 'writer', body: 'ping', replyTo: 'msg-1' },
    ];
    check('prompt serializes message, not text', wire(commands[0]) === '{"type":"prompt","message":"hello"}');
    check('abort serializes bare', wire(commands[1]) === '{"type":"abort"}');
    check('goal_create omits absent tokenBudget', wire(commands[2]) === '{"type":"goal_create","objective":"ship"}');
    check('goal_create includes tokenBudget', wire(commands[3]) === '{"type":"goal_create","objective":"ship","tokenBudget":12}');
    check('goal_pause wire', wire(commands[4]) === '{"type":"goal_pause"}');
    check('goal_resume wire', wire(commands[5]) === '{"type":"goal_resume"}');
    check('goal_complete wire', wire(commands[6]) === '{"type":"goal_complete"}');
    check('goal_drop wire', wire(commands[7]) === '{"type":"goal_drop"}');
    check('goal_get wire', wire(commands[8]) === '{"type":"goal_get"}');
    check('goal_journal wire', wire(commands[9]) === '{"type":"goal_journal"}');
    check('job_list wire', wire(commands[10]) === '{"type":"job_list"}');
    check('task_spawn serializes task only', wire(commands[11]) === '{"type":"task_spawn","args":{"task":"inspect source"}}');
    check('task_spawn serializes task and agent', wire(commands[12]) === '{"type":"task_spawn","args":{"task":"inspect source","agent":"writer"}}');
    check('job_cancel serializes jobIds', wire(commands[13]) === '{"type":"job_cancel","jobIds":["job-1"]}');
    check('job_output serializes jobId', wire(commands[14]) === '{"type":"job_output","jobId":"job-1"}');
    check('hub_send omits absent replyTo', wire(commands[15]) === '{"type":"hub_send","to":"writer","body":"ping"}');
    check('hub_send includes replyTo', wire(commands[16]) === '{"type":"hub_send","to":"writer","body":"ping","replyTo":"msg-1"}');
    for (const command of commands.slice(11)) {
      check(`${command.type} omits from`, !Object.prototype.hasOwnProperty.call(JSON.parse(wire(command)), 'from'));
    }
  }

  activeBlock = 'composer submit while streaming and prompt-busy convert';
  {
    check('streaming submit plans steer even when requested prompt',
      planComposerSubmitKind({ streaming: true, requested: 'prompt' }) === 'steer');
    check('idle submit keeps requested prompt',
      planComposerSubmitKind({ streaming: false, requested: 'prompt' }) === 'prompt');
    check('idle requested steer stays steer',
      planComposerSubmitKind({ streaming: false, requested: 'steer' }) === 'steer');
    check('session_processing is a closed rpc code',
      rpcErrorCode({ code: SESSION_PROCESSING_CODE }) === 'session_processing');
    check('busy_capacity is a closed rpc code',
      rpcErrorCode({ code: 'busy_capacity' }) === 'busy_capacity'
      && isBusyCapacityError({ rpcCode: 'busy_capacity' }) === true
      && isBusyCapacityError({ rpcCode: 'session_processing' }) === false
      && isBusyCapacityError(new Error('too many concurrent RPC commands (limit 16)')) === false);
    check('prose busy string is not a session_processing code',
      rpcErrorCode({ code: 'session is already processing; choose steer or followUp' }) === undefined);
    check('later live gaps coalesce onto one catch-up',
      planLiveGapCatchUp({ recoveryInFlight: true }) === 'coalesce'
      && planStreamCatchUpToast({ toastOwned: false, transport: false, busyCapacity: true }) === 'silent');
    check('typed prompt failure converts to steer once',
      planPromptBusySteerRetry({ submittedKind: 'prompt', rpcCode: 'session_processing', convertedOnce: false }) === 'steer');
    check('typed prompt failure does not convert twice',
      planPromptBusySteerRetry({ submittedKind: 'prompt', rpcCode: 'session_processing', convertedOnce: true }) === 'none');
    check('steer failure does not convert',
      planPromptBusySteerRetry({ submittedKind: 'steer', rpcCode: 'session_processing', convertedOnce: false }) === 'none');
    check('busy_capacity with processing prose does not convert',
      planPromptBusySteerRetry({ submittedKind: 'prompt', rpcCode: 'busy_capacity', convertedOnce: false }) === 'none');
    check('prompt session_processing suppresses the App toast',
      rpcFailureToastSuppressed({ commandType: 'prompt', rpcCode: 'session_processing' }) === true);
    check('steer session_processing still toasts',
      rpcFailureToastSuppressed({ commandType: 'steer', rpcCode: 'session_processing' }) === false);

    const streamingKinds: string[] = [];
    const streaming = await dispatchComposerSubmit({
      kind: planComposerSubmitKind({ streaming: true, requested: 'prompt' }),
      send: async (kind) => {
        streamingKinds.push(kind);
        return { ok: true };
      },
      onConvertToSteer: () => {
        throw new Error('streaming submit must not convert after send');
      },
    });
    check('submit-while-streaming routes steer',
      streaming.kind === 'steer' && streaming.converted === false
        && streamingKinds.join(',') === 'steer'
        && composerSubmitWire('steer', 'keep this').type === 'steer');
    check('missed turn_terminal with a streaming bubble plans settled refill',
      planStreamStallReconcile({ snapshotStreaming: false, liveHasStreamingAssistant: true }) === 'refill-settled'
        && planLiveSequenceGap({ cursor: 3, sequence: 5 }) === 'gap');

    const raceKinds: string[] = [];
    const raceMessages: string[] = [];
    let converted = 0;
    const raced = await dispatchComposerSubmit({
      kind: 'prompt',
      send: async (kind) => {
        raceKinds.push(kind);
        if (kind === 'prompt') {
          throw Object.assign(new Error('session is already processing; choose steer or followUp'), {
            rpcCode: SESSION_PROCESSING_CODE,
          });
        }
        raceMessages.push('keep this');
        return { queued: true };
      },
      onConvertToSteer: () => { converted += 1; },
    });
    check('typed prompt-failure auto-converts to steer once',
      raced.kind === 'steer' && raced.converted === true
        && converted === 1
        && raceKinds.join(',') === 'prompt,steer'
        && raceMessages.join(',') === 'keep this');

    const loopKinds: string[] = [];
    let loopThrew = false;
    try {
      await dispatchComposerSubmit({
        kind: 'prompt',
        send: async (kind) => {
          loopKinds.push(kind);
          throw Object.assign(new Error('session is already processing; choose steer or followUp'), {
            rpcCode: SESSION_PROCESSING_CODE,
          });
        },
        onConvertToSteer: () => {},
      });
    } catch {
      loopThrew = true;
    }
    check('typed prompt-failure converts only once then surfaces the steer error',
      loopThrew && loopKinds.join(',') === 'prompt,steer');

    const proseKinds: string[] = [];
    let proseThrew = false;
    try {
      await dispatchComposerSubmit({
        kind: 'prompt',
        send: async (kind) => {
          proseKinds.push(kind);
          throw Object.assign(new Error('session is already processing; choose steer or followUp'), {
            rpcCode: 'busy_capacity',
          });
        },
        onConvertToSteer: () => { throw new Error('prose must not convert'); },
      });
    } catch {
      proseThrew = true;
    }
    check('processing prose without session_processing code does not convert',
      proseThrew && proseKinds.join(',') === 'prompt');
  }

  activeBlock = 'isRpcResponse vs event frames';
  {
    const response = { type: 'response', command: 'prompt', success: true, id: 'c1' };
    const event = { type: 'message_start', sessionId: 'session-a', sequence: 1, itemId: 'a1' };
    check('isRpcResponse accepts response frames', isRpcResponse(response));
    check('isRpcResponse excludes event frames', !isRpcResponse(event));
    check('isRpcResponse excludes response-shaped events missing command', !isRpcResponse({ type: 'response' }));
    check('isRunnerEventFrame accepts App.tsx event frames', isRunnerEventFrame(event));
    check('isRunnerEventFrame excludes response frames', !isRunnerEventFrame(response));
    check('isRunnerEventFrame excludes untyped frames', !isRunnerEventFrame({ sessionId: 'session-a' }));
  }

  activeBlock = 'subscribe/unsubscribe';
  {
    const runnerId = 'runner-sub';
    const seen: unknown[] = [];
    const extra: unknown[] = [];
    const first = (frame: unknown) => { seen.push(frame); };
    const second = (frame: unknown) => { extra.push(frame); };
    const unsubscribe = subscribeRunnerEvents(runnerId, first);
    const unsubscribeSecond = subscribeRunnerEvents(runnerId, second);
    check('two listeners register', runnerEventListenerCount(runnerId) === 2);
    dispatchRunnerEvent(runnerId, { type: 'turn_started', sessionId: 's1' });
    check('first listener receives the frame', seen.length === 1);
    check('second listener receives the frame', extra.length === 1);
    unsubscribe();
    dispatchRunnerEvent(runnerId, { type: 'turn_complete', sessionId: 's1' });
    check('unsubscribed listener is silent', seen.length === 1);
    check('remaining listener still receives frames', extra.length === 2);
    unsubscribeSecond();
    check('unsubscribe of last listener drops the set', runnerEventListenerCount(runnerId) === 0);
    const emptyUnsub = subscribeRunnerEvents('  ', first);
    emptyUnsub();
    check('blank runnerId does not register', runnerEventListenerCount('  ') === 0);

    const boundedId = 'runner-bound';
    const unsubs: Array<() => void> = [];
    for (let i = 0; i < MAX_RUNNER_EVENT_LISTENERS; i += 1) {
      unsubs.push(subscribeRunnerEvents(boundedId, () => {}));
    }
    const overflowSeen: unknown[] = [];
    const overflowUnsub = subscribeRunnerEvents(boundedId, (frame) => { overflowSeen.push(frame); });
    check('listener set is bounded', runnerEventListenerCount(boundedId) === MAX_RUNNER_EVENT_LISTENERS);
    dispatchRunnerEvent(boundedId, { type: 'job_updated', sessionId: 's1' });
    check('overflow listener is a no-op', overflowSeen.length === 0);
    overflowUnsub();
    for (const unsub of unsubs) unsub();
    check('bounded set can be fully released', runnerEventListenerCount(boundedId) === 0);
  }

  activeBlock = 'event routing through runners';
  {
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: (async (input: RequestInfo | URL) => ({
        ok: true,
        json: async () => ({ tokenRequired: false, version: '0.2.11', codename: null }),
      } as Response)) as typeof fetch,
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
      now: () => 100,
      randomBytes: deterministicBytes,
    });
    const added = registry.add('Runner A', RUNNER_ORIGIN, AUTH_MARKER);
    await flush();
    await probe(sockets[0]);
    const connected = await added;
    const received: unknown[] = [];
    const unsubscribe = subscribeRunnerEvents(connected.id, (frame) => { received.push(frame); });
    const event = { type: 'message_start', sessionId: 'session-a', runtimeIncarnation: incarnationOf('session-a'), sequence: 1, itemId: 'a1' };
    sockets[0].emit(event);
    check('event routing reaches the subscriber', received.length === 1);
    const routed = received[0];
    const routedSessionId = routed && typeof routed === 'object' && 'sessionId' in routed && typeof routed.sessionId === 'string'
      ? routed.sessionId
      : '';
    check('routed frame keeps session identity', routedSessionId === 'session-a');
    sockets[0].respond('get_state', true, { sessionId: 'ignored' });
    check('rpc responses do not reach event subscribers', received.length === 1);
    sockets[0].emit({ sessionId: 'session-a' });
    check('untyped frames are not routed', received.length === 1);
    unsubscribe();
    registry.stop();
  }

  activeBlock = 'reconnect generation does not duplicate listeners';
  {
    const store = storage();
    store.setItem(hostTokenKey(RUNNER_AUTHORITY), AUTH_MARKER);
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: (async (input: RequestInfo | URL) => ({
        ok: true,
        json: async () => ({ tokenRequired: false, version: '0.2.11', codename: null }),
      } as Response)) as typeof fetch,
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
      now: () => 100,
      randomBytes: deterministicBytes,
    });
    const added = registry.add('Runner A', RUNNER_ORIGIN, AUTH_MARKER);
    await flush();
    await probe(sockets[0]);
    const connected = await added;
    const received: unknown[] = [];
    const unsubscribe = subscribeRunnerEvents(connected.id, (frame) => { received.push(frame); });
    check('one listener before reconnect', runnerEventListenerCount(connected.id) === 1);
    sockets[0].emit({ type: 'turn_started', sessionId: 'session-a', runtimeIncarnation: incarnationOf('session-a') });
    sockets[0].close(1006, '');
    const reconnectTimer = [...fakeTimers.scheduled.values()].find((timer) => timer.delay > 0);
    check('reconnect reuses the existing timer path', Boolean(reconnectTimer));
    if (reconnectTimer) {
      fakeTimers.scheduled.delete(reconnectTimer.id);
      reconnectTimer.handler();
    }
    await flush();
    check('reconnect opened a replacement socket', sockets.length === 2, String(sockets.length));
    await probe(sockets[1]);
    check('listener set is unchanged after reconnect', runnerEventListenerCount(connected.id) === 1);
    sockets[1].emit({ type: 'turn_complete', sessionId: 'session-a', runtimeIncarnation: incarnationOf('session-a') });
    const reconnectTypes = received.map((frame) => (
      frame && typeof frame === 'object' && 'type' in frame && typeof frame.type === 'string'
        ? frame.type
        : ''
    ));
    check(
      'post-reconnect event reaches the original listener once',
      reconnectTypes.filter((type) => type === 'turn_started').length === 1
        && reconnectTypes.filter((type) => type === 'turn_complete').length === 1
        && reconnectTypes.filter((type) => type === 'runner_connected').length === 1,
      reconnectTypes.join(','),
    );
    unsubscribe();
    registry.stop();
  }

  activeBlock = 'reconnect catchup refills displayed and resumes the turn';
  {
    const primary = { sessionId: 'listener-primary', runtimeIncarnation: incarnationOf('listener-primary') };
    const displayed = { sessionId: 'session-a', runtimeIncarnation: incarnationOf('session-a') };
    const catchup = planReconnectDisplayedCatchup(primary, displayed);
    check('catchup refills displayed not primary', catchup.refillSessionId === displayed.sessionId);
    check('catchup subscribes displayed on the new generation',
      catchup.subscribe.some((identity) => identity.sessionId === displayed.sessionId));
    const midTurn = planReconnectTurn({
      isStreaming: true,
      liveItems: [{ kind: 'assistant', id: 'msg-server', status: 'streaming', blocks: [] }],
      activeAssistantId: 'msg-server',
    });
    check('mid-turn catchup keeps the server assistant id',
      midTurn.kind === 'streaming' && midTurn.assistantId === 'msg-server');
    const finished = planReconnectTurn({
      isStreaming: false,
      liveItems: [{ kind: 'assistant', id: 'msg-server', status: 'streaming', blocks: [] }],
      activeAssistantId: 'msg-server',
    });
    check('finished-turn catchup is settled history', finished.kind === 'settled');
    check('reconnect subscribe cursor keeps last-good watermark',
      reconnectSubscribeCursor(872) === 872 && reconnectSubscribeCursor(undefined) === 0);
    check('cut timer holds while refill streams and fires after 15s of silence',
      planAuthoritativeCutTick({
        now: 20_000,
        lastProgressAt: 0,
        timeoutMs: AUTHORITATIVE_CUT_TIMEOUT_MS,
        refillInFlight: true,
      }) === 'hold'
      && planAuthoritativeCutTick({
        now: 14_999,
        lastProgressAt: 0,
        timeoutMs: AUTHORITATIVE_CUT_TIMEOUT_MS,
        refillInFlight: false,
      }) === 'hold'
      && planAuthoritativeCutTick({
        now: 15_000,
        lastProgressAt: 0,
        timeoutMs: AUTHORITATIVE_CUT_TIMEOUT_MS,
        refillInFlight: false,
      }) === 'fire');

    const store = storage();
    store.setItem(hostTokenKey(RUNNER_AUTHORITY), AUTH_MARKER);
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: (async (input: RequestInfo | URL) => ({
        ok: true,
        json: async () => ({ tokenRequired: false, version: '0.2.11', codename: null }),
      } as Response)) as typeof fetch,
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
      now: () => 100,
      randomBytes: deterministicBytes,
    });
    const added = registry.add('Runner A', RUNNER_ORIGIN, AUTH_MARKER);
    await flush();
    await probe(sockets[0]);
    const connected = await added;
    const received: unknown[] = [];
    const unsubscribe = subscribeRunnerEvents(connected.id, (frame) => { received.push(frame); });
    sockets[0].emit({
      type: 'turn_started',
      sessionId: displayed.sessionId,
      runtimeIncarnation: displayed.runtimeIncarnation,
    });
    sockets[0].close(1006, '');
    const reconnectTimer = [...fakeTimers.scheduled.values()].find((timer) => timer.delay > 0);
    if (reconnectTimer) {
      fakeTimers.scheduled.delete(reconnectTimer.id);
      reconnectTimer.handler();
    }
    await flush();
    await probe(sockets[1]);
    sockets[1].emit({
      type: 'message_update',
      sessionId: displayed.sessionId,
      runtimeIncarnation: displayed.runtimeIncarnation,
      itemId: 'msg-server',
      assistantMessageEvent: { type: 'text_delta', delta: ' more' },
    });
    sockets[1].emit({
      type: 'turn_complete',
      sessionId: displayed.sessionId,
      runtimeIncarnation: displayed.runtimeIncarnation,
    });
    const types = received.map((frame) => (
      frame && typeof frame === 'object' && 'type' in frame && typeof frame.type === 'string'
        ? frame.type
        : ''
    ));
    check(
      'post-reconnect displayed stream event is delivered on the new generation',
      types.filter((type) => type === 'message_update').length === 1
        && types.filter((type) => type === 'turn_complete').length === 1,
      types.join(','),
    );
    unsubscribe();
    registry.stop();
  }
  {
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: (async (input: RequestInfo | URL) => ({
        ok: true,
        json: async () => ({ tokenRequired: false, version: '0.2.11', codename: null }),
      } as Response)) as typeof fetch,
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
      now: () => 100,
      randomBytes: deterministicBytes,
    });
    const added = registry.add('Runner A', RUNNER_ORIGIN, AUTH_MARKER);
    await flush();
    await probe(sockets[0]);
    const connected = await added;
    const received: unknown[] = [];
    const unsubscribe = subscribeRunnerEvents(connected.id, (frame) => { received.push(frame); });
    sockets[0].emit({
      type: 'subscription_snapshot',
      reason: 'cold-full',
      sessionId: 'session-a',
      runtimeIncarnation: incarnationOf('session-a'),
      watermark: 1,
      state: {},
      messages: [{ message: { role: 'user', content: [{ type: 'text', text: 'hi' }] } }],
      events: [{ type: 'message_start', sessionId: 'session-a', runtimeIncarnation: incarnationOf('session-a'), sequence: 1, itemId: 'a1' }],
    });
    check('typed snapshot is dispatched', received.length === 1);
    const snapshot = received[0] as { type?: unknown; sessionId?: unknown; watermark?: unknown; messages?: unknown; events?: unknown; authoritative?: unknown };
    check('snapshot type is preserved', snapshot.type === 'subscription_snapshot');
    check('snapshot session identity is preserved', snapshot.sessionId === 'session-a');
    check('snapshot watermark is parsed', snapshot.watermark === 1);
    check('snapshot messages are parsed', Array.isArray(snapshot.messages) && snapshot.messages.length === 1);
    check('snapshot events keep order', Array.isArray(snapshot.events) && (snapshot.events as Array<{ type?: string }>)[0]?.type === 'message_start');
    check('snapshot carries the authoritative verdict', snapshot.authoritative === true);
    sockets[0].emit({
      type: 'session_resync',
      reason: 'resync',
      sessionId: 'session-a',
      runtimeIncarnation: incarnationOf('session-a'),
      watermark: 2,
      state: {},
      messages: [{ message: { role: 'assistant', content: [{ type: 'text', text: 'done' }] } }],
      events: [{ type: 'turn_started', sessionId: 'session-a', runtimeIncarnation: incarnationOf('session-a'), sequence: 2 }],
    });
    check('typed session_resync is dispatched after snapshot', received.length === 2);
    const resync = received[1] as { type?: unknown; sessionId?: unknown; watermark?: unknown; messages?: unknown; authoritative?: unknown };
    check('resync type is preserved', resync.type === 'session_resync');
    check('resync session identity is preserved', resync.sessionId === 'session-a');
    check('resync watermark is parsed', resync.watermark === 2);
    sockets[0].emit({ type: 'subscription_error', sessionId: 'session-a', runtimeIncarnation: incarnationOf('session-a'), error: 'already in progress' });
    sockets[0].emit({ type: 'subscription_error', error: 'missing session' });
    check('subscription_error is not dispatched', received.length === 2);
    sockets[0].emit({ type: 'session_resync_required', sessionId: 'session-a', runtimeIncarnation: incarnationOf('session-a'), sequence: 3 });
    check('session_resync_required is dispatched', received.length === 3);
    check('resync carries the authoritative verdict', resync.authoritative === true);
    sockets[0].emit({ type: 'message_start', sessionId: 'session-a', runtimeIncarnation: incarnationOf('session-a'), sequence: 4, itemId: 'a1' });
    check('ordinary events still dispatch', received.length === 4);
    sockets[0].emit({
      type: 'subscription_snapshot',
      reason: 'warm-continuity',
      sessionId: 'session-a',
      runtimeIncarnation: incarnationOf('session-a'),
      watermark: 4,
      state: {},
      messages: [],
      events: [],
    });
    const reconnect = received[4] as { type?: unknown; authoritative?: unknown };
    check('reconnect snapshot is flagged authoritative', received.length === 5 && reconnect.type === 'subscription_snapshot' && reconnect.authoritative === true);
    unsubscribe();
    registry.stop();
  }

  activeBlock = 'snapshot dispatch precedes command unblock';
  {
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: (async (input: RequestInfo | URL) => ({
        ok: true,
        json: async () => ({ tokenRequired: false, version: '0.2.11', codename: null }),
      } as Response)) as typeof fetch,
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
      now: () => 100,
      randomBytes: deterministicBytes,
    });
    const added = registry.add('Runner A', RUNNER_ORIGIN, AUTH_MARKER);
    await flush();
    await probe(sockets[0]);
    const connected = await added;
    const received: unknown[] = [];
    const unsubscribe = subscribeRunnerEvents(connected.id, (frame) => { received.push(frame); });
    const pending = registry.command(connected.id, { type: 'get_messages' }, { sourceSessionId: 'child-a' });
    await flush();
    sockets[0].emit({
      type: 'subscription_snapshot',
      reason: 'cold-full',
      sessionId: 'other-session',
      runtimeIncarnation: incarnationOf('other-session'),
      watermark: 9,
      state: {},
      messages: [],
      events: [{ type: 'turn_started', sessionId: 'other-session', runtimeIncarnation: incarnationOf('other-session'), sequence: 9 }],
    });
    await flush();
    check('wrong-session snapshot is not dispatched to runner listeners', received.length === 0);
    check('wrong-session snapshot does not send the pending command', !sockets[0].sent.some((frame) => (JSON.parse(frame) as { type?: string }).type === 'get_messages'));
    sockets[0].emit({
      type: 'subscription_snapshot',
      reason: 'cold-full',
      sessionId: 'child-a',
      runtimeIncarnation: incarnationOf('child-a'),
      watermark: 4,
      state: {},
      messages: [{ message: { role: 'user', content: [{ type: 'text', text: 'ship it' }] } }],
      events: [
        { type: 'turn_started', sessionId: 'child-a', runtimeIncarnation: incarnationOf('child-a'), sequence: 1 },
        { type: 'message_start', sessionId: 'child-a', runtimeIncarnation: incarnationOf('child-a'), sequence: 2, itemId: 'a1' },
        { type: 'tool_execution_start', sessionId: 'child-a', runtimeIncarnation: incarnationOf('child-a'), sequence: 3, itemId: 'tool-1' },
      ],
    });
    await flush();
    check('matching snapshot dispatches before command send', received.length === 1);
    const matching = received[0] as { type?: unknown; sessionId?: unknown; watermark?: unknown; events?: Array<{ type?: string }>; authoritative?: unknown };
    check('matching snapshot keeps session identity', matching.sessionId === 'child-a');
    check('matching snapshot watermark is parsed', matching.watermark === 4);
    check('matching snapshot events stay in order', (matching.events ?? []).map((event) => event.type).join(',') === 'turn_started,message_start,tool_execution_start');
    check('matching snapshot carries the authoritative verdict', matching.authoritative === true);
    check('matching snapshot unblocks get_messages', sockets[0].sent.some((frame) => (JSON.parse(frame) as { type?: string }).type === 'get_messages'));
    sockets[0].respond('get_messages', true, { messages: [] });
    await pending;
    sockets[0].emit({ type: 'message_start', sessionId: 'child-a', runtimeIncarnation: incarnationOf('child-a'), sequence: 5, itemId: 'a2' });
    check('ordinary events still dispatch after ready snapshot', received.length === 2);
    unsubscribe();
    registry.stop();
  }

  {
    activeBlock = 'subagent sheet TASK/LATEST ACTIVITY stacking';
    const here = dirname(fileURLToPath(import.meta.url));
    const styles = readFileSync(join(here, '../src/styles.css'), 'utf8');
    const panel = readFileSync(join(here, '../src/panels/SubagentsPanel.tsx'), 'utf8');
    const shrinking = subagentSheetSectionRects({
      viewportWidth: 390,
      padX: 16,
      gap: 10,
      statusHeight: 20,
      taskText: CJK_SHEET_TASK,
      fontSize: 13,
      lineHeight: 1.45,
      cjkAdvance: 1,
      activityHeight: 36,
      transcriptLabelHeight: 16,
      taskShrinks: true,
    });
    const shipped = phoneSheetRects(styles);
    check('CJK TASK intersects LATEST ACTIVITY when TASK shrinks',
      rectsIntersect(shrinking.task, shrinking.activity));
    check('shipped CSS does not shrink TASK below wrapped content',
      !subagentSheetTaskShrinks(styles)
      && !styles.includes('.subagent-details__section--transcript { flex: 1; }')
      && styles.includes('.subagent-details__body > * {'));
    check('390 CJK TASK and LATEST ACTIVITY do not intersect',
      !rectsIntersect(shipped.task, shipped.activity)
      && shipped.task.bottom <= shipped.activity.top);
    check('TASK block lives in the scrolling body, not the pinned head',
      panel.includes('data-details-task-section')
      && panel.indexOf('subagent-details__body') < panel.indexOf('data-details-description')
      && panel.indexOf('subagent-details__head') < panel.indexOf('subagent-details__body'));
  }

  console.log(`\nchatEvents.test: ${assertions} assertions, ${failures.length} failure(s)`);
  if (failures.length) {
    console.error(failures.join('\n'));
    process.exit(1);
  }
}

main().catch((error) => {
  console.error(`chatEvents.test (${activeBlock}): ${error instanceof Error ? error.message : String(error)}`);
  process.exit(1);
});
