import type { StorageLike } from '../hostToken';
import { createPageClientNamespace, nextCommandId, planSubscriptionSnapshot, type SubscriptionSnapshotFrame } from '../protocol';
import { safeText } from '../redact';
import { detachTransportHandlers } from '../socket';
import { rpcErrorCode, type RpcErrorCode, type RpcResponse } from '../types';
import {
  RunnerAuthRequiredError,
  RUNNER_TOKEN_REQUIRED_MESSAGE,
  RUNNER_UNREACHABLE_MESSAGE,
  fetchRunnerStatus,
  loadRunnerToken,
  parseRunnerEndpoint,
  saveRunnerToken,
  type PageHttpProtocol,
  type RunnerEndpoint,
  type RunnerStatus,
} from './auth';
import {
  RUNNER_COMMAND_TIMEOUT_MS,
  TMUX_TERMINAL_RESIZE_TIMEOUT_MS,
  RUNNER_RECONNECT_INITIAL_MS,
  identitiesFromRpcData,
  isRunnerEventFrame,
  isRpcResponse,
  parseTmuxTerminalExit,
  parseTmuxTerminalHistory,
  parseTmuxTerminalOutput,
  type RunnerCommand,
  lifecycleTargetRow,
  nextReconnectDelay,
  readNonemptyString,
  readRunnerProbe,
  readRuntimeIdentity,
  runnerProtocols,
  runtimeIdentityKey,
  type RunnerProbe,
  type RunnerRuntimeIdentity,
} from './protocol';
import { dispatchRunnerEvent } from './events';

export const RUNNER_STORAGE_KEY = 'rpi-web-chat-runners:v1';
const RUNNER_STORAGE_VERSION = 2;

export type RunnerConnectionState = 'connecting' | 'connected' | 'reconnecting' | 'token_required' | 'offline';

export interface RunnerRecord {
  id: string;
  label: string;
  origin: string;
  authority: string;
  createdAt: number;
  updatedAt: number;
  codename: string | null;
}

export interface RunnerView extends RunnerRecord {
  /** Last authoritative backend generation; advances only after a successful probe. */
  connectionEpoch: number;
  state: RunnerConnectionState;
  error: string;
  probe: RunnerProbe | null;
  status: RunnerStatus | null;
}

interface StoredRunnerDocument {
  version: number;
  runners: RunnerRecord[];
}

interface RunnerRuntime {
  record: RunnerRecord;
  socket: WebSocket | null;
  generation: number;
  sequence: number;
  namespace: string;
  reconnectDelay: number;
  reconnectTimer: number | null;
  stopped: boolean;
  view: RunnerView;
  pending: Map<string, PendingCommand>;
  pendingSubscriptions: Map<string, PendingSubscription>;
  sessionCursors: Map<string, number>;
  currentIdentities: Map<string, string>;
  firstProbe: FirstProbe | null;
  // Per-connection bootstrap ownership (listen.rs grants each connection
  // exactly one sessionId-less get_state): set once the backend binds this
  // connection's primary session, sent by every later get_state on this
  // connection, reset by openSocket() for each new backend connection.
  bootstrapSessionId: string | null;
  probeCompleted: boolean;
  preflightAbort: AbortController | null;
  activeEndpoint: RunnerEndpoint | null;
  prepareSeq: number;
  prepareCommittedSeq: number;
}

interface PendingCommand {
  generation: number;
  timeout: number;
  fingerprint: string;
  command: RunnerCommand['type'];
  /** Requested directory for new_session (null otherwise): the shared
   *  lifecycle row validator binds the created session to it exactly. */
  commandCwd: string | null;
  responseOwner: string | null;
  promise: Promise<RpcResponse>;
  resolve: (response: RpcResponse) => void;
  reject: (error: Error) => void;
}

interface PendingSubscription {
  generation: number;
  identity: RunnerRuntimeIdentity;
  timeout: number | null;
  ready: boolean;
  promise: Promise<void>;
  resolve: () => void;
  reject: (error: Error) => void;
}

export interface RunnerCommandOptions {
  commandId?: string;
  sourceSessionId?: string;
  allowBeforeProbe?: boolean;
  beforeSend?: () => void;
}

export interface RunnerSubscriptionError {
  type: 'subscription_error';
  command: 'subscribe';
  sessionId: string;
  runtimeIncarnation: string;
  error: string;
}

export class RunnerCommandError extends Error {
  readonly rpcCode?: RpcErrorCode;
  constructor(
    message: string,
    readonly kind: 'remote' | 'transport' | 'protocol',
    readonly response?: RpcResponse,
    readonly subscription?: RunnerSubscriptionError,
  ) {
    super(message);
    this.name = 'RunnerCommandError';
    this.rpcCode = rpcErrorCode(response?.data);
  }
}

interface FirstProbe {
  generation: number;
  resolve: (view: RunnerView) => void;
  reject: (error: Error) => void;
}

/** One prepareConnection attempt: the exact socket that opened it, its
 *  generation, the endpoint it was dialed on, and the monotonic seq token
 *  that owns its probe commit. */
interface PrepareAttempt {
  readonly seq: number;
  readonly generation: number;
  readonly endpoint: RunnerEndpoint;
  readonly socket: WebSocket;
}

export interface RunnerRegistryOptions {
  storage: StorageLike | null;
  webSocketFactory?: (url: string, protocols: string[]) => WebSocket;
  setTimeoutFn?: (handler: () => void, timeout: number) => number;
  clearTimeoutFn?: (timeout: number) => void;
  now?: () => number;
  randomBytes?: (length: number) => Uint8Array;
  beforeProbe?: (runnerId: string) => Promise<void>;
  fetchFn?: typeof fetch;
  pageProtocol?: PageHttpProtocol;
}

export type RunnerRegistryListener = (runners: RunnerView[]) => void;

function defaultWebSocketFactory(url: string, protocols: string[]): WebSocket {
  return new WebSocket(url, protocols);
}

function defaultSetTimeout(handler: () => void, timeout: number): number {
  return window.setTimeout(handler, timeout);
}

function defaultClearTimeout(timeout: number): void {
  window.clearTimeout(timeout);
}

function isRunnerRecordBase(value: unknown): value is Omit<RunnerRecord, 'codename'> {
  if (!value || typeof value !== 'object') return false;
  const record = value as Partial<RunnerRecord>;
  return typeof record.id === 'string'
    && record.id !== ''
    && typeof record.label === 'string'
    && typeof record.origin === 'string'
    && typeof record.authority === 'string'
    && typeof record.createdAt === 'number'
    && Number.isFinite(record.createdAt)
    && typeof record.updatedAt === 'number'
    && Number.isFinite(record.updatedAt);
}

export function isRunnerRecord(value: unknown): value is RunnerRecord {
  if (!isRunnerRecordBase(value)) return false;
  const codename = (value as { codename?: unknown }).codename;
  return codename === null || typeof codename === 'string';
}

