/**
 * O(1) live transcript viewport state for the exact displayed runtime.
 *
 * A fast switch away from a session must save that session's view WITHOUT a
 * full transcript DOM scan (the frontend-only fast path decides eligibility
 * BEFORE any save), so the scroll handler and fold toggles maintain the
 * viewport state with a CONSTANT number of DOM operations per event:
 * every scroll event resolves the anchor through bounded engine hit-test
 * probes (never a sibling walk or children scan — a first→last scrollbar
 * jump costs the same handful of operations as a one-pixel nudge), and every
 * fold toggle reads the toggled element's own stamped key. The switch
 * save/restore paths consume these production functions directly — the
 * switch harness drives them with fake elements in plain Node.
 */

import { isPinned } from './scrollPin';
import type { RuntimeKey } from './sessionSwitch';

/** Geometry a transcript content child exposes (real DOMRect satisfies
 *  this structurally; the test harness fabricates the same shape). */
export interface TranscriptAnchorRect {
  top: number;
  bottom: number;
}

/** The minimal element surface the anchor resolution reads: one stamped
 *  attribute, bounded ancestor links, and geometry. Real DOM elements
 *  satisfy this structurally. */
export interface TranscriptHitNode {
  getAttribute(name: string): string | null;
  readonly parentElement: TranscriptHitNode | null;
  getBoundingClientRect(): TranscriptAnchorRect;
}

/** The engine hit-test surface (real `document.elementsFromPoint`). */
export interface TranscriptHitSurface {
  elementsFromPoint(x: number, y: number): TranscriptHitNode[];
}

/** The live viewport state of the DISPLAYED transcript, maintained by App
 *  (scroll handler + fold-toggle capture listener) and read by a fast
 *  switch's save. The anchor is re-resolved per event from element
 *  identity; folds are maintained by stamped-key capture. */
export interface LiveTranscriptViewport {
  /** Bottom-pin state at the last observed scroll. */
  follow: boolean;
  /** First content child whose bottom edge is below the viewport top. */
  anchorItemId: string | null;
  /** The anchor child's top edge offset from the viewport top (px). */
  anchorOffsetPx: number;
  fallbackScrollTop: number;
  /** Open fold keys (see foldKey) of the displayed transcript. */
  openFoldKeys: Set<string>;
}

/** Stable slot key for one foldable `<details>` inside one transcript item:
 *  item id + fold index within the item + class name (one item may carry
 *  several fold groups; the class disambiguates index shifts). */
export function foldKey(itemId: string, foldIndex: number, className: string): string {
  return `${itemId}:${foldIndex}:${className}`;
}

/* ------------------------------------------------------------------ *
 * Production stamping API — the ONE writer of both stamp attributes
 * ------------------------------------------------------------------ */

/** Registry of every item/fold stamp the production API emitted, keyed
 *  `<attribute>=<value>`. Renders merely accumulate (React re-renders
 *  re-stamp the same keys); the stamp-coverage test resets the registry
 *  and asserts the exact unique key set over real component renders. */
const transcriptStamps = new Set<string>();

/** Clear the production stamp registry (stamp-coverage test isolation). */
export function resetTranscriptStamps(): void {
  transcriptStamps.clear();
}

/** Every item/fold key stamped through the API since the last reset. */
export function stampedTranscriptKeys(): ReadonlySet<string> {
  return transcriptStamps;
}

/** Stamp one transcript item root. The ONLY producer of
 *  `data-item-id`: every rendered transcript content child spreads this,
 *  so the O(1) anchor probe, the negotiated scan, and the anchor restore
 *  read exactly the keys produced here. */
export function transcriptItemStamp(itemId: string): { 'data-item-id': string } {
  if (itemId === '') throw new Error('transcriptItemStamp: empty item id');
  transcriptStamps.add(`${TRANSCRIPT_ITEM_ATTRIBUTE}=${itemId}`);
  const stamp: { 'data-item-id': string } = { [TRANSCRIPT_ITEM_ATTRIBUTE]: itemId };
  return stamp;
}

/** Stamp one transcript fold (`<details>`). The ONLY producer of
 *  `data-fold-key`: the full `foldKey` (item id + fold index + class) so
 *  the toggle capture listener and the restore read exactly the keys
 *  produced here. */
