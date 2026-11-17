/**
 * Per-runtime compacting progress — the Web mirror of TUI `is_compacting`.
 *
 * The backend owns the lifecycle: `compaction_start` / `compaction_end` are
 * published on the exact-runtime stream (crates/pi-coding/src/session.rs), and
 * an authoritative subscription cut carries `isCompacting` plus the ordered
 * events that led to it. The UI paints optimistically from a local `/compact`
 * dispatch, then takes the accepted events/cuts as authority:
 *
 * - every ACCEPTED end settles the runtime (success, abort, error, willRetry);
 *   only a later start begins a new compaction;
 * - an ordinary watermark-less `get_state` never arms or disarms this book;
 * - slots are keyed by the exact `runtimeKey(identity)`, so a retired
 *   incarnation's finally/event/cut cannot touch the session's next runtime.
 *
 * DOM/React-free so node tests can drive the book without mounting App.
 */
import { runtimeKey, type RuntimeIdentity, type RuntimeKey } from './sessionSwitch';
import { isCompactionEvent, type CompactionEvent, type ExactRuntimeEventFrame } from './types';

export type CompactMode = 'llm' | 'snap';

/** One exact runtime's compact state.
 *  - `requestGeneration` is the local `/compact` generation (the existing
 *    monotonic command counter, `seqRef`) that owns the optimistic paint, or
 *    null when no local request is outstanding.
 *  - `sequence` is the last projected compaction event sequence for this
 *    runtime: the same monotonic stream discipline as the live event gate, so
 *    a replayed/reordered start can never re-arm an accepted end. */
export type CompactSlot = {
  compacting: boolean;
  mode: CompactMode | null;
  requestGeneration: number | null;
  sequence: number;
};

export type CompactBook = { byRuntime: Record<RuntimeKey, CompactSlot> };

export function emptyCompactBook(): CompactBook {
  return { byRuntime: {} };
}

/** TUI loop-style reject: compact while a turn streams. */
export const COMPACT_BUSY_STREAMING =
  '/compact is unavailable while another turn is running';

/** Duplicate compact while this session is already compacting. */
export const COMPACT_BUSY_COMPACTING =
  '/compact is unavailable while compaction is already running';

export function compactAdmission(flags: {
  streaming: boolean;
  compacting: boolean;
}): { ok: true } | { ok: false; message: string } {
  if (flags.streaming) return { ok: false, message: COMPACT_BUSY_STREAMING };
  if (flags.compacting) return { ok: false, message: COMPACT_BUSY_COMPACTING };
  return { ok: true };
}

/** Composer Send/Enter while this session is compacting. Streaming still
 *  uses the existing Stop/steer path; only compacting blocks send. */
export const COMPACT_SEND_BLOCKED = COMPACT_BUSY_COMPACTING;

export function compactSendAdmission(compacting: boolean):
  | { ok: true }
  | { ok: false; message: string } {
  if (compacting) return { ok: false, message: COMPACT_SEND_BLOCKED };
  return { ok: true };
}


/** Visible chrome copy. Snap is named so the deterministic archive is not
 *  mistaken for an LLM summarization. */
export function compactStatusText(mode: CompactMode | null | undefined): string {
  return mode === 'snap' ? 'Snapcompacting context…' : 'Compacting context…';
}

export function compactBadgeLabel(mode: CompactMode | null | undefined): string {
  return mode === 'snap' ? 'snapcompacting' : 'compacting';
}

/** Display projection for one exact runtime (badge, send gate, aria copy). */
export function compactView(
  book: CompactBook,
  identity: RuntimeIdentity | null | undefined,
): { compacting: boolean; mode: CompactMode | null } {
  if (!identity) return { compacting: false, mode: null };
  const slot = book.byRuntime[runtimeKey(identity)];
  if (!slot || slot.compacting !== true) return { compacting: false, mode: null };
  return { compacting: true, mode: slot.mode };
}

/** Optimistic start: paint this runtime immediately under `requestGeneration`.
 *  The App reserves that generation from the existing monotonic command
 *  counter, so a superseded attempt's finally can never match a newer round. */
export function beginCompact(
  book: CompactBook,
  identity: RuntimeIdentity,
  mode: CompactMode,
  requestGeneration: number,
): CompactBook {
  const key = runtimeKey(identity);
  const previous = book.byRuntime[key];
  return {
    byRuntime: {
      ...book.byRuntime,
      [key]: {
        compacting: true,
        mode,
        requestGeneration,
        sequence: previous?.sequence ?? 0,
      },
    },
  };
}

/** RPC finally: clear only the exact identity AND generation that started this
 *  paint. A no-op once an accepted end already settled the runtime, and never
 *  able to clear a newer round or a different incarnation. */
export function settleCompact(
  book: CompactBook,
  identity: RuntimeIdentity,
  requestGeneration: number,
): CompactBook {
  const key = runtimeKey(identity);
  const slot = book.byRuntime[key];
  if (!slot || slot.requestGeneration !== requestGeneration) return book;
  return {
    byRuntime: {
      ...book.byRuntime,
      [key]: { ...slot, compacting: false, mode: null, requestGeneration: null },
    },
  };
}

