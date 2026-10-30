/**
 * Per-session compacting progress — the Web mirror of TUI `is_compacting`.
 *
 * `/compact` / `/compact --snap` wait for the RPC report (wait_for_idle +
 * summarization). The UI must paint immediately from optimistic local state,
 * then reconcile from `get_state.isCompacting`. Duplicate/stream dispatch is
 * rejected locally like the TUI match-arm (`!is_streaming && !is_compacting`),
 * never silently queued. Other sessions stay independent.
 *
 * DOM/React-free so node tests can drive the book without mounting App.
 */

export type CompactMode = 'llm' | 'snap';

/** Per-session compacting book. `token` is the local in-flight generation.
 * `settled` suppresses server snapshots captured before the matching RPC
 * completed until an authoritative false snapshot arrives. */
export type CompactBook = {
  compacting: Record<string, boolean>;
  mode: Record<string, CompactMode>;
  token: Record<string, number>;
  settled: Record<string, boolean>;
};

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

/** Optimistic start: bump the session token and mark compacting immediately. */
export function beginCompact(
  book: CompactBook,
  sid: string,
  mode: CompactMode,
): { book: CompactBook; token: number } {
  const token = (book.token[sid] ?? 0) + 1;
  return {
    token,
    book: {
      compacting: { ...book.compacting, [sid]: true },
      mode: { ...book.mode, [sid]: mode },
      token: { ...book.token, [sid]: token },
      settled: { ...book.settled, [sid]: false },
    },
  };
}

/** Settlement/finally: clear only the matching token so a newer compact
 *  (or a late timeout from a superseded attempt) cannot wipe live progress. */
export function settleCompact(book: CompactBook, sid: string, token: number): CompactBook {
  if (book.token[sid] !== token) return book;
  const compacting = { ...book.compacting, [sid]: false };
  const mode = { ...book.mode };
  delete mode[sid];
  const tokens = { ...book.token };
  delete tokens[sid];
  return {
    compacting,
    mode,
    token: tokens,
    settled: { ...book.settled, [sid]: true },
  };
}

/**
 * Reconcile from `get_state.isCompacting`.
 * - `true` adopts server/other-client activity unless a just-settled local RPC
 *   is waiting for a confirming false snapshot.
 * - `false` clears server state only when there is no local in-flight token and
 *   releases the post-settlement suppression marker.
 */
export function reconcileCompacting(
  book: CompactBook,
  sid: string,
  isCompacting: boolean,
): CompactBook {
  if (isCompacting) {
    if (book.settled[sid] === true) return book;
    if (book.compacting[sid] === true) {
      return book.mode[sid] ? book : {
        compacting: book.compacting,
        mode: { ...book.mode, [sid]: 'llm' },
        token: book.token,
        settled: book.settled,
      };
    }
    return {
      compacting: { ...book.compacting, [sid]: true },
      mode: { ...book.mode, [sid]: book.mode[sid] ?? 'llm' },
      token: book.token,
      settled: book.settled,
    };
  }
  const settled = { ...book.settled };
  delete settled[sid];
  if (book.token[sid] != null || book.compacting[sid] !== true) {
    return Object.keys(settled).length === Object.keys(book.settled).length
      ? book
      : { ...book, settled };
  }
  const compacting = { ...book.compacting, [sid]: false };
  const mode = { ...book.mode };
  delete mode[sid];
  return { compacting, mode, token: book.token, settled };
}
