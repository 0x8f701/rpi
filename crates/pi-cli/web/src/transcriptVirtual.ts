// Pure variable-height transcript virtualizer. No React, no DOM.
// TranscriptViewport is the only production consumer; App owns renderItem
// and exact-runtime height maps.

import type { Item } from './transcript';
import { isPinned, PIN_TOLERANCE_PX } from './scrollPin';

export const VIRTUAL_OVERSCAN = 8;
export const VIRTUAL_MAX_MOUNTED_ROWS = 80;
export const HEIGHT_EMA_ALPHA = 0.25;
/** CSS `--turn-inner-gap` on `.transcript-content` (styles.css). Geometry
 *  must include this gap; a silent 0 default desyncs spacers from DOM. */
export const TRANSCRIPT_ROW_GAP_PX = 8;

export const DEFAULT_ROW_HEIGHT = {
  user: 72,
  assistant: 120,
  assistantStreaming: 80,
  toolCard: 96,
  toolResult: 88,
  bash: 88,
  custom: 64,
  irc: 88,
  summary: 64,
  'handoff-divider': 96,
  approval: 96,
} as const;

export type RowHeightKind = keyof typeof DEFAULT_ROW_HEIGHT;
export type RowHeightEstimates = Record<RowHeightKind, number>;

export interface RuntimeIdentity {
  sessionId: string;
  runtimeIncarnation: string;
}

export interface TranscriptScrollSlot {
  follow: boolean;
  anchorItemId: string | null;
  anchorOffsetPx: number;
  fallbackScrollTop: number;
}

export interface VirtualViewport {
  scrollTop: number;
  clientHeight: number;
}

export interface ScrollGeometry {
  scrollTop: number;
  clientHeight: number;
  scrollHeight: number;
  heights: ReadonlyMap<string, number>;
  estimates: RowHeightEstimates;
  rowGap: number;
}

export interface VirtualRange {
  start: number;
  end: number;
  prefixHeight: number;
  suffixHeight: number;
  mountedCount: number;
  totalHeight: number;
  visibleStart: number;
  visibleEnd: number;
}

export interface VirtualRangeInput {
  items: readonly Item[];
  heights: ReadonlyMap<string, number>;
  estimates: RowHeightEstimates;
  viewport: VirtualViewport;
  follow?: boolean;
  overscan?: number;
  maxMounted?: number;
  rowGap: number;
}

export interface VirtualState {
  items: readonly Item[];
  heights: Map<string, number>;
  estimates: RowHeightEstimates;
  anchorIndex: number;
  follow: boolean;
}

export interface VirtualMeasurementPatch {
  heights: Map<string, number>;
  kindEstimates: RowHeightEstimates;
  scrollTopDelta: number;
}

export interface AnchorRestorePlan {
  scrollTop: number;
  anchorIndex: number;
  follow: boolean;
  totalHeight: number;
}

export function defaultRowHeightEstimates(): RowHeightEstimates {
  return { ...DEFAULT_ROW_HEIGHT };
}

export function defaultTranscriptScrollSlot(): TranscriptScrollSlot {
  return { follow: true, anchorItemId: null, anchorOffsetPx: 0, fallbackScrollTop: 0 };
}

/** Length-prefixed exact-runtime key. Not a delimiter-guessable concat. */
export function encodeRuntimeKey(identity: RuntimeIdentity): string {
  const sessionId = identity.sessionId;
  const runtimeIncarnation = identity.runtimeIncarnation;
  return `${sessionId.length}:${sessionId}:${runtimeIncarnation.length}:${runtimeIncarnation}`;
}

export function transcriptRowKey(identity: RuntimeIdentity, itemId: string): string {
  return `${encodeRuntimeKey(identity)}:${itemId}`;
}

export function rowHeightKind(item: Item): RowHeightKind {
  if (item.kind === 'assistant') {
    return item.status === 'streaming' ? 'assistantStreaming' : 'assistant';
  }
  return item.kind;
}

export function estimateRowHeight(item: Item, estimates: RowHeightEstimates): number {
  const kind = rowHeightKind(item);
  const estimated = estimates[kind];
  if (typeof estimated === 'number' && Number.isFinite(estimated) && estimated > 0) {
    return estimated;
  }
  return DEFAULT_ROW_HEIGHT[kind];
}

