#!/usr/bin/env node
// Focused unit regression for src/transcriptVirtual.ts — the pure
// variable-height range / measurement / semantic-anchor core consumed by
// TranscriptViewport. Bundled by `npm run build` with Vite's esbuild into a
// disposable Node module. Fake geometry only: no DOM, no React.
import type { Item } from '../src/transcript.ts';
import {
  VIRTUAL_OVERSCAN,
  VIRTUAL_MAX_MOUNTED_ROWS,
  DEFAULT_ROW_HEIGHT,
  TRANSCRIPT_ROW_GAP_PX,
  applyMeasuredHeight,
  captureTranscriptAnchor,
  clampWindow,
  collectMountedItemIds,
  computeVirtualRange,
  defaultRowHeightEstimates,
  deleteRuntimeRowHeights,
  encodeRuntimeKey,
  estimateRowHeight,
  estimateScrollTopForAnchor,
  foldKeysForItem,
  hubFoldKey,
  ircFoldKey,
  restoreTranscriptAnchor,
  shouldHydrateMarkdown,
  shouldRenderTranscriptRow,
  thinkingFoldKey,
  toolRawFoldKey,
  userAnalysisFoldKey,
  userLargeTextFoldKey,
  virtualRange,
  type RowHeightEstimates,
  type TranscriptScrollSlot,
  type VirtualRange,
  type VirtualState,
} from '../src/transcriptVirtual.ts';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: boolean, detail?: string) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

function user(id: string, extra?: Partial<Extract<Item, { kind: 'user' }>>): Item {
  return { kind: 'user', id, text: extra?.text ?? id, optimistic: extra?.optimistic ?? false, images: extra?.images, analysis: extra?.analysis };
}

function assistant(id: string, status: 'streaming' | 'final' = 'final'): Item {
  return { kind: 'assistant', id, status, blocks: [] };
}

function mixedItems(n: number): Item[] {
  const items: Item[] = [];
  for (let i = 0; i < n; i++) {
    const id = `item-${i}`;
    switch (i % 10) {
      case 0:
        items.push({ kind: 'user', id, text: id, optimistic: false });
        break;
      case 1:
        items.push({ kind: 'assistant', id, status: 'final', blocks: [] });
        break;
      case 2:
        items.push({ kind: 'assistant', id, status: 'streaming', blocks: [] });
        break;
      case 3:
        items.push({ kind: 'toolCard', id, toolCallId: 't', toolName: 'bash', args: {}, status: 'done', result: '' });
        break;
      case 4:
        items.push({ kind: 'toolResult', id, toolCallId: 't', text: 'out' });
        break;
      case 5:
        items.push({ kind: 'bash', id, command: 'ls', output: 'a' });
        break;
      case 6:
        items.push({ kind: 'custom', id, label: 'note', text: 'hi' });
        break;
      case 7:
        items.push({ kind: 'irc', id, direction: 'incoming', from: 'Child', to: 'Main', body: 'ping' });
        break;
      case 8:
        items.push({ kind: 'summary', id, label: 'summary', text: 's' });
        break;
      default:
        items.push({ kind: 'approval', id, method: 'ask', title: 'ok', message: 'go' });
        break;
    }
  }
  return items;
}

function rangeOf(
  items: readonly Item[],
  opts: {
    scrollTop?: number;
    clientHeight?: number;
    follow?: boolean;
    heights?: Map<string, number>;
    estimates?: RowHeightEstimates;
    rowGap?: number;
  } = {},
): VirtualRange {
  return computeVirtualRange({
    items,
    heights: opts.heights ?? new Map(),
    estimates: opts.estimates ?? defaultRowHeightEstimates(),
    viewport: { scrollTop: opts.scrollTop ?? 0, clientHeight: opts.clientHeight ?? 600 },
    follow: opts.follow,
    rowGap: opts.rowGap ?? TRANSCRIPT_ROW_GAP_PX,
  });
}

check('VIRTUAL_OVERSCAN == 8', VIRTUAL_OVERSCAN === 8);
check('VIRTUAL_MAX_MOUNTED_ROWS == 80', VIRTUAL_MAX_MOUNTED_ROWS === 80);
check('TRANSCRIPT_ROW_GAP_PX == 8', TRANSCRIPT_ROW_GAP_PX === 8);
check('default user estimate is 72', DEFAULT_ROW_HEIGHT.user === 72);
check('default assistant estimate is 120', DEFAULT_ROW_HEIGHT.assistant === 120);
check('default streaming estimate is 80', DEFAULT_ROW_HEIGHT.assistantStreaming === 80);

