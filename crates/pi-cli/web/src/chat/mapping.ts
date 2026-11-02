import type { StorageLike } from '../hostToken';
import type { RpcResponse } from '../types';
import type { RunnerCommand } from './protocol';
import { lifecycleTargetRow } from './protocol';
import { RunnerCommandError, type RunnerView } from './runners';

export const CHAT_MAPPING_STORAGE_KEY = 'rpi-web-chat-mapping:v1';
export const CHAT_MAPPING_LOCK = 'rpi-web-chat-mapping';
const CHAT_MAPPING_STORAGE_VERSION = 1;
const COMMAND_ID_CONFLICT = 'command id was already used for a different request';
const TARGET_NOT_FOUND = 'session was not found';
const BOUNDED_SWITCH_FAILURE = 'session could not be opened right now; verify the selection and retry';
// Exact allowlist of the backend's bounded new_session failure literals
// (session_runtime_manager.rs bounded_new_session_failure): one fixed,
// path-free category per operator-actionable directory failure. NO prefix
// passthrough — a remote error that is not one of these exact literals
// (including a novel "new session …" shape) never reaches the surface.
const NEW_SESSION_FAILURE_LITERALS = [
  'new session cwd must be an absolute directory path',
  'new session directory path is invalid',
  'new session path must not be a symbolic link',
  'new session path is not a directory',
  'new session directory access is denied',
  'new session directory does not exist',
  'new session directory could not be opened',
  'new session project requires trust approval',
] as const;
// Exact busy-capacity literal (bounded_switch_failure / the
// bounded_new_session_failure capacity branch, MAX_LOADED_SESSIONS = 128 in
// session_runtime_manager.rs). The limit number is compiled into the
// backend, so only the exact limit-128 shape is surfaced — any other limit
// value is a novel shape that fails closed to the generic rejection.
const BUSY_CAPACITY_FAILURE = 'too many busy sessions (limit 128); finish or stop active work and retry';
const DUPLICATE_PROJECT_CWD = 'A project already uses this directory.';

export interface ProjectRecord {
  id: string;
  label: string;
  cwd: string;
  createdAt: number;
  updatedAt: number;
}

interface MappingBase {
  id: string;
  projectId: string;
  runnerId: string;
  operationId: string;
  sourceSessionId: string;
  canaryId: string;
  createId: string;
  sessionName: string;
  createdAt: number;
  updatedAt: number;
}

type MappingIntent = MappingBase & { state: ProjectRunnerMapping['state'] };

export interface CreateOutcomeUnknownMapping extends MappingBase {
  state: 'create_outcome_unknown';
}

interface IdentifiedMapping extends MappingBase {
  sessionId: string;
  sessionPath: string;
}

export interface ProvisioningMapping extends IdentifiedMapping {
  state: 'provisioning';
}

export interface RollbackPendingMapping extends IdentifiedMapping {
  state: 'rollback_pending';
  archiveAttempt: number;
  reason: 'provisioning_failed' | 'activation_persistence_failed';
}

export interface ActiveMapping extends IdentifiedMapping {
  state: 'active';
}

export interface DeletingMapping extends IdentifiedMapping {
  state: 'deleting';
  archiveAttempt: number;
}

export interface ConflictMapping extends MappingBase {
  state: 'conflict';
  reason: 'create_outcome_unresolved' | 'identity_conflict';
  sessionId?: string;
  sessionPath?: string;
}

export type ProjectRunnerMapping =
  | CreateOutcomeUnknownMapping
  | ProvisioningMapping
  | RollbackPendingMapping
  | ActiveMapping
  | DeletingMapping
  | ConflictMapping;

interface StoredMappingDocument {
  version: number;
  projects: ProjectRecord[];
  mappings: ProjectRunnerMapping[];
}

export interface MappingSnapshot {
  projects: ProjectRecord[];
  mappings: ProjectRunnerMapping[];
  storageError: string;
}

export interface RunnerCommandClient {
  snapshot(): RunnerView[];
  command(
    runnerId: string,
    command: RunnerCommand,
    options?: { commandId?: string; sourceSessionId?: string; allowBeforeProbe?: boolean; beforeSend?: () => void },
  ): Promise<RpcResponse>;
}

export interface ChatMappingStoreOptions {
  storage: StorageLike | null;
  runners: RunnerCommandClient;
  withExclusiveLock?: <T>(callback: () => Promise<T>) => Promise<T>;
  now?: () => number;
  randomBytes?: (length: number) => Uint8Array;
}

export type ChatMappingListener = (snapshot: MappingSnapshot) => void;

type MappingDocumentLoad =
  | { kind: 'absent'; document: StoredMappingDocument }
  | { kind: 'valid'; document: StoredMappingDocument }
  | { kind: 'blocked'; error: string };

interface SessionRow {
  sessionId: string;
  path: string;
  cwd: string;
  name: string | null;
  status: 'native';
}

function emptyDocument(): StoredMappingDocument {
  return { version: CHAT_MAPPING_STORAGE_VERSION, projects: [], mappings: [] };
}

function isFiniteNumber(value: unknown): value is number {
  return typeof value === 'number' && Number.isFinite(value);
}

function isNonempty(value: unknown): value is string {
  return typeof value === 'string' && value.trim() !== '';
}

/** Platform-neutral absolute-directory check for the project cwd: the runner
 *  accepts whatever absolute path its own OS understands, so the form cannot
 *  hard-code POSIX. Accepts POSIX absolute (`/…`) and Windows drive-root
 *  (`C:\…` / `C:/…`) paths, and STRICT UNC paths only: exactly two leading
 *  separators, a nonempty server and share, and no doubled (empty) structural
 *  separator anywhere. Rejects drive-relative (`C:foo`, `C:`), share-less
 *  (`\\server`), triple-leading (`\\\server\share`), doubled-separator
 *  (`\\server\\share`, `\\server\share\\dir`), trailing-separator
 *  (`\\server\share\`), and every other malformed shape. */
export function isAbsoluteProjectCwd(value: string): boolean {
  if (value.startsWith('/')) return true;
  if (/^[A-Za-z]:[\\/]/.test(value)) return true;
  if (!value.startsWith('\\\\')) return false;
  // Strict UNC: exactly two leading separators, then every structural
  // segment — server, share, and any nested path — must be nonempty. An
  // empty segment is a doubled or trailing separator; fewer than two
  // segments is a share-less UNC.
  const segments = value.slice(2).split(/[\\/]/);
  return segments.length >= 2 && segments.every((segment) => segment !== '');
}

export function isProjectRecord(value: unknown): value is ProjectRecord {
  if (!value || typeof value !== 'object') return false;
  const project = value as Partial<ProjectRecord>;
  return isNonempty(project.id)
    && isNonempty(project.label)
    && isNonempty(project.cwd)
    && isFiniteNumber(project.createdAt)
    && isFiniteNumber(project.updatedAt);
}

function isMappingBase(value: unknown): value is MappingBase & { state: unknown } {
  if (!value || typeof value !== 'object') return false;
  const mapping = value as Partial<MappingBase> & { state?: unknown };
  return isNonempty(mapping.id)
    && isNonempty(mapping.projectId)
    && isNonempty(mapping.runnerId)
    && isNonempty(mapping.operationId)
    && isNonempty(mapping.sourceSessionId)
    && isNonempty(mapping.canaryId)
    && isNonempty(mapping.createId)
    && isNonempty(mapping.sessionName)
    && isFiniteNumber(mapping.createdAt)
    && isFiniteNumber(mapping.updatedAt);
}