export function itemHeight(
  item: Item,
  heights: ReadonlyMap<string, number>,
  estimates: RowHeightEstimates,
): number {
  const measured = heights.get(item.id);
  if (typeof measured === 'number' && Number.isFinite(measured) && measured > 0) {
    return measured;
  }
  return estimateRowHeight(item, estimates);
}

function spacerHeight(sum: number, count: number, rowGap: number): number {
  if (count <= 0) return 0;
  return sum + rowGap * Math.max(0, count - 1);
}

function buildTops(
  items: readonly Item[],
  heights: ReadonlyMap<string, number>,
  estimates: RowHeightEstimates,
  rowGap: number,
): { tops: number[]; totalHeight: number } {
  const n = items.length;
  const tops = new Array<number>(n);
  let acc = 0;
  for (let i = 0; i < n; i++) {
    tops[i] = acc;
    acc += itemHeight(items[i], heights, estimates);
    if (i < n - 1) acc += rowGap;
  }
  return { tops, totalHeight: acc };
}

function lastIndexAtOrBefore(tops: number[], offset: number): number {
  const n = tops.length;
  if (n === 0) return 0;
  let lo = 0;
  let hi = n - 1;
  let ans = 0;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (tops[mid] <= offset) {
      ans = mid;
      lo = mid + 1;
    } else {
      hi = mid - 1;
    }
  }
  return ans;
}

function firstIndexAtOrAfter(tops: number[], offset: number): number {
  const n = tops.length;
  let lo = 0;
  let hi = n;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (tops[mid] >= offset) hi = mid;
    else lo = mid + 1;
  }
  return lo;
}

export function clampWindow(
  n: number,
  visibleStart: number,
  visibleEnd: number,
  overscan: number,
  maxMounted: number,
  follow: boolean,
): { start: number; end: number } {
  if (n <= 0) return { start: 0, end: 0 };
  let start = Math.max(0, visibleStart - overscan);
  let end = Math.min(n, visibleEnd + overscan);
  const visibleCount = Math.max(0, visibleEnd - visibleStart);

  if (end - start > maxMounted) {
    const overscanAbove = visibleStart - start;
    const overscanBelow = end - visibleEnd;
    const extra = end - start - maxMounted;
    let shrinkAbove = Math.min(overscanAbove, Math.ceil(extra / 2));
    let shrinkBelow = extra - shrinkAbove;
    if (shrinkBelow > overscanBelow) {
      shrinkAbove += shrinkBelow - overscanBelow;
      shrinkBelow = overscanBelow;
    }
    shrinkAbove = Math.min(overscanAbove, Math.max(0, shrinkAbove));
    shrinkBelow = Math.min(overscanBelow, Math.max(0, shrinkBelow));
    start += shrinkAbove;
    end -= shrinkBelow;
  }

  if (end - start > maxMounted) {
    if (follow) {
      end = n;
      start = Math.max(0, end - maxMounted);
    } else {
      start = Math.max(0, Math.min(visibleStart, n));
      end = Math.min(n, start + maxMounted);
      if (end - start < maxMounted) start = Math.max(0, end - maxMounted);
      if (end - start > maxMounted) end = start + maxMounted;
    }
  }

  if (end <= start) {
    if (follow) {
      end = n;
      start = Math.max(0, n - 1);
    } else {
      start = Math.min(Math.max(0, visibleStart), n - 1);
      end = start + 1;
    }
  }

  if (visibleCount > 0 && end <= start) {
    start = Math.min(Math.max(0, visibleStart), n - 1);
    end = Math.min(n, start + 1);
  }

  return { start, end };
}

