/**
 * Exact-runtime session-switch reducer and wire validation.
 * Pure: no React, DOM, WebSocket, or toast side effects.
 */
import type { ComposerAttachment } from './attachments';
import type { ExactRuntimeEventFrame } from './types';

export const MAX_LOADED_SESSIONS = 128;
export const SWITCH_HANDOFF_RESPONSE_MODE = 'subscribe-handoff-v1' as const;
export const LIFECYCLE_RESPONSE_VERSION = 1 as const;
export const SWITCH_PENDING_ERROR = 'switch-pending';
export const MAX_SWITCH_MS = 1000;
export const AUTHORITATIVE_CUT_TIMEOUT_MS = 15_000;
export const SNAPSHOT_IN_PROGRESS_CODE = 'snapshot_in_progress';
export const SNAPSHOT_IN_PROGRESS_RETRY_MS = 80;

export type AuthoritativeCutTick = 'hold' | 'fire';

export function planAuthoritativeCutTick(input: {
  now: number;
  lastProgressAt: number;
  timeoutMs: number;
  refillInFlight: boolean;
}): AuthoritativeCutTick {
  if (input.refillInFlight) return 'hold';
  if (input.now - input.lastProgressAt < input.timeoutMs) return 'hold';
  return 'fire';
}

export function authoritativeCutTimerDelayMs(input: {
  now: number;
  lastProgressAt: number;
  timeoutMs: number;
  refillInFlight: boolean;
}): number {
  if (input.refillInFlight) return input.timeoutMs;
  const remaining = input.timeoutMs - (input.now - input.lastProgressAt);
  if (remaining <= 0) return 1;
  return remaining;
}

export function validateSwitchMilestones(values: Readonly<Record<string, number | null>>): string[] {
  const failures: string[] = [];
  for (const [milestone, ms] of Object.entries(values)) {
    if (ms === null || !Number.isFinite(ms) || ms < 0 || ms >= MAX_SWITCH_MS) {
      failures.push(`${milestone}=${String(ms)}`);
    }
  }
  return failures;
}

export interface RuntimeIdentity {
  sessionId: string;
  runtimeIncarnation: string;
}

export type RuntimeKey = string;

/** Backend-normalized catalog source: one of the five providers. */
export type CatalogSource = 'rpi' | 'pi' | 'omp' | 'codex' | 'grok';
/** Catalog membership. A native catalog row maps 1:1 to the runtime session
 *  id; a foreign catalog id may resolve to a DISTINCT native runtime id, so
 *  only the catalog tuple is identity-checked end to end. */
export type CatalogStatus = 'native' | 'foreign';

export interface SessionSwitchTarget {
  path: string;
  catalogSource: CatalogSource;
  catalogSessionId: string;
  catalogStatus: CatalogStatus;
  /** Advisory warm hint from the catalog's resident-runtime overlay; non-null
   *  only when the row is currently loaded. Never validated: an LRU reopen
   *  mints a fresh incarnation, the handoff response stays authoritative, and
   *  warmth is decided solely by the client's exact ready cache. */
  catalogRuntimeIncarnation: string | null;
}

export type SessionSwitchOrigin = 'user' | 'bootstrap';
export type SessionSwitchPhase = 'opening' | 'awaiting-authoritative-cut';

export interface PendingSessionSwitch {
  generation: number;
  source: RuntimeIdentity;
  targetRequest: SessionSwitchTarget;
  targetIdentity: RuntimeIdentity | null;
  responseCursor: number | null;
  phase: SessionSwitchPhase;
  warmDisplayed: boolean;
}

export interface SessionDisplayState {
  displayed: RuntimeIdentity;
  commandTarget: RuntimeIdentity;
  pending: PendingSessionSwitch | null;
  nextGeneration: number;
}

export interface ReadyRuntime {
  identity: RuntimeIdentity;
  cursor: number;
  /** Transport generation whose authoritative cut produced this entry.
   *  Ready means THIS connection has already received an authoritative cut
   *  for the exact identity: a reconnect (new socketGen) must negotiate and
   *  subscribe again before any frontend-only fast switch. */
  socketGeneration: number;
}
export interface AuthoritativeNativeSessionRow {
  source: 'rpi';
  sessionId: string;
  runtimeIncarnation: string;
  cwd: string;
  path: string;
  status: 'native';
  loaded: true;
  name?: string | null;
  summary?: string;
  displayTime?: string;
  messageCount?: number | null;
  [field: string]: unknown;
}


export interface SwitchHandoffV1 {
  sessionId: string;
  runtimeIncarnation: string;
  catalogSource: CatalogSource;
  catalogSessionId: string;
  catalogStatus: CatalogStatus;
  subscriptionRequired: true;
  lifecycleResponseVersion: 1;
  cursor: number;
  session: AuthoritativeNativeSessionRow;
  notice?: string | null;
}

export type SubscriptionCutReason = 'cold-full' | 'warm-continuity' | 'resync';

export interface SubscriptionCut {
  type: 'subscription_snapshot' | 'session_resync';
  sessionId: string;
  runtimeIncarnation: string;
  watermark: number;
  reason: SubscriptionCutReason;
  state: unknown;
  messages?: unknown[];
  events: ExactRuntimeEventFrame[];
  activeTurnId?: string;
}

export type RuntimeUnloadReason = 'lru' | 'closed';

export interface SessionUnload {
  type: 'session_unloaded';
  sessionId: string;
  runtimeIncarnation: string;
  reason: RuntimeUnloadReason;
}

export interface TranscriptScrollSlot {
  follow: boolean;
  anchorItemId: string | null;
  anchorOffsetPx: number;
  fallbackScrollTop: number;
}