export function loadRunnerRecords(storage: StorageLike | null): RunnerRecord[] {
  if (!storage) return [];
  try {
    const raw = storage.getItem(RUNNER_STORAGE_KEY);
    if (!raw) return [];
    const document = JSON.parse(raw) as Partial<StoredRunnerDocument>;
    if (document.version === 1) {
      // v1 → v2 migration: same key, same fields, codename seeded null.
      if (!Array.isArray(document.runners)) return [];
      const migrated = document.runners
        .filter(isRunnerRecordBase)
        .map((record) => ({ ...record, codename: null }));
      const collapsed = dedupeRunnersKeepHealthiest(migrated);
      try {
        storage.setItem(RUNNER_STORAGE_KEY, JSON.stringify({
          version: RUNNER_STORAGE_VERSION,
          runners: collapsed,
        } satisfies StoredRunnerDocument));
      } catch {
        // Migration persistence is best-effort; the migrated records still load.
      }
      return collapsed;
    }
    if (document.version !== RUNNER_STORAGE_VERSION || !Array.isArray(document.runners)) return [];
    const loaded = document.runners.filter(isRunnerRecord);
    const collapsed = dedupeRunnersKeepHealthiest(loaded);
    if (collapsed.length !== loaded.length) saveRunnerRecords(storage, collapsed);
    return collapsed;
  } catch {
    return [];
  }
}

export function saveRunnerRecords(storage: StorageLike | null, runners: RunnerRecord[]): boolean {
  if (!storage) return false;
  try {
    storage.setItem(RUNNER_STORAGE_KEY, JSON.stringify({
      version: RUNNER_STORAGE_VERSION,
      runners,
    } satisfies StoredRunnerDocument));
    return true;
  } catch {
    return false;
  }
}

function stripListenPortSuffix(name: string): string {
  const match = /^(.*)-(\d+)$/.exec(name);
  if (!match) return name;
  const port = Number(match[2]);
  if (!Number.isInteger(port) || port < 1024 || port > 65535) return name;
  return match[1] || name;
}

function hostnameFromAuthority(authority: string): string {
  const value = authority.trim();
  if (!value) return value;
  try {
    const url = new URL(value.includes('://') ? value : `http://${value}`);
    return url.hostname || value;
  } catch {
    return value.replace(/:\d+$/, '') || value;
  }
}

export function runnerIdentityName(record: Pick<RunnerRecord, 'codename' | 'label' | 'authority'>): string {
  return runnerDisplayName(record);
}

function runnerDedupeName(record: Pick<RunnerRecord, 'codename' | 'label' | 'authority'>): string {
  const named = record.codename?.trim() || record.label.trim();
  if (named) return named;
  return hostnameFromAuthority(record.authority);
}

export function runnerMentionName(
  record: Pick<RunnerRecord, 'codename' | 'label' | 'authority'>,
  peers: readonly Pick<RunnerRecord, 'codename' | 'label' | 'authority'>[],
): string {
  const display = runnerDisplayName(record).trim();
  const key = display.toLowerCase();
  if (key === '') return runnerDedupeName(record);
  const shared = peers.filter((peer) => runnerDisplayName(peer).trim().toLowerCase() === key);
  if (shared.length <= 1) return display;
  return runnerDedupeName(record);
}

export function runnerDisplayName(record: Pick<RunnerRecord, 'codename' | 'label' | 'authority'>): string {
  const named = record.codename?.trim() || record.label.trim();
  if (named) return stripListenPortSuffix(named);
  return hostnameFromAuthority(record.authority);
}

export function runnerDuplicateAuthority(
  runners: RunnerRecord[],
  name: string,
  excludeId?: string,
): string | null {
  const trimmed = name.trim();
  if (!trimmed) return null;
  const duplicate = runners.find((record) => (
    record.id !== excludeId
    && runnerDisplayName(record) === trimmed
  ));
  return duplicate ? duplicate.authority : null;
}

export function sameRunnerIdentity(
  left: Pick<RunnerRecord, 'authority' | 'codename' | 'label'>,
  right: Pick<RunnerRecord, 'authority' | 'codename' | 'label'>,
): boolean {
  const authorityLeft = left.authority.trim().toLowerCase();
  const authorityRight = right.authority.trim().toLowerCase();
  if (authorityLeft !== '' && authorityLeft === authorityRight) return true;
  const nameLeft = runnerDedupeName(left).trim().toLowerCase();
  const nameRight = runnerDedupeName(right).trim().toLowerCase();
  if (nameLeft === '' || nameRight === '') return false;
  return nameLeft === nameRight;
}

export function runnerHealthRank(state: RunnerConnectionState | undefined): number {
  switch (state) {
    case 'connected':
      return 4;
    case 'connecting':
      return 3;
    case 'reconnecting':
      return 2;
    case 'token_required':
      return 1;
    case 'offline':
      return 0;
    default:
      return 0;
  }
}

export function sortRunnersOnlineFirst<T extends { id: string; createdAt: number; state?: RunnerConnectionState }>(
  runners: readonly T[],
): T[] {
  return [...runners].sort((left, right) => {
    const health = runnerHealthRank(right.state) - runnerHealthRank(left.state);
    if (health !== 0) return health;
    return left.createdAt - right.createdAt || left.id.localeCompare(right.id);
  });
}

type RunnerDedupeRecord = Pick<RunnerRecord, 'id' | 'authority' | 'codename' | 'label' | 'createdAt' | 'updatedAt'> & {
  state?: RunnerConnectionState;
};

function preferHealthiestRunner<T extends RunnerDedupeRecord>(left: T, right: T): T {
  const health = runnerHealthRank(left.state) - runnerHealthRank(right.state);
  if (health !== 0) return health > 0 ? left : right;
  if (left.updatedAt !== right.updatedAt) return left.updatedAt > right.updatedAt ? left : right;
  if (left.createdAt !== right.createdAt) return left.createdAt < right.createdAt ? left : right;
  return left.id <= right.id ? left : right;
}

export function dedupeRunnersKeepHealthiest<T extends RunnerDedupeRecord>(runners: readonly T[]): T[] {
  const groups: T[][] = [];
  for (const runner of runners) {
    const group = groups.find((members) => members.some((member) => sameRunnerIdentity(member, runner)));
    if (group) group.push(runner);
    else groups.push([runner]);
  }
  return groups
    .map((group) => group.reduce(preferHealthiestRunner))
    .sort((left, right) => left.createdAt - right.createdAt || left.id.localeCompare(right.id));
}

export function createRunnerId(
  randomBytes: (length: number) => Uint8Array = (length) => {
    const bytes = new Uint8Array(length);
    globalThis.crypto.getRandomValues(bytes);
    return bytes;
  },
): string {
  return Array.from(randomBytes(8), (byte) => byte.toString(16).padStart(2, '0')).join('');
}

function subscriptionKey(generation: number, identity: RunnerRuntimeIdentity): string {
  return `${generation}:${runtimeIdentityKey(identity)}`;
}

function subscriptionControlType(
  value: unknown,
): 'subscription_snapshot' | 'session_resync' | 'subscription_error' | null {
  if (!value || typeof value !== 'object') return null;
  const type = (value as { type?: unknown }).type;
  if (type === 'subscription_snapshot' || type === 'session_resync' || type === 'subscription_error') {
    return type;
  }
  return null;
}