/** Project one compaction event onto a slot, without deciding whether the
 *  event is fresh (that ordering call belongs to the caller). */
function projectEvent(slot: CompactSlot | undefined, event: CompactionEvent): CompactSlot {
  const sequence = Math.max(slot?.sequence ?? 0, event.sequence);
  if (event.type === 'compaction_start') {
    // The wire carries no mode: keep a locally dispatched snap while that
    // request is still outstanding, else the truthful default.
    const mode: CompactMode = slot && slot.requestGeneration !== null ? (slot.mode ?? 'llm') : 'llm';
    return { compacting: true, mode, requestGeneration: slot?.requestGeneration ?? null, sequence };
  }
  return { compacting: false, mode: null, requestGeneration: null, sequence };
}

/** Project one ACCEPTED exact-runtime compaction event. App calls this only
 *  after the live gate validated identity, incarnation, and sequence.
 *
 *  Every accepted end settles the runtime — success, abort, error, and
 *  `willRetry` alike; the retry's next start begins a fresh compaction. */
export function applyCompactionEvent(book: CompactBook, event: CompactionEvent): CompactBook {
  const key = runtimeKey(event);
  const previous = book.byRuntime[key];
  if (previous && event.sequence <= previous.sequence) return book;
  return { byRuntime: { ...book.byRuntime, [key]: projectEvent(previous, event) } };
}

/** Take over compact state from one ACCEPTED authoritative cut: the cut state
 *  is the baseline at `watermark`, then the cut's ordered compaction events
 *  win (an end in the same cut beats a guard-lagged `isCompacting: true`), so
 *  the projection is independent of the stream-only cold replay subset.
 *
 *  - a `false` baseline never erases a local optimistic request whose own
 *    event/RPC has not landed yet (the cut predates that request's outcome);
 *  - a baseline older than the last projected compaction event cannot re-arm
 *    an already settled runtime;
 *  - a cut END beats a guard-lagged `isCompacting: true` only while it does
 *    not contradict newer knowledge: an end older than our cursor never
 *    applies, an end AT our cursor applies only when that cursor came from an
 *    idle runtime with no outstanding local request, and an already-projected
 *    START is never re-applied. */
export function reconcileCompactCut(
  book: CompactBook,
  identity: RuntimeIdentity,
  state: unknown,
  events: readonly ExactRuntimeEventFrame[],
  watermark: number,
): CompactBook {
  const key = runtimeKey(identity);
  const previous = book.byRuntime[key];
  const baseSequence = previous?.sequence ?? 0;
  const snapshot =
    state !== null && typeof state === 'object' && 'isCompacting' in state
      ? state.isCompacting
      : undefined;
  let next = book;
  if (typeof snapshot === 'boolean' && watermark >= baseSequence) {
    if (snapshot === true) {
      const mode: CompactMode =
        previous && previous.requestGeneration !== null ? (previous.mode ?? 'llm') : 'llm';
      next = {
        byRuntime: {
          ...next.byRuntime,
          [key]: {
            compacting: true,
            mode,
            requestGeneration: previous?.requestGeneration ?? null,
            sequence: baseSequence,
          },
        },
      };
    } else if (!previous || previous.requestGeneration === null) {
      next = {
        byRuntime: {
          ...next.byRuntime,
          [key]: {
            compacting: false,
            mode: null,
            requestGeneration: null,
            sequence: baseSequence,
          },
        },
      };
    }
  }
  // Only compaction events are projected: the full cut event list must never
  // be replayed through App's onEvent (it would duplicate tools/messages).
  const projections = events
    .filter(isCompactionEvent)
    .sort((left, right) => left.sequence - right.sequence);
  for (const event of projections) {
    // A cut re-states its range, so an end we already projected may beat a
    // guard-lagged `isCompacting: true`. But that authority is bounded:
    //  - the cut may only speak about its own range (events <= watermark);
    //  - a start at/before our cursor never begins another compaction;
    //  - an end OLDER than our cursor cannot clear a newer start we projected;
    //  - an end AT our cursor only beats the baseline when that cursor came
    //    from an idle runtime with no outstanding local request (otherwise it
    //    is the previous round's end and must not clear a newer local round).
    const staleStart = event.type === 'compaction_start' && event.sequence <= baseSequence;
    const staleEnd = event.type === 'compaction_end'
      && (event.sequence < baseSequence
        || (event.sequence === baseSequence
          && previous !== undefined
          && (previous.compacting === true || previous.requestGeneration !== null)));
    if (event.sequence > watermark || staleStart || staleEnd) continue;
    const slot = next.byRuntime[key];
    next = { byRuntime: { ...next.byRuntime, [key]: projectEvent(slot, event) } };
  }
  return next;
}

/** Runtime invalidation: drop exactly this incarnation's slot so a retired
 *  runtime's finally/event/cut cannot touch the session's next runtime. */
export function removeCompactRuntime(book: CompactBook, identity: RuntimeIdentity): CompactBook {
  const key = runtimeKey(identity);
  if (!(key in book.byRuntime)) return book;
  const byRuntime = { ...book.byRuntime };
  delete byRuntime[key];
  return { byRuntime };
}
