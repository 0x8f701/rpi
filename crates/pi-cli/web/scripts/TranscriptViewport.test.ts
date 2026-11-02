#!/usr/bin/env node
// Effect-capable TranscriptViewport harness. Exercises ResizeObserver
// registration, scroll jumps beyond overscan, measured CSS gap, container
// resize, pending interactive suppression, delayed Mermaid unmount, and
// observer cleanup. createRoot + act so layout effects run.
import { createElement } from 'react';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import type { Item } from '../src/transcript.ts';
import { MarkdownBody } from '../src/MarkdownBody.tsx';
import mermaidMock from 'mermaid';
import { mermaidSvgCacheTest } from '../src/markdown.ts';
import {
  captureTranscriptAnchor,
  collectMountedItemIds,
  computeVirtualRange,
  defaultRowHeightEstimates,
  encodeRuntimeKey,
  hubFoldKey,
  ircFoldKey,
  itemFoldSignature,
  restoreTranscriptAnchor,
  shouldHydrateMarkdown,
  thinkingFoldKey,
  toolRawFoldKey,
  transcriptRowKey,
  TRANSCRIPT_ROW_GAP_PX,
  userAnalysisFoldKey,
  userLargeTextFoldKey,
  VIRTUAL_MAX_MOUNTED_ROWS,
} from '../src/transcriptVirtual.ts';
import {
  TranscriptViewport,
  readTranscriptRowGap,
  transcriptRowPropsEqual,
  transcriptViewportStats,
  type TranscriptRenderContext,
  type TranscriptRowProps,
  type TranscriptScrollSlot,
  type VirtualMeasurementPatch,
} from '../src/TranscriptViewport.tsx';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: boolean, detail?: string) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

function user(id: string): Item {
  return { kind: 'user', id, text: id, optimistic: false };
}

function itemsOf(n: number): Item[] {
  const out: Item[] = [];
  for (let i = 0; i < n; i++) out.push(user(`item-${i}`));
  return out;
}

const identityA = { sessionId: 'sess-a', runtimeIncarnation: 'inc-1' };
const identityB = { sessionId: 'sess-b', runtimeIncarnation: 'inc-2' };

type Observed = {
  node: FakeEl;
  callback: ResizeObserverCallback;
};

const observed = new Set<Observed>();
class FakeResizeObserver {
  callback: ResizeObserverCallback;
  nodes = new Set<FakeEl>();
  constructor(callback: ResizeObserverCallback) {
    this.callback = callback;
  }
  observe(node: FakeEl) {
    this.nodes.add(node);
    observed.add({ node, callback: this.callback });
    node.observers.add(this);
  }
  unobserve(node: FakeEl) {
    this.nodes.delete(node);
    node.observers.delete(this);
    for (const entry of [...observed]) {
      if (entry.node === node && entry.callback === this.callback) observed.delete(entry);
    }
  }
  disconnect() {
    for (const node of [...this.nodes]) this.unobserve(node);
  }
}

type FakeGeometry = {
  scrollTop: number;
  clientHeight: number;
  scrollHeight: number;
};

let nextTranscriptGeometry: FakeGeometry | null = null;

