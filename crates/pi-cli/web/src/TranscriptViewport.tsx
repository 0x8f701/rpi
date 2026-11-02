import {
  memo,
  useCallback,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
  type UIEvent,
} from 'react';
import { createScrollPin, type ScrollPinController } from './scrollPin';
import type { Item } from './transcript';
import {
  applyMeasuredHeight,
  captureTranscriptAnchor,
  collectMountedItemIds,
  computeVirtualRange,
  defaultRowHeightEstimates,
  defaultTranscriptScrollSlot,
  encodeRuntimeKey,
  itemFoldSignature,
  restoreTranscriptAnchor,
  transcriptRowKey,
  TRANSCRIPT_ROW_GAP_PX,
  VIRTUAL_MAX_MOUNTED_ROWS,
  VIRTUAL_OVERSCAN,
  type RuntimeIdentity,
  type RowHeightEstimates,
  type TranscriptScrollSlot,
  type VirtualMeasurementPatch,
  type VirtualRange,
  type VirtualState,
} from './transcriptVirtual';

export interface TranscriptRenderContext {
  identity: RuntimeIdentity;
  rowKey: string;
  workspaceCwd?: string;
  openFoldKeys: ReadonlySet<string>;
  onToggleFold: (foldKey: string, open: boolean) => void;
  onLayoutChange: () => void;
  registerRow: (el: HTMLElement | null) => void;
}

export interface TranscriptViewportProps {
  identity: RuntimeIdentity | null;
  items: readonly Item[];
  workspaceCwd?: string;
  pending: boolean;
  rowHeights: ReadonlyMap<string, number>;
  estimates?: RowHeightEstimates;
  transcriptScroll?: TranscriptScrollSlot;
  openFoldKeys?: ReadonlySet<string>;
  emptyHint?: ReactNode;
  /** Authoritative CSS row-gap. Defaults to TRANSCRIPT_ROW_GAP_PX (8).
   *  Measured live from `.transcript-content` and never silently 0. */
  rowGap?: number;
  renderItem: (item: Item, ctx: TranscriptRenderContext) => ReactNode;
  onTranscriptScrollChange: (slot: TranscriptScrollSlot) => void;
  onRowHeightsChange: (patch: VirtualMeasurementPatch) => void;
  onToggleFold: (foldKey: string, open: boolean) => void;
  onLayoutInteractive: () => void;
}

export interface TranscriptRowProps {
  identity: RuntimeIdentity;
  item: Item;
  runtimeKey: string;
  foldSignature: string;
  workspaceCwd?: string;
  openFoldKeys: ReadonlySet<string>;
  renderItem: TranscriptViewportProps['renderItem'];
  onToggleFold: TranscriptViewportProps['onToggleFold'];
  onLayoutChange: () => void;
  registerRow: (el: HTMLElement | null) => void;
}

const EMPTY_FOLDS: ReadonlySet<string> = new Set();

export const transcriptViewportStats = {
  renderItemCalls: 0,
  mountedRows: 0,
  offscreenRenderItemCalls: 0,
  displayCommitMarks: 0,
  reset(): void {
    this.renderItemCalls = 0;
    this.mountedRows = 0;
    this.offscreenRenderItemCalls = 0;
    this.displayCommitMarks = 0;
  },
};

function instrumentEnabled(): boolean {
  if (typeof window === 'undefined') return false;
  const w = window as Window & { __RPI_SWITCH_INSTRUMENT__?: boolean };
  if (w.__RPI_SWITCH_INSTRUMENT__ === true) return true;
  try {
    return new URLSearchParams(w.location.search).get('switchPerf') === '1';
  } catch {
    return false;
  }
}

function switchStats(): Record<string, unknown> | null {
  if (typeof window === 'undefined') return null;
  const w = window as Window & { __RPI_SWITCH_STATS__?: Record<string, unknown> };
  if (!w.__RPI_SWITCH_STATS__) w.__RPI_SWITCH_STATS__ = {};
  return w.__RPI_SWITCH_STATS__;
}

function markSwitch(name: string): void {
  if (!instrumentEnabled()) return;
  try {
    performance.mark(name);
  } catch {
    /* User Timing unavailable */
  }
}