function hasIdentity(value: MappingBase & Record<string, unknown>): boolean {
  return isNonempty(value.sessionId) && isNonempty(value.sessionPath);
}

export function isProjectRunnerMapping(value: unknown): value is ProjectRunnerMapping {
  if (!isMappingBase(value)) return false;
  const mapping = value as unknown as MappingBase & Record<string, unknown>;
  switch (mapping.state) {
    case 'create_outcome_unknown':
      return true;
    case 'provisioning':
    case 'active':
      return hasIdentity(mapping);
    case 'rollback_pending':
      return hasIdentity(mapping)
        && Number.isInteger(mapping.archiveAttempt)
        && (mapping.archiveAttempt as number) >= 0
        && (mapping.reason === 'provisioning_failed'
          || mapping.reason === 'activation_persistence_failed');
    case 'deleting':
      return hasIdentity(mapping)
        && Number.isInteger(mapping.archiveAttempt)
        && (mapping.archiveAttempt as number) >= 0;
    case 'conflict':
      return (mapping.reason === 'create_outcome_unresolved'
        || mapping.reason === 'identity_conflict')
        && (mapping.sessionId === undefined || isNonempty(mapping.sessionId))
        && (mapping.sessionPath === undefined || isNonempty(mapping.sessionPath));
    default:
      return false;
  }
}

/** Full-shape validation for an untrusted mapping document (a server-synced
 *  payload before adoption). Runs exactly the version / array / per-record /
 *  cross-reference checks loadMappingDocument applies to persisted storage,
 *  so callers can reject an invalid remote payload before it persists. */
export function validateStoredDocument(document: unknown): string | null {
  if (document === null || typeof document !== 'object' || Array.isArray(document)) {
    return 'Project storage is corrupt.';
  }
  const raw = document as Partial<StoredMappingDocument>;
  if (raw.version !== CHAT_MAPPING_STORAGE_VERSION) {
    return 'Project storage version is unsupported.';
  }
  if (!Array.isArray(raw.projects) || !raw.projects.every(isProjectRecord)) {
    return 'Project storage is corrupt.';
  }
  if (!Array.isArray(raw.mappings) || !raw.mappings.every(isProjectRunnerMapping)) {
    return 'Project storage is corrupt.';
  }
  const parsed: StoredMappingDocument = {
    version: CHAT_MAPPING_STORAGE_VERSION,
    projects: raw.projects,
    mappings: raw.mappings,
  };
  return validateDocument(parsed);
}

function validateDocument(document: StoredMappingDocument): string | null {
  const projectIds = new Set<string>();
  for (const project of document.projects) {
    if (projectIds.has(project.id)) return 'Project storage contains duplicate identities.';
    projectIds.add(project.id);
  }
  const mappingIds = new Set<string>();
  const mappingKeys = new Set<string>();
  for (const mapping of document.mappings) {
    const key = mappingKey(mapping.projectId, mapping.runnerId);
    if (mappingIds.has(mapping.id) || mappingKeys.has(key)) {
      return 'Project storage contains duplicate identities.';
    }
    if (!projectIds.has(mapping.projectId)) {
      return 'Project storage references an unknown project.';
    }
    mappingIds.add(mapping.id);
    mappingKeys.add(key);
  }
  return null;
}

export function loadMappingDocument(storage: StorageLike | null): MappingDocumentLoad {
  if (!storage) return { kind: 'blocked', error: 'Project storage is unavailable.' };
  let raw: string | null;
  try {
    raw = storage.getItem(CHAT_MAPPING_STORAGE_KEY);
  } catch {
    return { kind: 'blocked', error: 'Project storage is unavailable.' };
  }
  if (!raw) return { kind: 'absent', document: emptyDocument() };
  try {
    const parsed = JSON.parse(raw) as Partial<StoredMappingDocument>;
    if (parsed.version !== CHAT_MAPPING_STORAGE_VERSION) {
      return { kind: 'blocked', error: 'Project storage version is unsupported.' };
    }
    if (!Array.isArray(parsed.projects) || !parsed.projects.every(isProjectRecord)) {
      return { kind: 'blocked', error: 'Project storage is corrupt.' };
    }
    if (!Array.isArray(parsed.mappings) || !parsed.mappings.every(isProjectRunnerMapping)) {
      return { kind: 'blocked', error: 'Project storage is corrupt.' };
    }
    const document: StoredMappingDocument = {
      version: CHAT_MAPPING_STORAGE_VERSION,
      projects: parsed.projects,
      mappings: parsed.mappings,
    };
    const error = validateDocument(document);
    return error ? { kind: 'blocked', error } : { kind: 'valid', document };
  } catch {
    return { kind: 'blocked', error: 'Project storage is corrupt.' };
  }
}

function saveMappingDocument(storage: StorageLike | null, document: StoredMappingDocument): void {
  if (!storage) throw new Error('Project storage is unavailable.');
  try {
    storage.setItem(CHAT_MAPPING_STORAGE_KEY, JSON.stringify(document));
  } catch {
    throw new Error('Project storage is unavailable.');
  }
}

const CHAT_MAPPING_LOCK_DB = 'rpi-web-chat-locks:v1';
const CHAT_MAPPING_LOCK_WAIT_MS = 120_000;
const CHAT_MAPPING_LOCK_MAX_RACES = 8;
const CHAT_MAPPING_LOCK_UNAVAILABLE = 'Project mutation locking is unavailable.';
const CHAT_MAPPING_LOCK_BUSY = 'Project mutation locking is busy; retry.';

function isVersionRace(error: unknown): boolean {
  return error instanceof DOMException && error.name === 'VersionError';
}

function currentChatMappingLockVersion(factory: IDBFactory): Promise<number> {
  const { promise, resolve, reject } = Promise.withResolvers<number>();
  let request: IDBOpenDBRequest;
  let database: IDBDatabase | null = null;
  let settled = false;
  const timer = setTimeout(() => {
    if (settled) return;
    settled = true;
    database?.close();
    reject(new Error(CHAT_MAPPING_LOCK_BUSY));
  }, CHAT_MAPPING_LOCK_WAIT_MS);

  try {
    request = factory.open(CHAT_MAPPING_LOCK_DB);
  } catch {
    clearTimeout(timer);
    reject(new Error(CHAT_MAPPING_LOCK_UNAVAILABLE));
    return promise;
  }

  request.onupgradeneeded = () => {
    database = request.result;
    if (settled) database.close();
  };
  request.onerror = () => {
    if (settled) return;
    settled = true;
    clearTimeout(timer);
    reject(new Error(CHAT_MAPPING_LOCK_UNAVAILABLE));
  };
  request.onsuccess = () => {
    database = request.result;
    if (settled) {
      database.close();
      return;
    }
    settled = true;
    clearTimeout(timer);
    const version = database.version;
    database.close();
    if (!Number.isSafeInteger(version) || version >= Number.MAX_SAFE_INTEGER) {
      reject(new Error(CHAT_MAPPING_LOCK_UNAVAILABLE));
      return;
    }
    resolve(version);
  };
  return promise;
}

