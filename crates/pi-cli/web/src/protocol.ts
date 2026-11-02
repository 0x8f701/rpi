/**
 * Web control-plane protocol helpers — per-page command-id namespacing and
 * subscription-snapshot cut application. Shared by App.tsx sendCommand /
 * onMessage and the node-runnable regression test (scripts/protocol.test.ts).
 * No DOM/WebSocket dependency so `esbuild --platform=node` can bundle the
 * test in isolation.
 *
 * Command ids: the resident MultiSessionRuntimeManager ledger keys on the
 * client-supplied id. A page that minted `c1`/`c2` used to mint the same
 * ids after reload, colliding with still-resident ledger entries
 * ("command id was already used for a different request"). Every id now
 * carries a stable per-page client namespace so a reload cannot reuse
 * another page's counter.
 *
 * Subscription bootstrap: a `subscription_snapshot` at cursor 0 already
 * carries the authoritative `messages` transcript plus the protocol replay
 * buffer. Replaying those pre-snapshot transcript events duplicated
 * completed-turn user/assistant rows. The snapshot cut is backend-
 * authoritative; the client applies messages once and only replays events
 * when a reconnect cursor needs catch-up.
 */

import { catalogRuntimeSessionId, selectSessionFromCatalog, type SessionPreferenceRow } from './sessionPreference';
import { parseSubscriptionCut, sameRuntime, type RuntimeIdentity, type SubscriptionCut } from './sessionSwitch';
import { isBusyCapacityError, type ExactRuntimeEventFrame } from './types';

/** Bytes of entropy in the per-page client namespace (12 base36 chars). */
const PAGE_NAMESPACE_BYTES = 6;

function defaultRandomBytes(n: number): Uint8Array {
  const out = new Uint8Array(n);
  const cryptoObj = (globalThis as { crypto?: { getRandomValues?: (target: Uint8Array) => Uint8Array } }).crypto;
  if (cryptoObj && typeof cryptoObj.getRandomValues === 'function') {
    cryptoObj.getRandomValues(out);
    return out;
  }
  for (let i = 0; i < n; i += 1) {
    out[i] = Math.floor(Math.random() * 256);
  }
  return out;
}

/** Mint a stable per-page client namespace. Inject `randomBytes` in tests. */
export function createPageClientNamespace(
  randomBytes: (n: number) => Uint8Array = defaultRandomBytes,
): string {
  const bytes = randomBytes(PAGE_NAMESPACE_BYTES);
  let namespace = '';
  for (let i = 0; i < bytes.length; i += 1) {
    namespace += (bytes[i] as number).toString(36).padStart(2, '0').slice(-2);
  }
  return namespace;
}

/** Command id for `seq` under a per-page namespace. Never the bare `c<N>` shape. */
export function nextCommandId(namespace: string, seq: number): string {
  return `c${namespace}-${seq}`;
}

/**
 * Fixed maximum length of the namespaced command id sendCommand assigns
 * (`c<namespace>-<seq>`). Real ids are far shorter in any real session, but
 * the pre-dispatch frame-byte check must NEVER undercount.
 */
export const MAX_COMMAND_ID = `c${'z'.repeat(PAGE_NAMESPACE_BYTES * 2)}-${'9'.repeat(23)}`;

export type SubscriptionSnapshotFrame = SubscriptionCut;

export interface SubscriptionApplyPlan {
  identity: { sessionId: string; runtimeIncarnation: string };
  applyMessages: boolean;
  replayEvents: ExactRuntimeEventFrame[];
  nextCursor: number;
}