export function transcriptFoldStamp(
  itemId: string,
  foldIndex: number,
  className: string,
): { 'data-fold-key': string } {
  if (itemId === '') throw new Error('transcriptFoldStamp: empty item id');
  const key = foldKey(itemId, foldIndex, className);
  transcriptStamps.add(`${TRANSCRIPT_FOLD_ATTRIBUTE}=${key}`);
  const stamp: { 'data-fold-key': string } = { [TRANSCRIPT_FOLD_ATTRIBUTE]: key };
  return stamp;
}


/** Full-scan anchor derivation (the negotiated-switch save and the live-state
 *  seed): the FIRST content child whose bottom edge is still below the
 *  viewport top is the anchor; its index maps 1:1 onto the rendered items. */
export function computeAnchorIndex(
  children: ArrayLike<{ getBoundingClientRect(): TranscriptAnchorRect }>,
  viewportTop: number,
): number {
  for (let index = 0; index < children.length; index += 1) {
    if (children[index].getBoundingClientRect().bottom > viewportTop) return index;
  }
  return -1;
}

/* ------------------------------------------------------------------ *
 * O(1) scroll-event anchor resolution
 * ------------------------------------------------------------------ */

/** Stable attribute carrying the owning transcript item id on every rendered
 *  content child. The scroll probe resolves the anchor's identity directly
 *  from element identity — never from a positional index. */
export const TRANSCRIPT_ITEM_ATTRIBUTE = 'data-item-id';

/** Stable attribute stamped on every transcript fold (`<details>`), carrying
 *  its full `foldKey` so the toggle capture listener needs no enumeration. */
export const TRANSCRIPT_FOLD_ATTRIBUTE = 'data-fold-key';

/** Selector for stamped folds (scan + restore paths only; the O(1) event
 *  paths never enumerate). */
export const TRANSCRIPT_FOLDS_SELECTOR = `details[${TRANSCRIPT_FOLD_ATTRIBUTE}]`;

/** Downward probe span (px) covering the widest gap between content children:
 *  the 8px row gap plus the largest inter-turn user-message margin (24px).
 *  A viewport top landing inside a gap still resolves the first child below
 *  it within this bound. */
export const VIEWPORT_ANCHOR_PROBE_MAX_PX = 40;
const VIEWPORT_ANCHOR_PROBE_STEP_PX = 8;

/** Horizontal probe inset (px) from the transcript CONTENT wrapper's left
 *  and right edges. Grounded on the stylesheet: `.transcript-content` owns
 *  the centered column with 16px/20px edge padding, and the #transcript
 *  scrollbar (14–18px) lives OUTSIDE the content wrapper's rect, so a ±24px
 *  inset lands INSIDE the children while never touching the scrollbar, the
 *  centered-column margins, or the content padding strip. The right column
 *  is what lets a narrow right-aligned user bubble (`.msg--user` is
 *  `align-self: flex-end`) win the anchor over a wider later item. */
export const VIEWPORT_ANCHOR_PROBE_X_INSET_PX = 24;

/** The rect the probe grid derives from: composed by the caller from TWO
 *  surfaces — the Y origin is the SCROLL CONTAINER's rect top (the viewport
 *  top: elementsFromPoint only hits boxes inside the viewport, so a probe
 *  anchored at the content wrapper's top would probe above the viewport once
 *  scrolled away and resolve a NULL anchor for every parked view), while the
 *  x-span comes from the transcript CONTENT wrapper (its columns span the
 *  rendered children; the container's rect would include the scrollbar and
 *  the centered-column margins). The negotiated full scan reads the same
 *  viewport top from the container. */
export interface TranscriptProbeRect {
  top: number;
  left: number;
  width: number;
}

/** Climb ancestors to the nearest element stamped with `attribute` — bounded
 *  by DOM depth, never a sibling enumeration. */
function closestStampedElement(
  node: TranscriptHitNode | null,
  attribute: string,
): TranscriptHitNode | null {
  let current = node;
  while (current) {
    if (current.getAttribute(attribute) !== null) return current;
    current = current.parentElement;
  }
  return null;
}

export interface ResolvedAnchor {
  /** The stamped item id of the first content child below the viewport top,
   *  or null when no child overlaps it (empty transcript / past the end). */
  anchorItemId: string | null;
  /** The anchor child's top edge offset from the viewport top (px). */
  anchorOffsetPx: number;
}