export interface SessionViewSlot {
  identity: RuntimeIdentity;
  lastUsed: number;
  draft: string;
  selectionStart: number;
  selectionEnd: number;
  attachments: ComposerAttachment[];
  intakeGeneration: number;
  pendingUploadIds: Set<string>;
  transcriptScroll: TranscriptScrollSlot;
  focusedElementKey: string | null;
  openFoldKeys: Set<string>;
  activePanel: string;
  panelScrollTop: number;
}

export type SwitchAction =
  | { type: 'invalidate-runtime'; identity: RuntimeIdentity }
  | { type: 'install-cursor'; identity: RuntimeIdentity; cursor: number }
  | { type: 'subscribe'; identity: RuntimeIdentity; cursor: number }
  | { type: 'display-warm'; identity: RuntimeIdentity }
  | { type: 'apply-messages'; identity: RuntimeIdentity; messages: unknown[]; preserveInflight: boolean }
  | { type: 'apply-state'; identity: RuntimeIdentity; state: unknown }
  | { type: 'replay-events'; identity: RuntimeIdentity; events: ExactRuntimeEventFrame[] }
  | { type: 'mark-ready'; identity: RuntimeIdentity; cursor: number }
  | { type: 'bind-authority'; identity: RuntimeIdentity; cursor: number }
  | { type: 'save-preference'; sessionId: string }
  | { type: 'toast-notice'; notice: string }
  | { type: 'toast-error'; message: string }
  | { type: 'fail-pending' }
  | { type: 'resubscribe-zero'; identity: RuntimeIdentity };

export interface SwitchHandoffPlan {
  state: SessionDisplayState;
  actions: SwitchAction[];
  warm: boolean;
  stale: boolean;
}

export interface SessionCutPlan {
  state: SessionDisplayState;
  actions: SwitchAction[];
  settlesPromise: boolean;
}

export interface SessionUnloadPlan {
  state: SessionDisplayState | null;
  actions: SwitchAction[];
  droppedKey: RuntimeKey;
  failedPending: boolean;
}

export type RequestSessionSwitch = (
  target: SessionSwitchTarget,
  options: { origin: SessionSwitchOrigin },
) => Promise<boolean>;

export class SwitchProtocolError extends Error {
  readonly code: string;
  constructor(code: string, message: string) {
    super(message);
    this.name = 'SwitchProtocolError';
    this.code = code;
  }
}

/** Length-prefixed identity key. Session ids may contain any delimiter. */
export function runtimeKey(identity: RuntimeIdentity): RuntimeKey {
  return `${identity.sessionId.length}:${identity.sessionId}:${identity.runtimeIncarnation.length}:${identity.runtimeIncarnation}`;
}

export function parseRuntimeKey(key: RuntimeKey): RuntimeIdentity | null {
  const first = key.indexOf(':');
  if (first <= 0) return null;
  const sessionLen = Number(key.slice(0, first));
  if (!Number.isInteger(sessionLen) || sessionLen < 1) return null;
  const sessionStart = first + 1;
  const sessionEnd = sessionStart + sessionLen;
  if (key.charAt(sessionEnd) !== ':') return null;
  const rest = key.slice(sessionEnd + 1);
  const second = rest.indexOf(':');
  if (second <= 0) return null;
  const incarnationLen = Number(rest.slice(0, second));
  if (!Number.isInteger(incarnationLen) || incarnationLen < 1) return null;
  const runtimeIncarnation = rest.slice(second + 1);
  if (runtimeIncarnation.length !== incarnationLen) return null;
  return {
    sessionId: key.slice(sessionStart, sessionEnd),
    runtimeIncarnation,
  };
}

export function sameRuntime(
  a: RuntimeIdentity | null | undefined,
  b: RuntimeIdentity | null | undefined,
): boolean {
  return !!a && !!b
    && a.sessionId === b.sessionId
    && a.runtimeIncarnation === b.runtimeIncarnation;
}

/**
 * Backend-normalized catalog source: EXACTLY the five canonical provider
 * keys. There are no wire aliases and no defaults — a missing, empty, or
 * unknown source (including the historic `native`/`primary`/`grok/hyper`
 * aliases) rejects (null) and never forms a switch target.
 */
const CATALOG_SOURCES: Readonly<Record<string, CatalogSource>> = {
  rpi: 'rpi',
  pi: 'pi',
  omp: 'omp',
  codex: 'codex',
  grok: 'grok',
};

export function normalizeCatalogSource(value: unknown): CatalogSource | null {
  if (typeof value !== 'string') return null;
  return CATALOG_SOURCES[value] ?? null;
}

/** Catalog status is exact: only the two canonical wire values normalize;
 *  missing/unknown rejects (null) instead of defaulting to native. */
export function normalizeCatalogStatus(value: unknown): CatalogStatus | null {
  return value === 'native' || value === 'foreign' ? value : null;
}

export function buildSwitchCommand(target: SessionSwitchTarget, sourceSessionId: string): Record<string, unknown> {
  return {
    type: 'switch_session',
    sessionPath: target.path,
    responseMode: SWITCH_HANDOFF_RESPONSE_MODE,
    sessionId: sourceSessionId,
    catalogSource: target.catalogSource,
    catalogSessionId: target.catalogSessionId,
    catalogStatus: target.catalogStatus,
  };
}

/** Backend-schema adapter: both fields must be non-empty strings. */
export function identityFromWire(
  sessionId: unknown,
  runtimeIncarnation: unknown,
): RuntimeIdentity | null {
  if (typeof sessionId !== 'string' || sessionId === '') return null;
  if (typeof runtimeIncarnation !== 'string' || runtimeIncarnation === '') return null;
  return { sessionId, runtimeIncarnation };
}

export function buildSubscribeFrame(identity: RuntimeIdentity, cursor: number): Record<string, unknown> {
  requireCursor(cursor, 'cursor');
  return {
    type: 'subscribe',
    sessionId: identity.sessionId,
    runtimeIncarnation: identity.runtimeIncarnation,
    cursor,
  };
}

