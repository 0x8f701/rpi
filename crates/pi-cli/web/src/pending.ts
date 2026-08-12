/**
 * Per-command timeout classes and the pending-command registry for the Web RPC
 * lifecycle — pure and node-testable (no DOM/browser dependency), mirroring
 * ./socket.ts's relationship with App.tsx: the registry owns the id -> pending
 * map and the (injectable) timeout timers; App owns the socket, the response
 * routing, and the optimistic-bubble side effects.
 *
 * Server ack contract this encodes (evidence: crates/pi-cli/src/modes/rpc.rs
 * dispatch arms):
 * - MOST commands are fast acks: the arm responds as soon as it completes
 *   (get_state, session_list, task_spawn, code_review_open/snapshot/refresh —
 *   snapshot/fork only, no LLM, code_review.rs, settings_*, workflow_*,
 *   skill, side_chat_prompt, prompt/steer/follow_up, ...). 30s is the default
 *   bound. prompt responds before the model turn: the turn is `tokio::spawn`'ed
 *   and streamed as events. Its default pre-spawn classifier is disabled and
 *   configured for a 4s provider deadline. Explicit settings can lengthen
 *   server work, but unknown and fast-ack commands deliberately retain this
 *   strict client bound rather than masking a stalled acceptance path.
 * - `compact` / `snapcompact` arms AWAIT the whole compaction before
 *   responding with the report: `app.compact()` is `wait_for_idle()` (waits
 *   for the in-flight turn to END — unbounded) plus provider summarization
 *   (`session.compact`, COMPACTION_SUMMARIZE_TIMEOUT-bounded per call). A
 *   legit compaction takes minutes, so the old fixed 30s timed out real
 *   `/compact` uses and the late report was dropped. LONG_OP class.
 *
 * Every class stays BOUNDED (nothing waits forever): a dead server still
 * surfaces as a timeout that names the command and the elapsed seconds, while
 * a dropped connection rejects pending for the CONNECTION reason (never
 * "timed out"). A CURRENT-generation fast-ack timeout is treated as an
 * unresponsive transport, not a command stall: the socket is OPEN but the
 * server swallowed the outbound frame, so the 30s pending timer fired before
 * the 60s liveness timer. The registry fires onTransportStale so the App
 * closes/replaces the socket and reconnects through the existing onclose
 * path, and rejects with a truthful connection-unresponsive error. A
 * stale-generation fast-ack timeout never closes the current socket, and a
 * long-op timeout never eagerly reconnects — both stay plain bounded
 * timeouts.
 */

/** Fast-ack commands: the server replies within this window or it is stuck. */
export const DEFAULT_COMMAND_TIMEOUT_MS = 30_000;

/** compact/snapcompact: the response IS the full compaction report, which is
 *  legitimately minutes behind (wait_for_idle + summarization). */
export const LONG_OP_TIMEOUT_MS = 10 * 60_000;

/** Commands whose server response is the full long-running result. */
const LONG_OP_TYPES: Record<string, true> = {
  compact: true,
  snapcompact: true,
};

/** The bounded timeout class for a command type. Unknown types (and any
 *  future fast-ack command) fall back to the default — a new long-running
 *  command must be added to the table above explicitly so it cannot silently
 *  inherit the fast-ack bound. */
export function commandTimeoutMs(type: string): number {
  if (LONG_OP_TYPES[type]) return LONG_OP_TIMEOUT_MS;
  return DEFAULT_COMMAND_TIMEOUT_MS;
}

/** User-facing timeout message: names the command and the elapsed seconds so
 *  "command timed out" stops hiding which command stalled and for how long.
 *  Used for the PLAIN bounded timeout path: long-op commands (compact/
 *  snapcompact, legitimately minutes behind) and stale-generation fast-ack
 *  commands whose socket was already replaced/closed — neither of which
 *  should close the current transport (see onTransportStale for the
 *  current-generation fast-ack path that does). */
export function timeoutErrorMessage(type: string, sentAt: number, now: number): string {
  const elapsed = Math.max(0, Math.round((now - sentAt) / 1000));
  return `${type} timed out after ${elapsed}s (no response from server)`;
}

/** User-facing message for a transport-stale fast-ack timeout: the socket is
 *  OPEN but the server swallowed the outbound frame (no ack within the
 *  fast-ack window), so the 30s pending timer fired before the 60s liveness
 *  timer could. Truthful about the failure mode (unresponsive transport) and
 *  the recovery (reconnecting), instead of the misleading "command timed out"
 *  that hides a dead-but-OPEN connection behind a per-command timeout. */
export function transportStaleMessage(type: string, sentAt: number, now: number): string {
  const elapsed = Math.max(0, Math.round((now - sentAt) / 1000));
  return `connection unresponsive after ${elapsed}s (no ack for ${type}); reconnecting`;
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
  /** Socket generation this command was sent on; the App settles it only when
   *  it matches the CURRENT generation (see ./socket.ts isCurrentPending). */
  gen: number;
  /** Wire command name ("prompt", "compact", ...) for the timeout message. */
  type: string;
  /** Stamp set by add() from the scheduler clock. */
  sentAt: number;
  timeoutMs: number;
  timer: unknown;
}