/** Target-owned lifecycle success gate (backend contract:
 *  session_runtime_manager.rs snapshot_response stamps the OPENED session's
 *  id on the success envelope of a full-snapshot lifecycle command — on the
 *  Chat wire those are new_session and switch_session; failures stay
 *  request-source-owned, and the negotiated source-owned switch handoff is
 *  not part of the Chat protocol). The FULL canonical row gate
 *  (protocol.ts lifecycleTargetRow) is shared with mapping.ts: envelope,
 *  data, state, and the authoritative row must agree on the exact target
 *  identity and incarnation, and the row must bind the exact requested
 *  directory (new_session), native status, loaded flag, and source. Any
 *  disagreement, absence, or empty field returns null so the source-owned
 *  ownership check below fails the response closed. */
function lifecycleEnvelopeTarget(response: RpcResponse, commandCwd: string | null): string | null {
  const command = response.command === 'new_session' || response.command === 'switch_session'
    ? response.command
    : null;
  if (command === null) return null;
  return lifecycleTargetRow(response, command, commandCwd)?.sessionId ?? null;
}

function isListenLocalTmuxCommand(
  command: RunnerCommand,
): command is Extract<RunnerCommand, { type: `tmux_${string}` }> {
  switch (command.type) {
    case 'tmux_list_sessions':
    case 'tmux_pane_snapshot':
    case 'tmux_send_keys':
    case 'tmux_session_create':
    case 'tmux_session_delete':
    case 'tmux_terminal_attach':
    case 'tmux_terminal_input':
    case 'tmux_terminal_resize':
    case 'tmux_terminal_detach':
      return true;
    default: {
      const sessionScoped: Exclude<RunnerCommand['type'], `tmux_${string}`> = command.type;
      void sessionScoped;
      return false;
    }
  }
}

export class RunnerRegistry {
  private readonly storage: StorageLike | null;
  private readonly webSocketFactory: (url: string, protocols: string[]) => WebSocket;
  private readonly setTimeoutFn: (handler: () => void, timeout: number) => number;
  private readonly clearTimeoutFn: (timeout: number) => void;
  private readonly now: () => number;
  private readonly randomBytes?: (length: number) => Uint8Array;
  private beforeProbe?: (runnerId: string) => Promise<void>;
  private readonly fetchFn: typeof fetch;
  private readonly pageProtocol?: PageHttpProtocol;
  private readonly runners = new Map<string, RunnerRuntime>();
  private readonly listeners = new Set<RunnerRegistryListener>();
  private started = false;
  private disposed = false;

  constructor(options: RunnerRegistryOptions) {
    this.storage = options.storage;
    this.webSocketFactory = options.webSocketFactory ?? defaultWebSocketFactory;
    this.setTimeoutFn = options.setTimeoutFn ?? defaultSetTimeout;
    this.clearTimeoutFn = options.clearTimeoutFn ?? defaultClearTimeout;
    this.now = options.now ?? Date.now;
    this.randomBytes = options.randomBytes;
    this.beforeProbe = options.beforeProbe;
    this.fetchFn = options.fetchFn ?? fetch.bind(globalThis);
    this.pageProtocol = options.pageProtocol;
    for (const record of loadRunnerRecords(this.storage)) {
      this.runners.set(record.id, this.createRuntime(record));
    }
  }

  setBeforeProbe(beforeProbe: (runnerId: string) => Promise<void>): void {
    this.beforeProbe = beforeProbe;
  }

  subscribe(listener: RunnerRegistryListener): () => void {
    this.listeners.add(listener);
    listener(this.snapshot());
    return () => {
      this.listeners.delete(listener);
    };
  }

  snapshot(): RunnerView[] {
    return dedupeRunnersKeepHealthiest(
      Array.from(this.runners.values(), (runner) => ({ ...runner.view })),
    );
  }

  start(): void {
    if (this.started || this.disposed) return;
    this.started = true;
    for (const runner of this.runners.values()) this.connect(runner);
    // Phone timers freeze while backgrounded; Open to trust is a new tab
    // whose return is often `focus`/`pageshow` without `visibilitychange`.
    this.bindWakeListeners();
  }

  private bindWakeListeners(): void {
    if (typeof document !== 'undefined') {
      document.addEventListener('visibilitychange', this.onWake);
    }
    if (typeof window !== 'undefined') {
      window.addEventListener('focus', this.onWake);
      window.addEventListener('pageshow', this.onWake);
    }
  }

  private unbindWakeListeners(): void {
    if (typeof document !== 'undefined') {
      document.removeEventListener('visibilitychange', this.onWake);
    }
    if (typeof window !== 'undefined') {
      window.removeEventListener('focus', this.onWake);
      window.removeEventListener('pageshow', this.onWake);
    }
  }

  private shouldWake(runner: RunnerRuntime): boolean {
    if (runner.stopped) return false;
    switch (runner.view.state) {
      case 'connecting':
      case 'reconnecting':
      case 'offline':
        return true;
      case 'token_required':
        return loadRunnerToken(this.storage, runner.record.authority) !== '';
      case 'connected':
        return false;
    }
  }

  private readonly onWake = (): void => {
    if (this.disposed) return;
    if (typeof document !== 'undefined' && document.visibilityState === 'hidden') return;
    for (const runner of this.runners.values()) {
      if (!this.shouldWake(runner)) continue;
      this.cancelReconnect(runner);
      runner.reconnectDelay = RUNNER_RECONNECT_INITIAL_MS;
      this.connect(runner);
    }
  };

  stop(): void {
    if (this.disposed) return;
    this.started = false;
    this.disposed = true;
    this.unbindWakeListeners();
    for (const runner of this.runners.values()) this.stopRuntime(runner, 'chat page closed');
  }

  register(label: string, address: string, token: string): { runner: RunnerView; connected: Promise<RunnerView> } {
    if (this.disposed) throw new Error('Chat runner registry is closed.');
    const endpoint = parseRunnerEndpoint(address, this.pageProtocol);
    if (!this.started) this.started = true;
    const trimmedLabel = label.trim() || endpoint.authority;
    const now = this.now();
    const authorityOwner = Array.from(this.runners.values())
      .find((runner) => runner.record.authority === endpoint.authority);
    if (authorityOwner && authorityOwner.record.origin !== endpoint.origin) {
      throw new Error('A runner already uses this network authority with a different scheme.');
    }
    const duplicate = authorityOwner
      ?? Array.from(this.runners.values()).find((runner) => (
        sameRunnerIdentity(runner.record, {
          authority: endpoint.authority,
          codename: null,
          label: trimmedLabel,
        })
      ));
    if (duplicate) {
      const previousRecord = duplicate.record;
      this.stopRuntime(duplicate, 'runner replaced');
      duplicate.record = {
        ...previousRecord,
        label: trimmedLabel,
        authority: endpoint.authority,
        origin: endpoint.origin,
        updatedAt: now,
      };
      duplicate.stopped = false;
      duplicate.view = this.initialView(duplicate.record, duplicate.view.connectionEpoch);
      duplicate.reconnectDelay = RUNNER_RECONNECT_INITIAL_MS;
      try {
        this.persist();
      } catch (error) {
        duplicate.record = previousRecord;
        duplicate.view = this.initialView(previousRecord, duplicate.view.connectionEpoch);
        this.emit();
        if (this.started) this.connect(duplicate);
        throw error;
      }
      saveRunnerToken(this.storage, endpoint.authority, token);
      this.emit();
      const connected = this.armFirstProbe(duplicate);
      this.connect(duplicate);
      return { runner: { ...duplicate.view }, connected };
    }

    const record: RunnerRecord = {
      id: createRunnerId(this.randomBytes),
      label: trimmedLabel,
      origin: endpoint.origin,
      authority: endpoint.authority,
      createdAt: now,
      updatedAt: now,
      codename: null,
    };
    const runtime = this.createRuntime(record);
    this.runners.set(record.id, runtime);
    try {
      this.persist();
    } catch (error) {
      this.runners.delete(record.id);
      throw error;
    }
    saveRunnerToken(this.storage, endpoint.authority, token);
    this.emit();
    const connected = this.armFirstProbe(runtime);
    this.connect(runtime);
    return { runner: { ...runtime.view }, connected };
  }