class FakeEl {
  tagName: string;
  nodeType = 1;
  parentNode: FakeEl | null = null;
  childNodes: FakeEl[] = [];
  attributes = new Map<string, string>();
  dataset: Record<string, string> = {};
  style: Record<string, string> = {};
  listeners = new Map<string, Set<(event: { type: string; currentTarget: FakeEl; target: FakeEl }) => void>>();
  observers = new Set<FakeResizeObserver>();
  ownerDocument: FakeDoc | null = null;
  text = '';
  isConnected = true;
  private scrollTopValue = 0;
  clientHeight = 600;
  offsetHeight = 80;
  scrollHeight = 0;
  get scrollTop() { return this.scrollTopValue; }
  set scrollTop(value: number) {
    this.scrollTopValue = Math.max(0, Math.min(value, Math.max(0, this.scrollHeight - this.clientHeight)));
  }
  constructor(tag: string) {
    this.tagName = tag.toUpperCase();
  }
  get className() { return this.attributes.get('class') ?? ''; }
  set className(value: string) { this.attributes.set('class', value); }
  get id() { return this.attributes.get('id') ?? ''; }
  set id(value: string) {
    this.attributes.set('id', value);
    if (value === 'transcript' && nextTranscriptGeometry) {
      this.scrollHeight = nextTranscriptGeometry.scrollHeight;
      this.clientHeight = nextTranscriptGeometry.clientHeight;
      this.offsetHeight = nextTranscriptGeometry.clientHeight;
      this.scrollTop = nextTranscriptGeometry.scrollTop;
    }
  }
  get firstChild() { return this.childNodes[0] ?? null; }
  get lastChild() { return this.childNodes[this.childNodes.length - 1] ?? null; }
  get nextSibling() {
    if (!this.parentNode) return null;
    const i = this.parentNode.childNodes.indexOf(this);
    return this.parentNode.childNodes[i + 1] ?? null;
  }
  get previousSibling() {
    if (!this.parentNode) return null;
    const i = this.parentNode.childNodes.indexOf(this);
    return this.parentNode.childNodes[i - 1] ?? null;
  }
  get innerHTML() { return this.text || this.childNodes.map((child) => child.innerHTML).join(''); }
  set innerHTML(value: string) {
    this.childNodes = [];
    this.text = value;
    const mermaidHost = value.match(/<div class="md-mermaid-host">([\s\S]*?)<\/div>/);
    if (mermaidHost) {
      this.text = '';
      const host = this.ownerDocument?.createElement('div') ?? new FakeEl('div');
      host.className = 'md-mermaid-host';
      host.textContent = mermaidHost[1]
        .replace(/&lt;/g, '<')
        .replace(/&gt;/g, '>')
        .replace(/&amp;/g, '&');
      this.appendChild(host);
    }
  }
  get textContent() { return this.text || this.childNodes.map((c) => c.textContent).join(''); }
  set textContent(value: string) {
    this.childNodes = [];
    this.text = value;
  }
  appendChild(node: FakeEl) {
    if (node.parentNode) node.parentNode.removeChild(node);
    node.parentNode = this;
    node.ownerDocument = this.ownerDocument;
    node.isConnected = this.isConnected;
    this.childNodes.push(node);
    return node;
  }
  insertBefore(node: FakeEl, before: FakeEl | null) {
    if (node.parentNode) node.parentNode.removeChild(node);
    node.parentNode = this;
    node.ownerDocument = this.ownerDocument;
    node.isConnected = this.isConnected;
    const i = before ? this.childNodes.indexOf(before) : -1;
    if (i < 0) this.childNodes.push(node);
    else this.childNodes.splice(i, 0, node);
    return node;
  }
  removeChild(node: FakeEl) {
    const i = this.childNodes.indexOf(node);
    if (i >= 0) this.childNodes.splice(i, 1);
    node.parentNode = null;
    const disconnect = (current: FakeEl) => {
      current.isConnected = false;
      for (const child of current.childNodes) disconnect(child);
    };
    disconnect(node);
    return node;
  }
  remove() {
    this.parentNode?.removeChild(this);
  }
  setAttribute(name: string, value: string) {
    if (name === 'id') this.id = value;
    else this.attributes.set(name, value);
    if (name.startsWith('data-')) {
      const key = name.slice(5).replace(/-([a-z])/g, (_, c: string) => c.toUpperCase());
      this.dataset[key] = value;
    }
  }
  getAttribute(name: string) { return this.attributes.get(name) ?? null; }
  removeAttribute(name: string) {
    this.attributes.delete(name);
    if (name.startsWith('data-')) {
      const key = name.slice(5).replace(/-([a-z])/g, (_, c: string) => c.toUpperCase());
      delete this.dataset[key];
    }
  }
  hasAttribute(name: string) { return this.attributes.has(name); }
  addEventListener(type: string, fn: (event: { type: string; currentTarget: FakeEl; target: FakeEl }) => void) {
    let set = this.listeners.get(type);
    if (!set) {
      set = new Set();
      this.listeners.set(type, set);
    }
    set.add(fn);
  }
  removeEventListener(type: string, fn: (event: { type: string; currentTarget: FakeEl; target: FakeEl }) => void) {
    this.listeners.get(type)?.delete(fn);
  }
  dispatchEvent(event: { type: string; currentTarget?: FakeEl; target?: FakeEl }) {
    const payload = { type: event.type, currentTarget: event.currentTarget ?? this, target: event.target ?? this };
    for (const fn of this.listeners.get(event.type) ?? []) fn(payload);
    return true;
  }
  querySelector(selector: string): FakeEl | null {
    return this.querySelectorAll(selector)[0] ?? null;
  }
  querySelectorAll(selector: string): FakeEl[] {
    const out: FakeEl[] = [];
    const walk = (node: FakeEl) => {
      if (matches(node, selector)) out.push(node);
      for (const child of node.childNodes) walk(child);
    };
    for (const child of this.childNodes) walk(child);
    return out;
  }
  contains(node: FakeEl | null) {
    if (!node) return false;
    let cur: FakeEl | null = node;
    while (cur) {
      if (cur === this) return true;
      cur = cur.parentNode;
    }
    return false;
  }
  focus() {}
  blur() {}
  click() { this.dispatchEvent({ type: 'click' }); }
}

