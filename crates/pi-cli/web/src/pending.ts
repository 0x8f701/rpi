/**
 * Per-command timeout classes and the pending-command registry for the Web RPC
 * lifecycle. Command latency is independent from WebSocket liveness: every
 * command remains bounded, but a command timeout rejects only that command.
 * The App heartbeat's ping/pong/silence window is the sole authority allowed
 * to classify an otherwise-open transport as unresponsive and reconnect it.
 *
 * Most RPCs acknowledge within 30 seconds. Commands whose response includes
 * bounded git acquisition, application forking, full compaction, or a full
 * durable transcript (`get_messages`) use the explicit long-operation class.
 * A dropped connection still drains pending entries immediately with the
 * connection reason, and late responses after a timeout remain no-ops because
 * timeout settlement deletes the entry.
 */

import type { RpcErrorCode } from './types';

/** Fast-ack commands: the server replies within this window or it is stuck. */
export const DEFAULT_COMMAND_TIMEOUT_MS = 30_000;

/** Long operations remain bounded without inheriting transport authority. */
export const LONG_OP_TIMEOUT_MS = 10 * 60_000;

/** Commands whose server response includes legitimately long bounded work. */
const LONG_OP_TYPES: Record<string, true> = {
  compact: true,
  snapcompact: true,
  get_messages: true,
  code_review_open: true,
  code_review_refresh: true,
  code_review_file_diff: true,
  code_review_auto_review: true,
  code_review_highlight: true,
  code_review_stop: true,
};

/** The bounded timeout class for a command type. Unknown types (and any
 *  future fast-ack command) fall back to the default — a new long-running
 *  command must be added to the table above explicitly so it cannot silently
 *  inherit the fast-ack bound. */
export function commandTimeoutMs(type: string): number {
  if (LONG_OP_TYPES[type]) return LONG_OP_TIMEOUT_MS;
  return DEFAULT_COMMAND_TIMEOUT_MS;
}


/** User-facing timeout message: names the command and elapsed seconds. */
export function timeoutErrorMessage(type: string, sentAt: number, now: number): string {
  const elapsed = Math.max(0, Math.round((now - sentAt) / 1000));
  return `${type} timed out after ${elapsed}s (no response from server)`;
}

/** Code Review panel-owned RPCs: background loads only (snapshot polls,
 *  per-file paging/context). Every other code_review_* command is a USER
 *  action whose failures belong to the shared App toast. */
const PANEL_OWNED_CODE_REVIEW_TYPES: Readonly<Record<string, true>> = {
  code_review_snapshot: true,
  code_review_file_diff: true,
  code_review_file_context: true,
};

/** Panel-owned RPCs surface their own errors; App must not duplicate a toast. */
export function isPanelOwnedRpc(type: string): boolean {
  return PANEL_OWNED_CODE_REVIEW_TYPES[type] === true
    || type.startsWith('todo_child_')
    || type.startsWith('file_explorer_')
    // Every workflow_* command is owned by the Workflow panel: list failures
    // land in the list error row, detail failures in the detail pane, action
    // (cancel/remove/…) failures in the detail error row, IRC send failures
    // in the IRC composer. The App layer must never duplicate them as a
    // global toast — in particular a stale `workflow_detail` "workflow was
    // not found" after an authoritative remove stays fully silent.
    || type.startsWith('workflow_')
    // Path completion is owned by the PathAutocomplete input: its promise
    // rejects into the component's quiet-failure catch (menu hides, manual
    // entry unaffected); the App must never duplicate a global toast.
    || type === 'path_complete'
    || type === 'job_list'
    || type === 'task_spawn'
    || type === 'job_cancel'
    || type === 'hub_send'
    || type === 'job_output'
    || type === 'agent_history';
}

/** Lifecycle commands whose SUCCESS envelope is TARGET-owned: the backend
 *  tags the response with the target runtime's sessionId (the caller routes
 *  an immediate follow-up fork by it), while failures stay request-source-
 *  owned. App.onResponse accepts the target-owned envelope ONLY when the
 *  envelope sessionId equals the payload sessionId (nonempty); any forged
 *  mismatch rejects exactly once. */