{
  const estimates = defaultRowHeightEstimates();
  check('estimateRowHeight uses kind map', estimateRowHeight(user('u1'), estimates) === 72);
  check('estimateRowHeight streaming assistant', estimateRowHeight(assistant('a1', 'streaming'), estimates) === 80);
  check('estimateRowHeight final assistant', estimateRowHeight(assistant('a1', 'final'), estimates) === 120);
}

{
  const empty = rangeOf([]);
  check('empty mountedCount is 0', empty.mountedCount === 0);
  check('empty start/end are 0', empty.start === 0 && empty.end === 0);
  check('empty spacers are 0', empty.prefixHeight === 0 && empty.suffixHeight === 0);
}

{
  const items = mixedItems(3);
  const r = rangeOf(items, { clientHeight: 600 });
  check('short list mounts every row', r.mountedCount === 3 && r.start === 0 && r.end === 3);
  check('short list spacers are 0', r.prefixHeight === 0 && r.suffixHeight === 0);
  check('visible range is never empty for non-empty items', r.visibleEnd > r.visibleStart);
}

{
  const items = mixedItems(20_000);
  const top = rangeOf(items, { scrollTop: 0, clientHeight: 600, follow: false });
  check('20k top mountedCount <= 80', top.mountedCount <= VIRTUAL_MAX_MOUNTED_ROWS, String(top.mountedCount));
  check('20k top mountedCount == visible+16 or less', top.mountedCount <= Math.min(top.visibleEnd - top.visibleStart + 16, 80));
  check('20k top starts at 0', top.start === 0);
  check('20k top mounts overscan below', top.end > top.visibleEnd || top.end === items.length);

  const follow = rangeOf(items, { clientHeight: 600, follow: true });
  check('20k follow mountedCount <= 80', follow.mountedCount <= VIRTUAL_MAX_MOUNTED_ROWS, String(follow.mountedCount));
  check('20k follow includes the last item', follow.end === items.length);
  check('20k follow prefix is positive', follow.prefixHeight > 0);
  check('20k follow suffix is 0', follow.suffixHeight === 0);
  const lastId = items[items.length - 1].id;
  check('20k follow last id is mounted', collectMountedItemIds(items, follow).includes(lastId));

  const mid = rangeOf(items, { scrollTop: 400_000, clientHeight: 600, follow: false });
  check('20k mid mountedCount <= 80', mid.mountedCount <= VIRTUAL_MAX_MOUNTED_ROWS, String(mid.mountedCount));
  check('20k mid has prefix and suffix', mid.prefixHeight > 0 && mid.suffixHeight > 0);
  check('20k mid visible never empty', mid.visibleEnd > mid.visibleStart);
}

{
  const items = mixedItems(20_000);
  const tiny = defaultRowHeightEstimates();
  for (const key of Object.keys(tiny) as Array<keyof RowHeightEstimates>) tiny[key] = 1;
  const hugeVisible = rangeOf(items, { scrollTop: 0, clientHeight: 4000, follow: false, estimates: tiny });
  check('tiny-row window still caps at 80', hugeVisible.mountedCount <= 80, String(hugeVisible.mountedCount));
  check('cap never empties a non-empty list', hugeVisible.mountedCount >= 1);
  const followCap = rangeOf(items, { clientHeight: 4000, follow: true, estimates: tiny });
  check('follow tiny-row window still caps at 80', followCap.mountedCount <= 80);
  check('follow cap still includes last item', followCap.end === items.length);
}

{
  const items = mixedItems(200);
  const estimates = defaultRowHeightEstimates();
  const heights = new Map<string, number>();
  const before = rangeOf(items, { follow: true, clientHeight: 600, heights, estimates });
  const appended = [...items, user('item-new')];
  const after = rangeOf(appended, { follow: true, clientHeight: 600, heights, estimates });
  check('follow append keeps last item in range', after.end === appended.length);
  check('follow append prefix grows', after.prefixHeight >= before.prefixHeight);
  const total = after.totalHeight;
  const bottom = Math.max(0, total - 600);
  check('follow bottom estimate is totalHeight - clientHeight', after.totalHeight - 600 === bottom);
}

