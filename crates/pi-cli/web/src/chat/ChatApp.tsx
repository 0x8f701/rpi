import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { useDesktopActions } from '../desktopBridge';
import {
  applyDrawerMode,
  applyOpenPath,
  closeFileConversationsDroppedBySlotPatch,
  fileExplorerCanonicalPath,
  fileExplorerSyncPageCacheWithSlots,
  readFileExplorerSlot,
  updateFileExplorerSlot,
  type FileExplorerSessionSlot,
  type FileExplorerTab,
} from '../fileExplorer';
import { PATH_COMPLETE_LIMIT, PathAutocomplete, type PathCompleteRequest, type PathCompleteResponse } from '../PathAutocomplete';
import { markdownOpenValidatedPath, markdownWorkspaceRelativePath, markdownOpenMissingFileToast, subscribeMarkdownOpenPath, type MarkdownPathOwner } from '../markdown';
import { FileExplorerPanel } from '../panels/FileExplorerPanel';

import { TmuxPreviewBoard } from './TmuxPreviewBoard';
import { TmuxTerminal } from './TmuxTerminal';
import { ChatEmptyState } from './ChatEmptyState';
import {
  CHAT_INVITE_EMPTY,
  CHAT_PROJECT_ATTACH_RUNNERS,
  CHAT_PROJECT_EMPTY_BODY,
  CHAT_PROJECT_EMPTY_TITLE,
  CHAT_PROJECT_FORM_HINT,
  CHAT_RAIL_EMPTY,
  CHAT_RUNNERS_EMPTY,
  CHAT_RUNNER_MAIN_EMPTY,
} from './chatEmpty';
import { BroadcastConversation } from './BroadcastConversation';
import { InviteRunnerDialog, type InviteRunnerCandidate } from './InviteRunnerDialog';
import { AddRunnerForm } from './AddRunnerForm';
import { SessionView, safeText, type SessionAuthority } from './SessionView';
import {
  CHAT_MAPPING_STORAGE_KEY,
  ChatMappingStore,
  type ActiveMapping,
  type DeletingMapping,
  type MappingSnapshot,
  type ProjectRecord,
  type ProjectRunnerMapping,
  type RollbackPendingMapping,
  validateStoredDocument,
} from './mapping';
import type { RpcResponse } from '../types';
import type { RunnerCommand } from './protocol';
import { isRunnerRecord, runnerDisplayName, RunnerCommandError, RunnerRegistry, dedupeRunnersKeepHealthiest, sortRunnersOnlineFirst, type RunnerRecord, type RunnerView } from './runners';
import {
  activeProjectMembers,
  deriveBroadcastRunStatus,
  deriveProjectAggregate,
  runnerActivityJobs,
  runnerSessionKey,
  TeamStore,
  teamJobKey,
  type ChannelTarget,
  type ContextPane,
  type ProjectAggregate,
  type TeamSnapshot,
} from './team';
import {
  DiscoveryClient,
  discoveredAlreadyAdded,
  fetchInviteCandidateStatus,
  type DiscoveredRunner,
  type InviteCandidateReach,
} from './discovery';
import { RUNNER_UNREACHABLE_MESSAGE, loadRunnerToken, runnerWebUrl } from './auth';
import {
  RUNNER_CONTEXT_ACTIONS,
  applyRunnerContextAction,
  openRunnerContextMenuFromEvent,
  runnerLongPressShouldArm,
  scheduleRunnerLongPress,
  type RunnerContextAction,
  type RunnerContextMenuTarget,
} from './runnerMenu';
import { subscribeRunnerEvents } from './events';
import {
  applyRunnerTokens,
  collectRunnerTokens,
  fetchMappingSync,
  isSyncNetworkFailure,
  pushMappingSync,
  syncErrorMessage,
  type MappingSyncPayload,
} from './chatSync';
import { loadTokenForAuthority, normalizeHostAuthority, saveTokenForAuthority, type StorageLike } from '../hostToken';
import { loadChatRailFocus, saveChatRailFocus, type ChatRailFocus } from './chatRail';
import { reconnectOverlayText, reconnectOverlayVisible } from '../reconnect';
import { ReconnectOverlay } from '../reconnectOverlay';
import './styles.css';