/**
 * O(1) viewport anchor resolution for one scroll event. A BOUNDED probe
 * grid — six y rows (viewport top … +VIEWPORT_ANCHOR_PROBE_MAX_PX, covering
 * the widest inter-child gap) × three x columns (left inset, center, right
 * inset of the content rect) — identifies, at the FIRST y row that hits a
 * stamped item, the EARLIEST VALID child: the stamped candidate with the
 * smallest rect.top whose bottom edge is still below the viewport top. That
 * row-major earliest-wins rule is exactly the "first child whose bottom
 * edge is below the viewport top" the old full scan walked to, and it is
 * why a narrow right-aligned user bubble (hit only by the right column,
 * invisible to the center one) wins over a wider item probed lower down.
 * The probe never walks siblings and never enumerates children: a
 * first→last scrollbar jump costs the same bounded grid as a one-pixel
 * nudge — at most 6 y rows × 3 x columns hit-tests plus one rect read per
 * distinct candidate on the deciding row.
 */
export function resolveViewportAnchor(
  surface: TranscriptHitSurface,
  probe: TranscriptProbeRect,
): ResolvedAnchor {
  const columns = [
    probe.left + VIEWPORT_ANCHOR_PROBE_X_INSET_PX,
    probe.left + probe.width / 2,
    probe.left + probe.width - VIEWPORT_ANCHOR_PROBE_X_INSET_PX,
  ];
  for (let probeY = 0; probeY <= VIEWPORT_ANCHOR_PROBE_MAX_PX; probeY += VIEWPORT_ANCHOR_PROBE_STEP_PX) {
    const y = probe.top + probeY;
    let earliest: { id: string; offsetPx: number; top: number } | null = null;
    const seen = new Set<string>();
    for (const x of columns) {
      for (const element of surface.elementsFromPoint(x, y)) {
        const item = closestStampedElement(element, TRANSCRIPT_ITEM_ATTRIBUTE);
        if (!item) continue;
        const id = item.getAttribute(TRANSCRIPT_ITEM_ATTRIBUTE);
        if (id === null || id === '' || seen.has(id)) continue;
        seen.add(id);
        const rect = item.getBoundingClientRect();
        // Valid anchor premise (same contract as computeAnchorIndex): the
        // child's bottom edge is still below the viewport top.
        if (rect.bottom <= probe.top) continue;
        if (!earliest || rect.top < earliest.top) {
          earliest = { id, offsetPx: rect.top - probe.top, top: rect.top };
        }
      }
    }
    if (earliest) return { anchorItemId: earliest.id, anchorOffsetPx: earliest.offsetPx };
  }
  return { anchorItemId: null, anchorOffsetPx: 0 };
}

/* ------------------------------------------------------------------ *
 * O(1) per-event live-state note
 * ------------------------------------------------------------------ */

/** The transcript scroll container surface the live-state note reads:
 *  scrollPin's ScrollPinElement shape plus the container rect. */
export interface TranscriptScrollSurface {
  scrollTop: number;
  scrollHeight: number;
  clientHeight: number;
  getBoundingClientRect(): TranscriptAnchorRect & { left: number; width: number };
}

/** Advance the live viewport by one scroll event: resolve the anchor with a
 *  constant number of engine operations (a bounded x×y hit-test probe grid,
 *  never a sibling walk or children scan — a first→last jump costs the same
 *  as a one-pixel scroll), then stamp follow from the pin controller and
 *  fallback from the current scrollTop. Geometry must not re-derive follow
 *  (remaining 1 is already a scroll-away). Mutates `live` in place; no-op
 *  without live state or the content rect. */
export function noteLiveViewportScroll(
  live: LiveTranscriptViewport | null,
  surface: TranscriptHitSurface | null,
  transcript: TranscriptScrollSurface | null,
  contentRect: TranscriptProbeRect | null,
  following: boolean,
): void {
  if (!live || !surface || !transcript || !contentRect) return;
  const anchor = resolveViewportAnchor(surface, contentRect);
  live.anchorItemId = anchor.anchorItemId;
  live.anchorOffsetPx = anchor.anchorOffsetPx;
  live.follow = following;
  live.fallbackScrollTop = transcript.scrollTop;
}