{
  const items = mixedItems(40);
  const estimates = defaultRowHeightEstimates();
  const heights = new Map<string, number>();
  for (const item of items) heights.set(item.id, estimateRowHeight(item, estimates));
  const state = (anchorIndex: number): VirtualState => ({
    items,
    heights: new Map(heights),
    estimates: { ...estimates },
    anchorIndex,
    follow: false,
  });
  const above = applyMeasuredHeight(state(10), items[2].id, heights.get(items[2].id)! + 40);
  check('height change above anchor produces exact scrollTopDelta', above.scrollTopDelta === 40, String(above.scrollTopDelta));
  const inside = applyMeasuredHeight(state(10), items[10].id, heights.get(items[10].id)! + 25);
  check('height change at the anchor does not move scrollTop', inside.scrollTopDelta === 0);
  const below = applyMeasuredHeight(state(10), items[30].id, heights.get(items[30].id)! + 80);
  check('height change below anchor produces 0', below.scrollTopDelta === 0);
  const followState: VirtualState = { ...state(39), follow: true };
  const followPatch = applyMeasuredHeight(followState, items[2].id, 400);
  check('follow ignores above-anchor delta (pin owns the bottom)', followPatch.scrollTopDelta === 0);
}

{
  const items = mixedItems(20);
  const estimates = defaultRowHeightEstimates();
  const heights = new Map<string, number>([[items[1].id, 80]]);
  const patch = applyMeasuredHeight(
    { items, heights, estimates, anchorIndex: 8, follow: false },
    items[1].id,
    400,
  );
  check('async mermaid-sized jump 80→400 above anchor is +320', patch.scrollTopDelta === 320, String(patch.scrollTopDelta));
  check('measured height is stored', patch.heights.get(items[1].id) === 400);
}

{
  const items = mixedItems(500);
  const estimates = defaultRowHeightEstimates();
  const unknown = rangeOf(items, { scrollTop: 12_000, clientHeight: 640, follow: false });
  const heights = new Map<string, number>();
  for (const item of items) heights.set(item.id, estimateRowHeight(item, estimates));
  const known = rangeOf(items, { scrollTop: 12_000, clientHeight: 640, follow: false, heights, estimates });
  check(
    'unknown estimates converge within one row after fill',
    Math.abs(known.start - unknown.start) <= 1 && Math.abs(known.end - unknown.end) <= 1,
    `unknown=${unknown.start}:${unknown.end} known=${known.start}:${known.end}`,
  );
  check('convergence does not render offscreen ids', known.mountedCount <= 80);
}

{
  const items = mixedItems(80);
  const estimates = defaultRowHeightEstimates();
  const heights = new Map<string, number>();
  for (const item of items) heights.set(item.id, 100);
  const captured = captureTranscriptAnchor(items, {
    scrollTop: 1350,
    clientHeight: 400,
    scrollHeight: 80 * 100 + 79 * TRANSCRIPT_ROW_GAP_PX,
    heights,
    estimates,
    rowGap: TRANSCRIPT_ROW_GAP_PX,
  });
  check('manual capture is unpinned', captured.follow === false);
  check('manual capture stores an item id', typeof captured.anchorItemId === 'string' && captured.anchorItemId !== null);
  const restoredA = restoreTranscriptAnchor(items, heights, captured, 400, estimates, TRANSCRIPT_ROW_GAP_PX);
  check('A restore keeps the same item', items[restoredA.anchorIndex].id === captured.anchorItemId);
  check('A restore keeps the pixel offset', restoredA.scrollTop === 1350, String(restoredA.scrollTop));

  const itemsB = mixedItems(30).map((item, i) => ({ ...item, id: `b-${i}` }));
  const heightsB = new Map<string, number>();
  for (const item of itemsB) heightsB.set(item.id, 60);
  const slotB: TranscriptScrollSlot = { follow: true, anchorItemId: itemsB[itemsB.length - 1].id, anchorOffsetPx: 0, fallbackScrollTop: 0 };
  const restoredB = restoreTranscriptAnchor(itemsB, heightsB, slotB, 400, estimates, TRANSCRIPT_ROW_GAP_PX);
  check('B follow restore pins to bottom', restoredB.follow === true);
  check('B follow restore is last index', restoredB.anchorIndex === itemsB.length - 1);

  const restoredA2 = restoreTranscriptAnchor(items, heights, captured, 400, estimates, TRANSCRIPT_ROW_GAP_PX);
  check('A→B→A restores the same item', items[restoredA2.anchorIndex].id === captured.anchorItemId);
  check('A→B→A restores the same pixel offset', restoredA2.scrollTop === 1350);
  check(
    'estimateScrollTopForAnchor matches restore',
    estimateScrollTopForAnchor(items, heights, estimates, captured, 400, TRANSCRIPT_ROW_GAP_PX) === restoredA2.scrollTop,
  );
}