/** Validate and plan one exact-runtime authoritative cut. */
export function planSubscriptionSnapshot(
  clientCursor: number,
  value: unknown,
): SubscriptionApplyPlan {
  if (!Number.isSafeInteger(clientCursor) || clientCursor < 0) {
    throw new TypeError('clientCursor must be a nonnegative safe integer');
  }
  const snapshot = parseSubscriptionCut(value);
  const identity = {
    sessionId: snapshot.sessionId,
    runtimeIncarnation: snapshot.runtimeIncarnation,
  };
  if (snapshot.reason !== 'warm-continuity') {
    return {
      identity,
      applyMessages: true,
      replayEvents: [],
      nextCursor: snapshot.watermark,
    };
  }
  if (snapshot.watermark < clientCursor) {
    throw new TypeError('warm-continuity watermark regresses behind client cursor');
  }
  const replayEvents: ExactRuntimeEventFrame[] = [];
  let expected = clientCursor + 1;
  for (const event of snapshot.events) {
    if (event.sequence <= clientCursor) continue;
    if (event.sequence > snapshot.watermark || event.sequence !== expected) {
      throw new TypeError('warm-continuity events are not contiguous through watermark');
    }
    replayEvents.push(event);
    expected += 1;
  }
  if (expected !== snapshot.watermark + 1) {
    throw new TypeError('warm-continuity events end before watermark');
  }
  return {
    identity,
    applyMessages: true,
    replayEvents,
    nextCursor: snapshot.watermark,
  };
}

/** A dispatched subscription cut with the client-computed authority verdict
 *  stamped by `runners.ts` (from `planSubscriptionSnapshot`, before the
 *  cursor advances). Only an authoritative cut may replace a transcript base
 *  or re-fetch panel-owned state. */
export interface SubscriptionCutFrame extends SubscriptionCut {
  /** Stamped from planSubscriptionSnapshot; absent on unstamped frames. */
  authoritative?: boolean;
}

/** Type guard for a dispatched cut whose messages may replace the transcript
 *  base. Reconnect snapshots (flagged non-authoritative) and unstamped
 *  frames never qualify; a cut without a `messages` array has no usable
 *  authority to apply. */
export function isAuthoritativeSubscriptionCut(
  frame: unknown,
): frame is SubscriptionCutFrame & { messages: unknown[] } {
  if (!frame || typeof frame !== 'object') return false;
  const record = frame as Partial<SubscriptionCutFrame>;
  if (record.type !== 'subscription_snapshot' && record.type !== 'session_resync') return false;
  return record.authoritative === true && Array.isArray(record.messages);
}

/** Sessions that must be restored after a WebSocket reconnect. Include both
 * loaded snapshots and remembered cursors: a background session may have a
 * cursor even when its latest snapshot was evicted from the active view. */
export function reconnectSubscriptionIds(
  loaded: Iterable<string>,
  cursorSessionIds: Iterable<string> = [],
  active: string | null | undefined = null,
): string[] {
  const ids = [...loaded, ...cursorSessionIds];
  if (active) ids.push(active);
  return Array.from(new Set(ids.filter((sid) => sid !== '')));
}

export interface ReconnectBootstrapPlan {
  firstCommand: { type: 'get_state' };
  deferredSessionIds: string[];
}

/** Every socket generation discovers the listener primary before emitting a
 * stale client subscription. Candidate ids remain deferred until session_list
 * confirms that the new listener has actually loaded them. */
export function planReconnectBootstrap(
  loaded: Iterable<string>,
  cursorSessionIds: Iterable<string>,
  active: string | null | undefined,
): ReconnectBootstrapPlan {
  return {
    firstCommand: { type: 'get_state' },
    deferredSessionIds: reconnectSubscriptionIds(loaded, cursorSessionIds, active),
  };
}

export interface ReconnectDisplayedCatchup {
  refillSessionId: string;
  displayed: RuntimeIdentity;
  subscribe: RuntimeIdentity[];
}

export function planReconnectDisplayedCatchup(
  primary: RuntimeIdentity,
  displayed: RuntimeIdentity | null,
): ReconnectDisplayedCatchup {
  const displayedIdentity = displayed ?? primary;
  const subscribe = sameRuntime(displayedIdentity, primary)
    ? [primary]
    : [primary, displayedIdentity];
  return {
    refillSessionId: displayedIdentity.sessionId,
    displayed: displayedIdentity,
    subscribe,
  };
}

export function reconnectSubscribeCursor(stored: number | undefined): number {
  if (typeof stored !== 'number' || !Number.isFinite(stored) || stored < 0) return 0;
  return Math.floor(stored);
}