/** One fold toggle lands in the live key set in O(1): the toggled `<details>`
 *  carries its own stamped `data-fold-key`. Returns whether the key was
 *  recorded (false for foreign details — a fold outside the stamped
 *  transcript owns no viewport state). */
export function noteLiveViewportFold(
  live: LiveTranscriptViewport | null,
  target: { getAttribute(name: string): string | null; open: boolean } | null,
): boolean {
  if (!live || !target) return false;
  const key = target.getAttribute(TRANSCRIPT_FOLD_ATTRIBUTE);
  if (key === null || key === '') return false;
  if (target.open) live.openFoldKeys.add(key);
  else live.openFoldKeys.delete(key);
  return true;
}

/* ------------------------------------------------------------------ *
 * Viewport slot state: scan / live read / restore
 * ------------------------------------------------------------------ */

/** The transcript view state a SessionViewSlot persists. */
export interface ViewportState {
  follow: boolean;
  anchorItemId: string | null;
  anchorOffsetPx: number;
  fallbackScrollTop: number;
  openFoldKeys: Set<string>;
}

/** The transcript content wrapper surface the scan/restore paths read. The
 *  O(1) event paths never touch it. Real elements satisfy this via thin
 *  adapters in App; the switch harness fabricates the same shape. */
export interface TranscriptContentRoot {
  /** Content children, 1:1 with the session's items (the empty hint is the
   *  only child when there are no items). */
  readonly children: ArrayLike<TranscriptHitNode>;
  /** Stamped folds only; never called from an O(1) event path. */
  stampedFolds(): ArrayLike<{ open: boolean; getAttribute(name: string): string | null }>;
}

/**
 * Read the viewport state for one save. `scan` (a NEGOTIATED switch, once
 * per round-trip) pays the full-scan anchor + fold derivation; the fast
 * path reads the incrementally maintained live state — O(1), no DOM reads —
 * falling back to the slot's own state when the live viewport belongs to
 * another runtime. Follow comes from the live pin stamp when present;
 * otherwise remaining 0 is following and remaining 1 is not.
 */
export function readTranscriptViewport(options: {
  live: LiveTranscriptViewport | null;
  current: Pick<ViewportState, 'anchorItemId' | 'anchorOffsetPx' | 'openFoldKeys'> | null;
  transcript: TranscriptScrollSurface | null;
  content: TranscriptContentRoot | null;
  scan: boolean;
}): ViewportState {
  const follow = options.live !== null
    ? options.live.follow
    : options.transcript
      ? isPinned(options.transcript.scrollTop, options.transcript.scrollHeight, options.transcript.clientHeight)
      : true;
  const fallbackScrollTop = options.transcript?.scrollTop ?? 0;
  if (options.scan && options.transcript && options.content) {
    const viewportTop = options.transcript.getBoundingClientRect().top;
    const children = options.content.children;
    const anchorIndex = computeAnchorIndex(children, viewportTop);
    const anchor = anchorIndex >= 0 ? children[anchorIndex] : null;
    const openFoldKeys = new Set<string>();
    for (const details of Array.from(options.content.stampedFolds())) {
      if (!details.open) continue;
      const key = details.getAttribute(TRANSCRIPT_FOLD_ATTRIBUTE);
      if (key) openFoldKeys.add(key);
    }
    return {
      follow,
      anchorItemId: anchor?.getAttribute(TRANSCRIPT_ITEM_ATTRIBUTE) ?? null,
      anchorOffsetPx: anchor ? anchor.getBoundingClientRect().top - viewportTop : 0,
      fallbackScrollTop: options.transcript.scrollTop,
      openFoldKeys,
    };
  }
  if (options.scan) {
    // A negotiated save without a mounted transcript derives an EMPTY view:
    // nothing is displayed, so no stale slot state may leak into the save.
    return { follow, anchorItemId: null, anchorOffsetPx: 0, fallbackScrollTop, openFoldKeys: new Set<string>() };
  }
  if (options.live) {
    // The live viewport is authoritative when it belongs to this runtime —
    // including a null anchor (scrolled past the end), which must NOT fall
    // back to a stale slot anchor.
    return {
      follow,
      anchorItemId: options.live.anchorItemId,
      anchorOffsetPx: options.live.anchorOffsetPx,
      fallbackScrollTop,
      openFoldKeys: new Set(options.live.openFoldKeys),
    };
  }
  return {
    follow,
    anchorItemId: options.current?.anchorItemId ?? null,
    anchorOffsetPx: options.current?.anchorOffsetPx ?? 0,
    fallbackScrollTop,
    openFoldKeys: new Set(options.current?.openFoldKeys ?? []),
  };
}