function upgradeChatMappingLock(
  factory: IDBFactory,
  version: number,
): Promise<IDBDatabase | null> {
  const { promise, resolve, reject } = Promise.withResolvers<IDBDatabase | null>();
  let request: IDBOpenDBRequest;
  let database: IDBDatabase | null = null;
  let upgraded = false;
  let settled = false;
  const timer = setTimeout(() => {
    if (settled) return;
    settled = true;
    database?.close();
    reject(new Error(CHAT_MAPPING_LOCK_BUSY));
  }, CHAT_MAPPING_LOCK_WAIT_MS);

  try {
    request = factory.open(CHAT_MAPPING_LOCK_DB, version);
  } catch (error) {
    clearTimeout(timer);
    if (isVersionRace(error)) resolve(null);
    else reject(new Error(CHAT_MAPPING_LOCK_UNAVAILABLE));
    return promise;
  }

  request.onupgradeneeded = () => {
    upgraded = true;
    database = request.result;
    if (settled) database.close();
  };
  request.onerror = () => {
    if (settled) return;
    settled = true;
    clearTimeout(timer);
    if (isVersionRace(request.error)) resolve(null);
    else reject(new Error(CHAT_MAPPING_LOCK_UNAVAILABLE));
  };
  request.onsuccess = () => {
    database = request.result;
    if (settled) {
      database.close();
      return;
    }
    settled = true;
    clearTimeout(timer);
    if (!upgraded) {
      database.close();
      resolve(null);
      return;
    }
    resolve(database);
  };
  return promise;
}

async function acquireChatMappingLock(factory: IDBFactory): Promise<IDBDatabase> {
  for (let attempt = 0; attempt < CHAT_MAPPING_LOCK_MAX_RACES; attempt += 1) {
    const currentVersion = await currentChatMappingLockVersion(factory);
    const database = await upgradeChatMappingLock(factory, currentVersion + 1);
    if (database) return database;
  }
  throw new Error(CHAT_MAPPING_LOCK_BUSY);
}

async function indexedDbExclusiveLock<T>(factory: IDBFactory, callback: () => Promise<T>): Promise<T> {
  const database = await acquireChatMappingLock(factory);
  try {
    // The open upgraded connection is the lock. A later version upgrade stays
    // blocked until this connection closes, including after a browser crash.
    return await callback();
  } finally {
    database.close();
  }
}

async function browserExclusiveLock<T>(callback: () => Promise<T>): Promise<T> {
  const locks = typeof navigator === 'object' ? navigator.locks : undefined;
  if (locks?.request) return locks.request(CHAT_MAPPING_LOCK, { mode: 'exclusive' }, callback);
  const factory = globalThis.indexedDB;
  if (!factory) throw new Error(CHAT_MAPPING_LOCK_UNAVAILABLE);
  return indexedDbExclusiveLock(factory, callback);
}

function createId(
  randomBytes: ((length: number) => Uint8Array) | undefined,
  prefix: string,
): string {
  const bytes = new Uint8Array(16);
  if (randomBytes) randomBytes(bytes.length).forEach((value, index) => { bytes[index] = value; });
  else globalThis.crypto.getRandomValues(bytes);
  const token = Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('');
  return `${prefix}-${token}`;
}

function mappingKey(projectId: string, runnerId: string): string {
  return `${projectId}::${runnerId}`;
}

function mappingName(operationId: string): string {
  return `chat-${operationId.slice(-24)}`;
}

function fixedError(error: unknown): string {
  if (error instanceof RunnerCommandError) {
    switch (error.kind) {
      case 'remote': {
        const remote = error.response?.error || error.subscription?.error || '';
        const command = error.response?.command ?? error.subscription?.command ?? '';
        // The runner's lifecycle failures are fixed, path-free literals:
        // one actionable reason per directory category, so surface the
        // exact bounded text. The allowlist is exact (never prefix-shaped)
        // and scoped to the two lifecycle commands that emit it — every
        // other remote failure collapses to the generic rejection.
        if (command === 'new_session' || command === 'switch_session') {
          if (remote === BOUNDED_SWITCH_FAILURE
            || remote === BUSY_CAPACITY_FAILURE
            || (command === 'new_session' && (NEW_SESSION_FAILURE_LITERALS as readonly string[]).includes(remote))) {
            return remote;
          }
        }
        return 'Runner rejected the project operation.';
      }
      case 'transport': return 'Runner connection interrupted the project operation.';
      case 'protocol': return 'Runner returned an invalid project operation response.';
    }
  }
  const message = error instanceof Error ? error.message : String(error);
  if (message.startsWith('Project ')
    || message.startsWith('Runner ')
    || message.startsWith('Mapped ')
    || message === DUPLICATE_PROJECT_CWD) return message;
  return 'Project operation failed.';
}

function responseData(response: RpcResponse): Record<string, unknown> {
  if (!response.data || typeof response.data !== 'object') {
    throw new Error('Runner response is missing project session state.');
  }
  return response.data as Record<string, unknown>;
}

/** Full lifecycle identity extraction through the ONE shared validator
 *  (protocol.ts lifecycleTargetRow, also applied by runners.ts at the
 *  settlement ownership gate): envelope/data/state/row must agree on the
 *  exact target identity and incarnation, the row must bind the exact
 *  requested directory, and the native status/loaded/source fields must be
 *  canonical. Any drift throws — a lifecycle success that does not bind the
 *  exact requested identity is never provisioned. When `expected` is
 *  supplied, the response must name the stored mapping identity exactly. */
function lifecycleIdentity(
  response: RpcResponse,
  command: 'new_session' | 'switch_session',
  cwd: string,
  expected?: { sessionId: string; path: string },
): SessionRow {
  const row = lifecycleTargetRow(response, command, cwd, expected);
  if (row === null) {
    throw new Error('Runner returned an invalid project session response.');
  }
  return {
    sessionId: row.sessionId,
    path: row.path,
    cwd: row.cwd,
    name: null,
    status: 'native',
  };
}

function sessionRows(response: RpcResponse, sourceSessionId: string): SessionRow[] {
  if (!response.success || response.command !== 'session_list' || response.sessionId !== sourceSessionId) {
    throw new Error('Runner returned an invalid project session catalog.');
  }
  const rows = responseData(response).sessions;
  if (!Array.isArray(rows)) throw new Error('Runner project session catalog is unavailable.');
  // Catalog row reconciliation binds the EXACT canonical production row
  // (session_runtime_manager.rs session_list_row_value + overlay_loaded_sessions):
  // native rpi source, native status, loaded true, nonempty runtimeIncarnation,
  // and nonempty id/path/cwd. Any row missing one stamp is not a reconcilable
  // live native runtime and is dropped — a malformed, foreign-source, or
  // unloaded row can never be treated as the exact mapped session.
  return rows.flatMap((value): SessionRow[] => {
    if (!value || typeof value !== 'object') return [];
    const row = value as Record<string, unknown>;
    if (row.source !== 'rpi'
      || row.status !== 'native'
      || row.loaded !== true
      || !isNonempty(row.runtimeIncarnation)
      || !isNonempty(row.sessionId)
      || !isNonempty(row.path)
      || !isNonempty(row.cwd)) return [];
    return [{
      sessionId: row.sessionId,
      path: row.path,
      cwd: row.cwd,
      name: typeof row.name === 'string' ? row.name : null,
      status: 'native',
    }];
  });
}

function exactIdentityState(
  rows: SessionRow[],
  mapping: IdentifiedMapping,
  projectCwd: string,
): 'present' | 'absent' | 'conflict' {
  const byId = rows.filter((row) => row.sessionId === mapping.sessionId);
  const byPath = rows.filter((row) => row.path === mapping.sessionPath);
  if (byId.length === 0 && byPath.length === 0) return 'absent';
  if (byId.length !== 1 || byPath.length !== 1 || byId[0] !== byPath[0]) return 'conflict';
  return byId[0].cwd === projectCwd ? 'present' : 'conflict';
}