  add(label: string, address: string, token: string): Promise<RunnerView> {
    try {
      return this.register(label, address, token).connected;
    } catch (error) {
      return Promise.reject(error);
    }
  }

  reconnect(id: string): void {
    if (this.disposed) return;
    const runner = this.runners.get(id);
    if (!runner) return;
    runner.stopped = false;
    runner.reconnectDelay = RUNNER_RECONNECT_INITIAL_MS;
    this.connect(runner);
  }

  command(
    id: string,
    command: RunnerCommand,
    options: RunnerCommandOptions = {},
  ): Promise<RpcResponse> {
    const runner = this.runners.get(id);
    const canSendBeforeProbe = options.allowBeforeProbe === true
      && runner?.socket?.readyState === 1;
    if (!runner || (runner.view.state !== 'connected' && !canSendBeforeProbe)) {
      return Promise.reject(new RunnerCommandError('Runner is not connected.', 'transport'));
    }
    return this.commandOnRuntime(runner, command, options);
  }

  private async commandOnRuntime(
    runner: RunnerRuntime,
    command: RunnerCommand,
    options: RunnerCommandOptions,
  ): Promise<RpcResponse> {
    if (this.disposed) {
      return Promise.reject(new RunnerCommandError('Chat runner registry is closed.', 'transport'));
    }
    // Listener tmux ops are listen-local (rpc.rs) and belong to the socket,
    // not a chat session; none of them may wait on a chat subscription.
    if (isListenLocalTmuxCommand(command)) {
      options.beforeSend?.();
      return this.sendCommand(runner, runner.generation, command, options.commandId);
    }
    const sourceSessionId = options.sourceSessionId?.trim() || runner.view.probe?.sessionId || '';
    if (!sourceSessionId) {
      return Promise.reject(new RunnerCommandError('Runner session identity is unavailable.', 'protocol'));
    }
    const generation = runner.generation;
    await this.ensureSubscription(runner, generation, sourceSessionId);
    if (!this.isCurrentGeneration(runner, generation)) {
      return Promise.reject(new RunnerCommandError('Runner is not connected.', 'transport'));
    }
    options.beforeSend?.();
    return this.sendCommand(
      runner,
      generation,
      command,
      options.commandId,
      sourceSessionId,
    );
  }

  updateToken(id: string, token: string): Promise<RunnerView> {
    if (this.disposed) return Promise.reject(new Error('Chat runner registry is closed.'));
    const runner = this.runners.get(id);
    if (!runner) return Promise.reject(new Error('Runner was not found.'));
    this.stopRuntime(runner, 'runner credentials replaced');
    runner.stopped = false;
    runner.reconnectDelay = RUNNER_RECONNECT_INITIAL_MS;
    runner.view = this.initialView(runner.record, runner.view.connectionEpoch);
    saveRunnerToken(this.storage, runner.record.authority, token);
    this.emit();
    const firstProbe = this.armFirstProbe(runner);
    this.connect(runner);
    return firstProbe;
  }

  delete(id: string): void {
    const runner = this.runners.get(id);
    if (!runner) return;
    this.stopRuntime(runner, 'runner deleted');
    this.runners.delete(id);
    const authorityIsUnused = !Array.from(this.runners.values())
      .some((candidate) => candidate.record.authority === runner.record.authority);
    try {
      this.persist();
    } catch (error) {
      runner.stopped = false;
      runner.view = this.initialView(runner.record, runner.view.connectionEpoch);
      this.runners.set(id, runner);
      this.emit();
      if (this.started) this.connect(runner);
      throw error;
    }
    if (authorityIsUnused) saveRunnerToken(this.storage, runner.record.authority, '');
    this.emit();
  }

  /** Persisted records for server-side sync pushes (no tokens — those stay
   *  in the host token store and ride the sync document separately). */
  exportRecords(): RunnerRecord[] {
    return Array.from(this.runners.values(), (runner) => ({ ...runner.record }));
  }

  /** Adopt server-synced records at boot: inserts runners this profile has
   *  never seen, keyed by authority; local records win on conflicts so a
   *  connected session is never displaced. Returns the records actually
   *  inserted plus a remote-record-id → local-runtime-id map (identity for
   *  fresh inserts, the pre-existing local id for an id or authority
   *  match) so the caller can rewrite synced mapping.runnerId values onto
   *  local identities. Inserts connect after persist succeeds — a failed
   *  persist rolls the inserts back and throws. Parked token_required
   *  runtimes with a nonempty stored token also connect (boot adoption may
   *  have just replaced that token). */
  importRecords(records: RunnerRecord[]): {
    inserted: RunnerRecord[];
    idByAuthority: Map<string, string>;
  } {
    const inserted: RunnerRecord[] = [];
    const idByAuthority = new Map<string, string>();
    for (const record of records) {
      if (!record || typeof record.id !== 'string' || record.id === '') continue;
      if (typeof record.authority !== 'string' || record.authority === '') continue;
      if (this.runners.has(record.id)) {
        idByAuthority.set(record.id, record.id);
        continue;
      }
      const identityMatch = Array.from(this.runners.values())
        .find((candidate) => sameRunnerIdentity(candidate.record, record));
      if (identityMatch) {
        idByAuthority.set(record.id, identityMatch.record.id);
        continue;
      }
      this.runners.set(record.id, this.createRuntime(record));
      inserted.push(record);
      idByAuthority.set(record.id, record.id);
    }
    if (inserted.length > 0) {
      try {
        this.persist();
      } catch (error) {
        for (const record of inserted) this.runners.delete(record.id);
        throw error;
      }
      this.emit();
    }
    if (this.started) {
      const insertedIds = new Set(inserted.map((record) => record.id));
      for (const runner of this.runners.values()) {
        if (runner.stopped) continue;
        const parkedWithToken = runner.view.state === 'token_required'
          && loadRunnerToken(this.storage, runner.record.authority) !== '';
        if (insertedIds.has(runner.record.id) || parkedWithToken) this.connect(runner);
      }
    }
    return { inserted, idByAuthority };
  }

  private createRuntime(record: RunnerRecord): RunnerRuntime {
    return {
      record,
      socket: null,
      generation: 0,
      sequence: 0,
      namespace: createPageClientNamespace(this.randomBytes),
      reconnectDelay: RUNNER_RECONNECT_INITIAL_MS,
      reconnectTimer: null,
      stopped: false,
      view: this.initialView(record),
      pending: new Map(),
      pendingSubscriptions: new Map(),
      sessionCursors: new Map(),
      currentIdentities: new Map(),
      firstProbe: null,
      bootstrapSessionId: null,
      probeCompleted: false,
      preflightAbort: null,
      activeEndpoint: null,
      prepareSeq: 0,
      prepareCommittedSeq: 0,
    };
  }
  private initialView(record: RunnerRecord, connectionEpoch = 0, status: RunnerStatus | null = null): RunnerView {
    return { ...record, connectionEpoch, state: 'connecting', error: '', probe: null, status };
  }