{
  const items = mixedItems(400);
  const estimates = defaultRowHeightEstimates();
  const heights = new Map<string, number>(items.map((item) => [item.id, 80]));
  const stride = 80 + TRANSCRIPT_ROW_GAP_PX;
  const farScrollTop = 250 * stride + 17;
  const mounted = rangeOf(items, { scrollTop: 0, clientHeight: 400, follow: false, heights, estimates });
  check('mounted window does not include far PageDown index', mounted.end <= 80);
  const capturedFar = captureTranscriptAnchor(items, {
    scrollTop: farScrollTop,
    clientHeight: 400,
    scrollHeight: 400 * 80 + 399 * TRANSCRIPT_ROW_GAP_PX,
    heights,
    estimates,
    rowGap: TRANSCRIPT_ROW_GAP_PX,
  });
  check('PageDown beyond overscan captures the far item, not the mounted window', capturedFar.anchorItemId === items[250].id, capturedFar.anchorItemId ?? 'null');
  check('PageDown beyond overscan keeps the in-item offset', capturedFar.anchorOffsetPx === 17, String(capturedFar.anchorOffsetPx));
  const restoredFar = restoreTranscriptAnchor(items, heights, capturedFar, 400, estimates, TRANSCRIPT_ROW_GAP_PX);
  check('PageDown restore returns the same scrollTop', restoredFar.scrollTop === farScrollTop);
}

{
  const items = mixedItems(20);
  const estimates = defaultRowHeightEstimates();
  const heights = new Map<string, number>(items.map((item) => [item.id, 80]));
  const before = restoreTranscriptAnchor(
    items,
    heights,
    { follow: false, anchorItemId: items[12].id, anchorOffsetPx: 10, fallbackScrollTop: 0 },
    400,
    estimates,
  );
  const folded = applyMeasuredHeight(
    { items, heights, estimates, anchorIndex: before.anchorIndex, follow: false },
    items[5].id,
    200,
  );
  check('fold toggle above the anchor corrects scrollTop', folded.scrollTopDelta === 120, String(folded.scrollTopDelta));
  const foldedBelow = applyMeasuredHeight(
    { items, heights: folded.heights, estimates: folded.kindEstimates, anchorIndex: before.anchorIndex, follow: false },
    items[18].id,
    300,
  );
  check('fold toggle below the anchor keeps the reading position', foldedBelow.scrollTopDelta === 0);
}

{
  const heightsByRuntime = new Map<string, Map<string, number>>();
  const a = encodeRuntimeKey({ sessionId: 'sess-a', runtimeIncarnation: 'inc-1' });
  const a2 = encodeRuntimeKey({ sessionId: 'sess-a', runtimeIncarnation: 'inc-2' });
  heightsByRuntime.set(a, new Map([['item-1', 80]]));
  heightsByRuntime.set(a2, new Map([['item-1', 90]]));
  check('unload deletes only the exact runtime height map', deleteRuntimeRowHeights(heightsByRuntime, a) === true);
  check('inc2 height map survives inc1 unload', heightsByRuntime.has(a2) && heightsByRuntime.get(a2)!.get('item-1') === 90);
  check('inc1 height map is gone', !heightsByRuntime.has(a));
}

{
  const items = mixedItems(100);
  const r = rangeOf(items, { scrollTop: 2000, clientHeight: 500 });
  const mounted = new Set(collectMountedItemIds(items, r));
  check('collectMountedItemIds length equals mountedCount', mounted.size === r.mountedCount);
  check('collectMountedItemIds length <= 80', mounted.size <= 80);
  const offscreen = items.find((item) => !mounted.has(item.id));
  check('offscreen id exists in a 100-row list', !!offscreen);
  check('shouldHydrateMarkdown is false for offscreen ids', !shouldHydrateMarkdown(offscreen!.id, mounted));
  check('shouldRenderTranscriptRow is false for offscreen ids', !shouldRenderTranscriptRow(offscreen!.id, mounted));
  const onscreen = items[r.start];
  check('shouldHydrateMarkdown is true for mounted ids', shouldHydrateMarkdown(onscreen.id, mounted));
}