function commandConflict(error: unknown, canaryId: string, sourceSessionId: string): boolean {
  if (!(error instanceof RunnerCommandError) || error.kind !== 'remote') return false;
  const response = error.response;
  return response?.id === canaryId
    && response.command === 'get_messages'
    && response.sessionId === sourceSessionId
    && response.success === false
    && response.error === COMMAND_ID_CONFLICT;
}

function ownedRemoteFailure(
  error: unknown,
  command: string,
  sourceSessionId: string,
): RpcResponse | null {
  if (!(error instanceof RunnerCommandError) || error.kind !== 'remote') return null;
  const response = error.response;
  if (!response
    || response.command !== command
    || response.sessionId !== sourceSessionId
    || response.success !== false) return null;
  return response;
}

function ownedSubscriptionFailure(error: unknown, sourceSessionId: string): string | null {
  if (!(error instanceof RunnerCommandError) || error.kind !== 'remote') return null;
  const subscription = error.subscription;
  if (!subscription
    || subscription.type !== 'subscription_error'
    || subscription.command !== 'subscribe'
    || subscription.sessionId !== sourceSessionId) return null;
  return subscription.error;
}

function isSourceUnknown(
  error: unknown,
  command: 'session_list' | 'switch_session' | 'set_session_name' | 'session_delete' | 'get_messages',
  sourceSessionId: string,
): boolean {
  const expected = `unknown session ${sourceSessionId}`;
  const owned = ownedRemoteFailure(error, command, sourceSessionId);
  if (owned) return owned.error === expected;
  return ownedSubscriptionFailure(error, sourceSessionId) === expected;
}

function isTargetNotFound(error: unknown, sourceSessionId: string): boolean {
  const message = ownedRemoteFailure(error, 'switch_session', sourceSessionId)?.error;
  return message === TARGET_NOT_FOUND || message === BOUNDED_SWITCH_FAILURE;
}

function isIdentityUnavailable(error: unknown): boolean {
  return error instanceof RunnerCommandError
    && error.kind === 'protocol'
    && error.message === 'Runner session identity is unavailable.';
}

const ARCHIVE_ACTIVE = 'cannot archive the active session';
const ARCHIVE_PRIMARY = 'cannot archive the primary session';

function isArchiveNeedsRetreat(error: unknown, owner: string): boolean {
  const remote = ownedRemoteFailure(error, 'session_delete', owner)?.error;
  return remote === ARCHIVE_ACTIVE || remote === ARCHIVE_PRIMARY;
}

function archiveFailure(error: unknown, mapping: IdentifiedMapping): string {
  const remote = error instanceof RunnerCommandError
    ? error.response?.error || error.subscription?.error || error.message
    : error instanceof Error ? error.message : String(error);
  if (remote.startsWith('unknown session ')) {
    return 'Runner session is no longer available for archive.';
  }
  if (remote === ARCHIVE_ACTIVE || remote === ARCHIVE_PRIMARY) {
    return 'Mapped session is currently active on the runner.';
  }
  if (remote.startsWith('session is busy')) {
    return 'Mapped session is busy and could not be archived.';
  }
  if (error instanceof Error && (error.message.startsWith('Runner ')
    || error.message.startsWith('Mapped ')
    || error.message.startsWith('Project '))) {
    return error.message;
  }
  if (error instanceof RunnerCommandError) return fixedError(error);
  return `Runner could not archive mapped session ${mapping.sessionName}.`;
}

function isIdentified(mapping: ProjectRunnerMapping): mapping is IdentifiedMapping & ProjectRunnerMapping {
  if (mapping.state === 'provisioning'
    || mapping.state === 'rollback_pending'
    || mapping.state === 'active'
    || mapping.state === 'deleting') return true;
  return mapping.state === 'conflict'
    && isNonempty(mapping.sessionId)
    && isNonempty(mapping.sessionPath);
}

export class ChatMappingStore {
  private readonly storage: StorageLike | null;
  private readonly runners: RunnerCommandClient;
  private readonly withExclusiveLock: <T>(callback: () => Promise<T>) => Promise<T>;
  private readonly now: () => number;
  private readonly randomBytes?: (length: number) => Uint8Array;
  private readonly listeners = new Set<ChatMappingListener>();
  private mutationTail: Promise<void> = Promise.resolve();
  private lockedDocument: StoredMappingDocument | null = null;
  private deferredPersistenceError = '';
  private snapshotValue: MappingSnapshot;

  constructor(options: ChatMappingStoreOptions) {
    this.storage = options.storage;
    this.runners = options.runners;
    this.withExclusiveLock = options.withExclusiveLock ?? browserExclusiveLock;
    this.now = options.now ?? Date.now;
    this.randomBytes = options.randomBytes;
    this.snapshotValue = this.readSnapshot();
  }

  /** Full persisted document for server-side sync pushes. While a mutation
   *  holds the exclusive lock the in-progress document is authoritative —
   *  the persisted copy may not reflect the mutation yet. */
  exportDocument(): StoredMappingDocument {
    if (this.lockedDocument) return this.lockedDocument;
    const loaded = loadMappingDocument(this.storage);
    return loaded.kind === 'valid' ? loaded.document : emptyDocument();
  }

  /** Adopt a server-synced document wholesale (boot pairing). Serialized
   *  through the same mutation queue as mutate() so adoption cannot race a
   *  concurrent project mutation inside the exclusive lock — mutate
   *  re-loads the persisted document there, applies the adopted document,
   *  and persists it. Throws on an invalid document instead of deferring. */
  async adoptDocument(document: StoredMappingDocument): Promise<void> {
    const error = validateDocument(document);
    if (error) throw new Error(error);
    await this.mutate(async () => ({ document, value: undefined }));
  }

  subscribe(listener: ChatMappingListener): () => void {
    this.listeners.add(listener);
    listener(this.snapshot());
    return () => { this.listeners.delete(listener); };
  }

  snapshot(): MappingSnapshot {
    return {
      projects: this.snapshotValue.projects.map((project) => ({ ...project })),
      mappings: this.snapshotValue.mappings.map((mapping) => ({ ...mapping })),
      storageError: this.snapshotValue.storageError,
    };
  }

  private liveCommandOwner(runnerId: string, stored: string): string {
    const runner = this.runners.snapshot().find((candidate) => candidate.id === runnerId);
    const live = runner?.probe?.sessionId?.trim() || '';
    return live || stored;
  }

  private concludeRequiredArchive(
    document: StoredMappingDocument,
    mapping: RollbackPendingMapping | DeletingMapping,
  ): StoredMappingDocument {
    const runnerPresent = this.runners.snapshot().some((candidate) => candidate.id === mapping.runnerId);
    if (!runnerPresent) {
      return this.settleArchivedMapping(document, mapping.id, false);
    }
    const pending = {
      ...mapping,
      archiveAttempt: mapping.archiveAttempt + 1,
      updatedAt: this.now(),
    };
    const current = this.replaceMapping(document, pending);
    this.persistDocument(current);
    return current;
  }

  private archiveRetreatRow(
    rows: SessionRow[],
    mapping: IdentifiedMapping,
    preferredId: string,
  ): SessionRow | null {
    const preferred = rows.find((row) => row.sessionId === preferredId && row.sessionId !== mapping.sessionId);
    if (preferred) return preferred;
    return rows.find((row) => row.sessionId !== mapping.sessionId) ?? null;
  }