  private armFirstProbe(runner: RunnerRuntime): Promise<RunnerView> {
    this.rejectFirstProbe(runner, new Error('Runner probe was replaced.'));
    return new Promise((resolve, reject) => {
      runner.firstProbe = {
        generation: runner.generation + 1,
        resolve,
        reject,
      };
    });
  }

  private abortPreflight(runner: RunnerRuntime): void {
    const pending = runner.preflightAbort;
    runner.preflightAbort = null;
    pending?.abort();
  }

  private persistCorrectedOrigin(runner: RunnerRuntime, endpoint: RunnerEndpoint): void {
    if (runner.record.origin === endpoint.origin && runner.record.authority === endpoint.authority) {
      return;
    }
    const previous = runner.record;
    runner.record = {
      ...previous,
      origin: endpoint.origin,
      authority: endpoint.authority,
      updatedAt: this.now(),
    };
    try {
      this.persist();
    } catch {
      runner.record = previous;
    }
  }

  private connect(runner: RunnerRuntime): void {
    if (runner.stopped || !this.runners.has(runner.record.id)) return;
    this.cancelReconnect(runner);
    this.abortPreflight(runner);
    this.replaceSocket(runner, 'runner reconnecting');
    const generation = ++runner.generation;
    this.updateView(runner, {
      state: runner.view.probe ? 'reconnecting' : 'connecting',
      error: '',
    });
    void this.connectWithEndpoint(
      runner,
      generation,
      parseRunnerEndpoint(runner.record.origin, this.pageProtocol),
    );
  }

  private async connectWithEndpoint(
    runner: RunnerRuntime,
    generation: number,
    endpoint: RunnerEndpoint,
  ): Promise<void> {
    if (!this.isCurrentGeneration(runner, generation)) return;
    const controller = new AbortController();
    this.abortPreflight(runner);
    runner.preflightAbort = controller;
    const result = await fetchRunnerStatus(endpoint, this.fetchFn, controller.signal);
    if (runner.preflightAbort === controller) runner.preflightAbort = null;
    if (!this.isCurrentGeneration(runner, generation) || controller.signal.aborted) return;
    if (!result.ok) {
      this.failUnreachable(runner, generation);
      return;
    }
    const status = result.status;

    const storedToken = loadRunnerToken(this.storage, endpoint.authority);
    this.updateView(runner, { status });
    if (status.tokenRequired && storedToken === '') {
      this.haltForToken(runner, generation, status);
      return;
    }
    const socketToken = status.tokenRequired ? storedToken : '';
    this.openSocket(runner, generation, endpoint, socketToken);
  }
  private failUnreachable(runner: RunnerRuntime, generation: number): void {
    if (!this.isCurrentGeneration(runner, generation)) return;
    this.rejectPending(runner, generation, RUNNER_UNREACHABLE_MESSAGE);
    this.updateView(runner, { state: 'reconnecting', error: RUNNER_UNREACHABLE_MESSAGE, status: null });
    this.rejectFirstProbe(runner, new Error(RUNNER_UNREACHABLE_MESSAGE), generation);
    this.scheduleReconnect(runner, generation);
  }

  private haltForToken(
    runner: RunnerRuntime,
    generation: number,
    status: RunnerStatus,
    message: string = RUNNER_TOKEN_REQUIRED_MESSAGE,
  ): void {
    if (!this.isCurrentGeneration(runner, generation)) return;
    this.rejectPending(runner, generation, message);
    this.updateView(runner, {
      state: 'token_required',
      error: message,
      status,
    });
    this.rejectFirstProbe(runner, new RunnerAuthRequiredError(message), generation);
  }

  private openSocket(
    runner: RunnerRuntime,
    generation: number,
    endpoint: RunnerEndpoint,
    token: string,
  ): void {
    if (!this.isCurrentGeneration(runner, generation)) return;
    runner.activeEndpoint = endpoint;
    this.resetConnectionOwnedState(runner);
    let socket: WebSocket;
    try {
      socket = this.webSocketFactory(endpoint.webSocketUrl, runnerProtocols(token));
    } catch (error) {
      this.failConnection(runner, generation, error, token !== '');
      return;
    }
    runner.socket = socket;
    socket.onopen = () => {
      if (!this.isCurrent(runner, generation, socket)) return;
      void this.prepareConnection(runner, generation, endpoint, socket).catch((error) => {
        if (!this.isCurrent(runner, generation, socket)) return;
        this.failConnection(runner, generation, error, token !== '');
      });
    };
    socket.onmessage = (event: MessageEvent) => {
      if (!this.isCurrent(runner, generation, socket) || typeof event.data !== 'string') return;
      let frame: unknown;
      try {
        frame = JSON.parse(event.data);
      } catch {
        return;
      }
      if (isRpcResponse(frame)) {
        this.settleResponse(runner, generation, frame);
        return;
      }
      if (this.settleSubscription(runner, generation, frame)) return;
      if (this.handleSessionUnload(runner, frame)) return;
      // App.tsx:3446-3450: non-response frames with a string type are events.
      // Listeners are module-scoped by runnerId, so reconnect generations
      // resume the same set — no second subscribe loop.
      if (isRunnerEventFrame(frame)) {
        // PTY overlay frames are machine-scoped: they carry terminalId, not
        // sessionId/runtimeIncarnation. Validate the frozen shape and fan
        // them out on the current connection before the session-runtime gate
        // (which would drop them). Never invent a session identity.
        if (parseTmuxTerminalOutput(frame) || parseTmuxTerminalExit(frame) || parseTmuxTerminalHistory(frame)) {
          dispatchRunnerEvent(runner.record.id, frame);
          return;
        }
        const identity = this.frameIdentity(frame);
        if (!identity || !this.isCurrentIdentity(runner, identity)) return;
        this.trackSessionCursor(runner, frame);
        dispatchRunnerEvent(runner.record.id, frame);
      }
    };
    socket.onclose = (event: CloseEvent) => {
      if (!this.isCurrent(runner, generation, socket)) return;
      runner.socket = null;
      this.rejectPending(runner, generation, 'connection closed');
      if (runner.stopped) return;
      if (event.code === 1006 && runner.view.status?.tokenRequired && !runner.probeCompleted) {
        this.haltForToken(runner, generation, runner.view.status);
        return;
      }
      const error = event.code === 1006
        ? RUNNER_UNREACHABLE_MESSAGE
        : event.code === 4000 && runner.view.error
          ? runner.view.error
          : `Connection closed (code ${event.code}).`;
      this.updateView(runner, { state: 'reconnecting', error });
      this.rejectFirstProbe(runner, new Error(error), generation);
      this.scheduleReconnect(runner, generation);
    };
    socket.onerror = () => undefined;
  }

  private async prepareConnection(
    runner: RunnerRuntime,
    generation: number,
    endpoint: RunnerEndpoint,
    socket: WebSocket,
  ): Promise<void> {
    const attempt: PrepareAttempt = {
      seq: ++runner.prepareSeq,
      generation,
      endpoint,
      socket,
    };
    if (!this.isCurrentGeneration(runner, generation)) return;
    await this.probe(runner, generation, attempt);
  }