export function emptyViewSlot(identity: RuntimeIdentity, lastUsed = 0): SessionViewSlot {
  return {
    identity,
    lastUsed,
    draft: '',
    selectionStart: 0,
    selectionEnd: 0,
    attachments: [],
    intakeGeneration: 0,
    pendingUploadIds: new Set(),
    transcriptScroll: {
      follow: true,
      anchorItemId: null,
      anchorOffsetPx: 0,
      fallbackScrollTop: 0,
    },
    focusedElementKey: null,
    openFoldKeys: new Set(),
    activePanel: '',
    panelScrollTop: 0,
  };
}

function requiredNonEmptyString(value: unknown, field: string): string {
  if (typeof value !== 'string' || value === '') {
    throw new SwitchProtocolError('invalid-identity', `${field} is required`);
  }
  return value;
}

function requireCursor(value: unknown, field: string): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) {
    throw new SwitchProtocolError(`invalid-${field}`, `${field} must be a nonnegative safe integer`);
  }
  return value;
}
export function parseAuthoritativeNativeSessionRow(
  value: unknown,
  expected: RuntimeIdentity & { cwd?: string; path?: string },
): AuthoritativeNativeSessionRow {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new SwitchProtocolError('invalid-session-row', 'handoff session row must be an object');
  }
  const session = value as Record<string, unknown>;
  if (session.sessionId !== expected.sessionId) {
    throw new SwitchProtocolError('target-mismatch', 'handoff session row sessionId disagrees with the target');
  }
  if (session.runtimeIncarnation !== expected.runtimeIncarnation) {
    throw new SwitchProtocolError('target-mismatch', 'handoff session row runtimeIncarnation disagrees with the target');
  }
  const cwd = requiredNonEmptyString(session.cwd, 'session.cwd');
  const path = requiredNonEmptyString(session.path, 'session.path');
  if (expected.cwd !== undefined && cwd !== expected.cwd) {
    throw new SwitchProtocolError('target-mismatch', 'handoff session row cwd disagrees with the target state');
  }
  if (expected.path !== undefined && path !== expected.path) {
    throw new SwitchProtocolError('target-mismatch', 'handoff session row path disagrees with the target state');
  }
  if (session.source !== 'rpi') {
    throw new SwitchProtocolError('invalid-session-row', 'handoff session row source must be rpi');
  }
  if (session.status !== 'native') {
    throw new SwitchProtocolError('invalid-session-row', 'handoff session row status must be native');
  }
  if (session.loaded !== true) {
    throw new SwitchProtocolError('invalid-session-row', 'handoff session row must be loaded');
  }
  if (session.name !== undefined && session.name !== null && typeof session.name !== 'string') {
    throw new SwitchProtocolError('invalid-session-row', 'handoff session row name must be a string or null');
  }
  if (session.summary !== undefined && typeof session.summary !== 'string') {
    throw new SwitchProtocolError('invalid-session-row', 'handoff session row summary must be a string');
  }
  if (session.displayTime !== undefined && typeof session.displayTime !== 'string') {
    throw new SwitchProtocolError('invalid-session-row', 'handoff session row displayTime must be a string');
  }
  if (session.messageCount !== undefined && session.messageCount !== null && typeof session.messageCount !== 'number') {
    throw new SwitchProtocolError('invalid-session-row', 'handoff session row messageCount must be a number or null');
  }
  return session as AuthoritativeNativeSessionRow;
}


export function parseSwitchHandoff(
  value: unknown,
  expected: Pick<SessionSwitchTarget, 'catalogSource' | 'catalogSessionId' | 'catalogStatus'>,
): SwitchHandoffV1 {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new SwitchProtocolError('invalid-handoff', 'handoff must be an object');
  }
  const record = value as Record<string, unknown>;
  if ('state' in record || 'messages' in record) {
    throw new SwitchProtocolError('forbidden-snapshot', 'handoff must not carry state or messages');
  }
  // The returned catalog tuple must be the exact requested tuple: the
  // backend echoes what it validated, and any drift means the response
  // describes a different catalog entry than the one the user picked.
  const catalogSource = normalizeCatalogSource(record.catalogSource);
  if (catalogSource === null) {
    throw new SwitchProtocolError('invalid-catalog-source', 'handoff catalogSource is not a known provider');
  }
  if (catalogSource !== expected.catalogSource) {
    throw new SwitchProtocolError('catalog-mismatch', 'handoff catalogSource does not match the requested catalog');
  }
  const catalogStatus = normalizeCatalogStatus(record.catalogStatus);
  if (record.catalogStatus !== catalogStatus) {
    throw new SwitchProtocolError('invalid-catalog-status', 'handoff catalogStatus must be native or foreign');
  }
  if (catalogStatus !== expected.catalogStatus) {
    throw new SwitchProtocolError('catalog-mismatch', 'handoff catalogStatus does not match the requested catalog');
  }
  const catalogSessionId = requiredNonEmptyString(record.catalogSessionId, 'catalogSessionId');
  if (catalogSessionId !== expected.catalogSessionId) {
    throw new SwitchProtocolError('catalog-mismatch', 'handoff catalogSessionId does not match the requested catalog');
  }
  const sessionId = requiredNonEmptyString(record.sessionId, 'sessionId');
  // Native rows map 1:1 to the runtime id. A foreign catalog id may resolve
  // to a distinct native runtime id, so the runtime id is only exact-checked
  // for native targets; foreign accepts whatever runtime the backend bound.
  if (expected.catalogStatus === 'native' && sessionId !== expected.catalogSessionId) {
    throw new SwitchProtocolError('target-mismatch', 'handoff sessionId does not match native catalog target');
  }
  const runtimeIncarnation = requiredNonEmptyString(record.runtimeIncarnation, 'runtimeIncarnation');
  if (record.subscriptionRequired !== true) {
    throw new SwitchProtocolError('invalid-subscription', 'subscriptionRequired must be true');
  }
  if (record.lifecycleResponseVersion !== LIFECYCLE_RESPONSE_VERSION) {
    throw new SwitchProtocolError('invalid-version', 'lifecycleResponseVersion must be 1');
  }
  const cursor = requireCursor(record.cursor, 'cursor');
  const notice = record.notice;
  if (notice !== undefined && notice !== null && typeof notice !== 'string') {
    throw new SwitchProtocolError('invalid-notice', 'notice must be a string or null');
  }
  const handoff: SwitchHandoffV1 = {
    sessionId,
    runtimeIncarnation,
    catalogSource,
    catalogSessionId,
    catalogStatus,
    subscriptionRequired: true,
    lifecycleResponseVersion: 1,
    cursor,
    session: parseAuthoritativeNativeSessionRow(record.session, { sessionId, runtimeIncarnation }),
  };
  if (typeof notice === 'string') handoff.notice = notice;
  else if (notice === null) handoff.notice = null;
  return handoff;
}