  private async archiveSwitchAway(
    runnerId: string,
    owner: string,
    retreat: SessionRow,
    commandId: string,
  ): Promise<string> {
    const switched = await this.runners.command(
      runnerId,
      { type: 'switch_session', sessionPath: retreat.path },
      { commandId, sourceSessionId: owner, allowBeforeProbe: true },
    );
    return lifecycleIdentity(switched, 'switch_session', retreat.cwd, {
      sessionId: retreat.sessionId,
      path: retreat.path,
    }).sessionId;
  }

  private async archiveDeleteMapped(
    mapping: IdentifiedMapping,
    rows: SessionRow[],
    owner: string,
    attempt: number,
  ): Promise<RpcResponse> {
    let commandOwner = owner;
    if (commandOwner === mapping.sessionId) {
      const retreat = this.archiveRetreatRow(rows, mapping, mapping.sourceSessionId);
      if (!retreat) throw new Error('Mapped session is currently active on the runner.');
      commandOwner = await this.archiveSwitchAway(
        mapping.runnerId,
        commandOwner,
        retreat,
        `${mapping.operationId}:archive-retreat:${attempt}`,
      );
    }
    try {
      return await this.runners.command(
        mapping.runnerId,
        { type: 'session_delete', sessionPath: mapping.sessionPath },
        {
          commandId: `${mapping.operationId}:archive:${attempt}`,
          sourceSessionId: commandOwner,
          allowBeforeProbe: true,
        },
      );
    } catch (error) {
      if (!isArchiveNeedsRetreat(error, commandOwner)) throw error;
      const retreat = this.archiveRetreatRow(rows, mapping, mapping.sourceSessionId);
      if (!retreat) throw error;
      commandOwner = await this.archiveSwitchAway(
        mapping.runnerId,
        commandOwner,
        retreat,
        `${mapping.operationId}:archive-retreat:${attempt}`,
      );
      return this.runners.command(
        mapping.runnerId,
        { type: 'session_delete', sessionPath: mapping.sessionPath },
        {
          commandId: `${mapping.operationId}:archive-retry:${attempt}`,
          sourceSessionId: commandOwner,
          allowBeforeProbe: true,
        },
      );
    }
  }

  async addProject(label: string, cwd: string): Promise<ProjectRecord> {
    return this.mutate(async (document) => {
      const normalizedCwd = cwd.trim();
      if (!isAbsoluteProjectCwd(normalizedCwd)) throw new Error('Project directory must be absolute.');
      const normalizedLabel = label.trim();
      if (!normalizedLabel) throw new Error('Project label is required.');
      if (document.projects.some((project) => project.cwd === normalizedCwd)) {
        throw new Error(DUPLICATE_PROJECT_CWD);
      }
      const now = this.now();
      let projectId = createId(this.randomBytes, 'project');
      while (document.projects.some((project) => project.id === projectId)) {
        projectId = createId(this.randomBytes, 'project');
      }
      const project: ProjectRecord = {
        id: projectId,
        label: normalizedLabel,
        cwd: normalizedCwd,
        createdAt: now,
        updatedAt: now,
      };
      return { document: { ...document, projects: [...document.projects, project] }, value: project };
    });
  }

  async mapProject(projectId: string, runnerId: string): Promise<ProjectRunnerMapping> {
    return this.mutate(async (document) => {
      const project = document.projects.find((candidate) => candidate.id === projectId);
      if (!project) throw new Error('Project was not found.');
      const runner = this.runners.snapshot().find((candidate) => candidate.id === runnerId);
      if (!runner || runner.state !== 'connected' || !runner.probe?.sessionId) {
        throw new Error('Runner is not connected.');
      }
      const key = mappingKey(projectId, runnerId);
      const existing = document.mappings.find((mapping) => mappingKey(mapping.projectId, mapping.runnerId) === key);
      if (existing) return { document, value: existing };
      let operationId = createId(this.randomBytes, 'mapping');
      while (document.mappings.some((mapping) => mapping.operationId === operationId)) {
        operationId = createId(this.randomBytes, 'mapping');
      }
      const canaryId = `${operationId}:canary`;
      const createCommandId = `${operationId}:create`;
      const probeSessionId = runner.probe.sessionId;
      const canary = await this.runners.command(
        runnerId,
        { type: 'get_state' },
        { commandId: canaryId, sourceSessionId: probeSessionId },
      );
      const canaryState = responseData(canary);
      const sourceSessionId = isNonempty(canaryState.sessionId) ? canaryState.sessionId : '';
      if (canary.command !== 'get_state'
        || canary.sessionId !== probeSessionId
        || sourceSessionId !== probeSessionId) {
        throw new Error('Runner primary session changed before project creation.');
      }
      const now = this.now();
      const mapping: CreateOutcomeUnknownMapping = {
        id: key,
        projectId,
        runnerId,
        operationId,
        sourceSessionId,
        canaryId,
        createId: createCommandId,
        sessionName: mappingName(operationId),
        state: 'create_outcome_unknown',
        createdAt: now,
        updatedAt: now,
      };
      const intent = { ...document, mappings: [...document.mappings, mapping] };
      this.persistDocument(intent);
      return this.createMappedSession(intent, project, mapping);
    });
  }

  async recoverRunner(runnerId: string): Promise<void> {
    await this.mutate(async (document) => {
      let current = document;
      for (const mappingId of document.mappings
        .filter((candidate) => candidate.runnerId === runnerId)
        .map((mapping) => mapping.id)) {
        const mapping = current.mappings.find((candidate) => candidate.id === mappingId);
        if (!mapping) continue;
        const project = current.projects.find((candidate) => candidate.id === mapping.projectId);
        if (!project) continue;
        current = await this.recoverMapping(current, project, mapping);
      }
      return { document: current, value: undefined };
    });
  }

  async removeRunner(runnerId: string, deleteRunner: () => void): Promise<string[]> {
    return this.mutate(async (document) => {
      let current = document;
      const skippedArchives: string[] = [];
      const mappingIds = current.mappings
        .filter((candidate) => candidate.runnerId === runnerId)
        .map((mapping) => mapping.id);
      for (const mappingId of mappingIds) {
        const mapping = current.mappings.find((candidate) => candidate.id === mappingId);
        if (!mapping || !isIdentified(mapping)) continue;
        const project = current.projects.find((candidate) => candidate.id === mapping.projectId);
        if (!project) throw new Error('Runner project was not found.');
        const deleting: DeletingMapping = {
          ...mapping,
          state: 'deleting',
          archiveAttempt: mapping.state === 'deleting' ? mapping.archiveAttempt : 0,
          updatedAt: this.now(),
        };
        current = this.replaceMapping(current, deleting);
        this.persistDocument(current);
        // User asked for removal: a failed archive (generic transport,
        // active-session archive, ...) is never terminal. Drop the tombstone
        // from the persisted document and still run deleteRunner() below.
        try {
          current = await this.archiveMapping(current, project, deleting, true, true);
          const leftover = current.mappings.find((candidate) => candidate.id === mappingId);
          if (leftover?.state === 'conflict') {
            skippedArchives.push(mapping.sessionName);
            current = this.removeMapping(current, mappingId);
            this.persistDocument(current);
          }
        } catch {
          skippedArchives.push(mapping.sessionName);
          current = this.removeMapping(current, mappingId);
          this.persistDocument(current);
        }
      }
      try {
        deleteRunner();
      } catch (error) {
        for (const mappingId of mappingIds) {
          const leftover = current.mappings.find((candidate) => candidate.id === mappingId);
          if (leftover?.state === 'deleting') {
            current = this.removeMapping(current, mappingId);
          }
        }
        this.persistDocument(current);
        throw error instanceof Error ? error : new Error(String(error));
      }
      for (const mappingId of mappingIds) {
        current = this.removeMapping(current, mappingId);
      }
      this.persistDocument(current);
      return { document: current, value: skippedArchives };
    });
  }