function lanDiscoverAddPayload(
  candidate: InviteRunnerCandidate,
  pageToken: string,
): { label: string; address: string; token: string } {
  const fragment = typeof window !== 'undefined' ? window.location.hash.replace(/^#/, '') : '';
  const token = pageToken || fragment;
  return { label: candidate.codename || candidate.label, address: candidate.address, token };
}

function candidateTone(state: InviteRunnerCandidate['state']): string {
  switch (state) {
    case 'online':
      return 'ok';
    case 'token_required':
      return 'error';
    case 'connecting':
      return 'warn';
    default:
      return 'muted';
  }
}

function shouldOfferCertTrust(runner: RunnerView): boolean {
  if (!runner.origin.startsWith('https://')) return false;
  switch (runner.state) {
    case 'token_required':
    case 'offline':
    case 'reconnecting':
      return true;
    case 'connecting':
    case 'connected':
      return false;
  }
}

function runnerTrustHref(
  runner: Pick<RunnerView, 'origin' | 'authority'>,
  storage: StorageLike | null,
): string {
  return runnerWebUrl(runner.origin, loadRunnerToken(storage, runner.authority));
}

function ChatSessionToast(props: { text: string; onDismiss: () => void }): JSX.Element | null {
  if (props.text === '') return null;
  return (
    <p className="chat-form-error chat-session-toast" role="alert">
      <span className="chat-session-toast__text">{safeText(props.text)}</span>
      <button
        className="chat-icon-button chat-session-toast__dismiss"
        type="button"
        aria-label="Dismiss"
        title="Dismiss"
        onClick={props.onDismiss}
      >
        ×
      </button>
    </p>
  );
}

function browserStorage(): Storage | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

export const CHAT_SELECTED_PROJECT_STORAGE_PREFIX = 'rpi-web-chat-project:v1:';
export const MAX_RUNNER_ERROR_CHARS = 240;

/** Chat renders its own Files reopen toggle id. App's FileExplorerPanel
 *  defaults to FILE_EXPLORER_TOGGLE_ID; Chat passes this distinct id so its
 *  drawer derives its own hide/tree/line ids (no document-wide id collisions
 *  with App chrome) and Escape focus restores Chat's toggle. */
export const CHAT_FILE_EXPLORER_TOGGLE_ID = 'chat-file-explorer-toggle-btn';

function pageHostAuthority(): string {
  try {
    return normalizeHostAuthority(typeof window !== 'undefined' && window.location ? window.location.host : '');
  } catch {
    return '';
  }
}

let mappingSyncInFlight = false;

const CHAT_SYNC_META_KEY = 'rpi-web-chat-sync:v1';

interface ChatSyncMeta { baseRevision: number; dirty: boolean; }

function readSyncMeta(storage: StorageLike | null): ChatSyncMeta {
  try {
    const raw = storage?.getItem(CHAT_SYNC_META_KEY) ?? null;
    if (!raw) return { baseRevision: 0, dirty: false };
    const parsed = JSON.parse(raw) as Partial<ChatSyncMeta>;
    if (typeof parsed.baseRevision !== 'number' || !Number.isFinite(parsed.baseRevision)
      || typeof parsed.dirty !== 'boolean') throw new Error('corrupt');
    return { baseRevision: parsed.baseRevision, dirty: parsed.dirty };
  } catch {
    return { baseRevision: 0, dirty: false };
  }
}

function writeSyncMeta(storage: StorageLike | null, meta: ChatSyncMeta): void {
  try {
    storage?.setItem(CHAT_SYNC_META_KEY, JSON.stringify(meta));
  } catch {
    // Meta persistence is best-effort: losing it only resets the revision
    // baseline, and the server-side conflict gate still protects the copy.
  }
}

/** Boot/push sync against the page host listener (chat_sync.rs), built on a
 *  revision-based dirty model: `{baseRevision, dirty}` persisted in
 *  `rpi-web-chat-sync:v1`. Boot adopts the server copy when the local state
 *  is empty or the server revision has moved past the last-seen baseline,
 *  then flushes a pending dirty push. Pushes always send the local document
 *  on `baseRevision`; a conflict adopts the server copy wholesale. A push
 *  ack clears the dirty flag only when live local state still matches the
 *  exact payload the server acknowledged; edits landing in flight re-push
 *  (3 iterations max, then the meta stays dirty for the next sync). Every
 *  mutation marks dirty before its push is scheduled, so skip and failure
 *  paths never lose the pending-sync marker. Tokens
 *  ride the same document — they pair browsers to listeners, the same trust
 *  tier as the listener's own token file, and stay on the operator's
 *  machines. Only the exact non-fatal categories in isSyncNetworkFailure
 *  are swallowed; everything else surfaces through reportError. */
async function runMappingSync(
  storage: StorageLike | null,
  pageAuthority: string,
  registry: RunnerRegistry,
  mappingStore: ChatMappingStore,
  mode: 'boot' | 'push',
  reportError: (message: string) => void,
): Promise<void> {
  if (mappingSyncInFlight || typeof window === 'undefined') {
    // A sync is already running: this call represents a live local change.
    // Persist dirty so the running loop's fingerprint check (or the next
    // sync) drains it instead of silently dropping the mutation.
    if (mappingSyncInFlight) {
      writeSyncMeta(storage, { ...readSyncMeta(storage), dirty: true });
    }
    return;
  }
  const pageToken = loadTokenForAuthority(storage, pageAuthority);
  if (pageToken === '') return;
  mappingSyncInFlight = true;
  try {
    const origin = window.location.origin;
    if (mode === 'boot') {
      const remote = await fetchMappingSync(origin, pageToken, isStoredMappingDocument);
      const meta = readSyncMeta(storage);
      // Unpushed local work (including an emptied document) must try CAS
      // first. Adopting here would replace that work with the server copy
      // and then push the adopted copy — a lost update. Conflict still
      // adopts wholesale, which is the documented LWW path.
      if (!meta.dirty && remote.document
        && (localDocumentIsEmpty(mappingStore, registry) || remote.revision > meta.baseRevision)) {
        await adoptRemoteSync(mappingStore, registry, storage, remote.document);
        writeSyncMeta(storage, { baseRevision: remote.revision, dirty: false });
      }
      if (readSyncMeta(storage).dirty) {
        await pushSyncLoop(origin, pageToken, storage, mappingStore, registry);
      }
      return;
    }
    await pushSyncLoop(origin, pageToken, storage, mappingStore, registry);
  } catch (error) {
    if (isSyncNetworkFailure(error)) return;
    reportError(syncErrorMessage(error));
  } finally {
    mappingSyncInFlight = false;
  }
}

function localDocumentIsEmpty(mappingStore: ChatMappingStore, registry: RunnerRegistry): boolean {
  const document = mappingStore.exportDocument();
  return document.projects.length === 0
    && document.mappings.length === 0
    && registry.exportRecords().length === 0;
}

/** One full push cycle: each iteration fingerprints the exact outbound
 *  payload, pushes it on the persisted baseRevision, adopts the server
 *  copy on conflict, and clears the dirty flag only while live local
 *  state still matches what the server just acknowledged — a write that
 *  lands in flight re-pushes on the next iteration (max 3, then the meta
 *  stays dirty for the next sync). */
async function pushSyncLoop(
  origin: string,
  pageToken: string,
  storage: StorageLike | null,
  mappingStore: ChatMappingStore,
  registry: RunnerRegistry,
): Promise<void> {
  for (let iteration = 0; iteration < 3; iteration += 1) {
    const meta = readSyncMeta(storage);
    const payload: MappingSyncPayload = {
      mapping: mappingStore.exportDocument(),
      runners: registry.exportRecords(),
      tokens: collectRunnerTokens(storage),
    };
    // Fingerprint THIS iteration's exact outbound payload: the ack below
    // clears dirty only if live state still equals what was submitted, so
    // one stable state exits after a single successful push.
    const submitted = JSON.stringify(payload);
    const result = await pushMappingSync(origin, pageToken, payload, meta.baseRevision, isStoredMappingDocument);
    // Advance the base on EVERY server response, before any re-push: the
    // next iteration must never resubmit a stale base revision.
    const revision = result.revision;
    if ('conflict' in result && result.conflict === true) {
      // The server copy is the operator's newer state (another device
      // wrote first). Adopt it wholesale rather than racing blind
      // overwrites.
      await adoptRemoteSync(mappingStore, registry, storage, result.document);
      writeSyncMeta(storage, { baseRevision: revision, dirty: true });
      continue;
    }
    if (localSyncFingerprint(storage, mappingStore, registry) === submitted) {
      writeSyncMeta(storage, { baseRevision: revision, dirty: false });
      return;
    }
    writeSyncMeta(storage, { baseRevision: revision, dirty: true });
    // Live state moved past the acknowledged payload: keep dirty set (the
    // caller marked it before this push was scheduled) and re-push.
  }
  writeSyncMeta(storage, { ...readSyncMeta(storage), dirty: true });
}

function localSyncFingerprint(
  storage: StorageLike | null,
  mappingStore: ChatMappingStore,
  registry: RunnerRegistry,
): string {
  return JSON.stringify({
    mapping: mappingStore.exportDocument(),
    runners: registry.exportRecords(),
    tokens: collectRunnerTokens(storage),
  });
}

/** Adopt a remote sync document. The mapping payload is validated first so
 *  an invalid document is rejected before ANY state persists; then tokens,
 *  runner records, and the mapping document apply in that order — with
 *  every mapping.runnerId rewritten through the registry's remote→local id
 *  map, and mappings whose runner resolved to nothing dropped. */
async function adoptRemoteSync(
  mappingStore: ChatMappingStore,
  registry: RunnerRegistry,
  storage: StorageLike | null,
  document: MappingSyncPayload,
): Promise<void> {
  const error = validateStoredDocument(document.mapping);
  if (error !== null) throw new Error(`Synced chat mapping document is invalid: ${error}`);
  applyRunnerTokens(storage, document.tokens);
  const { idByAuthority } = registry.importRecords(asRunnerRecords(document.runners));
  const mapping = document.mapping as StoredMappingDocumentShape;
  await mappingStore.adoptDocument({
    ...mapping,
    mappings: mapping.mappings.flatMap((record) => {
      const runnerId = idByAuthority.get(record.runnerId);
      return runnerId ? [{ ...record, runnerId }] : [];
    }),
  });
}

function asRunnerRecords(values: unknown[]): RunnerRecord[] {
  return values.filter(isRunnerRecord);
}

function isStoredMappingDocument(value: unknown): value is StoredMappingDocumentShape {
  return validateStoredDocument(value) === null;
}

type StoredMappingDocumentShape = { version: 1; projects: ProjectRecord[]; mappings: ProjectRunnerMapping[] };

export function selectedProjectStorageKey(authority: string): string {
  return `${CHAT_SELECTED_PROJECT_STORAGE_PREFIX}${encodeURIComponent(normalizeHostAuthority(authority))}`;
}

export function loadSelectedProjectId(storage: StorageLike | null, authority: string): string {
  if (!storage) return '';
  try {
    const raw = (storage.getItem(selectedProjectStorageKey(authority)) || '').trim();
    if (!raw || raw.length > 128 || /[/\\\n]/.test(raw)) return '';
    return raw;
  } catch {
    return '';
  }
}

export function saveSelectedProjectId(storage: StorageLike | null, authority: string, projectId: string): void {
  if (!storage) return;
  const key = selectedProjectStorageKey(authority);
  try {
    const value = projectId.trim();
    if (!value || value.length > 128 || /[/\\\n]/.test(value)) {
      storage.removeItem(key);
      return;
    }
    storage.setItem(key, value);
  } catch {
    /* private mode / quota — selection lives in memory only */
  }
}

export function selectStoredProject(
  projects: readonly ProjectRecord[],
  savedId: string,
): ProjectRecord | null {
  const ordered = [...projects].sort((left, right) => left.createdAt - right.createdAt || left.id.localeCompare(right.id));
  const saved = savedId.trim();
  if (saved) {
    const match = ordered.find((project) => project.id === saved);
    if (match) return match;
  }
  return ordered[0] ?? null;
}

export function boundedVisibleRunnerError(error: unknown): string {
  const redacted = safeText(error)
    .replace(/(?:[A-Za-z]:)?(?:\/|\\)[^\s'"]+/g, '[path]')
    .replace(/\s+/g, ' ')
    .trim();
  if (!redacted) return '';
  return redacted.length <= MAX_RUNNER_ERROR_CHARS
    ? redacted
    : `${redacted.slice(0, MAX_RUNNER_ERROR_CHARS - 1)}…`;
}

/** Bounded, redacted message for one markdown-open probe rejection, fed to
 *  the probe's own session-toast slot. Null/undefined rejections (and any
 *  bounded-empty message) fall back to the fixed literal so the row is
 *  never blank and never renders 'null'/'undefined'.
 *  Transient-vs-not-found is distinguished EXACTLY through the structured
 *  RunnerCommandError kind — never message text: a transport/protocol
 *  rejection proves nothing about the requested file, so it renders the
 *  fixed neutral literal instead of masquerading as a file verdict; a
 *  remote rejection is the runner's own file verdict and keeps its
 *  bounded, redacted text. */
export function markdownProbeErrorText(error: unknown): string {
  if (error instanceof RunnerCommandError && error.kind !== 'remote') {
    return 'file open failed';
  }
  return boundedVisibleRunnerError(
    error instanceof Error ? error.message : error == null ? '' : String(error),
  ) || 'file open failed';
}

export function chatMarkdownOpenFailureText(error: unknown, path: string): string {
  if (error instanceof RunnerCommandError && error.rpcCode === 'file_unavailable') {
    return markdownOpenMissingFileToast(path);
  }
  return markdownProbeErrorText(error);
}

/** Monotonic markdown-open probe attempt gate. Each new probe claims the
 *  next generation; only the LATEST attempt's completion may commit state
 *  (toast clear/set, file-owner switch, drawer open), so a slower earlier
 *  probe can never overwrite a newer probe's success or failure — whichever
 *  order the two completions arrive in. */
export interface MarkdownProbeAttempt {
  readonly attempt: number;
  readonly isCurrent: () => boolean;
}

export function beginMarkdownProbeAttempt(counter: { current: number }): MarkdownProbeAttempt {
  const attempt = counter.current + 1;
  counter.current = attempt;
  return { attempt, isCurrent: () => counter.current === attempt };
}



function statusTone(state: RunnerView['state']): string {
  if (state === 'token_required') return 'error';
  if (state === 'connected') return 'ok';
  if (state === 'offline') return 'muted';
  return 'warn';
}


function mappingSetupLabel(state: ProjectRunnerMapping['state']): string {
  switch (state) {
    case 'deleting':
    case 'rollback_pending':
      return 'Removing';
    case 'conflict':
      return 'Needs attention';
    default:
      return 'Setting up';
  }
}

export interface ChatSessionBinding {
  project: ProjectRecord;
  runner: RunnerView;
  mapping: ActiveMapping;
}

export function sessionBindingForMapping(
  project: ProjectRecord,
  runner: RunnerView,
  mapping: ProjectRunnerMapping | undefined,
): ChatSessionBinding | null {
  return mapping?.state === 'active' ? { project, runner, mapping } : null;
}

export function sessionBindingForOwner(
  projects: readonly ProjectRecord[],
  runners: readonly RunnerView[],
  mappings: readonly ProjectRunnerMapping[],
  owner: MarkdownPathOwner,
): ChatSessionBinding | null {
  const mappingId = owner.mappingId?.trim();
  const runnerId = owner.runnerId?.trim();
  const sessionId = owner.sessionId?.trim();
  if (!mappingId || !runnerId || !sessionId) return null;
  const mapping = mappings.find((candidate): candidate is ActiveMapping => (
    candidate.state === 'active'
    && candidate.id === mappingId
    && candidate.runnerId === runnerId
    && candidate.sessionId === sessionId
  ));
  if (!mapping) return null;
  const project = projects.find((candidate) => candidate.id === mapping.projectId);
  const runner = runners.find((candidate) => candidate.id === mapping.runnerId);
  if (!project || !runner) return null;
  return { project, runner, mapping };
}

export function chatMarkdownExplorerOwnerKey(
  owner: MarkdownPathOwner,
  path: string,
): string | null {
  const mappingId = owner.mappingId?.trim();
  const runnerId = owner.runnerId?.trim();
  const sessionId = owner.sessionId?.trim();
  if (!mappingId || !runnerId || !sessionId) return null;
  return markdownWorkspaceRelativePath(path, owner.workspaceCwd) ? mappingId : null;
}

export function chatFileExplorerSendOptions(
  owner: ChatSessionBinding | null,
): { runnerId: string; sourceSessionId: string; workspaceCwd: string } | null {
  if (!owner) return null;
  return {
    runnerId: owner.runner.id,
    sourceSessionId: owner.mapping.sessionId,
    workspaceCwd: owner.project.cwd,
  };
}

export function shouldRetryMarkdownFileUnavailable(error: unknown, retried: boolean): boolean {
  return !retried && error instanceof RunnerCommandError && error.rpcCode === 'file_unavailable';
}

export function readGetStateCwd(data: unknown): string {
  if (data === null || typeof data !== 'object' || Array.isArray(data) || !('cwd' in data)) return '';
  const cwd = data.cwd;
  return typeof cwd === 'string' ? cwd.trim() : '';
}

export function runChatMarkdownOpenProbe(args: {
  path: string;
  line: number | undefined;
  owner: MarkdownPathOwner;
  ownerBinding: ChatSessionBinding;
  workspaceCwd: string;
  retried: boolean;
  isCurrentAttempt: () => boolean;
  send: (runnerId: string, command: RunnerCommand, options?: { sourceSessionId?: string }) => Promise<RpcResponse>;
  onProbeStarted: () => void;
  onOpened: (relative: string, openLine: number | undefined) => void;
  onFailed: (error: unknown) => void;
  completion?: string;
}): void {
  const {
    path,
    line,
    owner,
    ownerBinding,
    workspaceCwd,
    retried,
    isCurrentAttempt,
    send,
    onProbeStarted,
    onOpened,
    onFailed,
    completion,
  } = args;
  void markdownOpenValidatedPath(
    path,
    line,
    { ...owner, workspaceCwd },
    async (command) => {
      if (isCurrentAttempt()) onProbeStarted();
      const options = chatFileExplorerSendOptions(ownerBinding);
      if (!options) throw new Error('Session is unavailable.');
      const response = await send(options.runnerId, command as RunnerCommand, {
        sourceSessionId: options.sourceSessionId,
      });
      return response.data;
    },
    (relative, openLine) => {
      if (!isCurrentAttempt()) return;
      onOpened(relative, openLine);
    },
    (error) => {
      if (!isCurrentAttempt()) return;
      if (!shouldRetryMarkdownFileUnavailable(error, retried)) {
        onFailed(error);
        return;
      }
      const options = chatFileExplorerSendOptions(ownerBinding);
      if (!options) {
        onFailed(error);
        return;
      }
      void send(options.runnerId, { type: 'get_state' }, { sourceSessionId: options.sourceSessionId })
        .then((response) => {
          if (!isCurrentAttempt()) return;
          runChatMarkdownOpenProbe({
            ...args,
            workspaceCwd: readGetStateCwd(response.data),
            retried: true,
          });
        })
        .catch((refreshError) => {
          if (!isCurrentAttempt()) return;
          onFailed(refreshError);
        });
    },
    completion,
  );
}

/** Completion authority for the new-project cwd field: the selected session
 *  binding when one exists, otherwise the first active mapping whose runner
 *  is connected (a project being created has no mapping of its own yet).
 *  Returns null when no runner session can answer — suggestions are then
 *  unavailable and manual input stays the only path. */
export function chatProjectCompletionBinding(
  active: ChatSessionBinding | null,
  projects: readonly ProjectRecord[],
  runners: readonly RunnerView[],
  mappings: readonly ProjectRunnerMapping[],
): ChatSessionBinding | null {
  if (active) return active;
  const mapping = mappings.find((row): row is ActiveMapping => row.state === 'active'
    && runners.some((runner) => runner.id === row.runnerId && runner.state === 'connected'));
  if (!mapping) return null;
  const project = projects.find((row) => row.id === mapping.projectId);
  const runner = runners.find((row) => row.id === mapping.runnerId);
  return project && runner ? { project, runner, mapping } : null;
}

/** Validates a `path_complete` response body; anything malformed yields null
 *  so garbage from a runner never reaches the autocomplete surface. */
export function chatPathCompletionResponse(data: unknown): PathCompleteResponse | null {
  if (!data || typeof data !== 'object') return null;
  const row = data as Record<string, unknown>;
  const base = row.base;
  const prefix = row.prefix;
  const truncated = row.truncated;
  if (typeof base !== 'string' || typeof prefix !== 'string' || typeof truncated !== 'boolean') return null;
  if (!Array.isArray(row.entries)) return null;
  const entries: PathCompleteResponse['entries'] = [];
  for (const entry of row.entries) {
    if (!entry || typeof entry !== 'object') return null;
    const cell = entry as Record<string, unknown>;
    if (typeof cell.name !== 'string' || typeof cell.path !== 'string'
      || (cell.kind !== 'directory' && cell.kind !== 'file')) return null;
    entries.push({ name: cell.name, path: cell.path, kind: cell.kind });
  }
  if (entries.length > PATH_COMPLETE_LIMIT) {
    return { base, prefix, entries: entries.slice(0, PATH_COMPLETE_LIMIT), truncated: true };
  }
  return { base, prefix, entries, truncated };
}

export interface ChatProjectPathCompleteDeps {
  /** Current completion binding; compared by identity after every response so
   *  a runner/session that changed mid-flight is dropped as stale. */
  bindingRef: { current: ChatSessionBinding | null };
  command: (
    runnerId: string,
    command: RunnerCommand,
    options: { sourceSessionId: string },
  ) => Promise<RpcResponse>;
}

/** Builds the `sendComplete` closure handed to PathAutocomplete for the
 *  chat-project cwd field. The component supplies `{type, path, kind, limit}`;
 *  this routes it through the existing registry command socket with the
 *  binding's session so relative paths resolve against that session's cwd.
 *  No binding, dead binding, transport failure, or stale response resolves to
 *  null — the component then shows no suggestions and manual input is
 *  unaffected. */
export function createChatProjectPathComplete(
  deps: ChatProjectPathCompleteDeps,
): (request: PathCompleteRequest) => Promise<PathCompleteResponse | null> {
  return async (request) => {
    const binding = deps.bindingRef.current;
    if (!binding) return null;
    const options = chatFileExplorerSendOptions(binding);
    if (!options) return null;
    try {
      const response = await deps.command(options.runnerId, {
        type: 'path_complete',
        path: request.path,
        kind: request.kind,
        limit: request.limit,
        sessionId: options.sourceSessionId,
      }, { sourceSessionId: options.sourceSessionId });
      if (deps.bindingRef.current !== binding) return null;
      if (response.success !== true) return null;
      return chatPathCompletionResponse(response.data);
    } catch {
      return null;
    }
  };
}

/** Shared epoch guarding the chat transport lifecycle. Each setup claims a
 *  new value; a deferred cleanup stop only fires when no newer setup (or
 *  pagehide stop) has claimed since. Lives on a ref so React 18 StrictMode's
 *  setup→cleanup→setup cycle on the same store instances can cancel the
 *  pending teardown between setups. */
export interface ChatTransportEpoch {
  current: number;
}

/** Mounts the chat transport stores and wires their terminal stop to the
 *  page lifecycle. Returns the React effect cleanup:
 *
 *  - `pagehide` stops synchronously (the page is going away; there is no
 *    later chance) and invalidates any pending deferred stop. `stop` must be
 *    idempotent — pagehide may already have fired when the cleanup runs.
 *  - The cleanup defers its stop by one microtask, guarded by the shared
 *    epoch: StrictMode's setup→cleanup→setup on the same stores claims a new
 *    epoch in the follow-up setup, cancelling the teardown between setups —
 *    disposed transports are not restartable. A real unmount has no
 *    follow-up setup, so the deferred stop runs and disposes everything.
 *  - Route hide keeps the component mounted, so the cleanup never runs on a
 *    hidden view. */
export function mountChatTransports(
  epoch: ChatTransportEpoch,
  start: () => void,
  stop: () => void,
  win: {
    addEventListener(type: string, listener: () => void, options?: { once?: boolean }): void;
    removeEventListener(type: string, listener: () => void): void;
  } = window,
): () => void {
  epoch.current += 1;
  const epochAtSetup = epoch.current;
  start();
  const stopAll = () => {
    epoch.current += 1;
    stop();
  };
  win.addEventListener('pagehide', stopAll, { once: true });
  return () => {
    win.removeEventListener('pagehide', stopAll);
    queueMicrotask(() => {
      if (epoch.current === epochAtSetup) stopAll();
    });
  };
}

export function applyChatMarkdownOpenPath(
  slots: Record<string, FileExplorerSessionSlot>,
  path: string,
  line: number | undefined,
  owner: MarkdownPathOwner,
): Record<string, FileExplorerSessionSlot> {
  const mappingId = owner.mappingId?.trim();
  if (!mappingId) return slots;
  const relative = markdownWorkspaceRelativePath(path, owner.workspaceCwd);
  if (!relative) return slots;
  return updateFileExplorerSlot(slots, mappingId, (slot) => applyOpenPath(slot, relative, line)).slots;
}

export function applyChatFileExplorerToggle(
  slots: Record<string, FileExplorerSessionSlot>,
  mappingId: string | null,
): Record<string, FileExplorerSessionSlot> {
  const ownerKey = mappingId?.trim();
  if (!ownerKey) return slots;
  return updateFileExplorerSlot(slots, ownerKey, (slot) => applyDrawerMode(slot, slot.mode === 'closed' ? 'open' : 'closed')).slots;
}

export function applyChatFileExplorerOpen(
  slots: Record<string, FileExplorerSessionSlot>,
  mappingId: string | null,
): Record<string, FileExplorerSessionSlot> {
  const ownerKey = mappingId?.trim();
  if (!ownerKey) return slots;
  return updateFileExplorerSlot(slots, ownerKey, (slot) => applyDrawerMode(slot, 'open')).slots;
}

function projectTone(
  projectId: string,
  mappings: readonly ProjectRunnerMapping[],
  runners: readonly RunnerView[],
): string {
  const active = mappings.filter((mapping): mapping is ActiveMapping => mapping.projectId === projectId && mapping.state === 'active');
  if (active.length === 0) return 'hollow';
  let online = 0;
  let tokenRequired = false;
  for (const mapping of active) {
    const runner = runners.find((candidate) => candidate.id === mapping.runnerId);
    if (!runner || runner.state !== 'connected') {
      if (runner?.state === 'token_required') tokenRequired = true;
      continue;
    }
    online += 1;
  }
  if (tokenRequired) return 'error';
  if (online === active.length) return 'ok';
  if (online > 0) return 'warn';
  return 'muted';
}

function jobsTrustLabel(aggregate: ProjectAggregate): 'checking' | 'orchestration off' | 'idle' {
  const sessions = aggregate.runnerSessions;
  if (sessions.length === 0) return 'checking';
  if (aggregate.orchestrationDisabledCount === sessions.length) {
    return 'orchestration off';
  }
  if (sessions.some((session) => session.orchestrationEnabled === null)) {
    return 'checking';
  }
  return 'idle';
}

function jobsNoteText(aggregate: ProjectAggregate): string {
  const trust = jobsTrustLabel(aggregate);
  if (trust === 'checking' || trust === 'orchestration off') return trust;
  if (aggregate.activeJobCount === 0 && aggregate.failedJobCount === 0) return 'No live jobs';
  return `${aggregate.activeJobCount} live · ${aggregate.failedJobCount} failed`;
}

function projectBroadcastCount(projectId: string, team: TeamSnapshot): number {
  const channel = team.channels?.[projectId];
  if (!channel) return 0;
  return channel.broadcastRuns.filter((run) => deriveBroadcastRunStatus(run) === 'running').length;
}

function sortedProjects(projects: readonly ProjectRecord[]): ProjectRecord[] {
  return [...projects].sort((left, right) => left.createdAt - right.createdAt || left.id.localeCompare(right.id));
}

/** Production project-form submit flow, shared by ChatApp and the focused
 *  chatProjectCwd test (no surrogate handler): one busy gate, the error slot
 *  cleared before the store reports, the bounded store message stored RAW in
 *  state (the render applies the single safeText pass), and accepted
 *  projects handed to onAccepted for the success side effects. */
export interface ChatProjectFormController {
  busy: () => boolean;
  submit: (
    event: { preventDefault(): void },
    submission: { label: string; cwd: string },
  ) => Promise<void>;
}

export function createChatProjectFormController(deps: {
  addProject: (label: string, cwd: string) => Promise<ProjectRecord>;
  onBusy: (busy: boolean) => void;
  onError: (message: string) => void;
  onAccepted: (project: ProjectRecord) => void;
}): ChatProjectFormController {
  let busy = false;
  return {
    busy: () => busy,
    submit: async (event, submission) => {
      event.preventDefault();
      if (busy) return;
      busy = true;
      deps.onBusy(true);
      deps.onError('');
      try {
        deps.onAccepted(await deps.addProject(submission.label, submission.cwd));
      } catch (error) {
        deps.onError(error instanceof Error ? error.message : String(error));
      } finally {
        busy = false;
        deps.onBusy(false);
      }
    },
  };
}

export interface ChatProjectFormProps {
  projectLabel: string;
  setProjectLabel: (next: string) => void;
  projectCwd: string;
  setProjectCwd: (next: string) => void;
  addProject: (event: React.FormEvent<HTMLFormElement>) => void;
  closeProjectForm: () => void;
  sendProjectPathComplete: (request: PathCompleteRequest) => Promise<PathCompleteResponse | null>;
  isProjectBusy: boolean;
  storageError: string;
  runners?: readonly { id: string; label: string; connected: boolean }[];
  selectedRunnerIds?: readonly string[];
  onToggleRunner?: (runnerId: string) => void;
}

/** Project-create modal, extracted from ChatApp so the chatProjectCwd
 *  integration test can render the real form without mounting the whole
 *  app. JSX is identical to the previously inline block. */
export function ChatProjectForm({
  projectLabel,
  setProjectLabel,
  projectCwd,
  setProjectCwd,
  addProject,
  closeProjectForm,
  sendProjectPathComplete,
  isProjectBusy,
  storageError,
  runners = [],
  selectedRunnerIds = [],
  onToggleRunner = () => {},
}: ChatProjectFormProps) {
  return (
    <div className="chat-project-modal-backdrop" onMouseDown={(event) => {
      if (event.target === event.currentTarget) {
        closeProjectForm();
      }
    }}>
      <section
        className="chat-panel chat-panel--form chat-project-modal"
        id="chat-project-form"
        role="dialog"
        aria-modal="true"
        aria-labelledby="chat-project-form-title"
        aria-describedby="chat-project-form-hint"
        onMouseDown={(event) => event.stopPropagation()}
      >
        <div className="chat-panel__head">
          <h2 id="chat-project-form-title">New project</h2>
          <button
            className="chat-button chat-button--quiet chat-project-modal__close"
            type="button"
            aria-label="Close"
            onClick={closeProjectForm}
          >×</button>
        </div>
        <p className="chat-project-modal__hint" id="chat-project-form-hint">{CHAT_PROJECT_FORM_HINT}</p>
        <form className="chat-runner-form chat-runner-form--project" onSubmit={addProject}>
          <label className="chat-field">
            Label
            <input
              id="chat-project-label"
              value={projectLabel}
              onChange={(event) => setProjectLabel(event.target.value)}
              maxLength={80}
              required
              autoFocus
            />
          </label>
          <label className="chat-field chat-field--wide">
            Path
            <PathAutocomplete
              id="chat-project-cwd"
              value={projectCwd}
              onChange={setProjectCwd}
              sendComplete={sendProjectPathComplete}
              kind="directory"
              placeholder="/workspace/project"
              ariaLabel="Path"
              required
            />
          </label>
          <button className="chat-button chat-button--primary" id="chat-project-add" type="submit" disabled={isProjectBusy || storageError !== ''}>
            Confirm
          </button>
        </form>
        {runners.length > 0 && (
          <fieldset id="chat-project-runners">
            <legend>{CHAT_PROJECT_ATTACH_RUNNERS}</legend>
            {runners.map((runner) => (
              <label className="chat-field" key={runner.id}>
                <input
                  type="checkbox"
                  checked={selectedRunnerIds.includes(runner.id)}
                  disabled={!runner.connected}
                  onChange={() => onToggleRunner(runner.id)}
                />
                {safeText(runner.label)}
              </label>
            ))}
          </fieldset>
        )}
      </section>
    </div>
  );
}

export function ChatApp() {
  const [storage] = useState(() => {
    const next = browserStorage();
    if (typeof window !== 'undefined') {
      const fragment = window.location.hash.replace(/^#/, '');
      if (fragment !== '') {
        saveTokenForAuthority(next, normalizeHostAuthority(window.location.host), fragment);
        window.history.replaceState(null, '', window.location.pathname + window.location.search);
      }
    }
    return next;
  });
  const [registry] = useState(() => new RunnerRegistry({ storage }));
  const [teamStore] = useState(() => new TeamStore({
    runners: registry,
    storage,
    pageAuthority: pageHostAuthority(),
  }));
  const [mappingStore] = useState(() => {
    const store = new ChatMappingStore({ storage, runners: registry });
    registry.setBeforeProbe((runnerId) => store.recoverRunner(runnerId));
    return store;
  });
  const [discovery] = useState(() => new DiscoveryClient());
  const [runners, setRunners] = useState<RunnerView[]>(() => registry.snapshot());
  const [team, setTeam] = useState<TeamSnapshot>(() => teamStore.snapshot());
  const [mapping, setMapping] = useState<MappingSnapshot>(() => mappingStore.snapshot());
  const [discovered, setDiscovered] = useState<DiscoveredRunner[]>([]);
  const [formOpen, setFormOpen] = useState(false);
  const [projectFormOpen, setProjectFormOpen] = useState(false);
  const [gearOpen, setGearOpen] = useState(false);
  const [workspaceMenuOpen, setWorkspaceMenuOpen] = useState(false);
  const [runnerGearOpen, setRunnerGearOpen] = useState(false);
  const [runnerContextMenu, setRunnerContextMenu] = useState<RunnerContextMenuTarget | null>(null);
  const isAddingRef = useRef(false);
  const [formError, setFormError] = useState('');
  const [dismissedToast, setDismissedToast] = useState('');
  const [projectLabel, setProjectLabel] = useState('');
  const [projectCwd, setProjectCwd] = useState('');
  const [isProjectBusy, setIsProjectBusy] = useState(false);
  const [projectSearch, setProjectSearch] = useState('');
  const [runnerSearch, setRunnerSearch] = useState('');
  const [runnerFormOpen, setRunnerFormOpen] = useState(false);
  const [tokenFormRunnerId, setTokenFormRunnerId] = useState<string | null>(null);
  const [tokenDraft, setTokenDraft] = useState('');
  const [createSessionOpen, setCreateSessionOpen] = useState(false);
  const [confirmRunnerDelete, setConfirmRunnerDelete] = useState(false);
  const [confirmProjectDelete, setConfirmProjectDelete] = useState(false);
  const [createSessionName, setCreateSessionName] = useState('');
  const [createSessionBusy, setCreateSessionBusy] = useState(false);
  const [previewRefreshEpoch, setPreviewRefreshEpoch] = useState(0);


  const [railFocus, setRailFocus] = useState<ChatRailFocus | null>(null);
  const [projectRunnerIds, setProjectRunnerIds] = useState<string[]>([]);
  const [inviteBusyKey, setInviteBusyKey] = useState<string | null>(null);
  const inviteBusyRef = useRef(false);
  // Last COMMITTED slot map, written only inside the close-diff effect below
  // (never render-assigned): the effect diffs it against the newly committed
  // map to derive removed tab identities, keeping the state updater pure.
  const fileExplorerCommittedSlotsRef = useRef<Record<string, FileExplorerSessionSlot>>({});
  // Owner keys (mapping ids) with a conversation close not yet settled. A
  // removed mapping's captured binding must survive until its close settles;
  // the capture rebuild prunes an entry exactly when it is unused — the
  // mapping is gone AND no close is still in flight for it.
  const fileExplorerClosePendingRef = useRef<Set<string>>(new Set());
  // Original runner+session binding per mapping id, captured from the live
  // mapping list while the mapping could still address its session (active,
  // deleting, or rollback-pending). A removed mapping's slot cleanup closes
  // its conversations through this captured identity — the current list no
  // longer names the victim and no fallback lookup may resurrect it; a
  // missing capture surfaces an error instead of a silent no-op.
  const fileExplorerBindingCaptureRef = useRef<Record<string, { runnerId: string; sourceSessionId: string }>>({});
  const [fileExplorerByMappingId, setFileExplorerByMappingId] = useState<Record<string, FileExplorerSessionSlot>>({});
  // The markdown-open probe has its OWN session-toast slot: formError
  // doubles as the invite/project form error, so a probe rejection landing
  // while a form is open must never masquerade as a form failure.
  const [markdownProbeError, setMarkdownProbeError] = useState('');
  // Monotonic attempt generation for markdown-open probes. Every new probe
  // (a target change: another owner key or path) supersedes the previous
  // attempt, so a slower EARLIER probe can never clear or overwrite the
  // toast, file-owner selection, or drawer a newer probe already committed —
  // whichever order the two completions arrive in.
  const markdownProbeAttemptRef = useRef(0);
  const [inviteDiscoveryStatus, setInviteDiscoveryStatus] = useState<Record<string, InviteCandidateReach | 'connecting'>>({});
  const chatRootRef = useRef<HTMLElement | null>(null);
  const inviteButtonRef = useRef<HTMLButtonElement | null>(null);
  const gearButtonRef = useRef<HTMLButtonElement | null>(null);
  const detailsButtonRef = useRef<HTMLButtonElement | null>(null);
  const gearMenuRef = useRef<HTMLDivElement | null>(null);
  const workspaceMenuRef = useRef<HTMLDivElement | null>(null);
  const workspaceMenuButtonRef = useRef<HTMLButtonElement | null>(null);
  const runnerGearMenuRef = useRef<HTMLDivElement | null>(null);
  const runnerContextMenuRef = useRef<HTMLDivElement | null>(null);
  const runnerMenuLongPressRef = useRef<{ pointerId: number; timer: number } | null>(null);
  const runnerMenuSkipClickRef = useRef(false);
  const projectAddButtonRef = useRef<HTMLButtonElement | null>(null);
  const runnerAddButtonRef = useRef<HTMLButtonElement | null>(null);
  const runnersRef = useRef<RunnerView[]>(runners);
  runnersRef.current = runners;
  const projectRunnerIdsRef = useRef<string[]>([]);
  projectRunnerIdsRef.current = projectRunnerIds;
  const [pendingProjectSelection, setPendingProjectSelection] = useState<string | null>(null);
  const [pageAuthority] = useState(pageHostAuthority);
  const [selectionHydrated, setSelectionHydrated] = useState(false);
  const [viewportWidth, setViewportWidth] = useState(() => typeof window === 'undefined' ? 1280 : window.innerWidth);
  const [tabletContextOpen, setTabletContextOpen] = useState(false);
  const [lanRailOpen, setLanRailOpen] = useState(() => typeof window === 'undefined' ? true : window.innerWidth >= 720);
  const [tmuxTerminal, setTmuxTerminal] = useState<{ runnerId: string; runnerLabel: string; session: string; hidden?: boolean } | null>(null);

  const transportEpochRef = useRef(0);
  const closeProjectForm = useCallback(() => {
    setProjectFormOpen(false);
    setFormError('');
    setProjectRunnerIds([]);
    requestAnimationFrame(() => projectAddButtonRef.current?.focus());
  }, []);

  useEffect(() => registry.subscribe(setRunners), [registry]);
  useEffect(() => mappingStore.subscribe(setMapping), [mappingStore]);
  useEffect(() => discovery.subscribe(setDiscovered), [discovery]);
  useEffect(() => teamStore.subscribe(setTeam), [teamStore]);

  useEffect(
    () => mountChatTransports(
      transportEpochRef,
      () => {
        // Pairing QR: {origin}/chat#{token} — /web consumes the fragment in
        // App.tsx; /chat must too, or the page WS is refused and LAN
        // discovery never reaches the client (mobile pairing is dead).
        const fragment = window.location.hash.replace(/^#/, '');
        if (fragment !== '') {
          saveTokenForAuthority(storage, normalizeHostAuthority(window.location.host), fragment);
          window.history.replaceState(null, '', window.location.pathname + window.location.search);
        }
        registry.start();
        discovery.start();
        void runMappingSync(storage, pageAuthority, registry, mappingStore, 'boot', setFormError);
      },
      () => {
        teamStore.dispose();
        registry.stop();
        discovery.stop();
      },
    ),
    [discovery, mappingStore, pageAuthority, registry, storage, teamStore],
  );

  const runnerRecordKey = JSON.stringify({
    // View state (connecting/connected churn) must not retrigger sync: only
    // record data and token changes count as local mutations worth pushing.
    runners: runners.map((runner) => [
      runner.id, runner.label, runner.origin, runner.authority, runner.codename,
    ]),
    tokens: collectRunnerTokens(storage),
  });
  useEffect(() => {
    // Any local mutation means a push is pending: mark dirty before the
    // debounce so an in-flight sync skip, a failed push, or a cleared
    // timer all leave the pending-sync marker in the meta.
    const meta = readSyncMeta(storage);
    if (!meta.dirty) writeSyncMeta(storage, { ...meta, dirty: true });
    const handle = window.setTimeout(() => {
      void runMappingSync(storage, pageAuthority, registry, mappingStore, 'push', setFormError);
    }, 400);
    return () => window.clearTimeout(handle);
  }, [mapping.projects, mapping.mappings, pageAuthority, mappingStore, registry, storage, runnerRecordKey]);
  useEffect(() => {
    const syncRunnerRecords = (event: StorageEvent) => {
      if (event.key !== 'rpi-web-chat-runners:v1' && event.key !== CHAT_MAPPING_STORAGE_KEY) return;
      // Echoed bytes (newValue === oldValue) are not a foreign update: only a
      // genuinely different document reloads the page.
      if (event.newValue === null || event.newValue === event.oldValue) return;
      window.location.reload();
    };
    window.addEventListener('storage', syncRunnerRecords);
    return () => window.removeEventListener('storage', syncRunnerRecords);
  }, [storage]);

  useEffect(() => {
    teamStore.reconcileInputs(mapping.projects, mapping.mappings, runners);
  }, [mapping.mappings, mapping.projects, runners, teamStore]);

  useEffect(() => {
    if (selectionHydrated) return;
    const saved = loadChatRailFocus(storage, pageAuthority);
    if (saved?.kind === 'runner' && runners.some((runner) => runner.id === saved.id)) {
      setRailFocus({ kind: 'runner', id: saved.id });
      setSelectionHydrated(true);
      return;
    }
    if (mapping.projects.length === 0) return;
    const savedProjectId = saved?.kind === 'project' ? saved.id : loadSelectedProjectId(storage, pageAuthority);
    const chosen = selectStoredProject(mapping.projects, savedProjectId);
    if (chosen) {
      teamStore.selectProject(chosen.id);
      setRailFocus({ kind: 'project', id: chosen.id });
    }
    setSelectionHydrated(true);
  }, [mapping.projects, pageAuthority, runners, selectionHydrated, storage, teamStore]);

  useEffect(() => {
    if (!pendingProjectSelection) return;
    if (!mapping.projects.some((project) => project.id === pendingProjectSelection)) return;
    teamStore.selectProject(pendingProjectSelection);
    saveSelectedProjectId(storage, pageAuthority, pendingProjectSelection);
    setRailFocus({ kind: 'project', id: pendingProjectSelection });
    saveChatRailFocus(storage, pageAuthority, { kind: 'project', id: pendingProjectSelection });
    if (typeof window !== 'undefined' && window.innerWidth < 720) teamStore.setMobilePane('conversation');
    setPendingProjectSelection(null);
  }, [mapping.projects, pageAuthority, pendingProjectSelection, storage, teamStore]);

  useEffect(() => {
    const onResize = () => setViewportWidth(window.innerWidth);
    window.addEventListener('resize', onResize);
    return () => window.removeEventListener('resize', onResize);
  }, []);

  const projects = useMemo(() => sortedProjects(mapping.projects), [mapping.projects]);
  const selectedProjectId = team.selectedProjectId ?? '';
  const selectedProject = useMemo(
    () => projects.find((project) => project.id === selectedProjectId) ?? null,
    [projects, selectedProjectId],
  );

  useEffect(() => {
    if (!selectionHydrated || !selectedProjectId) return;
    saveSelectedProjectId(storage, pageAuthority, selectedProjectId);
  }, [pageAuthority, selectedProjectId, selectionHydrated, storage]);

  useEffect(() => {
    if (!selectionHydrated) return;
    saveChatRailFocus(storage, pageAuthority, railFocus);
  }, [pageAuthority, railFocus, selectionHydrated, storage]);

  useEffect(() => {
    if (railFocus?.kind !== 'runner') return;
    if (runners.some((runner) => runner.id === railFocus.id)) return;
    if (selectedProjectId) {
      setRailFocus({ kind: 'project', id: selectedProjectId });
      return;
    }
    setRailFocus(null);
  }, [railFocus, runners, selectedProjectId]);

  const focusedRunner = railFocus?.kind === 'runner'
    ? runners.find((runner) => runner.id === railFocus.id) ?? null
    : null;
  const channel = selectedProjectId ? team.channels?.[selectedProjectId] ?? null : null;
  const projectMappings = useMemo(
    () => selectedProject
      ? mapping.mappings.filter((candidate) => candidate.projectId === selectedProject.id)
      : [],
    [mapping.mappings, selectedProject],
  );
  const activeMappings = useMemo(
    () => projectMappings.filter((candidate): candidate is ActiveMapping => candidate.state === 'active')
      .sort((left, right) => left.createdAt - right.createdAt || left.id.localeCompare(right.id)),
    [projectMappings],
  );
  const headerMappings = useMemo(
    () => projectMappings.filter((candidate): candidate is ActiveMapping | DeletingMapping | RollbackPendingMapping => (
      candidate.state === 'active' || candidate.state === 'deleting' || candidate.state === 'rollback_pending'
    )).sort((left, right) => left.createdAt - right.createdAt || left.id.localeCompare(right.id)),
    [projectMappings],
  );
  const inviteLobbyOpen = Boolean(selectedProject && headerMappings.length === 0);
  const members = useMemo(
    () => selectedProject
      ? activeProjectMembers(selectedProject.id, mapping.mappings, runners, team.sessions)
      : [],
    [mapping.mappings, runners, selectedProject, team.sessions],
  );
  const target = channel?.target ?? null;
  const conversationTarget = useMemo(() => {
    if (!selectedProject || target?.kind !== 'runner') return null;
    const mappingRow = activeMappings.find((candidate) => candidate.id === target.mappingId);
    const runner = mappingRow ? runners.find((candidate) => candidate.id === mappingRow.runnerId) : undefined;
    if (!mappingRow || !runner) return null;
    return { runner, mapping: mappingRow };
  }, [activeMappings, runners, selectedProject, target]);

  const activeSession = useMemo(() => {
    if (!selectedProject || !conversationTarget) return null;
    return sessionBindingForMapping(selectedProject, conversationTarget.runner, conversationTarget.mapping);
  }, [conversationTarget, selectedProject]);
  // The authoritative turn of the session this view owns. SessionView is
  // keyed by [runner, session], so returning to a still-streaming session
  // remounts it; the injected session subscription does not replay, so the
  // view has to be handed this state or its activity strip comes back empty.
  const activeSessionState = useMemo(() => {
    if (!activeSession) return null;
    return members.find((member) => member.mapping.id === activeSession.mapping.id)?.session ?? null;
  }, [activeSession, members]);
  const teamAggregate = useMemo(
    () => deriveProjectAggregate(selectedProject?.id ?? '', team.sessions),
    [selectedProject, team.sessions],
  );
  const addedOrigins = useMemo(() => runners.map((runner) => runner.origin), [runners]);
  const lanCandidates = useMemo(
    () => discovered.filter((runner) => !discoveredAlreadyAdded(runner, addedOrigins)),
    [addedOrigins, discovered],
  );

  useEffect(() => {
    if ((!formOpen && !inviteLobbyOpen && !runnerFormOpen) || lanCandidates.length === 0) {
      setInviteDiscoveryStatus({});
      return;
    }
    const abort = new AbortController();
    const pageProtocol = typeof location !== 'undefined' && location.protocol === 'https:' ? 'https:' : 'http:';
    setInviteDiscoveryStatus(Object.fromEntries(lanCandidates.map((candidate) => [candidate.origin, 'connecting'])));
    void Promise.all(lanCandidates.map(async (candidate) => {
      const status = await fetchInviteCandidateStatus(candidate.origin, fetch, pageProtocol, abort.signal)
        .catch((): InviteCandidateReach => ({ reachable: false }));
      if (abort.signal.aborted) return;
      setInviteDiscoveryStatus((current) => ({ ...current, [candidate.origin]: status }));
    }));
    return () => abort.abort();
  }, [formOpen, inviteLobbyOpen, lanCandidates, runnerFormOpen]);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return;
      if (workspaceMenuOpen) {
        setWorkspaceMenuOpen(false);
        workspaceMenuButtonRef.current?.focus();
        return;
      }
      if (tmuxTerminal && !tmuxTerminal.hidden) return;
      if (runnerContextMenu) {
        setRunnerContextMenu(null);
        return;
      }
      if (confirmRunnerDelete) {
        setConfirmRunnerDelete(false);
        return;
      }
      if (confirmProjectDelete) {
        setConfirmProjectDelete(false);
        return;
      }
      if (formOpen) {
        setFormOpen(false);
        setFormError('');
        inviteButtonRef.current?.focus();
        return;
      }
      if (tokenFormRunnerId) {
        setTokenFormRunnerId(null);
        setTokenDraft('');
        setFormError('');
        return;
      }
      if (createSessionOpen) {
        setCreateSessionOpen(false);
        setCreateSessionName('');
        setFormError('');
        return;
      }
      if (runnerFormOpen) {
        setRunnerFormOpen(false);
        setFormError('');
        runnerAddButtonRef.current?.focus();
        return;
      }


      if (runnerGearOpen) {
        setRunnerGearOpen(false);
        return;
      }
      if (gearOpen) {
        setGearOpen(false);
        gearButtonRef.current?.focus();
        return;
      }
      if (projectFormOpen) {
        closeProjectForm();
        return;
      }
      if (tabletContextOpen) {
        setTabletContextOpen(false);
        detailsButtonRef.current?.focus();
      }
    };
    document.addEventListener('keydown', onKeyDown, true);
    return () => document.removeEventListener('keydown', onKeyDown, true);
  }, [closeProjectForm, confirmProjectDelete, confirmRunnerDelete, createSessionOpen, formOpen, gearOpen, projectFormOpen, runnerContextMenu, runnerFormOpen, runnerGearOpen, tabletContextOpen, tmuxTerminal, tokenFormRunnerId, workspaceMenuOpen]);

  useEffect(() => {
    if (!workspaceMenuOpen) return;
    const onPointerDown = (event: PointerEvent) => {
      const root = workspaceMenuRef.current;
      if (!root || root.contains(event.target as Node)) return;
      setWorkspaceMenuOpen(false);
    };
    document.addEventListener('pointerdown', onPointerDown);
    return () => document.removeEventListener('pointerdown', onPointerDown);
  }, [workspaceMenuOpen]);

  useEffect(() => {
    if (!runnerContextMenu) return;
    const onPointerDown = (event: PointerEvent) => {
      const root = runnerContextMenuRef.current;
      if (!root || root.contains(event.target as Node)) return;
      setRunnerContextMenu(null);
    };
    document.addEventListener('pointerdown', onPointerDown);
    return () => document.removeEventListener('pointerdown', onPointerDown);
  }, [runnerContextMenu]);



  useEffect(() => {
    if (!gearOpen) return;
    gearMenuRef.current?.querySelector<HTMLElement>('button')?.focus();
  }, [gearOpen]);
  const visibleProjects = useMemo(() => {
    const query = projectSearch.trim().toLowerCase();
    if (!query) return projects;
    return projects.filter((project) => (
      project.label.toLowerCase().includes(query) || project.cwd.toLowerCase().includes(query)
    ));
  }, [projectSearch, projects]);
  const visibleRunners = useMemo(() => {
    const query = runnerSearch.trim().toLowerCase();
    const ordered = sortRunnersOnlineFirst(dedupeRunnersKeepHealthiest(runners));
    if (!query) return ordered;
    return ordered.filter((runner) => runnerDisplayName(runner).toLowerCase().includes(query));
  }, [runnerSearch, runners]);
  const activeBroadcast = useMemo(() => {
    if (!channel?.activeBroadcastRunId) return null;
    return channel.broadcastRuns.find((run) => run.id === channel.activeBroadcastRunId) ?? null;
  }, [channel]);
  const displayedBroadcast = activeBroadcast ?? channel?.broadcastRuns.at(-1) ?? null;
  const mappingBusy = useMemo(() => {
    if (!activeSession || !activeBroadcast) return false;
    const card = activeBroadcast.responses.find((response) => response.mappingId === activeSession.mapping.id);
    const attempt = card?.attempts.find((row) => row.id === card.currentAttemptId);
    return attempt?.status === 'dispatching' || attempt?.status === 'streaming' || attempt?.status === 'cancelling';
  }, [activeBroadcast, activeSession]);
  const contextPane: ContextPane = channel?.contextPane ?? 'files';
  const compact = viewportWidth < 1100;
  const mobile = viewportWidth < 720;
  const mobilePane = mobile && projects.length === 0 && runners.length === 0
    ? 'conversation'
    : (team.mobilePane ?? 'projects');

  const sendCommandForBinding = useCallback(async (
    owner: ChatSessionBinding | null,
    command: RunnerCommand,
  ): Promise<unknown> => {
    const options = chatFileExplorerSendOptions(owner);
    if (!options) throw new Error('Session is unavailable.');
    const response = await registry.command(options.runnerId, command, {
      sourceSessionId: options.sourceSessionId,
    });
    return response.data;
  }, [registry]);

  const sendSessionCommand = useCallback(async (command: RunnerCommand): Promise<unknown> => {
    return sendCommandForBinding(activeSession, command);
  }, [activeSession, sendCommandForBinding]);

  const patchFileExplorer = useCallback((mappingId: string, updater: (slot: FileExplorerSessionSlot) => FileExplorerSessionSlot) => {
    setFileExplorerByMappingId((prev) => updateFileExplorerSlot(prev, mappingId, updater).slots);
  }, []);

  // A mapping that left the stored document entirely is an explicit eviction:
  // its slot KEY is deleted in one pure state update (an emptied-but-retained
  // slot would re-trigger this effect forever); the committed-slot diff
  // effect below then closes its conversations through the binding captured
  // before the removal. The victim is marked pending FIRST so the capture
  // rebuild can never prune its binding between the eviction and the close
  // settle. Bindings rebuild AFTER the eviction pass: current mappings are
  // re-captured, removed mappings' captures survive while a close is still
  // in flight, and everything unused is pruned.
  useEffect(() => {
    const absent = Object.keys(fileExplorerByMappingId)
      .filter((mappingId) => !mapping.mappings.some((candidate) => candidate.id === mappingId));
    if (absent.length > 0) {
      for (const mappingId of absent) {
        if ((fileExplorerByMappingId[mappingId]?.tabs ?? []).some((tab) => tab.conversationId)) {
          fileExplorerClosePendingRef.current.add(mappingId);
        }
      }
      setFileExplorerByMappingId((prev) => {
        const next = { ...prev };
        let dropped = false;
        for (const mappingId of absent) {
          if (mappingId in next) {
            delete next[mappingId];
            dropped = true;
          }
        }
        return dropped ? next : prev;
      });
    }
    const capture: Record<string, { runnerId: string; sourceSessionId: string }> = {};
    for (const candidate of mapping.mappings) {
      if (candidate.state !== 'active' && candidate.state !== 'deleting' && candidate.state !== 'rollback_pending') continue;
      capture[candidate.id] = { runnerId: candidate.runnerId, sourceSessionId: candidate.sessionId };
    }
    for (const [ownerKey, binding] of Object.entries(fileExplorerBindingCaptureRef.current)) {
      if (!(ownerKey in capture) && fileExplorerClosePendingRef.current.has(ownerKey)) {
        capture[ownerKey] = binding;
      }
    }
    fileExplorerBindingCaptureRef.current = capture;
  }, [fileExplorerByMappingId, mapping.mappings]);


  const onMarkdownOpenPath = useCallback((path: string, line: number | undefined, owner: MarkdownPathOwner, completion?: string) => {
    const ownerKey = chatMarkdownExplorerOwnerKey(owner, path);
    const ownerBinding = sessionBindingForOwner(mapping.projects, runners, mapping.mappings, owner);
    if (!ownerKey || !ownerBinding) return;
    const attempt = beginMarkdownProbeAttempt(markdownProbeAttemptRef);
    const isCurrentAttempt = attempt.isCurrent;
    runChatMarkdownOpenProbe({
      path,
      line,
      owner,
      ownerBinding,
      workspaceCwd: ownerBinding.project.cwd,
      retried: false,
      isCurrentAttempt,
      completion,
      send: (runnerId, command, options) => registry.command(runnerId, command, options),
      onProbeStarted: () => { if (isCurrentAttempt()) setMarkdownProbeError(''); },
      onOpened: (relative, openLine) => {
        if (!isCurrentAttempt()) return;
        setMarkdownProbeError('');
        teamStore.selectProject(ownerBinding.project.id);
        teamStore.setFileOwner(ownerBinding.project.id, ownerBinding.mapping.id);
        teamStore.setContextPane(ownerBinding.project.id, 'files');
        patchFileExplorer(ownerKey, (slot) => applyOpenPath(slot, relative, openLine));
      },
      onFailed: (error) => {
        if (!isCurrentAttempt()) return;
        setMarkdownProbeError(chatMarkdownOpenFailureText(error, path));
      },
    });
  }, [mapping, patchFileExplorer, registry, runners, selectedProjectId, teamStore]);
  useEffect(() => subscribeMarkdownOpenPath(onMarkdownOpenPath), [onMarkdownOpenPath]);

  const openFileExplorer = useCallback(() => {
    if (!selectedProjectId) return;
    teamStore.setContextPane(selectedProjectId, 'files');
    const ownerId = channel?.fileOwnerMappingId ?? activeMappings[0]?.id ?? '';
    const ownerMapping = activeMappings.find((candidate) => candidate.id === ownerId) ?? activeMappings[0];
    if (!ownerMapping) return;
    setFileExplorerByMappingId((prev) => applyChatFileExplorerOpen(prev, ownerMapping.id));
  }, [activeMappings, channel, selectedProjectId, teamStore]);

  const reportSessionTransportError = useCallback((message: string) => setFormError(message), []);

  const fileOwnerMapping = useMemo(() => {
    const ownerId = channel?.fileOwnerMappingId ?? '';
    return activeMappings.find((candidate) => candidate.id === ownerId)
      ?? activeMappings[0]
      ?? null;
  }, [activeMappings, channel]);
  const fileExplorerOwner = useMemo(() => {
    if (!selectedProject || !fileOwnerMapping) return null;
    const runner = runners.find((candidate) => candidate.id === fileOwnerMapping.runnerId);
    if (!runner) return null;
    return sessionBindingForMapping(selectedProject, runner, fileOwnerMapping);
  }, [fileOwnerMapping, runners, selectedProject]);
  const fileExplorerMappingId = fileExplorerOwner?.mapping.id ?? null;
  const fileExplorerSessionId = fileExplorerOwner?.mapping.sessionId ?? null;
  const fileExplorerSlot = readFileExplorerSlot(fileExplorerByMappingId, fileExplorerMappingId);
  const fileExplorerOpen = contextPane === 'files' && fileExplorerSlot.mode !== 'closed';
  const sendFileExplorerCommand = useCallback(async (command: RunnerCommand): Promise<unknown> => {
    return sendCommandForBinding(fileExplorerOwner, command);
  }, [fileExplorerOwner, sendCommandForBinding]);
  // Conversation-close plumbing: the send routes through the VICTIM
  // mapping's captured original binding (runnerId + sourceSessionId), never
  // the currently displayed mapping; a missing capture throws so the close
  // surfaces as an error instead of returning undefined success. The pending
  // marker drops when the close settles so the capture rebuild can prune the
  // no-longer-needed binding.
  const sendFileConversationClose = useCallback((command: Record<string, unknown>, ownerKey: string): Promise<unknown> => {
    const binding = fileExplorerBindingCaptureRef.current[ownerKey];
    if (!binding) throw new Error(`no captured session binding for ${ownerKey}`);
    return registry.command(binding.runnerId, command as RunnerCommand, { sourceSessionId: binding.sourceSessionId })
      .then((response) => response.data)
      .finally(() => {
        fileExplorerClosePendingRef.current.delete(ownerKey);
        // Explicit prune at settle (no new state mirror): the stable store
        // snapshot tells whether the owner mapping still exists; a removed
        // mapping's binding is unused the moment its close settles.
        const snapshot = mappingStore.snapshot();
        if (!snapshot.mappings.some((candidate) => candidate.id === ownerKey)) {
          delete fileExplorerBindingCaptureRef.current[ownerKey];
        }
      });
  }, [mappingStore, registry]);

  // Invariant: layout-phase admission must precede every child passive read begin, or a fresh tab's first response is discarded.
  useLayoutEffect(() => {
    fileExplorerSyncPageCacheWithSlots(fileExplorerByMappingId);
  }, [fileExplorerByMappingId]);
  // Committed-slot diff → conversation closes (the App twin holds the full
  // rationale: committed ref written only here, pure updater, prev-owned
  // identities, StrictMode repeat sees no new removals). Closes route
  // through each victim's captured original binding; only the exact typed
  // already-closed outcome is nonfatal — every other failure reaches the
  // central form error layer exactly once.
  useEffect(() => {
    const prev = fileExplorerCommittedSlotsRef.current;
    const next = fileExplorerByMappingId;
    if (prev === next) return;
    const droppedPathsBySession: Record<string, string[]> = {};
    for (const [ownerKey, prevSlot] of Object.entries(prev)) {
      const kept = new Set<string>();
      const nextSlot = next[ownerKey];
      if (nextSlot) {
        for (const tab of nextSlot.tabs) {
          kept.add(`${fileExplorerCanonicalPath(tab.path)}\u0000${tab.conversationId ?? ''}`);
        }
      }
      const dropped: string[] = [];
      for (const tab of prevSlot.tabs) {
        const path = fileExplorerCanonicalPath(tab.path);
        // Identity pair, not path alone: a same-path replacement holding a
        // DIFFERENT (or no) conversation id must retire the old identity.
        if (kept.has(`${path}\u0000${tab.conversationId ?? ''}`)) continue;
        if (!dropped.includes(path)) dropped.push(path);
      }
      if (dropped.length > 0) droppedPathsBySession[ownerKey] = dropped;
    }
    fileExplorerCommittedSlotsRef.current = next;
    if (Object.keys(droppedPathsBySession).length === 0) return;
    closeFileConversationsDroppedBySlotPatch(
      prev,
      { slots: next, evictedSessions: [], droppedPathsBySession },
      sendFileConversationClose,
      (message) => setFormError(`file conversation close: ${message}`),
    );
  }, [fileExplorerByMappingId, sendFileConversationClose]);

  const sessionEvents = useCallback((callback: (frame: unknown) => void) => {
    if (!activeSession) return () => {};
    return teamStore.subscribeSession(activeSession.runner.id, activeSession.mapping.sessionId, callback);
  }, [activeSession, teamStore]);

  // SessionView's subscription handoff reads this once its listener is
  // registered: the subscription never replays, so the session state is the
  // only place a turn that started or ended in that window is still visible.
  // Read live from the store and bound to the exact runner/session the view
  // subscribes to — never the rendered `activeSessionState`, which a later
  // commit can replace after the session state has already moved on.
  const readSessionAuthority = useCallback((): SessionAuthority | null => {
    if (!activeSession) return null;
    const session = teamStore.snapshot().sessions[
      runnerSessionKey(activeSession.runner.id, activeSession.mapping.sessionId)
    ];
    if (!session || session.mappingId !== activeSession.mapping.id) return null;
    return { activeTurnId: session.activeTurnId, isStreaming: session.isStreaming };
  }, [activeSession, teamStore]);


  const selectProject = (projectId: string) => {
    setGearOpen(false);
    setFormOpen(false);
    setRunnerFormOpen(false);
    setConfirmProjectDelete(false);
    setConfirmRunnerDelete(false);
    setTabletContextOpen(false);
    setTmuxTerminal(null);
    setRunnerContextMenu(null);
    setRailFocus({ kind: 'project', id: projectId });
    saveChatRailFocus(storage, pageAuthority, { kind: 'project', id: projectId });
    teamStore.selectProject(projectId);
    saveSelectedProjectId(storage, pageAuthority, projectId);
    if (typeof window !== 'undefined' && window.innerWidth < 720) teamStore.setMobilePane('conversation');
  };

  const selectRunner = (runnerId: string) => {
    setGearOpen(false);
    setFormOpen(false);
    setRunnerFormOpen(false);
    setTabletContextOpen(false);
    setTmuxTerminal(null);
    setCreateSessionOpen(false);
    setCreateSessionName('');
    setTokenFormRunnerId(null);
    setConfirmRunnerDelete(false);
    setConfirmProjectDelete(false);
    setTokenDraft('');
    setRunnerContextMenu(null);
    setRailFocus({ kind: 'runner', id: runnerId });
    saveChatRailFocus(storage, pageAuthority, { kind: 'runner', id: runnerId });
    if (typeof window !== 'undefined' && window.innerWidth < 720) teamStore.setMobilePane('conversation');
  };

  const clearRunnerMenuLongPress = () => {
    const pending = runnerMenuLongPressRef.current;
    if (!pending) return;
    window.clearTimeout(pending.timer);
    runnerMenuLongPressRef.current = null;
  };

  const copyRunnerWebUrl = (url: string) => {
    if (typeof navigator === 'undefined' || typeof navigator.clipboard?.writeText !== 'function') {
      setFormError('Clipboard is unavailable.');
      return;
    }
    void navigator.clipboard.writeText(url).then(
      () => {},
      () => {
        setFormError('Could not copy the web URL.');
      },
    );
  };

  const runRunnerContextAction = (action: RunnerContextAction, runner: RunnerView) => {
    const webUrl = runnerTrustHref(runner, storage);
    applyRunnerContextAction(action, webUrl, {
      openTerminal: () => {
        selectRunner(runner.id);
      },
      openWeb: (url) => {
        window.open(url, '_blank', 'noopener');
      },
      openSettings: () => {
        selectRunner(runner.id);
        setRunnerGearOpen(true);
      },
      copyWebUrl: copyRunnerWebUrl,
    });
    setRunnerContextMenu(null);
  };

  const openRunnerAdd = () => {
    setFormError('');
    setGearOpen(false);
    setFormOpen(false);
    setRunnerFormOpen(true);
    setConfirmRunnerDelete(false);
    setConfirmProjectDelete(false);
  };


  const selectTarget = (next: ChannelTarget) => {
    if (!selectedProjectId) return;
    setTmuxTerminal(null);
    teamStore.selectTarget(selectedProjectId, next);
  };

  const openInvite = () => {
    setFormError('');
    setGearOpen(false);
    setConfirmRunnerDelete(false);
    setConfirmProjectDelete(false);
    if (inviteLobbyOpen) {
      document.getElementById('chat-runner-address')?.focus();
      return;
    }
    setFormOpen(true);
  };

  const closeInviteOverlay = () => {
    setFormOpen(false);
    setFormError('');
    inviteButtonRef.current?.focus();
  };

  const inviteExisting = async (runnerId: string): Promise<boolean> => {
    if (!selectedProject) return false;
    if (inviteBusyRef.current) return false;
    inviteBusyRef.current = true;
    setInviteBusyKey(runnerId);
    setFormError('');
    try {
      await mappingStore.mapProject(selectedProject.id, runnerId);
      setFormOpen(false);
      return true;
    } catch (error) {
      setFormError(error instanceof Error ? error.message : String(error));
      return false;
    } finally {
      inviteBusyRef.current = false;
      setInviteBusyKey(null);
    }
  };

  const addRunner = async (input: { label: string; address: string; token: string }): Promise<boolean> => {
    if (isAddingRef.current) return false;
    isAddingRef.current = true;
    setInviteBusyKey(input.address);
    setFormError('');
    try {
      const runner = await registry.add(input.label, input.address, input.token);
      setRunnerFormOpen(false);
      selectRunner(runner.id);
      return true;
    } catch (error) {
      setFormError(error instanceof Error ? error.message : String(error));
      return false;
    } finally {
      isAddingRef.current = false;
      setInviteBusyKey(null);
    }
  };

  const saveIndependentToken = async (runnerId: string, nextToken: string): Promise<boolean> => {
    if (inviteBusyRef.current) return false;
    inviteBusyRef.current = true;
    setInviteBusyKey(runnerId);
    setFormError('');
    try {
      await registry.updateToken(runnerId, nextToken);
      setRunnerFormOpen(false);
      setTokenFormRunnerId(null);
      setTokenDraft('');
      return true;
    } catch (error) {
      setFormError(error instanceof Error ? error.message : String(error));
      return false;
    } finally {
      inviteBusyRef.current = false;
      setInviteBusyKey(null);
    }
  };


  const addAndInvite = async (input: { label: string; address: string; token: string }): Promise<boolean> => {
    if (!selectedProject) return false;
    if (isAddingRef.current) return false;
    isAddingRef.current = true;
    setInviteBusyKey(input.address);
    setFormError('');
    try {
      const runner = await registry.add(input.label, input.address, input.token);
      await mappingStore.mapProject(selectedProject.id, runner.id);
      setFormOpen(false);
      return true;
    } catch (error) {
      setFormError(error instanceof Error ? error.message : String(error));
      return false;
    } finally {
      isAddingRef.current = false;
      setInviteBusyKey(null);
    }
  };

  const saveTokenAndInvite = async (runnerId: string, nextToken: string): Promise<boolean> => {
    if (!selectedProject) return false;
    if (inviteBusyRef.current) return false;
    inviteBusyRef.current = true;
    setInviteBusyKey(runnerId);
    setFormError('');
    try {
      await registry.updateToken(runnerId, nextToken);
      await mappingStore.mapProject(selectedProject.id, runnerId);
      setFormOpen(false);
      return true;
    } catch (error) {
      setFormError(error instanceof Error ? error.message : String(error));
      return false;
    } finally {
      inviteBusyRef.current = false;
      setInviteBusyKey(null);
    }
  };
  const removeRunner = async (runnerId: string) => {
    if (inviteBusyRef.current) return;
    inviteBusyRef.current = true;
    setInviteBusyKey(`remove:${runnerId}`);
    setFormError('');
    try {
      await mappingStore.removeRunner(runnerId, () => registry.delete(runnerId));
      setFormOpen(false);
      setRunnerFormOpen(false);
      if (railFocus?.kind === 'runner' && railFocus.id === runnerId) {
        setRailFocus(selectedProjectId ? { kind: 'project', id: selectedProjectId } : null);
      }
    } catch (error) {
      setFormError(safeText(error instanceof Error ? error.message : String(error)));
    } finally {
      inviteBusyRef.current = false;
      setInviteBusyKey(null);
    }
  };
  const projectFormController = useMemo(
    () => createChatProjectFormController({
      addProject: (label, cwd) => mappingStore.addProject(label, cwd),
      onBusy: setIsProjectBusy,
      onError: setFormError,
      onAccepted: (project) => {
        const selected = projectRunnerIdsRef.current;
        setProjectLabel('');
        setProjectCwd('');
        setProjectFormOpen(false);
        setProjectRunnerIds([]);
        setPendingProjectSelection(project.id);
        void (async () => {
          for (const runnerId of selected) {
            const runner = runnersRef.current.find((row) => row.id === runnerId);
            if (!runner || runner.state !== 'connected') continue;
            try {
              await mappingStore.mapProject(project.id, runnerId);
            } catch (error) {
              setFormError(error instanceof Error ? error.message : String(error));
            }
          }
        })();
      },
    }),
    [mappingStore],
  );
  const addProject = (event: React.FormEvent<HTMLFormElement>) => {
    // The production controller owns the busy gate and the error slot; the
    // submission carries the render-committed label/cwd state.
    void projectFormController.submit(event, { label: projectLabel, cwd: projectCwd });
  };

  const projectCompletionBinding = useMemo(
    () => chatProjectCompletionBinding(activeSession, mapping.projects, runners, mapping.mappings),
    [activeSession, mapping.mappings, mapping.projects, runners],
  );
  const projectCompletionBindingRef = useRef<ChatSessionBinding | null>(null);
  projectCompletionBindingRef.current = projectCompletionBinding;
  const sendProjectPathComplete = useMemo(
    () => createChatProjectPathComplete({
      bindingRef: projectCompletionBindingRef,
      command: (runnerId, command, options) => registry.command(runnerId, command, options),
    }),
    [registry],
  );

  const inviteCandidates = useMemo((): InviteRunnerCandidate[] => {
    const invitedRunnerIds = new Set(
      mapping.mappings
        .filter((candidate) => candidate.projectId === selectedProjectId)
        .map((candidate) => candidate.runnerId),
    );
    const rows: InviteRunnerCandidate[] = runners.map((runner) => {
      const invited = invitedRunnerIds.has(runner.id);
      let state: InviteRunnerCandidate['state'] = 'offline';
      if (invited) state = 'invited';
      else if (runner.state === 'connected') state = 'online';
      else if (runner.state === 'token_required') state = 'token_required';
      else if (runner.state === 'connecting' || runner.state === 'reconnecting') state = 'connecting';
      return {
        key: runner.id,
        runnerId: runner.id,
        label: runner.label,
        address: runner.origin,
        version: runner.status?.version ?? null,
        codename: runner.codename,
        state,
        error: boundedVisibleRunnerError(runner.error),
      };
    });
    for (const discoveredRunner of lanCandidates) {
      const reach = inviteDiscoveryStatus[discoveredRunner.origin] ?? 'connecting';
      const state: InviteRunnerCandidate['state'] = reach === 'connecting'
        ? 'connecting'
        : !reach.reachable
          ? 'offline'
          : reach.status.tokenRequired
            ? 'token_required'
            : 'online';
      rows.push({
        key: discoveredRunner.origin,
        runnerId: null,
        label: discoveredRunner.label,
        address: discoveredRunner.origin,
        version: reach !== 'connecting' && reach.reachable ? reach.status.version : discoveredRunner.version,
        codename: reach !== 'connecting' && reach.reachable ? reach.status.codename : discoveredRunner.codename,
        state,
        error: typeof reach !== 'string' && !reach.reachable ? RUNNER_UNREACHABLE_MESSAGE : '',
      });
    }
    return rows;
  }, [inviteDiscoveryStatus, lanCandidates, mapping.mappings, runners, selectedProjectId]);

  const independentCandidates = useMemo((): InviteRunnerCandidate[] => {
    const rows: InviteRunnerCandidate[] = runners.map((runner) => {
      let state: InviteRunnerCandidate['state'] = 'offline';
      if (runner.state === 'connected') state = 'online';
      else if (runner.state === 'token_required') state = 'token_required';
      else if (runner.state === 'connecting' || runner.state === 'reconnecting') state = 'connecting';
      return {
        key: runner.id,
        runnerId: runner.id,
        label: runner.label,
        address: runner.origin,
        version: runner.status?.version ?? null,
        codename: runner.codename,
        state,
        error: boundedVisibleRunnerError(runner.error),
      };
    });
    for (const discoveredRunner of lanCandidates) {
      const reach = inviteDiscoveryStatus[discoveredRunner.origin] ?? 'connecting';
      const state: InviteRunnerCandidate['state'] = reach === 'connecting'
        ? 'connecting'
        : !reach.reachable
          ? 'offline'
          : reach.status.tokenRequired
            ? 'token_required'
            : 'online';
      rows.push({
        key: discoveredRunner.origin,
        runnerId: null,
        label: discoveredRunner.label,
        address: discoveredRunner.origin,
        version: reach !== 'connecting' && reach.reachable ? reach.status.version : discoveredRunner.version,
        codename: reach !== 'connecting' && reach.reachable ? reach.status.codename : discoveredRunner.codename,
        state,
        error: typeof reach !== 'string' && !reach.reachable ? RUNNER_UNREACHABLE_MESSAGE : '',
      });
    }
    return rows;
  }, [inviteDiscoveryStatus, lanCandidates, runners]);
  const lanRailCandidates = useMemo(
    () => independentCandidates.filter((candidate) => candidate.runnerId === null),
    [independentCandidates],
  );

  const onlineCount = members.filter((member) => member.runner.state === 'connected').length;
  const connectionSummary = activeMappings.length === 0
    ? 'Offline'
    : onlineCount === members.length && members.length > 0
      ? 'Online'
      : onlineCount === 0
        ? 'Offline'
        : 'Mixed';
  const selectedRunnerError = conversationTarget
    ? boundedVisibleRunnerError(conversationTarget.runner.error)
    : '';
  const inviteFields = selectedProject === null ? null : {
    project: selectedProject,
    candidates: inviteCandidates,
    busyKey: inviteBusyKey,
    onError: setFormError,
    onInviteExisting: inviteExisting,
    onAddAndInvite: addAndInvite,
    onSaveTokenAndInvite: saveTokenAndInvite,
    onRemoveRunner: removeRunner,
    onClose: closeInviteOverlay,
  };

  const sessionToast = formError || team.channelLogError || mapping.storageError;
  const visibleSessionToast = formError !== ''
    ? formError
    : (dismissedToast === sessionToast ? '' : sessionToast);
  const independentAddForm = runnerFormOpen ? (
    <AddRunnerForm
      placement={runners.length === 0 ? 'lobby' : 'overlay'}
      busyKey={inviteBusyKey}
      error={formError}
      onError={setFormError}
      onAdd={addRunner}
      onClose={() => {
        setRunnerFormOpen(false);
        setFormError('');
        runnerAddButtonRef.current?.focus();
      }}
    />
  ) : null;
  const tmuxOverlay = tmuxTerminal ? (
    <TmuxTerminal
      key={`${tmuxTerminal.runnerId}:${tmuxTerminal.session}`}
      runnerId={tmuxTerminal.runnerId}
      runnerLabel={tmuxTerminal.runnerLabel}
      session={tmuxTerminal.session}
      authority={runners.find((runner) => runner.id === tmuxTerminal.runnerId)?.authority ?? ''}
      send={(command, options) => registry.command(tmuxTerminal.runnerId, command, options)}
      onEvent={(callback) => subscribeRunnerEvents(tmuxTerminal.runnerId, callback)}
      onClose={() => setTmuxTerminal((current) => (current ? { ...current, hidden: true } : null))}
      onError={setFormError}
      loadToken={(hostAuthority) => loadTokenForAuthority(storage, hostAuthority)}
      hidden={tmuxTerminal.hidden}
    />
  ) : null;
  const shellClass = [
    'chat-app',
    'chat-app--projects',
    compact ? 'chat-app--compact' : '',
    mobile ? `chat-app--mobile chat-app--mobile-${mobilePane}` : '',
  ].filter(Boolean).join(' ');

  const toggleNewProject = () => {
    setFormError('');
    setProjectFormOpen((open) => !open);
  };
  const toggleProjectSettings = () => {
    setFormOpen(false);
    setConfirmRunnerDelete(false);
    setConfirmProjectDelete(false);
    setGearOpen((open) => !open);
  };
  const toggleRunnerSettings = () => {
    setCreateSessionOpen(false);
    setTokenFormRunnerId(null);
    setConfirmRunnerDelete(false);
    setRunnerGearOpen((open) => !open);
  };
  useDesktopActions({
    'new-workspace': toggleNewProject,
    settings: focusedRunner ? toggleRunnerSettings : selectedProject ? toggleProjectSettings : undefined,
  });

  return (
    <main ref={chatRootRef} className={shellClass} id="chat-projects">
      <ReconnectOverlay visible={reconnectOverlayVisible(conversationTarget?.runner.state ?? 'off')} text={reconnectOverlayText(conversationTarget?.runner.state ?? 'off')} />
      <header className="chat-workspace-bar" id="chat-workspace-bar">
        <div className="brand">
          rpi<span className="brand-sub">chat</span>
        </div>
        <div className="chat-workspace-bar__menu" ref={workspaceMenuRef}>
          <button
            ref={workspaceMenuButtonRef}
            type="button"
            id="chat-workspace-menu-btn"
            className={workspaceMenuOpen ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
            aria-pressed={workspaceMenuOpen}
            aria-expanded={workspaceMenuOpen}
            aria-controls="chat-workspace-menu"
            title="Toggle the workspace menu"
            aria-label={workspaceMenuOpen ? 'Close menu' : 'Open menu'}
            onClick={() => setWorkspaceMenuOpen((open) => !open)}
          >
            ☰
          </button>
          {workspaceMenuOpen && (
            <nav id="chat-workspace-menu" className="chat-workspace-menu" aria-label="Workspace menu">
              <a
                className="panel-toggle"
                href="/web"
                title="Open web"
                aria-label="Open web"
              >
                Web
              </a>
            </nav>
          )}
        </div>
      </header>
      {projectFormOpen && (
        <ChatProjectForm
          projectLabel={projectLabel}
          setProjectLabel={setProjectLabel}
          projectCwd={projectCwd}
          setProjectCwd={setProjectCwd}
          addProject={addProject}
          closeProjectForm={closeProjectForm}
          sendProjectPathComplete={sendProjectPathComplete}
          isProjectBusy={isProjectBusy}
          storageError={mapping.storageError}
          runners={runners.map((runner) => ({
            id: runner.id,
            label: runnerDisplayName(runner),
            connected: runner.state === 'connected',
          }))}
          selectedRunnerIds={projectRunnerIds}
          onToggleRunner={(runnerId) => {
            setProjectRunnerIds((current) => (
              current.includes(runnerId)
                ? current.filter((id) => id !== runnerId)
                : [...current, runnerId]
            ));
          }}
        />
      )}
      <ChatSessionToast
        text={visibleSessionToast}
        onDismiss={() => {
          if (formError !== '') {
            setFormError('');
            return;
          }
          setDismissedToast(sessionToast);
        }}
      />
      <ChatSessionToast text={markdownProbeError} onDismiss={() => setMarkdownProbeError('')} />
      {runnerFormOpen && runners.length > 0 && independentAddForm}
      <aside className="chat-projects-rail" aria-label="Runners and Projects" id="chat-projects-rail" hidden={mobile && mobilePane !== 'projects'}>
        <section className="chat-runners-rail chat-rail-section" id="chat-runners-rail" aria-label="Runners">
          <div className="chat-rail-section__head">
            <span className="chat-workspace-label">Runners</span>
            <button
              ref={runnerAddButtonRef}
              className="chat-icon-button"
              id="chat-runner-add-btn"
              type="button"
              aria-label="Add runner"
              title="Add runner"
              onClick={openRunnerAdd}
            >+</button>
          </div>
          <input
            id="chat-runner-search"
            aria-label="Search runners"
            value={runnerSearch}
            onChange={(event) => setRunnerSearch(event.target.value)}
            placeholder="Search runners"
          />
          <nav className="chat-runners-list" aria-label="Runners">
            {visibleRunners.map((runner) => (
              <button
                className="chat-runner-row"
                data-runner-id={runner.id}
                data-selected={railFocus?.kind === 'runner' && railFocus.id === runner.id}
                data-tone={statusTone(runner.state)}
                type="button"
                key={runner.id}
                onClick={() => {
                  if (runnerMenuSkipClickRef.current) {
                    runnerMenuSkipClickRef.current = false;
                    return;
                  }
                  selectRunner(runner.id);
                }}
                onContextMenu={(event) => {
                  clearRunnerMenuLongPress();
                  setRunnerContextMenu(openRunnerContextMenuFromEvent(runner.id, event));
                }}
                onPointerDown={(event) => {
                  if (!runnerLongPressShouldArm(event.pointerType, event.button)) return;
                  clearRunnerMenuLongPress();
                  const pointerId = event.pointerId;
                  const timer = scheduleRunnerLongPress(
                    runner.id,
                    event.clientX,
                    event.clientY,
                    (handler, timeout) => window.setTimeout(handler, timeout),
                    (target) => {
                      runnerMenuLongPressRef.current = null;
                      runnerMenuSkipClickRef.current = true;
                      setRunnerContextMenu(target);
                    },
                  );
                  runnerMenuLongPressRef.current = { pointerId, timer };
                }}
                onPointerUp={(event) => {
                  if (runnerMenuLongPressRef.current?.pointerId === event.pointerId) clearRunnerMenuLongPress();
                }}
                onPointerCancel={(event) => {
                  if (runnerMenuLongPressRef.current?.pointerId === event.pointerId) clearRunnerMenuLongPress();
                }}
                onPointerLeave={(event) => {
                  if (runnerMenuLongPressRef.current?.pointerId === event.pointerId) clearRunnerMenuLongPress();
                }}
              >
                <span className="chat-status-dot" data-tone={statusTone(runner.state)} aria-hidden="true" />
                <span className="chat-runner-row__label">{safeText(runnerDisplayName(runner))}</span>
              </button>

            ))}
            {runners.length === 0 && <div className="chat-empty">{CHAT_RUNNERS_EMPTY}</div>}
          </nav>
        {lanRailCandidates.length > 0 && (
          <details
            className="chat-rail-lan"
            id="chat-rail-lan"
            aria-label="Discovered on LAN"
            open={lanRailOpen}
            onToggle={(event) => setLanRailOpen(event.currentTarget.open)}
          >
            <summary className="chat-rail-section__head">
              <span className="chat-workspace-label">Discovered on LAN</span>
            </summary>
            <div className="chat-rail-lan__list">
            {lanRailCandidates.map((candidate) => (
              <div className="chat-rail-lan__row" key={candidate.key} data-tone={candidateTone(candidate.state)}>
                <span className="chat-status-dot" data-tone={candidateTone(candidate.state)} aria-hidden="true" />
                <span className="chat-rail-lan__name">
                  {safeText(runnerDisplayName({ codename: candidate.codename, label: candidate.label, authority: candidate.address }))}
                </span>
                <button
                  className="chat-button chat-button--compact chat-button--primary"
                  type="button"
                  disabled={inviteBusyKey !== null}
                  onClick={() => void addRunner(lanDiscoverAddPayload(candidate, loadTokenForAuthority(storage, pageAuthority)))}
                >
                  {inviteBusyKey === candidate.key ? 'Connecting…' : 'Add'}
                </button>
              </div>
            ))}
            </div>
          </details>
        )}
        </section>
        <section className="chat-projects-section" aria-label="Projects">
        <div className="chat-projects-rail__head">
          <span className="chat-workspace-label">Projects</span>
          <button
            ref={projectAddButtonRef}
            className="chat-icon-button"
            id="chat-project-add-btn"
            type="button"
            aria-label="New project"
            title="New project"
            onClick={toggleNewProject}
          >+</button>
        </div>
        <input
            id="chat-project-search"
            aria-label="Search projects"
            value={projectSearch}
            onChange={(event) => setProjectSearch(event.target.value)}
            placeholder="Search projects"
          />
        <nav className="chat-projects-list" aria-label="Projects">
          {visibleProjects.map((project) => (
            <button
              className="chat-project-row"
              data-project-id={project.id}
              data-selected={railFocus?.kind === 'project' && railFocus.id === project.id}
              type="button"
              key={project.id}
              onClick={() => selectProject(project.id)}
            >
              <span className="chat-status-dot" data-tone={projectTone(project.id, mapping.mappings, runners)} aria-hidden="true" />
              <span className="chat-project-row__label">
                <span className="chat-channel-hash" aria-hidden="true">#</span>
                {safeText(project.label)}
              </span>
              {projectBroadcastCount(project.id, team) > 0 && (
                <span className="chat-project-row__broadcast">{projectBroadcastCount(project.id, team)}</span>
              )}
            </button>
          ))}
          {projects.length === 0 && <div className="chat-empty">{CHAT_RAIL_EMPTY}</div>}
        </nav>
        </section>
      </aside>

      {!focusedRunner && (
      <section
        className="chat-project-main"
        data-tablet-context-open={tabletContextOpen}
        id="chat-project-main"
        hidden={mobile && mobilePane !== 'conversation' && mobilePane !== 'context'}
      >
        {selectedProject && (
          <header className="chat-project-header">
            <div className="chat-project-header__row">
              {mobile && (
                <button
                  className="chat-button chat-button--compact chat-project-header__back"
                  id="chat-mobile-back-projects"
                  type="button"
                  aria-label="Back to Projects"
                  onClick={() => teamStore.setMobilePane('projects')}
                >
                  ←
                </button>
              )}
              <div className="chat-project-header__identity">
                <h1>
                  <span className="chat-channel-hash" aria-hidden="true">#</span>
                  {safeText(selectedProject.label)}
                </h1>
                <p className="chat-runner-origin">{activeMappings.length} runner{activeMappings.length === 1 ? '' : 's'} · {connectionSummary}</p>
              </div>
              <div className="chat-project-header__actions">
                {compact && (
                  <button
                    ref={detailsButtonRef}
                    aria-expanded={mobile ? mobilePane === 'context' : tabletContextOpen}
                    className="chat-button chat-button--compact"
                    type="button"
                    onClick={() => {
                      if (mobile) teamStore.setMobilePane('context');
                      else setTabletContextOpen((open) => !open);
                      if ((channel?.contextPane ?? 'files') === 'files') openFileExplorer();
                    }}

                  >
                    Details
                  </button>
                )}
                <button ref={inviteButtonRef} className="chat-button chat-button--primary" id="chat-invite-runner-btn" type="button" onClick={openInvite}>
                  Add runner
                </button>
                <button
                  ref={gearButtonRef}
                  className="chat-icon-button"
                  id="chat-project-gear-btn"
                  type="button"
                  aria-label="Project settings"
                  aria-expanded={gearOpen}
                  onClick={toggleProjectSettings}
                >
                  ⚙
                </button>
              </div>
            </div>
            {conversationTarget && shouldOfferCertTrust(conversationTarget.runner) ? (
              <a
                className="chat-button chat-button--compact"
                href={runnerTrustHref(conversationTarget.runner, storage)}
                target="_blank"
                rel="noopener noreferrer"
              >
                Open to trust
              </a>
            ) : selectedRunnerError !== '' && conversationTarget !== null && conversationTarget.runner.state !== 'reconnecting' && conversationTarget.runner.state !== 'connecting' ? (
              <p className="chat-runner-error" role="status">{selectedRunnerError}</p>
            ) : conversationTarget !== null && (conversationTarget.runner.state === 'reconnecting' || conversationTarget.runner.state === 'connecting') ? (
              <p className="chat-runner-error chat-runner-error--transient" role="status">Reconnecting…</p>
            ) : null}
            {headerMappings.length > 0 && (
            <div className="chat-target-select" aria-label="Conversation target">
              {activeMappings.length > 0 && (
                <button
                  className="chat-chip chat-chip--interactive"
                  id="chat-channel-target"
                  data-active={target?.kind === 'broadcast'}
                  type="button"
                  onClick={() => selectTarget({ kind: 'broadcast' })}
                >
                  Channel
                </button>
              )}
              {headerMappings.map((mappingRow) => {
                const runner = runners.find((candidate) => candidate.id === mappingRow.runnerId);
                if (!runner) return null;
                const removing = mappingRow.state === 'deleting' || mappingRow.state === 'rollback_pending';
                const selected = !removing && target?.kind === 'runner' && target.mappingId === mappingRow.id;
                return (
                  <button
                    className="chat-chip chat-chip--interactive"
                    data-runner-id={runner.id}
                    data-active={selected}
                    type="button"
                    key={mappingRow.id}
                    disabled={removing}
                    title={boundedVisibleRunnerError(runner.error) || undefined}
                    aria-description={boundedVisibleRunnerError(runner.error) || undefined}
                    onClick={() => {
                      if (removing) return;
                      selectTarget({ kind: 'runner', mappingId: mappingRow.id });
                    }}
                  >
                    <span className="chat-status-dot" data-tone={statusTone(runner.state)} aria-hidden="true" />
                    {safeText(runnerDisplayName(runner))}
                    {removing && <span>{mappingSetupLabel(mappingRow.state)}</span>}
                  </button>

                );
              })}
            </div>
            )}
          </header>
        )}

        {formOpen && inviteFields && !inviteLobbyOpen && (
          <InviteRunnerDialog placement="overlay" key={`${inviteFields.project.id}:overlay`} {...inviteFields} />
        )}

        {gearOpen && selectedProject && (
          <div ref={gearMenuRef} className="chat-gear-menu" id="chat-project-gear" role="dialog" aria-label="Project settings">
            <h2>{safeText(selectedProject.label)}</h2>
            {members.map((member) => {
              const mapped = member.mapping.state === 'active';
              return (
                <dl className="chat-runner-details" key={member.mapping.id}>
                  <div className="chat-runner-detail">
                    <dt>{safeText(runnerDisplayName(member.runner))}</dt>
                    <dd title={mapped ? member.mapping.sessionId : ''}>
                      {mapped ? (
                        <button
                          className="chat-gear-session-link"
                          type="button"
                          onClick={() => selectRunner(member.runner.id)}
                        >
                          {safeText(member.mapping.sessionName.replace(/^chat-/, ''))}
                        </button>
                      ) : '--'}
                    </dd>
                  </div>
                  <div className="chat-runner-detail">
                    <dt>Working directory</dt>
                    <dd>{mapped ? safeText(selectedProject.cwd) : '--'}</dd>
                  </div>
                </dl>
              );
            })}
            <button
              className="chat-button chat-button--compact chat-button--danger"
              id="chat-gear-delete-project"
              type="button"
              disabled={isProjectBusy}
              aria-label={confirmProjectDelete ? 'Confirm delete project' : 'Delete project'}
              onClick={() => {
                if (!confirmProjectDelete) {
                  setConfirmProjectDelete(true);
                  return;
                }
                setConfirmProjectDelete(false);
                void mappingStore.deleteProject(selectedProject.id).catch((error) => {
                  setFormError(error instanceof Error ? error.message : String(error));
                });
              }}
            >
              {confirmProjectDelete ? 'Delete?' : 'Delete project'}
            </button>
          </div>
        )}


        <div className="chat-project-body" style={{ position: tmuxTerminal ? 'relative' : undefined }}>
          <div className="chat-conversation" hidden={mobile && mobilePane === 'context'}>
            {!selectedProject && (
              <ChatEmptyState
                lanCandidates={lanRailCandidates}
                lanBusyKey={inviteBusyKey}
                onAddLan={(candidate) => void addRunner(lanDiscoverAddPayload(candidate, loadTokenForAuthority(storage, pageAuthority)))}
                onAction={() => {
                  setFormError('');
                  setProjectFormOpen(true);
                }}
              />
            )}
            {runnerFormOpen && runners.length === 0 && independentAddForm}
            {inviteLobbyOpen && inviteFields && (
              <section className="chat-lobby" id="chat-empty-project">
                <div className="chat-lobby__intro">
                  <p className="chat-empty__kicker">This project</p>
                  <h2 className="chat-empty__title" id="chat-empty-project-title">{CHAT_PROJECT_EMPTY_TITLE}</h2>
                  <p className="chat-empty__body">{CHAT_PROJECT_EMPTY_BODY}</p>
                </div>
                <InviteRunnerDialog placement="lobby" key={`${inviteFields.project.id}:lobby`} {...inviteFields} />
              </section>
            )}
            {selectedProject && target?.kind === 'runner' && activeSession && (
              <SessionView
                mappingId={activeSession.mapping.id}
                key={JSON.stringify([activeSession.runner.id, activeSession.mapping.sessionId])}
                activeTurnId={activeSessionState?.activeTurnId ?? null}
                isStreaming={activeSessionState?.isStreaming === true}
                readAuthority={readSessionAuthority}
                cwd={activeSession.project.cwd}
                events={sessionEvents}
                composerDisabledReason={mappingBusy ? 'This runner is responding to the current Broadcast.' : undefined}
                onTransportError={reportSessionTransportError}
                runner={activeSession.runner}
                sendCommand={sendSessionCommand}
                sessionId={activeSession.mapping.sessionId}
              />
            )}
            {selectedProject && target?.kind === 'broadcast' && (
              <BroadcastConversation
                project={selectedProject}
                runs={channel?.broadcastRuns ?? []}
                messages={channel?.channelMessages ?? []}
                members={members}
                disabledReason={displayedBroadcast && deriveBroadcastRunStatus(displayedBroadcast) === 'running'
                  ? 'A Broadcast is still running.'
                  : members.length === 0
                    ? CHAT_INVITE_EMPTY
                    : ''}
                sendCommand={async (command) => {
                  const member = members.find((row) => row.runner.state === 'connected');
                  if (!member) throw new Error('No runner is connected.');
                  const response = await registry.command(member.runner.id, command, {
                    sourceSessionId: member.mapping.sessionId,
                  });
                  return response.data;
                }}
                onSend={async (message, mappingIds) => {
                  await teamStore.sendBroadcast(selectedProject.id, message, mappingIds);
                }}
                onCancelTarget={async (runId, mappingId) => {
                  await teamStore.cancelBroadcastTarget(selectedProject.id, runId, mappingId);
                }}
                onRetryTarget={async (runId, mappingId) => {
                  await teamStore.retryBroadcastTarget(selectedProject.id, runId, mappingId);
                }}
                onViewRunner={(mappingId) => selectTarget({ kind: 'runner', mappingId })}
              />
            )}
          </div>

          <aside
            className="chat-context"
            hidden={!selectedProject || (mobile ? mobilePane !== 'context' : compact ? !tabletContextOpen : activeMappings.length === 0)}
            aria-label="Project details"
          >
            <div className="chat-context__head">
            {compact && (
              <button
                className="chat-button chat-button--compact chat-context__close"
                type="button"
                aria-label="Back"
                onClick={() => {
                  if (mobile) teamStore.setMobilePane('conversation');
                  else {
                    setTabletContextOpen(false);
                    detailsButtonRef.current?.focus();
                  }
                }}
              >
                ←
              </button>
            )}
            {compact && <span className="chat-context__title">Details</span>}
            <div className="chat-context__tabs" role="tablist">
              {(['files', 'jobs'] as const).map((pane) => (
                <button
                  className={`chat-button chat-button--compact${contextPane === pane ? ' is-pressed' : ''}`}
                  key={pane}
                  role="tab"
                  id={pane === 'files' ? CHAT_FILE_EXPLORER_TOGGLE_ID : undefined}
                  aria-selected={contextPane === pane}
                  type="button"
                  onClick={() => {
                    if (!selectedProjectId) return;
                    teamStore.setContextPane(selectedProjectId, pane);
                    if (pane === 'files') openFileExplorer();
                  }}
                >
                  {pane === 'files' ? 'Files' : 'Jobs'}
                </button>
              ))}
            </div>
            </div>
            {contextPane === 'jobs' && (
              <div className="chat-context__jobs">
                <p className="chat-team-note">
                  {jobsNoteText(teamAggregate)}
                </p>
                {members.map((member) => {
                  const activityJobs = runnerActivityJobs(member.session);
                  if (activityJobs.length === 0) return null;
                  return (
                    <section key={member.mapping.id}>
                      <h3>{safeText(runnerDisplayName(member.runner))}</h3>
                      {activityJobs.map((job) => (
                        <article className="chat-team-job" key={teamJobKey({ runnerId: member.runner.id, sessionId: member.mapping.sessionId, jobId: job.id })}>
                          <strong>{safeText(job.description || job.agent)}</strong>
                          <span>{job.status}</span>
                        </article>
                      ))}
                    </section>
                  );
                })}
              </div>
            )}

            {contextPane === 'files' && (
              <div className="chat-context__owned-pane chat-context__files">
                {activeMappings.length > 1 && (
                  <div className="chat-target-select" aria-label="Files owner">
                    {activeMappings.map((mappingRow) => {
                      const runner = runners.find((candidate) => candidate.id === mappingRow.runnerId);
                      if (!runner) return null;
                      return (
                        <button
                          className="chat-chip chat-chip--interactive"
                          data-active={fileOwnerMapping?.id === mappingRow.id}
                          type="button"
                          key={mappingRow.id}
                          onClick={() => selectedProjectId && teamStore.setFileOwner(selectedProjectId, mappingRow.id)}
                        >
                          {safeText(runnerDisplayName(runner))}
                        </button>
                      );
                    })}
                  </div>
                )}

                {fileExplorerOpen && fileExplorerOwner && fileExplorerMappingId && fileExplorerSessionId && (
                  <FileExplorerPanel
                    key={`file-explorer:${fileExplorerMappingId}`}
                    toggleId={CHAT_FILE_EXPLORER_TOGGLE_ID}
                    connectionEpoch={fileExplorerOwner.runner.connectionEpoch}
                    hostAuthority={fileExplorerOwner.runner.authority}
                    onClose={() => patchFileExplorer(fileExplorerMappingId, (slot) => applyDrawerMode(slot, 'closed'))}
                    onOpenPath={(path, line, view?: FileExplorerTab['view']) => {
                      patchFileExplorer(fileExplorerMappingId, (slot) => applyOpenPath(slot, path, line, view));
                    }}
                    overlay={false}
                    exclusivePanes={true}
                    pageProtocol={typeof location !== 'undefined' ? location.protocol : 'http:'}
                    downloadOrigin={fileExplorerOwner.runner.origin}
                    sendCommand={sendFileExplorerCommand as (command: Record<string, unknown>) => Promise<unknown>}
                    sessionId={fileExplorerMappingId}
                    slot={fileExplorerSlot}
                    workspaceCwd={fileExplorerOwner.project.cwd}
                    onPatchSlot={(updater) => { if (fileExplorerMappingId) patchFileExplorer(fileExplorerMappingId, updater); }}
                    onConversationError={setFormError}
                    conversationSessionId={fileExplorerSessionId}
                    markdownRunnerId={fileExplorerOwner.runner.id}
                    markdownMappingId={fileExplorerOwner.mapping.id}
                  />
                )}
              </div>
            )}
          </aside>
          {tmuxOverlay}
        </div>
      </section>
      )}

      {focusedRunner && (
      <section
        className="chat-runner-main"
        id="chat-runner-main"
        hidden={mobile && mobilePane !== 'conversation' && mobilePane !== 'context'}
      >
        <header className="chat-runner-header">
          <div className="chat-runner-header__row">
            {mobile && (
              <button className="chat-button chat-button--compact chat-runner-header__back" type="button" onClick={() => teamStore.setMobilePane('projects')}>
                ←
              </button>
            )}
            <div className="chat-runner-header__identity">
              <h1>{safeText(runnerDisplayName(focusedRunner))}</h1>
            </div>
            <button
              className="chat-button chat-button--compact"
              id="chat-runner-gear-btn"
              type="button"
              aria-label="Runner settings"
              aria-expanded={runnerGearOpen}
              onClick={toggleRunnerSettings}
            >
              ⚙
            </button>
          </div>
          {runnerGearOpen && (
            <div ref={runnerGearMenuRef} className="chat-gear-menu chat-runner-gear" id="chat-runner-gear" role="dialog" aria-label="Runner settings">
              <button
                className="chat-button chat-button--compact"
                id="chat-runner-create-session"
                type="button"
                aria-label="Create tmux session"
                disabled={focusedRunner.state !== 'connected'}
                onClick={() => {
                  setFormError('');
                  setConfirmRunnerDelete(false);
                  setTokenFormRunnerId(null);
                  setRunnerGearOpen(false);
                  setCreateSessionOpen(true);
                }}
              >
                + Session
              </button>
              <button
                className="chat-button chat-button--compact"
                type="button"
                onClick={() => {
                  setFormError('');
                  setConfirmRunnerDelete(false);
                  setGearOpen(false);
                  setRunnerFormOpen(false);
                  setCreateSessionOpen(false);
                  setTokenDraft('');
                  setRunnerGearOpen(false);
                  setTokenFormRunnerId(focusedRunner.id);
                }}
              >
                Set token
              </button>
              {shouldOfferCertTrust(focusedRunner) && (
                <a
                  className="chat-button chat-button--compact"
                  href={runnerTrustHref(focusedRunner, storage)}
                  target="_blank"
                  rel="noopener noreferrer"
                >
                  Open to trust
                </a>
              )}
              <button
                className="chat-button chat-button--compact chat-button--danger"
                id="chat-runner-delete"
                type="button"
                disabled={inviteBusyKey === `remove:${focusedRunner.id}`}
                aria-label={confirmRunnerDelete ? 'Confirm delete runner' : 'Delete runner'}
                onClick={() => {
                  if (!confirmRunnerDelete) {
                    setConfirmRunnerDelete(true);
                    return;
                  }
                  setConfirmRunnerDelete(false);
                  setRunnerGearOpen(false);
                  void removeRunner(focusedRunner.id);
                }}
              >
                {confirmRunnerDelete ? 'Delete?' : 'Delete runner'}
              </button>
            </div>
          )}
        </header>
        {createSessionOpen && focusedRunner.state === 'connected' && (
          <form
            className="chat-token-form"
            id="chat-runner-create-session-form"
            onSubmit={(event) => {
              event.preventDefault();
              const name = createSessionName.trim();
              if (name === '' || createSessionBusy) return;
              setCreateSessionBusy(true);
              setFormError('');
              void registry.command(focusedRunner.id, { type: 'tmux_session_create', session: name }, {
                sourceSessionId: focusedRunner.probe?.sessionId ?? undefined,
              }).then((response) => {
                if (!response.success) {
                  setFormError(response.error || 'Create failed');
                  return;
                }
                setCreateSessionOpen(false);
                setCreateSessionName('');
                setPreviewRefreshEpoch((n) => n + 1);
              }).catch((error) => {
                setFormError(error instanceof Error ? error.message : String(error));
              }).finally(() => {
                setCreateSessionBusy(false);
              });
            }}
          >
            <label className="chat-field">
              Session name
              <input
                id="chat-runner-create-session-name"
                type="text"
                value={createSessionName}
                onChange={(event) => setCreateSessionName(event.target.value)}
                autoFocus
              />
            </label>
            <button className="chat-button chat-button--primary" type="submit" disabled={createSessionBusy || createSessionName.trim() === ''}>
              Create
            </button>
            <button
              className="chat-button chat-button--quiet"
              type="button"
              onClick={() => {
                setCreateSessionOpen(false);
                setCreateSessionName('');
              }}
            >
              ×
            </button>
          </form>
        )}
        {tokenFormRunnerId === focusedRunner.id && (
          <form
            className="chat-token-form"
            id="chat-runner-token-form"
            onSubmit={(event) => {
              event.preventDefault();
              teamStore.setMobilePane('conversation');
              void saveIndependentToken(focusedRunner.id, tokenDraft);
            }}
          >
            <label className="chat-field">
              Token
              <input
                id="chat-runner-token-draft"
                type="password"
                autoComplete="off"
                value={tokenDraft}
                onChange={(event) => setTokenDraft(event.target.value)}
                autoFocus
              />
            </label>
            <button className="chat-button chat-button--primary" type="submit" disabled={inviteBusyKey !== null}>
              Save token
            </button>
            <button
              className="chat-button chat-button--quiet"
              type="button"
              onClick={() => {
                setTokenFormRunnerId(null);
                setTokenDraft('');
                setFormError('');
              }}
            >
              ×
            </button>
          </form>
        )}
        {focusedRunner.state === 'connected' ? (
          <TmuxPreviewBoard
            key={focusedRunner.id}
            runnerId={focusedRunner.id}
            runnerLabel={runnerDisplayName(focusedRunner)}
            sessionId={focusedRunner.probe?.sessionId ?? null}
            refreshEpoch={previewRefreshEpoch}
            send={(command, options) => registry.command(focusedRunner.id, command, options)}
            onOpenTerminal={(session) => setTmuxTerminal((current) => (
              current && current.runnerId === focusedRunner.id && current.session === session && current.hidden
                ? { ...current, hidden: false }
                : { runnerId: focusedRunner.id, runnerLabel: runnerDisplayName(focusedRunner), session }
            ))}
          />
        ) : (
          <div className="chat-runner-empty" id="chat-runner-empty">
            <p className="chat-empty">{CHAT_RUNNER_MAIN_EMPTY}</p>
            {(focusedRunner.state === 'token_required' || shouldOfferCertTrust(focusedRunner)) && (
            <div className="chat-runner-empty__actions">
              {focusedRunner.state === 'token_required' && (
                <button
                  className="chat-button chat-button--primary"
                  id="chat-runner-empty-set-token"
                  type="button"
                  onClick={() => {
                    setFormError('');
                    setConfirmRunnerDelete(false);
                    setGearOpen(false);
                    setRunnerFormOpen(false);
                    setCreateSessionOpen(false);
                    setTokenDraft('');
                    setRunnerGearOpen(false);
                    setTokenFormRunnerId(focusedRunner.id);
                  }}
                >
                  ⚙ Set token
                </button>
              )}
              {shouldOfferCertTrust(focusedRunner) && (
                <a
                  className="chat-button chat-button--compact"
                  id="chat-runner-empty-trust"
                  href={runnerTrustHref(focusedRunner, storage)}
                  target="_blank"
                  rel="noopener noreferrer"
                >
                  ↗ Open to trust
                </a>
              )}
            </div>
            )}
          </div>
        )}



        {tmuxOverlay}
      </section>
      )}

      {runnerContextMenu && (
        <div
          ref={runnerContextMenuRef}
          id="chat-runner-context-menu"
          className="chat-runner-context-menu"
          role="menu"
          aria-label="Runner"
          style={{ left: runnerContextMenu.x, top: runnerContextMenu.y }}
        >
          {RUNNER_CONTEXT_ACTIONS.map((action) => (
            <button
              key={action.id}
              id={`chat-runner-menu-${action.id}`}
              className="chat-runner-context-menu__item"
              type="button"
              role="menuitem"
              onClick={() => {
                const runner = runners.find((row) => row.id === runnerContextMenu.runnerId);
                if (!runner) {
                  setRunnerContextMenu(null);
                  return;
                }
                runRunnerContextAction(action.id, runner);
              }}
            >
              {action.label}
            </button>
          ))}
        </div>
      )}

    </main>
  );
}