export function reconnectRefillFailureToast(error: unknown): string {
  const raw = error instanceof Error ? error.message : String(error);
  const detail = raw.trim() === '' ? 'request failed' : raw;
  return `reconnect refill failed: ${detail}`;
}

export function applyReconnectRefillCatch(
  error: unknown,
  staleAbort: unknown,
  toast: (message: string, isError: boolean) => void,
): void {
  if (error === staleAbort) throw error;
  if (isBusyCapacityError(error)) return;
  toast(reconnectRefillFailureToast(error), true);
}

export function planReconnectRefillFailure(input: {
  stillOwned: boolean;
  error: unknown;
  staleAbort: unknown;
}): 'silent' | 'rethrow-stale' | 'toast-and-recover' {
  if (!input.stillOwned) return 'silent';
  if (input.error === input.staleAbort) return 'rethrow-stale';
  if (isBusyCapacityError(input.error)) return 'silent';
  return 'toast-and-recover';
}

export function reconnectRefillFinallyApplies(startedSlot: number, currentSlot: number): boolean {
  return startedSlot > 0 && startedSlot === currentSlot;
}

export function planDisplayedIncarnationRebind(
  displayed: { displayed: RuntimeIdentity; commandTarget: RuntimeIdentity } | null,
  identity: RuntimeIdentity,
): { displayed: RuntimeIdentity; commandTarget: RuntimeIdentity } | null {
  if (!displayed) return null;
  if (displayed.displayed.sessionId !== identity.sessionId) return null;
  if (displayed.displayed.runtimeIncarnation === identity.runtimeIncarnation) return null;
  return {
    displayed: identity,
    commandTarget: displayed.commandTarget.sessionId === identity.sessionId
      ? identity
      : displayed.commandTarget,
  };
}

export function planReconnectDisplayedResync(input: {
  boundSessionId: string | null | undefined;
  displayedSessionId: string | null | undefined;
}): 'keep' | 'resync' {
  if (typeof input.displayedSessionId !== 'string' || input.displayedSessionId === '') return 'keep';
  if (input.boundSessionId === input.displayedSessionId) return 'keep';
  return 'resync';
}

export function reconnectSavedSwitchIsRedundant(
  targetRuntimeSessionId: string,
  primarySessionId: string,
  displayedSessionId: string | undefined,
  catalogTarget?: { loaded?: boolean; runtimeIncarnation?: string | null },
): boolean {
  if (targetRuntimeSessionId === '' || targetRuntimeSessionId === primarySessionId) return false;
  if (displayedSessionId !== targetRuntimeSessionId) return false;
  if (catalogTarget?.loaded !== true) return false;
  const incarnation = catalogTarget.runtimeIncarnation;
  if (typeof incarnation !== 'string' || incarnation === '') return false;
  return true;
}

export function sessionSnapshotMatchesResident(
  wireIncarnation: unknown,
  residentIncarnation: string | undefined,
): boolean {
  if (typeof wireIncarnation !== 'string' || wireIncarnation === '') return false;
  if (residentIncarnation === undefined || residentIncarnation === '') return false;
  return residentIncarnation === wireIncarnation;
}

export type ReconnectRefillHold = {
  sessionId: string;
  runtimeIncarnation: string;
  socketGeneration: number;
};

export function reconnectRefillStillOwned(
  hold: ReconnectRefillHold,
  residentIncarnation: string | undefined,
  socketGenerationNow: number,
): boolean {
  if (hold.sessionId === '' || hold.runtimeIncarnation === '') return false;
  if (hold.socketGeneration !== socketGenerationNow) return false;
  return residentIncarnation === hold.runtimeIncarnation;
}

export function adoptReconnectRefillHold(
  hold: ReconnectRefillHold,
  residentSessionId: string,
  residentIncarnation: string | undefined,
  socketGenerationNow: number,
): ReconnectRefillHold {
  if (hold.sessionId !== residentSessionId) return hold;
  if (hold.socketGeneration !== socketGenerationNow) return hold;
  if (typeof residentIncarnation !== 'string' || residentIncarnation === '') return hold;
  if (hold.runtimeIncarnation === residentIncarnation) return hold;
  return { ...hold, runtimeIncarnation: residentIncarnation };
}