export function transcriptRowPropsEqual(prev: TranscriptRowProps, next: TranscriptRowProps): boolean {
  return prev.item === next.item
    && prev.runtimeKey === next.runtimeKey
    && prev.identity.sessionId === next.identity.sessionId
    && prev.identity.runtimeIncarnation === next.identity.runtimeIncarnation
    && prev.foldSignature === next.foldSignature
    && prev.workspaceCwd === next.workspaceCwd
    && prev.renderItem === next.renderItem
    && prev.onToggleFold === next.onToggleFold
    && prev.onLayoutChange === next.onLayoutChange
    && prev.registerRow === next.registerRow;
}

const TranscriptRow = memo(function TranscriptRow(props: TranscriptRowProps) {
  const ctx: TranscriptRenderContext = {
    identity: props.identity,
    rowKey: transcriptRowKey(props.identity, props.item.id),
    workspaceCwd: props.workspaceCwd,
    openFoldKeys: props.openFoldKeys,
    onToggleFold: props.onToggleFold,
    onLayoutChange: props.onLayoutChange,
    registerRow: props.registerRow,
  };
  transcriptViewportStats.renderItemCalls += 1;
  return props.renderItem(props.item, ctx);
}, transcriptRowPropsEqual);

function spacerStyle(height: number): { height: number; flexShrink: number; pointerEvents: 'none'; visibility: 'hidden' } {
  return {
    height,
    flexShrink: 0,
    pointerEvents: 'none',
    visibility: 'hidden',
  };
}

function measuredBoxHeight(el: HTMLElement): number {
  const style = typeof getComputedStyle === 'function' ? getComputedStyle(el) : null;
  const marginTop = style ? Number.parseFloat(style.marginTop) || 0 : 0;
  const marginBottom = style ? Number.parseFloat(style.marginBottom) || 0 : 0;
  return el.offsetHeight + marginTop + marginBottom;
}

export function readTranscriptRowGap(content: HTMLElement | null, fallback = TRANSCRIPT_ROW_GAP_PX): number {
  if (!content || typeof getComputedStyle !== 'function') return fallback;
  const raw = getComputedStyle(content).rowGap || getComputedStyle(content).gap;
  const gap = Number.parseFloat(raw);
  return Number.isFinite(gap) && gap >= 0 ? gap : fallback;
}