/** Seed one live viewport from a saved/restored slot state (activation,
 *  post-save reseed, post-restore reseed). The seed copies the key set: the
 *  slot owns its state, later toggles mutate the live copy only. */
export function seedLiveViewport(
  key: RuntimeKey,
  state: Pick<ViewportState, 'follow' | 'anchorItemId' | 'anchorOffsetPx' | 'fallbackScrollTop' | 'openFoldKeys'>,
): { key: RuntimeKey; viewport: LiveTranscriptViewport } {
  return {
    key,
    viewport: {
      follow: state.follow,
      anchorItemId: state.anchorItemId,
      anchorOffsetPx: state.anchorOffsetPx,
      fallbackScrollTop: state.fallbackScrollTop,
      openFoldKeys: new Set(state.openFoldKeys),
    },
  };
}

/* ------------------------------------------------------------------ *
 * Restore: write one saved viewport back into the displayed transcript
 * ------------------------------------------------------------------ */

/**
 * Write one restored viewport back into the displayed transcript: the
 * anchor item's top edge lands `anchorOffsetPx` below the viewport top
 * (fallback scrollTop when the anchor item no longer resolves), and the
 * stamped folds re-open exactly the saved keys. Runs once per switch. The
 * scroll container may be absent (a conditional mount) — the fold set still
 * applies. `follow` is the caller's pin decision — the pin controller owns
 * that write, so a following restore only re-applies folds here.
 */
export function applyRestoredViewport(
  transcript: TranscriptScrollSurface | null,
  content: TranscriptContentRoot | null,
  state: ViewportState,
): void {
  if (!state.follow && transcript) {
    const anchor = restoredAnchorElement(content, state.anchorItemId);
    transcript.scrollTop = anchor
      ? transcript.scrollTop + anchor.getBoundingClientRect().top - transcript.getBoundingClientRect().top - state.anchorOffsetPx
      : state.fallbackScrollTop;
  }
  if (content) {
    for (const details of Array.from(content.stampedFolds())) {
      const key = details.getAttribute(TRANSCRIPT_FOLD_ATTRIBUTE);
      details.open = key !== null && state.openFoldKeys.has(key);
    }
  }
}

/** Resolve one saved anchor item's element by its stamped key — one bounded
 *  scan per restore, never per scroll event. */
export function restoredAnchorElement(
  content: TranscriptContentRoot | null,
  anchorItemId: string | null,
): (TranscriptHitNode & TranscriptAnchorRect) | null {
  if (!content || !anchorItemId) return null;
  const children = content.children;
  for (let index = 0; index < children.length; index += 1) {
    const child = children[index];
    if (child.getAttribute(TRANSCRIPT_ITEM_ATTRIBUTE) === anchorItemId) return child as TranscriptHitNode & TranscriptAnchorRect;
  }
  return null;
}

/** Fixed per-surface DOM window bound; head/tail access is demand scroll, never auto-expansion. */
export const TRANSCRIPT_PROJECTION_CHUNK = 240;

export const TRANSCRIPT_PROJECTION_EDGE_PX = 24;

/** Minimal selection surface the demand-window hold reads. Real
 *  `document.getSelection()` satisfies this; tests pass a stub. */
export interface TranscriptTextSelection {
  isCollapsed: boolean;
  rangeCount?: number;
  anchorNode?: Node | null;
  focusNode?: Node | null;
  /** True when a Range covers the surface even if both endpoints sit outside it. */
  intersectsRoot?: boolean;
}

export interface TranscriptSelectionRoot {
  contains(node: Node | null): boolean;
}

/** True while a non-collapsed Range is anchored inside `root`. One-argument
 *  form is the unit-test seam. A null production root never holds. A remount
 *  while those endpoints are live drops the Range (selection collapses). */