const TARGET_OWNED_LIFECYCLE_TYPES: Readonly<Record<string, true>> = {
  new_session: true,
  fork: true,
  clone: true,
  session_fork_latest: true,
};

/** Whether a command type's successful response envelope is target-owned. */
export function isTargetOwnedLifecycle(type: string): boolean {
  return TARGET_OWNED_LIFECYCLE_TYPES[type] === true;
}

/** Whether onResponse should toast an RPC failure. Auxiliary pending entries
 *  (context-meter `get_session_stats`) and panel-owned commands stay silent
 *  at the App layer; the promise still rejects for the caller. */
export function rpcFailureToastOwned(opts: {
  type: string;
  boot: boolean;
  sessionId?: string;
  activeSessionId?: string | null;
  auxiliary?: boolean;
}): boolean {
  // switch_session failures are App-switch-owned: requestSessionSwitch rolls
  // the transaction back and emits the single user toast itself (bootstrap
  // stays silent). The global RPC layer must never double-toast it.
  if (opts.type === 'switch_session') return false;
  if (opts.boot || opts.auxiliary === true) return false;
  if ((opts.type === 'stt_transcribe'
    || opts.type === 'realtime_create_call'
    || opts.type === 'realtime_stop')
    && opts.sessionId !== opts.activeSessionId) {
    return false;
  }
  if (isPanelOwnedRpc(opts.type)) return false;
  return true;
}

/** A wire response frame relevant to the ownership decision. */
export interface RpcResponseOwnershipFrame {
  command?: string;
  sessionId?: string;
  success?: boolean;
  data?: unknown;
}

export type ResponseOwnershipVerdict =
  | 'accept'
  | 'command-mismatch'
  | 'source-ownership-mismatch'
  | 'target-ownership-mismatch';

/** Exact rejection message per non-accept verdict (App + test simulator
 *  share the map so the wire-visible text stays in lockstep). */
export const RESPONSE_OWNERSHIP_REJECTIONS: Readonly<
  Record<Exclude<ResponseOwnershipVerdict, 'accept'>, string>
> = {
  'command-mismatch': 'rpc response command ownership mismatch',
  'source-ownership-mismatch': 'rpc response session ownership mismatch',
  'target-ownership-mismatch': 'rpc response target ownership mismatch',
};

/** Ownership gate App.onResponse applies BEFORE settling a pending entry.
 *  Command identity is checked first: a frame that does not NAME the pending
 *  command can never own it. The target-owned exemption exists only for a
 *  SUCCESS envelope of a lifecycle command (verified against the payload
 *  sessionId); everything else — ordinary commands AND lifecycle failures,
 *  which the backend tags with the request source — is request-source-owned. */
export function classifyResponseOwnership(
  frame: RpcResponseOwnershipFrame,
  pending: Pick<PendingEntry, 'type' | 'sessionId'>,
): ResponseOwnershipVerdict {
  if (frame.command !== pending.type) return 'command-mismatch';
  const data: unknown = frame.data;
  const payloadSessionId = data !== null && typeof data === 'object' && 'sessionId' in data
    && typeof data.sessionId === 'string'
    ? data.sessionId
    : '';
  if (frame.success === true && isTargetOwnedLifecycle(pending.type)) {
    // The envelope is accepted ONLY when it equals the payload sessionId
    // (nonempty); a forged mismatch rejects exactly once with nothing
    // mutated — no activation, no overlay, no catalog bump.
    if (payloadSessionId === '' || frame.sessionId !== payloadSessionId) {
      return 'target-ownership-mismatch';
    }
    return 'accept';
  }
  if (pending.sessionId && frame.sessionId !== pending.sessionId) {
    return 'source-ownership-mismatch';
  }
  return 'accept';
}


/** Injectable timer/clock so the node test drives time deterministically and
 *  the browser build uses window.setTimeout/Date.now. */
export interface PendingScheduler {
  setTimeout(fn: () => void, ms: number): unknown;
  clearTimeout(timer: unknown): void;
  now(): number;
}