export function TranscriptViewport({
  identity,
  items,
  workspaceCwd,
  pending,
  rowHeights,
  estimates = defaultRowHeightEstimates(),
  transcriptScroll,
  openFoldKeys = EMPTY_FOLDS,
  emptyHint,
  rowGap: rowGapProp = TRANSCRIPT_ROW_GAP_PX,
  renderItem,
  onTranscriptScrollChange,
  onRowHeightsChange,
  onToggleFold,
  onLayoutInteractive,
}: TranscriptViewportProps): JSX.Element {
  const containerRef = useRef<HTMLDivElement>(null);
  const contentRef = useRef<HTMLDivElement>(null);
  const pinRef = useRef<ScrollPinController | null>(null);
  if (pinRef.current === null) pinRef.current = createScrollPin(transcriptScroll?.follow !== false);
  const heightsRef = useRef<Map<string, number>>(new Map(rowHeights));
  const estimatesRef = useRef<RowHeightEstimates>({ ...estimates });
  const itemsRef = useRef(items);
  const identityRef = useRef(identity);
  const pendingRef = useRef(pending);
  const scrollSlotRef = useRef<TranscriptScrollSlot>(transcriptScroll ?? defaultTranscriptScrollSlot());
  const rowNodesRef = useRef(new Map<string, HTMLElement>());
  const onRowHeightsChangeRef = useRef(onRowHeightsChange);
  const onTranscriptScrollChangeRef = useRef(onTranscriptScrollChange);
  const onLayoutInteractiveRef = useRef(onLayoutInteractive);
  const rowGapPropRef = useRef(rowGapProp);
  const registerRowFnsRef = useRef(new Map<string, (el: HTMLElement | null) => void>());
  const rowObserverRef = useRef<ResizeObserver | null>(null);
  const geometryObserverRef = useRef<ResizeObserver | null>(null);
  const persistScrollRafRef = useRef<number | null>(null);
  const interactiveRafRef = useRef<number | null>(null);
  const lastCommitKeyRef = useRef<string | null>(null);
  const lastRestoredKeyRef = useRef<string | null>(null);

  const runtimeKey = identity ? encodeRuntimeKey(identity) : '';
  itemsRef.current = items;
  identityRef.current = identity;
  pendingRef.current = pending;
  onRowHeightsChangeRef.current = onRowHeightsChange;
  onTranscriptScrollChangeRef.current = onTranscriptScrollChange;
  onLayoutInteractiveRef.current = onLayoutInteractive;
  rowGapPropRef.current = rowGapProp;
  heightsRef.current = new Map(rowHeights);
  for (const [id, node] of rowNodesRef.current) {
    if (node.dataset.runtimeKey === runtimeKey && !heightsRef.current.has(id) && node.isConnected) {
      heightsRef.current.set(id, measuredBoxHeight(node));
    }
  }
  estimatesRef.current = { ...estimates };

  const [geometry, setGeometry] = useState(() => ({
    scrollTop: transcriptScroll?.fallbackScrollTop ?? 0,
    clientHeight: 0,
    rowGap: rowGapProp,
  }));

  const follow = scrollSlotRef.current.follow;
  const gap = geometry.rowGap;

  const range: VirtualRange = useMemo(() => {
    const slot = scrollSlotRef.current;
    const clientHeight = geometry.clientHeight;
    const scrollTop = slot.follow && clientHeight > 0
      ? Math.max(0, restoreTranscriptAnchor(items, heightsRef.current, slot, clientHeight, estimatesRef.current, gap).scrollTop)
      : geometry.scrollTop;
    return computeVirtualRange({
      items,
      heights: heightsRef.current,
      estimates: estimatesRef.current,
      viewport: { scrollTop, clientHeight },
      follow: slot.follow,
      overscan: VIRTUAL_OVERSCAN,
      maxMounted: VIRTUAL_MAX_MOUNTED_ROWS,
      rowGap: gap,
    });
  }, [items, geometry.scrollTop, geometry.clientHeight, geometry.rowGap, follow, runtimeKey, gap, rowHeights, estimates, transcriptScroll]);
  const mountedIds = useMemo(() => new Set(collectMountedItemIds(items, range)), [items, range]);
  transcriptViewportStats.mountedRows = mountedIds.size;

  const applyFollowOrDelta = useCallback((delta: number) => {
    const el = containerRef.current;
    if (!el) return;
    if (scrollSlotRef.current.follow) {
      pinRef.current?.followIfPinned(el);
      return;
    }
    if (delta !== 0) el.scrollTop += delta;
  }, []);

  const measureNode = useCallback((itemId: string, node: HTMLElement) => {
    const height = measuredBoxHeight(node);
    if (!Number.isFinite(height) || height <= 0) return;
    const slot = scrollSlotRef.current;
    const state: VirtualState = {
      items: itemsRef.current,
      heights: heightsRef.current,
      estimates: estimatesRef.current,
      anchorIndex: slot.follow
        ? Math.max(0, itemsRef.current.length - 1)
        : itemsRef.current.findIndex((item) => item.id === slot.anchorItemId),
      follow: slot.follow,
    };
    if (state.anchorIndex < 0) state.anchorIndex = 0;
    const patch = applyMeasuredHeight(state, itemId, height);
    heightsRef.current = patch.heights;
    estimatesRef.current = patch.kindEstimates;
    applyFollowOrDelta(patch.scrollTopDelta);
    onRowHeightsChangeRef.current(patch);
  }, [applyFollowOrDelta]);

  const measureNodeRef = useRef(measureNode);
  measureNodeRef.current = measureNode;

  const persistScroll = useCallback(() => {
    const el = containerRef.current;
    if (!el) return;
    const measuredGap = readTranscriptRowGap(contentRef.current, rowGapPropRef.current);
    const slot = captureTranscriptAnchor(itemsRef.current, {
      scrollTop: el.scrollTop,
      clientHeight: el.clientHeight,
      scrollHeight: el.scrollHeight,
      heights: heightsRef.current,
      estimates: estimatesRef.current,
      rowGap: measuredGap,
    });
    scrollSlotRef.current = slot;
    pinRef.current?.onScroll(el);
    onTranscriptScrollChangeRef.current(slot);
    setGeometry((prev) => {
      if (prev.scrollTop === el.scrollTop && prev.clientHeight === el.clientHeight && prev.rowGap === measuredGap) return prev;
      return { scrollTop: el.scrollTop, clientHeight: el.clientHeight, rowGap: measuredGap };
    });
  }, []);

  const persistScrollRef = useRef(persistScroll);
  persistScrollRef.current = persistScroll;

  const registerRow = useCallback((itemId: string, el: HTMLElement | null) => {
    const prev = rowNodesRef.current.get(itemId);
    const observer = rowObserverRef.current;
    if (prev && prev !== el) {
      observer?.unobserve(prev);
      if (rowNodesRef.current.get(itemId) === prev) rowNodesRef.current.delete(itemId);
    }
    if (!el) {
      rowNodesRef.current.delete(itemId);
      return;
    }
    el.dataset.itemId = itemId;
    if (identityRef.current) el.dataset.runtimeKey = encodeRuntimeKey(identityRef.current);
    rowNodesRef.current.set(itemId, el);
    observer?.observe(el);
    measureNodeRef.current(itemId, el);
  }, []);

  const registerRowRef = useRef(registerRow);
  registerRowRef.current = registerRow;

  const registerRowFn = useCallback((itemId: string) => {
    const cached = registerRowFnsRef.current.get(itemId);
    if (cached) return cached;
    const fn = (el: HTMLElement | null) => registerRowRef.current(itemId, el);
    registerRowFnsRef.current.set(itemId, fn);
    return fn;
  }, []);

  const onScroll = useCallback((event: UIEvent<HTMLDivElement>) => {
    pinRef.current?.onScroll(event.currentTarget);
    if (persistScrollRafRef.current !== null) return;
    persistScrollRafRef.current = requestAnimationFrame(() => {
      persistScrollRafRef.current = null;
      persistScrollRef.current();
    });
  }, []);

  const onLayoutChange = useCallback(() => {
    const el = containerRef.current;
    if (!el) return;
    if (scrollSlotRef.current.follow) pinRef.current?.followIfPinned(el);
    for (const [itemId, node] of rowNodesRef.current) {
      if (node.isConnected) measureNodeRef.current(itemId, node);
    }
  }, []);

  useLayoutEffect(() => {
    const container = containerRef.current;
    const content = contentRef.current;
    if (!container) return;
    const gapNow = readTranscriptRowGap(content, rowGapPropRef.current);
    if (lastRestoredKeyRef.current !== runtimeKey) {
      lastRestoredKeyRef.current = runtimeKey;
      if (transcriptScroll) scrollSlotRef.current = transcriptScroll;
      const slot = scrollSlotRef.current;
      const restored = restoreTranscriptAnchor(
        items,
        heightsRef.current,
        slot,
        container.clientHeight,
        estimatesRef.current,
        gapNow,
      );
      if (slot.follow) pinRef.current?.forcePin(container);
      else container.scrollTop = restored.scrollTop;
    }
    setGeometry({
      scrollTop: container.scrollTop,
      clientHeight: container.clientHeight,
      rowGap: gapNow,
    });
  }, [runtimeKey, items, transcriptScroll]);

  useLayoutEffect(() => {
    if (interactiveRafRef.current !== null) {
      cancelAnimationFrame(interactiveRafRef.current);
      interactiveRafRef.current = null;
    }
    if (!identity || pending) {
      lastCommitKeyRef.current = identity ? `${encodeRuntimeKey(identity)}:pending` : null;
      return;
    }
    const commitKey = encodeRuntimeKey(identity);
    if (lastCommitKeyRef.current === commitKey) return;
    lastCommitKeyRef.current = commitKey;
    transcriptViewportStats.displayCommitMarks += 1;
    markSwitch('switch.display.commit');
    const stats = switchStats();
    if (stats) {
      stats.mountedRows = range.mountedCount;
      stats.transcriptIdentity = commitKey;
    }
    const scheduledIdentity = commitKey;
    interactiveRafRef.current = requestAnimationFrame(() => {
      markSwitch('switch.first.paint');
      interactiveRafRef.current = requestAnimationFrame(() => {
        interactiveRafRef.current = null;
        if (pendingRef.current) return;
        const current = identityRef.current;
        if (!current || encodeRuntimeKey(current) !== scheduledIdentity) return;
        markSwitch('switch.interactive');
        onLayoutInteractiveRef.current();
      });
    });
  }, [identity, pending, range.mountedCount]);

  useLayoutEffect(() => {
    if (typeof ResizeObserver === 'undefined') return;
    registerRowFnsRef.current.clear();
    const rowObserver = new ResizeObserver((entries) => {
      for (const entry of entries) {
        const node = entry.target as HTMLElement;
        const itemId = node.dataset.itemId;
        if (!itemId) continue;
        measureNodeRef.current(itemId, node);
      }
    });
    rowObserverRef.current = rowObserver;
    for (const node of rowNodesRef.current.values()) rowObserver.observe(node);

    const container = containerRef.current;
    const content = contentRef.current;
    const applyGeometry = () => {
      const el = containerRef.current;
      if (!el) return;
      const measuredGap = readTranscriptRowGap(contentRef.current, rowGapPropRef.current);
      if (scrollSlotRef.current.follow) pinRef.current?.followIfPinned(el);
      else persistScrollRef.current();
      setGeometry((prev) => {
        if (prev.clientHeight === el.clientHeight && prev.rowGap === measuredGap && prev.scrollTop === el.scrollTop) return prev;
        return { scrollTop: el.scrollTop, clientHeight: el.clientHeight, rowGap: measuredGap };
      });
    };
    const geometryObserver = new ResizeObserver(applyGeometry);
    geometryObserverRef.current = geometryObserver;
    if (container) geometryObserver.observe(container);
    if (content) geometryObserver.observe(content);
    applyGeometry();

    return () => {
      rowObserver.disconnect();
      geometryObserver.disconnect();
      if (rowObserverRef.current === rowObserver) rowObserverRef.current = null;
      if (geometryObserverRef.current === geometryObserver) geometryObserverRef.current = null;
    };
  }, [runtimeKey]);

  useLayoutEffect(() => {
    return () => {
      rowObserverRef.current?.disconnect();
      rowObserverRef.current = null;
      geometryObserverRef.current?.disconnect();
      geometryObserverRef.current = null;
      rowNodesRef.current.clear();
      registerRowFnsRef.current.clear();
      if (persistScrollRafRef.current !== null) cancelAnimationFrame(persistScrollRafRef.current);
      if (interactiveRafRef.current !== null) cancelAnimationFrame(interactiveRafRef.current);
    };
  }, []);

  const mountedItems: Item[] = [];
  for (let i = range.start; i < range.end; i++) mountedItems.push(items[i]);

  return (
    <main
      id="transcript"
      aria-live="polite"
      ref={containerRef}
      onScroll={onScroll}
      data-runtime-key={runtimeKey || undefined}
      data-pending={pending ? 'true' : 'false'}
    >
      <div className="transcript-content" ref={contentRef}>
        {items.length === 0 && emptyHint}
        {range.prefixHeight > 0 && (
          <div
            className="transcript-spacer transcript-spacer--prefix"
            aria-hidden="true"
            style={spacerStyle(range.prefixHeight)}
          />
        )}
        {identity && mountedItems.map((item) => (
          <TranscriptRow
            key={transcriptRowKey(identity, item.id)}
            identity={identity}
            item={item}
            runtimeKey={runtimeKey}
            foldSignature={itemFoldSignature(item, openFoldKeys)}
            workspaceCwd={workspaceCwd}
            openFoldKeys={openFoldKeys}
            renderItem={renderItem}
            onToggleFold={onToggleFold}
            onLayoutChange={onLayoutChange}
            registerRow={registerRowFn(item.id)}
          />
        ))}
        {range.suffixHeight > 0 && (
          <div
            className="transcript-spacer transcript-spacer--suffix"
            aria-hidden="true"
            style={spacerStyle(range.suffixHeight)}
          />
        )}
      </div>
    </main>
  );
}

export { TranscriptRow, VIRTUAL_MAX_MOUNTED_ROWS, VIRTUAL_OVERSCAN };
export type { RuntimeIdentity, TranscriptScrollSlot, VirtualMeasurementPatch, VirtualRange };
