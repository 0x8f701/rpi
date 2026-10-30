#!/usr/bin/env node
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { hostTokenKey, type StorageLike } from '../src/hostToken.ts';
import {
  RunnerAuthRequiredError,
  RUNNER_TOKEN_REQUIRED_MESSAGE,
  RUNNER_UNREACHABLE_MESSAGE,
  saveRunnerToken,
  parseRunnerEndpoint,
  parseRunnerInviteLink,
  readRunnerStatus,
} from '../src/chat/auth.ts';
import {
  RUNNER_STORAGE_KEY,
  RunnerCommandError,
  RunnerRegistry,
  loadRunnerRecords,
  runnerDisplayName,
  runnerDuplicateAuthority,
  runnerIdentityName,
  runnerMentionName,
  runnerHealthRank,
  sameRunnerIdentity,
  dedupeRunnersKeepHealthiest,
  sortRunnersOnlineFirst,
  saveRunnerRecords,
  type RunnerConnectionState,
  type RunnerRecord,
} from '../src/chat/runners.ts';

import {
  CHAT_MAPPING_STORAGE_KEY,
  ChatMappingStore,
} from '../src/chat/mapping.ts';
import { subscribeRunnerEvents } from '../src/chat/events.ts';
import { RUNNER_COMMAND_TIMEOUT_MS, RUNNER_RECONNECT_INITIAL_MS, TMUX_TERMINAL_RESIZE_TIMEOUT_MS } from '../src/chat/protocol.ts';
import {
  RUNNER_CONTEXT_ACTIONS,
  RUNNER_MENU_LONG_PRESS_MS,
  applyRunnerContextAction,
  openRunnerContextMenuFromEvent,
  runnerLongPressShouldArm,
  scheduleRunnerLongPress,
} from '../src/chat/runnerMenu.ts';

const RUNNER_A_ORIGIN = 'https://runner-a.example';
const RUNNER_A_AUTHORITY = 'runner-a.example';
const RUNNER_B_ORIGIN = 'https://runner-b.example';
const AUTH_MARKER_A = 'auth-marker-a';
const AUTH_MARKER_B = 'auth-marker-b';
const SAFE_CWD_A = '<workspace>/runner-a';

function incarnationOf(sessionId: string): string {
  return `inc-${sessionId}`;
}

function stateOf(sessionId: string, extra: Record<string, unknown> = {}): Record<string, unknown> {
  return { sessionId, runtimeIncarnation: incarnationOf(sessionId), ...extra };
}

function loadedRow(sessionId: string, extra: Record<string, unknown> = {}): Record<string, unknown> {
  return { sessionId, loaded: true, runtimeIncarnation: incarnationOf(sessionId), ...extra };
}

function snapshotFrame(
  sessionId: string,
  watermark: number,
  extra: Record<string, unknown> = {},
): Record<string, unknown> {
  const type = extra.type === 'session_resync' ? 'session_resync' : 'subscription_snapshot';
  const reason = typeof extra.reason === 'string'
    ? extra.reason
    : type === 'session_resync' ? 'resync' : 'cold-full';
  const frame: Record<string, unknown> = {
    type,
    reason,
    sessionId,
    runtimeIncarnation: typeof extra.runtimeIncarnation === 'string' ? extra.runtimeIncarnation : incarnationOf(sessionId),
    watermark,
    state: extra.state ?? {},
    events: extra.events ?? [],
  };
  frame.messages = extra.messages ?? [];
  return frame;
}

function unloadFrame(
  sessionId: string,
  extra: Record<string, unknown> = {},
): Record<string, unknown> {
  return {
    type: 'session_unloaded',
    sessionId,
    runtimeIncarnation: typeof extra.runtimeIncarnation === 'string' ? extra.runtimeIncarnation : incarnationOf(sessionId),
    reason: extra.reason === 'closed' ? 'closed' : 'lru',
  };
}

function eventType(frame: unknown): string {
  if (!frame || typeof frame !== 'object' || !('type' in frame) || typeof frame.type !== 'string') return '';
  return frame.type;
}

function eventIdentity(frame: unknown): { sessionId: string; runtimeIncarnation: string } | null {
  if (!frame || typeof frame !== 'object') return null;
  const sessionId = 'sessionId' in frame && typeof frame.sessionId === 'string' ? frame.sessionId : '';
  const runtimeIncarnation = 'runtimeIncarnation' in frame && typeof frame.runtimeIncarnation === 'string'
    ? frame.runtimeIncarnation
    : '';
  if (!sessionId || !runtimeIncarnation) return null;
  return { sessionId, runtimeIncarnation };
}

const failures: string[] = [];
let assertions = 0;
let activeBlock = 'module initialization';