  async unmapProjectRunner(projectId: string, runnerId: string): Promise<void> {
    await this.mutate(async (document) => {
      const key = mappingKey(projectId, runnerId);
      const mapping = document.mappings.find((candidate) => candidate.id === key);
      if (!mapping) return { document, value: undefined };
      let current = document;
      if (!isIdentified(mapping)) {
        current = this.removeMapping(current, mapping.id);
        this.persistDocument(current);
        return { document: current, value: undefined };
      }
      const project = current.projects.find((candidate) => candidate.id === projectId);
      if (!project) throw new Error('Runner project was not found.');
      const deleting: DeletingMapping = {
        ...mapping,
        state: 'deleting',
        archiveAttempt: mapping.state === 'deleting' ? mapping.archiveAttempt : 0,
        updatedAt: this.now(),
      };
      current = this.replaceMapping(current, deleting);
      this.persistDocument(current);
      // User asked to unmap: a failed archive is never terminal. Drop the
      // mapping, keep the project, and do not delete the runner.
      try {
        current = await this.archiveMapping(current, project, deleting, true, true);
      } catch {
        current = this.removeMapping(current, mapping.id);
        this.persistDocument(current);
        return { document: current, value: undefined };
      }
      if (current.mappings.some((candidate) => candidate.id === mapping.id)) {
        current = this.removeMapping(current, mapping.id);
        this.persistDocument(current);
      }
      return { document: current, value: undefined };
    });
  }

  async deleteProject(projectId: string): Promise<string[]> {
    return this.mutate(async (document) => {
      const project = document.projects.find((candidate) => candidate.id === projectId);
      if (!project) return { document, value: [] };
      let current = document;
      const skippedArchives: string[] = [];
      for (const mappingId of current.mappings
        .filter((candidate) => candidate.projectId === projectId)
        .map((mapping) => mapping.id)) {
        const mapping = current.mappings.find((candidate) => candidate.id === mappingId);
        if (!mapping) continue;
        if (!isIdentified(mapping)) {
          skippedArchives.push(mapping.sessionName);
          current = this.removeMapping(current, mapping.id);
          this.persistDocument(current);
          continue;
        }
        const deleting: DeletingMapping = {
          ...mapping,
          state: 'deleting',
          archiveAttempt: mapping.state === 'deleting' ? mapping.archiveAttempt : 0,
          updatedAt: this.now(),
        };
        current = this.replaceMapping(current, deleting);
        // User asked for deletion: a failed archive is never terminal. The
        // leftover check below drops the tombstone so the project itself is
        // still removed.
        try {
          current = await this.archiveMapping(current, project, deleting, true);
          if (current.mappings.some((candidate) => candidate.id === mappingId)) {
            skippedArchives.push(mapping.sessionName);
          }
        } catch {
          skippedArchives.push(mapping.sessionName);
        }
        if (current.mappings.some((candidate) => candidate.id === mappingId)) {
          current = this.removeMapping(current, mappingId);
          this.persistDocument(current);
        }
      }
      return {
        document: {
          ...current,
          projects: current.projects.filter((candidate) => candidate.id !== projectId),
        },
        value: skippedArchives,
      };
    });
  }

  private async createMappedSession(
    document: StoredMappingDocument,
    project: ProjectRecord,
    mapping: CreateOutcomeUnknownMapping,
  ): Promise<{ document: StoredMappingDocument; value: ProjectRunnerMapping }> {
    try {
      const response = await this.runners.command(
        mapping.runnerId,
        { type: 'new_session', cwd: project.cwd },
        { commandId: mapping.createId, sourceSessionId: mapping.sourceSessionId },
      );
      const identity = lifecycleIdentity(response, 'new_session', project.cwd);
      return this.provisionIdentity(document, project, mapping, identity);
    } catch (error) {
      if (error instanceof RunnerCommandError && error.kind === 'remote') {
        const current = this.removeMapping(document, mapping.id);
        this.persistDocument(current);
        // Rethrow raw: mutate's boundary fixedError is the single
        // sanitization pass, so the bounded new_session category literal
        // reaches the component exactly once and verbatim.
        throw error;
      }
      if (error instanceof RunnerCommandError && error.kind === 'protocol') {
        this.markConflict(document, mapping, 'create_outcome_unresolved');
        throw error;
      }
      throw new Error('Project session outcome is being reconciled after reconnect.');
    }
  }

  private async provisionIdentity(
    document: StoredMappingDocument,
    project: ProjectRecord,
    mapping: MappingIntent,
    identity: SessionRow,
  ): Promise<{ document: StoredMappingDocument; value: ProjectRunnerMapping }> {
    let current: StoredMappingDocument;
    const provisioning: ProvisioningMapping = {
      ...mapping,
      sessionId: identity.sessionId,
      sessionPath: identity.path,
      state: 'provisioning',
      updatedAt: this.now(),
    };
    current = this.replaceMapping(document, provisioning);
    this.persistDocument(current);
    try {
      await this.runners.command(
        mapping.runnerId,
        { type: 'set_session_name', name: mapping.sessionName, sessionPath: identity.path },
        { commandId: `${mapping.operationId}:name`, sourceSessionId: mapping.sourceSessionId },
      );
    } catch (error) {
      if (error instanceof RunnerCommandError && error.kind === 'transport') {
        throw new Error('Project session naming is being reconciled after reconnect.');
      }
      const rollback: RollbackPendingMapping = {
        ...provisioning,
        state: 'rollback_pending',
        archiveAttempt: 0,
        reason: 'provisioning_failed',
        updatedAt: this.now(),
      };
      current = this.replaceMapping(current, rollback);
      this.persistDocument(current);
      // Rethrow raw: mutate's boundary fixedError is the single
      // sanitization pass, so the bounded reason reaches the surface
      // exactly once instead of being double-converted here.
      throw error;
    }
    const active: ActiveMapping = { ...provisioning, state: 'active', updatedAt: this.now() };
    try {
      current = this.replaceMapping(current, active);
      this.persistDocument(current);
      return { document: current, value: active };
    } catch {
      const rollback: RollbackPendingMapping = {
        ...provisioning,
        state: 'rollback_pending',
        archiveAttempt: 0,
        reason: 'activation_persistence_failed',
        updatedAt: this.now(),
      };
      this.deferredPersistenceError = 'Project storage is unavailable.';
      try {
        await this.archiveMapping(this.replaceMapping(current, rollback), project, rollback);
      } catch {
        this.deferredPersistenceError = 'Project storage is unavailable.';
      }
      throw new Error('Project storage is unavailable.');
    }
  }