/** Strict envelope of a `session_handoff` response: the SAME
 *  subscribe-handoff-v1 shape the catalog switch consumes, but
 *  SELF-describing — the backend mints the new session, so there is no
 *  requested catalog tuple to bind against. Invariants pinned here: native
 *  rpi source/status (byte-exact), sessionId === catalogSessionId (the new
 *  runtime IS the new catalog row), subscription required (the transcript
 *  rides the authoritative subscription cut, never the envelope), and
 *  lifecycle response version 1. */
export interface SessionHandoffEnvelope {
  sessionId: string;
  runtimeIncarnation: string;
  catalogSessionId: string;
  cursor: number;
  notice?: string | null;
}

export function parseSessionHandoffEnvelope(value: unknown): SessionHandoffEnvelope {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new SwitchProtocolError('invalid-handoff', 'handoff must be an object');
  }
  const record = value as Record<string, unknown>;
  if ('state' in record || 'messages' in record) {
    throw new SwitchProtocolError('forbidden-snapshot', 'handoff must not carry state or messages');
  }
  const catalogSource = normalizeCatalogSource(record.catalogSource);
  if (record.catalogSource !== catalogSource || catalogSource !== 'rpi') {
    throw new SwitchProtocolError('invalid-catalog-source', 'session handoff catalogSource must be rpi');
  }
  const catalogStatus = normalizeCatalogStatus(record.catalogStatus);
  if (record.catalogStatus !== catalogStatus || catalogStatus !== 'native') {
    throw new SwitchProtocolError('invalid-catalog-status', 'session handoff catalogStatus must be native');
  }
  const sessionId = requiredNonEmptyString(record.sessionId, 'sessionId');
  const catalogSessionId = requiredNonEmptyString(record.catalogSessionId, 'catalogSessionId');
  if (sessionId !== catalogSessionId) {
    throw new SwitchProtocolError('target-mismatch', 'session handoff sessionId must equal catalogSessionId');
  }
  const runtimeIncarnation = requiredNonEmptyString(record.runtimeIncarnation, 'runtimeIncarnation');
  if (record.subscriptionRequired !== true) {
    throw new SwitchProtocolError('invalid-subscription', 'subscriptionRequired must be true');
  }
  if (record.lifecycleResponseVersion !== LIFECYCLE_RESPONSE_VERSION) {
    throw new SwitchProtocolError('invalid-version', 'lifecycleResponseVersion must be 1');
  }
  const cursor = requireCursor(record.cursor, 'cursor');
  const notice = record.notice;
  if (notice !== undefined && notice !== null && typeof notice !== 'string') {
    throw new SwitchProtocolError('invalid-notice', 'notice must be a string or null');
  }
  const envelope: SessionHandoffEnvelope = { sessionId, runtimeIncarnation, catalogSessionId, cursor };
  if (typeof notice === 'string') envelope.notice = notice;
  else if (notice === null) envelope.notice = null;
  return envelope;
}

export function parseSubscriptionCut(value: unknown): SubscriptionCut {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new SwitchProtocolError('invalid-cut', 'cut must be an object');
  }
  const record = value as Record<string, unknown>;
  const type = record.type;
  if (type !== 'subscription_snapshot' && type !== 'session_resync') {
    throw new SwitchProtocolError('invalid-cut-type', 'cut type is malformed');
  }
  const sessionId = requiredNonEmptyString(record.sessionId, 'sessionId');
  const runtimeIncarnation = requiredNonEmptyString(record.runtimeIncarnation, 'runtimeIncarnation');
  const watermark = requireCursor(record.watermark, 'watermark');
  const reason = record.reason;
  if (reason !== 'cold-full' && reason !== 'warm-continuity' && reason !== 'resync') {
    throw new SwitchProtocolError('invalid-reason', 'cut reason is malformed');
  }
  if (reason === 'resync' && type !== 'session_resync') {
    throw new SwitchProtocolError('invalid-reason', 'resync cuts must use session_resync');
  }
  if (reason !== 'resync' && type !== 'subscription_snapshot') {
    throw new SwitchProtocolError('invalid-reason', 'continuity cuts must use subscription_snapshot');
  }
  if (!Array.isArray(record.messages)) {
    throw new SwitchProtocolError('missing-messages', `${reason} requires a messages array`);
  }
  if (!Array.isArray(record.events)) {
    throw new SwitchProtocolError('invalid-events', 'events must be an array');
  }
  const events: ExactRuntimeEventFrame[] = [];
  for (const valueEvent of record.events) {
    if (!valueEvent || typeof valueEvent !== 'object' || Array.isArray(valueEvent)) {
      throw new SwitchProtocolError('invalid-events', 'cut events must be objects');
    }
    const event = valueEvent as Record<string, unknown>;
    if (event.sessionId !== sessionId || event.runtimeIncarnation !== runtimeIncarnation) {
      throw new SwitchProtocolError('event-identity', 'cut event identity mismatch');
    }
    const type = requiredNonEmptyString(event.type, 'event.type');
    const sequence = requireCursor(event.sequence, 'sequence');
    events.push({ ...event, type, sessionId, runtimeIncarnation, sequence });
  }
  const cut: SubscriptionCut = {
    type,
    sessionId,
    runtimeIncarnation,
    watermark,
    reason,
    state: record.state,
    events,
  };
  if (Array.isArray(record.messages)) cut.messages = record.messages;
  if (typeof record.activeTurnId === 'string' && record.activeTurnId !== '') {
    cut.activeTurnId = record.activeTurnId;
  }
  return cut;
}

