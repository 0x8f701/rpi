#!/usr/bin/env node
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import type { StorageLike } from '../src/hostToken.ts';
import type { RpcResponse } from '../src/types.ts';
import {
  CHAT_MAPPING_STORAGE_KEY,
  ChatMappingStore,
  loadMappingDocument,
  type ProjectRunnerMapping,
  type RunnerCommandClient,
} from '../src/chat/mapping.ts';
import type { RunnerCommand } from '../src/chat/protocol.ts';
import { RunnerCommandError, type RunnerView } from '../src/chat/runners.ts';

const SAFE_CWD = '/workspace/project';
const SOURCE_ID = 'source-session';
const TARGET_ID = 'mapped-session';
const TARGET_PATH = '/workspace/sessions/mapped.jsonl';
const SOURCE_PATH = '<workspace>/sessions/source.jsonl';
const RUNNER_ID = 'runner-a';
const SOURCE_DRIFT = `unknown session ${SOURCE_ID}`;
const TARGET_NOT_FOUND = 'session was not found';
const BOUNDED_SWITCH_FAILURE = 'session could not be opened right now; verify the selection and retry';
const BUSY_CAPACITY_FAILURE = 'too many busy sessions (limit 128); finish or stop active work and retry';
// Exact backend ledger literals (session_runtime_manager.rs
// command_ledger_failure): a replayed id with a different request digest,
// and an expired entry that provably cannot answer a re-send.
const COMMAND_ID_CONFLICT = 'command id was already used for a different request';
const COMMAND_EXPIRED = 'command result expired; issue a new command id';
function incarnationOf(sessionId: string): string {
  return `inc-${sessionId}`;
}

/** EXACT production `session_list` row shape for a loaded mapped native
 *  runtime (session_runtime_manager.rs session_list_row_value +
 *  overlay_loaded_sessions): rpi source, native status, loaded true, and a
 *  nonempty runtimeIncarnation overlay, alongside id/path/cwd. Every fake
 *  catalog row uses this shape; canonical-negative cases remove or corrupt
 *  exactly one stamp. */
function catalogRow(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    source: 'rpi',
    sessionId: TARGET_ID,
    runtimeIncarnation: incarnationOf(TARGET_ID),
    name: null,
    cwd: SAFE_CWD,
    path: TARGET_PATH,
    status: 'native',
    loaded: true,
    ...overrides,
  };
}

interface DurableStore extends StorageLike {
  values: Map<string, string>;
  writes: string[];
}

const failures: string[] = [];
let assertions = 0;
let activeBlock = 'module initialization';