  private async recoverMapping(
    document: StoredMappingDocument,
    project: ProjectRecord,
    mapping: ProjectRunnerMapping,
  ): Promise<StoredMappingDocument> {
    switch (mapping.state) {
      case 'create_outcome_unknown': {
        try {
          await this.runners.command(
            mapping.runnerId,
            { type: 'get_messages' },
            {
              commandId: mapping.canaryId,
              sourceSessionId: mapping.sourceSessionId,
              allowBeforeProbe: true,
            },
          );
          // Canary success: the runner's idempotency ledger does NOT hold
          // the original canary (a held canary replays as the exact
          // command-id conflict), so a create replay could duplicate a
          // session the lost create already opened, and no exact identity
          // is resolvable (naming never ran). No safe replay exists:
          // converge durably on the explicit create_outcome_unresolved
          // conflict instead of leaving a forever-unknown intent. Later
          // recovery passes hit the stable conflict branch: no commands,
          // no mutation.
          return this.markConflict(document, mapping, 'create_outcome_unresolved');
        } catch (error) {
          if (commandConflict(error, mapping.canaryId, mapping.sourceSessionId)) {
            // The runner holds the original sequence: fall through to the
            // stable create replay.
          } else if (isSourceUnknown(error, 'get_messages', mapping.sourceSessionId)) {
            // The source session is gone, so the source-owned create can
            // never be replayed. Converge on the explicit unresolved
            // conflict instead of returning a stuck mapping forever.
            return this.markConflict(document, mapping, 'create_outcome_unresolved');
          } else {
            return document; // transient canary failure: retryable next pass
          }
        }
        let replay: RpcResponse;
        try {
          replay = await this.runners.command(
            mapping.runnerId,
            { type: 'new_session', cwd: project.cwd },
            {
              commandId: mapping.createId,
              sourceSessionId: mapping.sourceSessionId,
              allowBeforeProbe: true,
            },
          );
        } catch (error) {
          if (error instanceof RunnerCommandError && error.kind === 'remote'
            && ownedRemoteFailure(error, 'new_session', mapping.sourceSessionId) !== null) {
            // ANY owned remote replay failure is DEFINITIVE for this intent:
            // the stable create id plus the source-owned failure envelope
            // prove the runner answered THIS create — whether the answer is
            // the ledger's replay of the original outcome (busy capacity,
            // bounded open, a definitive directory category) or the ledger's
            // own verdict (command result expired / command id reused). The
            // exact outcome stays unknowable, so the intent converges
            // durably on the explicit create_outcome_unresolved conflict —
            // never a forever retry, never a silent remove, never a duplicate
            // create. Later recovery passes hit the stable conflict branch:
            // no commands, no mutation.
            return this.markConflict(document, mapping, 'create_outcome_unresolved');
          }
          if (error instanceof RunnerCommandError && error.kind === 'protocol') {
            return this.markConflict(document, mapping, 'create_outcome_unresolved');
          }
          return document; // ambiguous transport: retryable next pass
        }
        let identity: SessionRow;
        try {
          identity = lifecycleIdentity(replay, 'new_session', project.cwd);
        } catch {
          // A stable replay response that never binds the exact requested
          // identity is an explicit conflict, never a permanent unknown.
          return this.markConflict(document, mapping, 'create_outcome_unresolved');
        }
        try {
          return (await this.provisionIdentity(document, project, mapping, identity)).document;
        } catch {
          // provisionIdentity already persisted its next durable state
          // before rethrowing (provisioning on naming transport ambiguity,
          // rollback_pending on a failed compensation whose archive already
          // ran). Keep the latest durable document so the mutation boundary
          // does not roll the intent back to create_outcome_unknown; the
          // matching recovery branch converges it on the next pass.
          const reloaded = loadMappingDocument(this.storage);
          return reloaded.kind === 'blocked' ? document : reloaded.document;
        }
      }
      case 'provisioning':
        return this.recoverProvisioning(document, project, mapping);
      case 'rollback_pending':
        return this.archiveMapping(document, project, mapping);
      case 'deleting':
        return this.archiveMapping(document, project, mapping);
      case 'active':
        return this.reconcileActiveMapping(document, project, mapping);
      case 'conflict':
        return document;
    }
  }
  private async rebindMappedSession(
    mapping: IdentifiedMapping,
    project: ProjectRecord,
  ): Promise<'ok' | 'conflict' | 'retry'> {
    const owner = this.liveCommandOwner(mapping.runnerId, mapping.sourceSessionId);
    try {
      const switched = await this.runners.command(
        mapping.runnerId,
        { type: 'switch_session', sessionPath: mapping.sessionPath },
        { sourceSessionId: owner, allowBeforeProbe: true },
      );
      try {
        // The shared validator enforces the full canonical row; the stored
        // mapping identity is the expectation, so a switch that resolved a
        // different live row fails closed as a conflict.
        lifecycleIdentity(switched, 'switch_session', project.cwd, {
          sessionId: mapping.sessionId,
          path: mapping.sessionPath,
        });
        return 'ok';
      } catch {
        return 'conflict';
      }
    } catch (error) {
      if (isIdentityUnavailable(error)) throw error;
      if (isSourceUnknown(error, 'switch_session', owner)
        || isTargetNotFound(error, owner)) {
        return 'conflict';
      }
      return 'retry';
    }
  }

  private async reconcileActiveMapping(
    document: StoredMappingDocument,
    project: ProjectRecord,
    mapping: ActiveMapping,
  ): Promise<StoredMappingDocument> {
    const owner = this.liveCommandOwner(mapping.runnerId, mapping.sourceSessionId);
    try {
      const catalog = await this.runners.command(
        mapping.runnerId,
        { type: 'session_list', scope: 'all_projects' },
        { sourceSessionId: owner, allowBeforeProbe: true },
      );
      const identity = exactIdentityState(
        sessionRows(catalog, owner),
        mapping,
        project.cwd,
      );
      if (identity === 'conflict') return this.markConflict(document, mapping, 'identity_conflict');
    } catch (error) {
      if (isIdentityUnavailable(error)) throw error;
      if (isSourceUnknown(error, 'session_list', owner)) {
        return this.markConflict(document, mapping, 'identity_conflict');
      }
      return document;
    }
    const switched = await this.rebindMappedSession(mapping, project);
    if (switched === 'conflict') return this.markConflict(document, mapping, 'identity_conflict');
    return document;
  }

  private async recoverProvisioning(
    document: StoredMappingDocument,
    project: ProjectRecord,
    mapping: ProvisioningMapping,
  ): Promise<StoredMappingDocument> {
    const owner = this.liveCommandOwner(mapping.runnerId, mapping.sourceSessionId);
    let rows: SessionRow[] = [];
    try {
      const response = await this.runners.command(
        mapping.runnerId,
        { type: 'session_list', scope: 'all_projects' },
        { sourceSessionId: owner, allowBeforeProbe: true },
      );
      rows = sessionRows(response, owner);
    } catch (error) {
      if (isIdentityUnavailable(error)) throw error;
      if (isSourceUnknown(error, 'session_list', owner)) {
        return this.markConflict(document, mapping, 'identity_conflict');
      }
      return document;
    }
    const identity = exactIdentityState(rows, mapping, project.cwd);
    if (identity === 'conflict') return this.markConflict(document, mapping, 'identity_conflict');
    if (identity === 'absent') {
      const switched = await this.rebindMappedSession(mapping, project);
      if (switched === 'conflict') return this.markConflict(document, mapping, 'identity_conflict');
      if (switched === 'retry') return document;
    }
    const row = rows.find((candidate) => candidate.sessionId === mapping.sessionId
      && candidate.path === mapping.sessionPath);
    if (identity === 'absent' || row?.name !== mapping.sessionName) {
      try {
        await this.runners.command(
          mapping.runnerId,
          { type: 'set_session_name', name: mapping.sessionName, sessionPath: mapping.sessionPath },
          { commandId: `${mapping.operationId}:name`, sourceSessionId: owner, allowBeforeProbe: true },
        );
      } catch (error) {
        if (isIdentityUnavailable(error)) throw error;
        if (isSourceUnknown(error, 'set_session_name', owner)) {
          return this.markConflict(document, mapping, 'identity_conflict');
        }
        if (!(error instanceof RunnerCommandError)
          || error.kind !== 'remote'
          || ownedRemoteFailure(error, 'set_session_name', owner) === null) {
          return document;
        }
        const rollback: RollbackPendingMapping = {
          ...mapping,
          state: 'rollback_pending',
          archiveAttempt: 0,
          reason: 'provisioning_failed',
          updatedAt: this.now(),
        };
        const current = this.replaceMapping(document, rollback);
        this.persistDocument(current);
        return this.archiveMapping(current, project, rollback);
      }
    }
    const active: ActiveMapping = { ...mapping, state: 'active', updatedAt: this.now() };
    const current = this.replaceMapping(document, active);
    this.persistDocument(current);
    return current;
  }