export function parseSessionUnload(value: unknown): SessionUnload {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new SwitchProtocolError('invalid-unload', 'unload must be an object');
  }
  const record = value as Record<string, unknown>;
  if (record.type !== 'session_unloaded') {
    throw new SwitchProtocolError('invalid-unload-type', 'unload type is malformed');
  }
  const sessionId = requiredNonEmptyString(record.sessionId, 'sessionId');
  const runtimeIncarnation = requiredNonEmptyString(record.runtimeIncarnation, 'runtimeIncarnation');
  if (record.reason !== 'lru' && record.reason !== 'closed') {
    throw new SwitchProtocolError('invalid-unload-reason', 'unload reason is malformed');
  }
  return {
    type: 'session_unloaded',
    sessionId,
    runtimeIncarnation,
    reason: record.reason,
  };
}

export interface SubscriptionErrorFrame {
  type: 'subscription_error';
  sessionId: string;
  runtimeIncarnation: string;
  error: string;
  code?: string;
}

export function parseSubscriptionError(value: unknown): SubscriptionErrorFrame {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new SwitchProtocolError('invalid-subscription-error', 'subscription_error must be an object');
  }
  const record = value as Record<string, unknown>;
  if (record.type !== 'subscription_error') {
    throw new SwitchProtocolError('invalid-subscription-error-type', 'subscription_error type is malformed');
  }
  const sessionId = requiredNonEmptyString(record.sessionId, 'sessionId');
  const runtimeIncarnation = requiredNonEmptyString(record.runtimeIncarnation, 'runtimeIncarnation');
  const error = typeof record.error === 'string' ? record.error : '';
  const frame: SubscriptionErrorFrame = {
    type: 'subscription_error',
    sessionId,
    runtimeIncarnation,
    error,
  };
  if (typeof record.code === 'string' && record.code !== '') frame.code = record.code;
  return frame;
}

export function shouldRetrySnapshotInProgress(
  code: string | undefined,
  alreadyRetried: boolean,
): boolean {
  return code === SNAPSHOT_IN_PROGRESS_CODE && !alreadyRetried;
}

export function planSubscriptionError(
  state: SessionDisplayState,
  identity: RuntimeIdentity,
): { generation: number } | null {
  const pending = state.pending;
  if (!pending || pending.targetIdentity === null) return null;
  if (!sameRuntime(pending.targetIdentity, identity)) return null;
  return { generation: pending.generation };
}

export interface FastSwitchContext {
  ready: ReadyRuntime | undefined;
  residentIncarnation: string | null;
  viewRestorable: boolean;
  /** Current transport generation; must equal ready.socketGeneration for the
   *  ready entry to be exact-ready on this socket. */
  socketGeneration: number;
}

export type FastSwitchPlan =
  | {
    kind: 'fast';
    state: SessionDisplayState;
    actions: SwitchAction[];
    /** Pending generation the fast path supersedes, settled false by the
     *  caller; null when no transaction was in flight. */
    supersededGeneration: number | null;
  }
  | { kind: 'negotiated' };

/**
 * Same catalog row (path + source + session id + status): the coalescing key.
 * Never the incarnation hint — an LRU reopen or a catalog overlay refresh may
 * change the resident incarnation while a transaction is in flight, and both
 * clicks mean the same catalog row.
 */
export function sameCatalogTarget(a: SessionSwitchTarget, b: SessionSwitchTarget): boolean {
  return a.path === b.path
    && a.catalogSource === b.catalogSource
    && a.catalogSessionId === b.catalogSessionId
    && a.catalogStatus === b.catalogStatus;
}

/**
 * Frontend-only exact-ready fast path. Returns a fast plan ONLY when every
 * gate holds; any miss is fail-closed to the existing negotiated backend path:
 *   - the catalog tuple is rpi/native,
 *   - catalogSessionId equals the ready identity sessionId,
 *   - the advisory catalog hint EXACTLY equals the ready incarnation (null or
 *     stale means the row may have been reopened — the hint is never trusted),
 *   - the resident overlay matches the ready incarnation,
 *   - the cached view slot is safely restorable,
 *   - the ready entry was stamped by the CURRENT socket generation (a
 *     reconnect's bootstrap must negotiate and subscribe again),
 *   - no in-flight transaction targets the same catalog row (that caller
 *     coalesces onto the stored promise through beginSessionSwitch; the fast
 *     path never false-settles its own in-flight switch).
 * A fast plan allocates no generation, emits zero switch_session/subscribe
 * frames, never mutates target messages/state, and leaves background
 * subscriptions intact: the ready cache is authoritative.
 */