{
  const tool: Item = { kind: 'toolCard', id: 'tool-1', toolCallId: 'call-9', toolName: 'hub', args: {}, status: 'running', result: '' };
  const keys = foldKeysForItem(tool);
  check('thinking fold key', thinkingFoldKey('a1') === 'a1:thinking');
  check('tool-raw fold key uses toolCallId', toolRawFoldKey('tool-1', 'call-9') === 'tool-1:tool-raw:call-9');
  check('user large-text fold key', userLargeTextFoldKey('u1') === 'u1:user-large-text');
  check('user analysis fold key', userAnalysisFoldKey('u1') === 'u1:user-analysis');
  check('irc fold key', ircFoldKey('i1') === 'i1:irc');
  check('hub fold key', hubFoldKey('tool-1') === 'tool-1:hub');
  check('tool item exposes raw + hub folds', keys.includes('tool-1:tool-raw:call-9') && keys.includes('tool-1:hub'));
}

{
  const items = mixedItems(10);
  const r = rangeOf(items, { scrollTop: 0, clientHeight: 600 });
  check('prefix spacer matches virtualRange.prefixHeight', r.prefixHeight === 0);
  const follow = rangeOf(mixedItems(200), { follow: true, clientHeight: 200 });
  check('follow suffix spacer is 0', follow.suffixHeight === 0);
  check('follow prefix spacer equals unmounted prefix height', follow.prefixHeight > 0);
}

encodeRuntimeKey({ sessionId: 'a:b', runtimeIncarnation: 'c:d' });
check(
  'runtime key is length-prefixed, not a guessable concat',
  encodeRuntimeKey({ sessionId: 'a:b', runtimeIncarnation: 'c:d' }) === '3:a:b:3:c:d',
);

{
  const empty = clampWindow(0, 0, 0, 8, 80, false);
  check('clampWindow n=0 returns empty', empty.start === 0 && empty.end === 0);

  const short = clampWindow(3, 0, 3, 8, 80, false);
  check('clampWindow short list keeps all rows', short.start === 0 && short.end === 3);

  const top = clampWindow(200, 0, 10, 8, 80, false);
  check('clampWindow top overscan does not go below 0', top.start === 0);
  check('clampWindow top end is visible+overscan', top.end === 18);

  const mid = clampWindow(200, 40, 50, 8, 80, false);
  check('clampWindow mid keeps 8 overscan each side', mid.start === 32 && mid.end === 58);

  const tail = clampWindow(200, 190, 200, 8, 80, true);
  check('clampWindow follow tail includes last item', tail.end === 200);
  check('clampWindow follow tail overscans above', tail.start === 182);

  const shrink = clampWindow(400, 0, 200, 8, 80, false);
  check('clampWindow shrinks overscan when visible+16 > 80', shrink.end - shrink.start === 80, `${shrink.start}:${shrink.end}`);
  check('clampWindow shrink never empties a visible range', shrink.end > shrink.start);

  const shrinkFollow = clampWindow(400, 300, 400, 8, 80, true);
  check('clampWindow follow shrink keeps the last 80', shrinkFollow.end === 400 && shrinkFollow.start === 320);

  const leftoverBelow = clampWindow(100, 90, 100, 8, 5, false);
  check('clampWindow leftover-below shrink still <= maxMounted', leftoverBelow.end - leftoverBelow.start <= 5);
  check('clampWindow leftover-below keeps a non-empty window', leftoverBelow.end > leftoverBelow.start);

  const leftoverFollow = clampWindow(100, 0, 100, 8, 5, true);
  check('clampWindow leftover follow pins to the tail', leftoverFollow.end === 100 && leftoverFollow.start === 95);

  const inverted = clampWindow(50, 20, 10, 0, 80, false);
  check('clampWindow inverted visible range still mounts >=1', inverted.end > inverted.start);

  const invertedFollow = clampWindow(50, 20, 10, 0, 80, true);
  check('clampWindow inverted follow still includes last item', invertedFollow.end === 50 && invertedFollow.start === 49);

  const emptyVisible = clampWindow(10, 4, 4, 0, 80, false);
  check('clampWindow empty visible still mounts one row', emptyVisible.end === emptyVisible.start + 1);

  const emptyVisibleFollow = clampWindow(10, 4, 4, 0, 80, true);
  check('clampWindow empty visible follow mounts the last row', emptyVisibleFollow.start === 9 && emptyVisibleFollow.end === 10);
}