export function liveTextSelectionHoldsProjection(
  selection: TranscriptTextSelection | null | undefined,
  root?: TranscriptSelectionRoot | null,
): boolean {
  if (selection == null) return false;
  if (selection.rangeCount === 0) return false;
  if (selection.isCollapsed !== false) return false;
  // One-arg form is the unit-test seam (no surface). A null production
  // root means the content wrapper is not mounted yet — do not hold.
  if (root === undefined) return true;
  if (root === null) return false;
  const anchor = selection.anchorNode ?? null;
  const focus = selection.focusNode ?? null;
  if ((anchor !== null && root.contains(anchor)) || (focus !== null && root.contains(focus))) return true;
  return selection.intersectsRoot === true;
}

/** Map a live `Selection` onto the hold helper, including ranges that cover
 *  the surface with both endpoints outside it (header-to-footer / select-all). */
export function readTranscriptTextSelection(
  selection: Selection | null | undefined,
  root: TranscriptSelectionRoot | null,
): TranscriptTextSelection | null {
  if (selection == null) return null;
  let intersectsRoot = false;
  if (root && selection.rangeCount > 0) {
    try {
      intersectsRoot = selection.getRangeAt(0).intersectsNode(root as Node);
    } catch {
      intersectsRoot = false;
    }
  }
  return {
    isCollapsed: selection.isCollapsed,
    rangeCount: selection.rangeCount,
    anchorNode: selection.anchorNode,
    focusNode: selection.focusNode,
    intersectsRoot,
  };
}

export interface TranscriptProjectionWindow {
  start: number;
  end: number;
  hiddenBefore: number;
  complete: boolean;
}

/** Rendered window [start, min(total, start + TRANSCRIPT_PROJECTION_CHUNK)): the chunk bound holds for any stored start. */
export function projectTranscriptWindow(total: number, start: number): TranscriptProjectionWindow {
  const safeTotal = Number.isSafeInteger(total) && total > 0 ? total : 0;
  const boundedStart = Math.max(0, Math.min(Number.isSafeInteger(start) ? start : 0, safeTotal));
  const end = Math.min(safeTotal, boundedStart + TRANSCRIPT_PROJECTION_CHUNK);
  return { start: boundedStart, end, hiddenBefore: boundedStart, complete: boundedStart === 0 && end === safeTotal };
}
export function tailProjectionStart(total: number): number {
  return Math.max(0, total - TRANSCRIPT_PROJECTION_CHUNK);
}

/** Bounded window start covering a restored anchor; a missing/non-finite index fail-closes to the tail chunk. */
export function projectionStartForAnchor(total: number, anchorIndex: number): number {
  if (!Number.isSafeInteger(total) || total <= 0) return 0;
  if (!Number.isSafeInteger(anchorIndex)) return tailProjectionStart(total);
  const anchor = Math.min(Math.max(anchorIndex, 0), total - 1);
  return Math.max(0, Math.min(anchor - (TRANSCRIPT_PROJECTION_CHUNK >> 1), tailProjectionStart(total)));
}

/** Bounded DISJOINT retained slices for a PARKED view whose saved anchor sits
 *  below the tail chunk: the surface must render BOTH the user's reading
 *  window (the anchor neighborhood) AND the session tail (the ownership
 *  marker), and no single contiguous chunk-sized window can span both. Each
 *  slice is half a chunk, so the pair never exceeds one full projection
 *  chunk of mounted items, and the slices are disjoint by construction
 *  whenever the anchor sits at least one chunk above the tail (the caller
 *  only enters slice mode in that case — a tail-chunk anchor is covered by
 *  the plain tail window). */
export function projectRetainedSlices(
  total: number,
  anchorIndex: number,
): { anchor: TranscriptProjectionWindow; tail: TranscriptProjectionWindow } {
  const safeTotal = Number.isSafeInteger(total) && total > 0 ? total : 0;
  const anchor = Math.min(Math.max(Number.isSafeInteger(anchorIndex) ? anchorIndex : 0, 0), Math.max(0, safeTotal - 1));
  const half = TRANSCRIPT_PROJECTION_CHUNK >> 1;
  const quarter = half >> 1;
  const anchorStart = Math.max(0, Math.min(anchor - quarter, Math.max(0, safeTotal - half)));
  const anchorEnd = Math.min(safeTotal, anchorStart + half);
  const tailStart = Math.max(anchorEnd, Math.max(0, safeTotal - half));
  return {
    anchor: { start: anchorStart, end: anchorEnd, hiddenBefore: anchorStart, complete: false },
    tail: { start: tailStart, end: safeTotal, hiddenBefore: tailStart, complete: false },
  };
}