export interface PendingEntry {
  id: string;
  resolve: (value: unknown) => void;
  reject: (reason: Error) => void;
  bubbleId?: string;
  sessionId?: string;
  turnId?: string;
  /** Socket generation this command was sent on; the App settles it only when
   *  it matches the CURRENT generation (see ./socket.ts isCurrentPending). */
  gen: number;
  /** Wire command name ("prompt", "compact", ...) for the timeout message. */
  type: string;
  /** Stamp set by add() from the scheduler clock. */
  sentAt: number;
  timeoutMs: number;
  timer: unknown;
  /** True for background auxiliary RPCs whose failures must reject quietly. */
  auxiliary?: boolean;

}

export interface PendingRegistryOptions {
  scheduler: PendingScheduler;
  /** Called before a timed-out entry is rejected with the exact timeout
   * message the registry computed; the returned boolean (`undefined` =
   * unowned) stamps the rejected error's `toastOwned`. Drains never invoke
   * it (the frame may already have been delivered). */
  onTimeout?: (entry: PendingEntry, timeoutErrorMessage: string) => boolean | void;
}

export interface PendingCommandError extends Error {
  rpc?: boolean;
  toastOwned?: boolean;
  rpcCode?: RpcErrorCode;
}

function pendingCommandError(message: string, rpc = false, toastOwned = false): PendingCommandError {
  const error: PendingCommandError = new Error(message);
  if (rpc) error.rpc = true;
  if (toastOwned) error.toastOwned = true;
  return error;
}

/** True only when the error is structurally stamped `toastOwned: true` (the App already surfaced it). */
export function isToastOwnedRpcError(error: unknown): boolean {
  return (error as { toastOwned?: unknown } | null | undefined)?.toastOwned === true;
}

/**
 * The id -> pending map with per-command bounded timeout timers. Settlement
 * (response, timeout, drain) always removes the entry and clears its timer, so
 * a late response after a timeout or a drain is a no-op (take() -> undefined).
 */
export class PendingRegistry {
  private readonly entries = new Map<string, PendingEntry>();

  constructor(private readonly options: PendingRegistryOptions) {}

  get size(): number {
    return this.entries.size;
  }

  has(id: string): boolean {
    return this.entries.has(id);
  }

  /** Register a command and arm its (bounded, per-class) timeout. */
  add(id: string, entry: Omit<PendingEntry, 'id' | 'sentAt' | 'timer' | 'timeoutMs'>): void {
    const sentAt = this.options.scheduler.now();
    const full: PendingEntry = {
      ...entry,
      id,
      sentAt,
      timeoutMs: commandTimeoutMs(entry.type),
      timer: 0,
    };
    full.timer = this.options.scheduler.setTimeout(() => {
      if (this.entries.get(id) !== full) return;
      this.entries.delete(id);
      const message = timeoutErrorMessage(full.type, full.sentAt, this.options.scheduler.now());
      const toastOwned = this.options.onTimeout?.(full, message) === true;
      full.reject(pendingCommandError(message, false, toastOwned));
    }, full.timeoutMs);
    this.entries.set(id, full);
  }

  /** Delete + clear the timer, returning the entry. The settlement path: the
   *  App checks the generation (isCurrentPending) and resolves/rejects. After
   *  a timeout or a drain, take() returns undefined — a late response is
   *  ignored, and a drained entry can never be double-settled. */
  take(id: string): PendingEntry | undefined {
    const entry = this.entries.get(id);
    if (!entry) return undefined;
    this.entries.delete(id);
    this.options.scheduler.clearTimeout(entry.timer);
    return entry;
  }

  /** Reject + delete every entry NOT on keepGen. `toastOwned` is true only
   *  when the caller already surfaced `reason`; consumers can then suppress
   *  their local duplicate while unowned immediate failures still render. */
  drainExcept(keepGen: number, reason: string, rpc = false, toastOwned = false): void {
    for (const [id, entry] of this.entries) {
      if (entry.gen === keepGen) continue;
      this.entries.delete(id);
      this.options.scheduler.clearTimeout(entry.timer);
      entry.reject(pendingCommandError(reason, rpc, toastOwned));
    }
  }

  /** Reject every entry, optionally marking an already-surfaced error. */
  drainAll(reason: string, rpc = false, toastOwned = false): void {
    for (const [id, entry] of this.entries) {
      this.entries.delete(id);
      this.options.scheduler.clearTimeout(entry.timer);
      entry.reject(pendingCommandError(reason, rpc, toastOwned));
    }
  }
}