{
  const items = mixedItems(0);
  const alias = virtualRange({
    items,
    heights: new Map(),
    estimates: defaultRowHeightEstimates(),
    viewport: { scrollTop: 0, clientHeight: 600 },
    rowGap: TRANSCRIPT_ROW_GAP_PX,
  });
  check('virtualRange alias empty n=0', alias.mountedCount === 0 && alias.prefixHeight === 0 && alias.suffixHeight === 0);

  const giant = mixedItems(20_000);
  const followGiant = virtualRange({
    items: giant,
    heights: new Map(),
    estimates: defaultRowHeightEstimates(),
    viewport: { scrollTop: 0, clientHeight: 720 },
    follow: true,
    rowGap: TRANSCRIPT_ROW_GAP_PX,
  });
  check('20k hard cap is 80', followGiant.mountedCount <= 80);
  check('20k follow still includes the last row', followGiant.end === 20_000);
  const prefixIds = collectMountedItemIds(giant, followGiant);
  check('20k follow never hydrates offscreen item-0', !prefixIds.includes('item-0') && !shouldHydrateMarkdown('item-0', new Set(prefixIds)));
  check('20k follow hydrates the last mounted id', shouldHydrateMarkdown(giant[giant.length - 1].id, new Set(prefixIds)));
}

{
  const items = mixedItems(12);
  const estimates = defaultRowHeightEstimates();
  const heights = new Map<string, number>(items.map((item) => [item.id, 50]));
  const range = rangeOf(items, { scrollTop: 0, clientHeight: 600, heights, estimates });
  check('full-fit prefix/suffix spacers are 0', range.prefixHeight === 0 && range.suffixHeight === 0);
  const long = mixedItems(200);
  const longHeights = new Map<string, number>(long.map((item) => [item.id, 40]));
  const mid = rangeOf(long, { scrollTop: 2000, clientHeight: 400, follow: false, heights: longHeights, estimates });
  const expectedPrefix = mid.start * 40 + Math.max(0, mid.start - 1) * TRANSCRIPT_ROW_GAP_PX;
  const suffixCount = long.length - mid.end;
  const expectedSuffix = suffixCount * 40 + Math.max(0, suffixCount - 1) * TRANSCRIPT_ROW_GAP_PX;
  check('prefix spacer includes 8px CSS gap', mid.prefixHeight === expectedPrefix, `${mid.prefixHeight} vs ${expectedPrefix}`);
  check('suffix spacer includes 8px CSS gap', mid.suffixHeight === expectedSuffix, `${mid.suffixHeight} vs ${expectedSuffix}`);
  const zeroGap = rangeOf(long, { scrollTop: 2000, clientHeight: 400, follow: false, heights: longHeights, estimates, rowGap: 0 });
  check('8px CSS gap increases totalHeight versus explicit 0', mid.totalHeight > zeroGap.totalHeight, `${mid.totalHeight} vs ${zeroGap.totalHeight}`);
}

{
  const items = mixedItems(6);
  const estimates = defaultRowHeightEstimates();
  const heights = new Map<string, number>(items.map((item) => [item.id, 100]));
  const missing = applyMeasuredHeight(
    { items, heights, estimates, anchorIndex: 3, follow: false },
    'missing-id',
    180,
  );
  check('unknown itemId stores height but delta is 0', missing.heights.get('missing-id') === 180 && missing.scrollTopDelta === 0);
  const invalid = applyMeasuredHeight(
    { items, heights, estimates, anchorIndex: 3, follow: false },
    items[0].id,
    0,
  );
  check('non-positive measured height is ignored', invalid.scrollTopDelta === 0 && invalid.heights.get(items[0].id) === 100);

  const roundTrip = captureTranscriptAnchor(items, {
    scrollTop: 220,
    clientHeight: 150,
    scrollHeight: 600 + 5 * TRANSCRIPT_ROW_GAP_PX,
    heights,
    estimates,
    rowGap: TRANSCRIPT_ROW_GAP_PX,
  });
  const restored = restoreTranscriptAnchor(items, heights, roundTrip, 150, estimates, TRANSCRIPT_ROW_GAP_PX);
  check('capture/restore roundtrip keeps follow=false', restored.follow === false);
  check('capture/restore roundtrip restores the same item', items[restored.anchorIndex].id === roundTrip.anchorItemId);
  check('capture/restore roundtrip restores the same offset', restored.scrollTop === 220, String(restored.scrollTop));

  const missingAnchor = restoreTranscriptAnchor(
    items,
    heights,
    { follow: false, anchorItemId: 'gone', anchorOffsetPx: 12, fallbackScrollTop: 80 },
    150,
    estimates,
  );
  check('missing anchor falls back to fallbackScrollTop', missingAnchor.scrollTop === 80);
}


console.log(`\ntranscriptVirtual.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
