/** Composer context-window usage meter — pure wire parsing + TUI-parity
 *  formatting, shared by App.tsx and the node-runnable regression test
 *  (scripts/contextMeter.test.ts). No DOM/browser dependency so
 *  `esbuild --platform=node` can bundle the test in isolation.
 *
 *  The meter mirrors the TUI composer footer's context segment
 *  (crates/pi-cli/src/tui.rs `footer_context_text` + `footer_token_k`):
 *  the backend serves the SAME `get_session_stats` -> `SessionStats` wire
 *  (crates/pi-coding/src/session.rs, serde camelCase), and a session with no
 *  model (or a model without a context window) renders nothing.
 */

export interface ContextUsageWire {
  tokens?: number | null;
  contextWindow: number;
  percent?: number | null;
}

/** `get_session_stats` targeted at the OWNING session so a background
 *  meter refresh cannot query the active session. */
export function sessionStatsCommand(sid: string): { type: 'get_session_stats'; sessionId: string } {
  return { type: 'get_session_stats', sessionId: sid };
}

export interface ContextUsageRequestCut {
  sequence: number;
  bySessionId: Record<string, number>;
}

export function beginContextUsageRequest(
  bySessionId: Record<string, number>,
  sessionId: string,
): ContextUsageRequestCut {
  const sequence = (bySessionId[sessionId] ?? 0) + 1;
  return { sequence, bySessionId: { ...bySessionId, [sessionId]: sequence } };
}

export function ownsContextUsageResponse(
  bySessionId: Record<string, number>,
  sessionId: string,
  sequence: number,
): boolean {
  return bySessionId[sessionId] === sequence;
}

/** Parse the optional `contextUsage` block of a `get_session_stats`
 *  response. Returns `null` (`hidden` on the meter) when the block is absent
 *  or the model has no context window — exactly the TUI's render-nothing
 *  rule — and clamps malformed numeric fields to bounded values so a hostile
 *  snapshot can never push NaN/Infinity into the DOM. */
export function parseContextUsage(data: unknown): ContextUsageWire | null {
  const record = (data && typeof data === 'object' ? data : {}) as { contextUsage?: unknown };
  const raw = (record.contextUsage && typeof record.contextUsage === 'object'
    ? record.contextUsage
    : null) as { tokens?: unknown; contextWindow?: unknown; percent?: unknown } | null;
  if (!raw) return null;
  const contextWindow =
    typeof raw.contextWindow === 'number' && Number.isFinite(raw.contextWindow)
      ? Math.floor(raw.contextWindow)
      : 0;
  if (contextWindow <= 0) return null;
  // `tokens` may be legitimately null (no usage yet) but never NaN/±∞.
  const tokens =
    typeof raw.tokens === 'number' && Number.isFinite(raw.tokens)
      ? Math.max(0, Math.floor(raw.tokens))
      : null;
  const percent =
    typeof raw.percent === 'number' && Number.isFinite(raw.percent) ? raw.percent : null;
  return { tokens, contextWindow, percent };
}

/** Compact a token count the TUI footer way: `84k` past a thousand, trun-
 *  cating (no rounding) exactly like `tui.rs footer_token_k`. */
function tokenK(n: number): string {
  return n >= 1000 ? `${Math.floor(n / 1000)}k` : `${n}`;
}

/** Meter text, e.g. `84k/200k`; `''` when there is nothing to render. The
 *  visible surface is compact used/limit only — the percentage lives in the
 *  accessible name/title (`contextMeterAccessible`). */
export function formatContextMeter(usage: ContextUsageWire | null): string {
  if (!usage || usage.contextWindow <= 0) return '';
  const tokens = usage.tokens ?? 0;
  return `${tokenK(tokens)}/${tokenK(usage.contextWindow)}`;
}

/** M-abbreviate past a million (`1M`) — the TUI tokenK style continued one
 *  scale up, so a 1,000,000-token window reads `1M`, not `1000k`. */
export function contextMeterTotalK(n: number): string {
  return n >= 1000000 ? `${Math.floor(n / 1000000)}M` : tokenK(n);
}

/** Mobile-tier meter parts (≤720px composer row): the thin usage bar fill
 *  percentage (rounded, clamped to [0, 100]) and the abbreviated total.
 *  `null` when there is nothing to render — same render-nothing rule as the
 *  text form. */
export function contextMeterMobile(usage: ContextUsageWire | null): { percent: number; total: string } | null {
  if (!usage || usage.contextWindow <= 0) return null;
  const percent = Math.min(100, Math.max(0, Math.round(usage.percent ?? 0)));
  return { percent, total: contextMeterTotalK(usage.contextWindow) };
}

/** Accessible name/title for the meter: full percentage plus the exact
 *  bounded used/limit values (raw numbers, not k-compacted), e.g.
 *  `42% 84000/200000`; `''` when there is nothing to render. Percent mirrors
 *  the TUI: missing -> 0, rounded, clamped to [0, 999]. */
export function contextMeterAccessible(usage: ContextUsageWire | null): string {
  if (!usage || usage.contextWindow <= 0) return '';
  const percent = Math.min(999, Math.max(0, Math.round(usage.percent ?? 0)));
  const tokens = usage.tokens ?? 0;
  return `${percent}% ${tokens}/${usage.contextWindow}`;
}