export function planReconnectRefillStep(input: {
  hold: ReconnectRefillHold;
  residentIncarnation: string | undefined;
  socketGenerationNow: number;
  wireIncarnation: unknown;
  displayedSessionId?: string | null;
}): 'reject' | 'apply' {
  if (!reconnectRefillStillOwned(
    input.hold,
    input.residentIncarnation,
    input.socketGenerationNow,
  )) {
    return 'reject';
  }
  const snapshot = planSessionSnapshotApply({
    shouldActivate: false,
    wireIncarnation: input.wireIncarnation,
    residentIncarnation: input.residentIncarnation,
    displayedSessionId: input.displayedSessionId,
    targetSessionId: input.hold.sessionId,
  });
  return snapshot.apply ? 'apply' : 'reject';
}

export function commandRejectedBySwitchPending(
  switchPending: boolean,
  opts?: { auxiliary?: boolean; allowDuringSwitch?: boolean },
): boolean {
  return switchPending && opts?.auxiliary !== true && opts?.allowDuringSwitch !== true;
}

export function planSessionSnapshotApply(input: {
  shouldActivate: boolean;
  wireIncarnation: unknown;
  residentIncarnation: string | undefined;
  displayedSessionId: string | null | undefined;
  targetSessionId: string;
}): { apply: boolean; activate: boolean } {
  if (!input.shouldActivate && !sessionSnapshotMatchesResident(
    input.wireIncarnation,
    input.residentIncarnation,
  )) {
    return { apply: false, activate: false };
  }
  return {
    apply: true,
    activate: input.shouldActivate && input.displayedSessionId !== input.targetSessionId,
  };
}

/** Nested RpcSessionState (subscription cut / lifecycle) omits
 *  runtimeIncarnation; get_state injects it only on the RPC envelope.
 *  Stamp the already-authenticated identity so shouldActivate=false apply
 *  can match the resident overlay. Never overwrite a nonempty wire value. */
export function stampSessionStateIncarnation(
  state: unknown,
  runtimeIncarnation: string,
): unknown {
  if (typeof runtimeIncarnation !== 'string' || runtimeIncarnation === '') return state;
  if (!state || typeof state !== 'object' || Array.isArray(state)) {
    return state;
  }
  const record = state as Record<string, unknown>;
  if (typeof record.runtimeIncarnation === 'string' && record.runtimeIncarnation !== '') {
    return state;
  }
  return { ...record, runtimeIncarnation };
}

export interface ReconnectCatalogRow extends SessionPreferenceRow {
  source?: string;
  status?: string;
  nativeSessionId?: string;
  nativePath?: string;
  loaded?: boolean;
  runtimeIncarnation?: string | null;
}

export interface ReconnectCatalogRestoreOptions {
  rows: readonly ReconnectCatalogRow[];
  savedSessionId: string;
  primarySessionId: string;
  displayedSessionId?: string;
  displayedRuntimeIncarnation?: string;
  switchSession: (target: ReconnectCatalogRow) => Promise<string | null>;
  subscribeSession: (identity: { sessionId: string; runtimeIncarnation: string }) => void;
  /** Retire stale cached identities before the saved-target switch; fail closed on incomplete rosters. */
  retireResident?: (plan: ReconnectRosterPlan) => void;
}

export interface ReconnectRosterPlan {
  /** Loaded runtime identities keyed by native session id (`already_imported` → nativeSessionId). */
  authoritative: ReadonlyMap<string, string>;
  /** Absence is evidence of replacement only when every loaded row had a unique usable identity. */
  omissionSafe: boolean;
}