/** The window plan for a PARKED (follow=false) surface with a valid saved
 *  anchor. A tail-chunk anchor rides the canonical tail window — one
 *  contiguous window covers both the anchor and the tail. An anchor BELOW
 *  the tail chunk cannot share one contiguous chunk-sized window with the
 *  tail, so the plan is bounded DISJOINT retained slices (the anchor
 *  neighborhood plus the tail) — ALWAYS, regardless of any prior stored
 *  window: a pre-existing anchor-centered or canonical-head stored window
 *  alone would render the reading position without the tail ownership
 *  marker, and the demand walk's ownership of the window is already enforced
 *  by the walk latch, so the planner needs no stored-window input. Both
 *  shapes keep the mounted total within one projection chunk. */
export type ParkedWindowPlan =
  | { kind: 'tail'; start: number }
  | { kind: 'slices'; anchor: TranscriptProjectionWindow; tail: TranscriptProjectionWindow };

export function planParkedWindow(
  total: number,
  anchorIndex: number,
): ParkedWindowPlan {
  const tailStart = tailProjectionStart(total);
  if (anchorIndex >= total - TRANSCRIPT_PROJECTION_CHUNK) {
    return { kind: 'tail', start: tailStart };
  }
  const slices = projectRetainedSlices(total, anchorIndex);
  return { kind: 'slices', anchor: slices.anchor, tail: slices.tail };
}

/** Normalization on data-length change: follow rides the tail chunk; parked keeps its start clamped. */
export function normalizeTranscriptWindow(total: number, start: number, follow: boolean): number {
  if (!Number.isSafeInteger(total) || total <= 0) return 0;
  if (follow) return tailProjectionStart(total);
  return Math.max(0, Math.min(Number.isSafeInteger(start) ? start : 0, total - 1));
}

/** Rendered overlap every full-window demand shift keeps, so the anchor node
 *  each commit measures exists in BOTH the old and new window. */
export const TRANSCRIPT_PROJECTION_SHIFT_OVERLAP = TRANSCRIPT_PROJECTION_CHUNK >> 1;

/** Demand shifts: OLDER (near top) grows a partial window to the chunk, then slides a full window by CHUNK - OVERLAP; NEWER (near bottom) slides the same clamped to the tail. */
export function shiftProjectionWindowOlder(total: number, start: number): number {
  const end = projectTranscriptWindow(total, start).end;
  if (start <= 0) return start;
  const size = end - start;
  const shift = size < TRANSCRIPT_PROJECTION_CHUNK
    ? TRANSCRIPT_PROJECTION_CHUNK - size
    : TRANSCRIPT_PROJECTION_CHUNK - TRANSCRIPT_PROJECTION_SHIFT_OVERLAP;
  return Math.max(0, start - shift);
}

export function shiftProjectionWindowNewer(total: number, start: number): number {
  const end = projectTranscriptWindow(total, start).end;
  if (end >= total) return start;
  return Math.min(start + TRANSCRIPT_PROJECTION_CHUNK - TRANSCRIPT_PROJECTION_SHIFT_OVERLAP, tailProjectionStart(total));
}

export interface DemandProjectionDecisionInput {
  total: number;
  start: number;
  follow: boolean;
  atOlderEdge: boolean;
  atNewerEdge: boolean;
  inSliceMode: boolean;
  sliceAnchor: number;
  selection: TranscriptTextSelection | null | undefined;
  root: TranscriptSelectionRoot | null;
  gestureActive: boolean;
}

export type DemandProjectionCommitKind = 'hold' | 'sliceCollapse' | 'normalize' | 'shift' | 'rest';

export interface DemandProjectionDecision {
  hold: boolean;
  /** True when decide() must take the walk latch. Slice collapse is
   *  always a commit, even when `nextStart` equals the stored start
   *  (the render still has to drop disjoint slices). */
  commit: boolean;
  kind: DemandProjectionCommitKind;
  nextStart: number;
}

/** Last painted plan vs this render's speculative plan. A hold uses the
 *  last committed snapshot when one exists. It never treats the current
 *  speculative parked/follow plan as committed (a scroll can change the
 *  live anchor before React paints). First paint with no commit yet
 *  falls through to speculative. */