  private async probe(runner: RunnerRuntime, generation: number, attempt: PrepareAttempt): Promise<void> {
    const endpoint = attempt.endpoint;
    const stateResponse = await this.sendCommand(
      runner,
      generation,
      { type: 'get_state' },
      undefined,
      runner.bootstrapSessionId ?? undefined,
    );
    const state = stateResponse.data;
    if (!this.isCurrentGeneration(runner, generation)) return;
    const primary = this.identityFromStateData(state);
    if (!primary) {
      throw new Error(
        this.stateSessionId(state)
          ? 'get_state response is missing runtime incarnation.'
          : 'get_state response is missing runner session identity.',
      );
    }
    runner.bootstrapSessionId = primary.sessionId;
    const sessionsResponse = await this.sendCommand(
      runner,
      generation,
      { type: 'session_list', scope: 'current' },
      undefined,
      primary.sessionId,
    );
    if (!this.isCurrentGeneration(runner, generation)) return;
    const probe = readRunnerProbe(state, sessionsResponse.data);
    // Stale reverse completion gate: only the NEWEST prepare attempt may
    // persist probe results. A suspended older attempt that resumes after a
    // newer attempt already committed (reverse completion) may still send
    // its redundant probe frames over the live socket, but its completion
    // must never persist the obsolete endpoint origin, overwrite the newer
    // probe/view, re-resolve the first probe, or run mapping recovery —
    // whichever order the two completions arrive in.
    if (attempt.seq <= runner.prepareCommittedSeq) return;
    runner.prepareCommittedSeq = attempt.seq;
    runner.reconnectDelay = RUNNER_RECONNECT_INITIAL_MS;
    runner.probeCompleted = true;
    this.persistCorrectedOrigin(runner, endpoint);
    if (probe.runnerCodename !== null && probe.runnerCodename !== runner.record.codename) {
      runner.record = { ...runner.record, codename: probe.runnerCodename };
      try {
        this.persist();
      } catch {
        // Codename adoption persistence is best-effort; the in-memory record keeps it.
      }
    }
    this.updateView(runner, { connectionEpoch: generation, error: '', probe });
    if (this.beforeProbe) await this.beforeProbe(runner.record.id);
    if (!this.isCurrent(runner, generation, attempt.socket)) return;
    if (attempt.seq !== runner.prepareCommittedSeq) return;
    this.updateView(runner, { connectionEpoch: generation, state: 'connected', error: '', probe });
    dispatchRunnerEvent(runner.record.id, { type: 'runner_connected', connectionEpoch: generation });
    this.resolveFirstProbe(runner, generation);
  }

  private sendCommand(
    runner: RunnerRuntime,
    generation: number,
    command: RunnerCommand,
    commandId?: string,
    sourceSessionId?: string,
  ): Promise<RpcResponse> {
    const socket = runner.socket;
    if (!socket || socket.readyState !== 1 || !this.isCurrentGeneration(runner, generation)) {
      return Promise.reject(new RunnerCommandError('Runner is not connected.', 'transport'));
    }
    const id = commandId?.trim() || nextCommandId(runner.namespace, ++runner.sequence);
    if (!id) return Promise.reject(new RunnerCommandError('Command identity is unavailable.', 'protocol'));
    const sessionId = sourceSessionId?.trim();
    const frame = sessionId ? { ...command, id, sessionId } : { ...command, id };
    const fingerprint = JSON.stringify(frame);
    const existing = runner.pending.get(id);
    if (existing) {
      if (existing.generation === generation && existing.fingerprint === fingerprint) return existing.promise;
      return Promise.reject(new RunnerCommandError(
        'Command identity is already pending for a different request.',
        'protocol',
      ));
    }
    let resolvePending: (response: RpcResponse) => void = () => {};
    let rejectPending: (error: Error) => void = () => {};
    const promise = new Promise<RpcResponse>((resolve, reject) => {
      resolvePending = resolve;
      rejectPending = reject;
    });
    const timeout = this.setTimeoutFn(() => {
      const pending = runner.pending.get(id);
      if (!pending || pending.generation !== generation) return;
      runner.pending.delete(id);
      pending.reject(new RunnerCommandError(`${command.type} timed out.`, 'transport'));
      // Fit/liveness resize timeout must not retire the socket: that kills the attach PTY.
      if (command.type === 'tmux_terminal_resize') return;
      this.failConnection(runner, generation, new Error(`${command.type} timed out.`), false);
    }, command.type === 'tmux_terminal_resize' ? TMUX_TERMINAL_RESIZE_TIMEOUT_MS : RUNNER_COMMAND_TIMEOUT_MS);
    runner.pending.set(id, {
      generation,
      timeout,
      fingerprint,
      command: command.type,
      commandCwd: command.type === 'new_session' ? command.cwd : null,
      responseOwner: sessionId || null,
      promise,
      resolve: resolvePending,
      reject: rejectPending,
    });
    try {
      socket.send(fingerprint);
    } catch (error) {
      this.clearTimeoutFn(timeout);
      runner.pending.delete(id);
      rejectPending(new RunnerCommandError(
        error instanceof Error ? error.message : String(error),
        'transport',
      ));
      this.failConnection(runner, generation, error, false);
    }
    return promise;
  }

  private settleResponse(runner: RunnerRuntime, generation: number, response: RpcResponse): void {
    const id = response.id ?? '';
    const pending = runner.pending.get(id);
    if (!pending || pending.generation !== generation || !this.isCurrentGeneration(runner, generation)) return;
    const responseOwner = response.sessionId ?? null;
    const ownsBootstrap = pending.responseOwner === null && responseOwner !== null;
    // Lifecycle success (new_session/switch_session on the Chat wire —
    // full-snapshot lifecycle commands; fork/clone/session_fork_latest
    // share the shape on the App wire but are not Chat commands) is
    // TARGET-owned: the backend stamps the OPENED session on the success
    // envelope, which never equals the source session the command was sent
    // from. It is accepted ONLY when envelope, data, state, and the
    // authoritative session row agree on the exact target identity AND the
    // exact nonempty runtimeIncarnation is present where the wire supplies
    // it (lifecycleEnvelopeTarget) — there is no source-owned fallback for
    // a lifecycle success. Failures and ordinary commands stay
    // request-source-owned.
    const lifecycleSuccess = response.success
      && (response.command === 'new_session' || response.command === 'switch_session');
    const ownershipHolds = lifecycleSuccess
      ? lifecycleEnvelopeTarget(response, pending.commandCwd) !== null
      : ownsBootstrap || responseOwner === pending.responseOwner;
    if (response.command !== pending.command || !ownershipHolds) {
      this.clearTimeoutFn(pending.timeout);
      runner.pending.delete(id);
      pending.reject(new RunnerCommandError('Runner response ownership did not match the request.', 'protocol', response));
      return;
    }
    this.clearTimeoutFn(pending.timeout);
    runner.pending.delete(id);
    if (response.success) {
      if (ownsBootstrap) runner.bootstrapSessionId = responseOwner;
      this.learnIdentities(runner, response.command, response.data);
      pending.resolve(response);
      return;
    }
    pending.reject(new RunnerCommandError(
      response.error || `${response.command} failed.`,
      'remote',
      response,
    ));
  }