/** Plan retirement against a successful session_list roster. Conflicting incarnations fail closed. */
export function planReconnectRoster(
  rows: readonly ReconnectCatalogRow[],
): ReconnectRosterPlan {
  const authoritative = new Map<string, string>();
  const conflicted = new Set<string>();
  let validated = true;
  for (const row of rows) {
    if (row?.loaded !== true) continue;
    const runtimeSessionId = catalogRuntimeSessionId(row);
    if (runtimeSessionId === ''
      || typeof row.runtimeIncarnation !== 'string' || row.runtimeIncarnation === '') {
      validated = false;
      continue;
    }
    if (conflicted.has(runtimeSessionId)) continue;
    const existing = authoritative.get(runtimeSessionId);
    if (existing === undefined) {
      authoritative.set(runtimeSessionId, row.runtimeIncarnation);
      continue;
    }
    if (existing !== row.runtimeIncarnation) {
      authoritative.delete(runtimeSessionId);
      conflicted.add(runtimeSessionId);
      validated = false;
    }
  }
  return { authoritative, omissionSafe: validated && authoritative.size > 0 };
}

/** Restore a saved session only after primary discovery. With no saved
 * preference the listener-selected primary remains active; catalog ordering
 * must not override an explicit `--resume` or the fresh primary runtime.
 * Retire stale cached identities before the switch and before any subscription.
 * A saved target is then activated, and every other catalog row currently
 * loaded by the new listener is subscribed with its exact runtime identity.
 * Rows are consumed in catalog order; aliases resolving to the same runtime
 * keep the first row. The bound primary and already-active target are skipped. */
export function shouldPersistPrimaryPreference(
  savedSessionId: string,
  restoredTarget: SessionPreferenceRow | null,
): boolean {
  return restoredTarget === null && savedSessionId.trim() === '';
}

export function bootstrapSavedSwitchNeedsRetry(
  savedSessionId: string,
  switchSucceeded: boolean,
): boolean {
  return savedSessionId.trim() !== '' && !switchSucceeded;
}

export async function restoreReconnectCatalog(
  options: ReconnectCatalogRestoreOptions,
): Promise<SessionPreferenceRow | null> {
  options.retireResident?.(planReconnectRoster(options.rows));
  const selected = options.savedSessionId.trim() === ''
    ? null
    : selectSessionFromCatalog(options.rows, options.savedSessionId);
  const target = selected
    ? options.rows.find((row) => row === selected) ?? null
    : null;
  let activeSessionId = options.primarySessionId;
  if (target && catalogRuntimeSessionId(target) !== options.primarySessionId) {
    const targetRuntimeSessionId = catalogRuntimeSessionId(target);
    if (reconnectSavedSwitchIsRedundant(
      targetRuntimeSessionId,
      options.primarySessionId,
      options.displayedSessionId,
      target,
    )) {
      activeSessionId = targetRuntimeSessionId;
    } else {
      const boundSessionId = await options.switchSession(target);
      if (boundSessionId) activeSessionId = boundSessionId;
    }
  }
  const skipSessionIds = new Set<string>([options.primarySessionId, activeSessionId]);
  const seenSessionIds = new Set<string>();
  for (const row of options.rows) {
    if (row?.loaded !== true) continue;
    const runtimeSessionId = catalogRuntimeSessionId(row);
    if (runtimeSessionId === '') continue;
    if (typeof row.runtimeIncarnation !== 'string' || row.runtimeIncarnation === '') continue;
    if (skipSessionIds.has(runtimeSessionId)) {
      if (
        runtimeSessionId === options.displayedSessionId
        && typeof options.displayedRuntimeIncarnation === 'string'
        && options.displayedRuntimeIncarnation !== ''
        && options.displayedRuntimeIncarnation !== row.runtimeIncarnation
      ) {
        options.subscribeSession({
          sessionId: runtimeSessionId,
          runtimeIncarnation: row.runtimeIncarnation,
        });
      }
      continue;
    }
    if (seenSessionIds.has(runtimeSessionId)) continue;
    seenSessionIds.add(runtimeSessionId);
    options.subscribeSession({ sessionId: runtimeSessionId, runtimeIncarnation: row.runtimeIncarnation });
  }
  return target;
}