export function resolveHeldRenderPlan<T>(hold: boolean, committed: T | null, speculative: T): T {
  if (hold && committed !== null) return committed;
  return speculative;
}

/** Layout-effect write: only an unheld paint may become the committed
 *  snapshot. A hold leaves the previous commit untouched. */
export function commitRenderPlanIfUnheld<T>(hold: boolean, committed: T | null, speculative: T): T | null {
  return hold ? committed : speculative;
}

/** Rendered window start. A hold keeps the last committed start and
 *  skips follow-tail normalize so a growing session cannot remount
 *  the nodes that own a live Range. */
export function projectionStartForRender(
  total: number,
  start: number,
  follow: boolean,
  hold: boolean,
): number {
  if (hold) return start;
  return normalizeTranscriptWindow(total, start, follow);
}

/** Pure demand-window decision used by TranscriptSurface. A hold keeps
 *  `start`. Normalize is its own commit and never demand-shifts in the
 *  same decision (the original decide() boundary). After the hold
 *  lifts, an already-normalized at-edge view can shift without another
 *  user scroll. */
export function planDemandProjectionDecision(input: DemandProjectionDecisionInput): DemandProjectionDecision {
  if (input.gestureActive || liveTextSelectionHoldsProjection(input.selection, input.root)) {
    return { hold: true, commit: false, kind: 'hold', nextStart: input.start };
  }
  if (input.inSliceMode) {
    const next = input.sliceAnchor >= 0 && input.sliceAnchor < input.total - TRANSCRIPT_PROJECTION_CHUNK
      ? projectionStartForAnchor(input.total, input.sliceAnchor)
      : tailProjectionStart(input.total);
    return { hold: false, commit: true, kind: 'sliceCollapse', nextStart: next };
  }
  const resynced = normalizeTranscriptWindow(input.total, input.start, input.follow);
  if (resynced !== input.start) {
    return { hold: false, commit: true, kind: 'normalize', nextStart: resynced };
  }
  if (input.follow) {
    return { hold: false, commit: false, kind: 'rest', nextStart: resynced };
  }
  if (input.atOlderEdge) {
    const next = shiftProjectionWindowOlder(input.total, resynced);
    if (next !== resynced) return { hold: false, commit: true, kind: 'shift', nextStart: next };
    return { hold: false, commit: false, kind: 'rest', nextStart: resynced };
  }
  if (input.atNewerEdge) {
    const next = shiftProjectionWindowNewer(input.total, resynced);
    if (next !== resynced) return { hold: false, commit: true, kind: 'shift', nextStart: next };
    return { hold: false, commit: false, kind: 'rest', nextStart: resynced };
  }
  return { hold: false, commit: false, kind: 'rest', nextStart: resynced };
}

export function applyOpenFoldKeys(
  content: TranscriptContentRoot | null,
  openFoldKeys: ReadonlySet<string>,
): number {
  if (!content) return 0;
  let opened = 0;
  for (const details of Array.from(content.stampedFolds())) {
    const key = details.getAttribute(TRANSCRIPT_FOLD_ATTRIBUTE);
    const open = key !== null && openFoldKeys.has(key);
    if (open && !details.open) opened += 1;
    details.open = open;
  }
  return opened;
}
export function applySurfaceVisibility(surface: HTMLElement, active: boolean): void {
  surface.hidden = !active;
  if (active) surface.removeAttribute('inert');
  else surface.setAttribute('inert', '');
  surface.setAttribute('aria-hidden', String(!active));
}
export function compensatePrependedStage(options: {
  scroll: { scrollTop: number } | null;
  content: TranscriptContentRoot | null;
  prepended: number;
  openFoldKeys: ReadonlySet<string>;
}): { openedFolds: number; deltaPx: number } {
  const { scroll, content, prepended, openFoldKeys } = options;
  if (!scroll || !content || prepended <= 0) return { openedFolds: 0, deltaPx: 0 };
  const openedFolds = applyOpenFoldKeys(content, openFoldKeys);
  const children = content.children;
  const kept = children.length > prepended ? children[prepended] : null;
  if (!kept || children.length === 0) return { openedFolds, deltaPx: 0 };
  const deltaPx = kept.getBoundingClientRect().top - children[0].getBoundingClientRect().top;
  return { openedFolds, deltaPx };
}