  private ensureSubscription(
    runner: RunnerRuntime,
    generation: number,
    sessionId: string,
  ): Promise<void> {
    const identity = this.currentIdentity(runner, sessionId);
    if (!identity) {
      return Promise.reject(new RunnerCommandError('Runner session identity is unavailable.', 'protocol'));
    }
    const key = subscriptionKey(generation, identity);
    const existing = runner.pendingSubscriptions.get(key);
    if (existing) return existing.promise;
    const socket = runner.socket;
    if (!socket || socket.readyState !== 1 || !this.isCurrentGeneration(runner, generation)) {
      return Promise.reject(new RunnerCommandError('Runner is not connected.', 'transport'));
    }
    let resolvePending = (): void => {};
    let rejectPending = (_error: Error): void => {};
    const promise = new Promise<void>((resolve, reject) => {
      resolvePending = resolve;
      rejectPending = reject;
    });
    const timeout = this.setTimeoutFn(() => {
      const pending = runner.pendingSubscriptions.get(key);
      if (!pending || pending.generation !== generation || pending.ready) return;
      runner.pendingSubscriptions.delete(key);
      pending.reject(new RunnerCommandError('subscribe timed out.', 'transport'));
      try {
        socket.close(4000, 'command timeout');
      } catch {
        return;
      }
    }, RUNNER_COMMAND_TIMEOUT_MS);
    runner.pendingSubscriptions.set(key, {
      generation,
      identity,
      timeout,
      ready: false,
      promise,
      resolve: resolvePending,
      reject: rejectPending,
    });
    try {
      const cursor = runner.sessionCursors.get(runtimeIdentityKey(identity)) ?? 0;
      socket.send(JSON.stringify({
        type: 'subscribe',
        sessionId: identity.sessionId,
        runtimeIncarnation: identity.runtimeIncarnation,
        cursor,
      }));
    } catch (error) {
      this.clearTimeoutFn(timeout);
      runner.pendingSubscriptions.delete(key);
      rejectPending(new RunnerCommandError(
        error instanceof Error ? error.message : String(error),
        'transport',
      ));
    }
    return promise;
  }

  private settleSubscription(runner: RunnerRuntime, generation: number, frame: unknown): boolean {
    const type = subscriptionControlType(frame);
    if (!type) return false;
    if (!this.isCurrentGeneration(runner, generation)) return true;
    const identity = this.frameIdentity(frame);
    if (!identity || !this.isCurrentIdentity(runner, identity)) return true;
    const key = subscriptionKey(generation, identity);
    const pending = runner.pendingSubscriptions.get(key);
    if (type === 'subscription_error') {
      if (!pending || pending.ready) return true;
      if (pending.timeout !== null) {
        this.clearTimeoutFn(pending.timeout);
        pending.timeout = null;
      }
      runner.pendingSubscriptions.delete(key);
      const error = frame && typeof frame === 'object' && 'error' in frame && typeof frame.error === 'string'
        ? frame.error
        : '';
      const message = error || 'subscribe failed.';
      pending.reject(new RunnerCommandError(message, 'remote', {
        type: 'response',
        command: 'subscribe',
        success: false,
        sessionId: identity.sessionId,
        error: message,
      }, {
        type: 'subscription_error',
        command: 'subscribe',
        sessionId: identity.sessionId,
        runtimeIncarnation: identity.runtimeIncarnation,
        error: message,
      }));
      return true;
    }
    let plan;
    try {
      plan = planSubscriptionSnapshot(
        runner.sessionCursors.get(runtimeIdentityKey(identity)) ?? 0,
        frame as SubscriptionSnapshotFrame,
      );
    } catch {
      return true;
    }
    if (plan.identity.sessionId !== identity.sessionId
      || plan.identity.runtimeIncarnation !== identity.runtimeIncarnation) {
      return true;
    }
    // Publish the authority verdict WITH the cut and BEFORE the cursor
    // advances: consumers apply flagged messages as the transcript base, so
    // an unconsumed authoritative cut can never be skipped by a cursor that
    // silently advanced past it.
    dispatchRunnerEvent(runner.record.id, {
      ...(frame as SubscriptionSnapshotFrame),
      authoritative: plan.applyMessages,
    });
    for (const event of plan.replayEvents) {
      dispatchRunnerEvent(runner.record.id, event);
    }
    runner.sessionCursors.set(runtimeIdentityKey(identity), plan.nextCursor);
    if (pending && !pending.ready) {
      if (pending.timeout !== null) {
        this.clearTimeoutFn(pending.timeout);
        pending.timeout = null;
      }
      pending.ready = true;
      pending.resolve();
    }
    return true;
  }

  private trackSessionCursor(runner: RunnerRuntime, frame: unknown): void {
    const identity = this.frameIdentity(frame);
    if (!identity || !this.isCurrentIdentity(runner, identity)) return;
    if (!frame || typeof frame !== 'object') return;
    const key = runtimeIdentityKey(identity);
    const type = 'type' in frame ? frame.type : undefined;
    if (type === 'session_resync_required') {
      runner.sessionCursors.set(key, 0);
      return;
    }
    const sequence = 'sequence' in frame ? frame.sequence : undefined;
    if (typeof sequence !== 'number' || !Number.isFinite(sequence)) return;
    const current = runner.sessionCursors.get(key) ?? 0;
    if (sequence > current) runner.sessionCursors.set(key, sequence);
  }

  private currentIdentity(runner: RunnerRuntime, sessionId: string): RunnerRuntimeIdentity | null {
    const runtimeIncarnation = runner.currentIdentities.get(sessionId);
    if (!runtimeIncarnation) return null;
    return { sessionId, runtimeIncarnation };
  }

  private isCurrentIdentity(runner: RunnerRuntime, identity: RunnerRuntimeIdentity): boolean {
    return runner.currentIdentities.get(identity.sessionId) === identity.runtimeIncarnation;
  }

  private frameIdentity(frame: unknown): RunnerRuntimeIdentity | null {
    if (!frame || typeof frame !== 'object') return null;
    return readRuntimeIdentity(
      'sessionId' in frame ? frame.sessionId : null,
      'runtimeIncarnation' in frame ? frame.runtimeIncarnation : null,
    );
  }

  private stateSessionId(state: unknown): string | null {
    if (!state || typeof state !== 'object' || !('sessionId' in state)) return null;
    return readNonemptyString(state.sessionId);
  }

  private identityFromStateData(state: unknown): RunnerRuntimeIdentity | null {
    return identitiesFromRpcData('get_state', state)[0] ?? null;
  }

  private learnIdentities(runner: RunnerRuntime, command: string, data: unknown): void {
    for (const identity of identitiesFromRpcData(command, data)) {
      this.rememberIdentity(runner, identity);
    }
  }

  private rememberIdentity(runner: RunnerRuntime, identity: RunnerRuntimeIdentity): void {
    const previous = runner.currentIdentities.get(identity.sessionId);
    if (previous === identity.runtimeIncarnation) return;
    if (previous) {
      this.dropExactRuntimeOwnership(runner, { sessionId: identity.sessionId, runtimeIncarnation: previous });
    }
    runner.currentIdentities.set(identity.sessionId, identity.runtimeIncarnation);
  }