function check(name: string, condition: boolean, detail = ''): void {
  assertions += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

function storage(): DurableStore {
  const values = new Map<string, string>();
  const writes: string[] = [];
  return {
    values,
    writes,
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => {
      writes.push(value);
      values.set(key, value);
    },
    removeItem: (key) => { values.delete(key); },
  };
}

function failingStorage(): StorageLike {
  return {
    getItem: () => null,
    setItem: () => { throw new Error('storage unavailable marker'); },
    removeItem: () => {},
  };
}

let byteSeed = 0;
function deterministicBytes(length: number): Uint8Array {
  byteSeed = (byteSeed + 1) % 240;
  return Uint8Array.from({ length }, (_, index) => byteSeed + index + 1);
}

function runnerView(sessionId = SOURCE_ID, state: RunnerView['state'] = 'connected'): RunnerView {
  return {
    id: RUNNER_ID,
    label: 'Runner A',
    origin: 'https://runner-a.example',
    authority: 'runner-a.example',
    createdAt: 1,
    updatedAt: 1,
    connectionEpoch: 1,
    state,
    error: '',
    probe: { sessionId, runtimeIncarnation: incarnationOf(sessionId), cwd: '<workspace>/runner', sessionCount: 1 },
  };
}

interface CommandCall {
  command: RunnerCommand;
  options: { commandId?: string; sourceSessionId?: string; allowBeforeProbe?: boolean };
}

class FakeRunnerClient implements RunnerCommandClient {
  readonly calls: CommandCall[] = [];
  // `executions` records what the runner actually EXECUTED (handler runs);
  // a ledger replay answers from its settled response without executing, so
  // duplicate-create assertions count executions, never wire frames.
  readonly executions: CommandCall[] = [];
  readonly rows: Array<Record<string, unknown>> = [];
  createResponse: RpcResponse | null = null;
  // Settled new_session failure literal (backend bounded_new_session_failure
  // shape): the create executes, the failure response settles into the
  // ledger, and the caller observes the same remote failure.
  createFailure: string | null = null;
  // Transport loss AFTER the command reached the runner and settled: the
  // ledger holds the outcome but the client never sees the response
  // (ambiguous transport — the mapProject intent stays create_outcome_unknown).
  loseCreateResponse = false;
  // Transport loss on a LEDGER REPLAY of a settled REPLAYABLE outcome
  // (a failure response carries no transcript): the settled outcome
  // replays but the connection drops before delivery — replayable, never a
  // re-execution.
  loseReplayResponse = false;
  // Client-side protocol verdict on a create replay (malformed response the
  // runners gate refuses before any runner-side meaning is assigned).
  createReplayProtocolFailure = false;
  canarySourceGone = false;
  archiveFailures = 0;
  nameTransportFailure = false;
  nameRemoteFailure = false;
  listFailure: 'transport' | 'remote' | null = null;
  switchFailure: 'transport' | 'remote' | null = null;
  listRemoteError: string | null = null;
  switchRemoteError: string | null = null;
  nameRemoteError: string | null = null;
  listResponseSessionId: string | null = null;
  switchResponseSessionId: string | null = null;
  nameResponseSessionId: string | null = null;
  listResponseCommand: string | null = null;
  switchResponseCommand: string | null = null;
  nameResponseCommand: string | null = null;
  subscribeError: string | null = null;
  subscribeSessionId: string | null = null;
  subscribeOmitMetadata = false;
  liveSessionId = SOURCE_ID;


  allowSwitchSession = false;
  connected = true;
  omitProbe = false;
  // Real command ledger (session_runtime_manager.rs ProtocolState): commands
  // are admitted under the client-supplied id together with a request
  // digest; a re-send with the same id and digest replays the settled
  // response verbatim and NEVER re-executes, while the same id with a
  // different digest is the exact command-id conflict. Expired entries keep
  // their digest and answer the fixed backend expired literal. Transport
  // failures admit nothing (the command never provably reached the runner).
  //
  // Transcript expiration mirrors complete_owned_command exactly: ANY settled
  // response whose data contains `messages` (every successful full-snapshot
  // lifecycle and get_messages response) moves straight into the expired set
  // with its request digest retained — delivered to the original caller, but
  // never replayable. Only message-free settled responses (failures,
  // get_state, session_list, set_session_name, session_delete) stay
  // replayable.
  private readonly ledger = new Map<string, { digest: string; settled: RpcResponse }>();
  private readonly expiredLedger = new Map<string, string>();

  /** Runner restart: the whole ledger is forgotten, so the next recovery
   *  canary executes fresh — canary SUCCESS (no continuity proof). */
  resetLedger(): void {
    this.ledger.clear();
    this.expiredLedger.clear();
  }

  /** Handler EXECUTIONS only — a replayed command never appears here, so
   *  "zero duplicate create" is asserted against executions, not wire frames. */
  executionsOf(commandType: string): number {
    return this.executions.filter((call) => call.command.type === commandType).length;
  }

  snapshot(): RunnerView[] {
    const view = runnerView(this.liveSessionId, this.connected ? 'connected' : 'reconnecting');
    return [this.omitProbe ? { ...view, probe: null } : view];
  }

  async command(
    _runnerId: string,
    command: RunnerCommand,
    options: { commandId?: string; sourceSessionId?: string; allowBeforeProbe?: boolean } = {},
  ): Promise<RpcResponse> {
    this.calls.push({ command, options });
    if (!this.connected && options.allowBeforeProbe !== true) {
      throw new RunnerCommandError('Runner is not connected.', 'transport');
    }
    if (this.subscribeError !== null) {
      if (this.subscribeOmitMetadata) {
        throw new RunnerCommandError(this.subscribeError, 'remote');
      }
      const sessionId = this.subscribeSessionId ?? options.sourceSessionId ?? SOURCE_ID;
      throw new RunnerCommandError(this.subscribeError, 'remote', undefined, {
        type: 'subscription_error',
        command: 'subscribe',
        sessionId,
        runtimeIncarnation: incarnationOf(sessionId),
        error: this.subscribeError,
      });
    }
    const id = options.commandId;
    if (id !== undefined) {
      const digest = JSON.stringify(command);
      const held = this.ledger.get(id);
      const expiredDigest = this.expiredLedger.get(id);
      if (held || expiredDigest !== undefined) {
        if ((held ? held.digest : expiredDigest) !== digest) {
          return this.failRemote(id, command.type, options, COMMAND_ID_CONFLICT);
        }
        if (!held) {
          // Exact digest against an expired ledger entry: the backend never
          // re-executes; it answers the fixed expired literal.
          return this.failRemote(id, command.type, options, COMMAND_EXPIRED);
        }
        if (this.loseReplayResponse) {
          // The settled response was replayed internally but the connection
          // dropped before delivery: ambiguous transport for the client.
          throw new RunnerCommandError('connection closed', 'transport');
        }
        if (this.createReplayProtocolFailure && command.type === 'new_session') {
          throw new RunnerCommandError('invalid response', 'protocol');
        }
        return this.deliver(held.settled);
      }
    }
    this.executions.push({ command, options });
    const settled = await this.execute(command, options, id);
    if (id !== undefined) {
      // complete_owned_command mirror: a settled response whose data carries
      // `messages` is EXPIRED from the moment it settles — its request digest
      // is retained, the response is delivered to the original caller, and a
      // stable-id re-send can only ever answer the fixed expired literal.
      if (settled.data !== null && typeof settled.data === 'object' && 'messages' in settled.data) {
        this.expiredLedger.set(id, JSON.stringify(command));
      } else {
        this.ledger.set(id, { digest: JSON.stringify(command), settled });
      }
    }
    if (command.type === 'new_session' && this.loseCreateResponse) {
      throw new RunnerCommandError('connection closed', 'transport');
    }
    return this.deliver(settled);
  }

  private failRemote(
    id: string | undefined,
    command: string,
    options: { sourceSessionId?: string },
    error: string,
  ): never {
    const failed = response(id, command, false, undefined, error, options.sourceSessionId ?? SOURCE_ID);
    throw new RunnerCommandError(failed.error || '', 'remote', failed);
  }

  private deliver(settled: RpcResponse): RpcResponse {
    if (!settled.success) {
      throw new RunnerCommandError(settled.error || '', 'remote', settled);
    }
    return settled;
  }

  private async execute(
    command: RunnerCommand,
    options: { commandId?: string; sourceSessionId?: string; allowBeforeProbe?: boolean },
    id: string | undefined,
  ): Promise<RpcResponse> {
    const sourceSessionId = options.sourceSessionId ?? SOURCE_ID;
    switch (command.type) {
      case 'get_state':
        return response(id, 'get_state', true, { sessionId: SOURCE_ID, runtimeIncarnation: incarnationOf(SOURCE_ID), cwd: '<workspace>/runner' });
      case 'get_messages': {
        if (this.canarySourceGone) {
          return response(id, 'get_messages', false, undefined, `unknown session ${sourceSessionId}`, sourceSessionId);
        }
        return response(id, 'get_messages', true, { messages: [] });
      }
      case 'new_session':
        if (this.createFailure !== null) {
          // Backend contract: lifecycle failures stay request-source-owned
          // and settle into the idempotency ledger as a failed response.
          return response(id, 'new_session', false, undefined, this.createFailure, sourceSessionId);
        }
        if (this.createResponse) return this.createResponse;
        // Backend contract (session_runtime_manager.rs snapshot_response):
        // the success envelope is TARGET-owned and agrees with data/state,
        // the data.session row, and the runtimeIncarnation.
        return response(id, 'new_session', true, {
          sessionId: TARGET_ID,
          runtimeIncarnation: incarnationOf(TARGET_ID),
          state: { sessionId: TARGET_ID, sessionFile: TARGET_PATH, cwd: command.cwd },
          session: {
            source: 'rpi',
            sessionId: TARGET_ID,
            runtimeIncarnation: incarnationOf(TARGET_ID),
            path: TARGET_PATH,
            cwd: command.cwd,
            status: 'native',
            loaded: true,
          },
          messages: [],
        }, undefined, TARGET_ID);
      case 'set_session_name':
        if (this.nameTransportFailure) {
          throw new RunnerCommandError('connection closed', 'transport');
        }
        if (this.nameRemoteFailure) {
          return response(
            id,
            this.nameResponseCommand ?? 'set_session_name',
            false,
            undefined,
            this.nameRemoteError ?? 'name rejected marker',
            this.nameResponseSessionId ?? SOURCE_ID,
          );
        }
        this.rows.splice(0, this.rows.length, catalogRow({ name: command.name }));
        return response(id, 'set_session_name', true, {});
      case 'session_list': {
        if (this.listFailure === 'transport') {
          throw new RunnerCommandError('connection closed', 'transport');
        }
        if (this.listFailure === 'remote') {
          return response(
            id,
            this.listResponseCommand ?? 'session_list',
            false,
            undefined,
            this.listRemoteError ?? 'catalog rejected marker',
            this.listResponseSessionId ?? sourceSessionId,
          );
        }
        return response(
          id,
          'session_list',
          true,
          { sessions: [...this.rows] },
          undefined,
          sourceSessionId,
        );
      }
      case 'session_delete': {
        if (this.archiveFailures > 0) {
          this.archiveFailures -= 1;
          return response(
            id,
            'session_delete',
            false,
            undefined,
            'archive rejected marker',
            sourceSessionId,
          );
        }
        if (options.sourceSessionId && options.sourceSessionId !== this.liveSessionId) {
          return response(
            id,
            'session_delete',
            false,
            undefined,
            `unknown session ${options.sourceSessionId}`,
            sourceSessionId,
          );
        }
        this.rows.splice(0, this.rows.length);
        return response(
          id,
          'session_delete',
          true,
          { archived: true, sessionId: TARGET_ID },
          undefined,
          sourceSessionId,
        );
      }
      case 'switch_session': {
        if (!this.allowSwitchSession) throw new Error('unexpected switch_session');
        if (this.switchFailure === 'transport') {
          throw new RunnerCommandError('connection closed', 'transport');
        }
        if (this.switchFailure === 'remote') {
          return response(
            id,
            this.switchResponseCommand ?? 'switch_session',
            false,
            undefined,
            this.switchRemoteError ?? 'switch rejected marker',
            this.switchResponseSessionId ?? sourceSessionId,
          );
        }
        if (this.switchResponse) return this.switchResponse;
        if (command.sessionPath === TARGET_PATH) {
          this.liveSessionId = TARGET_ID;
          return response(id, 'switch_session', true, {
            sessionId: TARGET_ID,
            runtimeIncarnation: incarnationOf(TARGET_ID),
            state: { sessionId: TARGET_ID, sessionFile: TARGET_PATH, cwd: SAFE_CWD },
            session: {
              source: 'rpi',
              sessionId: TARGET_ID,
              runtimeIncarnation: incarnationOf(TARGET_ID),
              path: TARGET_PATH,
              cwd: SAFE_CWD,
              status: 'native',
              loaded: true,
            },
            messages: [],
          }, undefined, TARGET_ID);
        }
        const row = this.rows.find((candidate) => candidate.path === command.sessionPath);
        const sessionId = typeof row?.sessionId === 'string' && row.sessionId !== '' ? row.sessionId : SOURCE_ID;
        const cwd = typeof row?.cwd === 'string' && row.cwd !== '' ? row.cwd : '<workspace>/runner';
        this.liveSessionId = sessionId;
        return response(id, 'switch_session', true, {
          sessionId,
          runtimeIncarnation: incarnationOf(sessionId),
          state: { sessionId, sessionFile: command.sessionPath, cwd },
          session: {
            source: 'rpi',
            sessionId,
            runtimeIncarnation: incarnationOf(sessionId),
            path: command.sessionPath,
            cwd,
            status: 'native',
            loaded: true,
          },
          messages: [],
        }, undefined, sessionId);
      }
    }
  }
}

function response(
  id: string | undefined,
  command: string,
  success: boolean,
  data?: unknown,
  error?: string,
  sessionId = SOURCE_ID,
): RpcResponse {
  return { type: 'response', id, command, sessionId, success, data, error };
}

function immediateLock<T>(callback: () => Promise<T>): Promise<T> {
  return callback();
}
class FakeVersionLockFactory {
  version = 0;
  openCalls = 0;
  heldConnection: { closed: boolean } | null = null;

  open(_name: string, requestedVersion?: number): IDBOpenDBRequest {
    this.openCalls += 1;
    const request: {
      result: IDBDatabase;
      error: DOMException | null;
      onupgradeneeded: ((event: IDBVersionChangeEvent) => void) | null;
      onsuccess: ((event: Event) => void) | null;
      onerror: ((event: Event) => void) | null;
    } = {
      result: undefined as unknown as IDBDatabase,
      error: null,
      onupgradeneeded: null,
      onsuccess: null,
      onerror: null,
    };

    queueMicrotask(() => {
      const nextVersion = requestedVersion ?? Math.max(this.version, 1);
      if (requestedVersion !== undefined && requestedVersion <= this.version) {
        request.error = new DOMException('version race', 'VersionError');
        request.onerror?.({} as Event);
        return;
      }
      const upgraded = nextVersion > this.version;
      this.version = nextVersion;
      const connection = {
        version: nextVersion,
        closed: false,
        close() { this.closed = true; },
      };
      request.result = connection as unknown as IDBDatabase;
      if (requestedVersion !== undefined) this.heldConnection = connection;
      if (upgraded) request.onupgradeneeded?.({} as IDBVersionChangeEvent);
      request.onsuccess?.({} as Event);
    });
    return request as unknown as IDBOpenDBRequest;
  }
}

async function withBrowserLockGlobals<T>(
  navigatorValue: unknown,
  indexedDbValue: unknown,
  callback: () => Promise<T>,
): Promise<T> {
  const navigatorDescriptor = Object.getOwnPropertyDescriptor(globalThis, 'navigator');
  const indexedDbDescriptor = Object.getOwnPropertyDescriptor(globalThis, 'indexedDB');
  Object.defineProperty(globalThis, 'navigator', { configurable: true, value: navigatorValue });
  Object.defineProperty(globalThis, 'indexedDB', { configurable: true, value: indexedDbValue });
  try {
    return await callback();
  } finally {
    if (navigatorDescriptor) Object.defineProperty(globalThis, 'navigator', navigatorDescriptor);
    else delete (globalThis as { navigator?: unknown }).navigator;
    if (indexedDbDescriptor) Object.defineProperty(globalThis, 'indexedDB', indexedDbDescriptor);
    else delete (globalThis as { indexedDB?: unknown }).indexedDB;
  }
}

async function rejection(promise: Promise<unknown>): Promise<string> {
  try {
    await promise;
    return '';
  } catch (error) {
    return error instanceof Error ? error.message : String(error);
  }
}

function storedMappings(store: DurableStore): ProjectRunnerMapping[] {
  const loaded = loadMappingDocument(store);
  return loaded.kind === 'blocked' ? [] : loaded.document.mappings;
}

async function createProject(store: ChatMappingStore): Promise<string> {
  return (await store.addProject('Product', SAFE_CWD)).id;
}

function mappingRegistry(durable: DurableStore, client: FakeRunnerClient): ChatMappingStore {
  return new ChatMappingStore({
    storage: durable,
    runners: client,
    withExclusiveLock: immediateLock,
    randomBytes: deterministicBytes,
  });
}

async function seedMapped(state: 'active' | 'provisioning' = 'active'): Promise<{
  durable: DurableStore;
  client: FakeRunnerClient;
  registry: ChatMappingStore;
  mapping: Extract<ProjectRunnerMapping, { state: 'active' | 'provisioning' }>;
}> {
  const durable = storage();
  const client = new FakeRunnerClient();
  client.allowSwitchSession = true;
  const registry = mappingRegistry(durable, client);
  const projectId = await createProject(registry);
  await registry.mapProject(projectId, RUNNER_ID);
  const loaded = loadMappingDocument(durable);
  if (loaded.kind === 'blocked') throw new Error(loaded.error);
  const current = loaded.document.mappings[0];
  if (current?.state !== 'active') throw new Error('seed fixture missing');
  if (state === 'active') return { durable, client, registry, mapping: current };
  const provisioning = { ...current, state: 'provisioning' as const };
  durable.values.set(CHAT_MAPPING_STORAGE_KEY, JSON.stringify({
    ...loaded.document,
    mappings: [provisioning],
  }));
  return { durable, client, registry, mapping: provisioning };
}



async function main(): Promise<void> {
  activeBlock = 'browser mutation lock fallback';
  {
    const factory = new FakeVersionLockFactory();
    const durable = storage();
    let lockOpenDuringWrite = false;
    const persist = durable.setItem.bind(durable);
    durable.setItem = (key, value) => {
      lockOpenDuringWrite = factory.heldConnection?.closed === false;
      persist(key, value);
    };
    const registry = new ChatMappingStore({
      storage: durable,
      runners: new FakeRunnerClient(),
      randomBytes: deterministicBytes,
    });
    await withBrowserLockGlobals({}, factory, () => registry.addProject('Product', SAFE_CWD));
    check('IndexedDB upgraded connection stays open through callback', lockOpenDuringWrite);
    check('IndexedDB upgraded connection closes after callback', factory.heldConnection?.closed === true);
    check('IndexedDB fallback executes mutation callback once', durable.writes.length === 1, String(durable.writes.length));
    check('IndexedDB fallback reads then upgrades the database version', factory.openCalls === 2, String(factory.openCalls));

    const failingFactory = new FakeVersionLockFactory();
    const callbackError = await withBrowserLockGlobals({}, failingFactory, () => rejection(new ChatMappingStore({
      storage: failingStorage(),
      runners: new FakeRunnerClient(),
      randomBytes: deterministicBytes,
    }).addProject('Product', SAFE_CWD)));
    check('IndexedDB fallback propagates callback errors', callbackError === 'Project storage is unavailable.', callbackError);
    check('IndexedDB connection closes after callback error', failingFactory.heldConnection?.closed === true);

    let webLockCalls = 0;
    const webLockDurable = storage();
    const webLockNavigator = { locks: { request: async <T>(
      name: string,
      options: { mode: string },
      callback: () => Promise<T>,
    ): Promise<T> => {
      webLockCalls += 1;
      check('Web Locks uses mapping lock name and exclusive mode', name === 'rpi-web-chat-mapping' && options.mode === 'exclusive');
      return callback();
    } } };
    const forbiddenIdb = { open: () => { throw new Error('IndexedDB must not be used'); } };
    await withBrowserLockGlobals(webLockNavigator, forbiddenIdb, () => new ChatMappingStore({
      storage: webLockDurable,
      runners: new FakeRunnerClient(),
      randomBytes: deterministicBytes,
    }).addProject('Product', SAFE_CWD));
    check('browser lock prefers Web Locks', webLockCalls === 1);

    const unavailableDurable = storage();
    const unavailableError = await withBrowserLockGlobals({}, undefined, () => rejection(new ChatMappingStore({
      storage: unavailableDurable,
      runners: new FakeRunnerClient(),
      randomBytes: deterministicBytes,
    }).addProject('Product', SAFE_CWD)));
    check('missing Web Locks and IndexedDB fails closed',
      unavailableError === 'Project mutation locking is unavailable.', unavailableError);
    check('fail-closed locking executes no mutation callback', unavailableDurable.writes.length === 0);

    const here = dirname(fileURLToPath(import.meta.url));
    const source = readFileSync(join(here, '../src/chat/mapping.ts'), 'utf8');
    check('IndexedDB lock uses bounded version-race retries',
      source.includes('const CHAT_MAPPING_LOCK_MAX_RACES = 8')
      && source.includes('attempt < CHAT_MAPPING_LOCK_MAX_RACES'));
    check('blocked IndexedDB request has bounded wait and closes late success',
      source.includes('const CHAT_MAPPING_LOCK_WAIT_MS = 120_000')
      && source.includes('if (settled) {')
      && source.includes('database.close()'));
    check('upgraded connection remains open around callback and closes in finally',
      source.includes('return await callback();')
      && source.includes('finally {\n    database.close();'));
  }

  activeBlock = 'strict storage';
  {
    const store = storage();
    store.values.set(CHAT_MAPPING_STORAGE_KEY, '{bad json');
    const loaded = loadMappingDocument(store);
    check('corrupt mapping storage fails closed', loaded.kind === 'blocked');
    const client = new FakeRunnerClient();
    const registry = new ChatMappingStore({
      storage: store,
      runners: client,
      withExclusiveLock: immediateLock,
      randomBytes: deterministicBytes,
    });
    const error = await rejection(registry.addProject('Product', SAFE_CWD));
    check('corrupt storage blocks mutation', error === 'Project storage is corrupt.', error);
    check('corrupt storage sends no runner command', client.calls.length === 0);
  }

  {
    activeBlock = 'duplicate project cwd';
    const durable = storage();
    const client = new FakeRunnerClient();
    const registry = new ChatMappingStore({
      storage: durable,
      runners: client,
      withExclusiveLock: immediateLock,
      randomBytes: deterministicBytes,
    });
    const first = await registry.addProject('Product', SAFE_CWD);
    const projectsBefore = registry.snapshot().projects.length;
    const error = await rejection(registry.addProject('Second', SAFE_CWD));
    check('duplicate cwd rejects with exact message', error === 'A project already uses this directory.', error);
    check('duplicate cwd keeps the existing record', registry.snapshot().projects.length === projectsBefore
      && registry.snapshot().projects[0]?.id === first.id);
    check('duplicate cwd persists no second project', storedMappings(durable).length === 0);
    check('duplicate cwd sends no runner command', client.calls.length === 0);
  }

  {
    activeBlock = 'local project cwd validation';
    // Empty/whitespace and relative directories fail closed locally with
    // the one actionable message, before any runner command is sent.
    const durable = storage();
    const client = new FakeRunnerClient();
    const registry = new ChatMappingStore({
      storage: durable,
      runners: client,
      withExclusiveLock: immediateLock,
      randomBytes: deterministicBytes,
    });
    for (const cwd of ['', '   ', 'workspace/relative']) {
      const error = await rejection(registry.addProject('Product', cwd));
      check(`cwd ${JSON.stringify(cwd)} rejects with the absolute-directory message`,
        error === 'Project directory must be absolute.', error);
      check(`cwd ${JSON.stringify(cwd)} persists no project`, registry.snapshot().projects.length === 0);
      check(`cwd ${JSON.stringify(cwd)} sends no runner command`, client.calls.length === 0);
    }
  }

  {
    activeBlock = 'new session directory-category rejection';
    // The runner accepts any existing local absolute directory; a rejected
    // create surfaces its fixed, path-free category literal exactly once,
    // removes only the new intent mapping, keeps the project, and never
    // retries the create. The frontend allowlist is the EXACT backend
    // literal set (bounded_new_session_failure) — never prefix-shaped, so a
    // novel "new session …" remote shape fails closed to the generic
    // rejection instead of leaking runner-internal text.
    const allowlist = [
      'new session cwd must be an absolute directory path',
      'new session directory path is invalid',
      'new session path must not be a symbolic link',
      'new session path is not a directory',
      'new session directory access is denied',
      'new session directory does not exist',
      // bounded_new_session_failure's fallthrough category.
      BOUNDED_SWITCH_FAILURE,
      // Exact busy-capacity literal (MAX_LOADED_SESSIONS = 128): the limit
      // number is compiled into the backend, so only limit 128 surfaces.
      'too many busy sessions (limit 128); finish or stop active work and retry',
    ];
    for (const remote of allowlist) {
      const durable = storage();
      const client = new FakeRunnerClient();
      const registry = new ChatMappingStore({
        storage: durable,
        runners: client,
        withExclusiveLock: immediateLock,
        randomBytes: deterministicBytes,
      });
      const projectId = await createProject(registry);
      const original = client.command.bind(client);
      client.command = async (runnerId, command, options = {}) => {
        if (command.type === 'new_session') {
          client.calls.push({ command, options });
          const failed = response(
            options.commandId,
            'new_session',
            false,
            undefined,
            remote,
            options.sourceSessionId ?? SOURCE_ID,
          );
          throw new RunnerCommandError(failed.error || '', 'remote', failed);
        }
        return original(runnerId, command, options);
      };
      const error = await rejection(registry.mapProject(projectId, RUNNER_ID));
      check(`${remote}: rejection surfaces the bounded reason`, error === remote, error);
      check(`${remote}: rejection removes the mapping`, storedMappings(durable).length === 0);
      check(`${remote}: rejection keeps the project`, registry.snapshot().projects.length === 1);
      check(`${remote}: exactly one create attempt, no retries`, client.calls.filter(
        (call) => call.command.type === 'new_session').length === 1);
      check(`${remote}: rejection keeps other mappings untouched`, storedMappings(durable)
        .every((mapping) => mapping.projectId !== projectId));
      check(`${remote}: create preserved the exact source session`, client.calls.find(
        (call) => call.command.type === 'new_session')?.options.sourceSessionId === SOURCE_ID);
      check(`${remote}: bounded reason carries no operator path`, !error.includes('/') && !error.includes('\\'));
    }
    for (const remote of [
      'new session directory does not exist: /workspace/missing',
      // Exact busy shape but a wrong compiled limit: novel, fails closed.
      'too many busy sessions (limit 4); finish or stop active work and retry',
      'too many busy sessions (limit 4); every loaded session has work in progress',
      // The bounded literal on a non-lifecycle command never passes.
      'session was not found',
    ]) {
      const durable = storage();
      const client = new FakeRunnerClient();
      const registry = new ChatMappingStore({
        storage: durable,
        runners: client,
        withExclusiveLock: immediateLock,
        randomBytes: deterministicBytes,
      });
      const projectId = await createProject(registry);
      const original = client.command.bind(client);
      client.command = async (runnerId, command, options = {}) => {
        if (command.type === 'new_session') {
          client.calls.push({ command, options });
          const failed = response(
            options.commandId,
            'new_session',
            false,
            undefined,
            remote,
            options.sourceSessionId ?? SOURCE_ID,
          );
          throw new RunnerCommandError(failed.error || '', 'remote', failed);
        }
        return original(runnerId, command, options);
      };
      const error = await rejection(registry.mapProject(projectId, RUNNER_ID));
      check(`${remote}: novel shape fails closed to the generic rejection`,
        error === 'Runner rejected the project operation.', error);
      check(`${remote}: novel shape never leaks runner text`,
        !error.includes('new session') && !error.includes('busy') && !error.includes('/'), error);
      check(`${remote}: novel shape removes the failed intent mapping`, storedMappings(durable).length === 0);
      check(`${remote}: novel shape keeps the project`, registry.snapshot().projects.length === 1);
    }
  }

  {
    activeBlock = 'successful mapping';
    const durable = storage();
    const client = new FakeRunnerClient();
    const registry = new ChatMappingStore({
      storage: durable,
      runners: client,
      withExclusiveLock: immediateLock,
      now: () => 100,
      randomBytes: deterministicBytes,
    });
    const projectId = await createProject(registry);
    const mapped = await registry.mapProject(projectId, RUNNER_ID);
    check('successful mapping becomes active', mapped.state === 'active', mapped.state);
    const commandTypes = client.calls.map((call) => call.command.type);
    check('canary precedes create', commandTypes.indexOf('get_state') < commandTypes.indexOf('new_session'));
    const createIndex = client.calls.findIndex((call) => call.command.type === 'new_session');
    check('create intent persisted before remote create', durable.writes.some((raw) =>
      raw.includes('create_outcome_unknown')), String(createIndex));
    check('create uses stable command id', client.calls[createIndex].options.commandId?.endsWith(':create') === true);
    check('naming targets primary source', client.calls.find((call) => call.command.type === 'set_session_name')
      ?.options.sourceSessionId === SOURCE_ID);
    check('naming passes exact target path', (client.calls.find((call) => call.command.type === 'set_session_name')
      ?.command as { sessionPath?: string }).sessionPath === TARGET_PATH);
    check('durable active mapping retains target identity', storedMappings(durable)[0]?.state === 'active');
    const raw = durable.values.get(CHAT_MAPPING_STORAGE_KEY) || '';
    check('mapping JSON contains no runner token', !raw.toLowerCase().includes('token'));
  }

  {
    activeBlock = 'target-owned lifecycle envelope';
    // The success envelope is TARGET-owned: envelope, data, and state must
    // all carry the exact same opened-session identity. A source-stamped
    // (legacy) envelope or any identity disagreement must fail closed —
    // never provision, never persist a target identity.
    async function envelopeCase(
      data: unknown,
      envelopeSessionId = TARGET_ID,
    ): Promise<{ error: string; mapping: ProjectRunnerMapping | undefined }> {
      const durable = storage();
      const client = new FakeRunnerClient();
      const registry = new ChatMappingStore({
        storage: durable,
        runners: client,
        withExclusiveLock: immediateLock,
        randomBytes: deterministicBytes,
      });
      const projectId = await createProject(registry);
      client.createResponse = {
        type: 'response',
        id: undefined,
        command: 'new_session',
        sessionId: envelopeSessionId,
        success: true,
        data,
      };
      const error = await rejection(registry.mapProject(projectId, RUNNER_ID));
      return { error, mapping: storedMappings(durable)[0] };
    }
    const targetData = {
      sessionId: TARGET_ID,
      runtimeIncarnation: incarnationOf(TARGET_ID),
      state: { sessionId: TARGET_ID, sessionFile: TARGET_PATH, cwd: SAFE_CWD },
      session: {
        source: 'rpi',
        sessionId: TARGET_ID,
        runtimeIncarnation: incarnationOf(TARGET_ID),
        path: TARGET_PATH,
        cwd: SAFE_CWD,
        status: 'native',
        loaded: true,
      },
      messages: [],
    };
    const sourceEnvelope = await envelopeCase(targetData, SOURCE_ID);
    check('source-owned success envelope is never provisioned',
      sourceEnvelope.error.includes('reconciled'), sourceEnvelope.error);
    check('source-owned envelope keeps the create outcome unknown',
      sourceEnvelope.mapping?.state === 'create_outcome_unknown', sourceEnvelope.mapping?.state);
    check('source-owned envelope persists no session identity',
      sourceEnvelope.mapping?.sessionId === undefined && sourceEnvelope.mapping?.sessionPath === undefined);

    const stateDrift = await envelopeCase({
      ...targetData,
      state: { sessionId: 'other-target', sessionFile: TARGET_PATH, cwd: SAFE_CWD },
    });
    check('state identity disagreement stays reconciling', stateDrift.error.includes('reconciled'), stateDrift.error);
    check('state identity disagreement persists no provisioned mapping',
      stateDrift.mapping?.sessionId === undefined, stateDrift.mapping?.state);

    const dataDrift = await envelopeCase({
      ...targetData,
      sessionId: 'other-target',
    });
    check('data identity disagreement stays reconciling', dataDrift.error.includes('reconciled'), dataDrift.error);
    check('data identity disagreement persists no provisioned mapping',
      dataDrift.mapping?.sessionId === undefined, dataDrift.mapping?.state);

    const missingEnvelope = await envelopeCase(targetData, '');
    check('envelope-less lifecycle success fails closed', missingEnvelope.error.includes('reconciled'), missingEnvelope.error);
    const provisioned = await envelopeCase(targetData);
    check('target-owned envelope provisions the mapping',
      provisioned.mapping?.state === 'provisioning' || provisioned.mapping?.state === 'active',
      provisioned.error);
    check('target-owned envelope persists the exact target identity',
      provisioned.mapping?.sessionId === TARGET_ID && provisioned.mapping?.sessionPath === TARGET_PATH);

    // Canonical identity incarnation: a missing, empty, or row-disagreeing
    // runtimeIncarnation fails the lifecycle success closed even when every
    // sessionId agrees.
    const missingIncarnation = await envelopeCase({
      ...targetData,
      runtimeIncarnation: undefined,
    });
    check('missing runtimeIncarnation stays reconciling', missingIncarnation.error.includes('reconciled'), missingIncarnation.error);
    check('missing runtimeIncarnation persists no provisioned mapping',
      missingIncarnation.mapping?.sessionId === undefined, missingIncarnation.mapping?.state);

    const emptyIncarnation = await envelopeCase({
      ...targetData,
      runtimeIncarnation: '   ',
    });
    check('empty runtimeIncarnation stays reconciling', emptyIncarnation.error.includes('reconciled'), emptyIncarnation.error);

    const rowIncarnationDrift = await envelopeCase({
      ...targetData,
      session: { sessionId: TARGET_ID, runtimeIncarnation: incarnationOf('other-target') },
    });
    check('row incarnation disagreement stays reconciling',
      rowIncarnationDrift.error.includes('reconciled'), rowIncarnationDrift.error);
    check('row incarnation disagreement persists no provisioned mapping',
      rowIncarnationDrift.mapping?.sessionId === undefined, rowIncarnationDrift.mapping?.state);

    const rowSessionDrift = await envelopeCase({
      ...targetData,
      session: { sessionId: 'other-target', runtimeIncarnation: incarnationOf(TARGET_ID) },
    });
    check('row session disagreement stays reconciling',
      rowSessionDrift.error.includes('reconciled'), rowSessionDrift.error);

    const missingRow = await envelopeCase({
      ...targetData,
      session: undefined,
    });
    check('missing session row stays reconciling', missingRow.error.includes('reconciled'), missingRow.error);

    // Complete canonical row drift: every canonical field the shared
    // validator binds must agree exactly — path with the state sessionFile,
    // cwd with the state snapshot AND the requested project directory, and
    // the native status/loaded/source stamps. Any single-field drift fails
    // the lifecycle success closed (stays reconciling, never provisioned).
    async function rowDriftCase(
      name: string,
      row: Record<string, unknown>,
      stateCwd?: string,
    ): Promise<void> {
      const outcome = await envelopeCase({
        ...targetData,
        state: { sessionId: TARGET_ID, sessionFile: TARGET_PATH, cwd: stateCwd ?? SAFE_CWD },
        session: row,
      });
      check(`${name} stays reconciling`, outcome.error.includes('reconciled'), outcome.error);
      check(`${name} persists no provisioned mapping`,
        outcome.mapping?.sessionId === undefined && outcome.mapping?.sessionPath === undefined,
        outcome.mapping?.state);
    }
    await rowDriftCase('row path drift from the state snapshot', {
      source: 'rpi', sessionId: TARGET_ID, runtimeIncarnation: incarnationOf(TARGET_ID),
      path: '/workspace/sessions/other.jsonl', cwd: SAFE_CWD, status: 'native', loaded: true,
    });
    await rowDriftCase('row cwd drift from the state snapshot', {
      source: 'rpi', sessionId: TARGET_ID, runtimeIncarnation: incarnationOf(TARGET_ID),
      path: TARGET_PATH, cwd: '/workspace/other', status: 'native', loaded: true,
    });
    await rowDriftCase('row cwd drift from the requested project directory', {
      source: 'rpi', sessionId: TARGET_ID, runtimeIncarnation: incarnationOf(TARGET_ID),
      path: TARGET_PATH, cwd: SAFE_CWD, status: 'native', loaded: true,
    }, '/workspace/other');
    await rowDriftCase('row cwd disagreeing only with state cwd', {
      source: 'rpi', sessionId: TARGET_ID, runtimeIncarnation: incarnationOf(TARGET_ID),
      path: TARGET_PATH, cwd: SAFE_CWD, status: 'native', loaded: true,
    }, '/workspace/other');
    await rowDriftCase('row foreign status', {
      source: 'rpi', sessionId: TARGET_ID, runtimeIncarnation: incarnationOf(TARGET_ID),
      path: TARGET_PATH, cwd: SAFE_CWD, status: 'foreign', loaded: true,
    });
    await rowDriftCase('row missing status', {
      source: 'rpi', sessionId: TARGET_ID, runtimeIncarnation: incarnationOf(TARGET_ID),
      path: TARGET_PATH, cwd: SAFE_CWD, loaded: true,
    });
    await rowDriftCase('row unloaded status', {
      source: 'rpi', sessionId: TARGET_ID, runtimeIncarnation: incarnationOf(TARGET_ID),
      path: TARGET_PATH, cwd: SAFE_CWD, status: 'native', loaded: false,
    });
    await rowDriftCase('row missing loaded flag', {
      source: 'rpi', sessionId: TARGET_ID, runtimeIncarnation: incarnationOf(TARGET_ID),
      path: TARGET_PATH, cwd: SAFE_CWD, status: 'native',
    });
    // The canonical native source is UNCONDITIONAL: the backend always stamps
    // lifecycle targets `source: "rpi"` (lifecycle_target_session_row), so a
    // missing, null, empty, whitespace, non-string, or foreign source fails
    // closed — the row is never accepted without the exact rpi token.
    for (const [sourceLabel, source] of [
      ['missing source', undefined],
      ['null source', null],
      ['empty source', ''],
      ['whitespace source', ' rpi '],
      ['nonstring source', 7],
      ['foreign source', 'pi'],
    ] as const) {
      const outcome = await envelopeCase({
        ...targetData,
        session: {
          source,
          sessionId: TARGET_ID,
          runtimeIncarnation: incarnationOf(TARGET_ID),
          path: TARGET_PATH,
          cwd: SAFE_CWD,
          status: 'native',
          loaded: true,
        },
      });
      check(`${sourceLabel} lifecycle row stays reconciling`, outcome.error.includes('reconciled'), outcome.error);
      check(`${sourceLabel} lifecycle row persists no provisioned mapping`,
        outcome.mapping?.sessionId === undefined && outcome.mapping?.sessionPath === undefined,
        outcome.mapping?.state);
    }
  }

  {
    activeBlock = 'invite idempotency';
    const durable = storage();
    const client = new FakeRunnerClient();
    const registry = new ChatMappingStore({
      storage: durable,
      runners: client,
      withExclusiveLock: immediateLock,
      now: () => 100,
      randomBytes: deterministicBytes,
    });
    const projectId = await createProject(registry);
    const first = await registry.mapProject(projectId, RUNNER_ID);
    const callsAfterFirst = client.calls.length;
    const second = await registry.mapProject(projectId, RUNNER_ID);
    check('second invite returns the existing mapping', second.id === first.id && second.state === 'active');
    check('second invite sends no new runner command', client.calls.length === callsAfterFirst, String(client.calls.length));
    check('second invite keeps a single durable mapping', storedMappings(durable).length === 1);
  }

  {
    activeBlock = 'lost create response continuity';
    // The create reaches the runner and settles into its ledger, but the
    // connection drops before the response is delivered: the mapProject
    // intent survives as create_outcome_unknown (ambiguous transport).
    const durable = storage();
    const client = new FakeRunnerClient();
    client.loseCreateResponse = true;
    const registry = mappingRegistry(durable, client);
    const projectId = await createProject(registry);
    const error = await rejection(registry.mapProject(projectId, RUNNER_ID));
    check('lost create response stays visibly reconciling', error.includes('reconciled'), error);
    check('lost create response leaves durable unknown state', storedMappings(durable)[0]?.state === 'create_outcome_unknown');
    check('lost create response executed the create exactly once', client.executionsOf('new_session') === 1);

    // Runner restart: the ledger is forgotten, so the recovery canary
    // executes FRESH — canary SUCCESS proves no ledger continuity. A create
    // replay could duplicate a session the lost create already opened, and
    // no exact identity is resolvable (naming never ran). No safe replay
    // exists — the intent converges durably on the explicit
    // create_outcome_unresolved conflict, never a forever-unknown.
    const createsBefore = client.executionsOf('new_session');
    client.resetLedger();
    await registry.recoverRunner(RUNNER_ID);
    const converged = storedMappings(durable)[0];
    check('canary success converges to the explicit unresolved conflict',
      converged?.state === 'conflict' && converged.reason === 'create_outcome_unresolved',
      `${converged?.state}:${JSON.stringify(converged ?? null)}`);
    check('canary success never replays the create', client.executionsOf('new_session') === createsBefore,
      String(client.executionsOf('new_session')));
    check('canary success persists no session identity',
      converged?.sessionId === undefined && converged?.sessionPath === undefined);

    // Later recovery on the settled conflict is stable: no commands, no
    // mutation at all.
    const callsAfterConvergence = client.calls.length;
    const writesAfterConvergence = durable.writes.length;
    await registry.recoverRunner(RUNNER_ID);
    check('conflict recovery sends no commands', client.calls.length === callsAfterConvergence,
      String(client.calls.length - callsAfterConvergence));
    check('conflict recovery persists nothing', durable.writes.length === writesAfterConvergence,
      String(durable.writes.length - writesAfterConvergence));
  }

  {
    activeBlock = 'lost successful create replay answers expired';
    // Real ledger truth (session_runtime_manager.rs complete_owned_command):
    // a successful new_session response always carries `messages`, so it
    // EXPIRES the moment it settles — delivered once to the original caller,
    // then never replayable. The create settles and its response is lost on
    // the wire; the recovery canary replays against the ledger (get_messages
    // under the canary id collides with the recorded get_state digest — the
    // exact command-id conflict proving continuity), and the stable-id
    // create replay then answers the backend's exact expired literal: the
    // settled success is never replayable and the exact outcome stays
    // unknowable, so the intent converges durably on the explicit
    // create_outcome_unresolved conflict — never a replay-to-active.
    const durable = storage();
    const client = new FakeRunnerClient();
    client.loseCreateResponse = true;
    const registry = mappingRegistry(durable, client);
    const projectId = await createProject(registry);
    await rejection(registry.mapProject(projectId, RUNNER_ID));
    check('expired-create seeding leaves unknown state', storedMappings(durable)[0]?.state === 'create_outcome_unknown');
    client.loseCreateResponse = false;
    const createsBefore = client.executionsOf('new_session');
    await registry.recoverRunner(RUNNER_ID);
    const mapping = storedMappings(durable)[0];
    check('continuity canary conflict permits the stable create replay', client.calls.filter(
      (call) => call.command.type === 'new_session').length === 2);
    check('create replay uses the original stable id', new Set(client.calls
      .filter((call) => call.command.type === 'new_session')
      .map((call) => call.options.commandId)).size === 1);
    check('expired success replay converges to the explicit unresolved conflict',
      mapping?.state === 'conflict' && mapping.reason === 'create_outcome_unresolved', mapping?.state);
    check('expired success replay never re-executes the settled create',
      client.executionsOf('new_session') === createsBefore);
    check('expired success replay persists no session identity',
      mapping?.sessionId === undefined && mapping?.sessionPath === undefined);
    // The convergence is STABLE: the settled conflict branch sends no
    // commands and mutates nothing, and the expired create can never be
    // re-executed into a different outcome by a later pass.
    const callsBeforeStable = client.calls.length;
    const writesBeforeStable = durable.writes.length;
    await registry.recoverRunner(RUNNER_ID);
    check('expired-create conflict settlement sends no commands',
      client.calls.length === callsBeforeStable, String(client.calls.length - callsBeforeStable));
    check('expired-create conflict settlement persists nothing',
      durable.writes.length === writesBeforeStable);
    check('expired-create conflict stays settled', storedMappings(durable)[0]?.state === 'conflict'
      && storedMappings(durable)[0]?.reason === 'create_outcome_unresolved', storedMappings(durable)[0]?.state);
    check('expired-create conflict runs zero duplicate creates',
      client.executionsOf('new_session') === createsBefore);
  }


  {
    activeBlock = 'naming transport ambiguity';
    const durable = storage();
    const client = new FakeRunnerClient();
    client.nameTransportFailure = true;
    const registry = new ChatMappingStore({
      storage: durable,
      runners: client,
      withExclusiveLock: immediateLock,
      randomBytes: deterministicBytes,
    });
    const projectId = await createProject(registry);
    const error = await rejection(registry.mapProject(projectId, RUNNER_ID));
    check('ambiguous naming remains provisioning', error.includes('reconciled'), error);
    check('ambiguous naming is durable', storedMappings(durable)[0]?.state === 'provisioning');
    client.rows.push(catalogRow());
    client.nameTransportFailure = false;
    await registry.recoverRunner(RUNNER_ID);
    check('unnamed provisioning retries the stable name command', client.calls.filter(
      (call) => call.command.type === 'set_session_name').length === 2);
    check('retried naming keeps the stable command id', new Set(client.calls
      .filter((call) => call.command.type === 'set_session_name')
      .map((call) => call.options.commandId)).size === 1);
    check('named provisioning reconciles active', storedMappings(durable)[0]?.state === 'active');
  }

  {
    activeBlock = 'compensation and retry';
    const durable = storage();
    const client = new FakeRunnerClient();
    client.archiveFailures = 1;
    const registry = new ChatMappingStore({
      storage: durable,
      runners: client,
      withExclusiveLock: immediateLock,
      randomBytes: deterministicBytes,
    });
    const projectId = await createProject(registry);
    const original = client.command.bind(client);
    client.command = async (runnerId, command, options = {}) => {
      if (command.type === 'set_session_name') {
        const failed = response(options.commandId, 'set_session_name', false, undefined, 'name rejected marker');
        throw new RunnerCommandError(failed.error || '', 'remote', failed);
      }
      if (command.type === 'new_session') {
        client.rows.push(catalogRow());
      }
      return original(runnerId, command, options);
    };
    await rejection(registry.mapProject(projectId, RUNNER_ID));
    const pending = storedMappings(durable)[0];
    check('failed compensation remains rollback pending', pending?.state === 'rollback_pending', pending?.state);
    // The naming rejection persists the rollback intent (archiveAttempt 0);
    // the compensation archive itself runs on the first recovery pass.
    check('failed compensation has not archived yet', pending?.state === 'rollback_pending'
      && pending.archiveAttempt === 0
      && client.calls.every((call) => call.command.type !== 'session_delete'));
    client.command = async (runnerId, command, options = {}) => {
      if (command.type === 'session_list') {
        client.calls.push({ command, options });
        return response(options.commandId, 'session_list', true, {
          sessions: client.rows.length > 0 ? [...client.rows] : [catalogRow()],
        });
      }
      return original(runnerId, command, options);
    };
    // First archive attempt: the catalog finds the identity but the archive
    // delete is refused — the mapping survives with the attempt durably
    // advanced, never silently dropped.
    await registry.recoverRunner(RUNNER_ID);
    const refused = storedMappings(durable)[0];
    check('refused archive keeps the rollback pending intent', refused?.state === 'rollback_pending'
      && refused.archiveAttempt === 1, refused?.state);
    await registry.recoverRunner(RUNNER_ID);
    check('successful later archive removes mapping', storedMappings(durable).length === 0);
    const archiveIds = client.calls.filter((call) => call.command.type === 'session_delete')
      .map((call) => call.options.commandId);
    check('definitive archive retry advances command id', new Set(archiveIds).size === 2, archiveIds.join(','));
  }

  {
    activeBlock = 'identified conflict cleanup';
    const durable = storage();
    const client = new FakeRunnerClient();
    const registry = new ChatMappingStore({
      storage: durable,
      runners: client,
      withExclusiveLock: immediateLock,
      randomBytes: deterministicBytes,
    });
    const projectId = await createProject(registry);
    await registry.mapProject(projectId, RUNNER_ID);
    const loaded = loadMappingDocument(durable);
    if (loaded.kind === 'blocked') throw new Error(loaded.error);
    const active = loaded.document.mappings[0];
    if (active?.state !== 'active') throw new Error('active conflict fixture missing');
    durable.values.set(CHAT_MAPPING_STORAGE_KEY, JSON.stringify({
      ...loaded.document,
      mappings: [{ ...active, state: 'conflict', reason: 'identity_conflict' }],
    }));
    await registry.deleteProject(projectId);
    check('identified conflict cleanup archives backend session', client.calls.some(
      (call) => call.command.type === 'session_delete'));
    check('identified conflict cleanup removes mapping', storedMappings(durable).length === 0);
  }

  {
    activeBlock = 'unresolved cleanup';
    const durable = storage();
    const client = new FakeRunnerClient();
    const registry = new ChatMappingStore({
      storage: durable,
      runners: client,
      withExclusiveLock: immediateLock,
      randomBytes: deterministicBytes,
    });
    const projectId = await createProject(registry);
    const original = client.command.bind(client);
    client.command = async (runnerId, command, options = {}) => {
      if (command.type === 'new_session') {
        client.calls.push({ command, options });
        throw new RunnerCommandError('connection closed', 'transport');
      }
      return original(runnerId, command, options);
    };
    await rejection(registry.mapProject(projectId, RUNNER_ID));
    check('unresolved create exists before cleanup', storedMappings(durable)[0]?.state === 'create_outcome_unknown');
    let runnerDeleted = false;
    await registry.removeRunner(RUNNER_ID, () => { runnerDeleted = true; });
    check('unresolved runner cleanup removes local mapping', storedMappings(durable).length === 0);
    check('unresolved runner cleanup permits registry deletion', runnerDeleted);
  }

  {
    activeBlock = 'registry persist failure keeps deleting tombstone';
    const durable = storage();
    const client = new FakeRunnerClient();
    const registry = mappingRegistry(durable, client);
    const projectId = await createProject(registry);
    await registry.mapProject(projectId, RUNNER_ID);
    let runnerDeleteCalls = 0;
    const error = await rejection(registry.removeRunner(RUNNER_ID, () => {
      runnerDeleteCalls += 1;
      throw new Error('Runner registry storage is unavailable.');
    }));
    const leftover = storedMappings(durable)[0];
    const live = registry.snapshot().mappings[0];
    check('registry persist failure is visible', error === 'Runner registry storage is unavailable.', error);
    check('registry persist failure still archives the mapped session', client.calls.some(
      (call) => call.command.type === 'session_delete'));
    check('registry persist failure still invokes registry deletion', runnerDeleteCalls === 1);
    check('registry persist failure does not leave deleting forever', leftover === undefined
      || leftover.state !== 'deleting', leftover?.state);
    check('registry persist failure drops Removing chip mapping', live === undefined
      || live.state !== 'deleting', live?.state);
    check('registry persist failure does not drop the project', registry.snapshot().projects.length === 1);
  }

  {
    activeBlock = 'persist failure recover keeps deleting tombstone';
    const durable = storage();
    const client = new FakeRunnerClient();
    const registry = mappingRegistry(durable, client);
    const projectId = await createProject(registry);
    await registry.mapProject(projectId, RUNNER_ID);
    await rejection(registry.removeRunner(RUNNER_ID, () => {
      throw new Error('Runner registry storage is unavailable.');
    }));
    await registry.recoverRunner(RUNNER_ID);
    const leftover = storedMappings(durable);
    const live = registry.snapshot().mappings[0];
    check('persist failure recover does not keep deleting tombstone', leftover.every((mapping) => mapping.state !== 'deleting')
      && (live === undefined || live.state !== 'deleting'), leftover[0]?.state);
    let runnerDeleted = false;
    await registry.removeRunner(RUNNER_ID, () => { runnerDeleted = true; });
    check('successful registry delete after recover removes mapping', storedMappings(durable).every((mapping) => mapping.state !== 'deleting')
      && runnerDeleted, storedMappings(durable).length);
  }

  {
    activeBlock = 'unresolved registry persist failure keeps mapping';
    const durable = storage();
    const client = new FakeRunnerClient();
    const registry = mappingRegistry(durable, client);
    const projectId = await createProject(registry);
    const original = client.command.bind(client);
    client.command = async (runnerId, command, options = {}) => {
      if (command.type === 'new_session') {
        client.calls.push({ command, options });
        throw new RunnerCommandError('connection closed', 'transport');
      }
      return original(runnerId, command, options);
    };
    await rejection(registry.mapProject(projectId, RUNNER_ID));
    let runnerDeleteCalls = 0;
    const error = await rejection(registry.removeRunner(RUNNER_ID, () => {
      runnerDeleteCalls += 1;
      throw new Error('Runner registry storage is unavailable.');
    }));
    check('unresolved registry persist failure is visible', error === 'Runner registry storage is unavailable.', error);
    check('unresolved registry persist failure invoked deletion', runnerDeleteCalls === 1);
    check('unresolved registry persist failure keeps create mapping', storedMappings(durable)[0]?.state === 'create_outcome_unknown');
  }

  {
    activeBlock = 'identified runner removal waits for registry commit';
    const durable = storage();
    const client = new FakeRunnerClient();
    const registry = mappingRegistry(durable, client);
    const projectId = await createProject(registry);
    await registry.mapProject(projectId, RUNNER_ID);
    const writesBeforeDelete = durable.writes.length;
    let runnerDeleted = false;
    await registry.removeRunner(RUNNER_ID, () => {
      check('identified removal keeps tombstone until registry commit', storedMappings(durable)[0]?.state === 'deleting'
        && durable.writes.length > writesBeforeDelete, storedMappings(durable)[0]?.state);
      runnerDeleted = true;
    });
    check('identified runner removal archives mapped session', client.calls.some(
      (call) => call.command.type === 'session_delete'));
    check('identified runner removal commits registry deletion', runnerDeleted);
    check('identified runner removal drops mapping after registry commit', storedMappings(durable).length === 0);
  }

  {
    activeBlock = 'rollback recover without probe uses sourceSessionId';
    const { durable, client, registry } = await seedMapped('active');
    const loaded = loadMappingDocument(durable);
    if (loaded.kind === 'blocked') throw new Error(loaded.error);
    const active = loaded.document.mappings[0];
    if (active?.state !== 'active') throw new Error('probe-null rollback fixture missing');
    durable.values.set(CHAT_MAPPING_STORAGE_KEY, JSON.stringify({
      ...loaded.document,
      mappings: [{
        ...active,
        state: 'rollback_pending',
        archiveAttempt: 0,
        reason: 'provisioning_failed',
      }],
    }));
    client.omitProbe = true;
    client.connected = false;
    const callsBefore = client.calls.length;
    await registry.recoverRunner(RUNNER_ID);
    const added = client.calls.slice(callsBefore);
    const listed = added.find((call) => call.command.type === 'session_list');
    check('probe-null rollback archives the mapped session', storedMappings(durable).length === 0);
    check('probe-null rollback lists through stored sourceSessionId', listed?.options.sourceSessionId === SOURCE_ID
      && listed?.options.allowBeforeProbe === true, listed?.options.sourceSessionId);
    check('probe-null rollback does not use an empty owner', added.every((call) => call.options.sourceSessionId !== ''));
    check('probe-null rollback deletes the mapped session', added.some((call) => call.command.type === 'session_delete'));
  }

  {
    activeBlock = 'project cleanup';
    const durable = storage();
    const client = new FakeRunnerClient();
    const registry = new ChatMappingStore({
      storage: durable,
      runners: client,
      withExclusiveLock: immediateLock,
      randomBytes: deterministicBytes,
    });
    const projectId = await createProject(registry);
    await registry.mapProject(projectId, RUNNER_ID);
    await registry.deleteProject(projectId);
    const snapshot = registry.snapshot();
    check('project deletion archives mapped session', client.calls.some((call) => call.command.type === 'session_delete'));
    check('project deletion removes project only after cleanup', snapshot.projects.length === 0);
    check('project deletion removes mapping', snapshot.mappings.length === 0);
  }

  {
    activeBlock = 'reconnect archive uses live runner owner';
    const durable = storage();
    const client = new FakeRunnerClient();
    const registry = mappingRegistry(durable, client);
    const projectId = await createProject(registry);
    await registry.mapProject(projectId, RUNNER_ID);
    client.liveSessionId = 'restarted-primary';
    const callsBefore = client.calls.length;
    await registry.deleteProject(projectId);
    const added = client.calls.slice(callsBefore);
    const listed = added.find((call) => call.command.type === 'session_list');
    const archived = added.find((call) => call.command.type === 'session_delete');
    check('reconnect archive catalogs through live owner', listed?.options.sourceSessionId === 'restarted-primary');
    check('reconnect archive deletes through live owner', archived?.options.sourceSessionId === 'restarted-primary');
    check('reconnect archive deletes stored path', archived?.command.type === 'session_delete'
      && archived.command.sessionPath === TARGET_PATH);
    check('reconnect archive does not use stale source', added.every((call) =>
      call.options.sourceSessionId !== SOURCE_ID));
    check('reconnect archive removes mapped session', storedMappings(durable).length === 0);
  }

  {
    activeBlock = 'active mapped archive failure still deletes the project';
    const durable = storage();
    const client = new FakeRunnerClient();
    const registry = mappingRegistry(durable, client);
    const projectId = await createProject(registry);
    await registry.mapProject(projectId, RUNNER_ID);
    client.liveSessionId = 'restarted-primary';
    const original = client.command.bind(client);
    client.command = async (runnerId, command, options = {}) => {
      if (command.type === 'session_delete') {
        client.calls.push({ command, options });
        const failed = response(
          options.commandId,
          'session_delete',
          false,
          undefined,
          'cannot archive the active session',
          options.sourceSessionId ?? SOURCE_ID,
        );
        throw new RunnerCommandError(failed.error || '', 'remote', failed);
      }
      return original(runnerId, command, options);
    };
    await registry.deleteProject(projectId);
    const snapshot = registry.snapshot();
    check('active archive failure attempted delete through live owner', client.calls.some(
      (call) => call.command.type === 'session_delete' && call.options.sourceSessionId === 'restarted-primary'));
    check('active archive failure still removes the project', snapshot.projects.length === 0);
    check('active archive failure drops the deleting tombstone', storedMappings(durable).length === 0
      && snapshot.mappings.every((mapping) => mapping.state !== 'deleting'), storedMappings(durable)[0]?.state);
  }

  {
    activeBlock = 'transport archive failure still deletes the project';
    const durable = storage();
    const client = new FakeRunnerClient();
    const registry = mappingRegistry(durable, client);
    const projectId = await createProject(registry);
    await registry.mapProject(projectId, RUNNER_ID);
    client.listFailure = 'transport';
    await registry.deleteProject(projectId);
    const snapshot = registry.snapshot();
    check('transport archive failure attempted the archive', client.calls.some(
      (call) => call.command.type === 'session_list'));
    check('transport archive failure still removes the project', snapshot.projects.length === 0);
    check('transport archive failure leaves no deleting mapping', storedMappings(durable).length === 0
      && snapshot.mappings.every((mapping) => mapping.state !== 'deleting'), storedMappings(durable)[0]?.state);
  }

  {
    activeBlock = 'removeRunner transport archive failure still deletes the runner';
    const durable = storage();
    const client = new FakeRunnerClient();
    const registry = mappingRegistry(durable, client);
    const projectId = await createProject(registry);
    await registry.mapProject(projectId, RUNNER_ID);
    client.listFailure = 'transport';
    let runnerDeleted = false;
    await registry.removeRunner(RUNNER_ID, () => { runnerDeleted = true; });
    check('removeRunner transport archive failure still calls deleteRunner', runnerDeleted);
    check('removeRunner transport archive failure drops the deleting tombstone', storedMappings(durable).length === 0
      && registry.snapshot().mappings.every((mapping) => mapping.state !== 'deleting'), storedMappings(durable)[0]?.state);
    check('removeRunner transport archive failure keeps the project', registry.snapshot().projects.length === 1);
  }

  {
    activeBlock = 'unmapProjectRunner drops one mapping and keeps the project';
    const durable = storage();
    const client = new FakeRunnerClient();
    const registry = mappingRegistry(durable, client);
    const projectId = await createProject(registry);
    const other = await registry.addProject('Other', '/workspace/other');
    await registry.mapProject(projectId, RUNNER_ID);
    await registry.mapProject(other.id, RUNNER_ID);
    check('two mappings exist before unmap', storedMappings(durable).length === 2);
    await registry.unmapProjectRunner(projectId, RUNNER_ID);
    const leftover = storedMappings(durable);
    const snapshot = registry.snapshot();
    check('unmap archives the mapped session', client.calls.some((call) => call.command.type === 'session_delete'));
    check('unmap drops only that project-runner pair', leftover.length === 1
      && leftover[0]?.projectId === other.id
      && leftover[0]?.runnerId === RUNNER_ID, leftover.map((mapping) => mapping.projectId).join(','));
    check('unmap keeps both projects', snapshot.projects.length === 2
      && snapshot.projects.some((project) => project.id === projectId)
      && snapshot.projects.some((project) => project.id === other.id));
    check('unmap leaves the runner command client intact', client.snapshot().some((runner) => runner.id === RUNNER_ID));
  }

  {
    activeBlock = 'unmapProjectRunner transport archive failure still drops the mapping';
    const durable = storage();
    const client = new FakeRunnerClient();
    const registry = mappingRegistry(durable, client);
    const projectId = await createProject(registry);
    await registry.mapProject(projectId, RUNNER_ID);
    client.listFailure = 'transport';
    await registry.unmapProjectRunner(projectId, RUNNER_ID);
    const snapshot = registry.snapshot();
    check('unmap transport archive failure attempted the archive', client.calls.some(
      (call) => call.command.type === 'session_list'));
    check('unmap transport archive failure drops the mapping', storedMappings(durable).length === 0
      && snapshot.mappings.every((mapping) => mapping.state !== 'deleting'), storedMappings(durable)[0]?.state);
    check('unmap transport archive failure keeps the project', snapshot.projects.length === 1
      && snapshot.projects[0]?.id === projectId);
    check('unmap transport archive failure leaves the runner', client.snapshot().some((runner) => runner.id === RUNNER_ID));
  }

  {
    activeBlock = 'archive retreats when live owner is the mapped session';
    const durable = storage();
    const client = new FakeRunnerClient();
    const registry = mappingRegistry(durable, client);
    const projectId = await createProject(registry);
    await registry.mapProject(projectId, RUNNER_ID);
    client.allowSwitchSession = true;
    client.liveSessionId = TARGET_ID;
    client.rows.splice(0, client.rows.length,
      catalogRow({ name: storedMappings(durable)[0]?.state === 'active' ? storedMappings(durable)[0].sessionName : null }),
      catalogRow({
        sessionId: SOURCE_ID,
        runtimeIncarnation: incarnationOf(SOURCE_ID),
        path: SOURCE_PATH,
        cwd: '<workspace>/runner',
        name: null,
      }),
    );
    const callsBefore = client.calls.length;
    await registry.deleteProject(projectId);
    const added = client.calls.slice(callsBefore);
    const switched = added.find((call) => call.command.type === 'switch_session');
    const archived = added.filter((call) => call.command.type === 'session_delete');
    check('mapped-current archive switches to bootstrap first', switched?.command.type === 'switch_session'
      && switched.command.sessionPath === SOURCE_PATH
      && switched.options.sourceSessionId === TARGET_ID);
    check('mapped-current archive deletes through bootstrap', archived.length === 1
      && archived[0]?.options.sourceSessionId === SOURCE_ID
      && archived[0]?.command.sessionPath === TARGET_PATH);
    check('mapped-current archive removes mapping', storedMappings(durable).length === 0);
    check('mapped-current archive removes project', registry.snapshot().projects.length === 0);
  }

  {
    activeBlock = 'archive retreats after primary-session delete rejection';
    const durable = storage();
    const client = new FakeRunnerClient();
    const registry = mappingRegistry(durable, client);
    const projectId = await createProject(registry);
    await registry.mapProject(projectId, RUNNER_ID);
    client.allowSwitchSession = true;
    const mappedName = storedMappings(durable)[0]?.state === 'active' ? storedMappings(durable)[0].sessionName : null;
    client.rows.splice(0, client.rows.length,
      catalogRow({ name: mappedName }),
      catalogRow({
        sessionId: SOURCE_ID,
        runtimeIncarnation: incarnationOf(SOURCE_ID),
        path: SOURCE_PATH,
        cwd: '<workspace>/runner',
        name: null,
      }),
    );
    let deletes = 0;
    const original = client.command.bind(client);
    client.command = async (runnerId, command, options = {}) => {
      if (command.type === 'session_delete') {
        deletes += 1;
        if (deletes === 1) {
          client.calls.push({ command, options });
          const failed = response(
            options.commandId,
            'session_delete',
            false,
            undefined,
            'cannot archive the primary session',
            options.sourceSessionId ?? SOURCE_ID,
          );
          throw new RunnerCommandError(failed.error || '', 'remote', failed);
        }
      }
      return original(runnerId, command, options);
    };
    const callsBefore = client.calls.length;
    await registry.deleteProject(projectId);
    const added = client.calls.slice(callsBefore);
    const switched = added.find((call) => call.command.type === 'switch_session');
    check('primary-session archive retries after retreat', deletes === 2);
    check('primary-session archive switches to bootstrap', switched?.command.type === 'switch_session'
      && switched.command.sessionPath === SOURCE_PATH);
    check('primary-session archive removes mapping', storedMappings(durable).length === 0);
  }

  {
    activeBlock = 'restart rebinding';
    const durable = storage();
    const client = new FakeRunnerClient();
    client.allowSwitchSession = true;
    const registry = new ChatMappingStore({
      storage: durable,
      runners: client,
      withExclusiveLock: immediateLock,
      randomBytes: deterministicBytes,
    });
    const projectId = await createProject(registry);
    const mapped = await registry.mapProject(projectId, RUNNER_ID);
    const writesBefore = durable.writes.length;
    const callsBefore = client.calls.length;
    await registry.recoverRunner(RUNNER_ID);
    const rebound = storedMappings(durable)[0];
    const added = client.calls.slice(callsBefore);
    const listed = added[0]?.command;
    const switched = added[1]?.command;
    check('present catalog rebinds with session_list first', listed?.type === 'session_list'
      && listed.scope === 'all_projects');
    check('present catalog list is pre-probe', added[0]?.options.allowBeforeProbe === true
      && added[0]?.options.sourceSessionId === SOURCE_ID);
    check('present catalog switches exact stored path', switched?.type === 'switch_session'
      && switched.sessionPath === TARGET_PATH);
    check('present catalog switch is pre-probe', added[1]?.options.allowBeforeProbe === true
      && added[1]?.options.sourceSessionId === SOURCE_ID);
    check('successful rebind keeps session identity', rebound?.state === 'active'
      && rebound.sessionId === mapped.sessionId
      && rebound.sessionPath === mapped.sessionPath, rebound?.state);
    check('successful rebind does not persist', durable.writes.length === writesBefore);
    check('successful rebind does not create', added.every((call) => call.command.type !== 'new_session'));

    const callsAfterSuccess = client.calls.length;
    await registry.recoverRunner(RUNNER_ID);
    check('idempotent second recover stays active', storedMappings(durable)[0]?.state === 'active'
      && storedMappings(durable)[0]?.sessionId === TARGET_ID
      && storedMappings(durable)[0]?.sessionPath === TARGET_PATH);
    check('idempotent second recover still lists then switches', client.calls.slice(callsAfterSuccess)
      .map((call) => call.command.type).join(',') === 'session_list,switch_session');

    client.rows.splice(0, client.rows.length, catalogRow({ path: '/workspace/sessions/other.jsonl' }));
    const switchesBeforeConflict = client.calls.filter((call) => call.command.type === 'switch_session').length;
    await registry.recoverRunner(RUNNER_ID);
    const conflicted = storedMappings(durable)[0];
    check('catalog identity conflict marks mapping conflict', conflicted?.state === 'conflict'
      && conflicted.reason === 'identity_conflict', conflicted?.state);
    check('catalog identity conflict skips switch_session', client.calls.filter(
      (call) => call.command.type === 'switch_session').length === switchesBeforeConflict);
    check('conflict keeps stored identity', conflicted?.sessionId === TARGET_ID
      && conflicted?.sessionPath === TARGET_PATH);
    const conflictCalls = client.calls.length;
    await registry.recoverRunner(RUNNER_ID);
    check('stale conflict settlement sends no commands', client.calls.length === conflictCalls);
  }

  {
    activeBlock = 'restart rebinding absence';
    const { durable, client, registry } = await seedMapped('active');
    client.rows.splice(0, client.rows.length);
    const createsBefore = client.calls.filter((call) => call.command.type === 'new_session').length;
    const namesBefore = client.calls.filter((call) => call.command.type === 'set_session_name').length;
    const callsBefore = client.calls.length;
    await registry.recoverRunner(RUNNER_ID);
    const rebound = storedMappings(durable)[0];
    const added = client.calls.slice(callsBefore);
    check('header-only catalog omission stays active', rebound?.state === 'active'
      && rebound.sessionId === TARGET_ID
      && rebound.sessionPath === TARGET_PATH, rebound?.state);
    check('header-only catalog omission lists then switches', added.map((call) => call.command.type)
      .join(',') === 'session_list,switch_session');
    check('header-only catalog omission switches exact stored path', added[1]?.command.type === 'switch_session'
      && added[1].command.sessionPath === TARGET_PATH);
    check('header-only catalog omission is pre-probe', added.every((call) =>
      call.options.allowBeforeProbe === true && call.options.sourceSessionId === SOURCE_ID));
    check('header-only catalog omission does not create', client.calls.filter(
      (call) => call.command.type === 'new_session').length === createsBefore);
    check('header-only catalog omission does not rename', client.calls.filter(
      (call) => call.command.type === 'set_session_name').length === namesBefore);
  }

  {
    activeBlock = 'restart rebinding header-only provisioning';
    const { durable, client, registry, mapping } = await seedMapped('provisioning');
    client.rows.splice(0, client.rows.length);
    const createsBefore = client.calls.filter((call) => call.command.type === 'new_session').length;
    const namesBefore = client.calls.filter((call) => call.command.type === 'set_session_name').length;
    const callsBefore = client.calls.length;
    await registry.recoverRunner(RUNNER_ID);
    const recovered = storedMappings(durable)[0];
    const added = client.calls.slice(callsBefore);
    const listed = added[0];
    const switched = added[1];
    const named = added.find((call) => call.command.type === 'set_session_name');
    check('header-only provisioning becomes active', recovered?.state === 'active'
      && recovered.sessionId === TARGET_ID
      && recovered.sessionPath === TARGET_PATH, recovered?.state);
    check('header-only provisioning lists then switches then names', added.map((call) => call.command.type)
      .join(',') === 'session_list,switch_session,set_session_name');
    check('header-only provisioning list is pre-probe', listed?.command.type === 'session_list'
      && listed.options.allowBeforeProbe === true
      && listed.options.sourceSessionId === SOURCE_ID);
    check('header-only provisioning switches exact stored path', switched?.command.type === 'switch_session'
      && switched.command.sessionPath === TARGET_PATH
      && switched.options.allowBeforeProbe === true
      && switched.options.sourceSessionId === SOURCE_ID);
    check('header-only provisioning retries the stable name id', named?.options.commandId
      === `${mapping.operationId}:name`);
    check('header-only provisioning name is pre-probe', named?.options.allowBeforeProbe === true
      && named.options.sourceSessionId === SOURCE_ID);
    check('header-only provisioning does not create', client.calls.filter(
      (call) => call.command.type === 'new_session').length === createsBefore);
    check('header-only provisioning sends one extra name', client.calls.filter(
      (call) => call.command.type === 'set_session_name').length === namesBefore + 1);
    check('header-only provisioning keeps a single name command id', new Set(client.calls
      .filter((call) => call.command.type === 'set_session_name')
      .map((call) => call.options.commandId)).size === 1);
  }
  {
    activeBlock = 'restart rebinding source drift';
    async function driftCase(
      name: string,
      state: 'active' | 'provisioning',
      apply: (client: FakeRunnerClient) => void,
    ): Promise<void> {
      const { durable, client, registry } = await seedMapped(state);
      apply(client);
      const createsBefore = client.calls.filter((call) => call.command.type === 'new_session').length;
      await registry.recoverRunner(RUNNER_ID);
      const mapping = storedMappings(durable)[0];
      check(`${name} marks identity conflict`, mapping?.state === 'conflict'
        && mapping.reason === 'identity_conflict', mapping?.state);
      check(`${name} keeps stored identity`, mapping?.sessionId === TARGET_ID
        && mapping?.sessionPath === TARGET_PATH);
      check(`${name} does not create`, client.calls.filter(
        (call) => call.command.type === 'new_session').length === createsBefore);
    }
    await driftCase('active list source drift', 'active', (client) => {
      client.listFailure = 'remote';
      client.listRemoteError = SOURCE_DRIFT;
    });
    await driftCase('active switch source drift', 'active', (client) => {
      client.rows.splice(0, client.rows.length);
      client.switchFailure = 'remote';
      client.switchRemoteError = SOURCE_DRIFT;
    });
    await driftCase('provisioning list source drift', 'provisioning', (client) => {
      client.listFailure = 'remote';
      client.listRemoteError = SOURCE_DRIFT;
    });
    await driftCase('provisioning switch source drift', 'provisioning', (client) => {
      client.rows.splice(0, client.rows.length);
      client.switchFailure = 'remote';
      client.switchRemoteError = SOURCE_DRIFT;
    });
    await driftCase('provisioning name source drift', 'provisioning', (client) => {
      // A runner restart clears the catalog AND the command ledger, so the
      // provisioning recovery re-sends the stable name id and it re-executes
      // — the drift answer then reaches the ownership gate.
      client.rows.splice(0, client.rows.length);
      client.resetLedger();
      client.nameRemoteFailure = true;
      client.nameRemoteError = SOURCE_DRIFT;
    });
    async function subscribeDrift(name: string, state: 'active' | 'provisioning'): Promise<void> {
      const { durable, client, registry } = await seedMapped(state);
      client.subscribeError = SOURCE_DRIFT;
      const createsBefore = client.calls.filter((call) => call.command.type === 'new_session').length;
      const switchesBefore = client.calls.filter((call) => call.command.type === 'switch_session').length;
      const namesBefore = client.calls.filter((call) => call.command.type === 'set_session_name').length;
      const listsBefore = client.calls.filter((call) => call.command.type === 'session_list').length;
      await registry.recoverRunner(RUNNER_ID);
      const mapping = storedMappings(durable)[0];
      check(`${name} marks identity conflict`, mapping?.state === 'conflict'
        && mapping.reason === 'identity_conflict', mapping?.state);
      check(`${name} keeps stored identity`, mapping?.sessionId === TARGET_ID
        && mapping?.sessionPath === TARGET_PATH);
      check(`${name} does not create`, client.calls.filter(
        (call) => call.command.type === 'new_session').length === createsBefore);
      check(`${name} does not switch`, client.calls.filter(
        (call) => call.command.type === 'switch_session').length === switchesBefore);
      check(`${name} does not name`, client.calls.filter(
        (call) => call.command.type === 'set_session_name').length === namesBefore);
      check(`${name} stops at first subscribe`, client.calls.filter(
        (call) => call.command.type === 'session_list').length === listsBefore + 1);
    }
    await subscribeDrift('active subscribe source drift', 'active');
    await subscribeDrift('provisioning subscribe source drift', 'provisioning');
  }

  {
    activeBlock = 'restart rebinding target not found';
    async function missingTarget(
      name: string,
      state: 'active' | 'provisioning',
      error: string,
    ): Promise<void> {
      const { durable, client, registry } = await seedMapped(state);
      client.rows.splice(0, client.rows.length);
      client.switchFailure = 'remote';
      client.switchRemoteError = error;
      const createsBefore = client.calls.filter((call) => call.command.type === 'new_session').length;
      await registry.recoverRunner(RUNNER_ID);
      const mapping = storedMappings(durable)[0];
      check(`${name} marks identity conflict`, mapping?.state === 'conflict'
        && mapping.reason === 'identity_conflict', mapping?.state);
      check(`${name} keeps stored identity`, mapping?.sessionId === TARGET_ID
        && mapping?.sessionPath === TARGET_PATH);
      check(`${name} does not create`, client.calls.filter(
        (call) => call.command.type === 'new_session').length === createsBefore);
    }
    await missingTarget('active target not found', 'active', TARGET_NOT_FOUND);
    await missingTarget('active bounded switch failure', 'active', BOUNDED_SWITCH_FAILURE);
    await missingTarget('provisioning target not found', 'provisioning', TARGET_NOT_FOUND);
    await missingTarget('provisioning bounded switch failure', 'provisioning', BOUNDED_SWITCH_FAILURE);
  }

  {
    activeBlock = 'restart rebinding wrong ownership';
    async function retryable(
      name: string,
      state: 'active' | 'provisioning',
      apply: (client: FakeRunnerClient) => void,
    ): Promise<void> {
      const { durable, client, registry } = await seedMapped(state);
      apply(client);
      const writesBefore = durable.writes.length;
      await registry.recoverRunner(RUNNER_ID);
      const mapping = storedMappings(durable)[0];
      check(`${name} remains ${state}`, mapping?.state === state, mapping?.state);
      check(`${name} keeps stored identity`, mapping?.sessionId === TARGET_ID
        && mapping?.sessionPath === TARGET_PATH);
      check(`${name} does not persist`, durable.writes.length === writesBefore);
    }
    await retryable('active list foreign session', 'active', (client) => {
      client.listFailure = 'remote';
      client.listRemoteError = SOURCE_DRIFT;
      client.listResponseSessionId = 'other-source';
    });
    await retryable('active list wrong command', 'active', (client) => {
      client.listFailure = 'remote';
      client.listRemoteError = SOURCE_DRIFT;
      client.listResponseCommand = 'switch_session';
    });
    await retryable('active list wrong error', 'active', (client) => {
      client.listFailure = 'remote';
      client.listRemoteError = `${SOURCE_DRIFT} now`;
    });
    await retryable('active switch foreign session', 'active', (client) => {
      client.switchFailure = 'remote';
      client.switchRemoteError = SOURCE_DRIFT;
      client.switchResponseSessionId = 'other-source';
    });
    await retryable('active switch wrong command', 'active', (client) => {
      client.rows.splice(0, client.rows.length);
      client.switchFailure = 'remote';
      client.switchRemoteError = TARGET_NOT_FOUND;
      client.switchResponseCommand = 'session_list';
    });
    await retryable('active catalog rejected marker', 'active', (client) => {
      client.listFailure = 'remote';
    });
    await retryable('active switch rejected marker', 'active', (client) => {
      client.switchFailure = 'remote';
    });
    await retryable('provisioning list foreign session', 'provisioning', (client) => {
      client.listFailure = 'remote';
      client.listRemoteError = SOURCE_DRIFT;
      client.listResponseSessionId = 'other-source';
    });
    await retryable('provisioning catalog rejected marker', 'provisioning', (client) => {
      client.listFailure = 'remote';
    });
    await retryable('provisioning name transport', 'provisioning', (client) => {
      // Restart clears the ledger so the stable name id re-executes.
      client.rows.splice(0, client.rows.length);
      client.resetLedger();
      client.nameTransportFailure = true;
    });
    await retryable('provisioning list transport', 'provisioning', (client) => {
      client.listFailure = 'transport';
    });
    await retryable('provisioning name foreign session', 'provisioning', (client) => {
      // Restart clears the ledger so the stable name id re-executes.
      client.rows.splice(0, client.rows.length);
      client.resetLedger();
      client.nameRemoteFailure = true;
      client.nameRemoteError = SOURCE_DRIFT;
      client.nameResponseSessionId = 'other-source';
    });
    await retryable('provisioning name wrong command', 'provisioning', (client) => {
      // Restart clears the ledger so the stable name id re-executes.
      client.rows.splice(0, client.rows.length);
      client.resetLedger();
      client.nameRemoteFailure = true;
      client.nameRemoteError = SOURCE_DRIFT;
      client.nameResponseCommand = 'switch_session';
    });
    await retryable('active subscribe unrelated remote', 'active', (client) => {
      client.subscribeError = 'subscription snapshot already in progress';
    });
    await retryable('provisioning subscribe unrelated remote', 'provisioning', (client) => {
      client.subscribeError = 'subscription snapshot already in progress';
    });
    await retryable('active subscribe foreign session', 'active', (client) => {
      client.subscribeError = SOURCE_DRIFT;
      client.subscribeSessionId = 'other-source';
    });
    await retryable('provisioning subscribe foreign session', 'provisioning', (client) => {
      client.subscribeError = SOURCE_DRIFT;
      client.subscribeSessionId = 'other-source';
    });
    await retryable('active subscribe message-only remote', 'active', (client) => {
      client.subscribeError = SOURCE_DRIFT;
      client.subscribeOmitMetadata = true;
    });
    await retryable('provisioning subscribe message-only remote', 'provisioning', (client) => {
      client.subscribeError = SOURCE_DRIFT;
      client.subscribeOmitMetadata = true;
    });
    {
      const { durable, client, registry } = await seedMapped('provisioning');
      const createsBefore = client.calls.filter((call) => call.command.type === 'new_session').length;
      const writesBefore = durable.writes.length;
      client.rows.splice(0, client.rows.length);
      // A restart clears the ledger so the stable name id re-executes and
      // the rejected name reaches the ownership gate as a fresh failure.
      client.resetLedger();
      client.nameRemoteFailure = true;
      await registry.recoverRunner(RUNNER_ID);
      const rollback = durable.writes.slice(writesBefore).flatMap((raw) => {
        const loaded = loadMappingDocument({
          getItem: () => raw,
          setItem: () => {},
          removeItem: () => {},
        });
        return loaded.kind === 'valid' ? loaded.document.mappings : [];
      }).find((mapping) => mapping.state === 'rollback_pending');
      check('provisioning name rejected marker persists rollback', rollback?.state === 'rollback_pending'
        && rollback.archiveAttempt === 0
        && rollback.reason === 'provisioning_failed', rollback?.state);
      check('provisioning name rejected marker archives mapping', storedMappings(durable).length === 0);
      check('provisioning name rejected marker names then archives', client.calls.some(
        (call) => call.command.type === 'set_session_name'));
      check('provisioning name rejected marker does not create', client.calls.filter(
        (call) => call.command.type === 'new_session').length === createsBefore);
      check('provisioning name rejected marker catalogs absent identity', client.calls.some(
        (call) => call.command.type === 'session_list' && call.options.commandId?.includes(':catalog:')));
      check('provisioning name rejected marker skips delete for absent identity', client.calls.every(
        (call) => call.command.type !== 'session_delete'));
    }
  }


  {
    activeBlock = 'restart rebinding identity mismatch';
    async function mismatchCase(
      name: string,
      data: unknown,
      command = 'switch_session',
      sessionId = SOURCE_ID,
    ): Promise<void> {
      const durable = storage();
      const client = new FakeRunnerClient();
      client.allowSwitchSession = true;
      const registry = new ChatMappingStore({
        storage: durable,
        runners: client,
        withExclusiveLock: immediateLock,
        randomBytes: deterministicBytes,
      });
      const projectId = await createProject(registry);
      await registry.mapProject(projectId, RUNNER_ID);
      client.switchResponse = { type: 'response', id: undefined, command, sessionId, success: true, data };
      await registry.recoverRunner(RUNNER_ID);
      const mapping = storedMappings(durable)[0];
      check(`${name} marks identity conflict`, mapping?.state === 'conflict'
        && mapping.reason === 'identity_conflict', mapping?.state);
      check(`${name} keeps stored path`, mapping?.sessionId === TARGET_ID
        && mapping?.sessionPath === TARGET_PATH);
    }
    await mismatchCase('mismatched switch session id', {
      sessionId: 'foreign-session',
      runtimeIncarnation: incarnationOf('foreign-session'),
      state: { sessionId: 'foreign-session', sessionFile: TARGET_PATH, cwd: SAFE_CWD },
      session: {
        source: 'rpi',
        sessionId: 'foreign-session',
        runtimeIncarnation: incarnationOf('foreign-session'),
        path: TARGET_PATH,
        cwd: SAFE_CWD,
        status: 'native',
        loaded: true,
      },
      messages: [],
    });
    await mismatchCase('mismatched switch cwd', {
      sessionId: TARGET_ID,
      runtimeIncarnation: incarnationOf(TARGET_ID),
      state: { sessionId: TARGET_ID, sessionFile: TARGET_PATH, cwd: '/workspace/other' },
      session: {
        source: 'rpi',
        sessionId: TARGET_ID,
        runtimeIncarnation: incarnationOf(TARGET_ID),
        path: TARGET_PATH,
        cwd: '/workspace/other',
        status: 'native',
        loaded: true,
      },
      messages: [],
    });
    await mismatchCase('mismatched switch command', {
      sessionId: TARGET_ID,
      runtimeIncarnation: incarnationOf(TARGET_ID),
      state: { sessionId: TARGET_ID, sessionFile: TARGET_PATH, cwd: SAFE_CWD },
      session: {
        source: 'rpi',
        sessionId: TARGET_ID,
        runtimeIncarnation: incarnationOf(TARGET_ID),
        path: TARGET_PATH,
        cwd: SAFE_CWD,
        status: 'native',
        loaded: true,
      },
      messages: [],
    }, 'new_session');
    await mismatchCase('mismatched switch source session', {
      sessionId: TARGET_ID,
      runtimeIncarnation: incarnationOf(TARGET_ID),
      state: { sessionId: TARGET_ID, sessionFile: TARGET_PATH, cwd: SAFE_CWD },
      session: {
        source: 'rpi',
        sessionId: TARGET_ID,
        runtimeIncarnation: incarnationOf(TARGET_ID),
        path: TARGET_PATH,
        cwd: SAFE_CWD,
        status: 'native',
        loaded: true,
      },
      messages: [],
    }, 'switch_session', 'other-source');
  }

  {
    activeBlock = 'restart rebinding transport retry';
    const durable = storage();
    const client = new FakeRunnerClient();
    client.allowSwitchSession = true;
    const registry = new ChatMappingStore({
      storage: durable,
      runners: client,
      withExclusiveLock: immediateLock,
      randomBytes: deterministicBytes,
    });
    const projectId = await createProject(registry);
    await registry.mapProject(projectId, RUNNER_ID);
    client.listFailure = 'transport';
    const writesBeforeList = durable.writes.length;
    await registry.recoverRunner(RUNNER_ID);
    check('session_list transport leaves mapping active', storedMappings(durable)[0]?.state === 'active');
    check('session_list transport does not persist', durable.writes.length === writesBeforeList);
    client.listFailure = null;
    client.switchFailure = 'remote';
    const writesBeforeSwitch = durable.writes.length;
    await registry.recoverRunner(RUNNER_ID);
    check('switch remote failure leaves mapping active', storedMappings(durable)[0]?.state === 'active'
      && storedMappings(durable)[0]?.sessionId === TARGET_ID);
    check('switch remote failure does not persist', durable.writes.length === writesBeforeSwitch);
    client.switchFailure = 'transport';
    await registry.recoverRunner(RUNNER_ID);
    check('switch transport failure remains retryable', storedMappings(durable)[0]?.state === 'active');
    client.switchFailure = null;
    const callsBeforeRetry = client.calls.length;
    await registry.recoverRunner(RUNNER_ID);
    check('later generation rebind succeeds', storedMappings(durable)[0]?.state === 'active'
      && storedMappings(durable)[0]?.sessionPath === TARGET_PATH);
    check('later generation rebind switches exact path', client.calls.slice(callsBeforeRetry).some((call) =>
      call.command.type === 'switch_session' && call.command.sessionPath === TARGET_PATH));
  }

  {
    activeBlock = 'reconnect recover uses live probe owner before connected';
    const { durable, client, registry } = await seedMapped('active');
    client.connected = false;
    client.liveSessionId = 'restarted-primary';
    const callsBefore = client.calls.length;
    await registry.recoverRunner(RUNNER_ID);
    const added = client.calls.slice(callsBefore);
    check('live-owner recover stays active', storedMappings(durable)[0]?.state === 'active'
      && storedMappings(durable)[0]?.sessionId === TARGET_ID, storedMappings(durable)[0]?.state);
    check('live-owner recover lists through the new primary', added[0]?.command.type === 'session_list'
      && added[0]?.options.sourceSessionId === 'restarted-primary'
      && added[0]?.options.allowBeforeProbe === true);
    check('live-owner recover switches through the new primary', added[1]?.command.type === 'switch_session'
      && added[1]?.options.sourceSessionId === 'restarted-primary'
      && added[1]?.options.allowBeforeProbe === true);
    check('live-owner recover does not use stale source', added.every((call) =>
      call.options.sourceSessionId !== SOURCE_ID));
  }

  {
    activeBlock = 'restart rebinding pre-probe no-op';
    const durable = storage();
    const client = new FakeRunnerClient();
    client.allowSwitchSession = true;
    const registry = new ChatMappingStore({
      storage: durable,
      runners: client,
      withExclusiveLock: immediateLock,
      randomBytes: deterministicBytes,
    });
    const projectId = await createProject(registry);
    await registry.mapProject(projectId, RUNNER_ID);
    const loaded = loadMappingDocument(durable);
    if (loaded.kind === 'blocked') throw new Error(loaded.error);
    const active = loaded.document.mappings[0];
    if (active?.state !== 'active') throw new Error('pre-probe fixture missing');
    durable.values.set(CHAT_MAPPING_STORAGE_KEY, JSON.stringify({
      ...loaded.document,
      mappings: [{
        ...active,
        state: 'rollback_pending',
        archiveAttempt: 0,
        reason: 'provisioning_failed',
      }],
    }));
    client.connected = false;
    const deletesBefore = client.calls.filter((call) => call.command.type === 'session_delete').length;
    await registry.recoverRunner(RUNNER_ID);
    check('connecting archive removes the rollback mapping', storedMappings(durable).length === 0);
    check('connecting archive deletes through the live probe owner', client.calls.filter(
      (call) => call.command.type === 'session_delete').length === deletesBefore + 1);
    const archiveList = [...client.calls].reverse().find((call) => call.command.type === 'session_list');
    check('connecting archive list allows before probe', archiveList?.options.allowBeforeProbe === true
      && archiveList?.options.sourceSessionId === SOURCE_ID);

    durable.values.set(CHAT_MAPPING_STORAGE_KEY, JSON.stringify({
      ...loaded.document,
      mappings: [{ ...active, state: 'provisioning' }],
    }));
    const namesBefore = client.calls.filter((call) => call.command.type === 'set_session_name').length;
    await registry.recoverRunner(RUNNER_ID);
    check('connecting provisioning becomes active', storedMappings(durable)[0]?.state === 'active'
      && storedMappings(durable)[0]?.sessionId === TARGET_ID, storedMappings(durable)[0]?.state);
    check('connecting provisioning retries the name', client.calls.filter(
      (call) => call.command.type === 'set_session_name').length === namesBefore + 1);

    durable.values.set(CHAT_MAPPING_STORAGE_KEY, JSON.stringify({
      ...loaded.document,
      mappings: [active],
    }));
    const callsBeforeActive = client.calls.length;
    await registry.recoverRunner(RUNNER_ID);
    const preProbeActive = client.calls.slice(callsBeforeActive);
    check('pre-probe active still lists then switches', preProbeActive.map((call) => call.command.type)
      .join(',') === 'session_list,switch_session');
    check('pre-probe active commands allow before probe', preProbeActive.every((call) =>
      call.options.allowBeforeProbe === true));
    check('pre-probe active stays mapped', storedMappings(durable)[0]?.state === 'active'
      && storedMappings(durable)[0]?.sessionPath === TARGET_PATH);
  }

  {
    activeBlock = 'platform-neutral project cwd validation';
    // The runner accepts whatever absolute path its own OS understands, so
    // the store accepts POSIX, Windows drive-root, and STRICT UNC
    // absolutes (exactly two leading separators, nonempty server and share,
    // no doubled or trailing structural separators), and still rejects
    // drive-relative, malformed-UNC, and relative shapes before any runner
    // command is sent.
    const durable = storage();
    const client = new FakeRunnerClient();
    const registry = new ChatMappingStore({
      storage: durable,
      runners: client,
      withExclusiveLock: immediateLock,
      randomBytes: deterministicBytes,
    });
    const accepted = [
      '/workspace/project',
      'C:\\projects\\app',
      'C:/projects/app',
      'Z:\\',
      '\\\\server\\share',
      '\\\\server\\share\\nested\\dir',
    ];
    for (const cwd of accepted) {
      const acceptedProject = await registry.addProject(`Product ${accepted.indexOf(cwd)}`, cwd);
      check(`cwd ${JSON.stringify(cwd)} is accepted as absolute`,
        registry.snapshot().projects.some((project) => project.id === acceptedProject.id
          && project.cwd === cwd));
      check(`cwd ${JSON.stringify(cwd)} persists no mapping commands`, client.calls.length === 0);
    }
    for (const cwd of [
      'workspace/relative',
      'C:relative\\path',
      'C:',
      '1:\\projects',
      ':\\projects',
      '\\\\server',
      '\\\\server\\',
      '\\\\',
      // Strict-UNC rejections: triple leading separator, doubled or
      // trailing structural separators anywhere, and doubled mixed ones.
      '\\\\\\server\\share',
      '\\\\server\\\\share',
      '\\\\server\\share\\',
      '\\\\server\\share\\\\deep',
      '\\\\server//share',
      '\\/\\share',
      'server\\share',
      '',
      '   ',
    ]) {
      const error = await rejection(registry.addProject('Rejected', cwd));
      check(`cwd ${JSON.stringify(cwd)} rejects with the absolute-directory message`,
        error === 'Project directory must be absolute.', error);
    }
    check('platform-neutral validation persists exactly the accepted projects',
      registry.snapshot().projects.length === accepted.length);
  }

  {
    activeBlock = 'naming compensation single conversion';
    // The set_session_name compensation rethrows raw: the mutate boundary
    // fixedError is the ONE sanitization pass, so the generic rejection
    // literal surfaces exactly once, never double-wrapped.
    const durable = storage();
    const client = new FakeRunnerClient();
    const registry = new ChatMappingStore({
      storage: durable,
      runners: client,
      withExclusiveLock: immediateLock,
      randomBytes: deterministicBytes,
    });
    const projectId = await createProject(registry);
    const original = client.command.bind(client);
    client.command = async (runnerId, command, options = {}) => {
      if (command.type === 'set_session_name') {
        client.calls.push({ command, options });
        const failed = response(options.commandId, 'set_session_name', false, undefined, 'name rejected marker');
        throw new RunnerCommandError(failed.error || '', 'remote', failed);
      }
      return original(runnerId, command, options);
    };
    const error = await rejection(registry.mapProject(projectId, RUNNER_ID));
    check('naming rejection converts exactly once to the bounded literal',
      error === 'Runner rejected the project operation.', error);
    check('naming rejection leaves rollback pending', storedMappings(durable)[0]?.state === 'rollback_pending');
  }

  {
    activeBlock = 'create outcome unknown convergence';
    // Real command ledger seed: the create reaches the runner and settles
    // into its idempotency ledger (a bounded failure response — message-free
    // and replayable), then the connection drops before the response is
    // delivered — ambiguous transport. The intent therefore survives as
    // create_outcome_unknown, and every outcome below plays out through the
    // recovery replay: the canary collides with the recorded get_state
    // digest (exact command-id conflict, ledger continuity) and the
    // stable-id create replay answers from the settled ledger without ever
    // re-executing. A SUCCESSFUL create settles as EXPIRED (its snapshot
    // always carries `messages`), so a lost successful create can never
    // replay to active/provisioning — that convergence lives in the
    // 'lost successful create replay answers expired' block above.
    async function seedLostCreate(
      createFailure: string | null = null,
    ): Promise<{ durable: DurableStore; client: FakeRunnerClient; registry: ChatMappingStore; createsBefore: number }> {
      const durable = storage();
      const client = new FakeRunnerClient();
      client.createFailure = createFailure;
      client.loseCreateResponse = true;
      const registry = mappingRegistry(durable, client);
      const projectId = await createProject(registry);
      const error = await rejection(registry.mapProject(projectId, RUNNER_ID));
      check('transport-lost seed stays visibly reconciling', error.includes('reconciled'), error);
      check('transport-lost seed leaves durable unknown state',
        storedMappings(durable)[0]?.state === 'create_outcome_unknown', storedMappings(durable)[0]?.state);
      return { durable, registry, client, createsBefore: client.executionsOf('new_session') };
    }

    // ANY owned remote replay failure is DEFINITIVE for the intent — the
    // stable create id plus the source-owned failure envelope prove the
    // runner answered THIS create, whether the answer is the ledger's replay
    // of the original outcome (a definitive directory category here) or its
    // own verdict. The exact outcome stays unknowable, so the intent
    // converges on the explicit create_outcome_unresolved conflict — never
    // a forever retry, never a silent remove, never a duplicate create.
    async function replayFailureCase(label: string, createFailure: string): Promise<void> {
      const { durable, registry, client, createsBefore } = await seedLostCreate(createFailure);
      client.loseCreateResponse = false;
      await registry.recoverRunner(RUNNER_ID);
      const mapping = storedMappings(durable)[0];
      check(`${label} converges to the explicit unresolved conflict`, mapping?.state === 'conflict'
        && mapping.reason === 'create_outcome_unresolved', mapping?.state);
      check(`${label} never re-executes the settled create`, client.executionsOf('new_session') === createsBefore,
        String(client.executionsOf('new_session')));
      check(`${label} replays the create exactly once on the wire`, client.calls.filter(
        (call) => call.command.type === 'new_session').length === createsBefore + 1);
      check(`${label} persists no session identity`,
        mapping?.sessionId === undefined && mapping?.sessionPath === undefined);
      // The convergence is STABLE: the settled conflict branch sends no
      // commands and mutates nothing, and the settled failure can never be
      // re-executed into a different outcome by a later pass.
      const callsBeforeSettled = client.calls.length;
      const writesBeforeStable = durable.writes.length;
      await registry.recoverRunner(RUNNER_ID);
      check(`${label} conflict settlement sends no commands`,
        client.calls.length === callsBeforeSettled, String(client.calls.length - callsBeforeSettled));
      check(`${label} conflict settlement persists nothing`,
        durable.writes.length === writesBeforeStable);
      check(`${label} conflict stays settled`, storedMappings(durable)[0]?.state === 'conflict'
        && storedMappings(durable)[0]?.reason === 'create_outcome_unresolved', storedMappings(durable)[0]?.state);
      check(`${label} conflict runs zero duplicate creates`,
        client.executionsOf('new_session') === createsBefore);
    }
    await replayFailureCase('definitive replay failure', 'new session directory does not exist');
    // The rewritten busy convergence: the busy create settles into the ledger
    // and its response is lost on the wire; the recovery replay replays the
    // recorded refusal — it can NEVER re-execute a cleared busy condition —
    // and the intent converges on the stable explicit conflict.
    await replayFailureCase('busy replay failure', BUSY_CAPACITY_FAILURE);
    await replayFailureCase('bounded-open replay failure', BOUNDED_SWITCH_FAILURE);

    // The automatic transcript expiration: a lost SUCCESSFUL create's
    // settled entry is already in the expired set (digest retained), so the
    // stable re-send answers the backend's exact expired literal — explicit
    // conflict, never a retry, never a re-execution. (The seeded create here
    // is message-free only when it FAILED; the success expiry is asserted in
    // the dedicated block above and by get_messages canaries below.)
    {
      const { durable, registry, client, createsBefore } = await seedLostCreate();
      client.loseCreateResponse = false;
      await registry.recoverRunner(RUNNER_ID);
      const mapping = storedMappings(durable)[0];
      check('command-expired replay marks create_outcome_unresolved', mapping?.state === 'conflict'
        && mapping.reason === 'create_outcome_unresolved', mapping?.state);
      check('command-expired replay never re-executes the create',
        client.executionsOf('new_session') === createsBefore);
      check('command-expired replay replays the stable id exactly once on the wire',
        client.calls.filter((call) => call.command.type === 'new_session').length === 2);
      check('command-expired replay persists no session identity',
        mapping?.sessionId === undefined && mapping?.sessionPath === undefined);
    }

    // A client-side protocol verdict on a settled REPLAYABLE failure replay
    // (the malformed response the runners ownership gate refuses) is also an
    // explicit conflict, never a permanent unknown.
    {
      const { durable, registry, client, createsBefore } = await seedLostCreate('new session directory does not exist');
      client.loseCreateResponse = false;
      client.createReplayProtocolFailure = true;
      await registry.recoverRunner(RUNNER_ID);
      const mapping = storedMappings(durable)[0];
      check('protocol replay failure marks unresolved conflict', mapping?.state === 'conflict'
        && mapping.reason === 'create_outcome_unresolved', mapping?.state);
      check('replay protocol failure replays the create exactly once', client.calls.filter(
        (call) => call.command.type === 'new_session').length === createsBefore + 1);
      check('replay protocol failure never re-executes the settled create',
        client.executionsOf('new_session') === createsBefore);
    }

    // An ambiguous transport drop on the replay of a settled REPLAYABLE
    // failure keeps the bounded surfaced unknown — the ONLY state that stays
    // retryable — and the next pass, with the connection healthy, replays
    // the settled refusal from the ledger again (never a re-execution) and
    // converges the intent on the stable explicit conflict.
    {
      const { durable, registry, client, createsBefore } = await seedLostCreate(BUSY_CAPACITY_FAILURE);
      client.loseCreateResponse = false;
      client.loseReplayResponse = true;
      await registry.recoverRunner(RUNNER_ID);
      check('replay transport failure stays retryable', storedMappings(durable)[0]?.state === 'create_outcome_unknown',
        storedMappings(durable)[0]?.state);
      check('replay transport failure replays the create exactly once', client.calls.filter(
        (call) => call.command.type === 'new_session').length === createsBefore + 1);
      check('replay transport failure never re-executes the settled create',
        client.executionsOf('new_session') === createsBefore);
      client.loseReplayResponse = false;
      await registry.recoverRunner(RUNNER_ID);
      check('cleared transport replays the settled refusal to the explicit conflict',
        storedMappings(durable)[0]?.state === 'conflict'
        && storedMappings(durable)[0]?.reason === 'create_outcome_unresolved', storedMappings(durable)[0]?.state);
      check('cleared transport never re-executes the create', client.executionsOf('new_session') === createsBefore);
    }

    // Canary replay against a permanently gone source session (a restart
    // that also lost the source) can never replay the create: converge on
    // the explicit unresolved conflict without a single create attempt.
    {
      const { durable, registry, client, createsBefore } = await seedLostCreate();
      client.resetLedger();
      client.canarySourceGone = true;
      await registry.recoverRunner(RUNNER_ID);
      const mapping = storedMappings(durable)[0];
      check('source-gone canary marks create_outcome_unresolved', mapping?.state === 'conflict'
        && mapping.reason === 'create_outcome_unresolved', mapping?.state);
      check('source-gone canary never replays the create', client.executionsOf('new_session') === createsBefore);
    }

    // A create whose FIRST delivery never reached the runner at all (the
    // connection dropped before the command was admitted — nothing in the
    // ledger) replays by EXECUTING fresh, and a fresh success that never
    // binds the exact requested identity converges on the explicit
    // unresolved conflict.
    {
      const durable = storage();
      const client = new FakeRunnerClient();
      // One-shot client-side loss: ONLY the first create delivery dies before
      // reaching the runner (no wire frame, no execution, nothing in the
      // ledger); the recovery replay then flows through the real client.
      let loseFirstCreate = true;
      const original = client.command.bind(client);
      client.command = async (runnerId, command, options = {}) => {
        if (command.type === 'new_session' && loseFirstCreate) {
          loseFirstCreate = false;
          throw new RunnerCommandError('connection closed', 'transport');
        }
        return original(runnerId, command, options);
      };
      const registry = mappingRegistry(durable, client);
      const projectId = await createProject(registry);
      await rejection(registry.mapProject(projectId, RUNNER_ID));
      check('pre-admission transport loss leaves unknown state',
        storedMappings(durable)[0]?.state === 'create_outcome_unknown', storedMappings(durable)[0]?.state);
      // The replay executes FRESH (the create was never admitted) and the
      // drifted success envelope names a foreign directory identity.
      client.createResponse = {
        type: 'response',
        id: undefined,
        command: 'new_session',
        sessionId: TARGET_ID,
        success: true,
        data: {
          sessionId: TARGET_ID,
          runtimeIncarnation: incarnationOf(TARGET_ID),
          state: { sessionId: TARGET_ID, sessionFile: TARGET_PATH, cwd: '/workspace/other' },
          session: { sessionId: TARGET_ID, runtimeIncarnation: incarnationOf(TARGET_ID) },
          messages: [],
        },
      };
      const createsBefore = client.executionsOf('new_session');
      await registry.recoverRunner(RUNNER_ID);
      const mapping = storedMappings(durable)[0];
      check('identity-drifting replay marks create_outcome_unresolved', mapping?.state === 'conflict'
        && mapping.reason === 'create_outcome_unresolved', mapping?.state);
      check('identity-drifting replay executes the fresh create once', client.calls.filter(
        (call) => call.command.type === 'new_session').length === createsBefore + 1);
      check('identity-drifting replay persists no session identity',
        mapping?.sessionId === undefined && mapping?.sessionPath === undefined);
    }
  }

  {
    activeBlock = 'catalog row canonical negatives';
    // Catalog row reconciliation binds the EXACT canonical production row.
    // A mapped row missing ANY canonical stamp (rpi source, native status,
    // loaded true, nonempty runtimeIncarnation, nonempty id/path/cwd) is
    // dropped — never treated as the exact mapped session — so provisioning
    // recovery sees the identity ABSENT and re-runs the authoritative
    // rebind (switch) and the stable name command instead of trusting a
    // degraded row.
    const negatives: Array<[string, (row: Record<string, unknown>) => Record<string, unknown>]> = [
      ['missing source', ({ source, ...row }) => row],
      ['foreign source', (row) => ({ ...row, source: 'pi' })],
      ['whitespace source', (row) => ({ ...row, source: ' rpi ' })],
      ['nonstring source', (row) => ({ ...row, source: 7 })],
      ['missing status', ({ status, ...row }) => row],
      ['foreign status', (row) => ({ ...row, status: 'foreign' })],
      ['missing loaded flag', ({ loaded, ...row }) => row],
      ['unloaded flag', (row) => ({ ...row, loaded: false })],
      ['missing runtimeIncarnation', ({ runtimeIncarnation, ...row }) => row],
      ['empty runtimeIncarnation', (row) => ({ ...row, runtimeIncarnation: '   ' })],
      ['empty sessionId', (row) => ({ ...row, sessionId: '' })],
      ['missing path', ({ path, ...row }) => row],
      ['empty cwd', (row) => ({ ...row, cwd: '' })],
    ];
    for (const [label, degrade] of negatives) {
      const { durable, client, registry } = await seedMapped('provisioning');
      client.rows.splice(0, client.rows.length, degrade(catalogRow({ name: 'chat-name' })));
      const switchesBefore = client.calls.filter((call) => call.command.type === 'switch_session').length;
      const namesBefore = client.calls.filter((call) => call.command.type === 'set_session_name').length;
      await registry.recoverRunner(RUNNER_ID);
      const mapping = storedMappings(durable)[0];
      check(`${label} degraded row forces the authoritative rebind`, client.calls.filter(
        (call) => call.command.type === 'switch_session').length === switchesBefore + 1, mapping?.state);
      check(`${label} degraded row cannot satisfy the identity catalog`, client.calls.filter(
        (call) => call.command.type === 'set_session_name').length === namesBefore + 1,
        String(namesBefore));
    }
    // The canonical control: a fully stamped production row carrying the
    // mapped session name satisfies provisioning recovery with NO rebind
    // and NO rename.
    {
      const { durable, client, registry, mapping } = await seedMapped('provisioning');
      // The seed's mapProject already issued the naming command; the control
      // asserts the RECOVERY adds no rebind and no rename on top of it (the
      // same baseline pattern the negatives loop above uses).
      const switchesBefore = client.calls.filter((call) => call.command.type === 'switch_session').length;
      const namesBefore = client.calls.filter((call) => call.command.type === 'set_session_name').length;
      client.rows.splice(0, client.rows.length, catalogRow({ name: mapping.sessionName }));
      await registry.recoverRunner(RUNNER_ID);
      check('canonical production row reconciles without rebind', client.calls.filter(
        (call) => call.command.type === 'switch_session').length === switchesBefore,
        JSON.stringify(client.calls.slice(-3)));
      check('canonical production row reconciles without rename', client.calls.filter(
        (call) => call.command.type === 'set_session_name').length === namesBefore);
      check('canonical production row reconciles active', storedMappings(durable)[0]?.state === 'active');
    }
  }

  check('all focused blocks completed', activeBlock !== '');
  {
    const here = dirname(fileURLToPath(import.meta.url));
    const app = readFileSync(join(here, '../src/chat/ChatApp.tsx'), 'utf8');
    check('push fingerprint includes collectRunnerTokens', app.includes('tokens: collectRunnerTokens(storage)'));
    check('push effect keys off runnerRecordKey', app.includes('storage, runnerRecordKey]);'));
  }
  console.log(`\nchatMapping.test: ${assertions} assertions, ${failures.length} failure(s)`);
  if (failures.length) {
    console.error(failures.join('\n'));
    process.exit(1);
  }
}

main().catch((error) => {
  console.error(`chatMapping.test (${activeBlock}): ${error instanceof Error ? error.message : String(error)}`);
  process.exit(1);
});