  private settleArchivedMapping(
    current: StoredMappingDocument,
    mappingId: string,
    retainTombstone: boolean,
  ): StoredMappingDocument {
    if (retainTombstone) return current;
    const removed = this.removeMapping(current, mappingId);
    this.persistDocument(removed);
    return removed;
  }

  private async archiveMapping(
    document: StoredMappingDocument,
    project: ProjectRecord,
    mapping: RollbackPendingMapping | DeletingMapping,
    required = false,
    retainTombstone = false,
  ): Promise<StoredMappingDocument> {
    const owner = this.liveCommandOwner(mapping.runnerId, mapping.sourceSessionId);
    if (!owner) {
      if (!required) return document;
      const concluded = this.concludeRequiredArchive(document, mapping);
      if (concluded.mappings.some((candidate) => candidate.id === mapping.id)) {
        throw new Error('Runner is not connected.');
      }
      return concluded;
    }
    const attempt = mapping.archiveAttempt + 1;
    let current = document;
    try {
      const response = await this.runners.command(
        mapping.runnerId,
        { type: 'session_list', scope: 'all_projects' },
        {
          commandId: `${mapping.operationId}:catalog:${attempt}`,
          sourceSessionId: owner,
          allowBeforeProbe: true,
        },
      );
      const rows = sessionRows(response, owner);
      const state = exactIdentityState(rows, mapping, project.cwd);
      if (state === 'absent') {
        return this.settleArchivedMapping(current, mapping.id, retainTombstone);
      }
      if (state === 'conflict') {
        const conflicted = this.markConflict(current, mapping, 'identity_conflict');
        if (required) throw new Error('Mapped session identity conflicted during archive.');
        return conflicted;
      }
      const pending = { ...mapping, archiveAttempt: attempt, updatedAt: this.now() };
      current = this.replaceMapping(current, pending);
      this.persistDocument(current);
      const deleted = await this.archiveDeleteMapped(mapping, rows, owner, attempt);
      if (!deleted.success
        || deleted.command !== 'session_delete'
        || !deleted.sessionId
        || deleted.sessionId === mapping.sessionId) {
        throw new Error('Runner returned an invalid project session response.');
      }
      const data = responseData(deleted);
      if (data.archived !== true) {
        throw new Error('Runner returned an invalid project session response.');
      }
      if (data.sessionId !== mapping.sessionId) {
        const conflicted = this.markConflict(current, pending, 'identity_conflict');
        if (required) throw new Error('Mapped session identity conflicted during archive.');
        return conflicted;
      }
      current = this.settleArchivedMapping(current, mapping.id, retainTombstone);
      return current;
    } catch (error) {
      if (isSourceUnknown(error, 'session_list', owner)
        || isSourceUnknown(error, 'session_delete', owner)
        || isSourceUnknown(error, 'switch_session', owner)) {
        if (required) {
          const concluded = this.concludeRequiredArchive(current, mapping);
          if (concluded.mappings.some((candidate) => candidate.id === mapping.id)) {
            throw new Error('Runner session is no longer available for archive.');
          }
          return concluded;
        }
        return current;
      }
      const ownedDelete = ownedRemoteFailure(error, 'session_delete', owner);
      if (ownedDelete?.error === TARGET_NOT_FOUND) {
        return this.settleArchivedMapping(current, mapping.id, retainTombstone);
      }
      if (required) {
        const concluded = this.concludeRequiredArchive(current, mapping);
        if (concluded.mappings.some((candidate) => candidate.id === mapping.id)) {
          throw new Error(archiveFailure(error, mapping));
        }
        return concluded;
      }
      return current;
    }
  }

  private markConflict(
    document: StoredMappingDocument,
    mapping: ProjectRunnerMapping,
    reason: ConflictMapping['reason'],
  ): StoredMappingDocument {
    const conflict: ConflictMapping = {
      ...mapping,
      state: 'conflict',
      reason,
      updatedAt: this.now(),
    };
    const current = this.replaceMapping(document, conflict);
    this.persistDocument(current);
    return current;
  }

  private replaceMapping(
    document: StoredMappingDocument,
    mapping: ProjectRunnerMapping,
  ): StoredMappingDocument {
    return {
      ...document,
      mappings: document.mappings.map((candidate) => candidate.id === mapping.id ? mapping : candidate),
    };
  }

  private removeMapping(document: StoredMappingDocument, mappingId: string): StoredMappingDocument {
    return { ...document, mappings: document.mappings.filter((mapping) => mapping.id !== mappingId) };
  }

  private async mutate<T>(
    mutation: (document: StoredMappingDocument) => Promise<{ document: StoredMappingDocument; value: T }>,
  ): Promise<T> {
    const prior = this.mutationTail;
    let release: () => void = () => {};
    const turn = new Promise<void>((resolve) => { release = resolve; });
    this.mutationTail = prior.then(() => turn);
    await prior;
    try {
      return await this.withExclusiveLock(async () => {
        const loaded = loadMappingDocument(this.storage);
        if (loaded.kind === 'blocked') throw new Error(loaded.error);
        this.lockedDocument = loaded.document;
        try {
          const outcome = await mutation(loaded.document);
          const latest = this.lockedDocument ?? loaded.document;
          if (JSON.stringify(outcome.document) !== JSON.stringify(latest)) {
            this.persistDocument(outcome.document);
          }
          if (this.deferredPersistenceError) {
            const message = this.deferredPersistenceError;
            this.deferredPersistenceError = '';
            throw new Error(message);
          }
          return outcome.value;
        } finally {
          this.lockedDocument = null;
          this.deferredPersistenceError = '';
        }
      });
    } catch (error) {
      const message = fixedError(error);
      this.refreshError(message);
      throw new Error(message);
    } finally {
      release();
    }
  }

  private readSnapshot(): MappingSnapshot {
    const loaded = loadMappingDocument(this.storage);
    if (loaded.kind === 'blocked') return { projects: [], mappings: [], storageError: loaded.error };
    return { projects: loaded.document.projects, mappings: loaded.document.mappings, storageError: '' };
  }

  private persistDocument(document: StoredMappingDocument): void {
    saveMappingDocument(this.storage, document);
    if (this.lockedDocument) this.lockedDocument = document;
    this.applyDocument(document);
  }

  private applyDocument(document: StoredMappingDocument): void {
    this.snapshotValue = { projects: document.projects, mappings: document.mappings, storageError: '' };
    this.emit();
  }

  private refreshError(_operationError: string): void {
    this.snapshotValue = this.readSnapshot();
    this.emit();
  }

  private emit(): void {
    const snapshot = this.snapshot();
    for (const listener of this.listeners) listener(snapshot);
  }
}