  private handleSessionUnload(runner: RunnerRuntime, frame: unknown): boolean {
    if (!frame || typeof frame !== 'object' || !('type' in frame) || frame.type !== 'session_unloaded') {
      return false;
    }
    const identity = this.frameIdentity(frame);
    if (!identity) return true;
    // Keep the unload observable before ownership cleanup: listeners must see
    // the exact frame even when a later incarnation already replaced it.
    dispatchRunnerEvent(runner.record.id, frame);
    if (!this.isCurrentIdentity(runner, identity)) return true;
    this.dropExactRuntimeOwnership(runner, identity);
    return true;
  }

  private dropExactRuntimeOwnership(runner: RunnerRuntime, identity: RunnerRuntimeIdentity): void {
    if (runner.currentIdentities.get(identity.sessionId) !== identity.runtimeIncarnation) return;
    runner.currentIdentities.delete(identity.sessionId);
    runner.sessionCursors.delete(runtimeIdentityKey(identity));
    for (const [key, pending] of runner.pendingSubscriptions) {
      if (pending.identity.sessionId !== identity.sessionId
        || pending.identity.runtimeIncarnation !== identity.runtimeIncarnation) continue;
      if (pending.timeout !== null) this.clearTimeoutFn(pending.timeout);
      runner.pendingSubscriptions.delete(key);
      if (!pending.ready) {
        pending.reject(new RunnerCommandError('Runner session identity is unavailable.', 'protocol'));
      }
    }
  }

  private resetConnectionOwnedState(runner: RunnerRuntime): void {
    runner.bootstrapSessionId = null;
    runner.probeCompleted = false;
    runner.currentIdentities.clear();
  }

  private failConnection(
    runner: RunnerRuntime,
    generation: number,
    error: unknown,
    hadToken: boolean,
  ): void {
    if (!this.isCurrentGeneration(runner, generation)) return;
    const raw = error instanceof Error ? error.message : String(error);
    const message = safeText(raw) || (hadToken ? 'Runner probe failed.' : 'Runner token is required.');
    this.rejectPending(runner, generation, message);
    const socket = runner.socket;
    runner.socket = null;
    if (socket) {
      detachTransportHandlers(socket);
      try {
        socket.close(4000, 'runner probe failed');
      } catch {
        runner.socket = null;
      }
    }
    this.updateView(runner, { state: 'reconnecting', error: message });
    this.rejectFirstProbe(runner, new Error(message), generation);
    this.scheduleReconnect(runner, generation);
  }

  private scheduleReconnect(runner: RunnerRuntime, generation: number): void {
    if (runner.stopped || runner.reconnectTimer !== null) return;
    const delay = runner.reconnectDelay;
    runner.reconnectDelay = nextReconnectDelay(delay);
    runner.reconnectTimer = this.setTimeoutFn(() => {
      runner.reconnectTimer = null;
      if (runner.stopped || !this.isCurrentGeneration(runner, generation)) return;
      this.connect(runner);
    }, delay);
  }

  private stopRuntime(runner: RunnerRuntime, reason: string): void {
    runner.stopped = true;
    this.abortPreflight(runner);
    this.cancelReconnect(runner);
    this.replaceSocket(runner, reason);
    this.rejectFirstProbe(runner, new Error(reason));
  }

  private replaceSocket(runner: RunnerRuntime, reason: string): void {
    const generation = runner.generation;
    this.rejectPending(runner, generation, reason);
    this.resetConnectionOwnedState(runner);
    const socket = runner.socket;
    runner.socket = null;
    if (!socket) return;
    detachTransportHandlers(socket);
    try {
      socket.close(1000, reason);
    } catch {
      runner.socket = null;
    }
  }

  private rejectPending(runner: RunnerRuntime, generation: number, reason: string): void {
    for (const [id, pending] of runner.pending) {
      if (pending.generation !== generation) continue;
      this.clearTimeoutFn(pending.timeout);
      runner.pending.delete(id);
      pending.reject(new RunnerCommandError(reason, 'transport'));
    }
    for (const [key, pending] of runner.pendingSubscriptions) {
      if (pending.generation !== generation) continue;
      if (pending.timeout !== null) this.clearTimeoutFn(pending.timeout);
      runner.pendingSubscriptions.delete(key);
      if (!pending.ready) pending.reject(new RunnerCommandError(reason, 'transport'));
    }
  }

  private cancelReconnect(runner: RunnerRuntime): void {
    if (runner.reconnectTimer === null) return;
    this.clearTimeoutFn(runner.reconnectTimer);
    runner.reconnectTimer = null;
  }

  private resolveFirstProbe(runner: RunnerRuntime, generation: number): void {
    const firstProbe = runner.firstProbe;
    if (!firstProbe || firstProbe.generation !== generation) return;
    runner.firstProbe = null;
    firstProbe.resolve({ ...runner.view });
  }

  private rejectFirstProbe(runner: RunnerRuntime, error: Error, generation?: number): void {
    const firstProbe = runner.firstProbe;
    if (!firstProbe || (generation !== undefined && firstProbe.generation > generation)) return;
    runner.firstProbe = null;
    firstProbe.reject(error);
  }

  private isCurrent(runner: RunnerRuntime, generation: number, socket: WebSocket): boolean {
    return this.isCurrentGeneration(runner, generation) && runner.socket === socket;
  }

  private isCurrentGeneration(runner: RunnerRuntime, generation: number): boolean {
    return !this.disposed
      && !runner.stopped
      && runner.generation === generation
      && this.runners.get(runner.record.id) === runner;
  }

  private collapseDuplicateRuntimes(): void {
    if (this.runners.size < 2) return;
    const keepIds = new Set(
      dedupeRunnersKeepHealthiest(
        Array.from(this.runners.values(), (runtime) => runtime.view),
      ).map((view) => view.id),
    );
    const dropped: RunnerRuntime[] = [];
    for (const runtime of this.runners.values()) {
      if (!keepIds.has(runtime.record.id)) dropped.push(runtime);
    }
    if (dropped.length === 0) return;
    const remaining = Array.from(this.runners.values(), (runtime) => runtime.record)
      .filter((record) => keepIds.has(record.id));
    if (!saveRunnerRecords(this.storage, remaining)) return;
    for (const runtime of dropped) {
      this.stopRuntime(runtime, 'duplicate runner identity');
      this.runners.delete(runtime.record.id);
    }
  }

  private updateView(
    runner: RunnerRuntime,
    update: Partial<Pick<RunnerView, 'connectionEpoch' | 'state' | 'error' | 'probe' | 'status'>>,
  ): void {
    // The view always mirrors the current record (codename adoption included):
    // the record is the source of truth, the previous view only carries
    // state/error/probe/status forward, and the update wins last.
    runner.view = { ...runner.view, ...runner.record, ...update };
    this.collapseDuplicateRuntimes();
    this.emit();
  }

  private persist(): void {
    if (!saveRunnerRecords(this.storage, Array.from(this.runners.values(), (runner) => runner.record))) {
      throw new Error('Runner registry storage is unavailable.');
    }
  }

  private emit(): void {
    const snapshot = this.snapshot();
    for (const listener of this.listeners) listener(snapshot);
  }
}