function check(name: string, condition: boolean, detail = ''): void {
  assertions += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
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

function unavailableStorage(): StorageLike {
  return {
    getItem: () => null,
    setItem: () => { throw new Error('storage unavailable marker'); },
    removeItem: () => {},
  };
}

function failNextSetStorage(): StorageLike & { values: Map<string, string>; failNextSet: () => void } {
  const values = new Map<string, string>();
  let shouldFail = false;
  return {
    values,
    failNextSet: () => { shouldFail = true; },
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => {
      if (shouldFail) {
        shouldFail = false;
        throw new Error('storage unavailable marker');
      }
      values.set(key, value);
    },
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
  closeCode = 0;
  closeReason = '';

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

  respondLast(command: string, success: boolean, data?: unknown, error?: string, envelopeSessionId?: string): void {
    const sent = this.sent.map((frame) => JSON.parse(frame) as { id?: string; type?: string; sessionId?: string })
      .filter((frame) => frame.type === command)
      .at(-1);
    if (!sent?.id) throw new Error(`missing ${command} command on ${this.url}`);
    const bootstrapSessionId = command === 'get_state'
      && data && typeof data === 'object'
      && typeof (data as { sessionId?: unknown }).sessionId === 'string'
      ? (data as { sessionId: string }).sessionId
      : '';
    const sessionId = envelopeSessionId || sent.sessionId || bootstrapSessionId;
    this.onmessage?.({
      target: this,
      data: JSON.stringify({
        type: 'response',
        id: sent.id,
        command,
        ...(sessionId ? { sessionId } : {}),
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
    this.closeCode = code;
    this.closeReason = reason;
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
function okStatus(tokenRequired = false, origin?: string): Response {
  return {
    ok: true,
    json: async () => ({
      tokenRequired,
      version: '0.2.11',
      codename: origin ? `code-${origin}` : null,
    }),
  } as Response;
}

function defaultFetch(): typeof fetch {
  return (async (input: RequestInfo | URL) => {
    const url = String(input);
    if (!url.endsWith('/status')) throw new Error(`unexpected fetch ${url}`);
    return okStatus(false, url);
  }) as typeof fetch;
}

function statusFetch(handler: (url: string) => Response | Promise<Response> | Error): typeof fetch {
  return (async (input: RequestInfo | URL) => {
    const url = String(input);
    if (!url.endsWith('/status')) throw new Error(`unexpected fetch ${url}`);
    const result = await handler(url);
    if (result instanceof Error) throw result;
    return result;
  }) as typeof fetch;
}


function seededRecord(): RunnerRecord {
  return {
    id: 'seeded-runner',
    label: 'Seeded runner',
    origin: RUNNER_A_ORIGIN,
    authority: RUNNER_A_AUTHORITY,
    createdAt: 10,
    updatedAt: 10,
    codename: null,
  };
}

async function flush(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
}

type WakeHandler = () => void;

function installWakePage(): {
  document: { visibilityState: 'visible' | 'hidden' };
  dispatch: (type: 'visibilitychange' | 'focus' | 'pageshow') => void;
  restore: () => void;
} {
  const documentListeners = new Map<string, Set<WakeHandler>>();
  const windowListeners = new Map<string, Set<WakeHandler>>();
  const listen = (map: Map<string, Set<WakeHandler>>, type: string, handler: WakeHandler) => {
    const set = map.get(type) ?? new Set<WakeHandler>();
    set.add(handler);
    map.set(type, set);
  };
  const unlisten = (map: Map<string, Set<WakeHandler>>, type: string, handler: WakeHandler) => {
    map.get(type)?.delete(handler);
  };
  const fakeDocument = {
    visibilityState: 'visible' as 'visible' | 'hidden',
    addEventListener(type: string, handler: WakeHandler) { listen(documentListeners, type, handler); },
    removeEventListener(type: string, handler: WakeHandler) { unlisten(documentListeners, type, handler); },
  };
  const fakeWindow = {
    addEventListener(type: string, handler: WakeHandler) { listen(windowListeners, type, handler); },
    removeEventListener(type: string, handler: WakeHandler) { unlisten(windowListeners, type, handler); },
  };
  const previousDocument = Object.getOwnPropertyDescriptor(globalThis, 'document');
  const previousWindow = Object.getOwnPropertyDescriptor(globalThis, 'window');
  Object.defineProperty(globalThis, 'document', { configurable: true, value: fakeDocument });
  Object.defineProperty(globalThis, 'window', { configurable: true, value: fakeWindow });
  return {
    document: fakeDocument,
    dispatch(type) {
      const handlers = type === 'visibilitychange' ? documentListeners.get(type) : windowListeners.get(type);
      for (const handler of handlers ?? []) handler();
    },
    restore() {
      if (previousDocument) Object.defineProperty(globalThis, 'document', previousDocument);
      else delete (globalThis as { document?: unknown }).document;
      if (previousWindow) Object.defineProperty(globalThis, 'window', previousWindow);
      else delete (globalThis as { window?: unknown }).window;
    },
  };
}

async function waitForCommand(socket: FakeWebSocket, command: string): Promise<void> {
  for (let attempt = 0; attempt < 20; attempt += 1) {
    if (socket.sent.some((frame) => (JSON.parse(frame) as { type?: string }).type === command)) return;
    await flush();
  }
  throw new Error(`missing ${command} command on ${socket.url}`);
}

async function waitForFrameCount(socket: FakeWebSocket, type: string, count: number): Promise<void> {
  for (let attempt = 0; attempt < 40; attempt += 1) {
    if (commandFrames(socket, type).length >= count) return;
    await flush();
  }
  throw new Error(`missing ${type} frame #${count} on ${socket.url}`);
}

async function rejection(promise: Promise<unknown>): Promise<string> {
  const error = await rejectedError(promise);
  return error instanceof Error ? error.message : error === undefined ? '' : String(error);
}

async function rejectedError(promise: Promise<unknown>): Promise<unknown> {
  try {
    await promise;
    return undefined;
  } catch (error) {
    return error;
  }
}

function sentTypes(socket: FakeWebSocket): string[] {
  return socket.sent.map((frame) => (JSON.parse(frame) as { type?: string }).type ?? '');
}

function subscribeFrames(socket: FakeWebSocket): Array<{ type: string; sessionId?: string; runtimeIncarnation?: string; cursor?: unknown }> {
  return socket.sent
    .map((frame) => JSON.parse(frame) as { type: string; sessionId?: string; runtimeIncarnation?: string; cursor?: unknown })
    .filter((frame) => frame.type === 'subscribe');
}

function commandFrames<T extends { type?: string }>(socket: FakeWebSocket, type: string): T[] {
  return socket.sent
    .map((raw) => JSON.parse(raw) as T)
    .filter((frame) => frame.type === type);
}

async function main(): Promise<void> {
  installWebSocketConstants();
  activeBlock = 'address parser';

  {
    const endpoint = parseRunnerEndpoint(`${RUNNER_A_ORIGIN}/web`);
    check('address parser stores origin', endpoint.origin === RUNNER_A_ORIGIN, endpoint.origin);
    check('address parser derives secure websocket URL', endpoint.webSocketUrl === 'wss://runner-a.example/ws', endpoint.webSocketUrl);
    let rejected = false;
    try { parseRunnerEndpoint('https://name:marker@runner-a.example'); } catch { rejected = true; }
    check('address parser rejects embedded user info', rejected);
    let fragmentRejected = false;
    try { parseRunnerEndpoint(`${RUNNER_A_ORIGIN}/chat#secret`); } catch { fragmentRejected = true; }
    check('address parser rejects fragment', fragmentRejected);
    let queryRejected = false;
    try { parseRunnerEndpoint(`${RUNNER_A_ORIGIN}/chat?x=1`); } catch { queryRejected = true; }
    check('address parser rejects query', queryRejected);
    const httpsBare = parseRunnerEndpoint('192.0.2.15:8765', 'https:');
    check('bare address inherits https page protocol', httpsBare.origin === 'https://192.0.2.15:8765', httpsBare.origin);
    check('bare https address uses wss', httpsBare.webSocketUrl === 'wss://192.0.2.15:8765/ws', httpsBare.webSocketUrl);
    const httpBare = parseRunnerEndpoint('192.0.2.15:8765', 'http:');
    check('bare address inherits http page protocol', httpBare.origin === 'http://192.0.2.15:8765', httpBare.origin);
    const explicitHttp = parseRunnerEndpoint('http://192.0.2.15:8765', 'https:');
    check('explicit http is not rewritten by https page', explicitHttp.origin === 'http://192.0.2.15:8765', explicitHttp.origin);
    let portlessHostRejected = false;
    try { parseRunnerEndpoint('not-a-runner', 'https:'); } catch { portlessHostRejected = true; }
    check('address parser rejects a portless single-label host', portlessHostRejected);
    const parsedStatus = readRunnerStatus({ tokenRequired: false, version: '0.2.11', codename: null });
    check('status reader accepts null codename', parsedStatus?.tokenRequired === false && parsedStatus.codename === null);
    check('status reader rejects malformed body', readRunnerStatus({ tokenRequired: 'yes', version: '0.2.11' }) === null);
  }

  {
    activeBlock = 'invite link parser';
    const hashed = parseRunnerInviteLink(`${RUNNER_A_ORIGIN}/chat#secret`);
    check('invite link stores origin without hash', hashed.endpoint.origin === RUNNER_A_ORIGIN, hashed.endpoint.origin);
    check('invite link derives wss /ws', hashed.endpoint.webSocketUrl === 'wss://runner-a.example/ws', hashed.endpoint.webSocketUrl);
    check('invite link reads hash token', hashed.token === 'secret');
    const webHashed = parseRunnerInviteLink(`${RUNNER_A_ORIGIN}/web#secret`);
    check('invite /web hash uses the same origin and token', webHashed.endpoint.origin === RUNNER_A_ORIGIN && webHashed.token === 'secret');
    const originHashed = parseRunnerInviteLink(`${RUNNER_A_ORIGIN}#secret`);
    check('invite origin hash uses the same origin and token', originHashed.endpoint.origin === RUNNER_A_ORIGIN && originHashed.token === 'secret');
    const noHash = parseRunnerInviteLink(RUNNER_A_ORIGIN);
    check('invite origin without hash has empty token', noHash.endpoint.origin === RUNNER_A_ORIGIN && noHash.token === '');
    let emptyInvite = '';
    let emptyEndpoint = '';
    try { parseRunnerInviteLink('  '); } catch (error) { emptyInvite = error instanceof Error ? error.message : ''; }
    try { parseRunnerEndpoint('  '); } catch (error) { emptyEndpoint = error instanceof Error ? error.message : ''; }
    check('empty invite link uses the endpoint empty error', emptyInvite === emptyEndpoint && emptyInvite === 'Runner address is required.');
    let inviteUserinfo = false;
    try { parseRunnerInviteLink('https://name:marker@runner-a.example/chat#secret'); } catch { inviteUserinfo = true; }
    check('invite link still rejects userinfo', inviteUserinfo);
    let inviteQuery = false;
    try { parseRunnerInviteLink(`${RUNNER_A_ORIGIN}/chat?x=1#secret`); } catch { inviteQuery = true; }
    check('invite link still rejects query', inviteQuery);
    let inviteWs = false;
    try { parseRunnerInviteLink('wss://runner-a.example/ws#secret'); } catch { inviteWs = true; }
    check('invite link rejects wss transport URLs', inviteWs);
    let inviteWsPlain = false;
    try { parseRunnerInviteLink('ws://runner-a.example/ws#secret'); } catch { inviteWsPlain = true; }
    check('invite link rejects ws transport URLs', inviteWsPlain);
  }

  {
    activeBlock = 'invite dialog paste';
    const invite = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../src/chat/InviteRunnerDialog.tsx'), 'utf8');
    check('invite heading is Add runner', invite.includes('<h3>Add runner</h3>') && !invite.includes('<h3>Add new address</h3>'));
    check('invite field is Runner link', invite.includes('Runner link') && invite.includes('id="chat-runner-address"'));
    check('invite placeholder is a chat hash link', invite.includes('placeholder="https://192.0.2.10:8765/chat#token"'));
    check('submitAdd parses invite links', invite.includes('parseRunnerInviteLink(address)')
      && invite.includes('address: parsed.endpoint.origin')
      && invite.includes('token: token.trim() || parsed.token'));
    check('LAN Add stays on lanDiscoverAddPayload', invite.includes('lanDiscoverAddPayload')
      && invite.includes('onAddAndInvite(lanDiscoverAddPayload(candidate))'));
  }

  {
    activeBlock = 'record persistence';
    const store = storage();
    check('save runner records succeeds', saveRunnerRecords(store, [seededRecord()]));
    check('load runner records restores versioned data', loadRunnerRecords(store)[0]?.id === 'seeded-runner');
    const raw = store.values.get(RUNNER_STORAGE_KEY) ?? '';
    check('durable runner JSON contains no token field', !raw.toLowerCase().includes('token'), raw);
  }

  {
    activeBlock = 'v1 to v2 migration';
    const store = storage();
    const v1Record = {
      id: 'v1-runner',
      label: 'V1 runner',
      origin: RUNNER_A_ORIGIN,
      authority: RUNNER_A_AUTHORITY,
      createdAt: 5,
      updatedAt: 6,
    };
    store.setItem(RUNNER_STORAGE_KEY, JSON.stringify({ version: 1, runners: [v1Record] }));
    const loaded = loadRunnerRecords(store);
    check('v1 record migrates', loaded.length === 1 && loaded[0]?.id === 'v1-runner', String(loaded.length));
    check('migrated record seeds codename null', loaded[0]?.codename === null, String(loaded[0]?.codename));
    check('migrated record keeps label', loaded[0]?.label === 'V1 runner', loaded[0]?.label ?? '');
    check('migrated record keeps authority', loaded[0]?.authority === RUNNER_A_AUTHORITY, loaded[0]?.authority ?? '');
    const persisted = JSON.parse(store.values.get(RUNNER_STORAGE_KEY) ?? '{}') as { version?: unknown; runners?: unknown[] };
    check('migration rewrites the same key to version 2', persisted.version === 2, String(persisted.version));
    check('migration persists codename null', (persisted.runners?.[0] as { codename?: unknown } | undefined)?.codename === null);
    check('migration keeps the same storage key', store.values.has(RUNNER_STORAGE_KEY));
  }

  {
    activeBlock = 'v1 migration with malformed rows';
    const store = storage();
    store.setItem(RUNNER_STORAGE_KEY, JSON.stringify({
      version: 1,
      runners: [
        { id: 'good', label: 'Good', origin: RUNNER_A_ORIGIN, authority: RUNNER_A_AUTHORITY, createdAt: 1, updatedAt: 1 },
        { id: '', label: 'Bad id', origin: RUNNER_A_ORIGIN, authority: RUNNER_A_AUTHORITY, createdAt: 1, updatedAt: 1 },
        { id: 'no-label', origin: RUNNER_A_ORIGIN, authority: RUNNER_A_AUTHORITY, createdAt: 1, updatedAt: 1 },
      ],
    }));
    const loaded = loadRunnerRecords(store);
    check('malformed v1 rows are dropped', loaded.length === 1 && loaded[0]?.id === 'good', String(loaded.length));
    check('surviving v1 row seeds codename null', loaded[0]?.codename === null, String(loaded[0]?.codename));
  }

  {
    activeBlock = 'invalid v2 document';
    const store = storage();
    store.setItem(RUNNER_STORAGE_KEY, JSON.stringify({ version: 2, runners: 'not-an-array' }));
    check('v2 with non-array runners loads nothing', loadRunnerRecords(store).length === 0);
    store.setItem(RUNNER_STORAGE_KEY, JSON.stringify({ version: 3, runners: [seededRecord()] }));
    check('unknown future version loads nothing', loadRunnerRecords(store).length === 0);
    store.setItem(RUNNER_STORAGE_KEY, JSON.stringify({ version: 2, runners: [{ ...seededRecord(), codename: 42 }] }));
    check('v2 record with non-string codename is rejected', loadRunnerRecords(store).length === 0);
    store.setItem(RUNNER_STORAGE_KEY, JSON.stringify({ version: 2, runners: [{ ...seededRecord(), codename: 'alpha-one' }] }));
    const named = loadRunnerRecords(store);
    check('v2 record with string codename loads', named.length === 1 && named[0]?.codename === 'alpha-one', String(named[0]?.codename));
    store.setItem(RUNNER_STORAGE_KEY, 'not-json');
    check('corrupt storage loads nothing', loadRunnerRecords(store).length === 0);
  }

  {
    activeBlock = 'display name fallback';
    check(
      'codename wins over label and authority',
      runnerDisplayName({ codename: 'alpha-one', label: 'My box', authority: 'host:8765' }) === 'alpha-one',
    );
    check(
      'label wins when codename is null',
      runnerDisplayName({ codename: null, label: 'My box', authority: 'host:8765' }) === 'My box',
    );
    check(
      'authority hostname is the last fallback',
      runnerDisplayName({ codename: null, label: '   ', authority: 'host:8765' }) === 'host',
    );
    check(
      'blank codename falls back to label',
      runnerDisplayName({ codename: '   ', label: 'My box', authority: 'host:8765' }) === 'My box',
    );
    check(
      'listen-port suffix is display-only',
      runnerDisplayName({ codename: 'x3-8765', label: 'box', authority: 'host:8765' }) === 'x3',
    );
    check(
      'mention identity strips the same listen-port suffix',
      runnerIdentityName({ codename: 'x3-8765', label: 'box', authority: 'host:8765' }) === 'x3'
      && runnerIdentityName({ codename: 'psy-8765', label: 'box', authority: 'host:8765' }) === 'psy',
    );
    const listenA = { authority: '127.0.0.1:37367', codename: 'x3-37367', label: 'Runner A' };
    const listenB = { authority: '127.0.0.1:35445', codename: 'x3-35445', label: 'Runner B' };
    const lonePsy = { authority: 'host:8765', codename: 'psy-8765', label: 'box' };
    check(
      'solo mention keeps the stripped display name',
      runnerMentionName(lonePsy, [lonePsy]) === 'psy',
    );
    check(
      'clashing co-hosted mentions keep the unstripped codename',
      runnerMentionName(listenA, [listenA, listenB]) === 'x3-37367'
      && runnerMentionName(listenB, [listenA, listenB]) === 'x3-35445',
    );
    check(
      'port 65536 is outside 1024-65535 and stays on the name',
      runnerDisplayName({ codename: 'x-65536', label: 'box', authority: 'host:8765' }) === 'x-65536',
    );

  }

  {
    activeBlock = 'duplicate display name helper';
    const records: RunnerRecord[] = [
      { ...seededRecord(), id: 'a', label: 'Runner A', authority: 'host-a', codename: null },
      { ...seededRecord(), id: 'b', label: 'Runner B', authority: 'host-b', codename: 'alpha-one' },
      { ...seededRecord(), id: 'c', label: 'Runner A', authority: 'host-c', codename: null },
    ];
    check(
      'label duplicate returns the secondary authority',
      runnerDuplicateAuthority(records, 'Runner A') === 'host-a',
      runnerDuplicateAuthority(records, 'Runner A') ?? '',
    );
    check(
      'codename duplicate returns the secondary authority',
      runnerDuplicateAuthority(records, 'alpha-one') === 'host-b',
      runnerDuplicateAuthority(records, 'alpha-one') ?? '',
    );
    check(
      'unique name returns null',
      runnerDuplicateAuthority(records, 'Runner C') === null,
      runnerDuplicateAuthority(records, 'Runner C') ?? '',
    );
    check(
      'excluded id skips self and finds the other duplicate',
      runnerDuplicateAuthority(records, 'Runner A', 'a') === 'host-c',
      runnerDuplicateAuthority(records, 'Runner A', 'a') ?? '',
    );
    check(
      'blank name returns null',
      runnerDuplicateAuthority(records, '   ') === null,
      runnerDuplicateAuthority(records, '   ') ?? '',
    );
  }

  {
    activeBlock = 'runner identity dedupe';
    const offlinePsy: RunnerRecord & { state: RunnerConnectionState } = {
      id: 'offline-psy',
      label: 'psy',
      origin: 'https://host.example:8765',
      authority: 'host.example:8765',
      createdAt: 1,
      updatedAt: 1,
      codename: 'psy',
      state: 'offline',
    };
    const amberPsy: RunnerRecord & { state: RunnerConnectionState } = {
      id: 'amber-psy',
      label: 'psy',
      origin: 'https://192.0.2.15:8765',
      authority: '192.0.2.15:8765',
      createdAt: 2,
      updatedAt: 2,
      codename: 'psy',
      state: 'reconnecting',
    };
    const connectedPsy: RunnerRecord & { state: RunnerConnectionState } = {
      ...offlinePsy,
      id: 'live-psy',
      updatedAt: 3,
      state: 'connected',
    };
    check('same authority is one identity', sameRunnerIdentity(offlinePsy, { ...offlinePsy, id: 'other' }));
    check('same name and listen port is one identity', sameRunnerIdentity(offlinePsy, amberPsy));
    const redPsy: RunnerRecord & { state: RunnerConnectionState } = {
      id: 'red-psy',
      label: 'psy',
      origin: 'https://192.0.2.15:8765',
      authority: '192.0.2.15:8765',
      createdAt: 1,
      updatedAt: 1,
      codename: 'psy',
      state: 'token_required',
    };
    const amberPsyOtherPort: RunnerRecord & { state: RunnerConnectionState } = {
      id: 'amber-psy-lan',
      label: 'psy',
      origin: 'https://host-b.example',
      authority: 'host-b.example',
      createdAt: 2,
      updatedAt: 2,
      codename: 'psy',
      state: 'reconnecting',
    };
    check('same display name is one identity regardless of authority port',
      sameRunnerIdentity(redPsy, amberPsyOtherPort));
    const photoPair = dedupeRunnersKeepHealthiest([redPsy, amberPsyOtherPort]);
    check('token_required + reconnecting psy is one rail row',
      photoPair.length === 1 && photoPair[0]?.id === 'amber-psy-lan' && photoPair[0]?.state === 'reconnecting',
      photoPair.map((row) => `${row.id}:${row.state}`).join(','));
    check('reconnecting ranks above token_required',
      runnerHealthRank('reconnecting') > runnerHealthRank('token_required'));
    check('different names stay distinct', !sameRunnerIdentity(
      { authority: 'a:8765', codename: 'alpha', label: 'alpha' },
      { authority: 'b:8765', codename: 'beta', label: 'beta' },
    ));
    check('connected ranks above reconnecting', runnerHealthRank('connected') > runnerHealthRank('reconnecting'));
    check('reconnecting ranks above offline', runnerHealthRank('reconnecting') > runnerHealthRank('offline'));
    const byAuthority = dedupeRunnersKeepHealthiest([offlinePsy, connectedPsy]);
    check('same-authority dedupe keeps the connected record',
      byAuthority.length === 1 && byAuthority[0]?.id === 'live-psy',
      byAuthority.map((row) => row.id).join(','));
    const byIdentity = dedupeRunnersKeepHealthiest([offlinePsy, amberPsy]);
    check('restart duplicate keeps the reconnecting row',
      byIdentity.length === 1 && byIdentity[0]?.id === 'amber-psy',
      byIdentity.map((row) => `${row.id}:${row.state}`).join(','));
    const degenerate = dedupeRunnersKeepHealthiest([
      { ...connectedPsy, authority: '' },
      connectedPsy,
    ]);
    check('empty-authority record collapses onto the named runner',
      degenerate.length === 1 && degenerate[0]?.id === 'live-psy',
      degenerate.map((row) => `${row.id}:${row.authority || '(empty)'}`).join(','));
    check('empty authority matches the named runner', sameRunnerIdentity(
      { authority: '', codename: null, label: 'psy' },
      { authority: '192.0.2.15:8765', codename: null, label: 'psy' },
    ));
    check('empty authority does not match a different name', !sameRunnerIdentity(
      { authority: '', codename: null, label: 'psy' },
      { authority: '192.0.2.15:8765', codename: null, label: 'other' },
    ));
    const distinct = dedupeRunnersKeepHealthiest([
      { ...offlinePsy, id: 'alpha', label: 'alpha', authority: 'alpha:8765', codename: 'alpha' },
      { ...amberPsy, id: 'beta', label: 'beta', authority: 'beta:8765', codename: 'beta' },
    ]);
    check('distinct identities stay two rows', distinct.length === 2, String(distinct.length));
    check('co-hosted listen-port suffixes stay distinct identities', !sameRunnerIdentity(
      { authority: '127.0.0.1:37367', codename: 'x3-37367', label: 'Runner A' },
      { authority: '127.0.0.1:35445', codename: 'x3-35445', label: 'Runner B' },
    ));
    const cohosted = dedupeRunnersKeepHealthiest([
      {
        ...connectedPsy,
        id: 'listen-a',
        label: 'Runner A',
        origin: 'http://127.0.0.1:37367',
        authority: '127.0.0.1:37367',
        createdAt: 1,
        updatedAt: 1,
        codename: 'x3-37367',
        state: 'connected' as const,
      },
      {
        ...connectedPsy,
        id: 'listen-b',
        label: 'Runner B',
        origin: 'http://127.0.0.1:35445',
        authority: '127.0.0.1:35445',
        createdAt: 2,
        updatedAt: 2,
        codename: 'x3-35445',
        state: 'connected' as const,
      },
    ]);
    check('co-hosted listeners stay two rail rows',
      cohosted.length === 2
      && cohosted.some((row) => row.id === 'listen-a')
      && cohosted.some((row) => row.id === 'listen-b'),
      cohosted.map((row) => row.id).join(','));
    const photoOrder = sortRunnersOnlineFirst([
      { id: 'warn-a', createdAt: 3, state: 'reconnecting' as const },
      { id: 'online-b', createdAt: 6, state: 'connected' as const },
      { id: 'online-a', createdAt: 1, state: 'connected' as const },
      { id: 'warn-b', createdAt: 4, state: 'reconnecting' as const },
      { id: 'warn-c', createdAt: 5, state: 'reconnecting' as const },
      { id: 'online-c', createdAt: 7, state: 'connected' as const },
    ]);
    check('online runners sort ahead of reconnecting',
      photoOrder.map((row) => row.id).join(',') === 'online-a,online-b,online-c,warn-a,warn-b,warn-c',
      photoOrder.map((row) => row.id).join(','));
    check('same health keeps createdAt order',
      photoOrder.filter((row) => row.state === 'connected').map((row) => row.id).join(',') === 'online-a,online-b,online-c');
    const store = storage();
    saveRunnerRecords(store, [
      { ...offlinePsy },
      { ...offlinePsy, id: 'stale-psy', updatedAt: 0 },
    ]);
    const loaded = loadRunnerRecords(store);
    check('load collapses same-authority records to the newest',
      loaded.length === 1 && loaded[0]?.id === 'offline-psy',
      loaded.map((row) => row.id).join(','));
    const registry = new RunnerRegistry({
      storage: store,
      fetchFn: defaultFetch(),
      webSocketFactory: () => new FakeWebSocket('wss://host.example:8765/ws', []) as unknown as WebSocket,
    });
    check('registry snapshot is one row after load collapse',
      registry.snapshot().length === 1 && registry.snapshot()[0]?.id === 'offline-psy',
      registry.snapshot().map((row) => row.id).join(','));
  }

  {
    activeBlock = 'storage unavailable';
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
      storage: unavailableStorage(),
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      randomBytes: deterministicBytes,
    });
    const error = await rejection(registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A));
    await flush();
    check('unavailable durable storage rejects add', error === 'Runner registry storage is unavailable.', error);
    check('unavailable durable storage opens no socket', sockets.length === 0, String(sockets.length));
    check('unavailable durable storage retains no in-memory runner', registry.snapshot().length === 0);
  }

  {
    activeBlock = 'successful lifecycle';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    check('add opens exactly one socket', sockets.length === 1, String(sockets.length));
    check('token stored only in authority bucket', store.values.get(hostTokenKey(RUNNER_A_AUTHORITY)) === AUTH_MARKER_A);
    check('persisted metadata excludes auth marker', !(store.values.get(RUNNER_STORAGE_KEY) ?? '').includes(AUTH_MARKER_A));

    sockets[0].open();
    check('probe starts with get_state', JSON.parse(sockets[0].sent[0] ?? '{}').type === 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    check('probe continues with session_list', sockets[0].sent.some((frame) => JSON.parse(frame).type === 'session_list'));
    sockets[0].respond('session_list', true, { sessions: [loadedRow('session-a')] });
    const connected = await added;
    check('successful add settles connected', connected.state === 'connected', connected.state);
    check('successful probe records primary session', connected.probe?.sessionId === 'session-a');
    check('successful probe records session count', connected.probe?.sessionCount === 1);
    check('successful probe publishes a positive connection epoch', connected.connectionEpoch > 0, String(connected.connectionEpoch));
    check('probe wire is get_state then session_list', sentTypes(sockets[0]).join(',') === 'get_state,session_list');
    check('probe does not send subscribe', subscribeFrames(sockets[0]).length === 0);

    sockets[0].close(1006, '');
    check('transport drop enters reconnecting', registry.snapshot()[0]?.state === 'reconnecting');
    check('transport drop retains the last authoritative epoch', registry.snapshot()[0]?.connectionEpoch === connected.connectionEpoch, String(registry.snapshot()[0]?.connectionEpoch));
    check('first reconnect delay is bounded initial delay', [...fakeTimers.scheduled.values()][0]?.delay === RUNNER_RECONNECT_INITIAL_MS);
    check('reconnect timer creates replacement socket', fakeTimers.runDelay(RUNNER_RECONNECT_INITIAL_MS));
    await flush();
    check('reconnect opened a second socket', sockets.length === 2, String(sockets.length));
    sockets[1].open();
    await waitForCommand(sockets[1], 'get_state');
    sockets[1].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[1].respond('session_list', true, { sessions: [loadedRow('session-a')] });
    await flush();
    const reconnectedEpoch = registry.snapshot()[0]?.connectionEpoch ?? 0;
    check('recovered socket publishes a greater authoritative epoch', reconnectedEpoch > connected.connectionEpoch, `${connected.connectionEpoch}->${reconnectedEpoch}`);

    const retokened = registry.updateToken(connected.id, AUTH_MARKER_B);
    await flush();
    check('credential replacement retains the prior authoritative epoch while probing', registry.snapshot()[0]?.connectionEpoch === reconnectedEpoch, String(registry.snapshot()[0]?.connectionEpoch));
    check('credential replacement opened a third socket', sockets.length === 3, String(sockets.length));
    sockets[2].open();
    await waitForCommand(sockets[2], 'get_state');
    sockets[2].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[2].respond('session_list', true, { sessions: [loadedRow('session-a')] });
    const retokenedView = await retokened;
    check('credential replacement publishes a greater authoritative epoch', retokenedView.connectionEpoch > reconnectedEpoch, `${reconnectedEpoch}->${retokenedView.connectionEpoch}`);

    registry.delete(connected.id);
    check('delete removes runner record', registry.snapshot().length === 0);
    check('delete removes scoped token', !store.values.has(hostTokenKey(RUNNER_A_AUTHORITY)));
    check('delete closes current socket', sockets[2].closed);
    check('delete cancels reconnect timers', fakeTimers.scheduled.size === 0, String(fakeTimers.scheduled.size));
    check('delete persists an empty registry', loadRunnerRecords(store).length === 0);
  }

  {
    activeBlock = 'target-owned lifecycle responses';
    // Backend contract (session_runtime_manager.rs snapshot_response): a
    // successful new_session/switch_session envelope is stamped with the
    // OPENED session's id (never the source the command was sent from), and
    // data.sessionId/data.state.sessionId must agree exactly. Failures stay
    // request-source-owned and ordinary commands never change ownership.
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [loadedRow('session-a')] });
    const connected = await added;

    const SOURCE = 'session-a';
    const respondToLast = (type: string, envelope: Record<string, unknown>): void => {
      const sent = commandFrames<{ id?: string }>(sockets[0], type).at(-1);
      if (!sent?.id) throw new Error(`no ${type} frame on the socket`);
      sockets[0].emit({ type: 'response', id: sent.id, command: type, ...envelope });
    };

    // The first lifecycle command must wait for its subscription snapshot.
    const pendingCreate = registry.command(connected.id, { type: 'new_session', cwd: SAFE_CWD_A }, {
      sourceSessionId: 'session-a',
    });
    await flush();
    sockets[0].emit(snapshotFrame('session-a', 0));
    await waitForCommand(sockets[0], 'new_session');

    const createRequest = commandFrames<{ id?: string; sessionId?: string }>(sockets[0], 'new_session')[0];
    sockets[0].emit({
      type: 'response',
      id: createRequest.id,
      command: 'new_session',
      // Target-owned: the envelope carries the OPENED session, never the source.
      sessionId: 'fresh-target',
      success: true,
      data: {
        sessionId: 'fresh-target',
        runtimeIncarnation: incarnationOf('fresh-target'),
        state: { sessionId: 'fresh-target', sessionFile: '/store/fresh.jsonl', cwd: SAFE_CWD_A },
        session: {
          source: 'rpi',
          sessionId: 'fresh-target',
          runtimeIncarnation: incarnationOf('fresh-target'),
          path: '/store/fresh.jsonl',
          cwd: SAFE_CWD_A,
          status: 'native',
          loaded: true,
        },
        messages: [],
      },
    });
    const created = await pendingCreate;
    check('target-owned new_session envelope resolves', created.success === true && created.sessionId === 'fresh-target', JSON.stringify(created));
    check('target-owned success never needs the source envelope', created.sessionId !== 'session-a');

    // The learned target identity is immediately usable as a command source:
    // the registry subscribed the fresh identity from the lifecycle data.
    const pendingChildCommand = registry.command(connected.id, { type: 'get_messages' }, {
      sourceSessionId: 'fresh-target',
    });
    await flush();
    const targetSubscribe = subscribeFrames(sockets[0]).find((frame) => frame.sessionId === 'fresh-target');
    check('learned target identity is subscribable', targetSubscribe?.sessionId === 'fresh-target'
      && targetSubscribe?.runtimeIncarnation === incarnationOf('fresh-target'), JSON.stringify(subscribeFrames(sockets[0])));
    sockets[0].emit(snapshotFrame('fresh-target', 0));
    await flush();
    respondToLast('get_messages', { sessionId: 'fresh-target', success: true, data: { messages: [] } });
    await pendingChildCommand;

    // Disagreement between envelope, data, and state fails the success
    // closed: a target envelope that data contradicts can never satisfy the
    // source-owned fallback either.
    const mismatchedEnvelope = registry.command(connected.id, { type: 'new_session', cwd: '/workspace/other' }, {
      sourceSessionId: SOURCE,
    });
    // The command must actually hit the wire before respondToLast can pick
    // up ITS frame — otherwise the answer lands on a stale id and the
    // pending command can never settle.
    await flush();
    respondToLast('new_session', { sessionId: 'fresh-target', success: true, data: { sessionId: 'other-target', runtimeIncarnation: incarnationOf('other-target'), state: { sessionId: 'other-target', sessionFile: '/store/other.jsonl', cwd: '/workspace/fresh' }, session: { sessionId: 'other-target', runtimeIncarnation: incarnationOf('other-target') } } });
    check('data/state disagreement rejects a target envelope', await rejection(mismatchedEnvelope) === 'Runner response ownership did not match the request.');

    // Canonical identity incarnation: the exact nonempty runtimeIncarnation
    // must be present where the wire supplies it (data + the authoritative
    // data.session row, agreeing) even when every sessionId agrees.
    async function lifecycleRejection(data: Record<string, unknown>): Promise<string> {
      const pending = registry.command(connected.id, { type: 'new_session', cwd: '/workspace/incarnation' }, {
        sourceSessionId: SOURCE,
      });
      // The command must actually hit the wire before respondToLast can pick
      // up ITS frame; a stale-id answer would leave the pending unsettled.
      await flush();
      respondToLast('new_session', {
        sessionId: 'fresh-target',
        success: true,
        data,
      });
      return await rejection(pending);
    }
    const targetRow = {
      source: 'rpi',
      sessionId: 'fresh-target',
      runtimeIncarnation: incarnationOf('fresh-target'),
      path: '/store/fresh.jsonl',
      cwd: '/workspace/incarnation',
      status: 'native',
      loaded: true,
    };
    const rowDrift = (session: Record<string, unknown>): Record<string, unknown> => ({
      sessionId: 'fresh-target',
      state: { sessionId: 'fresh-target', sessionFile: '/store/fresh.jsonl', cwd: '/workspace/incarnation' },
      session,
      messages: [],
    });
    check('missing runtimeIncarnation rejects a target envelope',
      await lifecycleRejection({ sessionId: 'fresh-target', ...rowDrift(targetRow) })
        === 'Runner response ownership did not match the request.');
    check('empty runtimeIncarnation rejects a target envelope',
      await lifecycleRejection({ sessionId: 'fresh-target', runtimeIncarnation: '   ', ...rowDrift(targetRow) })
        === 'Runner response ownership did not match the request.');
    check('row incarnation disagreement rejects a target envelope',
      await lifecycleRejection({ sessionId: 'fresh-target', runtimeIncarnation: incarnationOf('fresh-target'), ...rowDrift({ sessionId: 'fresh-target', runtimeIncarnation: incarnationOf('other-target'), path: '/store/fresh.jsonl', cwd: '/workspace/incarnation', status: 'native', loaded: true }) })
        === 'Runner response ownership did not match the request.');
    check('row session disagreement rejects a target envelope',
      await lifecycleRejection({ sessionId: 'fresh-target', runtimeIncarnation: incarnationOf('fresh-target'), ...rowDrift({ sessionId: 'other-target', runtimeIncarnation: incarnationOf('fresh-target'), path: '/store/fresh.jsonl', cwd: '/workspace/incarnation', status: 'native', loaded: true }) })
        === 'Runner response ownership did not match the request.');
    check('missing session row rejects a target envelope',
      await lifecycleRejection({ sessionId: 'fresh-target', runtimeIncarnation: incarnationOf('fresh-target'), state: { sessionId: 'fresh-target', sessionFile: '/store/fresh.jsonl', cwd: '/workspace/incarnation' }, messages: [] })
        === 'Runner response ownership did not match the request.');
    check('row not loaded rejects a target envelope',
      await lifecycleRejection({ sessionId: 'fresh-target', runtimeIncarnation: incarnationOf('fresh-target'), state: { sessionId: 'fresh-target', sessionFile: '/store/fresh.jsonl', cwd: '/workspace/incarnation' }, session: { ...targetRow, loaded: false }, messages: [] })
        === 'Runner response ownership did not match the request.');
    check('non-native row status rejects a target envelope',
      await lifecycleRejection({ sessionId: 'fresh-target', runtimeIncarnation: incarnationOf('fresh-target'), ...rowDrift({ ...targetRow, status: 'foreign' }) })
        === 'Runner response ownership did not match the request.');
    // The canonical native source is UNCONDITIONAL: the backend always
    // stamps lifecycle targets `source: "rpi"`
    // (lifecycle_target_session_row), so a missing, null, empty, whitespace,
    // non-string, or foreign source fails the ownership gate closed.
    check('missing row source rejects a target envelope',
      await lifecycleRejection({ sessionId: 'fresh-target', runtimeIncarnation: incarnationOf('fresh-target'), ...rowDrift({ ...targetRow, source: undefined }) })
        === 'Runner response ownership did not match the request.');
    check('null row source rejects a target envelope',
      await lifecycleRejection({ sessionId: 'fresh-target', runtimeIncarnation: incarnationOf('fresh-target'), ...rowDrift({ ...targetRow, source: null }) })
        === 'Runner response ownership did not match the request.');
    check('empty row source rejects a target envelope',
      await lifecycleRejection({ sessionId: 'fresh-target', runtimeIncarnation: incarnationOf('fresh-target'), ...rowDrift({ ...targetRow, source: '' }) })
        === 'Runner response ownership did not match the request.');
    check('nonstring row source rejects a target envelope',
      await lifecycleRejection({ sessionId: 'fresh-target', runtimeIncarnation: incarnationOf('fresh-target'), ...rowDrift({ ...targetRow, source: 7 }) })
        === 'Runner response ownership did not match the request.');
    check('foreign row source rejects a target envelope',
      await lifecycleRejection({ sessionId: 'fresh-target', runtimeIncarnation: incarnationOf('fresh-target'), ...rowDrift({ ...targetRow, source: 'pi' }) })
        === 'Runner response ownership did not match the request.');
    check('padded row source rejects a target envelope',
      await lifecycleRejection({ sessionId: 'fresh-target', runtimeIncarnation: incarnationOf('fresh-target'), ...rowDrift({ ...targetRow, source: ' rpi ' }) })
        === 'Runner response ownership did not match the request.');
    check('row path drift from state rejects a target envelope',
      await lifecycleRejection({ sessionId: 'fresh-target', runtimeIncarnation: incarnationOf('fresh-target'), ...rowDrift({ ...targetRow, path: '/store/other.jsonl' }) })
        === 'Runner response ownership did not match the request.');

    const sourceEnvelope = registry.command(connected.id, { type: 'switch_session', sessionPath: '/store/x.jsonl' }, {
      sourceSessionId: SOURCE,
    });
    await flush();
    respondToLast('switch_session', {
      // Source-owned envelope on a lifecycle SUCCESS: inconsistent with the
      // backend contract, and its data claims a different target.
      sessionId: 'session-a',
      success: true,
      data: { sessionId: 'foreign', runtimeIncarnation: incarnationOf('foreign'), state: { sessionId: 'foreign', sessionFile: '/store/foreign.jsonl', cwd: '/workspace/foreign' } },
    });
    check('source envelope on lifecycle success is rejected', await rejection(sourceEnvelope) === 'Runner response ownership did not match the request.');

    const foreignOrdinary = registry.command(connected.id, { type: 'get_messages' }, {
      sourceSessionId: SOURCE,
    });
    await flush();
    respondToLast('get_messages', { sessionId: 'fresh-target', success: true, data: { messages: [] } });
    check('ordinary command keeps source-owned responses', await rejection(foreignOrdinary) === 'Runner response ownership did not match the request.');

    const failedCreate = registry.command(connected.id, { type: 'new_session', cwd: '/workspace/missing' }, {
      sourceSessionId: SOURCE,
    });
    await flush();
    // Failures stay request-source-owned: the bounded literal arrives on the
    // source envelope and rejects as a remote failure (never an ownership
    // protocol error).
    respondToLast('new_session', { sessionId: SOURCE, success: false, error: 'new session directory does not exist' });
    check('lifecycle failure stays source-owned remote', await rejection(failedCreate) === 'new session directory does not exist');
    registry.stop();
  }

  {
    activeBlock = 'probe adopts authenticated codename once';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    check('record starts with codename null', registry.snapshot()[0]?.codename === null, String(registry.snapshot()[0]?.codename));
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A, runnerCodename: 'alpha-one' }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [] });
    const connected = await added;
    check('probe exposes the authenticated codename', connected.probe?.runnerCodename === 'alpha-one', connected.probe?.runnerCodename ?? '');
    check('probe adoption updates the record codename', registry.snapshot()[0]?.codename === 'alpha-one', String(registry.snapshot()[0]?.codename));
    check('probe adoption persists the codename', loadRunnerRecords(store)[0]?.codename === 'alpha-one', String(loadRunnerRecords(store)[0]?.codename));

    sockets[0].close(1006, '');
    check('reconnect timer armed after adoption drop', fakeTimers.runDelay(RUNNER_RECONNECT_INITIAL_MS));
    await flush();
    sockets[1].open();
    await waitForCommand(sockets[1], 'get_state');
    sockets[1].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A, runnerCodename: 'alpha-one' }));
    await flush();
    sockets[1].respond('session_list', true, { sessions: [] });
    await flush();
    check('reconnect with the same codename does not rewrite storage', loadRunnerRecords(store)[0]?.codename === 'alpha-one');
    registry.stop();
  }

  {
    activeBlock = 'probe missing codename keeps cached';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A, runnerCodename: 'alpha-one' }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [] });
    await added;
    sockets[0].close(1006, '');
    check('reconnect timer armed after cached drop', fakeTimers.runDelay(RUNNER_RECONNECT_INITIAL_MS));
    await flush();
    sockets[1].open();
    await waitForCommand(sockets[1], 'get_state');
    sockets[1].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[1].respond('session_list', true, { sessions: [loadedRow('session-a'), loadedRow('child-a')] });
    await flush();
    check('missing codename keeps the cached record codename', registry.snapshot()[0]?.codename === 'alpha-one', String(registry.snapshot()[0]?.codename));
    check('missing codename keeps the persisted codename', loadRunnerRecords(store)[0]?.codename === 'alpha-one', String(loadRunnerRecords(store)[0]?.codename));
    registry.stop();
  }

  {
    activeBlock = 'duplicate persistence rollback';
    const store = failNextSetStorage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
      randomBytes: deterministicBytes,
    });
    const firstAdd = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [loadedRow('session-a'), loadedRow('child-a')] });
    await firstAdd;
    store.failNextSet();
    const error = await rejection(registry.add('Renamed runner', RUNNER_A_ORIGIN, AUTH_MARKER_B));
    await flush();
    check('duplicate storage failure rejects replacement', error === 'Runner registry storage is unavailable.', error);
    check('duplicate storage failure restores label', registry.snapshot()[0]?.label === 'Runner A');
    check('duplicate storage failure preserves scoped token', store.values.get(hostTokenKey(RUNNER_A_AUTHORITY)) === AUTH_MARKER_A);
    check('duplicate storage failure reconnects prior runner', sockets.length === 2, String(sockets.length));
  }

  {
    activeBlock = 'delete persistence rollback';
    const store = failNextSetStorage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
      randomBytes: deterministicBytes,
    });
    const firstAdd = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [loadedRow('session-a'), loadedRow('child-a')] });
    const connected = await firstAdd;
    store.failNextSet();
    let error = '';
    try { registry.delete(connected.id); } catch (cause) { error = cause instanceof Error ? cause.message : String(cause); }
    await flush();
    check('delete storage failure is visible', error === 'Runner registry storage is unavailable.', error);
    check('delete storage failure restores runner', registry.snapshot().length === 1);
    check('delete storage failure preserves scoped token', store.values.get(hostTokenKey(RUNNER_A_AUTHORITY)) === AUTH_MARKER_A);
    check('delete storage failure reconnects restored runner', sockets.length === 2, String(sockets.length));
  }

  {
    activeBlock = 'first auth failure recovery';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
      randomBytes: deterministicBytes,
    });
    const firstAdd = registry.add('Runner B', RUNNER_B_ORIGIN, AUTH_MARKER_B);
    await flush();
    sockets[0].close(1006, '');
    await flush();
    check('first 1006 does not open a second socket', sockets.length === 1, String(sockets.length));
    const error = await rejection(firstAdd);
    check('first 1006 uses unreachable copy', error === RUNNER_UNREACHABLE_MESSAGE, error);
    check('failed first add remains durable', loadRunnerRecords(store)[0]?.origin === RUNNER_B_ORIGIN);
    check('failed first add remains reconnecting', registry.snapshot()[0]?.state === 'reconnecting');
    check('failed first add has no command timeout', ![...fakeTimers.scheduled.values()].some((timer) => timer.delay > RUNNER_RECONNECT_INITIAL_MS));
    check('failed first add has one reconnect timer', fakeTimers.scheduled.size === 1, String(fakeTimers.scheduled.size));

    const corrected = registry.updateToken(registry.snapshot()[0].id, AUTH_MARKER_A);
    await flush();
    check('credential replacement cancels prior reconnect timer', fakeTimers.scheduled.size === 0, String(fakeTimers.scheduled.size));
    check('credential replacement opens a fresh generation', sockets.length === 2, String(sockets.length));
    sockets[1].open();
    await waitForCommand(sockets[1], 'get_state');
    sockets[1].respond('get_state', true, stateOf('session-b', { cwd: '<workspace>/runner-b' }));
    await flush();
    sockets[1].respond('session_list', true, { sessions: [] });
    check('later generation connects after first failure', (await corrected).state === 'connected');
  }

  {
    activeBlock = 'scheme conflict';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
      randomBytes: deterministicBytes,
    });
    const firstAdd = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    if (!sockets[0].onopen) throw new Error('scheme block socket has no open handler');
    sockets[0].open();
    check('scheme test sends get_state', sockets[0].sent.some((frame) => JSON.parse(frame).type === 'get_state'));
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [loadedRow('session-a'), loadedRow('child-a')] });
    await firstAdd;
    const conflict = await rejection(registry.add('Runner A insecure', 'http://runner-a.example', AUTH_MARKER_B));
    await flush();
    check('same authority with another scheme is rejected', conflict.includes('different scheme'), conflict);
    check('scheme conflict opens no replacement socket', sockets.length === 1, String(sockets.length));
    check('scheme conflict does not overwrite scoped token', store.values.get(hostTokenKey(RUNNER_A_AUTHORITY)) === AUTH_MARKER_A);
  }

  {
    activeBlock = 'get_state failure';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
      randomBytes: deterministicBytes,
    });
    const firstAdd = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', false, undefined, 'probe rejected marker');
    const error = await rejection(firstAdd);
    check('get_state failure settles first add', error === 'probe rejected marker', error);
    check('get_state failure clears command timeout and keeps only reconnect', fakeTimers.scheduled.size === 1, String(fakeTimers.scheduled.size));
    check('get_state failure stays visible after probe socket closes', registry.snapshot()[0]?.error === 'probe rejected marker');
  }

  {
    const store = storage();
    saveRunnerRecords(store, [seededRecord()]);
    store.setItem(hostTokenKey(RUNNER_A_AUTHORITY), AUTH_MARKER_A);
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
      randomBytes: deterministicBytes,
    });
    registry.start();
    await flush();
    sockets[0].open();
    const lateHandler = sockets[0].onmessage;
    registry.delete('seeded-runner');
    lateHandler?.({
      target: sockets[0],
      data: JSON.stringify({
        type: 'response',
        id: 'stale-response',
        command: 'get_state',
        success: true,
        data: { sessionId: 'late', cwd: '<workspace>/late' },
      }),
    } as MessageEvent);
    await flush();
    check('late probe response after delete is dropped', registry.snapshot().length === 0);
    check('late response after delete schedules no reconnect', fakeTimers.scheduled.size === 0);
  }

  {
    const store = storage();
    saveRunnerRecords(store, [seededRecord()]);
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      randomBytes: deterministicBytes,
    });
    registry.start();
    await flush();
    registry.stop();
    registry.start();
    await flush();
    check('disposed registry never restarts sockets', sockets.length === 1, String(sockets.length));
  }

  {
    activeBlock = 'command subscribe is per source session';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [loadedRow('session-a'), loadedRow('child-a'), loadedRow('child-b')] });
    const connected = await added;
    check('connected probe still has no subscribe', subscribeFrames(sockets[0]).length === 0);

    const primary = registry.command(connected.id, { type: 'get_messages' });
    const childA = registry.command(connected.id, { type: 'job_list' }, { sourceSessionId: 'child-a' });
    const childAAgain = registry.command(connected.id, { type: 'goal_get' }, { sourceSessionId: 'child-a' });
    await flush();
    const firstSubscribes = subscribeFrames(sockets[0]);
    check('one subscribe per source session', firstSubscribes.length === 2, String(firstSubscribes.length));
    check('primary subscribe uses exact identity', firstSubscribes[0]?.sessionId === 'session-a' && firstSubscribes[0]?.runtimeIncarnation === incarnationOf('session-a') && firstSubscribes[0]?.cursor === 0);
    check('child subscribe uses exact identity', firstSubscribes[1]?.sessionId === 'child-a' && firstSubscribes[1]?.runtimeIncarnation === incarnationOf('child-a') && firstSubscribes[1]?.cursor === 0);
    check('commands wait for subscription snapshot', !sentTypes(sockets[0]).includes('get_messages'));

    sockets[0].emit(snapshotFrame('session-a', 1));
    await flush();
    check('primary snapshot unblocks only that session', sentTypes(sockets[0]).includes('get_messages'));
    check('child commands still wait', !sentTypes(sockets[0]).includes('job_list'));
    sockets[0].respond('get_messages', true, { messages: [] });
    await primary;

    sockets[0].emit(snapshotFrame('child-a', 4, { type: 'session_resync' }));
    await flush();
    check('session_resync unblocks the child session', sentTypes(sockets[0]).includes('job_list') && sentTypes(sockets[0]).includes('goal_get'));
    sockets[0].respond('job_list', true, { jobs: [] });
    sockets[0].respond('goal_get', true, {});
    await childA;
    await childAAgain;

    let beforeSendCalls = 0;
    const guarded = registry.command(connected.id, { type: 'get_agent_status' }, {
      sourceSessionId: 'child-b',
      beforeSend: () => { beforeSendCalls += 1; },
    });
    await flush();
    check('beforeSend waits for exact-session subscription', beforeSendCalls === 0 && !sentTypes(sockets[0]).includes('get_agent_status'));
    sockets[0].emit(snapshotFrame('child-b', 1));
    await flush();
    check('beforeSend runs once immediately before command dispatch', beforeSendCalls === 1 && sentTypes(sockets[0]).includes('get_agent_status'));
    sockets[0].respond('get_agent_status', true, {});
    await guarded;

    const afterReady = registry.command(connected.id, { type: 'goal_journal' }, { sourceSessionId: 'child-a' });
    await flush();
    check('ready session does not resubscribe', subscribeFrames(sockets[0]).length === 3);
    sockets[0].respond('goal_journal', true, {});
    await afterReady;
    registry.stop();
  }

  {
    activeBlock = 'preprobe command subscribes source primary';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    const runnerId = registry.snapshot()[0]?.id ?? '';
    sockets[0].open();
    await flush();
    check('open probe starts with get_state only', sentTypes(sockets[0]).join(',') === 'get_state');
    const preprobe = registry.command(runnerId, { type: 'get_messages' }, {
      sourceSessionId: 'session-a',
      allowBeforeProbe: true,
    });
    await flush();
    check('preprobe command does not subscribe without exact identity', subscribeFrames(sockets[0]).length === 0);
    check('preprobe stays on get_state only', sentTypes(sockets[0]).join(',') === 'get_state');
    const preprobeError = await rejection(preprobe);
    check('preprobe fails closed without incarnation', preprobeError === 'Runner session identity is unavailable.', preprobeError);
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [loadedRow('session-a')] });
    await added;
    registry.stop();
  }

  {
    activeBlock = 'subscription_error rejects without failing generation';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [loadedRow('child-a')] });
    const connected = await added;
    const pending = registry.command(connected.id, { type: 'prompt', message: 'hello' }, { sourceSessionId: 'child-a' });
    await flush();
    sockets[0].emit({ type: 'subscription_error', sessionId: 'child-a', runtimeIncarnation: incarnationOf('child-a'), error: 'subscription snapshot already in progress' });
    const thrown = await rejectedError(pending);
    const typed = thrown instanceof RunnerCommandError ? thrown : null;
    check('subscription_error rejects the command', typed?.message === 'subscription snapshot already in progress', typed?.message ?? String(thrown));
    check('subscription_error kind is remote', typed?.kind === 'remote');
    check('subscription_error metadata type', typed?.subscription?.type === 'subscription_error');
    check('subscription_error metadata command', typed?.subscription?.command === 'subscribe');
    check('subscription_error metadata session identity', typed?.subscription?.sessionId === 'child-a');
    check('subscription_error metadata incarnation', typed?.subscription?.runtimeIncarnation === incarnationOf('child-a'));
    check('subscription_error metadata error', typed?.subscription?.error === 'subscription snapshot already in progress');
    check('subscription_error keeps the generation connected', registry.snapshot()[0]?.state === 'connected');
    check('subscription_error does not arm reconnect', ![...fakeTimers.scheduled.values()].some((timer) => timer.delay === RUNNER_RECONNECT_INITIAL_MS));
    check('subscription_error does not close the socket', !sockets[0].closed);
    const retry = registry.command(connected.id, { type: 'prompt', message: 'hello' }, { sourceSessionId: 'child-a' });
    await flush();
    check('failed session can subscribe again', subscribeFrames(sockets[0]).length === 2);
    sockets[0].emit(snapshotFrame('child-a', 1));
    await flush();
    sockets[0].respond('prompt', true, {});
    await retry;
    registry.stop();
  }

  {
    activeBlock = 'subscribe timeout closes 4000';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [loadedRow('session-a'), loadedRow('child-a')] });
    const connected = await added;
    const pending = registry.command(connected.id, { type: 'get_messages' }, { sourceSessionId: 'child-a' });
    await flush();
    check('subscribe timeout is armed', [...fakeTimers.scheduled.values()].some((timer) => timer.delay === RUNNER_COMMAND_TIMEOUT_MS));
    check('timeout fires', fakeTimers.runDelay(RUNNER_COMMAND_TIMEOUT_MS));
    const error = await rejection(pending);
    check('subscribe timeout rejects the command', error === 'subscribe timed out.', error);
    check('subscribe timeout closes 4000', sockets[0].closed && sockets[0].closeCode === 4000 && sockets[0].closeReason === 'command timeout');
    check('subscribe timeout enters reconnecting', registry.snapshot()[0]?.state === 'reconnecting');
    registry.stop();
  }

  {
    activeBlock = 'stale generation drops pending subscribe';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [loadedRow('session-a'), loadedRow('child-a')] });
    const connected = await added;
    const pending = registry.command(connected.id, { type: 'get_messages' }, { sourceSessionId: 'child-a' });
    await flush();
    registry.delete(connected.id);
    sockets[0].emit(snapshotFrame('child-a', 1));
    await flush();
    const error = await rejection(pending);
    check('teardown rejects pending subscribe', error === 'runner deleted', error);
    check('stale snapshot cannot reconnect a deleted runner', registry.snapshot().length === 0);
    check('stale snapshot schedules no reconnect', fakeTimers.scheduled.size === 0);
  }

  {
    activeBlock = 'reconnect generation subscribes once on the new socket';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [loadedRow('session-a'), loadedRow('child-a')] });
    const connected = await added;
    const first = registry.command(connected.id, { type: 'get_messages' }, { sourceSessionId: 'child-a' });
    await flush();
    sockets[0].emit(snapshotFrame('child-a', 1));
    await flush();
    sockets[0].respond('get_messages', true, { messages: [] });
    await first;
    sockets[0].close(1006, '');
    check('reconnect timer armed after drop', fakeTimers.runDelay(RUNNER_RECONNECT_INITIAL_MS));
    await flush();
    sockets[1].open();
    await waitForCommand(sockets[1], 'get_state');
    sockets[1].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[1].respond('session_list', true, { sessions: [loadedRow('session-a'), loadedRow('child-a')] });
    await flush();
    check('reconnect probe does not subscribe', subscribeFrames(sockets[1]).length === 0);
    const second = registry.command(connected.id, { type: 'get_messages' }, { sourceSessionId: 'child-a' });
    const secondAgain = registry.command(connected.id, { type: 'job_list' }, { sourceSessionId: 'child-a' });
    await flush();
    check('new generation sends exactly one subscribe', subscribeFrames(sockets[1]).length === 1, String(subscribeFrames(sockets[1]).length));
    check('reconnect subscribe uses stored watermark', subscribeFrames(sockets[1])[0]?.sessionId === 'child-a' && subscribeFrames(sockets[1])[0]?.cursor === 1, JSON.stringify(subscribeFrames(sockets[1])[0]));
    sockets[1].emit(snapshotFrame('child-a', 8));
    await flush();
    sockets[1].respond('get_messages', true, { messages: [] });
    sockets[1].respond('job_list', true, { jobs: [] });
    await second;
    await secondAgain;
    registry.stop();
  }

  {
    activeBlock = 'live event sequence retains cursor across reconnect';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [loadedRow('session-a'), loadedRow('child-a')] });
    const connected = await added;
    const first = registry.command(connected.id, { type: 'get_messages' }, { sourceSessionId: 'child-a' });
    await flush();
    sockets[0].emit(snapshotFrame('child-a', 2));
    await flush();
    sockets[0].respond('get_messages', true, { messages: [] });
    await first;
    sockets[0].emit({ type: 'message_start', sessionId: 'child-a', runtimeIncarnation: incarnationOf('child-a'), sequence: 5, itemId: 'a1' });
    sockets[0].close(1006, '');
    check('reconnect timer armed after live turn drop', fakeTimers.runDelay(RUNNER_RECONNECT_INITIAL_MS));
    await flush();
    sockets[1].open();
    await waitForCommand(sockets[1], 'get_state');
    sockets[1].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[1].respond('session_list', true, { sessions: [loadedRow('session-a'), loadedRow('child-a')] });
    await flush();
    const second = registry.command(connected.id, { type: 'get_messages' }, { sourceSessionId: 'child-a' });
    await flush();
    check('reconnect subscribe uses live sequence cursor', subscribeFrames(sockets[1])[0]?.sessionId === 'child-a' && subscribeFrames(sockets[1])[0]?.cursor === 5, JSON.stringify(subscribeFrames(sockets[1])[0]));
    sockets[1].emit(snapshotFrame('child-a', 8));
    await flush();
    sockets[1].respond('get_messages', true, { messages: [] });
    await second;
    registry.stop();
  }

  {
    activeBlock = 'session_resync_required resets stored cursor';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [loadedRow('session-a'), loadedRow('child-a')] });
    const connected = await added;
    const first = registry.command(connected.id, { type: 'get_messages' }, { sourceSessionId: 'child-a' });
    await flush();
    sockets[0].emit(snapshotFrame('child-a', 9));
    await flush();
    sockets[0].respond('get_messages', true, { messages: [] });
    await first;
    sockets[0].emit({ type: 'session_resync_required', sessionId: 'child-a', runtimeIncarnation: incarnationOf('child-a'), sequence: 12 });
    sockets[0].close(1006, '');
    check('reconnect timer armed after resync-required drop', fakeTimers.runDelay(RUNNER_RECONNECT_INITIAL_MS));
    await flush();
    sockets[1].open();
    await waitForCommand(sockets[1], 'get_state');
    sockets[1].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[1].respond('session_list', true, { sessions: [loadedRow('session-a'), loadedRow('child-a')] });
    await flush();
    const second = registry.command(connected.id, { type: 'get_messages' }, { sourceSessionId: 'child-a' });
    await flush();
    check('resync-required reconnect subscribes from cursor 0', subscribeFrames(sockets[1])[0]?.sessionId === 'child-a' && subscribeFrames(sockets[1])[0]?.cursor === 0, JSON.stringify(subscribeFrames(sockets[1])[0]));
    sockets[1].emit(snapshotFrame('child-a', 3, { type: 'session_resync' }));
    await flush();
    sockets[1].respond('get_messages', true, { messages: [] });
    await second;
    registry.stop();
  }

  {
    activeBlock = 'unconsumed authoritative cut keeps cursor at 0';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [loadedRow('session-a'), loadedRow('child-a')] });
    const connected = await added;
    const received: unknown[] = [];
    const unsubscribe = subscribeRunnerEvents(connected.id, (frame) => { received.push(frame); });

    const first = registry.command(connected.id, { type: 'get_messages' }, { sourceSessionId: 'child-a' });
    await flush();
    sockets[0].emit({ type: 'session_resync', sessionId: 'child-a', runtimeIncarnation: incarnationOf('child-a'), watermark: 7 });
    await flush();
    check('cut missing exact fields is ignored', received.length === 0, String(received.length));
    check('invalid cut does not unblock the command', !sentTypes(sockets[0]).includes('get_messages'));
    sockets[0].emit({ type: 'session_resync', reason: 'resync', sessionId: 'child-a', runtimeIncarnation: incarnationOf('child-a'), watermark: 7, state: {}, events: [] });
    await flush();
    check('cut without messages is ignored', received.length === 0, String(received.length));
    check('unusable cut does not unblock the command', !sentTypes(sockets[0]).includes('get_messages'));
    sockets[0].emit(snapshotFrame('child-a', 0, { type: 'session_resync' }));
    await flush();
    const usableZero = received[0] as { type?: unknown; authoritative?: unknown };
    check('usable empty-messages resync unblocks from cursor 0', usableZero.type === 'session_resync' && usableZero.authoritative === true, JSON.stringify(usableZero));
    check('unusable cut dispatches before command unblock', sentTypes(sockets[0]).includes('get_messages'));
    sockets[0].respond('get_messages', true, { messages: [] });
    await first;
    sockets[0].close(1006, '');
    check('reconnect timer armed after unusable cut drop', fakeTimers.runDelay(RUNNER_RECONNECT_INITIAL_MS));
    await flush();
    sockets[1].open();
    await waitForCommand(sockets[1], 'get_state');
    sockets[1].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[1].respond('session_list', true, { sessions: [loadedRow('session-a'), loadedRow('child-a')] });
    await flush();
    const second = registry.command(connected.id, { type: 'get_messages' }, { sourceSessionId: 'child-a' });
    await flush();
    // Unconsumed: the rejected cuts never advanced the exact-identity cursor.
    check('reconnect after unusable cut subscribes from cursor 0', subscribeFrames(sockets[1])[0]?.sessionId === 'child-a' && subscribeFrames(sockets[1])[0]?.cursor === 0, JSON.stringify(subscribeFrames(sockets[1])[0]));
    sockets[1].emit(snapshotFrame('child-a', 3, { type: 'session_resync' }));
    await flush();
    const usable = received.filter((frame) => frame && typeof frame === 'object' && 'type' in frame && frame.type === 'session_resync')[1] as { type?: unknown; authoritative?: unknown };
    check('usable resync dispatches authoritative', usable.type === 'session_resync' && usable.authoritative === true);
    sockets[1].respond('get_messages', true, { messages: [] });
    await second;
    sockets[1].close(1006, '');
    check('reconnect timer armed after consumed cut drop', fakeTimers.runDelay(RUNNER_RECONNECT_INITIAL_MS));
    await flush();
    sockets[2].open();
    await waitForCommand(sockets[2], 'get_state');
    sockets[2].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[2].respond('session_list', true, { sessions: [loadedRow('session-a'), loadedRow('child-a')] });
    await flush();
    const third = registry.command(connected.id, { type: 'get_messages' }, { sourceSessionId: 'child-a' });
    await flush();
    check('consumed cut advances the reconnect cursor to its watermark', subscribeFrames(sockets[2])[0]?.cursor === 3, JSON.stringify(subscribeFrames(sockets[2])[0]));
    sockets[2].emit(snapshotFrame('child-a', 4));
    await flush();
    sockets[2].respond('get_messages', true, { messages: [] });
    await third;
    unsubscribe();
    registry.stop();
  }

  {
    activeBlock = 'preflight stored origin fails offline';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const fetched: string[] = [];
    const registry = new RunnerRegistry({
      fetchFn: statusFetch((url) => {
        fetched.push(url);
        return new Error('TLS handshake failed');
      }),
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
      randomBytes: deterministicBytes,
    });
    const firstAdd = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    const error = await rejection(firstAdd);
    check('network failure uses exact unreachable copy', error === RUNNER_UNREACHABLE_MESSAGE, error);
    check('network failure does not mention token', !error.toLowerCase().includes('token') && !error.toLowerCase().includes('unavailable'));
    check('network failure opens no websocket', sockets.length === 0, String(sockets.length));
    check('network failure goes offline', registry.snapshot()[0]?.state === 'reconnecting', registry.snapshot()[0]?.state ?? '');
    check('network failure prefights the stored origin once', fetched.length === 1 && fetched[0] === `${RUNNER_A_ORIGIN}/status`, fetched.join(','));
    check('network failure arms one reconnect timer', fakeTimers.scheduled.size === 1, String(fakeTimers.scheduled.size));
    registry.stop();
  }

  {
    activeBlock = 'preflight token required without token';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const fetched: string[] = [];
    const registry = new RunnerRegistry({
      fetchFn: statusFetch((url) => {
        fetched.push(url);
        return okStatus(true);
      }),
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
      randomBytes: deterministicBytes,
    });
    const firstAdd = registry.add('Runner B', RUNNER_B_ORIGIN, '');
    await flush();
    const thrown = await rejectedError(firstAdd);
    check('token required rejects typed auth error', thrown instanceof RunnerAuthRequiredError);
    check('token required message is exact', thrown instanceof Error && thrown.message === RUNNER_TOKEN_REQUIRED_MESSAGE, thrown instanceof Error ? thrown.message : String(thrown));
    check('token required opens no websocket', sockets.length === 0, String(sockets.length));
    check('token required state is token_required', registry.snapshot()[0]?.state === 'token_required', registry.snapshot()[0]?.state ?? '');
    check('token required keeps status.tokenRequired', registry.snapshot()[0]?.status?.tokenRequired === true);
    check('token required prefights the stored origin once', fetched.length === 1 && fetched[0] === `${RUNNER_B_ORIGIN}/status`, fetched.join(','));
    check('token required does not preflight another origin', fetched.every((url) => url.startsWith(`${RUNNER_B_ORIGIN}/`)), fetched.join(','));
    check('token required does not arm reconnect', fakeTimers.scheduled.size === 0, String(fakeTimers.scheduled.size));
    const corrected = registry.updateToken(registry.snapshot()[0].id, AUTH_MARKER_A);
    await flush();
    check('updateToken after token required opens a socket', sockets.length === 1, String(sockets.length));
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-b', { cwd: '<workspace>/runner-b' }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [] });
    check('updateToken connects after supplying token', (await corrected).state === 'connected');
    registry.stop();
  }

  {
    activeBlock = 'token-gated handshake 1006 does not open another origin';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const fetched: string[] = [];
    const registry = new RunnerRegistry({
      fetchFn: statusFetch((url) => {
        fetched.push(url);
        return okStatus(true);
      }),
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
      randomBytes: deterministicBytes,
    });
    const firstAdd = registry.add('Runner B', RUNNER_B_ORIGIN, AUTH_MARKER_B);
    await flush();
    check('token-gated add opens the primary websocket', sockets.length === 1, String(sockets.length));
    sockets[0].close(1006, '');
    await flush();
    const handshakeThrown = await rejectedError(firstAdd);
    check('token-gated 1006 rejects typed auth error', handshakeThrown instanceof RunnerAuthRequiredError);
    check('token-gated 1006 message is exact', handshakeThrown instanceof Error && handshakeThrown.message === RUNNER_TOKEN_REQUIRED_MESSAGE, handshakeThrown instanceof Error ? handshakeThrown.message : String(handshakeThrown));
    check('token-gated 1006 does not open a second socket', sockets.length === 1, String(sockets.length));
    check('token-gated 1006 does not preflight another origin', fetched.every((url) => url.startsWith(`${RUNNER_B_ORIGIN}/`)), fetched.join(','));
    check('token-gated 1006 state is token_required', registry.snapshot()[0]?.state === 'token_required', registry.snapshot()[0]?.state ?? '');
    check('token-gated 1006 does not arm reconnect', fakeTimers.scheduled.size === 0, String(fakeTimers.scheduled.size));
    registry.stop();
  }

  {
    activeBlock = 'success then invalid-token reconnect stays on stored origin';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const fetched: string[] = [];
    let tokenRequired = false;
    const registry = new RunnerRegistry({
      fetchFn: statusFetch((url) => {
        fetched.push(url);
        return okStatus(tokenRequired);
      }),
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
      randomBytes: deterministicBytes,
    });
    const added = registry.add('Runner B', RUNNER_B_ORIGIN, AUTH_MARKER_B);
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-b', { cwd: '<workspace>/runner-b' }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [loadedRow('session-b')] });
    const connected = await added;
    check('prior success leaves a probe on the view', connected.probe?.sessionId === 'session-b', connected.probe?.sessionId ?? '');
    tokenRequired = true;
    sockets[0].close(1006, '');
    await flush();
    check('post-probe 1006 keeps stale probe while reconnecting', registry.snapshot()[0]?.state === 'reconnecting'
      && registry.snapshot()[0]?.probe?.sessionId === 'session-b', registry.snapshot()[0]?.state ?? '');
    check('post-probe 1006 arms reconnect on the stored origin', fakeTimers.runDelay(RUNNER_RECONNECT_INITIAL_MS));
    await flush();
    check('invalid-token reconnect opens one socket on the same origin', sockets.length === 2
      && sockets[1]?.url === sockets[0]?.url, sockets[1]?.url ?? '');
    sockets[1].close(1006, '');
    await flush();
    check('invalid-token 1006 with stale probe halts for token', registry.snapshot()[0]?.state === 'token_required', registry.snapshot()[0]?.state ?? '');
    check('invalid-token 1006 stays on two stored-origin sockets', sockets.length === 2, String(sockets.length));
    check('invalid-token 1006 does not preflight another origin', fetched.every((url) => url.startsWith(`${RUNNER_B_ORIGIN}/`)), fetched.join(','));
    check('invalid-token 1006 does not arm reconnect', fakeTimers.scheduled.size === 0, String(fakeTimers.scheduled.size));
    registry.stop();
  }

  {
    activeBlock = 'cross-machine tokenless runner opens the socket';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const fetched: string[] = [];
    const registry = new RunnerRegistry({
      fetchFn: statusFetch((url) => {
        fetched.push(url);
        return okStatus(false);
      }),
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
      randomBytes: deterministicBytes,
    });
    const added = registry.add('Runner B', RUNNER_B_ORIGIN, '');
    await flush();
    check('cross-machine tokenless opens the socket', sockets.length === 1, String(sockets.length));
    check('cross-machine tokenless prefights the primary scheme once', fetched.length === 1 && fetched[0] === `${RUNNER_B_ORIGIN}/status`, fetched.join(','));
    check('cross-machine tokenless keeps status.tokenRequired false', registry.snapshot()[0]?.status?.tokenRequired === false);
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-b', { cwd: '<workspace>/runner-b' }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [] });
    const connected = await added;
    check('cross-machine tokenless settles connected', connected.state === 'connected', connected.state);
    check('cross-machine tokenless keeps error empty', registry.snapshot()[0]?.error === '', registry.snapshot()[0]?.error ?? '');
    check('cross-machine tokenless does not arm reconnect', fakeTimers.scheduled.size === 0, String(fakeTimers.scheduled.size));
    const retokened = registry.updateToken(connected.id, AUTH_MARKER_A);
    await flush();
    check('tokenless Set token opens a replacement socket', sockets.length === 2, String(sockets.length));
    check('tokenless Set token does not offer rpi-auth', sockets[1]?.protocols.length === 0, JSON.stringify(sockets[1]?.protocols));
    sockets[1].open();
    await waitForCommand(sockets[1], 'get_state');
    sockets[1].respond('get_state', true, stateOf('session-b', { cwd: '<workspace>/runner-b' }));
    await flush();
    sockets[1].respond('session_list', true, { sessions: [] });
    check('tokenless Set token reconnects without a wire token', (await retokened).state === 'connected');
    registry.stop();
  }

  {
    activeBlock = 'same-origin tokenless runner stays connected';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
      randomBytes: deterministicBytes,
    });
    const added = registry.add('Runner B', RUNNER_B_ORIGIN, '');
    await flush();
    check('same-origin tokenless opens the socket', sockets.length === 1, String(sockets.length));
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-b', { cwd: '<workspace>/runner-b' }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [] });
    const connected = await added;
    check('same-origin tokenless settles connected', connected.state === 'connected', connected.state);
    check('same-origin tokenless keeps error empty', registry.snapshot()[0]?.error === '', registry.snapshot()[0]?.error ?? '');
    registry.stop();
  }

  {
    activeBlock = 'configured-token cross-machine runner uses the socket';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: statusFetch(() => okStatus(true)),
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
      randomBytes: deterministicBytes,
    });
    const added = registry.add('Runner B', RUNNER_B_ORIGIN, AUTH_MARKER_B);
    await flush();
    check('configured-token cross-machine opens the socket', sockets.length === 1, String(sockets.length));
    check('configured-token cross-machine sends the token subprotocol', sockets[0]?.protocols[0] === `rpi-auth.${AUTH_MARKER_B}`, JSON.stringify(sockets[0]?.protocols ?? []));
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-b', { cwd: '<workspace>/runner-b' }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [] });
    const connected = await added;
    check('configured-token cross-machine settles connected', connected.state === 'connected', connected.state);
    registry.stop();
  }
  {
    activeBlock = 'stored https origin 1006 reconnects on the same origin';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const fetched: string[] = [];
    const registry = new RunnerRegistry({
      fetchFn: statusFetch((url) => {
        fetched.push(url);
        return okStatus(false);
      }),
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
      pageProtocol: 'https:',
    });
    const firstAdd = registry.add('LAN', '192.0.2.15:8765', AUTH_MARKER_A);
    await flush();
    check('https page opens wss', sockets[0]?.url === 'wss://192.0.2.15:8765/ws', sockets[0]?.url ?? '');
    sockets[0].close(1006, '');
    await flush();
    check('https 1006 does not open a second socket', sockets.length === 1, String(sockets.length));
    const droppedHttps = await rejection(firstAdd);
    check('https 1006 uses unreachable copy', droppedHttps === RUNNER_UNREACHABLE_MESSAGE, droppedHttps);
    check('https 1006 keeps the stored origin', loadRunnerRecords(store)[0]?.origin === 'https://192.0.2.15:8765', loadRunnerRecords(store)[0]?.origin ?? '');
    check('https 1006 prefights only the stored origin', fetched.every((url) => url === 'https://192.0.2.15:8765/status'), fetched.join(','));
    check('https 1006 arms reconnect', fakeTimers.runDelay(RUNNER_RECONNECT_INITIAL_MS));
    await flush();
    check('https reconnect opens the same wss origin', sockets.length === 2 && sockets[1]?.url === 'wss://192.0.2.15:8765/ws', sockets[1]?.url ?? '');
    registry.stop();
  }

  {
    activeBlock = 'register yields the runner before the first probe';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: statusFetch(() => okStatus(false)),
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
      pageProtocol: 'https:',
    });
    const started = registry.register('LAN', '192.0.2.15:8765', AUTH_MARKER_A);
    check('register returns a runner id immediately', started.runner.id.length > 0, started.runner.id);
    check('register persists before the probe', loadRunnerRecords(store).length === 1, String(loadRunnerRecords(store).length));
    await flush();
    sockets[0].close(1006, '');
    await flush();
    const droppedRegister = await rejection(started.connected);
    check('register connected uses unreachable copy', droppedRegister === RUNNER_UNREACHABLE_MESSAGE, droppedRegister);
    check('register keeps the runner after probe failure', registry.snapshot().length === 1);
    registry.stop();
  }

  {
    activeBlock = 'stored http origin 1006 reconnects on the same origin';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const fetched: string[] = [];
    const registry = new RunnerRegistry({
      fetchFn: statusFetch((url) => {
        fetched.push(url);
        return okStatus(false);
      }),
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
      pageProtocol: 'http:',
    });
    const firstAdd = registry.add('LAN', '192.0.2.15:8765', AUTH_MARKER_A);
    await flush();
    check('http page opens ws', sockets[0]?.url === 'ws://192.0.2.15:8765/ws', sockets[0]?.url ?? '');
    sockets[0].close(1006, '');
    await flush();
    check('http 1006 does not open a second socket', sockets.length === 1, String(sockets.length));
    const droppedHttp = await rejection(firstAdd);
    check('http 1006 uses unreachable copy', droppedHttp === RUNNER_UNREACHABLE_MESSAGE, droppedHttp);
    check('http 1006 keeps the stored origin', loadRunnerRecords(store)[0]?.origin === 'http://192.0.2.15:8765', loadRunnerRecords(store)[0]?.origin ?? '');
    check('http 1006 prefights only the stored origin', fetched.every((url) => url === 'http://192.0.2.15:8765/status'), fetched.join(','));
    check('http 1006 arms reconnect', fakeTimers.runDelay(RUNNER_RECONNECT_INITIAL_MS));
    await flush();
    check('http reconnect opens the same ws origin', sockets.length === 2 && sockets[1]?.url === 'ws://192.0.2.15:8765/ws', sockets[1]?.url ?? '');
    registry.stop();
  }

  {
    activeBlock = '1006 during bootstrap then mapping recovery before connected';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const PRIMARY = 'primary-x';
    const PRIMARY_CWD = '<workspace>/primary-x';
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
      beforeProbe: async (runnerId) => {
        await registry.command(runnerId, { type: 'session_list', scope: 'all_projects' }, {
          sourceSessionId: PRIMARY,
          allowBeforeProbe: true,
        });
        await registry.command(runnerId, { type: 'switch_session', sessionPath: PRIMARY_CWD }, {
          sourceSessionId: PRIMARY,
          allowBeforeProbe: true,
        });
      },
    });
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    check('bootstrap get_state is sessionless before 1006', commandFrames<{ sessionId?: string }>(sockets[0], 'get_state')[0].sessionId === undefined, sockets[0].sent[0] ?? '');
    sockets[0].close(1006, '');
    await flush();
    check('1006 during bootstrap does not open a second socket', sockets.length === 1, String(sockets.length));
    const dropped = await rejection(added);
    check('1006 during bootstrap uses unreachable copy', dropped === RUNNER_UNREACHABLE_MESSAGE, dropped);
    check('1006 during bootstrap arms reconnect', fakeTimers.runDelay(RUNNER_RECONNECT_INITIAL_MS));
    await flush();
    check('reconnect opens a new generation socket', sockets.length === 2, String(sockets.length));
    sockets[1].open();
    await waitForCommand(sockets[1], 'get_state');
    check('replacement get_state is sessionless', commandFrames<{ sessionId?: string }>(sockets[1], 'get_state')[0].sessionId === undefined, JSON.stringify(commandFrames(sockets[1], 'get_state')[0]));
    sockets[1].respond('get_state', true, stateOf(PRIMARY, { cwd: PRIMARY_CWD }));
    await waitForCommand(sockets[1], 'session_list');
    sockets[1].respond('session_list', true, { sessions: [loadedRow(PRIMARY)] });
    check('connected waits for mapping recovery', registry.snapshot()[0]?.state !== 'connected', registry.snapshot()[0]?.state ?? '');
    await waitForCommand(sockets[1], 'subscribe');
    check('recovery subscribe binds the learned primary', subscribeFrames(sockets[1])[0]?.sessionId === PRIMARY, JSON.stringify(subscribeFrames(sockets[1])[0]));
    sockets[1].emit(snapshotFrame(PRIMARY, 0));
    await waitForFrameCount(sockets[1], 'session_list', 2);
    sockets[1].respondLast('session_list', true, {
      sessions: [loadedRow(PRIMARY, { path: PRIMARY_CWD, cwd: PRIMARY_CWD, status: 'native', source: 'rpi' })],
    });
    await waitForCommand(sockets[1], 'switch_session');
    sockets[1].respond('switch_session', true, {
      sessionId: PRIMARY,
      runtimeIncarnation: incarnationOf(PRIMARY),
      state: { sessionId: PRIMARY, cwd: PRIMARY_CWD, sessionFile: PRIMARY_CWD },
      session: {
        source: 'rpi',
        sessionId: PRIMARY,
        runtimeIncarnation: incarnationOf(PRIMARY),
        path: PRIMARY_CWD,
        cwd: PRIMARY_CWD,
        status: 'native',
        loaded: true,
      },
    });
    let recovered = registry.snapshot()[0];
    for (let attempt = 0; attempt < 40 && recovered?.state !== 'connected'; attempt += 1) {
      await flush();
      recovered = registry.snapshot()[0];
    }
    check('runner settles connected after recovery', recovered?.state === 'connected', recovered?.state ?? '');
    check('probe records the bound primary session', recovered?.probe?.sessionId === PRIMARY, recovered?.probe?.sessionId ?? '');
    check('recovery-before-connected wire order', sentTypes(sockets[1]).join(',') === 'get_state,session_list,subscribe,session_list,switch_session', sentTypes(sockets[1]).join(','));
    check('reconnect socket was never closed', sockets[1].closed === false);
    check('failure loop never spawned a third socket', sockets.length === 2, String(sockets.length));
    registry.stop();
  }

  {
    activeBlock = 'mapping recovery failure does not publish connected';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
      beforeProbe: async () => {
        throw new Error('mapping recovery failed');
      },
    });
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await waitForCommand(sockets[0], 'session_list');
    sockets[0].respond('session_list', true, { sessions: [loadedRow('session-a')] });
    const error = await rejection(added);
    check('recovery failure rejects the first probe', error === 'mapping recovery failed', error);
    check('recovery failure does not publish connected', registry.snapshot()[0]?.state !== 'connected', registry.snapshot()[0]?.state ?? '');
    registry.stop();
  }

  {
    activeBlock = 'connected is not published until recovery completes';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    let releaseRecover: () => void = () => {};
    const recoverGate = new Promise<void>((resolve) => { releaseRecover = resolve; });
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
      beforeProbe: async () => {
        await recoverGate;
      },
    });
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await waitForCommand(sockets[0], 'session_list');
    sockets[0].respond('session_list', true, { sessions: [loadedRow('session-a')] });
    await flush();
    check('probe identity is published before connected', registry.snapshot()[0]?.probe?.sessionId === 'session-a', registry.snapshot()[0]?.probe?.sessionId ?? '');
    check('state stays connecting while recovery is in flight', registry.snapshot()[0]?.state === 'connecting', registry.snapshot()[0]?.state ?? '');
    releaseRecover();
    const connected = await added;
    check('connected publishes after awaited recovery', connected.state === 'connected', connected.state);
    registry.stop();
  }

  {
    activeBlock = 'reload recovery never spends the bootstrap allowance';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const MAPPED = 'mapped-reload';
    const MAPPED_CWD = '<workspace>/mapped-reload';
    let recoveries = 0;
    saveRunnerToken(store, RUNNER_A_AUTHORITY, AUTH_MARKER_A);
    saveRunnerRecords(store, [seededRecord()]);
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
      now: () => 100,
      beforeProbe: async (runnerId) => {
        recoveries += 1;
        await registry.command(runnerId, { type: 'session_list', scope: 'all_projects' }, {
          sourceSessionId: MAPPED,
          allowBeforeProbe: true,
        });
        await registry.command(runnerId, { type: 'switch_session', sessionPath: MAPPED_CWD }, {
          sourceSessionId: MAPPED,
          allowBeforeProbe: true,
        });
      },
    });
    check('reloaded runner starts connecting', registry.snapshot()[0]?.id === 'seeded-runner', registry.snapshot()[0]?.id ?? '');
    registry.start();
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    check('reload recovery waits for bootstrap identity before subscribe', subscribeFrames(sockets[0]).length === 0);
    const reloadBootstrap = commandFrames<{ sessionId?: string }>(sockets[0], 'get_state')[0];
    check('sessionful recovery did not spend the bootstrap', reloadBootstrap.sessionId === undefined, JSON.stringify(reloadBootstrap));
    sockets[0].respond('get_state', true, stateOf(MAPPED, { cwd: MAPPED_CWD }));
    await waitForCommand(sockets[0], 'session_list');
    const probeList = commandFrames<{ sessionId?: string }>(sockets[0], 'session_list')[0];
    check('probe get_state response binds and the probe continues sessionful', probeList.sessionId === MAPPED, JSON.stringify(probeList));
    sockets[0].respond('session_list', true, { sessions: [loadedRow(MAPPED)] });
    check('reload recovery does not publish connected before subscribe', registry.snapshot()[0]?.state !== 'connected', registry.snapshot()[0]?.state ?? '');
    await waitForCommand(sockets[0], 'subscribe');
    const recoverySubscribe = subscribeFrames(sockets[0])[0];
    check('recovery subscribe binds the mapped session', recoverySubscribe.sessionId === MAPPED && recoverySubscribe.runtimeIncarnation === incarnationOf(MAPPED) && recoverySubscribe.cursor === 0, JSON.stringify(recoverySubscribe));
    sockets[0].emit(snapshotFrame(MAPPED, 0));
    await waitForFrameCount(sockets[0], 'session_list', 2);
    const recoveryList = commandFrames<{ scope?: string; sessionId?: string }>(sockets[0], 'session_list')[1];
    check('recovery session_list binds the mapped session', recoveryList.scope === 'all_projects' && recoveryList.sessionId === MAPPED, JSON.stringify(recoveryList));
    sockets[0].respondLast('session_list', true, { sessions: [loadedRow(MAPPED, { path: MAPPED_CWD, cwd: MAPPED_CWD, status: 'native' })] });
    await waitForCommand(sockets[0], 'switch_session');
    const recoverySwitch = commandFrames<{ sessionId?: string }>(sockets[0], 'switch_session')[0];
    check('recovery switch_session binds the mapped session', recoverySwitch.sessionId === MAPPED, JSON.stringify(recoverySwitch));
    sockets[0].respond('switch_session', true, {
      sessionId: MAPPED,
      runtimeIncarnation: incarnationOf(MAPPED),
      state: { sessionId: MAPPED, cwd: MAPPED_CWD, sessionFile: MAPPED_CWD },
      session: {
        source: 'rpi',
        sessionId: MAPPED,
        runtimeIncarnation: incarnationOf(MAPPED),
        path: MAPPED_CWD,
        cwd: MAPPED_CWD,
        status: 'native',
        loaded: true,
      },
    });
    await flush();
    check('exact BUG-5 reload recovery wire order', sentTypes(sockets[0]).join(',') === 'get_state,session_list,subscribe,session_list,switch_session', sentTypes(sockets[0]).join(','));
    check('reload recovery connects clean', registry.snapshot()[0]?.state === 'connected', registry.snapshot()[0]?.state ?? '');
    check('reload recovery ran once after identity', recoveries === 1, String(recoveries));
    check('reload recovery arms no reconnect timer', fakeTimers.scheduled.size === 0, String(fakeTimers.scheduled.size));
    registry.stop();
  }

  {
    activeBlock = 'mapped get_messages after same-port recovery uses live owner';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const OLD_PRIMARY = 'old-primary';
    const NEW_PRIMARY = 'new-primary';
    const MAPPED = 'mapped-session';
    const PROJECT_CWD = '/workspace/mapped-project';
    const MAPPED_PATH = '/workspace/sessions/mapped.jsonl';
    const BOOTSTRAP_CWD = '/workspace/bootstrap';
    saveRunnerToken(store, RUNNER_A_AUTHORITY, AUTH_MARKER_A);
    saveRunnerRecords(store, [seededRecord()]);
    store.setItem(CHAT_MAPPING_STORAGE_KEY, JSON.stringify({
      version: 1,
      projects: [{ id: 'project-1', label: 'Mapped', cwd: PROJECT_CWD, createdAt: 1, updatedAt: 1 }],
      mappings: [{
        id: 'project-1::seeded-runner',
        projectId: 'project-1',
        runnerId: 'seeded-runner',
        operationId: 'op-1',
        sourceSessionId: OLD_PRIMARY,
        canaryId: 'op-1:canary',
        createId: 'op-1:create',
        sessionName: 'mapped',
        sessionId: MAPPED,
        sessionPath: MAPPED_PATH,
        state: 'active',
        createdAt: 1,
        updatedAt: 1,
      }],
    }));
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
    const mapping = new ChatMappingStore({
      storage: store,
      runners: registry,
      withExclusiveLock: async (callback) => callback(),
      now: () => 100,
      randomBytes: deterministicBytes,
    });
    registry.setBeforeProbe((runnerId) => mapping.recoverRunner(runnerId));
    registry.start();
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf(NEW_PRIMARY, { cwd: BOOTSTRAP_CWD }));
    await waitForCommand(sockets[0], 'session_list');
    sockets[0].respond('session_list', true, { sessions: [loadedRow(NEW_PRIMARY)] });
    check('same-port recover stays connecting until mapped switch', registry.snapshot()[0]?.state !== 'connected', registry.snapshot()[0]?.state ?? '');
    await waitForCommand(sockets[0], 'subscribe');
    check('same-port recover subscribes the new primary', subscribeFrames(sockets[0])[0]?.sessionId === NEW_PRIMARY, JSON.stringify(subscribeFrames(sockets[0])[0]));
    sockets[0].emit(snapshotFrame(NEW_PRIMARY, 0));
    await waitForFrameCount(sockets[0], 'session_list', 2);
    const catalog = commandFrames<{ scope?: string; sessionId?: string }>(sockets[0], 'session_list')[1];
    check('same-port recover catalogs through the new primary', catalog.scope === 'all_projects' && catalog.sessionId === NEW_PRIMARY, JSON.stringify(catalog));
    sockets[0].respondLast('session_list', true, {
      sessions: [
        loadedRow(NEW_PRIMARY, { path: '/workspace/bootstrap.jsonl', cwd: BOOTSTRAP_CWD, status: 'native', source: 'rpi' }),
        loadedRow(MAPPED, { path: MAPPED_PATH, cwd: PROJECT_CWD, status: 'native', source: 'rpi' }),
      ],
    });
    await waitForCommand(sockets[0], 'switch_session');
    const switchFrame = commandFrames<{ id?: string; sessionId?: string; sessionPath?: string }>(sockets[0], 'switch_session')[0];
    check('same-port recover switches the stored mapped path from the new primary', switchFrame.sessionId === NEW_PRIMARY && switchFrame.sessionPath === MAPPED_PATH, JSON.stringify(switchFrame));
    sockets[0].respondLast('switch_session', true, {
      sessionId: MAPPED,
      runtimeIncarnation: incarnationOf(MAPPED),
      state: { sessionId: MAPPED, sessionFile: MAPPED_PATH, cwd: PROJECT_CWD },
      session: {
        source: 'rpi',
        sessionId: MAPPED,
        runtimeIncarnation: incarnationOf(MAPPED),
        path: MAPPED_PATH,
        cwd: PROJECT_CWD,
        status: 'native',
        loaded: true,
      },
    }, undefined, MAPPED);
    for (let attempt = 0; attempt < 40; attempt += 1) {
      if (registry.snapshot()[0]?.state === 'connected') break;
      await flush();
    }
    check('same-port recover publishes connected after the mapped switch', registry.snapshot()[0]?.state === 'connected', registry.snapshot()[0]?.state ?? '');
    const history = registry.command('seeded-runner', { type: 'get_messages' }, { sourceSessionId: MAPPED });
    await waitForFrameCount(sockets[0], 'subscribe', 2);
    check('mapped get_messages subscribes the mapped session', subscribeFrames(sockets[0])[1]?.sessionId === MAPPED, JSON.stringify(subscribeFrames(sockets[0])[1]));
    sockets[0].emit(snapshotFrame(MAPPED, 0));
    await waitForCommand(sockets[0], 'get_messages');
    const historyFrame = commandFrames<{ sessionId?: string }>(sockets[0], 'get_messages')[0];
    check('mapped get_messages binds the mapped session', historyFrame.sessionId === MAPPED, JSON.stringify(historyFrame));
    sockets[0].respond('get_messages', true, { messages: [] });
    const historyResponse = await history;
    check('mapped get_messages succeeds after same-port recovery', historyResponse.success === true && historyResponse.sessionId === MAPPED, JSON.stringify(historyResponse));
    const sent = registry.command('seeded-runner', { type: 'prompt', message: 'ping after recovery' }, { sourceSessionId: MAPPED });
    await waitForCommand(sockets[0], 'prompt');
    const promptFrame = commandFrames<{ sessionId?: string; message?: string }>(sockets[0], 'prompt')[0];
    check('mapped send binds the mapped session', promptFrame.sessionId === MAPPED && promptFrame.message === 'ping after recovery', JSON.stringify(promptFrame));
    sockets[0].respond('prompt', true, {});
    const sentResponse = await sent;
    check('mapped send succeeds after same-port recovery', sentResponse.success === true, JSON.stringify(sentResponse));
    check('same-port recover never addressed the dead primary', sockets[0].sent.every((raw) => {
      const frame = JSON.parse(raw) as { sessionId?: string };
      return frame.sessionId !== OLD_PRIMARY;
    }));
    registry.stop();
  }

  {
    activeBlock = 'mid-probe 1006 reconnect re-bootstraps the new connection';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    await flush();
    await waitForCommand(sockets[0], 'get_state');
    check('first connection bootstraps sessionless', commandFrames<{ sessionId?: string }>(sockets[0], 'get_state')[0].sessionId === undefined, sockets[0].sent[0] ?? '');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    check('first connection spends exactly one bootstrap', commandFrames(sockets[0], 'get_state').length === 1, String(commandFrames(sockets[0], 'get_state').length));
    sockets[0].close(1006, '');
    await flush();
    check('mid-probe 1006 does not open a second socket', sockets.length === 1, String(sockets.length));
    const droppedMid = await rejection(added);
    check('mid-probe 1006 uses unreachable copy', droppedMid === RUNNER_UNREACHABLE_MESSAGE, droppedMid);
    check('mid-probe 1006 arms reconnect', fakeTimers.runDelay(RUNNER_RECONNECT_INITIAL_MS));
    await flush();
    check('mid-probe reconnect opens a new generation', sockets.length === 2, String(sockets.length));
    sockets[1].open();
    await flush();
    await waitForCommand(sockets[1], 'get_state');
    const replacement = commandFrames<{ sessionId?: string }>(sockets[1], 'get_state')[0];
    check('reconnect re-bootstraps without the closed binding', replacement.sessionId === undefined, JSON.stringify(replacement));
    sockets[1].respond('get_state', true, stateOf('session-b', { cwd: SAFE_CWD_A }));
    await flush();
    await waitForCommand(sockets[1], 'session_list');
    const replacementList = commandFrames<{ sessionId?: string }>(sockets[1], 'session_list')[0];
    check('reconnect probe catalogs its own newly bound session', replacementList.sessionId === 'session-b', JSON.stringify(replacementList));
    sockets[1].respond('session_list', true, { sessions: [loadedRow('session-b')] });
    let reconnected = registry.snapshot()[0];
    for (let attempt = 0; attempt < 40 && reconnected?.state !== 'connected'; attempt += 1) {
      await flush();
      reconnected = registry.snapshot()[0];
    }
    check('mid-probe reconnect settles connected', reconnected?.state === 'connected', reconnected?.state ?? '');
    check('probe record moves to the reconnect binding', reconnected?.probe?.sessionId === 'session-b', reconnected?.probe?.sessionId ?? '');
    check('reconnect socket stays open', sockets[1].closed === false);
    registry.stop();
  }

  {
    activeBlock = 'reopen incarnation drops stale frames';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [loadedRow('session-a')] });
    const connected = await added;
    const first = registry.command(connected.id, { type: 'get_messages' });
    await flush();
    const firstSub = subscribeFrames(sockets[0])[0];
    check('first subscribe uses incarnation 1', firstSub?.sessionId === 'session-a' && firstSub?.runtimeIncarnation === incarnationOf('session-a') && firstSub?.cursor === 0, JSON.stringify(firstSub));
    sockets[0].emit(snapshotFrame('session-a', 4));
    await flush();
    sockets[0].respond('get_messages', true, { messages: [] });
    await first;
    sockets[0].emit({ type: 'message_start', sessionId: 'session-a', runtimeIncarnation: incarnationOf('session-a'), sequence: 7, itemId: 'a1' });

    const received: unknown[] = [];
    const unsubscribe = subscribeRunnerEvents(connected.id, (frame) => { received.push(frame); });
    const catalog = registry.command(connected.id, { type: 'session_list', scope: 'current' });
    await flush();
    sockets[0].respondLast('session_list', true, { sessions: [loadedRow('session-a', { runtimeIncarnation: 'inc-session-a-2' })] });
    await catalog;
    const reopened = registry.command(connected.id, { type: 'job_list' });
    await flush();
    const reopenSub = subscribeFrames(sockets[0])[1];
    check('reopen subscribe uses the new incarnation from cursor 0', reopenSub?.sessionId === 'session-a' && reopenSub?.runtimeIncarnation === 'inc-session-a-2' && reopenSub?.cursor === 0, JSON.stringify(reopenSub));
    sockets[0].emit(snapshotFrame('session-a', 2, { runtimeIncarnation: 'inc-session-a-2' }));
    await flush();
    const afterCurrent = received.length;
    check('current incarnation snapshot is accepted', afterCurrent === 1, String(afterCurrent));
    sockets[0].emit(snapshotFrame('session-a', 9, { runtimeIncarnation: incarnationOf('session-a') }));
    sockets[0].emit({ type: 'message_start', sessionId: 'session-a', runtimeIncarnation: incarnationOf('session-a'), sequence: 10, itemId: 'stale' });
    await flush();
    check('stale incarnation control and live frames are ignored', received.length === afterCurrent, String(received.length));
    sockets[0].respond('job_list', true, { jobs: [] });
    await reopened;
    check('current incarnation snapshot unblocks the reopened command', sentTypes(sockets[0]).includes('job_list'));
    unsubscribe();
    registry.stop();
  }

  {
    activeBlock = 'session_unloaded prunes exact current identity';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [loadedRow('session-a'), loadedRow('child-a')] });
    const connected = await added;
    const received: unknown[] = [];
    const unsubscribe = subscribeRunnerEvents(connected.id, (frame) => { received.push(frame); });
    const pending = registry.command(connected.id, { type: 'get_messages' }, { sourceSessionId: 'child-a' });
    await flush();
    check('current unload test armed the child subscribe', subscribeFrames(sockets[0])[0]?.sessionId === 'child-a' && subscribeFrames(sockets[0])[0]?.runtimeIncarnation === incarnationOf('child-a'));
    sockets[0].emit(unloadFrame('child-a'));
    await flush();
    const unloaded = received.filter((frame) => eventType(frame) === 'session_unloaded');
    check('current unload is dispatched once', unloaded.length === 1, String(unloaded.length));
    check('current unload keeps exact identity', eventIdentity(unloaded[0])?.sessionId === 'child-a' && eventIdentity(unloaded[0])?.runtimeIncarnation === incarnationOf('child-a'), JSON.stringify(unloaded[0]));
    const pendingError = await rejection(pending);
    check('current unload rejects the pending subscription', pendingError === 'Runner session identity is unavailable.', pendingError);
    check('current unload keeps the generation connected', registry.snapshot()[0]?.state === 'connected', registry.snapshot()[0]?.state ?? '');
    check('current unload does not close the socket', sockets[0].closed === false);
    check('current unload does not arm reconnect', ![...fakeTimers.scheduled.values()].some((timer) => timer.delay === RUNNER_RECONNECT_INITIAL_MS));
    const after = registry.command(connected.id, { type: 'job_list' }, { sourceSessionId: 'child-a' });
    const afterError = await rejection(after);
    check('unloaded identity cannot subscribe until it is relearned', afterError === 'Runner session identity is unavailable.', afterError);
    sockets[0].emit({ type: 'message_start', sessionId: 'child-a', runtimeIncarnation: incarnationOf('child-a'), sequence: 2, itemId: 'gone' });
    await flush();
    check('unloaded identity live frames are not dispatched', received.length === 1, String(received.length));
    unsubscribe();
    registry.stop();
  }

  {
    activeBlock = 'stale session_unloaded cannot drop new incarnation';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [loadedRow('session-a')] });
    const connected = await added;
    const first = registry.command(connected.id, { type: 'get_messages' });
    await flush();
    sockets[0].emit(snapshotFrame('session-a', 4));
    await flush();
    sockets[0].respond('get_messages', true, { messages: [] });
    await first;
    const received: unknown[] = [];
    const unsubscribe = subscribeRunnerEvents(connected.id, (frame) => { received.push(frame); });
    const catalog = registry.command(connected.id, { type: 'session_list', scope: 'current' });
    await flush();
    sockets[0].respondLast('session_list', true, { sessions: [loadedRow('session-a', { runtimeIncarnation: 'inc-session-a-2' })] });
    await catalog;
    const reopened = registry.command(connected.id, { type: 'job_list' });
    await flush();
    const reopenSub = subscribeFrames(sockets[0])[1];
    check('stale-unload reopen subscribe uses the new incarnation from cursor 0', reopenSub?.sessionId === 'session-a' && reopenSub?.runtimeIncarnation === 'inc-session-a-2' && reopenSub?.cursor === 0, JSON.stringify(reopenSub));
    sockets[0].emit(snapshotFrame('session-a', 2, { runtimeIncarnation: 'inc-session-a-2' }));
    await flush();
    sockets[0].respond('job_list', true, { jobs: [] });
    await reopened;
    sockets[0].emit(unloadFrame('session-a', { runtimeIncarnation: incarnationOf('session-a') }));
    await flush();
    const staleUnloads = received.filter((frame) => eventType(frame) === 'session_unloaded');
    check('stale unload remains observable', staleUnloads.length === 1 && eventIdentity(staleUnloads[0])?.runtimeIncarnation === incarnationOf('session-a'), JSON.stringify(staleUnloads[0]));
    sockets[0].emit({ type: 'message_start', sessionId: 'session-a', runtimeIncarnation: 'inc-session-a-2', sequence: 3, itemId: 'current' });
    await flush();
    check('stale unload cannot drop the new current identity', received.some((frame) => eventType(frame) === 'message_start' && eventIdentity(frame)?.runtimeIncarnation === 'inc-session-a-2'));
    const stillCurrent = registry.command(connected.id, { type: 'goal_get' });
    await flush();
    check('stale unload does not force a new subscribe', subscribeFrames(sockets[0]).length === 2, String(subscribeFrames(sockets[0]).length));
    sockets[0].respond('goal_get', true, {});
    await stillCurrent;
    unsubscribe();
    registry.stop();
  }

  {
    activeBlock = 'reopen after unload subscribes new incarnation from cursor 0';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [loadedRow('session-a'), loadedRow('child-a')] });
    const connected = await added;
    const primary = registry.command(connected.id, { type: 'get_messages' });
    await flush();
    sockets[0].emit(snapshotFrame('session-a', 1));
    await flush();
    sockets[0].respond('get_messages', true, { messages: [] });
    await primary;
    const child = registry.command(connected.id, { type: 'get_messages' }, { sourceSessionId: 'child-a' });
    await flush();
    sockets[0].emit(snapshotFrame('child-a', 4));
    await flush();
    sockets[0].respondLast('get_messages', true, { messages: [] });
    await child;
    const received: unknown[] = [];
    const unsubscribe = subscribeRunnerEvents(connected.id, (frame) => { received.push(frame); });
    sockets[0].emit(unloadFrame('child-a', { reason: 'closed' }));
    await flush();
    check('ready unload is dispatched once', received.length === 1 && eventType(received[0]) === 'session_unloaded', JSON.stringify(received[0]));
    const catalog = registry.command(connected.id, { type: 'session_list', scope: 'current' });
    await flush();
    sockets[0].respondLast('session_list', true, { sessions: [loadedRow('session-a'), loadedRow('child-a', { runtimeIncarnation: 'inc-child-a-2' })] });
    await catalog;
    const reopened = registry.command(connected.id, { type: 'job_list' }, { sourceSessionId: 'child-a' });
    await flush();
    const reopenSub = subscribeFrames(sockets[0]).at(-1);
    check('reopen after unload subscribes the new incarnation from cursor 0', reopenSub?.sessionId === 'child-a' && reopenSub?.runtimeIncarnation === 'inc-child-a-2' && reopenSub?.cursor === 0, JSON.stringify(reopenSub));
    sockets[0].emit(snapshotFrame('child-a', 1, { runtimeIncarnation: 'inc-child-a-2' }));
    await flush();
    sockets[0].respond('job_list', true, { jobs: [] });
    await reopened;
    check('reopen after unload unblocks on the new incarnation snapshot', sentTypes(sockets[0]).includes('job_list'));
    sockets[0].emit(unloadFrame('child-a'));
    await flush();
    check('stale unload of the previous incarnation cannot delete the reopened identity', received.filter((frame) => eventType(frame) === 'session_unloaded').length === 2, String(received.length));
    const stillOpen = registry.command(connected.id, { type: 'goal_get' }, { sourceSessionId: 'child-a' });
    await flush();
    check('reopened identity stays ready after stale unload', subscribeFrames(sockets[0]).filter((frame) => frame.sessionId === 'child-a').length === 2, JSON.stringify(subscribeFrames(sockets[0])));
    sockets[0].respond('goal_get', true, {});
    await stillOpen;
    unsubscribe();
    registry.stop();
  }

  {
    activeBlock = 'RunnerCommandError.rpcCode from rpcErrorCode(response.data)';
    const remote = (code: unknown, message = 'file is unavailable') => new RunnerCommandError(message, 'remote', {
      type: 'response',
      command: 'file_explorer_read',
      success: false,
      error: message,
      data: { code },
    });
    check('rpcCode comes from response data', remote('file_unavailable').rpcCode === 'file_unavailable');
    check('rpcCode ignores error message text', remote('busy_capacity').rpcCode === 'busy_capacity');
    check('unknown data.code is not an rpcCode', remote('not_a_closed_union_member').rpcCode === undefined);
    check('malformed data has no rpcCode', new RunnerCommandError('file is unavailable', 'remote', {
      type: 'response',
      command: 'file_explorer_read',
      success: false,
      error: 'file is unavailable',
      data: 'file_unavailable',
    }).rpcCode === undefined);
    check('transport errors have no rpcCode', new RunnerCommandError('Runner is not connected.', 'transport').rpcCode === undefined);

    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [loadedRow('session-a')] });
    const connected = await added;
    const pending = registry.command(connected.id, { type: 'file_explorer_read', path: 'src/lib.rs', maxLines: 1 });
    await flush();
    sockets[0].emit(snapshotFrame('session-a', 1));
    await flush();
    sockets[0].respondLast('file_explorer_read', false, { code: 'file_unavailable' }, 'file is unavailable');
    try {
      await pending;
      check('remote file_unavailable rejects', false);
    } catch (error) {
      check(
        'live remote reject stamps rpcCode from data, not message',
        error instanceof RunnerCommandError
          && error.kind === 'remote'
          && error.rpcCode === 'file_unavailable'
          && error.message === 'file is unavailable',
        error instanceof Error ? `${error.name}:${error.message}` : String(error),
      );
    }
    registry.stop();
  }

  {
    activeBlock = 'visibility wake retries a hung connecting HTTPS probe';
    const page = installWakePage();
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const fetched: string[] = [];
    const registry = new RunnerRegistry({
      fetchFn: statusFetch((url) => {
        fetched.push(url);
        if (fetched.length === 1) return new Promise<Response>(() => undefined);
        return okStatus(false);
      }),
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
      randomBytes: deterministicBytes,
    });
    registry.start();
    const hungAdd = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    check('hung preflight stays connecting', registry.snapshot()[0]?.state === 'connecting', registry.snapshot()[0]?.state ?? '');
    check('hung preflight fetched once', fetched.length === 1, String(fetched.length));
    page.document.visibilityState = 'hidden';
    page.dispatch('visibilitychange');
    await flush();
    check('hidden visibilitychange does not retry connecting', fetched.length === 1, String(fetched.length));
    page.document.visibilityState = 'visible';
    page.dispatch('visibilitychange');
    await flush();
    check('visible visibilitychange retries hung connecting', fetched.length === 2, String(fetched.length));
    check('visibility retry opened a socket', sockets.length === 1, String(sockets.length));
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [] });
    await flush();
    check('visibility wake of hung connecting settles connected', registry.snapshot()[0]?.state === 'connected', registry.snapshot()[0]?.state ?? '');
    registry.stop();
    check('stale hung first-probe rejects on stop', await rejection(hungAdd) === 'chat page closed');
    page.restore();
  }

  {
    activeBlock = 'focus wake reconnects after Open to trust without waiting backoff';
    const page = installWakePage();
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const fetched: string[] = [];
    const registry = new RunnerRegistry({
      fetchFn: statusFetch((url) => {
        fetched.push(url);
        if (fetched.length === 1) return new Error('TLS handshake failed');
        return okStatus(false);
      }),
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
      randomBytes: deterministicBytes,
    });
    registry.start();
    const firstAdd = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    const error = await rejection(firstAdd);
    check('cert fail uses unreachable copy', error === RUNNER_UNREACHABLE_MESSAGE, error);
    check('cert fail stays reconnecting', registry.snapshot()[0]?.state === 'reconnecting', registry.snapshot()[0]?.state ?? '');
    check('cert fail armed backoff', fakeTimers.scheduled.size === 1, String(fakeTimers.scheduled.size));
    page.dispatch('focus');
    await flush();
    check('focus wake retries without the backoff timer', fetched.length === 2, String(fetched.length));
    check('focus wake cancelled backoff', fakeTimers.scheduled.size === 0, String(fakeTimers.scheduled.size));
    check('focus wake opened a socket', sockets.length === 1, String(sockets.length));
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [] });
    await flush();
    check('focus wake after Open to trust settles connected', registry.snapshot()[0]?.state === 'connected', registry.snapshot()[0]?.state ?? '');
    const connectedSockets = sockets.length;
    page.dispatch('pageshow');
    await flush();
    check('pageshow does not reconnect an already connected runner', sockets.length === connectedSockets, String(sockets.length));
    registry.stop();
    page.restore();
  }

  {
    activeBlock = 'wake retries token_required only when a token is stored';
    const page = installWakePage();
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const fetched: string[] = [];
    const registry = new RunnerRegistry({
      fetchFn: statusFetch((url) => {
        fetched.push(url);
        return okStatus(true);
      }),
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
      randomBytes: deterministicBytes,
    });
    registry.start();
    const firstAdd = registry.add('Runner B', RUNNER_B_ORIGIN, '');
    await flush();
    await rejectedError(firstAdd);
    check('tokenless halt is token_required', registry.snapshot()[0]?.state === 'token_required', registry.snapshot()[0]?.state ?? '');
    page.dispatch('focus');
    await flush();
    check('wake does not retry token_required without a token', fetched.length === 1 && sockets.length === 0, `${fetched.length}:${sockets.length}`);
    const corrected = registry.updateToken(registry.snapshot()[0].id, AUTH_MARKER_A);
    await flush();
    sockets[0].close(1006, '');
    await flush();
    await rejectedError(corrected);
    check('set-token then 1006 is token_required with a stored token', registry.snapshot()[0]?.state === 'token_required', registry.snapshot()[0]?.state ?? '');
    const fetchesAfterToken = fetched.length;
    page.dispatch('pageshow');
    await flush();
    check('pageshow retries token_required after Set token', fetched.length === fetchesAfterToken + 1, String(fetched.length));
    check('pageshow after Set token opened a replacement socket', sockets.length === 2, String(sockets.length));
    sockets[1].open();
    await waitForCommand(sockets[1], 'get_state');
    sockets[1].respond('get_state', true, stateOf('session-b', { cwd: '<workspace>/runner-b' }));
    await flush();
    sockets[1].respond('session_list', true, { sessions: [] });
    await flush();
    check('wake after Set token and Open to trust settles connected', registry.snapshot()[0]?.state === 'connected', registry.snapshot()[0]?.state ?? '');
    registry.stop();
    page.restore();
  }

  {
    activeBlock = 'importRecords wakes parked token_required after token adoption';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const fetched: string[] = [];
    const registry = new RunnerRegistry({
      fetchFn: statusFetch((url) => {
        fetched.push(url);
        return okStatus(true);
      }),
      storage: store,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
      randomBytes: deterministicBytes,
    });
    registry.start();
    const firstAdd = registry.add('Runner B', RUNNER_B_ORIGIN, '');
    await flush();
    await rejectedError(firstAdd);
    const parked = registry.snapshot()[0];
    check('pre-adopt halt is token_required', parked?.state === 'token_required', parked?.state ?? '');
    check('pre-adopt opens no websocket', sockets.length === 0, String(sockets.length));
    const fetchesBeforeAdopt = fetched.length;
    const sameRecords = registry.importRecords(registry.exportRecords());
    await flush();
    check('empty-token import inserts nothing', sameRecords.inserted.length === 0, String(sameRecords.inserted.length));
    check('empty-token import leaves parked token_required', registry.snapshot()[0]?.state === 'token_required', registry.snapshot()[0]?.state ?? '');
    check('empty-token import does not preflight again', fetched.length === fetchesBeforeAdopt, String(fetched.length));
    saveRunnerToken(store, parked.authority, AUTH_MARKER_A);
    check('adopted token is nonempty before importRecords', store.getItem(hostTokenKey(parked.authority)) === AUTH_MARKER_A);
    const afterToken = registry.importRecords(registry.exportRecords());
    await flush();
    check('token-adopt import inserts nothing', afterToken.inserted.length === 0, String(afterToken.inserted.length));
    check('token-adopt import prefights again', fetched.length === fetchesBeforeAdopt + 1, String(fetched.length));
    check('token-adopt import opens a websocket', sockets.length === 1, String(sockets.length));
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-b', { cwd: '<workspace>/runner-b' }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [] });
    await flush();
    check('token-adopt import settles connected', registry.snapshot()[0]?.state === 'connected', registry.snapshot()[0]?.state ?? '');
    registry.stop();
  }

  {
    activeBlock = 'listen-local tmux commands skip chat subscription';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [loadedRow('session-a')] });
    const connected = await added;
    check('connected green runner has not subscribed', subscribeFrames(sockets[0]).length === 0);

    const tmuxCommands = [
      { type: 'tmux_list_sessions' },
      { type: 'tmux_pane_snapshot', session: 'dev' },
      { type: 'tmux_send_keys', session: 'dev', text: 'x' },
      { type: 'tmux_session_create', session: 'fresh' },
      { type: 'tmux_session_delete', session: 'dev' },
      { type: 'tmux_terminal_attach', session: 'dev', cols: 80, rows: 24 },
      { type: 'tmux_terminal_input', terminalId: 'term-1', data: 'YQ==' },
      { type: 'tmux_terminal_resize', terminalId: 'term-1', cols: 80, rows: 24 },
      { type: 'tmux_terminal_detach', terminalId: 'term-1' },
    ] as const;
    const pendingTmux = tmuxCommands.map((command) => registry.command(connected.id, command, {
      sourceSessionId: 'session-a',
    }));
    await flush();
    check('tmux ops do not open a chat subscribe', subscribeFrames(sockets[0]).length === 0);
    for (const command of tmuxCommands) {
      const frames = commandFrames<{ type?: string; sessionId?: string }>(sockets[0], command.type);
      check(`${command.type} sends before any chat snapshot`, frames.length === 1, String(frames.length));
      check(`${command.type} omits sessionId on the wire`, frames[0] !== undefined && !('sessionId' in frames[0]));
    }

    const pendingMessages = registry.command(connected.id, { type: 'get_messages' }, {
      sourceSessionId: 'session-a',
    });
    await flush();
    check('non-tmux commands still subscribe', subscribeFrames(sockets[0]).length === 1, String(subscribeFrames(sockets[0]).length));
    check('get_messages still waits for the snapshot', !sentTypes(sockets[0]).includes('get_messages'));

    sockets[0].emit(snapshotFrame('session-a', 1));
    await flush();
    check('snapshot unblocks only the session-scoped command', sentTypes(sockets[0]).includes('get_messages'));
    sockets[0].respond('get_messages', true, { messages: [] });
    await pendingMessages;
    for (const command of tmuxCommands) sockets[0].respond(command.type, true, {});
    await Promise.all(pendingTmux);
    registry.stop();
  }

  {
    activeBlock = 'tmux_terminal_resize timeout is 8s and does not fire at 3s';
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const registry = new RunnerRegistry({
      fetchFn: defaultFetch(),
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
    const added = registry.add('Runner A', RUNNER_A_ORIGIN, AUTH_MARKER_A);
    await flush();
    sockets[0].open();
    await waitForCommand(sockets[0], 'get_state');
    sockets[0].respond('get_state', true, stateOf('session-a', { cwd: SAFE_CWD_A }));
    await flush();
    sockets[0].respond('session_list', true, { sessions: [loadedRow('session-a')] });
    const connected = await added;
    const pendingResize = registry.command(connected.id, {
      type: 'tmux_terminal_resize',
      terminalId: 'term-1',
      cols: 80,
      rows: 24,
      fitWindow: true,
    });
    await flush();
    check(
      'resize timeout is 8s',
      [...fakeTimers.scheduled.values()].some((timer) => timer.delay === TMUX_TERMINAL_RESIZE_TIMEOUT_MS),
      JSON.stringify([...fakeTimers.scheduled.values()].map((timer) => timer.delay)),
    );
    check('3s does not retire a slow fit', !fakeTimers.runDelay(3000));
    check('runner stays connected at 3s', registry.snapshot()[0]?.state === 'connected', registry.snapshot()[0]?.state ?? '');
    check('8s resize timeout fires', fakeTimers.runDelay(TMUX_TERMINAL_RESIZE_TIMEOUT_MS));
    const resizeError = await rejection(pendingResize);
    check('slow fit times out as transport', resizeError === 'tmux_terminal_resize timed out.', resizeError);
    await flush();
    check('timed-out fit keeps the socket', registry.snapshot()[0]?.state === 'connected', registry.snapshot()[0]?.state ?? '');
    registry.stop();
  }

  {
    activeBlock = 'runner row context menu does not jump to /web';
    let prevented = false;
    let opened = false;
    const target = openRunnerContextMenuFromEvent('runner-1', {
      preventDefault: () => { prevented = true; },
      clientX: 24,
      clientY: 80,
    });
    check('contextmenu prevents the browser menu', prevented);
    check('contextmenu does not open /web', opened === false);
    check('menu targets the runner at the pointer',
      target.runnerId === 'runner-1' && target.x === 24 && target.y === 80,
      JSON.stringify(target));
    check('menu lists terminal, /web, settings, and copy URL',
      RUNNER_CONTEXT_ACTIONS.map((action) => action.id).join(',') === 'terminal,web,settings,copy-url'
      && RUNNER_CONTEXT_ACTIONS.map((action) => action.label).join('|') === 'Open terminal|Open /web|Runner settings|Copy web URL');

    const fired: string[] = [];
    const webUrl = 'https://runner.example/web#marker';
    for (const action of RUNNER_CONTEXT_ACTIONS) {
      applyRunnerContextAction(action.id, webUrl, {
        openTerminal: () => { fired.push('terminal'); },
        openWeb: (url) => { fired.push(`web:${url === webUrl ? 'url' : 'other'}`); },
        openSettings: () => { fired.push('settings'); },
        copyWebUrl: (url) => { fired.push(`copy:${url === webUrl ? 'url' : 'other'}`); },
      });
    }
    check('each menu action fires its own handler',
      fired.join(',') === 'terminal,web:url,settings,copy:url',
      fired.join(','));
    check('Open /web is an explicit menu action',
      fired.includes('web:url') && opened === false);

    check('mouse right-click does not arm long-press',
      !runnerLongPressShouldArm('mouse', 2) && !runnerLongPressShouldArm('mouse', 0));
    check('touch and pen primary press arm long-press',
      runnerLongPressShouldArm('touch', 0) && runnerLongPressShouldArm('pen', 0)
      && !runnerLongPressShouldArm('touch', 1));

    const fakeTimers = timers();
    let longPress: { runnerId: string; x: number; y: number } | null = null;
    scheduleRunnerLongPress('runner-1', 12, 40, fakeTimers.setTimeoutFn, (menu) => {
      longPress = menu;
    });
    check('long-press delay is 500ms',
      [...fakeTimers.scheduled.values()].some((timer) => timer.delay === RUNNER_MENU_LONG_PRESS_MS)
      && RUNNER_MENU_LONG_PRESS_MS === 500,
      JSON.stringify([...fakeTimers.scheduled.values()].map((timer) => timer.delay)));
    check('long-press does not fire before 500ms', !fakeTimers.runDelay(499) && longPress === null);
    check('long-press opens the same menu at the pointer',
      fakeTimers.runDelay(500)
      && longPress !== null
      && longPress.runnerId === 'runner-1'
      && longPress.x === 12
      && longPress.y === 40,
      JSON.stringify(longPress));
    check('long-press does not open /web', opened === false);
  }

  console.log(`\nchatRunners.test: ${assertions} assertions, ${failures.length} failure(s)`);
  if (failures.length) {
    console.error(failures.join('\n'));
    process.exit(1);
  }
}

main().catch((error) => {
  console.error(`chatRunners.test (${activeBlock}): ${error instanceof Error ? error.message : String(error)}`);
  process.exit(1);
});