export interface PendingRegistryOptions {
  scheduler: PendingScheduler;
  /** Whether a pending entry's generation is the CURRENT socket generation at
   *  timeout time. A CURRENT-generation fast-ack timeout is treated as an
   *  unresponsive transport (the socket is OPEN but the server swallowed the
   *  outbound frame; the 30s pending timer fired before the 60s liveness
   *  timer): onTransportStale fires and the promise rejects with a truthful
   *  connection-unresponsive error so the App can close/replace the socket and
   *  schedule a reconnect through the existing onclose path. A STALE-gen
   *  timeout (the socket was already replaced/closed) must NEVER close the
   *  current socket — it rejects as a plain timeout and the newer socket is
   *  untouched. Defaults to "always current" so a test that does not model
   *  generations still observes transport-stale on a fast-ack timeout when
   *  onTransportStale is provided; the real App passes the live predicate. */
  isCurrentGen?: (gen: number) => boolean;
  /** Called right before a PLAIN timed-out entry (a long-op command, or a
   *  stale-generation fast-ack whose socket was already replaced/closed) is
   *  rejected. The App removes the optimistic bubble here: a plain timeout
   *  means the server never answered AND the transport is not being closed,
   *  so the bubble must not linger as if the message were accepted. Drain/
   *  replace paths do NOT call this — the frame may already have been
   *  delivered, and the bubble stays for reconnect/retry semantics. */
  onTimeout?: (entry: PendingEntry) => void;
  /** Called INSTEAD of onTimeout when a CURRENT-generation fast-ack command
   *  times out — the socket is OPEN but unresponsive (the 30s pending timer
   *  beat the 60s liveness timer; the server swallowed the frame). The App
   *  closes the socket here so the existing onclose path drains remaining
   *  pending and schedules a reconnect; the timed-out command's promise then
   *  rejects with a connection-unresponsive error (NOT a generic command
   *  timeout). Bubbles are NOT removed here (close/retry semantics — the
   *  frame may yet have been delivered, and the reconnect path owns retry),
   *  matching the onclose drain. Long-op timeouts and stale-gen timeouts
   *  never fire this — they stay plain bounded timeouts so a legitimately
   *  long compaction cannot trigger an eager reconnect. */
  onTransportStale?: (entry: PendingEntry) => void;
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
      // Already settled/drained before the deadline: the timeout must not
      // double-reject (e.g. a drained command on a replaced socket).
      if (this.entries.get(id) !== full) return;
      this.entries.delete(id);
      const now = this.options.scheduler.now();
      // Fail closed on a dead-but-OPEN socket. A CURRENT-generation fast-ack
      // timeout means the server swallowed the outbound frame: the 30s
      // pending timer fired while the socket stayed OPEN and before the 60s
      // liveness timer. Treat it as an unresponsive transport — fire
      // onTransportStale so the App closes/replaces the socket and schedules a
      // reconnect through the existing onclose path, and reject with a
      // truthful connection-unresponsive error instead of a generic command
      // timeout. A STALE generation (the socket was already replaced/closed)
      // must NEVER close the current socket: it falls through to the plain
      // timeout path and the newer socket is untouched. A long-op command
      // (compact/snapcompact) is legitimately minutes behind and must NOT
      // eagerly reconnect just because it ran long: it stays a plain bounded
      // timeout. onTransportStale absent (e.g. a primitive registry test) also
      // falls through, preserving the plain-timeout contract.
      const isCurrent = this.options.isCurrentGen ? this.options.isCurrentGen(full.gen) : true;
      const fastAck = full.timeoutMs !== LONG_OP_TIMEOUT_MS;
      if (fastAck && isCurrent && this.options.onTransportStale) {
        // Transport stale: this socket is OPEN but unresponsive (the 30s
        // pending timer beat the 60s liveness timer; the server swallowed the
        // frame). Fire onTransportStale ONCE so the App closes/replaces the
        // socket and reconnects through the existing onclose path, and reject
        // with a truthful connection-unresponsive error. Then eagerly settle
        // every OTHER current-generation fast-ack pending on the same dead
        // socket: they would fire their own onTransportStale moments later
        // (duplicate close/toast) or sit unbounded until the async onclose
        // drain. Each is deleted + timer-cleared + rejected with the same
        // truthful message — bounded, truthful, no double settlement, and
        // onTransportStale fires exactly once per generation. Long-op entries
        // on the same socket are LEFT for the onclose drain (their 10-minute
        // bound is a legitimately long class; the imminent close drains them
        // with the connection reason, bubbles kept). Stale-gen entries belong
        // to a replaced socket and stay out of this loop.
        this.options.onTransportStale(full);
        full.reject(new Error(transportStaleMessage(full.type, full.sentAt, now)));
        for (const [otherId, other] of this.entries) {
          if (other.gen !== full.gen) continue;
          if (other.timeoutMs === LONG_OP_TIMEOUT_MS) continue; // long-op: onclose drains it
          this.entries.delete(otherId);
          this.options.scheduler.clearTimeout(other.timer);
          other.reject(new Error(transportStaleMessage(other.type, other.sentAt, now)));
        }
        return;
      }
      this.options.onTimeout?.(full);
      full.reject(new Error(timeoutErrorMessage(full.type, full.sentAt, now)));
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

  /** Reject + delete every entry NOT on keepGen — the socket replace/drop
   *  drain. Bubbles are intentionally NOT removed (no onTimeout): the frame
   *  may already have been delivered, and the reconnect path owns retry
   *  semantics. */
  drainExcept(keepGen: number, reason: string, rpc = false): void {
    for (const [id, entry] of this.entries) {
      if (entry.gen === keepGen) continue;
      this.entries.delete(id);
      this.options.scheduler.clearTimeout(entry.timer);
      const error = new Error(reason);
      if (rpc) (error as Error & { rpc?: boolean }).rpc = true;
      entry.reject(error);
    }
  }

  /** Reject + delete EVERY entry (host switch / full app reset). */
  drainAll(reason: string, rpc = false): void {
    for (const [id, entry] of this.entries) {
      this.entries.delete(id);
      this.options.scheduler.clearTimeout(entry.timer);
      const error = new Error(reason);
      if (rpc) (error as Error & { rpc?: boolean }).rpc = true;
      entry.reject(error);
    }
  }
}