export function planFastSessionSwitch(
  display: SessionDisplayState,
  target: SessionSwitchTarget,
  context: FastSwitchContext,
): FastSwitchPlan {
  const ready = context.ready;
  if (!ready
    || ready.identity.sessionId === ''
    || ready.identity.runtimeIncarnation === ''
    || ready.socketGeneration !== context.socketGeneration
    || target.catalogSource !== 'rpi'
    || target.catalogStatus !== 'native'
    || target.catalogSessionId !== ready.identity.sessionId
    || target.catalogRuntimeIncarnation === null
    || target.catalogRuntimeIncarnation !== ready.identity.runtimeIncarnation
    || context.residentIncarnation !== ready.identity.runtimeIncarnation
    || !context.viewRestorable
    || (display.pending !== null && sameCatalogTarget(display.pending.targetRequest, target))) {
    return { kind: 'negotiated' };
  }
  return {
    kind: 'fast',
    state: {
      displayed: ready.identity,
      commandTarget: ready.identity,
      pending: null,
      nextGeneration: display.nextGeneration,
    },
    actions: [{ type: 'save-preference', sessionId: ready.identity.sessionId }],
    supersededGeneration: display.pending?.generation ?? null,
  };
}

export function beginSessionSwitch(
  state: SessionDisplayState,
  target: SessionSwitchTarget,
): { state: SessionDisplayState; generation: number; coalesced: boolean; replacedGeneration: number | null } {
  const pending = state.pending;
  if (pending) {
    // Same-target coalescing keys on the catalog row, never the incarnation
    // hint: see sameCatalogTarget.
    if (sameCatalogTarget(pending.targetRequest, target)) {
      return {
        state,
        generation: pending.generation,
        coalesced: true,
        replacedGeneration: null,
      };
    }
    const restored: SessionDisplayState = {
      displayed: pending.source,
      commandTarget: pending.source,
      pending: null,
      nextGeneration: state.nextGeneration,
    };
    const started = beginSessionSwitch(restored, target);
    return { ...started, replacedGeneration: pending.generation };
  }
  const generation = state.nextGeneration;
  return {
    generation,
    coalesced: false,
    replacedGeneration: null,
    state: {
      displayed: state.displayed,
      commandTarget: state.commandTarget,
      pending: {
        generation,
        source: state.commandTarget,
        targetRequest: target,
        targetIdentity: null,
        responseCursor: null,
        phase: 'opening',
        warmDisplayed: false,
      },
      nextGeneration: generation + 1,
    },
  };
}

export function failSessionSwitch(
  state: SessionDisplayState,
  generation: number,
): SessionDisplayState {
  if (!state.pending || state.pending.generation !== generation) return state;
  return {
    displayed: state.pending.source,
    commandTarget: state.pending.source,
    pending: null,
    nextGeneration: state.nextGeneration,
  };
}

function failPendingPlan(
  state: SessionDisplayState,
  generation: number,
  extra: SwitchAction[] = [],
): SessionCutPlan {
  if (!state.pending || state.pending.generation !== generation) {
    return { state, actions: extra, settlesPromise: false };
  }
  return {
    state: {
      displayed: state.pending.source,
      commandTarget: state.pending.source,
      pending: null,
      nextGeneration: state.nextGeneration,
    },
    actions: [{ type: 'fail-pending' }, ...extra],
    settlesPromise: false,
  };
}

export function acceptSwitchHandoff(
  state: SessionDisplayState,
  generation: number,
  handoff: SwitchHandoffV1,
  ready: ReadyRuntime | undefined,
  liveCursor: number = 0,
): SwitchHandoffPlan {
  const pending = state.pending;
  if (!pending || pending.generation !== generation || pending.phase !== 'opening') {
    return { state, actions: [], warm: false, stale: true };
  }
  // The handoff's runtime identity is authoritative. The catalog's
  // incarnation hint is advisory only: an LRU reopen mints a fresh
  // incarnation after the catalog was captured, so a mismatch must never
  // fail the switch. Warmth is decided below by the exact ready cache OR
  // the LIVE subscription cursor.
  const targetIdentity: RuntimeIdentity = {
    sessionId: handoff.sessionId,
    runtimeIncarnation: handoff.runtimeIncarnation,
  };
  const actions: SwitchAction[] = [];
  if (ready && !sameRuntime(ready.identity, targetIdentity)) {
    actions.push({ type: 'invalidate-runtime', identity: ready.identity });
  }
  // A→B→A defect fix: the READY cache can lag the handoff cursor while the
  // LIVE subscription for the target runtime is already caught up. Warm iff
  // the ready cursor OR the live cursor is at/past the handoff cursor.
  const warm = !!ready
    && sameRuntime(ready.identity, targetIdentity)
    && (ready.cursor >= handoff.cursor || liveCursor >= handoff.cursor);
  // Subscribe cursor (grok's min/max formula): when warm with a stale ready
  // cache, subscribe at max(ready.cursor, handoff.cursor) — the live cursor
  // is authoritative; when warm with a caught-up ready cache, subscribe at
  // the handoff cursor; a cold client subscribes from 0 so the
  // authoritative cut is a full cold-full snapshot.
  const subscribeCursor = !warm
    ? 0
    : ready.cursor >= handoff.cursor
      ? handoff.cursor
      : Math.max(ready.cursor, liveCursor);
  actions.push({ type: 'install-cursor', identity: targetIdentity, cursor: subscribeCursor });
  actions.push({ type: 'subscribe', identity: targetIdentity, cursor: subscribeCursor });
  if (warm) actions.push({ type: 'display-warm', identity: targetIdentity });
  if (typeof handoff.notice === 'string' && handoff.notice.trim() !== '') {
    actions.push({ type: 'toast-notice', notice: handoff.notice });
  }
  return {
    state: {
      displayed: warm ? targetIdentity : state.displayed,
      commandTarget: state.commandTarget,
      pending: {
        ...pending,
        targetIdentity,
        responseCursor: subscribeCursor,
        phase: 'awaiting-authoritative-cut',
        warmDisplayed: warm,
      },
      nextGeneration: state.nextGeneration,
    },
    actions,
    warm,
    stale: false,
  };
}