function matches(node: FakeEl, selector: string): boolean {
  if (selector.startsWith('.')) return node.className.split(/\s+/).includes(selector.slice(1));
  if (selector.startsWith('#')) return node.id === selector.slice(1);
  if (selector.startsWith('[') && selector.endsWith(']')) {
    const body = selector.slice(1, -1);
    const eq = body.indexOf('=');
    if (eq < 0) return node.hasAttribute(body);
    const name = body.slice(0, eq);
    const value = body.slice(eq + 1).replace(/^["']|["']$/g, '');
    return node.getAttribute(name) === value;
  }
  return node.tagName === selector.toUpperCase();
}

class FakeDoc extends FakeEl {
  body: FakeEl;
  documentElement: FakeEl;
  defaultView: FakeWin | null = null;
  constructor() {
    super('#document');
    this.documentElement = new FakeEl('html');
    this.body = new FakeEl('body');
    this.documentElement.appendChild(this.body);
    this.appendChild(this.documentElement);
    this.ownerDocument = this;
    this.body.ownerDocument = this;
  }
  createElement(tag: string) {
    const el = new FakeEl(tag);
    el.ownerDocument = this;
    return el;
  }
  createElementNS(_ns: string, tag: string) { return this.createElement(tag); }
  createTextNode(text: string) {
    const el = new FakeEl('#text');
    el.text = text;
    el.nodeType = 3;
    el.ownerDocument = this;
    return el;
  }
  createComment(text: string) {
    const el = new FakeEl('#comment');
    el.text = text;
    el.nodeType = 8;
    el.ownerDocument = this;
    return el;
  }
  createDocumentFragment() { return this.createElement('#fragment'); }
  getElementById(id: string) { return this.querySelector(`#${id}`); }
}

const rafQueue: Array<{ id: number; fn: FrameRequestCallback }> = [];
let nextRafId = 1;
function flushRaf() {
  const due = rafQueue.splice(0);
  for (const frame of due) frame.fn(0);
}

class FakeWin {
  document: FakeDoc;
  HTMLElement = FakeEl;
  Element = FakeEl;
  Node = FakeEl;
  SVGElement = FakeEl;
  Text = FakeEl;
  Comment = FakeEl;
  DocumentFragment = FakeEl;
  HTMLIFrameElement = FakeEl;
  HTMLInputElement = FakeEl;
  HTMLButtonElement = FakeEl;
  HTMLTextAreaElement = FakeEl;
  ResizeObserver = FakeResizeObserver;
  requestAnimationFrame = (fn: FrameRequestCallback) => {
    const id = nextRafId;
    nextRafId += 1;
    rafQueue.push({ id, fn });
    return id;
  };
  cancelAnimationFrame = (id: number) => {
    const index = rafQueue.findIndex((frame) => frame.id === id);
    if (index >= 0) rafQueue.splice(index, 1);
  };
  addEventListener() {}
  removeEventListener() {}
  getComputedStyle(el: FakeEl) {
    return {
      rowGap: el.style.rowGap || `${TRANSCRIPT_ROW_GAP_PX}px`,
      gap: el.style.gap || el.style.rowGap || `${TRANSCRIPT_ROW_GAP_PX}px`,
      marginTop: el.style.marginTop || '0px',
      marginBottom: el.style.marginBottom || '0px',
    };
  }
  constructor(doc: FakeDoc) {
    this.document = doc;
    doc.defaultView = this;
  }
}

function installDom() {
  observed.clear();
  rafQueue.length = 0;
  nextRafId = 1;
  const document = new FakeDoc();
  const windowObj = new FakeWin(document);
  Object.defineProperty(globalThis, 'window', { configurable: true, writable: true, value: windowObj });
  Object.defineProperty(globalThis, 'document', { configurable: true, writable: true, value: document });
  Object.defineProperty(globalThis, 'ResizeObserver', { configurable: true, writable: true, value: FakeResizeObserver });
  globalThis.HTMLElement = FakeEl as unknown as typeof HTMLElement;
  globalThis.Element = FakeEl as unknown as typeof Element;
  globalThis.Node = FakeEl as unknown as typeof Node;
  globalThis.SVGElement = FakeEl as unknown as typeof SVGElement;
  globalThis.Text = FakeEl as unknown as typeof Text;
  (globalThis as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  globalThis.requestAnimationFrame = windowObj.requestAnimationFrame;
  globalThis.cancelAnimationFrame = windowObj.cancelAnimationFrame;
  globalThis.getComputedStyle = windowObj.getComputedStyle as unknown as typeof getComputedStyle;
  return { document, windowObj };
}

function flushResize(node: FakeEl) {
  let deliveries = 0;
  for (const entry of [...observed]) {
    if (entry.node === node) {
      deliveries += 1;
      entry.callback([{ target: node } as unknown as ResizeObserverEntry], {} as ResizeObserver);
    }
  }
  return deliveries;
}

let renderCalls = 0;
const renderedIds: string[] = [];
function renderItem(item: Item, ctx: TranscriptRenderContext) {
  renderCalls += 1;
  renderedIds.push(item.id);
  return createElement(
    'div',
    {
      className: `msg msg--${item.kind}`,
      'data-item-id': item.id,
      'data-runtime-key': encodeRuntimeKey(ctx.identity),
      'data-row-key': ctx.rowKey,
      ref: ctx.registerRow,
      style: { height: 80 },
    },
    item.id,
  );
}

function mountViewport(opts: {
  items: Item[];
  identity?: { sessionId: string; runtimeIncarnation: string };
  pending?: boolean;
  scroll?: TranscriptScrollSlot;
  heights?: Map<string, number>;
  onScrollChange?: (slot: TranscriptScrollSlot) => void;
  onHeights?: (patch: VirtualMeasurementPatch) => void;
  onInteractive?: () => void;
  geometry?: FakeGeometry;
}) {
  const { document } = installDom();
  const host = document.createElement('div');
  document.body.appendChild(host);
  const root: Root = createRoot(host as unknown as Element);
  renderCalls = 0;
  renderedIds.length = 0;
  transcriptViewportStats.reset();
  const identity = opts.identity ?? identityA;
  nextTranscriptGeometry = opts.geometry ?? null;
  act(() => {
    root.render(createElement(TranscriptViewport, {
      identity,
      items: opts.items,
      pending: opts.pending ?? false,
      rowHeights: opts.heights ?? new Map(),
      transcriptScroll: opts.scroll ?? { follow: true, anchorItemId: null, anchorOffsetPx: 0, fallbackScrollTop: 0 },
      openFoldKeys: new Set<string>(),
      renderItem,
      onTranscriptScrollChange: opts.onScrollChange ?? (() => {}),
      onRowHeightsChange: opts.onHeights ?? (() => {}),
      onToggleFold: () => {},
      onLayoutInteractive: opts.onInteractive ?? (() => {}),
    }));
  });
  nextTranscriptGeometry = null;
  const container = host.querySelector('#transcript') as FakeEl | null;
  const content = host.querySelector('.transcript-content') as FakeEl | null;
  if (container && !opts.geometry) {
    container.clientHeight = 600;
    container.offsetHeight = 600;
  }
  if (content) {
    content.style.rowGap = `${TRANSCRIPT_ROW_GAP_PX}px`;
    content.offsetHeight = opts.items.length * 80;
  }
  if (container) act(() => flushResize(container));
  if (content) act(() => flushResize(content));
  return { root, host, document, container, content };
}

{
  const { root, host, container, content } = mountViewport({ items: itemsOf(0) });
  check('empty still mounts one #transcript', !!container && host.querySelectorAll('#transcript').length === 1);
  check('empty still mounts one .transcript-content', !!content && host.querySelectorAll('.transcript-content').length === 1);
  check('empty list does not render item rows', renderCalls === 0);
  const gap = readTranscriptRowGap(content as unknown as HTMLElement, 0);
  check('measured empty content gap is 8 not silent 0', gap === 8, String(gap));
  act(() => root.unmount());
}

{
  const items = itemsOf(3);
  const { root, host, container } = mountViewport({ items });
  const rowCount = host.querySelectorAll('[data-item-id]').length;
  check('short list one #transcript', host.querySelectorAll('#transcript').length === 1);
  check('short list mounts every item in the DOM', rowCount === 3, String(rowCount));
  check('row observer attached to mounted rows', observed.size >= 3);
  check('container is observed', !!container && [...observed].some((entry) => entry.node === container));
  act(() => root.unmount());
  check('unmount disconnects observers', observed.size === 0, String(observed.size));
}

{
  const items = itemsOf(400);
  const heights = new Map<string, number>(items.map((item) => [item.id, 80]));
  const stride = 80 + TRANSCRIPT_ROW_GAP_PX;
  const farTop = 250 * stride + 17;
  const scrollHeight = 400 * 80 + 399 * TRANSCRIPT_ROW_GAP_PX;
  let persistedScroll: TranscriptScrollSlot | null = null;
  const { root, host, container, content } = mountViewport({
    items,
    heights,
    scroll: { follow: false, anchorItemId: items[0].id, anchorOffsetPx: 0, fallbackScrollTop: 0 },
    geometry: { scrollTop: 0, clientHeight: 400, scrollHeight },
    onScrollChange: (slot) => { persistedScroll = slot; },
  });
  act(() => flushRaf());
  act(() => flushRaf());
  if (container) {
    container.scrollTop = farTop;
    act(() => container.dispatchEvent({ type: 'scroll', currentTarget: container }));
    act(() => flushRaf());
  }
  check(
    'unpinned scroll commits far anchor before observer phase',
    persistedScroll?.follow === false
      && persistedScroll.anchorItemId === 'item-250'
      && persistedScroll.anchorOffsetPx === 17
      && persistedScroll.fallbackScrollTop === farTop,
    JSON.stringify(persistedScroll),
  );
  check('prefix spacer present after PageDown beyond overscan', !!host.querySelector('.transcript-spacer--prefix'));
  const range = computeVirtualRange({
    items,
    heights,
    estimates: defaultRowHeightEstimates(),
    viewport: { scrollTop: farTop, clientHeight: 400 },
    follow: false,
    rowGap: TRANSCRIPT_ROW_GAP_PX,
  });
  check('recomputed range after jump is still <= 80', range.mountedCount <= 80);
  check('recomputed range includes the far item', range.start <= 250 && range.end > 250);
  const beforeGapIds = host.querySelectorAll('[data-item-id]').map((node) => node.dataset.itemId);
  const beforePrefixHeight = Number.parseFloat(host.querySelector('.transcript-spacer--prefix')?.style.height ?? '0');
  const beforeSuffixHeight = Number.parseFloat(host.querySelector('.transcript-spacer--suffix')?.style.height ?? '0');
  check('unpinned pre-gap window is exact', beforeGapIds[0] === 'item-242' && beforeGapIds.at(-1) === 'item-262' && beforeGapIds.length === 21, beforeGapIds.join(','));
  check('unpinned pre-gap spacers are exact', beforePrefixHeight === 21_288 && beforeSuffixHeight === 12_048, `${beforePrefixHeight}/${beforeSuffixHeight}`);
  if (content) {
    content.style.rowGap = '16px';
    let contentDeliveries = 0;
    act(() => {
      contentDeliveries = flushResize(content);
      flushRaf();
    });
    check('content-only observer callback is delivered', contentDeliveries > 0, String(contentDeliveries));
  }
  const afterGapIds = host.querySelectorAll('[data-item-id]').map((node) => node.dataset.itemId);
  const afterPrefixHeight = Number.parseFloat(host.querySelector('.transcript-spacer--prefix')?.style.height ?? '0');
  const afterSuffixHeight = Number.parseFloat(host.querySelector('.transcript-spacer--suffix')?.style.height ?? '0');
  check('content observer changes only the computed-gap window exactly', afterGapIds[0] === 'item-221' && afterGapIds.at(-1) === 'item-241' && afterGapIds.length === 21, afterGapIds.join(','));
  check('content observer changes only the computed-gap spacers exactly', afterPrefixHeight === 21_200 && afterSuffixHeight === 15_152, `${afterPrefixHeight}/${afterSuffixHeight}`);
  check('content observer preserves fixed clientHeight', container?.clientHeight === 400, String(container?.clientHeight));
  check('content observer preserves unpinned scrollTop', container?.scrollTop === farTop, String(container?.scrollTop));
  check('content observer keeps unpinned range bounded', afterGapIds.length <= VIRTUAL_MAX_MOUNTED_ROWS, String(afterGapIds.length));
  act(() => root.unmount());
  check('unmount after content resize clears observers', observed.size === 0, String(observed.size));
}

{
  const items = itemsOf(400);
  const heights = new Map<string, number>(items.map((item) => [item.id, 80]));
  const scrollHeight = 400 * 80 + 399 * TRANSCRIPT_ROW_GAP_PX;
  const { root, host, container, content } = mountViewport({
    items,
    heights,
    geometry: { scrollTop: scrollHeight, clientHeight: 600, scrollHeight },
  });
  const beforeResizeIds = host.querySelectorAll('[data-item-id]').map((node) => node.dataset.itemId);
  const beforePrefixHeight = Number.parseFloat(host.querySelector('.transcript-spacer--prefix')?.style.height ?? '0');
  check('pinned pre-container-resize window is exact', beforeResizeIds[0] === 'item-385' && beforeResizeIds.at(-1) === 'item-399' && beforeResizeIds.length === 15, beforeResizeIds.join(','));
  check('pinned pre-container-resize prefix is exact', beforePrefixHeight === 33_872, String(beforePrefixHeight));
  if (container) {
    container.clientHeight = 240;
    let containerDeliveries = 0;
    act(() => {
      containerDeliveries = flushResize(container);
      flushRaf();
    });
    check('container-only observer callback is delivered', containerDeliveries > 0, String(containerDeliveries));
  }
  const afterResizeIds = host.querySelectorAll('[data-item-id]').map((node) => node.dataset.itemId);
  const afterPrefixHeight = Number.parseFloat(host.querySelector('.transcript-spacer--prefix')?.style.height ?? '0');
  check('container observer changes only the pinned mounted range exactly', afterResizeIds[0] === 'item-389' && afterResizeIds.at(-1) === 'item-399' && afterResizeIds.length === 11, afterResizeIds.join(','));
  check('container observer changes only the pinned prefix exactly', afterPrefixHeight === 34_224, String(afterPrefixHeight));
  check('container observer preserves fixed computed gap', content?.style.rowGap === `${TRANSCRIPT_ROW_GAP_PX}px`, content?.style.rowGap);
  check('container observer semantically re-pins for the new clientHeight', container?.scrollTop === 34_952, String(container?.scrollTop));
  check('container observer keeps pinned range bounded', afterResizeIds.length <= VIRTUAL_MAX_MOUNTED_ROWS, String(afterResizeIds.length));
  act(() => root.unmount());
  check('unmount after container resize clears observers', observed.size === 0, String(observed.size));
}

{
  const items = itemsOf(20);
  let patches = 0;
  let lastIdentityKey = '';
  const onHeights = (patch: VirtualMeasurementPatch) => {
    patches += 1;
    lastIdentityKey = encodeRuntimeKey(identityA);
    void patch;
  };
  const { root, host } = mountViewport({ items, onHeights });
  const firstPatches = patches;
  check('initial measure reports heights', firstPatches > 0);
  act(() => {
    root.render(createElement(TranscriptViewport, {
      identity: identityB,
      items,
      pending: false,
      rowHeights: new Map(),
      transcriptScroll: { follow: true, anchorItemId: null, anchorOffsetPx: 0, fallbackScrollTop: 0 },
      openFoldKeys: new Set<string>(),
      renderItem,
      onTranscriptScrollChange: () => {},
      onRowHeightsChange: (patch) => {
        patches += 1;
        lastIdentityKey = encodeRuntimeKey(identityB);
        void patch;
      },
      onToggleFold: () => {},
      onLayoutInteractive: () => {},
    }));
  });
  check('runtime switch retargets height patches', lastIdentityKey === encodeRuntimeKey(identityB), lastIdentityKey);
  check('displayed runtime key is B after switch', !!host.querySelector(`[data-runtime-key="${encodeRuntimeKey(identityB)}"]`));
  check('inactive runtime key is absent', !host.querySelector(`[data-runtime-key="${encodeRuntimeKey(identityA)}"]`));
  act(() => root.unmount());
}

{
  let interactive = 0;
  const { root } = mountViewport({
    items: itemsOf(3),
    pending: true,
    onInteractive: () => { interactive += 1; },
  });
  act(() => {
    flushRaf();
    flushRaf();
  });
  check('pending=true never calls onLayoutInteractive', interactive === 0, String(interactive));
  act(() => root.unmount());
  act(() => {
    flushRaf();
    flushRaf();
  });
  check('unmount cancels pending interactive RAFs', interactive === 0);
}

{
  let interactive = 0;
  const { root } = mountViewport({
    items: itemsOf(3),
    pending: false,
    onInteractive: () => { interactive += 1; },
  });
  act(() => {
    flushRaf();
    flushRaf();
  });
  check('pending=false fires onLayoutInteractive after two RAFs', interactive === 1, String(interactive));
  act(() => root.unmount());
}

{
  mermaidSvgCacheTest.reset();
  const { document } = installDom();
  const host = document.createElement('div');
  document.body.appendChild(host);
  const root: Root = createRoot(host as unknown as Element);
  let layout = 0;
  let started = 0;
  const delayed = Promise.withResolvers<{ svg: string }>();
  const originalRender = mermaidMock.render;
  mermaidMock.render = () => {
    started += 1;
    return delayed.promise;
  };
  try {
    act(() => {
      root.render(createElement(MarkdownBody, {
        text: '```mermaid\ngraph TD; A-->B;\n```',
        onLayoutChange: () => { layout += 1; },
      }));
    });
    const body = host.querySelector('.md-mermaid-host');
    check('mermaid effect starts one async render', started === 1, String(started));
    check('mermaid host is mounted before delay', body !== null);
    act(() => root.unmount());
    check('unmount disconnects mermaid host', body?.isConnected === false);
    act(() => {
      delayed.resolve({ svg: '<svg class="late-svg" xmlns="http://www.w3.org/2000/svg"></svg>' });
    });
    check('delayed mermaid after unmount does not call onLayoutChange', layout === 0, String(layout));
    check('delayed mermaid after unmount does not mutate disconnected host', !body?.innerHTML.includes('late-svg'));
  } finally {
    mermaidMock.render = originalRender;
  }
}

{
  const items = itemsOf(20_000);
  const range = computeVirtualRange({
    items,
    heights: new Map(),
    estimates: defaultRowHeightEstimates(),
    viewport: { scrollTop: 0, clientHeight: 720 },
    follow: true,
    rowGap: TRANSCRIPT_ROW_GAP_PX,
  });
  const mounted = collectMountedItemIds(items, range);
  check('20k follow mountedCount <= 80', mounted.length <= 80);
  check('20k follow last item is mounted', mounted[mounted.length - 1] === items[items.length - 1].id);
  check('offscreen markdown hydration is false', !shouldHydrateMarkdown(items[0].id, new Set(mounted)));
  const captured = captureTranscriptAnchor(items, {
    scrollTop: 12_345,
    clientHeight: 720,
    scrollHeight: range.totalHeight,
    heights: new Map(),
    estimates: defaultRowHeightEstimates(),
    rowGap: TRANSCRIPT_ROW_GAP_PX,
  });
  const restored = restoreTranscriptAnchor(items, new Map(), captured, 720, defaultRowHeightEstimates(), TRANSCRIPT_ROW_GAP_PX);
  check('full-prefix capture restores the same item', items[restored.anchorIndex].id === captured.anchorItemId);
}

{
  check('thinking fold key round-trips', thinkingFoldKey('msg-1') === 'msg-1:thinking');
  check('tool-raw fold key round-trips', toolRawFoldKey('msg-2', 'call-0') === 'msg-2:tool-raw:call-0');
  check('user-large-text fold key round-trips', userLargeTextFoldKey('msg-3') === 'msg-3:user-large-text');
  check('user-analysis fold key round-trips', userAnalysisFoldKey('msg-3') === 'msg-3:user-analysis');
  check('irc fold key round-trips', ircFoldKey('msg-4') === 'msg-4:irc');
  check('hub fold key round-trips', hubFoldKey('msg-5') === 'msg-5:hub');
  const item: Item = { kind: 'assistant', id: 'msg-1', status: 'final', blocks: [] };
  check('fold signature changes when thinking opens', itemFoldSignature(item, new Set(['msg-1:thinking'])) === 'msg-1:thinking,');
}

{
  const item: Item = user('item-9');
  const base: TranscriptRowProps = {
    identity: identityA,
    item,
    runtimeKey: encodeRuntimeKey(identityA),
    foldSignature: '',
    workspaceCwd: '/tmp',
    openFoldKeys: new Set(),
    renderItem,
    onToggleFold: () => {},
    onLayoutChange: () => {},
    registerRow: () => {},
  };
  check('row memo equal on same item reference', transcriptRowPropsEqual(base, { ...base }));
  check('row memo unequal on item identity change', !transcriptRowPropsEqual(base, { ...base, item: user('item-9') }));
  check(
    'row memo unequal on runtime incarnation change',
    !transcriptRowPropsEqual(base, { ...base, identity: identityB, runtimeKey: encodeRuntimeKey(identityB) }),
  );
  check('row memo unequal on fold signature change', !transcriptRowPropsEqual(base, { ...base, foldSignature: 'item-9:user-large-text,' }));
  const otherCb = () => {};
  check('row memo unequal on callback identity change', !transcriptRowPropsEqual(base, { ...base, onLayoutChange: otherCb }));
}

{
  const rowKey = transcriptRowKey(identityA, 'item-0');
  check('stable row key is runtimeKey + item.id', rowKey === `${encodeRuntimeKey(identityA)}:item-0`);
}

console.log(`\nTranscriptViewport.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