export function computeVirtualRange(input: VirtualRangeInput): VirtualRange {
  const items = input.items;
  const n = items.length;
  const overscan = input.overscan ?? VIRTUAL_OVERSCAN;
  const maxMounted = input.maxMounted ?? VIRTUAL_MAX_MOUNTED_ROWS;
  const rowGap = input.rowGap;
  const follow = input.follow === true;
  const clientHeight = input.viewport.clientHeight;
  const scrollTop = Math.max(0, input.viewport.scrollTop);

  if (n === 0) {
    return {
      start: 0,
      end: 0,
      prefixHeight: 0,
      suffixHeight: 0,
      mountedCount: 0,
      totalHeight: 0,
      visibleStart: 0,
      visibleEnd: 0,
    };
  }

  const { tops, totalHeight } = buildTops(items, input.heights, input.estimates, rowGap);
  const viewTop = follow ? Math.max(0, totalHeight - Math.max(0, clientHeight)) : scrollTop;
  const viewBottom = viewTop + Math.max(0, clientHeight);

  let visibleStart = lastIndexAtOrBefore(tops, viewTop);
  let visibleEnd = firstIndexAtOrAfter(tops, viewBottom);
  if (visibleEnd <= visibleStart) visibleEnd = Math.min(n, visibleStart + 1);
  if (clientHeight > 0 && visibleEnd <= visibleStart) {
    visibleStart = Math.min(visibleStart, n - 1);
    visibleEnd = Math.min(n, visibleStart + 1);
  }
  if (follow) visibleEnd = n;

  const { start, end } = clampWindow(n, visibleStart, visibleEnd, overscan, maxMounted, follow);
  let prefix = 0;
  for (let i = 0; i < start; i++) prefix += itemHeight(items[i], input.heights, input.estimates);
  let suffix = 0;
  for (let i = end; i < n; i++) suffix += itemHeight(items[i], input.heights, input.estimates);

  return {
    start,
    end,
    prefixHeight: spacerHeight(prefix, start, rowGap),
    suffixHeight: spacerHeight(suffix, n - end, rowGap),
    mountedCount: end - start,
    totalHeight,
    visibleStart,
    visibleEnd,
  };
}

export function applyMeasuredHeight(state: VirtualState, itemId: string, height: number): VirtualMeasurementPatch {
  const nextHeights = new Map(state.heights);
  const nextEstimates = { ...state.estimates };
  if (!Number.isFinite(height) || height <= 0) {
    return { heights: nextHeights, kindEstimates: nextEstimates, scrollTopDelta: 0 };
  }

  let measuredIndex = -1;
  for (let i = 0; i < state.items.length; i++) {
    if (state.items[i].id === itemId) {
      measuredIndex = i;
      break;
    }
  }

  const item = measuredIndex >= 0 ? state.items[measuredIndex] : null;
  const oldHeight = item
    ? itemHeight(item, state.heights, state.estimates)
    : (state.heights.get(itemId) ?? 0);
  nextHeights.set(itemId, height);

  if (item) {
    const kind = rowHeightKind(item);
    const previous = nextEstimates[kind];
    nextEstimates[kind] = previous * (1 - HEIGHT_EMA_ALPHA) + height * HEIGHT_EMA_ALPHA;
  }

  const delta = height - oldHeight;
  if (delta === 0 || state.follow || measuredIndex < 0) {
    return { heights: nextHeights, kindEstimates: nextEstimates, scrollTopDelta: 0 };
  }
  const scrollTopDelta = measuredIndex < state.anchorIndex ? delta : 0;
  return { heights: nextHeights, kindEstimates: nextEstimates, scrollTopDelta };
}

export function captureTranscriptAnchor(
  items: readonly Item[],
  container: ScrollGeometry,
): TranscriptScrollSlot {
  const rowGap = container.rowGap;
  const fallbackScrollTop = Math.max(0, container.scrollTop);
  if (items.length === 0) {
    return { follow: true, anchorItemId: null, anchorOffsetPx: 0, fallbackScrollTop };
  }

  const follow = isPinned(container.scrollTop, container.scrollHeight, container.clientHeight, PIN_TOLERANCE_PX);
  if (follow) {
    return {
      follow: true,
      anchorItemId: items[items.length - 1]?.id ?? null,
      anchorOffsetPx: 0,
      fallbackScrollTop,
    };
  }

  const { tops } = buildTops(items, container.heights, container.estimates, rowGap);
  const index = lastIndexAtOrBefore(tops, container.scrollTop);
  const item = items[index];
  const top = tops[index] ?? 0;
  return {
    follow: false,
    anchorItemId: item?.id ?? null,
    anchorOffsetPx: container.scrollTop - top,
    fallbackScrollTop,
  };
}
export function restoreTranscriptAnchor(
  items: readonly Item[],
  metrics: ReadonlyMap<string, number>,
  slot: TranscriptScrollSlot,
  viewportHeight: number,
  estimates: RowHeightEstimates = defaultRowHeightEstimates(),
  rowGap = TRANSCRIPT_ROW_GAP_PX,
): AnchorRestorePlan {
  const { tops, totalHeight } = buildTops(items, metrics, estimates, rowGap);
  const maxScroll = Math.max(0, totalHeight - Math.max(0, viewportHeight));
  if (items.length === 0) {
    return { scrollTop: 0, anchorIndex: 0, follow: true, totalHeight: 0 };
  }

  if (slot.follow) {
    return {
      scrollTop: maxScroll,
      anchorIndex: items.length - 1,
      follow: true,
      totalHeight,
    };
  }

  let anchorIndex = -1;
  if (slot.anchorItemId) {
    for (let i = 0; i < items.length; i++) {
      if (items[i].id === slot.anchorItemId) {
        anchorIndex = i;
        break;
      }
    }
  }

  if (anchorIndex < 0) {
    return {
      scrollTop: Math.min(maxScroll, Math.max(0, slot.fallbackScrollTop)),
      anchorIndex: lastIndexAtOrBefore(tops, slot.fallbackScrollTop),
      follow: false,
      totalHeight,
    };
  }

  const scrollTop = Math.min(maxScroll, Math.max(0, tops[anchorIndex] + slot.anchorOffsetPx));
  return { scrollTop, anchorIndex, follow: false, totalHeight };
}