function replayThroughWatermark(
  events: ExactRuntimeEventFrame[],
  clientCursor: number,
  watermark: number,
): ExactRuntimeEventFrame[] | null {
  if (watermark < clientCursor) return null;
  const replay: ExactRuntimeEventFrame[] = [];
  let expected = clientCursor + 1;
  for (const event of events) {
    if (event.sequence <= clientCursor) continue;
    if (event.sequence > watermark || event.sequence !== expected) return null;
    replay.push(event);
    expected += 1;
  }
  return expected === watermark + 1 ? replay : null;
}

export function warmContinuityNeedsHistoryConfirm(
  reason: SubscriptionCutReason,
  messages: unknown,
  actions: readonly SwitchAction[],
): boolean {
  if (reason !== 'warm-continuity') return false;
  if (!actions.some((action) => action.type === 'bind-authority')) return false;
  return Array.isArray(messages) && messages.length === 0;
}

export type WarmHistoryConfirmHold = {
  source: SessionDisplayState;
  committed: SessionDisplayState;
  generation: number;
  identity: RuntimeIdentity;
  socketGeneration: number;
  preBindActions: SwitchAction[];
  commitActions: SwitchAction[];
  settlesPromise: boolean;
};

export function planWarmHistoryConfirm(
  source: SessionDisplayState,
  plan: SessionCutPlan,
  identity: RuntimeIdentity,
  socketGeneration: number,
): WarmHistoryConfirmHold | null {
  const pending = source.pending;
  if (!pending) return null;
  const bindAt = plan.actions.findIndex((action) => action.type === 'bind-authority');
  if (bindAt < 0) return null;
  const markAt = plan.actions.findIndex((action) => action.type === 'mark-ready');
  const splitAt = markAt >= 0 && markAt < bindAt ? markAt : bindAt;
  return {
    source,
    committed: plan.state,
    generation: pending.generation,
    identity,
    socketGeneration,
    preBindActions: plan.actions.slice(0, splitAt),
    commitActions: plan.actions.slice(splitAt),
    settlesPromise: plan.settlesPromise,
  };
}

export function warmHistoryConfirmIsCurrent(
  hold: WarmHistoryConfirmHold,
  current: SessionDisplayState | null,
  socketGeneration: number,
): boolean {
  const pending = current?.pending;
  if (!pending) return false;
  if (pending.generation !== hold.generation) return false;
  if (!sameRuntime(pending.targetIdentity, hold.identity)) return false;
  if (socketGeneration !== hold.socketGeneration) return false;
  return true;
}

export function bindAuthorityCursor(cutCursor: number, liveCursor: number): number {
  return liveCursor > cutCursor ? liveCursor : cutCursor;
}

export function preserveLiveBindCursor(
  actions: readonly SwitchAction[],
  liveCursor: number,
): SwitchAction[] {
  return actions.map((action) => {
    if (action.type !== 'bind-authority' && action.type !== 'mark-ready') return action;
    return { ...action, cursor: bindAuthorityCursor(action.cursor, liveCursor) };
  });
}

export function acceptedCutLiveCursor(
  events: readonly { sequence: number }[],
  watermark: number,
): number {
  if (events.length === 0) return watermark;
  return events[events.length - 1]!.sequence;
}

export function switchFailureShouldToast(origin: SessionSwitchOrigin): boolean {
  return origin === 'user' || origin === 'bootstrap';
}

export function acceptAuthoritativeCut(
  state: SessionDisplayState,
  generation: number,
  cut: SubscriptionCut,
): SessionCutPlan {
  const identity: RuntimeIdentity = {
    sessionId: cut.sessionId,
    runtimeIncarnation: cut.runtimeIncarnation,
  };
  const pending = state.pending;
  const matchesPending = !!pending
    && pending.generation === generation
    && pending.phase === 'awaiting-authoritative-cut'
    && sameRuntime(pending.targetIdentity, identity);
  if (pending && pending.generation !== generation) {
    return { state, actions: [], settlesPromise: false };
  }
  const clientCursor = matchesPending ? (pending?.responseCursor ?? 0) : 0;
  if (matchesPending && cut.watermark < clientCursor) {
    return failPendingPlan(state, generation, [{ type: 'resubscribe-zero', identity }]);
  }
  const replay = replayThroughWatermark(cut.events, clientCursor, cut.watermark);
  // Resync attests messages; replay-buffer tails are not a 1..watermark chain.
  if (cut.reason === 'cold-full' && replay === null && cut.events.length > 0) {
    if (matchesPending) return failPendingPlan(state, generation, [{ type: 'resubscribe-zero', identity }]);
    return {
      state,
      actions: [{ type: 'resubscribe-zero', identity }],
      settlesPromise: false,
    };
  }

  const actions: SwitchAction[] = [];
  if (cut.state !== undefined) {
    actions.push({ type: 'apply-state', identity, state: cut.state });
  }
  if (cut.reason === 'warm-continuity') {
    const continuityReplay = replay;
    if (continuityReplay === null) {
      if (matchesPending) return failPendingPlan(state, generation, [{ type: 'resubscribe-zero', identity }]);
      return {
        state,
        actions: [{ type: 'resubscribe-zero', identity }],
        settlesPromise: false,
      };
    }
    const messages = cut.messages;
    if (!Array.isArray(messages)) {
      if (matchesPending) return failPendingPlan(state, generation, [{ type: 'resubscribe-zero', identity }]);
      return {
        state,
        actions: [{ type: 'resubscribe-zero', identity }],
        settlesPromise: false,
      };
    }
    actions.push({
      type: 'apply-messages',
      identity,
      messages,
      preserveInflight: true,
    });
    if (continuityReplay.length > 0) {
      actions.push({ type: 'replay-events', identity, events: continuityReplay });
    }
  } else {
    const messages = cut.messages;
    if (!Array.isArray(messages)) {
      if (matchesPending) return failPendingPlan(state, generation, [{ type: 'resubscribe-zero', identity }]);
      return {
        state,
        actions: [{ type: 'resubscribe-zero', identity }],
        settlesPromise: false,
      };
    }
    actions.push({
      type: 'apply-messages',
      identity,
      messages,
      preserveInflight: cut.reason === 'cold-full',
    });
  }

  // Every valid authoritative cut advances the client cursor/ready cache for
  // the exact runtime, pending or background: a later warm handoff for a
  // background-loaded session must find the ready entry instead of
  // re-subscribing from scratch. Replay applies before mark-ready, and a
  // matching pending cut binds authority after it.
  actions.push({ type: 'mark-ready', identity, cursor: cut.watermark });

  if (!matchesPending) {
    return { state, actions, settlesPromise: false };
  }
  actions.push({ type: 'bind-authority', identity, cursor: cut.watermark });
  actions.push({ type: 'save-preference', sessionId: identity.sessionId });
  return {
    state: {
      displayed: identity,
      commandTarget: identity,
      pending: null,
      nextGeneration: state.nextGeneration,
    },
    actions,
    settlesPromise: true,
  };
}

function liveUnloadFallback(
  state: SessionDisplayState,
  identity: RuntimeIdentity,
  fallback: RuntimeIdentity | null,
): RuntimeIdentity | null {
  if (!sameRuntime(state.displayed, identity)) return state.displayed;
  if (!sameRuntime(state.commandTarget, identity)) return state.commandTarget;
  if (state.pending && !sameRuntime(state.pending.source, identity)) return state.pending.source;
  if (fallback && !sameRuntime(fallback, identity)) return fallback;
  return null;
}

export function unloadRuntime(
  state: SessionDisplayState,
  identity: RuntimeIdentity,
  fallback: RuntimeIdentity | null = null,
): SessionUnloadPlan {
  const droppedKey = runtimeKey(identity);
  const pending = state.pending;
  const pendingMatches = !!pending
    && pending.targetIdentity !== null
    && sameRuntime(pending.targetIdentity, identity);
  if (pending && pendingMatches) {
    return {
      state: {
        displayed: pending.source,
        commandTarget: pending.source,
        pending: null,
        nextGeneration: state.nextGeneration,
      },
      actions: [{ type: 'fail-pending' }],
      droppedKey,
      failedPending: true,
    };
  }
  const authorityUnloaded = sameRuntime(state.displayed, identity)
    || sameRuntime(state.commandTarget, identity);
  if (!authorityUnloaded) {
    return {
      state,
      actions: [],
      droppedKey,
      failedPending: false,
    };
  }
  const restore = liveUnloadFallback(state, identity, fallback);
  if (!restore) {
    return {
      state: null,
      actions: pending ? [{ type: 'fail-pending' }] : [],
      droppedKey,
      failedPending: !!pending,
    };
  }
  return {
    state: {
      displayed: restore,
      commandTarget: restore,
      pending: null,
      nextGeneration: state.nextGeneration,
    },
    actions: pending ? [{ type: 'fail-pending' }] : [],
    droppedKey,
    failedPending: !!pending,
  };
}

export function evictViewSlot(
  slots: ReadonlyMap<RuntimeKey, SessionViewSlot>,
  displayed: RuntimeIdentity,
  pending: PendingSessionSwitch | null,
): RuntimeKey | null {
  if (slots.size < MAX_LOADED_SESSIONS) return null;
  const protectedKeys = new Set<RuntimeKey>([runtimeKey(displayed)]);
  if (pending?.targetIdentity) protectedKeys.add(runtimeKey(pending.targetIdentity));
  let victim: { key: RuntimeKey; lastUsed: number } | null = null;
  for (const [key, slot] of slots) {
    if (protectedKeys.has(key)) continue;
    if (!victim || slot.lastUsed < victim.lastUsed) {
      victim = { key, lastUsed: slot.lastUsed };
    }
  }
  return victim?.key ?? null;
}

export function upsertViewSlot(
  slots: ReadonlyMap<RuntimeKey, SessionViewSlot>,
  slot: SessionViewSlot,
  displayed: RuntimeIdentity,
  pending: PendingSessionSwitch | null,
): Map<RuntimeKey, SessionViewSlot> {
  const next = new Map(slots);
  const key = runtimeKey(slot.identity);
  if (next.has(key)) {
    next.set(key, slot);
    return next;
  }
  if (next.size >= MAX_LOADED_SESSIONS) {
    const victim = evictViewSlot(next, displayed, pending);
    if (victim) next.delete(victim);
  }
  if (next.size < MAX_LOADED_SESSIONS) next.set(key, slot);
  return next;
}

export interface LifecycleReadyGateInputs {
  displayed: RuntimeIdentity | null;
  commandTarget: RuntimeIdentity | null;
  pending: PendingSessionSwitch | null;
  ready: ReadyRuntime | undefined;
  socketGeneration: number;
  displayedKey: RuntimeKey | null;
  /** Items version the displayed surface last committed (null = never). */
  committedItemsVersion: number | null;
  /** Current items version of the displayed session's authoritative cache. */
  currentItemsVersion: number;
}

/** The authoritative-ready marker gate: exact authority, empty pending, current
 *  socket generation, exact displayed surface, and committedItemsVersion ===
 *  currentItemsVersion (an install-cursor empty commit never satisfies a cut). */
export function lifecycleReadyGate(g: LifecycleReadyGateInputs): boolean {
  return g.displayed !== null
    && g.pending == null
    && sameRuntime(g.commandTarget, g.displayed)
    && sameRuntime(g.ready?.identity, g.displayed)
    && g.ready?.socketGeneration === g.socketGeneration
    && g.displayedKey !== null
    && g.displayedKey === runtimeKey(g.displayed)
    && g.committedItemsVersion !== null
    && g.committedItemsVersion === g.currentItemsVersion;
}