export function collectMountedItemIds(items: readonly Item[], range: VirtualRange): string[] {
  const ids: string[] = [];
  for (let i = range.start; i < range.end; i++) ids.push(items[i].id);
  return ids;
}

export function shouldHydrateMarkdown(itemId: string, mountedIds: ReadonlySet<string>): boolean {
  return mountedIds.has(itemId);
}

export function shouldRenderTranscriptRow(itemId: string, mountedIds: ReadonlySet<string>): boolean {
  return mountedIds.has(itemId);
}

export function thinkingFoldKey(itemId: string): string {
  return `${itemId}:thinking`;
}

export function toolRawFoldKey(itemId: string, toolCallIdOrIndex: string | number): string {
  return `${itemId}:tool-raw:${toolCallIdOrIndex}`;
}

export function userLargeTextFoldKey(itemId: string): string {
  return `${itemId}:user-large-text`;
}

export function userAnalysisFoldKey(itemId: string): string {
  return `${itemId}:user-analysis`;
}

export function ircFoldKey(itemId: string): string {
  return `${itemId}:irc`;
}

export function hubFoldKey(itemId: string): string {
  return `${itemId}:hub`;
}

export function foldKeysForItem(item: Item): string[] {
  switch (item.kind) {
    case 'assistant':
      return [thinkingFoldKey(item.id)];
    case 'toolCard':
      return [toolRawFoldKey(item.id, item.toolCallId), hubFoldKey(item.id)];
    case 'user':
      return [userLargeTextFoldKey(item.id), userAnalysisFoldKey(item.id)];
    case 'irc':
      return [ircFoldKey(item.id)];
    default:
      return [];
  }
}

export function itemFoldSignature(item: Item, openFoldKeys: ReadonlySet<string>): string {
  const keys = foldKeysForItem(item);
  if (keys.length === 0) return '';
  let signature = '';
  for (const key of keys) {
    if (openFoldKeys.has(key)) signature += `${key},`;
  }
  return signature;
}

export function deleteRuntimeRowHeights(
  heightsByRuntimeKey: Map<string, Map<string, number>>,
  runtimeKey: string,
): boolean {
  return heightsByRuntimeKey.delete(runtimeKey);
}

export function anchorIndexForSlot(items: readonly Item[], slot: TranscriptScrollSlot): number {
  if (items.length === 0) return 0;
  if (slot.follow) return items.length - 1;
  if (!slot.anchorItemId) return 0;
  for (let i = 0; i < items.length; i++) {
    if (items[i].id === slot.anchorItemId) return i;
  }
  return 0;
}

export const virtualRange = computeVirtualRange;
export function estimateScrollTopForAnchor(
  items: readonly Item[],
  heights: ReadonlyMap<string, number>,
  estimates: RowHeightEstimates,
  anchor: TranscriptScrollSlot,
  viewportHeight = 0,
  rowGap = TRANSCRIPT_ROW_GAP_PX,
): number {
  return restoreTranscriptAnchor(items, heights, anchor, viewportHeight, estimates, rowGap).scrollTop;
}
