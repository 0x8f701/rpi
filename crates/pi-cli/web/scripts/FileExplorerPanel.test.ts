#!/usr/bin/env node
// Header chrome a11y plus delayed per-pane loading: static Files, tree toggle
// left of the title, icon-only Refresh, then compact ⛶ ×; tree and content
// indicators appear only after 151ms for their own request identity.
import { createElement, useEffect, useLayoutEffect, useState, type Dispatch, type SetStateAction } from 'react';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  FileExplorerPanel,
  FileExplorerPaneLoading,
  FILE_EXPLORER_CLOSE_BTN_ID,
  FILE_EXPLORER_LOADING_DELAY_MS,
  FILE_EXPLORER_REFRESH_BTN_ID,
  FILE_EXPLORER_TREE_TOGGLE_ID,
  FILE_EXPLORER_TOGGLE_ID,
  fileExplorerBodyLayout,
  fileExplorerContentLoadingIdentity,
  fileExplorerControlId,
  fileExplorerIsRefreshBusy,
  fileExplorerTreeLoadingIdentity,
  fileExplorerLineDataOfNode,
  fileExplorerSelectionAnchor,
  fileExplorerSelectionFromSelection,
  fileExplorerSelectionRange,
  FILE_EXPLORER_SELECTION_EDGE_PX,
  FILE_CONVERSATION_POLL_IDLE_MS,
  FILE_CONVERSATION_POLL_STREAMING_MS,
  useFileExplorerDelayedLoading,
} from '../src/panels/FileExplorerPanel.tsx';
import {
  emptyFileExplorerPageCache,
  emptyFileExplorerSlot,
  fileExplorerApplyCachedPage,
  fileExplorerBeginCachedPage,
  fileExplorerSyncPageCacheWithSlots,
  emptyFileExplorerViewState,
  fileExplorerConversationDraft,
  fileExplorerViewForOwner,
  fileExplorerWithPathSurface,
  fileExplorerWithConversationDraft,
  getFileExplorerViewCacheSnapshot,
  fileExplorerWriteDirState,
  updateFileExplorerDirCache,
  updateFileExplorerViewCache,
  fileExplorerPageCacheKey,
  fileExplorerReadCachedPage,
  readModuleFileExplorerPageCache,
  fileExplorerConversationToastLatchesResetAll,
  fileExplorerPeekSnapshotFlight,
  fileExplorerResetSnapshotFlights,
  fileExplorerSnapshotFlightKey,
  writeModuleFileExplorerPageCache,
  type FileExplorerSessionSlot,
} from '../src/fileExplorer.ts';

const failures: string[] = [];
let ran = 0;
function check(name: string, condition: unknown, detail = ''): void {
  ran += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}


const panelProps = {
  sendCommand: () => Promise.resolve({}),
  hostAuthority: '127.0.0.1:1',
  pageProtocol: 'http:',
  connectionEpoch: 0,
  onPatchSlot: () => {},
  onConversationError: () => {},
  onOpenPath: () => {},
  onClose: () => {},
};

function renderExplorer(extra: Record<string, unknown> = {}): string {
  return renderToStaticMarkup(createElement(FileExplorerPanel, {
    ...panelProps,
    sessionId: 'sess-header',
    slot: emptyFileExplorerSlot(),
    ...extra,
  }));
}

function headerSlice(html: string): string {
  const start = html.indexOf('data-testid="file-explorer-header"');
  const body = html.indexOf('data-testid="file-explorer-layout"');
  return start >= 0 && body > start ? html.slice(start, body) : '';
}

{
  const html = renderExplorer();
  const head = headerSlice(html);
  check('a11y: Files title is static text outside the controls', head.includes('>Files<') && !html.includes('file-explorer-hide-btn') && !html.includes('file-explorer__title-button') && !head.includes(FILE_EXPLORER_TREE_TOGGLE_ID) && !head.includes('File surface'));
  check('a11y: Graph is an icon-only header action', head.includes('data-testid="file-explorer-map-toggle"') && head.includes('aria-label="Graph"') && !head.includes('>Graph<') && !head.includes('◇') && !head.includes('File surface'));
  check('a11y: Refresh keeps its accessible name and has no text label', head.includes(`id="${FILE_EXPLORER_REFRESH_BTN_ID}"`) && head.includes('aria-label="Refresh"') && head.includes('title="Refresh"') && head.includes('aria-hidden="true"') && !head.includes('file-explorer__refresh-label') && !head.includes('Refreshing'));
  check('a11y: close button names Close file explorer', head.includes(`id="${FILE_EXPLORER_CLOSE_BTN_ID}"`) && head.includes('aria-label="Close file explorer"') && head.includes('title="Close file explorer"'));
  check('a11y: header has no expand control', !html.includes('expand-btn') && !html.includes('Expand file explorer') && !html.includes('Shrink file explorer'));
  check('a11y: expanded search is the tree toolbar and precedes the only toggle', html.includes('data-testid="file-explorer-search"') && html.includes('aria-label="Search workspace files and folders"') && !html.includes('file-explorer__tree-edge') && html.indexOf('data-testid="file-explorer-tree"') < html.indexOf('file-explorer-search') && html.indexOf('file-explorer-search') < html.indexOf(`id="${FILE_EXPLORER_TREE_TOGGLE_ID}"`) && html.includes('aria-label="Collapse file tree"') && (html.match(new RegExp(`id="${FILE_EXPLORER_TREE_TOGGLE_ID}"`, 'g')) ?? []).length === 1);
  check('a11y: tree viewport starts visible', html.includes('data-testid="file-explorer-tree"') && !/data-testid="file-explorer-tree"[^>]*hidden/.test(html));
  check('a11y: desktop SSR layout stays split', html.includes('data-testid="file-explorer-layout"') && html.includes('data-layout="split"'));
}

{
  const html = renderExplorer({ exclusivePanes: true });
  check('exclusive panes SSR is tree not split', html.includes('data-layout="tree"') && !html.includes('data-layout="split"'));
  check('exclusive panes SSR hides content until a file opens', /data-testid="file-explorer-content"[^>]*\bhidden/.test(html));
  check('exclusive panes SSR hides the split divider', /data-testid="file-explorer-divider"[^>]*\bhidden/.test(html));
}

{
  const html = renderToStaticMarkup(createElement(FileExplorerPanel, {
    ...panelProps,
    exclusivePanes: true,
    sessionId: 'sess-exclusive-active',
    slot: { tabs: [{ path: 'src/a.ts', view: 'file' }], activePath: 'src/a.ts', mode: 'open' },
  }));
  check('exclusive panes with active path SSR is content', html.includes('data-layout="content"') && !html.includes('data-layout="split"'));
  check('exclusive panes with active path SSR shows content', html.includes('data-testid="file-explorer-content"') && !/data-testid="file-explorer-content"[^>]*\bhidden/.test(html));
  check('exclusive panes with active path SSR hides the tree', /data-testid="file-explorer-tree"[^>]*\bhidden/.test(html));
}



{
  fileExplorerSyncPageCacheWithSlots({
    'sess-md': { tabs: [{ path: 'docs/README.md', view: 'file' }], activePath: 'docs/README.md', mode: 'open' },
  });
  const request = { sessionId: 'sess-md', path: 'docs/README.md', view: 'file' as const, generation: 1 };
  let cache = emptyFileExplorerPageCache();
  cache = fileExplorerBeginCachedPage(cache, request);
  cache = fileExplorerApplyCachedPage(cache, request, {
    path: 'docs/README.md',
    lines: ['# Title'],
    truncated: false,
  });
  writeModuleFileExplorerPageCache(cache);
  const html = renderToStaticMarkup(createElement(FileExplorerPanel, {
    ...panelProps,
    sessionId: 'sess-md',
    slot: { tabs: [{ path: 'docs/README.md', view: 'file' }], activePath: 'docs/README.md', mode: 'open' },
  }));
  const head = headerSlice(html);
  const content = html.slice(html.indexOf('data-testid="file-explorer-content"'));
  check('a11y: surface group lives outside the header', !head.includes('File surface'));
  check('a11y: header does not contain the tree toggle or Ask', !head.includes(FILE_EXPLORER_TREE_TOGGLE_ID) && !head.includes('>Ask<') && head.includes('aria-label="Refresh"') && head.includes(FILE_EXPLORER_CLOSE_BTN_ID));
}
{
  const css = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '..', 'src', 'styles.css'), 'utf8');
  check(
    'content toolbar keeps 44px surface controls',
    /\.file-explorer__surface-btn\s*\{[^}]*min-height:\s*44px/.test(css)
      || /file-explorer__surface-btn[\s\S]{0,180}min-height:\s*44px/.test(css),
  );
  check(
    'header touch controls stay at least 44px',
    /file-explorer__refresh-btn[\s\S]{0,220}min-width:\s*44px/.test(css)
      && /file-explorer__refresh-btn[\s\S]{0,220}min-height:\s*44px/.test(css)
      && /file-explorer__close[\s\S]{0,220}min-(?:width|height):\s*44px/.test(css),
  );
}

{
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  fileExplorerSyncPageCacheWithSlots({
    'sess-loading': { tabs: [{ path: 'src/a.ts', view: 'file' }], activePath: 'src/a.ts', mode: 'open' },
  });
  const html = renderToStaticMarkup(createElement(FileExplorerPanel, {
    ...panelProps,
    sessionId: 'sess-loading',
    slot: { tabs: [{ path: 'src/a.ts', view: 'file' }], activePath: 'src/a.ts', mode: 'open' },
  }));
  check('immediate spinner bug: first paint has no spinner', !html.includes('file-explorer__spinner'));
  check('empty-div layout gap: first paint has no empty loading div', !html.includes('file-explorer__loading'));
  check('fast first paint never renders the pane shimmer', !html.includes('file-explorer__pane-shimmer') && !html.includes('file-explorer__pane-loading'));
  check('tree search remains while content is not ready', html.includes('data-testid="file-explorer-search"') && html.includes('data-testid="file-explorer-tree"') && html.includes('data-testid="file-explorer-content"'));
}

{
  const tree = fileExplorerTreeLoadingIdentity('sess-id', { loading: true, generation: 4, entries: [] }, true, 8, 0);
  const content = fileExplorerContentLoadingIdentity('sess-id', 'src/a.ts', 'file', 'loading', 4);
  check('tree and content identities stay distinct', tree != null && content != null && tree !== content);
  check('idle tree identity is null', fileExplorerTreeLoadingIdentity('sess-id', { loading: false, generation: 4, entries: [] }, false, 8, 0) == null);
  check('ready content identity is null', fileExplorerContentLoadingIdentity('sess-id', 'src/a.ts', 'file', 'ready', 4) == null);
  const nextTree = fileExplorerTreeLoadingIdentity('sess-id', { loading: true, generation: 5, entries: [] }, true, 8, 0);
  check('tree generation replacement is a new identity', tree != null && nextTree != null && tree !== nextTree);
  const nextContent = fileExplorerContentLoadingIdentity('sess-id', 'src/b.ts', 'file', 'loading', 4);
  check('content path replacement is a new identity', content != null && nextContent != null && content !== nextContent);
  check(
    'refresh does not get a tree shimmer identity when entries exist',
    fileExplorerTreeLoadingIdentity('sess-id', {
      loading: true,
      generation: 4,
      entries: [{ name: 'a.ts', path: 'a.ts', kind: 'file' }],
    }, true, 8, 0) == null,
  );
  check(
    'refresh does not get a tree shimmer identity when changes exist',
    fileExplorerTreeLoadingIdentity('sess-id', { loading: true, generation: 4, entries: [] }, true, 8, 2) == null,
  );
  check('initial empty tree still gets an identity', fileExplorerTreeLoadingIdentity('sess-id', { loading: true, generation: 4, entries: [] }, false, 8, 0) != null);
  check('refresh busy only when a painted pane is loading', fileExplorerIsRefreshBusy(true, false, true, false) === true);
  check('initial empty load is not refresh busy', fileExplorerIsRefreshBusy(false, false, true, true) === false);
  check('painted idle is not refresh busy', fileExplorerIsRefreshBusy(true, true, false, false) === false);
}

{
  check('body layout: wide is split with tree open', fileExplorerBodyLayout(false, false) === 'split');
  check('body layout: wide stays split when tree collapsed', fileExplorerBodyLayout(false, true) === 'split');
  check('body layout: phone open tree is tree', fileExplorerBodyLayout(true, false) === 'tree');
  check('body layout: phone collapsed tree is content', fileExplorerBodyLayout(true, true) === 'content');
}

type MiniListener = (event: MiniEvent) => void;
type MiniEvent = {
  type: string;
  target: MiniNode;
  currentTarget: MiniNode | null;
  bubbles: boolean;
  cancelable: boolean;
  defaultPrevented: boolean;
  button: number;
  key: string;
  preventDefault(): void;
  stopPropagation(): void;
  stopImmediatePropagation(): void;
};

type MiniNode = {
  nodeType: number;
  nodeName: string;
  tagName?: string;
  namespaceURI: string | null;
  ownerDocument: MiniDocument;
  parentNode: MiniNode | null;
  childNodes: MiniNode[];
  firstChild: MiniNode | null;
  lastChild: MiniNode | null;
  nextSibling: MiniNode | null;
  previousSibling: MiniNode | null;
  attributes: Map<string, string>;
  style: Record<string, unknown>;
  listeners: Map<string, Set<MiniListener>>;
  text: string;
  innerHTML: string;
  textContent: string;
  className: string;
  id: string;
  appendChild(node: MiniNode): MiniNode;
  removeChild(node: MiniNode): MiniNode;
  insertBefore(node: MiniNode, before: MiniNode | null): MiniNode;
  remove(): void;
  setAttribute(name: string, value: string): void;
  getAttribute(name: string): string | null;
  removeAttribute(name: string): void;
  hasAttribute(name: string): boolean;
  setAttributeNS(_ns: string | null, name: string, value: string): void;
  getAttributeNS(_ns: string | null, name: string): string | null;
  addEventListener(type: string, fn: MiniListener, options?: boolean | { capture?: boolean }): void;
  removeEventListener(type: string, fn: MiniListener, options?: boolean | { capture?: boolean }): void;
  dispatchEvent(event: { type: string; target?: MiniNode; key?: string }): boolean;
  click(): void;
  focus(): void;
  blur(): void;
  contains(node: MiniNode | null): boolean;
  querySelector(selector: string): MiniNode | null;
  querySelectorAll(selector: string): MiniNode[];
  cloneNode(deep?: boolean): MiniNode;
  /** Stable fixture box. Zero size is not layout proof. */
  getBoundingClientRect(): {
    x: number; y: number; top: number; left: number;
    right: number; bottom: number; width: number; height: number;
  };
};

type MiniDocument = MiniNode & {
  nodeType: 9;
  documentElement: MiniNode;
  head: MiniNode;
  body: MiniNode;
  activeElement: MiniNode | null;
  defaultView: MiniWindow;
  createElement(tag: string): MiniNode;
  createElementNS(ns: string, tag: string): MiniNode;
  createTextNode(text: string): MiniNode;
  createComment(text: string): MiniNode;
  createDocumentFragment(): MiniNode;
  getElementById(id: string): MiniNode | null;
};
type MiniSelection = {
  anchorNode: MiniNode | null;
  focusNode: MiniNode | null;
  isCollapsed: boolean;
};
type MiniWindow = {
  document: MiniDocument;
  HTMLElement: new () => object;
  HTMLIFrameElement: new () => object;
  HTMLInputElement: new () => object;
  HTMLButtonElement: new () => object;
  HTMLTextAreaElement: new () => object;
  SVGElement: new () => object;
  Element: new () => object;
  Node: new () => object;
  Text: new () => object;
  Comment: new () => object;
  DocumentFragment: new () => object;
  setTimeout: (fn: () => void, ms?: number) => number;
  clearTimeout: (id: number) => void;
  addEventListener(): void;
  removeEventListener(): void;
  matchMedia(query: string): { matches: boolean; addEventListener(): void; removeEventListener(): void };
  getSelection(): MiniSelection | null;
};

function createMiniDom(narrow = false): { window: MiniWindow; document: MiniDocument } {
  class FakeCtor {}
  const relink = (parent: MiniNode) => {
    parent.firstChild = parent.childNodes[0] ?? null;
    parent.lastChild = parent.childNodes[parent.childNodes.length - 1] ?? null;
    for (let i = 0; i < parent.childNodes.length; i += 1) {
      const child = parent.childNodes[i];
      child.parentNode = parent;
      child.previousSibling = parent.childNodes[i - 1] ?? null;
      child.nextSibling = parent.childNodes[i + 1] ?? null;
    }
  };

  const makeNode = (nodeType: number, nodeName: string, owner: MiniDocument | null): MiniNode => {
    const node = {
      nodeType,
      nodeName,
      tagName: nodeType === 1 ? nodeName : undefined,
      namespaceURI: nodeType === 1 ? 'http://www.w3.org/1999/xhtml' : null,
      ownerDocument: owner as MiniDocument,
      parentNode: null,
      childNodes: [] as MiniNode[],
      firstChild: null,
      lastChild: null,
      nextSibling: null,
      previousSibling: null,
      attributes: new Map<string, string>(),
      style: {
        setProperty(name: string, value: string): void {
          this[name as string] = value;
        },
        removeProperty(name: string): string {
          const previous = this[name];
          delete this[name];
          return typeof previous === 'string' ? previous : '';
        },
        getPropertyValue(name: string): string {
          const value = this[name];
          return typeof value === 'string' ? value : '';
        },
      },
      listeners: new Map(),
      text: '',
      get nodeValue() {
        return nodeType === 3 || nodeType === 8 ? node.text : null;
      },
      set nodeValue(value: string | null) {
        if (nodeType === 3 || nodeType === 8) node.text = value ?? '';
      },
      get textContent() {
        if (nodeType === 3 || nodeType === 8) return node.text;
        return node.childNodes.map((child) => child.textContent).join('');
      },
      set textContent(value: string) {
        node.childNodes = [];
        node.firstChild = null;
        node.lastChild = null;
        if (nodeType === 3 || nodeType === 8) {
          node.text = value;
          return;
        }
        if (value !== '') node.appendChild(ownerDocument.createTextNode(value));
      },
      set innerHTML(html: string) {
        node.childNodes = [];
        node.firstChild = null;
        node.lastChild = null;
        let text = '';
        let index = 0;
        while (index < html.length) {
          const ch = html[index];
          if (ch === '<') {
            const tagEnd = html.indexOf('>', index);
            if (tagEnd === -1) break;
            index = tagEnd + 1;
            continue;
          }
          text += ch;
          index += 1;
        }
        text = text
          .replace(/&lt;/g, '<')
          .replace(/&gt;/g, '>')
          .replace(/&quot;/g, '"')
          .replace(/&#0?39;/g, "'")
          .replace(/&amp;/g, '&');
        if (text !== '') node.appendChild(ownerDocument.createTextNode(text));
      },
      get innerHTML(): string {
        return node.childNodes.map((child) => child.textContent).join('');
      },
      get className() {
        return node.getAttribute('class') ?? '';
      },
      set className(value: string) {
        node.setAttribute('class', value);
      },
      get id() {
        return node.getAttribute('id') ?? '';
      },
      set id(value: string) {
        node.setAttribute('id', value);
      },
      appendChild(child: MiniNode) {
        return node.insertBefore(child, null);
      },
      removeChild(child: MiniNode) {
        const index = node.childNodes.indexOf(child);
        if (index < 0) throw new Error('not a child');
        node.childNodes.splice(index, 1);
        child.parentNode = null;
        child.nextSibling = null;
        child.previousSibling = null;
        relink(node);
        return child;
      },
      insertBefore(child: MiniNode, before: MiniNode | null) {
        if (child.parentNode) child.parentNode.removeChild(child);
        if (child.nodeType === 11) {
          const fragmentChildren = [...child.childNodes];
          child.childNodes = [];
          relink(child);
          for (const nested of fragmentChildren) node.insertBefore(nested, before);
          return child;
        }
        const index = before === null ? node.childNodes.length : node.childNodes.indexOf(before);
        node.childNodes.splice(index < 0 ? node.childNodes.length : index, 0, child);
        relink(node);
        return child;
      },
      remove() {
        node.parentNode?.removeChild(node);
      },
      setAttribute(name: string, value: string) {
        node.attributes.set(name, String(value));
      },
      getAttribute(name: string) {
        return node.attributes.has(name) ? node.attributes.get(name)! : null;
      },
      removeAttribute(name: string) {
        node.attributes.delete(name);
      },
      hasAttribute(name: string) {
        return node.attributes.has(name);
      },
      setAttributeNS(_ns: string | null, name: string, value: string) {
        node.setAttribute(name, value);
      },
      getAttributeNS(_ns: string | null, name: string) {
        return node.getAttribute(name);
      },
      addEventListener(type: string, fn: MiniListener, options?: boolean | { capture?: boolean }) {
        const capture = options === true || (typeof options === 'object' && options.capture === true);
        const key = capture ? `${type}-capture` : type;
        const set = node.listeners.get(key) ?? new Set();
        set.add(fn);
        node.listeners.set(key, set);
      },
      removeEventListener(type: string, fn: MiniListener, options?: boolean | { capture?: boolean }) {
        const capture = options === true || (typeof options === 'object' && options.capture === true);
        const key = capture ? `${type}-capture` : type;
        node.listeners.get(key)?.delete(fn);
      },
      dispatchEvent(raw: { type: string; target?: MiniNode; key?: string; preventDefault?: () => void; stopPropagation?: () => void }) {
        const payload: MiniEvent = {
          type: raw.type,
          target: raw.target ?? node,
          currentTarget: null,
          bubbles: true,
          cancelable: true,
          defaultPrevented: false,
          button: 0,
          key: raw.key ?? '',
          preventDefault() { payload.defaultPrevented = true; raw.preventDefault?.(); },
          stopPropagation() { stopped = true; raw.stopPropagation?.(); },
          stopImmediatePropagation() { stopped = true; immediate = true; },
        };
        let stopped = false;
        let immediate = false;
        const path: MiniNode[] = [];
        let current: MiniNode | null = node;
        while (current !== null) {
          path.push(current);
          current = current.parentNode;
        }
        const fire = (entry: MiniNode, key: string) => {
          payload.currentTarget = entry;
          const set = entry.listeners.get(key);
          if (!set) return;
          for (const fn of [...set]) {
            fn(payload);
            if (immediate) return;
          }
        };
        for (let i = path.length - 1; i >= 0; i -= 1) {
          fire(path[i], `${raw.type}-capture`);
          if (stopped) return !payload.defaultPrevented;
        }
        fire(node, raw.type);
        if (stopped) return !payload.defaultPrevented;
        for (let i = 1; i < path.length; i += 1) {
          fire(path[i], raw.type);
          if (stopped) return !payload.defaultPrevented;
        }
        return !payload.defaultPrevented;
      },
      click() {
        node.dispatchEvent({ type: 'click', target: node });
      },
      focus() {},
      blur() {},
      contains(other: MiniNode | null) {
        if (other === node) return true;
        return node.childNodes.some((child) => child.contains(other));
      },
      querySelector(selector: string) {
        return node.querySelectorAll(selector)[0] ?? null;
      },
      querySelectorAll(selector: string) {
        const out: MiniNode[] = [];
        const visit = (current: MiniNode) => {
          if (current.nodeType === 1 && matches(current, selector)) out.push(current);
          for (const child of current.childNodes) visit(child);
        };
        for (const child of node.childNodes) visit(child);
        return out;
      },
      cloneNode(deep = false) {
        const clone = makeNode(node.nodeType, node.nodeName, ownerDocument);
        clone.namespaceURI = node.namespaceURI;
        clone.text = node.text;
        for (const [name, value] of node.attributes) clone.attributes.set(name, value);
        if (deep) {
          for (const child of node.childNodes) clone.appendChild(child.cloneNode(true));
        }
        return clone;
      },
      getBoundingClientRect() {
        // Production measure() reads the content viewport, toolbar, source
        // rows, and selection bar. A stable zero box keeps that call from
        // throwing. It is not layout proof; the anchor proof is the pure
        // helper plus the browser lane.
        return { x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0 };
      },
    } as MiniNode;
    return node;
  };

  const matches = (node: MiniNode, selector: string): boolean => {
    if (selector.includes('.') && !selector.startsWith('.') && !selector.startsWith('[') && !selector.startsWith('#')) {
      const dot = selector.indexOf('.');
      const tag = selector.slice(0, dot);
      const className = selector.slice(dot + 1);
      return (tag === '' || node.tagName === tag.toUpperCase())
        && node.className.split(/\s+/).includes(className);
    }
    if (selector.startsWith('.')) return node.className.split(/\s+/).includes(selector.slice(1));
    if (selector.startsWith('#')) return node.id === selector.slice(1);
    if (selector.startsWith('[') && selector.endsWith(']')) {
      const body = selector.slice(1, -1);
      const eq = body.indexOf('=');
      if (eq < 0) return node.hasAttribute(body);
      const name = body.slice(0, eq);
      const raw = body.slice(eq + 1).replace(/^["']|["']$/g, '');
      return node.getAttribute(name) === raw;
    }
    return node.tagName === selector.toUpperCase();
  };

  const document = makeNode(9, '#document', null) as MiniDocument;
  const ownerDocument = document;
  document.ownerDocument = document;
  const html = makeNode(1, 'HTML', document);
  const head = makeNode(1, 'HEAD', document);
  const body = makeNode(1, 'BODY', document);
  document.appendChild(html);
  html.appendChild(head);
  html.appendChild(body);
  document.documentElement = html;
  document.head = head;
  document.body = body;
  document.activeElement = body;
  document.createElement = (tag: string) => makeNode(1, tag.toUpperCase(), document);
  document.createElementNS = (ns: string, tag: string) => {
    const node = makeNode(1, tag.toUpperCase(), document);
    node.namespaceURI = ns;
    return node;
  };
  document.createTextNode = (text: string) => {
    const node = makeNode(3, '#text', document);
    node.text = text;
    return node;
  };
  document.createComment = (text: string) => {
    const node = makeNode(8, '#comment', document);
    node.text = text;
    return node;
  };
  document.createDocumentFragment = () => makeNode(11, '#document-fragment', document);
  document.getElementById = (id: string) => document.querySelector(`#${id}`);

  const windowObj: MiniWindow = {
    document,
    HTMLElement: FakeCtor,
    HTMLIFrameElement: FakeCtor,
    HTMLInputElement: FakeCtor,
    HTMLButtonElement: FakeCtor,
    HTMLTextAreaElement: FakeCtor,
    SVGElement: FakeCtor,
    Element: FakeCtor,
    Node: FakeCtor,
    Text: FakeCtor,
    Comment: FakeCtor,
    DocumentFragment: FakeCtor,
    setTimeout: (fn: () => void, ms?: number) => globalThis.setTimeout(fn, ms) as unknown as number,
    clearTimeout: (id: number) => globalThis.clearTimeout(id),
    addEventListener() {},
    removeEventListener() {},
    matchMedia: (query: string) => ({
      matches: narrow && query === '(max-width: 720px)',
      addEventListener() {},
      removeEventListener() {},
    }),
    getSelection: () => null,
  };
  document.defaultView = windowObj;
  return { window: windowObj, document };
}

/** Host committed-slot shape (App/Chat): admission syncs in the layout phase
 *  so it precedes every child passive read begin; the committed ref stays
 *  passive and is written only after render. */
function useCommittedFileExplorerSlots(slots: Record<string, FileExplorerSessionSlot>): void {
  const committed = useState<{ current: Record<string, FileExplorerSessionSlot> }>(() => ({ current: {} }))[0];
  useLayoutEffect(() => {
    fileExplorerSyncPageCacheWithSlots(slots);
  }, [slots]);
  useEffect(() => {
    const prev = committed.current;
    if (prev === slots) return;
    committed.current = slots;
  }, [committed, slots]);
}

function openFileSlot(path: string, conversationId?: string): FileExplorerSessionSlot {
  return {
    tabs: [{ path, view: 'file', ...(conversationId ? { conversationId } : {}) }],
    activePath: path,
    mode: 'open',
  };
}

/** Admit the tab that owns this page BEFORE the registry write. Admission
 *  is sticky across tests; a later write re-prunes to it, so seeding first
 *  drops the lines. */
function seedReadCache(sessionId: string, path: string, lines: string[]): void {
  fileExplorerSyncPageCacheWithSlots({ [sessionId]: openFileSlot(path) });
  const request = { sessionId, path, view: 'file' as const, generation: 1 };
  let cache = emptyFileExplorerPageCache();
  cache = fileExplorerBeginCachedPage(cache, request);
  cache = fileExplorerApplyCachedPage(cache, request, { path, lines, truncated: false });
  writeModuleFileExplorerPageCache(cache);
}

let clockNow = 0;
let clockNextId = 1;
let clockPending: Array<{ id: number; at: number; fn: () => void }> = [];

function fakeSetTimeout(fn: () => void, ms?: number): number {
  const id = clockNextId;
  clockNextId += 1;
  clockPending.push({ id, at: clockNow + (ms ?? 0), fn });
  return id;
}

function fakeClearTimeout(id: unknown): void {
  clockPending = clockPending.filter((entry) => entry.id !== id);
}

function advanceClock(ms: number): void {
  const until = clockNow + ms;
  for (;;) {
    clockPending.sort((a, b) => a.at - b.at || a.id - b.id);
    const next = clockPending[0];
    if (next === undefined || next.at > until) break;
    clockPending = clockPending.filter((entry) => entry.id !== next.id);
    clockNow = Math.max(clockNow, next.at);
    next.fn();
  }
  clockNow = until;
}

function DelayDriver({
  tree,
  content,
}: {
  tree: string | null;
  content: string | null;
}) {
  const treeShown = useFileExplorerDelayedLoading(tree);
  const contentShown = useFileExplorerDelayedLoading(content);
  return createElement(
    'div',
    { 'data-testid': 'delay-root' },
    treeShown ? createElement(FileExplorerPaneLoading, { pane: 'tree' }) : null,
    contentShown ? createElement(FileExplorerPaneLoading, { pane: 'content' }) : null,
    createElement('input', { 'data-testid': 'file-explorer-search', 'aria-label': 'Search workspace files' }),
  );
}

let probeTree: string | null = 'tree:1';
let probeContent: string | null = 'content:1';
let setProbe: Dispatch<SetStateAction<{ tree: string | null; content: string | null }>> | null = null;

function DelayProbeHost() {
  const [ids, setIds] = useState({ tree: probeTree, content: probeContent });
  setProbe = setIds;
  return createElement(DelayDriver, ids);
}

async function withDelayHarness(
  run: (doc: MiniDocument, root: Root) => Promise<void>,
  opts?: { matchMediaMatches?: boolean },
): Promise<void> {
  clockNow = 0;
  clockNextId = 1;
  clockPending = [];
  const previousWindow = (globalThis as { window?: unknown }).window;
  const previousDocument = (globalThis as { document?: unknown }).document;
  const previousSetTimeout = globalThis.setTimeout;
  const previousClearTimeout = globalThis.clearTimeout;
  const mini = createMiniDom(opts?.matchMediaMatches === true);
  globalThis.setTimeout = fakeSetTimeout as unknown as typeof setTimeout;
  globalThis.clearTimeout = fakeClearTimeout as typeof clearTimeout;
  mini.window.setTimeout = fakeSetTimeout;
  mini.window.clearTimeout = fakeClearTimeout;
  Object.defineProperty(globalThis, 'window', { configurable: true, writable: true, value: mini.window });
  Object.defineProperty(globalThis, 'document', { configurable: true, writable: true, value: mini.document });
  globalThis.HTMLElement = mini.window.HTMLElement as unknown as typeof HTMLElement;
  globalThis.SVGElement = mini.window.SVGElement as unknown as typeof SVGElement;
  globalThis.Element = mini.window.Element as unknown as typeof Element;
  globalThis.Node = mini.window.Node as unknown as typeof Node;
  globalThis.Text = mini.window.Text as unknown as typeof Text;
  (globalThis as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  const container = mini.document.createElement('div');
  mini.document.body.appendChild(container);
  const root = createRoot(container as unknown as Element);
  try {
    await run(mini.document, root);
  } finally {
    await act(async () => { root.unmount(); });
    globalThis.setTimeout = previousSetTimeout;
    globalThis.clearTimeout = previousClearTimeout;
    if (previousWindow === undefined) delete (globalThis as { window?: unknown }).window;
    else Object.defineProperty(globalThis, 'window', { configurable: true, writable: true, value: previousWindow });
    if (previousDocument === undefined) delete (globalThis as { document?: unknown }).document;
    else Object.defineProperty(globalThis, 'document', { configurable: true, writable: true, value: previousDocument });
  }
}

function loadingOf(doc: MiniDocument, pane: 'tree' | 'content'): MiniNode | null {
  return doc.body.querySelector(`[data-testid="file-explorer-${pane}-loading"]`);
}

{
  check('delay gate is 151ms', FILE_EXPLORER_LOADING_DELAY_MS === 151);
}

await withDelayHarness(async (doc, root) => {
  probeTree = 'tree:1';
  probeContent = 'content:1';
  await act(async () => {
    root.render(createElement(DelayProbeHost));
  });
  check('immediate spinner bug: no tree overlay at t=0', loadingOf(doc, 'tree') === null);
  check('immediate spinner bug: no content overlay at t=0', loadingOf(doc, 'content') === null);
  check('empty-div layout gap: search stays without a loading placeholder', doc.body.querySelector('[data-testid="file-explorer-search"]') != null && doc.body.querySelector('.file-explorer__loading') === null);
  await act(async () => { advanceClock(150); });
  check('fast path: still hidden at 150ms', loadingOf(doc, 'tree') === null && loadingOf(doc, 'content') === null);
  await act(async () => { advanceClock(1); });
  const tree = loadingOf(doc, 'tree');
  const content = loadingOf(doc, 'content');
  check('slow path: tree shimmer at 151ms', tree != null && tree.querySelector('.file-explorer__pane-shimmer') != null && (tree.textContent ?? '') === '');
  check('slow path: content shimmer at 151ms', content != null && content.querySelector('.file-explorer__pane-shimmer') != null && (content.textContent ?? '') === '');
});

await withDelayHarness(async (doc, root) => {
  probeTree = 'tree:1';
  probeContent = 'content:1';
  await act(async () => {
    root.render(createElement(DelayProbeHost));
  });
  await act(async () => { advanceClock(100); });
  await act(async () => {
    setProbe?.({ tree: 'tree:2', content: 'content:1' });
  });
  await act(async () => { advanceClock(51); });
  check('cross-pane timing bleed: content still appears at original 151ms', loadingOf(doc, 'content') != null);
  check('stale identity visibility: replaced tree is still hidden at original 151ms', loadingOf(doc, 'tree') === null);
  await act(async () => { advanceClock(100); });
  check('stale identity visibility: new tree identity appears 151ms after replacement', loadingOf(doc, 'tree') != null);
  check('cross-pane timing bleed: content overlay survives tree identity replacement', loadingOf(doc, 'content') != null);
});

await withDelayHarness(async (doc, root) => {
  probeTree = 'tree:1';
  probeContent = 'content:1';
  await act(async () => {
    root.render(createElement(DelayProbeHost));
  });
  await act(async () => { advanceClock(151); });
  check('both panes visible before content identity replacement', loadingOf(doc, 'tree') != null && loadingOf(doc, 'content') != null);
  await act(async () => {
    setProbe?.({ tree: 'tree:1', content: 'content:2' });
  });
  check('cross-pane: content hide does not hide tree', loadingOf(doc, 'tree') != null);
  check('stale identity visibility: old content overlay is gone', loadingOf(doc, 'content') === null);
  await act(async () => { advanceClock(150); });
  check('replaced content still hidden at 150ms', loadingOf(doc, 'content') === null);
  await act(async () => { advanceClock(1); });
  check('replaced content appears at 151ms on its own clock', loadingOf(doc, 'content') != null && loadingOf(doc, 'tree') != null);
});

await withDelayHarness(async (doc, root) => {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  fileExplorerSyncPageCacheWithSlots({
    'sess-delay-panel': { tabs: [{ path: 'src/a.ts', view: 'file' }], activePath: 'src/a.ts', mode: 'open' },
  });
  const parked = () => new Promise<unknown>(() => {});
  await act(async () => {
    root.render(createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand: parked,
      sessionId: 'sess-delay-panel',
      slot: { tabs: [{ path: 'src/a.ts', view: 'file' }], activePath: 'src/a.ts', mode: 'open' },
    }));
  });
  const tree = doc.body.querySelector('[data-testid="file-explorer-tree"]');
  check('panel empty-div: search stays in the tree toolbar while requests are in flight', tree != null && tree.querySelector('[data-testid="file-explorer-search"]') != null && tree.querySelector('.file-explorer__tree-list')?.querySelector('[data-testid="file-explorer-search"]') == null);
  check('panel empty-div: no file-explorer__loading node', doc.body.querySelector('.file-explorer__loading') === null);
  check('panel immediate spinner: no spinner and no overlay at t=0', doc.body.querySelector('.file-explorer__spinner') === null && loadingOf(doc, 'tree') === null && loadingOf(doc, 'content') === null);
  check('tree toggle and close survive delayed loading', doc.body.querySelector(`#${FILE_EXPLORER_TREE_TOGGLE_ID}`) != null && doc.body.querySelector(`#${FILE_EXPLORER_CLOSE_BTN_ID}`) != null);
  await act(async () => { advanceClock(150); });
  check('panel fast path: still no overlays at 150ms', loadingOf(doc, 'tree') === null && loadingOf(doc, 'content') === null);
  await act(async () => { advanceClock(1); });
  check('panel slow path: tree overlay at 151ms', loadingOf(doc, 'tree') != null);
  check('panel slow path: content overlay at 151ms', loadingOf(doc, 'content') != null);
});

await withDelayHarness(async (doc, root) => {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  fileExplorerSyncPageCacheWithSlots({
    'sess-refresh-panel': { tabs: [{ path: 'src/a.ts', view: 'file' }], activePath: 'src/a.ts', mode: 'open' },
  });
  updateFileExplorerDirCache((prev) => fileExplorerWriteDirState(prev, 'sess-refresh-panel', '', () => ({
    entries: [{ name: 'a.ts', path: 'src/a.ts', kind: 'file' }],
    truncated: false,
    loading: false,
    error: null,
    transportIncomplete: false,
    generation: 1,
  })));
  const request = { sessionId: 'sess-refresh-panel', path: 'src/a.ts', view: 'file' as const, generation: 1 };
  let cache = emptyFileExplorerPageCache();
  cache = fileExplorerBeginCachedPage(cache, request);
  cache = fileExplorerApplyCachedPage(cache, request, {
    path: 'src/a.ts',
    lines: ['const x = 1;'],
    truncated: false,
  });
  writeModuleFileExplorerPageCache(cache);
  await act(async () => {
    root.render(createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand: () => new Promise(() => {}),
      sessionId: 'sess-refresh-panel',
      slot: { tabs: [{ path: 'src/a.ts', view: 'file' }], activePath: 'src/a.ts', mode: 'open' },
    }));
  });
  const refreshBtn = doc.body.querySelector(`#${FILE_EXPLORER_REFRESH_BTN_ID}`);
  const glyph = refreshBtn?.querySelector('.file-explorer__refresh-glyph');
  check('refresh stays icon-only while painted panes reload', refreshBtn?.querySelector('.file-explorer__refresh-label') == null && (refreshBtn?.textContent ?? '').replace(glyph?.textContent ?? '', '').trim() === '');
  check('refresh never renders Refreshing prose', !(doc.body.textContent ?? '').includes('Refreshing'));
  await act(async () => { advanceClock(151); });
  check('refresh has no pane-level indicator at 151ms', loadingOf(doc, 'tree') === null && loadingOf(doc, 'content') === null);
  check(
    'only the Refresh button is marked busy',
    refreshBtn != null
      && refreshBtn.className.split(/\s+/).includes('is-refreshing')
      && refreshBtn.getAttribute('aria-busy') === 'true'
      && refreshBtn.getAttribute('aria-label') === 'Refresh'
      && refreshBtn.getAttribute('title') === 'Refresh',
  );
  check(
    'busy Refresh keeps an aria-hidden glyph without a text label',
    glyph != null
      && glyph.getAttribute('aria-hidden') === 'true'
      && refreshBtn?.querySelector('.file-explorer__refresh-label') == null,
  );
});

interface ExplorerExclusiveState {
  layout: string | null;
  collapsedClass: boolean;
  pressed: boolean | null;
  label: string;
  treeHidden: boolean;
  contentHidden: boolean;
  searchMounted: boolean;
}

function explorerExclusiveState(doc: MiniDocument): ExplorerExclusiveState {
  const body = doc.body.querySelector('[data-testid="file-explorer-layout"]');
  const tree = doc.body.querySelector('[data-testid="file-explorer-tree"]');
  const content = doc.body.querySelector('[data-testid="file-explorer-content"]');
  const toggle = doc.body.querySelector(`#${FILE_EXPLORER_TREE_TOGGLE_ID}`);
  const pressedRaw = toggle?.getAttribute('aria-pressed');
  return {
    layout: body?.getAttribute('data-layout') ?? null,
    collapsedClass: (body?.className ?? '').split(/\s+/).includes('file-explorer__body--tree-collapsed'),
    pressed: pressedRaw == null ? null : pressedRaw === 'true',
    label: toggle?.getAttribute('aria-label') ?? '',
    treeHidden: tree?.hasAttribute('hidden') === true,
    contentHidden: content?.hasAttribute('hidden') === true,
    searchMounted: doc.body.querySelector('[data-testid="file-explorer-search"]') != null,
  };
}

function foldSynced(state: ExplorerExclusiveState, collapsed: boolean): boolean {
  return state.collapsedClass === collapsed
    && state.pressed === collapsed
    && state.treeHidden === collapsed
    && state.contentHidden === !collapsed
    && (collapsed ? state.label === 'Expand file tree' : state.label === 'Collapse file tree');
}

await withDelayHarness(async (doc, root) => {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  fileExplorerSyncPageCacheWithSlots({
    'sess-narrow-exclusive': emptyFileExplorerSlot(),
  });
  updateFileExplorerDirCache((prev) => fileExplorerWriteDirState(prev, 'sess-narrow-exclusive', '', () => ({
    entries: [{ name: 'a.ts', path: 'src/a.ts', kind: 'file' }],
    truncated: false,
    loading: false,
    error: null,
    transportIncomplete: false,
    generation: 1,
  })));
  function NarrowExclusiveHost() {
    const [slot, setSlot] = useState(emptyFileExplorerSlot());
    return createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand: () => new Promise(() => {}),
      sessionId: 'sess-narrow-exclusive',
      slot,
      onOpenPath: (path: string, _line?: number, view: 'file' | 'diff' = 'file') => {
        setSlot({ tabs: [{ path, view }], activePath: path, mode: 'open' });
      },
    });
  }
  await act(async () => {
    root.render(createElement(NarrowExclusiveHost));
  });
  const opened = explorerExclusiveState(doc);
  check('phone exclusive: open tree fills the body', opened.layout === 'tree' && opened.treeHidden === false && opened.contentHidden === true);
  check('phone exclusive: open tree fold class XOR matches hidden', foldSynced(opened, false));
  const fileRow = doc.body.querySelector('.file-explorer__row--typescript');
  check('phone exclusive: file row is present', fileRow != null);
  await act(async () => { fileRow?.click(); });
  const afterOpen = explorerExclusiveState(doc);
  check('phone exclusive: opening a file is content and hides the tree', afterOpen.layout === 'content' && afterOpen.treeHidden === true && afterOpen.contentHidden === false);
  check('phone exclusive: opening a file sets --tree-collapsed with matching hidden', foldSynced(afterOpen, true));
  const toggle = doc.body.querySelector(`#${FILE_EXPLORER_TREE_TOGGLE_ID}`);
  check('phone exclusive: collapsed toggle names Expand and hides search with the tree', toggle != null && toggle.getAttribute('aria-label') === 'Expand file tree' && doc.body.querySelector('[data-testid="file-explorer-tree"]')?.hasAttribute('hidden') === true && doc.body.querySelector('[data-testid="file-explorer-search"]') != null && (toggle.textContent ?? '').includes('Files'));
  const beforeExpand = explorerExclusiveState(doc);
  await act(async () => { toggle?.click(); });
  const afterExpand = explorerExclusiveState(doc);
  check('phone exclusive: expand is tree and hides content', afterExpand.layout === 'tree' && afterExpand.treeHidden === false && afterExpand.contentHidden === true);
  check('phone exclusive: expand clears --tree-collapsed with matching hidden', foldSynced(afterExpand, false));
  check(
    'phone exclusive: expand shows search with the tree and is not label-only',
    afterExpand.label === 'Collapse file tree'
      && afterExpand.searchMounted === true
      && afterExpand.treeHidden === false
      && beforeExpand.searchMounted === true
      && beforeExpand.treeHidden === true
      && afterExpand.collapsedClass === false
      && afterExpand.pressed === false,
  );
  await act(async () => { doc.body.querySelector(`#${FILE_EXPLORER_TREE_TOGGLE_ID}`)?.click(); });
  const afterCollapse = explorerExclusiveState(doc);
  check('phone exclusive: collapse is content and hides the tree', afterCollapse.layout === 'content' && afterCollapse.treeHidden === true && afterCollapse.contentHidden === false);
  check('phone exclusive: collapse sets --tree-collapsed with matching hidden', foldSynced(afterCollapse, true));
  check(
    'phone exclusive: collapse hides search with the tree',
    afterCollapse.searchMounted === true
      && afterCollapse.treeHidden === true
      && afterCollapse.label === 'Expand file tree'
      && afterCollapse.collapsedClass === true
      && afterCollapse.pressed === true,
  );
}, { matchMediaMatches: true });

await withDelayHarness(async (doc, root) => {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  fileExplorerSyncPageCacheWithSlots({
    'sess-exclusive-panes': emptyFileExplorerSlot(),
  });
  updateFileExplorerDirCache((prev) => fileExplorerWriteDirState(prev, 'sess-exclusive-panes', '', () => ({
    entries: [{ name: 'a.ts', path: 'src/a.ts', kind: 'file' }],
    truncated: false,
    loading: false,
    error: null,
    transportIncomplete: false,
    generation: 1,
  })));
  function ExclusivePanesHost() {
    const [slot, setSlot] = useState(emptyFileExplorerSlot());
    return createElement(FileExplorerPanel, {
      ...panelProps,
      exclusivePanes: true,
      sendCommand: () => new Promise(() => {}),
      sessionId: 'sess-exclusive-panes',
      slot,
      onOpenPath: (path: string, _line?: number, view: 'file' | 'diff' = 'file') => {
        setSlot({ tabs: [{ path, view }], activePath: path, mode: 'open' });
      },
    });
  }
  await act(async () => {
    root.render(createElement(ExclusivePanesHost));
  });
  const opened = explorerExclusiveState(doc);
  check('exclusive panes desktop: open tree fills the body', opened.layout === 'tree' && opened.treeHidden === false && opened.contentHidden === true);
  check('exclusive panes desktop: open tree fold class XOR matches hidden', foldSynced(opened, false));
  check('exclusive panes desktop: divider is hidden on the tree pane', doc.body.querySelector('[data-testid="file-explorer-divider"]')?.hasAttribute('hidden') === true);
  const fileRow = doc.body.querySelector('.file-explorer__row--typescript');
  check('exclusive panes desktop: file row is present', fileRow != null);
  await act(async () => { fileRow?.click(); });
  const afterOpen = explorerExclusiveState(doc);
  check('exclusive panes desktop: opening a file is content and hides the tree', afterOpen.layout === 'content' && afterOpen.treeHidden === true && afterOpen.contentHidden === false);
  check('exclusive panes desktop: opening a file sets --tree-collapsed with matching hidden', foldSynced(afterOpen, true));
  check('exclusive panes desktop: divider stays hidden after open', doc.body.querySelector('[data-testid="file-explorer-divider"]')?.hasAttribute('hidden') === true);
  const toggle = doc.body.querySelector(`#${FILE_EXPLORER_TREE_TOGGLE_ID}`);
  check('exclusive panes: collapsed toggle names Expand, shows Files, and keeps search inside the hidden tree', toggle != null && toggle.getAttribute('aria-label') === 'Expand file tree' && doc.body.querySelector('[data-testid="file-explorer-tree"]')?.hasAttribute('hidden') === true && doc.body.querySelector('[data-testid="file-explorer-search"]') != null && (toggle.textContent ?? '').includes('Files'));
  await act(async () => { toggle?.click(); });
  const afterExpand = explorerExclusiveState(doc);
  check('exclusive panes desktop: expand is tree and hides content', afterExpand.layout === 'tree' && afterExpand.treeHidden === false && afterExpand.contentHidden === true && afterExpand.searchMounted === true);
  check('exclusive panes desktop: expand clears --tree-collapsed with matching hidden', foldSynced(afterExpand, false));
  check('exclusive panes desktop: divider stays hidden on the expanded tree', doc.body.querySelector('[data-testid="file-explorer-divider"]')?.hasAttribute('hidden') === true);
}, { matchMediaMatches: false });

await withDelayHarness(async (doc, root) => {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  fileExplorerSyncPageCacheWithSlots({
    'sess-delete-error': emptyFileExplorerSlot(),
  });
  updateFileExplorerDirCache((prev) => fileExplorerWriteDirState(prev, 'sess-delete-error', '', () => ({
    entries: [{ name: 'a.ts', path: 'src/a.ts', kind: 'file' }],
    truncated: false,
    loading: false,
    error: null,
    transportIncomplete: false,
    generation: 1,
  })));
  const sendCommand = (command: Record<string, unknown>) => {
    if (command.type === 'file_explorer_delete') {
      return Promise.reject(new Error('path is unavailable'));
    }
    return new Promise(() => {});
  };
  await act(async () => {
    root.render(createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand,
      sessionId: 'sess-delete-error',
      slot: emptyFileExplorerSlot(),
    }));
  });
  const fileRow = doc.body.querySelector('.file-explorer__row--typescript');
  check('delete business error: file row is present before delete', fileRow != null);
  await act(async () => {
    fileRow?.dispatchEvent({ type: 'contextmenu' });
  });
  const errorRow = [...doc.body.querySelectorAll('.file-explorer__error')]
    .find((node) => (node.textContent ?? '').includes('path is unavailable'));
  check('delete business error: inline error names path is unavailable', errorRow != null);
  check('delete business error: file row remains after failed delete', doc.body.querySelector('.file-explorer__row--typescript') != null);
});

await withDelayHarness(async (doc, root) => {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  fileExplorerSyncPageCacheWithSlots({
    'sess-delete-transport': emptyFileExplorerSlot(),
  });
  updateFileExplorerDirCache((prev) => fileExplorerWriteDirState(prev, 'sess-delete-transport', '', () => ({
    entries: [{ name: 'a.ts', path: 'src/a.ts', kind: 'file' }],
    truncated: false,
    loading: false,
    error: null,
    transportIncomplete: false,
    generation: 1,
  })));
  const sendCommand = (command: Record<string, unknown>) => {
    if (command.type === 'file_explorer_delete') {
      return Promise.reject(Object.assign(new Error('not connected'), { transport: true }));
    }
    return new Promise(() => {});
  };
  await act(async () => {
    root.render(createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand,
      sessionId: 'sess-delete-transport',
      slot: emptyFileExplorerSlot(),
    }));
  });
  const fileRow = doc.body.querySelector('.file-explorer__row--typescript');
  check('delete transport error: file row is present before delete', fileRow != null);
  await act(async () => {
    fileRow?.dispatchEvent({ type: 'contextmenu' });
  });
  check('delete transport error: file row remains', doc.body.querySelector('.file-explorer__row--typescript') != null);
});

await withDelayHarness(async (doc, root) => {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  fileExplorerSyncPageCacheWithSlots({ 'sess-dir-errors': emptyFileExplorerSlot() });
  updateFileExplorerViewCache('sess-dir-errors', (view) => ({
    ...view,
    expandedDirs: { '': true, gone: true, broken: true, stale: true },
  }));
  const requested: string[] = [];
  const rootEntries = [
    { name: 'gone', path: 'gone', kind: 'dir' },
    { name: 'broken', path: 'broken', kind: 'dir' },
    { name: 'stale', path: 'stale', kind: 'dir' },
    { name: 'svc', path: 'svc', kind: 'dir' },
  ];
  const sendCommand = (command: Record<string, unknown>) => {
    if (command.type === 'file_explorer_changes') {
      return Promise.resolve({ files: [], isRepo: true, truncated: false, repositories: [{ path: 'svc', head: 'a1b2c3d' }] });
    }
    if (command.type !== 'file_explorer_list') return Promise.resolve({});
    const path = typeof command.path === 'string' ? command.path : '';
    requested.push(path);
    if (path === '') return Promise.resolve({ entries: rootEntries, truncated: false });
    if (path === 'gone') {
      return Promise.reject(Object.assign(new Error('directory is unavailable'), { rpcCode: 'directory_unavailable' }));
    }
    if (path === 'broken') return Promise.reject(new Error('list failed'));
    if (path === 'stale') return Promise.reject(new Error('not connected'));
    return Promise.resolve({ entries: [], truncated: false });
  };
  updateFileExplorerDirCache((prev) => {
    let next = fileExplorerWriteDirState(prev, 'sess-dir-errors', '', () => ({
      entries: [
        { name: 'gone', path: 'gone', kind: 'dir' as const },
        { name: 'broken', path: 'broken', kind: 'dir' as const },
        { name: 'stale', path: 'stale', kind: 'dir' as const },
      ],
      truncated: false,
      loading: false,
      error: null,
      transportIncomplete: false,
      generation: 1,
    }));
    next = fileExplorerWriteDirState(next, 'sess-dir-errors', 'stale', () => ({
      entries: [{ name: 'keep.ts', path: 'stale/keep.ts', kind: 'file' as const }],
      truncated: false,
      loading: false,
      error: null,
      transportIncomplete: false,
      generation: 1,
    }));
    return next;
  });
  await act(async () => {
    root.render(createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand,
      sessionId: 'sess-dir-errors',
      slot: emptyFileExplorerSlot(),
    }));
  });
  await act(async () => {});
  const dirNames = [...doc.body.querySelectorAll('.file-explorer__row--dir')]
    .map((node) => (node.textContent ?? '').trim());
  check('typed unavailable directory row is removed', !dirNames.some((name) => name.includes('gone')));
  check('typed unavailable child was actually listed', requested.includes('gone'));
  check('generic list error keeps its directory row', dirNames.some((name) => name.includes('broken')));
  check('generic child was actually listed', requested.includes('broken'));
  check('transport failure keeps its directory row', dirNames.some((name) => name.includes('stale')));
  check('transport child was actually listed', requested.includes('stale'));
  const errors = [...doc.body.querySelectorAll('.file-explorer__error')].map((node) => node.textContent ?? '');
  check('typed unavailable directory has no error or Retry', !errors.some((text) => text.includes('directory is unavailable')));
  check('generic list error stays visible with Retry', errors.some((text) => text.includes('list failed') && text.includes('Retry')));
  check('transport failure keeps Connection lost and Retry', errors.some((text) => text.includes('Connection lost') && text.includes('Retry')));
  check('transport failure keeps the last listed file', doc.body.textContent?.includes('keep.ts') === true);
  const hashBadge = doc.body.querySelector('[data-testid="file-explorer-repo-hash"]');
  check('nested repo dir row shows its HEAD short hash', hashBadge !== null && hashBadge.textContent === 'a1b2c3d');
});
// ---------------------------------------------------------------------
// Ask (file conversation): visible toggle, pure selection parser,
// lazy open→prompt→draft lifecycle, bounded admitted-owner polling.
// ---------------------------------------------------------------------


function wireEntry(id: string, role: 'user' | 'assistant', text: string, partial = false): Record<string, unknown> {
  return {
    id,
    kind: role,
    role,
    text,
    toolName: null,
    toolArgs: null,
    output: null,
    thinking: null,
    isError: false,
    isPartial: partial,
  };
}

function snapshotFor(conversationId: string, extra: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    conversationId,
    path: 'src/a.ts',
    streaming: false,
    status: 'ready',
    entries: [],
    ...extra,
  };
}

function promptAck(conversationId: unknown, extra: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    conversationId,
    path: 'src/a.ts',
    streaming: false,
    status: 'ready',
    entries: [wireEntry('e1', 'user', 'what is this?')],
    accepted: true,
    ...extra,
  };
}

async function flushMicrotasks(): Promise<void> {
  await act(async () => {
    for (let round = 0; round < 5; round += 1) await Promise.resolve();
  });
}

{
  const html = renderExplorer();
  check('ask: no dock without an active tab', !html.includes('file-explorer-ask-split') && !html.includes('file-explorer-ask-toggle'));
}

{
  const askSourceSlot = openFileSlot('src/app.rs');
  fileExplorerSyncPageCacheWithSlots({ 'sess-ask-ssr': askSourceSlot });
  updateFileExplorerViewCache('sess-ask-ssr', () => emptyFileExplorerViewState());
  seedReadCache('sess-ask-ssr', 'src/app.rs', ['fn main() {}']);
  const html = renderToStaticMarkup(createElement(FileExplorerPanel, {
    ...panelProps,
    sessionId: 'sess-ask-ssr',
    slot: askSourceSlot,
  }));
  const content = html.slice(html.indexOf('data-testid="file-explorer-content"'));
  const head = headerSlice(html);
  check('ask: open file has no Ask button', !content.includes('file-explorer-ask-toggle') && !content.includes('aria-label="Ask about this file"') && !head.includes('file-explorer-ask-toggle'));
  check('ask: non-previewable tab shows no surface buttons', !content.includes('>Source<') && !content.includes('>Preview<'));
  check('ask: dock is present beside source without a toggle', content.includes('data-testid="file-explorer-ask-split"') && content.includes('class="file-conversation"') && content.includes('file-explorer__code'));
  check('ask: source rows carry actual data-line numbers', html.includes('data-line="1"'));
}
{
  const askMarkdownSlot = openFileSlot('notes/a.md');
  fileExplorerSyncPageCacheWithSlots({ 'sess-ask-md': askMarkdownSlot });
  updateFileExplorerViewCache('sess-ask-md', () => emptyFileExplorerViewState());
  seedReadCache('sess-ask-md', 'notes/a.md', ['# hi']);
  const html = renderToStaticMarkup(createElement(FileExplorerPanel, {
    ...panelProps,
    sessionId: 'sess-ask-md',
    slot: askMarkdownSlot,
  }));
  const content = html.slice(html.indexOf('data-testid="file-explorer-content"'));
  const previewAt = content.indexOf('>Preview<');
  check('ask: previewable toolbar offers Source and Preview', previewAt >= 0 && content.includes('>Source<') && !content.includes('file-explorer-ask-toggle'));
  check('ask: previewable file still docks the conversation', content.includes('data-testid="file-explorer-ask-split"'));
}

{
  const askOnSlot = openFileSlot('src/app.rs', 'conv-static');
  fileExplorerSyncPageCacheWithSlots({ 'sess-ask-on': askOnSlot });
  updateFileExplorerViewCache('sess-ask-on', () => fileExplorerWithPathSurface(emptyFileExplorerViewState(), 'src/app.rs', 'ask'));
  seedReadCache('sess-ask-on', 'src/app.rs', ['fn main() {}']);
  const html = renderToStaticMarkup(createElement(FileExplorerPanel, {
    ...panelProps,
    sessionId: 'sess-ask-on',
    slot: askOnSlot,
  }));
  check('ask: stored ask surface still shows source beside the dock', !html.includes('>Source<') && !html.includes('file-explorer-ask-toggle'));
  check('ask: dock mounts beside the content column', html.includes('data-testid="file-explorer-ask-split"')
    && html.includes('class="file-conversation"')
    && html.indexOf('data-testid="file-explorer-content"') < html.indexOf('data-testid="file-explorer-ask-split"')
    && html.includes('file-explorer__code'));
  check('ask: placeholder snapshot binds the tab conversation id', html.includes('data-conversation-id="conv-static"') && html.includes('data-session-id="sess-ask-on"'));
}

{
  updateFileExplorerViewCache('sess-ask-diff', () => emptyFileExplorerViewState());
  const html = renderToStaticMarkup(createElement(FileExplorerPanel, {
    ...panelProps,
    sessionId: 'sess-ask-diff',
    slot: { tabs: [{ path: 'src/app.rs', view: 'diff' }], activePath: 'src/app.rs', mode: 'open' },
  }));
  const focusTag = /<button[^>]*id="file-explorer-focus-btn"[^>]*>/.exec(html)?.[0] ?? '';
  check('ask: diff view docks Ask and enables the fullscreen toggle',
    html.includes('data-testid="file-explorer-ask-split"')
      && html.includes('class="file-conversation"')
      && focusTag.includes('aria-pressed="false"')
      && focusTag.includes('aria-label="Show conversation fullscreen"')
      && !focusTag.includes('disabled'));
}

{
  check('selection: ordered range', JSON.stringify(fileExplorerSelectionRange(7, 3)) === '{"startLine":3,"endLine":7}');
  check('selection: same endpoint is a one-line range', JSON.stringify(fileExplorerSelectionRange(4, 4)) === '{"startLine":4,"endLine":4}');
  check('selection: missing endpoint yields no range', fileExplorerSelectionRange(null, 2) === null && fileExplorerSelectionRange(2, null) === null);
  check('selection: non-positive or fractional endpoints yield no range', fileExplorerSelectionRange(0, 2) === null && fileExplorerSelectionRange(2.5, 4) === null);
  const lineNode = {
    hasAttribute: (name: string) => name === 'data-line',
    getAttribute: (name: string) => (name === 'data-line' ? '4' : null),
    parentNode: null as unknown,
  };
  const textNode = { parentNode: lineNode as unknown };
  check('selection: data-line resolves through the nearest ancestor', fileExplorerLineDataOfNode(textNode) === 4 && fileExplorerLineDataOfNode(lineNode) === 4);
  check('selection: unnumbered nodes resolve to null', fileExplorerLineDataOfNode(null) === null && fileExplorerLineDataOfNode({ parentNode: null }) === null);
  const malformedLine = { hasAttribute: () => true, getAttribute: () => 'x', parentNode: null };
  check('selection: malformed data-line resolves to null', fileExplorerLineDataOfNode(malformedLine) === null);
  check('selection: endpoint pair maps to the prompt range', JSON.stringify(fileExplorerSelectionFromSelection({ anchorNode: lineNode, focusNode: textNode })) === '{"startLine":4,"endLine":4}');
  check('selection: endpoint outside numbered rows yields no range', fileExplorerSelectionFromSelection({ anchorNode: lineNode, focusNode: null }) === null);
}
{
  const edge = FILE_EXPLORER_SELECTION_EDGE_PX;
  check('anchor edge is 8px', edge === 8);
  const viewport = { top: 0, left: 0, width: 400, height: 300 };
  const bar = { width: 180, height: 32 };
  const selection = { top: 40, left: 16, width: 220, height: 48 };
  const below = fileExplorerSelectionAnchor({ viewport, selection, bar });
  const selectionBottom = selection.top + selection.height;
  check(
    'anchor places the bar below a selection that leaves room',
    below.visible === true
      && below.placement === 'below'
      && below.top === selectionBottom + edge
      && below.left === selection.left
      && below.top >= selectionBottom + edge
      && below.left >= edge
      && below.left + bar.width <= viewport.width - edge
      && below.top + bar.height <= viewport.height - edge,
  );
  const low = { top: 250, left: 16, width: 220, height: 40 };
  const above = fileExplorerSelectionAnchor({ viewport, selection: low, bar });
  check(
    'anchor flips above when below would leave the viewport',
    above.visible === true
      && above.placement === 'above'
      && above.top + bar.height <= low.top
      && above.top >= edge,
  );
  const wide = fileExplorerSelectionAnchor({
    viewport,
    selection: { top: 40, left: 360, width: 30, height: 20 },
    bar,
  });
  check(
    'anchor clamps the bar inside the 8px viewport inset',
    wide.visible === true
      && wide.left >= edge
      && wide.left + bar.width <= viewport.width - edge
      && wide.top >= edge
      && wide.top + bar.height <= viewport.height - edge,
  );
  const offscreen = fileExplorerSelectionAnchor({
    viewport,
    selection: { top: 320, left: 16, width: 40, height: 20 },
    bar,
  });
  const missing = fileExplorerSelectionAnchor({ viewport, selection: null, bar });
  check('anchor hides a selection that misses the viewport', offscreen.visible === false);
  check('anchor hides a missing selection', missing.visible === false);
  check('anchor without a popup input omits the popup box', below.popup === undefined);
  const popupBelow = fileExplorerSelectionAnchor({
    viewport,
    selection,
    bar,
    popup: { width: 200, height: 100 },
  });
  const barBottom = below.top + bar.height;
  check(
    'popup anchored below the bar when the space below fits it',
    popupBelow.placement === 'below'
      && popupBelow.popup != null
      && popupBelow.popup.top === barBottom + edge
      && popupBelow.popup.left === below.left
      && popupBelow.popup.maxWidth === viewport.width - 2 * edge
      && popupBelow.popup.maxHeight === viewport.height - edge - (barBottom + edge),
  );
  const popupAbove = fileExplorerSelectionAnchor({
    viewport,
    selection: low,
    bar,
    popup: { width: 200, height: 100 },
  });
  check(
    'popup flips above the bar when only the space above fits it',
    popupAbove.placement === 'above'
      && popupAbove.popup != null
      && popupAbove.popup.top === above.top - edge - 100
      && popupAbove.popup.top >= edge
      && popupAbove.popup.left === above.left
      && popupAbove.popup.maxHeight === above.top - edge - (viewport.top + edge),
  );
  const popupCapped = fileExplorerSelectionAnchor({
    viewport,
    selection,
    bar,
    popup: { width: 200, height: 500 },
  });
  check(
    'popup caps its height at the larger direction space',
    popupCapped.popup != null
      && popupCapped.popup.maxHeight === viewport.height - edge - (barBottom + edge)
      && popupCapped.popup.maxHeight < 500
      && popupCapped.popup.top === barBottom + edge
      && popupCapped.popup.top + popupCapped.popup.maxHeight === viewport.height - edge
      && popupCapped.popup.maxWidth === viewport.width - 2 * edge,
  );
  const tiny = fileExplorerSelectionAnchor({
    viewport: { top: 0, left: 0, width: 200, height: 20 },
    selection: { top: 5, left: 10, width: 50, height: 8 },
    bar: { width: 120, height: 10 },
    popup: { width: 80, height: 5 },
  });
  check('popup is omitted when no direction has an edge of room', tiny.visible === true && tiny.popup === undefined);
  const hiddenWithPopup = fileExplorerSelectionAnchor({
    viewport,
    selection: { top: 320, left: 16, width: 40, height: 20 },
    bar,
    popup: { width: 200, height: 100 },
  });
  check('invisible selection never carries a popup', hiddenWithPopup.visible === false && hiddenWithPopup.popup === undefined);
  const popupClampedTop = fileExplorerSelectionAnchor({
    viewport,
    selection: low,
    bar,
    popup: { width: 200, height: 300 },
  });
  check(
    'clamped popup top never overlaps the bar above',
    popupClampedTop.popup != null
      && popupClampedTop.popup.top === viewport.top + edge
      && popupClampedTop.popup.top + Math.min(300, popupClampedTop.popup.maxHeight) <= popupClampedTop.top,
  );
  const widePopup = fileExplorerSelectionAnchor({
    viewport,
    selection,
    bar,
    popup: { width: 500, height: 100 },
  });
  check(
    'popup wider than maxWidth clamps left of the bar instead of clipping right',
    widePopup.popup != null
      && widePopup.popup.left === viewport.left + edge
      && widePopup.popup.left < widePopup.left
      && widePopup.popup.maxWidth === viewport.width - 2 * edge,
  );
  const blameBar = { width: 220, height: 52 };
  const blameSelectionTop = 80;
  const blameSelectionHeight = 72;
  const blameViewport = { top: 44, left: 0, width: 390, height: 280 };
  const blameOverlap = fileExplorerSelectionAnchor({
    viewport: blameViewport,
    selection: { top: blameSelectionTop, left: 16, width: 360, height: blameSelectionHeight },
    bar: blameBar,
    popup: { width: 360, height: 160 },
  });
  const blameBarTop = blameSelectionTop + blameSelectionHeight + edge;
  const blamePopupTop = blameBarTop + blameBar.height + edge;
  const blamePopupMax = blameViewport.top + blameViewport.height - edge - blamePopupTop;
  const blameSelectionBottom = blameSelectionTop + blameSelectionHeight;
  const blamePopupHeight = Math.min(160, blameOverlap.popup?.maxHeight ?? 0);
  check(
    'open blame popup stays below the selection inside the viewport inset',
    blameOverlap.placement === 'below'
      && blameOverlap.top === blameBarTop
      && blameOverlap.popup != null
      && blameOverlap.popup.top === blamePopupTop
      && blameOverlap.popup.maxHeight === blamePopupMax
      && blameOverlap.popup.maxHeight < 160
      && (blameOverlap.top >= blameSelectionBottom || blameOverlap.top + blameBar.height <= blameSelectionTop)
      && (blameOverlap.popup.top >= blameSelectionBottom || blameOverlap.popup.top + blamePopupHeight <= blameSelectionTop)
      && blameOverlap.popup.top + blameOverlap.popup.maxHeight <= blameViewport.top + blameViewport.height - edge,
  );



}

await withDelayHarness(async (doc, root) => {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  fileExplorerSyncPageCacheWithSlots({ 'sess-ask-send': emptyFileExplorerSlot() });
  updateFileExplorerViewCache('sess-ask-send', () => emptyFileExplorerViewState());
  updateFileExplorerDirCache((prev) => fileExplorerWriteDirState(prev, 'sess-ask-send', '', () => ({
    entries: [{ name: 'a.ts', path: 'src/a.ts', kind: 'file' }],
    truncated: false,
    loading: false,
    error: null,
    transportIncomplete: false,
    generation: 1,
  })));

  const commands: Array<Record<string, unknown>> = [];
  const errors: string[] = [];
  let latestSlot: FileExplorerSessionSlot | null = null;
  let resolveOpen: (() => void) | null = null;
  const send = (command: Record<string, unknown>) => {
    commands.push(command);
    if (command.type === 'file_read') return new Promise<unknown>(() => {});
    if (command.type === 'file_conversation_open') {
      return new Promise<unknown>((resolve) => {
        resolveOpen = () => resolve(snapshotFor(String(command.conversationId)));
      });
    }
    if (command.type === 'file_conversation_prompt') {
      return Promise.resolve(promptAck(command.conversationId));
    }
    return new Promise<unknown>(() => {});
  };

  function AskSendHost() {
    const [slots, setSlots] = useState<Record<string, FileExplorerSessionSlot>>({ 'sess-ask-send': emptyFileExplorerSlot() });
    const slot = slots['sess-ask-send'] ?? emptyFileExplorerSlot();
    latestSlot = slot;
    useCommittedFileExplorerSlots(slots);
    return createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand: send,
      sessionId: 'sess-ask-send',
      slot,
      onOpenPath: (path: string, _line?: number, view: 'file' | 'diff' = 'file') => {
        setSlots((prev) => ({
          ...prev,
          'sess-ask-send': { tabs: [{ path, view }], activePath: path, mode: 'open' },
        }));
      },
      onPatchSlot: (updater: (slot: FileExplorerSessionSlot) => FileExplorerSessionSlot) => {
        setSlots((prev) => ({
          ...prev,
          'sess-ask-send': updater(prev['sess-ask-send'] ?? emptyFileExplorerSlot()),
        }));
      },
      onConversationError: (message: string) => {
        errors.push(message);
      },
    });
  }

  await act(async () => {
    root.render(createElement(AskSendHost));
  });
  const fileRow = doc.body.querySelector('.file-explorer__row--typescript');
  check('ask send: file row is present', fileRow != null);
  await act(async () => { fileRow?.click(); });
  await act(async () => { seedReadCache('sess-ask-send', 'src/a.ts', ['const x = 1;']); });
  const code = doc.body.querySelector('.file-explorer__code');
  const line = doc.body.querySelector('[data-line="1"]');
  check('ask send: source line is mounted', code != null && line != null);
  const textNode = line?.childNodes.find((child) => child.nodeType === 3) ?? line?.lastChild ?? null;
  const selection: MiniSelection = { anchorNode: textNode, focusNode: line, isCollapsed: false };
  (doc.defaultView as MiniWindow).getSelection = () => selection;
  await act(async () => { code?.dispatchEvent({ type: 'mouseup', target: code }); });
  check('ask send: opening the file mounts the dock beside source', doc.body.querySelector('[data-testid="file-explorer-ask-toggle"]') == null
    && doc.body.querySelector('[data-testid="file-explorer-ask-split"]') != null
    && doc.body.querySelector('.file-conversation') != null
    && doc.body.querySelector('.file-explorer__code') != null);
  check('ask send: blank draft cannot send', doc.body.querySelector('.file-conversation__action--send')?.hasAttribute('disabled') === true);
  await act(async () => {
    updateFileExplorerViewCache('sess-ask-send', (view) => fileExplorerWithConversationDraft(view, 'src/a.ts', 'what is this?'));
  });
  const sendButton = doc.body.querySelector('.file-conversation__action--send');
  check('ask send: typed draft enables send', sendButton != null && sendButton.hasAttribute('disabled') === false);
  await act(async () => { sendButton?.click(); });
  const opened = commands.find((command) => command.type === 'file_conversation_open');
  check('ask send: first send opens the lazily allocated id', opened != null
    && typeof opened.conversationId === 'string'
    && opened.conversationId !== ''
    && opened.path === 'src/a.ts'
    && opened.sessionId === 'sess-ask-send');
  check('ask send: open allocation landed on the existing tab', typeof latestSlot?.tabs[0]?.conversationId === 'string'
    && latestSlot?.tabs[0]?.conversationId === opened?.conversationId);
  check('ask send: prompt waits for the open ack', commands.every((command) => command.type !== 'file_conversation_prompt'));
  await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
  check('ask send: poll skips the id while its open is pending', commands.every((command) => command.type !== 'file_conversation_snapshot')
    && typeof latestSlot?.tabs[0]?.conversationId === 'string');
  const conversationId = opened?.conversationId;
  await act(async () => { resolveOpen?.(); });
  await flushMicrotasks();
  const prompt = commands.find((command) => command.type === 'file_conversation_prompt');
  check('ask send: prompt reuses the exact id, message, and owner', prompt != null
    && prompt.conversationId === conversationId
    && prompt.message === 'what is this?'
    && prompt.sessionId === 'sess-ask-send');
  check('ask send: prompt carries the source mouseup range', JSON.stringify(prompt?.selection) === '{"startLine":1,"endLine":1}');
  await flushMicrotasks();
  check('ask send: accepted ack clears the unchanged draft', fileExplorerConversationDraft(
    fileExplorerViewForOwner(getFileExplorerViewCacheSnapshot(), 'sess-ask-send'),
    'src/a.ts',
  ) === '');
  check('ask send: accepted snapshot entry renders in the dock', (doc.body.querySelector('.file-conversation__transcript')?.textContent ?? '').includes('what is this?'));
  check('ask send: no conversation errors surfaced', errors.length === 0);
});

await withDelayHarness(async (doc, root) => {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  const compactedSlot = openFileSlot('src/a.ts', 'conv-compact');
  fileExplorerSyncPageCacheWithSlots({ 'sess-ask-compact': compactedSlot });
  updateFileExplorerViewCache('sess-ask-compact', () => fileExplorerWithPathSurface(emptyFileExplorerViewState(), 'src/a.ts', 'ask'));
  updateFileExplorerDirCache((prev) => fileExplorerWriteDirState(prev, 'sess-ask-compact', '', () => ({
    entries: [{ name: 'a.ts', path: 'src/a.ts', kind: 'file' }],
    truncated: false,
    loading: false,
    error: null,
    transportIncomplete: false,
    generation: 1,
  })));
  seedReadCache('sess-ask-compact', 'src/a.ts', ['line 1', 'line 2', 'line 3', 'line 4', 'line 5', 'line 6', 'line 7']);
  const commands: Array<Record<string, unknown>> = [];
  let resolveOpen: (() => void) | null = null;
  const send = (command: Record<string, unknown>) => {
    commands.push(command);
    if (command.type === 'file_conversation_open') {
      return new Promise<unknown>((resolve) => {
        resolveOpen = () => resolve(snapshotFor(String(command.conversationId)));
      });
    }
    if (command.type === 'file_conversation_prompt') {
      return Promise.resolve(promptAck(command.conversationId));
    }
    return new Promise<unknown>(() => {});
  };

  function CompactedAskHost() {
    const [slots] = useState<Record<string, FileExplorerSessionSlot>>({ 'sess-ask-compact': compactedSlot });
    const slot = slots['sess-ask-compact'] ?? emptyFileExplorerSlot();
    useCommittedFileExplorerSlots(slots);
    return createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand: send,
      sessionId: 'sess-ask-compact',
      slot,
    });
  }

  await act(async () => {
    root.render(createElement(CompactedAskHost));
  });
  const code = doc.body.querySelector('.file-explorer__code');
  const line = doc.body.querySelector('[data-line="4"]');
  const focus = doc.body.querySelector('[data-line="7"]');
  check('ask compact: selected lines are painted before compaction', code != null && line != null && focus != null);
  const selection: MiniSelection = { anchorNode: line, focusNode: focus, isCollapsed: false };
  (doc.defaultView as MiniWindow).getSelection = () => selection;
  await act(async () => { code?.dispatchEvent({ type: 'mouseup', target: code }); });
  check('ask compact: dock discloses the retained range', (doc.body.querySelector('.file-conversation__context')?.textContent ?? '') === 'Selection lines 4–7');
  const tail = Array.from({ length: 400 }, (_, index) => `tail ${40001 + index}`);
  const kept = readModuleFileExplorerPageCache();
  const current = fileExplorerReadCachedPage(kept, 'sess-ask-compact', 'src/a.ts', 'file');
  check('ask compact: seeded page is present', current != null);
  const replaced = {
    ...kept,
    [fileExplorerPageCacheKey('sess-ask-compact', 'src/a.ts', 'file')]: {
      ...current!,
      startCursor: 40000,
      lines: tail,
      totalLines: 40400,
      nextCursor: undefined,
      truncated: false,
    },
  };
  await act(async () => { writeModuleFileExplorerPageCache(replaced); });
  check(
    'ask compact: painted window no longer contains lines 4–7',
    doc.body.querySelector('[data-line="4"]') == null
      && doc.body.querySelector('#file-explorer-line-40001') != null,
  );
  check(
    'ask compact: dock still discloses the retained range',
    (doc.body.querySelector('.file-conversation__context')?.textContent ?? '') === 'Selection lines 4–7',
  );
  await act(async () => {
    updateFileExplorerViewCache('sess-ask-compact', (view) => fileExplorerWithConversationDraft(view, 'src/a.ts', 'explain the early lines'));
  });
  const sendButton = doc.body.querySelector('.file-conversation__action--send');
  await act(async () => { sendButton?.click(); });
  await act(async () => { resolveOpen?.(); });
  await flushMicrotasks();
  const prompt = commands.find((command) => command.type === 'file_conversation_prompt');
  check(
    'ask compact: prompt keeps the retained range outside the painted window',
    prompt != null
      && prompt.conversationId === 'conv-compact'
      && JSON.stringify(prompt.selection) === '{"startLine":4,"endLine":7}',
  );
});

// Left-drag contract: mouseup selection fully inside the painted source
// mounts the floating toolbar outside the code pane (browser E2E predicate).
await withDelayHarness(async (doc, root) => {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  const floatSlot = openFileSlot('src/a.ts', 'conv-float');
  fileExplorerSyncPageCacheWithSlots({ 'sess-ask-float': floatSlot });
  updateFileExplorerViewCache('sess-ask-float', () => fileExplorerWithPathSurface(emptyFileExplorerViewState(), 'src/a.ts', 'ask'));
  updateFileExplorerDirCache((prev) => fileExplorerWriteDirState(prev, 'sess-ask-float', '', () => ({
    entries: [{ name: 'a.ts', path: 'src/a.ts', kind: 'file' }],
    truncated: false,
    loading: false,
    error: null,
    transportIncomplete: false,
    generation: 1,
  })));
  seedReadCache('sess-ask-float', 'src/a.ts', ['line 1', 'line 2', 'line 3', 'line 4', 'line 5', 'line 6', 'line 7']);

  function FloatAskHost() {
    const [slots] = useState<Record<string, FileExplorerSessionSlot>>({ 'sess-ask-float': floatSlot });
    const slot = slots['sess-ask-float'] ?? emptyFileExplorerSlot();
    useCommittedFileExplorerSlots(slots);
    return createElement(FileExplorerPanel, {
      ...panelProps,
      sessionId: 'sess-ask-float',
      slot,
    });
  }

  await act(async () => {
    root.render(createElement(FloatAskHost));
  });
  const code = doc.body.querySelector('.file-explorer__code');
  const line = doc.body.querySelector('[data-line="4"]');
  const focus = doc.body.querySelector('[data-line="7"]');
  check('ask float: selected lines are painted before mouseup', code != null && line != null && focus != null);
  const selection: MiniSelection = { anchorNode: line, focusNode: focus, isCollapsed: false };
  (doc.defaultView as MiniWindow).getSelection = () => selection;
  await act(async () => { code?.dispatchEvent({ type: 'mouseup', target: code }); });
  check(
    'ask float: mouseup captures the retained range for the dock',
    (doc.body.querySelector('.file-conversation__context')?.textContent ?? '') === 'Selection lines 4–7',
  );
  const toolbar = doc.body.querySelector('[data-testid="file-selection-actions"]');
  const wrapper = doc.body.querySelector('.file-explorer__selection-float');
  check(
    'ask float: left-drag mouseup mounts the selection toolbar outside the code pane',
    toolbar != null
      && wrapper != null
      && wrapper.contains(toolbar)
      && code != null
      && !code.contains(wrapper)
      && toolbar.getAttribute('role') === 'toolbar'
      && toolbar.getAttribute('aria-label') === 'Selected lines',
  );
});

await withDelayHarness(async (doc, root) => {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  const askDupSlot = openFileSlot('src/a.ts');
  fileExplorerSyncPageCacheWithSlots({ 'sess-ask-dup': askDupSlot });
  updateFileExplorerViewCache('sess-ask-dup', () => fileExplorerWithPathSurface(emptyFileExplorerViewState(), 'src/a.ts', 'ask'));
  updateFileExplorerDirCache((prev) => fileExplorerWriteDirState(prev, 'sess-ask-dup', '', () => ({
    entries: [{ name: 'a.ts', path: 'src/a.ts', kind: 'file' }],
    truncated: false,
    loading: false,
    error: null,
    transportIncomplete: false,
    generation: 1,
  })));
  seedReadCache('sess-ask-dup', 'src/a.ts', ['const x = 1;']);

  const commands: Array<Record<string, unknown>> = [];
  const errors: string[] = [];
  let resolveOpen: ((value: Record<string, unknown>) => void) | null = null;
  const send = (command: Record<string, unknown>) => {
    if (command.type !== 'file_conversation_open' && command.type !== 'file_conversation_prompt') {
      return new Promise<unknown>(() => {});
    }
    commands.push(command);
    if (command.type === 'file_conversation_open') {
      return new Promise<unknown>((resolve) => {
        resolveOpen = () => resolve(snapshotFor(String(command.conversationId)));
      });
    }
    return Promise.resolve(promptAck(command.conversationId));
  };

  function DupSendHost() {
    const [slots, setSlots] = useState<Record<string, FileExplorerSessionSlot>>({ 'sess-ask-dup': askDupSlot });
    const slot = slots['sess-ask-dup'] ?? emptyFileExplorerSlot();
    useCommittedFileExplorerSlots(slots);
    return createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand: send,
      sessionId: 'sess-ask-dup',
      slot,
      onPatchSlot: (updater: (slot: FileExplorerSessionSlot) => FileExplorerSessionSlot) => {
        setSlots((prev) => ({
          ...prev,
          'sess-ask-dup': updater(prev['sess-ask-dup'] ?? emptyFileExplorerSlot()),
        }));
      },
      onConversationError: (message: string) => {
        errors.push(message);
      },
    });
  }

  await act(async () => {
    root.render(createElement(DupSendHost));
  });
  await act(async () => {
    updateFileExplorerViewCache('sess-ask-dup', (view) => fileExplorerWithConversationDraft(view, 'src/a.ts', 'what is this?'));
  });
  const sendButton = doc.body.querySelector('.file-conversation__action--send');
  check('ask duplicate: typed draft enables send', sendButton != null && sendButton.hasAttribute('disabled') === false);
  await act(async () => { sendButton?.click(); });
  await act(async () => { sendButton?.click(); });
  const opens = commands.filter((command) => command.type === 'file_conversation_open');
  check('ask duplicate: actual promise lifetime blocks a second open', opens.length === 1 && typeof opens[0]?.conversationId === 'string');
  await act(async () => { resolveOpen?.(); });
  await flushMicrotasks();
  const prompts = commands.filter((command) => command.type === 'file_conversation_prompt');
  check('ask duplicate: exactly one prompt after the open settles', prompts.length === 1 && prompts[0]?.conversationId === opens[0]?.conversationId);
  check('ask duplicate: accepted flow clears the draft once with no errors', fileExplorerConversationDraft(
    fileExplorerViewForOwner(getFileExplorerViewCacheSnapshot(), 'sess-ask-dup'),
    'src/a.ts',
  ) === '' && errors.length === 0);
});

await withDelayHarness(async (doc, root) => {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  const askBusySlot = openFileSlot('src/a.ts', 'conv-busy');
  fileExplorerSyncPageCacheWithSlots({ 'sess-ask-busy': askBusySlot });
  updateFileExplorerViewCache('sess-ask-busy', () => fileExplorerWithPathSurface(emptyFileExplorerViewState(), 'src/a.ts', 'ask'));
  updateFileExplorerDirCache((prev) => fileExplorerWriteDirState(prev, 'sess-ask-busy', '', () => ({
    entries: [{ name: 'a.ts', path: 'src/a.ts', kind: 'file' }],
    truncated: false,
    loading: false,
    error: null,
    transportIncomplete: false,
    generation: 1,
  })));
  seedReadCache('sess-ask-busy', 'src/a.ts', ['const x = 1;']);

  const commands: Array<Record<string, unknown>> = [];
  const errors: string[] = [];
  const send = (command: Record<string, unknown>) => {
    if (command.type !== 'file_conversation_open' && command.type !== 'file_conversation_prompt') {
      return new Promise<unknown>(() => {});
    }
    commands.push(command);
    if (command.type === 'file_conversation_open') {
      return Promise.resolve(snapshotFor(String(command.conversationId)));
    }
    return Promise.resolve(promptAck(command.conversationId, { accepted: false, busy: true }));
  };

  function BusyAskHost() {
    const [slots] = useState<Record<string, FileExplorerSessionSlot>>({ 'sess-ask-busy': askBusySlot });
    const slot = slots['sess-ask-busy'] ?? emptyFileExplorerSlot();
    useCommittedFileExplorerSlots(slots);
    return createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand: send,
      sessionId: 'sess-ask-busy',
      slot,
      onConversationError: (message: string) => {
        errors.push(message);
      },
    });
  }

  await act(async () => {
    root.render(createElement(BusyAskHost));
  });
  await act(async () => {
    updateFileExplorerViewCache('sess-ask-busy', (view) => fileExplorerWithConversationDraft(view, 'src/a.ts', 'kept'));
  });
  const sendButton = doc.body.querySelector('.file-conversation__action--send');
  check('ask busy: typed draft enables send', sendButton != null && sendButton.hasAttribute('disabled') === false);
  await act(async () => { sendButton?.click(); });
  check('ask busy: unconfirmed id re-opens idempotently with the same id', commands[0]?.type === 'file_conversation_open'
    && commands[0]?.conversationId === 'conv-busy'
    && commands[0]?.sessionId === 'sess-ask-busy'
    && commands[1]?.type === 'file_conversation_prompt'
    && commands[1]?.conversationId === 'conv-busy');
  await flushMicrotasks();
  check('ask busy: rejected ack keeps the typed draft', fileExplorerConversationDraft(
    fileExplorerViewForOwner(getFileExplorerViewCacheSnapshot(), 'sess-ask-busy'),
    'src/a.ts',
  ) === 'kept');
  check('ask busy: rejection is not an inline error', errors.length === 0 && doc.body.querySelector('.file-explorer__error') === null);
});

await withDelayHarness(async (doc, root) => {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  const askPollSlot = openFileSlot('src/a.ts', 'conv-poll');
  fileExplorerSyncPageCacheWithSlots({ 'sess-ask-poll': askPollSlot });
  updateFileExplorerViewCache('sess-ask-poll', () => fileExplorerWithPathSurface(emptyFileExplorerViewState(), 'src/a.ts', 'ask'));
  updateFileExplorerDirCache((prev) => fileExplorerWriteDirState(prev, 'sess-ask-poll', '', () => ({
    entries: [{ name: 'a.ts', path: 'src/a.ts', kind: 'file' }],
    truncated: false,
    loading: false,
    error: null,
    transportIncomplete: false,
    generation: 1,
  })));
  seedReadCache('sess-ask-poll', 'src/a.ts', ['const x = 1;']);

  const commands: Array<Record<string, unknown>> = [];
  const errors: string[] = [];
  let latestSlot: FileExplorerSessionSlot | null = null;
  const responses: Array<Record<string, unknown> | Error> = [
    snapshotFor('conv-poll', {
      entries: [wireEntry('e1', 'assistant', 'polled answer')],
    }),
    snapshotFor('conv-poll', {
      entries: [wireEntry('e1', 'assistant', 'polled answer')],
    }),
  ];
  const send = (command: Record<string, unknown>) => {
    if (command.type !== 'file_conversation_snapshot') return new Promise<unknown>(() => {});
    commands.push(command);
    const next = responses.shift();
    if (next === undefined) throw new Error('missing snapshot fixture response');
    return next instanceof Error ? Promise.reject(next) : Promise.resolve(next);
  };

  function PollAskHost() {
    const [slots, setSlots] = useState<Record<string, FileExplorerSessionSlot>>({ 'sess-ask-poll': askPollSlot });
    const slot = slots['sess-ask-poll'] ?? emptyFileExplorerSlot();
    latestSlot = slot;
    useCommittedFileExplorerSlots(slots);
    return createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand: send,
      sessionId: 'sess-ask-poll',
      slot,
      onPatchSlot: (updater: (slot: FileExplorerSessionSlot) => FileExplorerSessionSlot) => {
        setSlots((prev) => ({
          ...prev,
          'sess-ask-poll': updater(prev['sess-ask-poll'] ?? emptyFileExplorerSlot()),
        }));
      },
      onConversationError: (message: string) => {
        errors.push(message);
      },
    });
  }

  await act(async () => {
    root.render(createElement(PollAskHost));
  });
  await flushMicrotasks();
  check('poll: admitted id-bearing tab polls immediately', commands.length === 1
    && commands[0]?.conversationId === 'conv-poll'
    && commands[0]?.sessionId === 'sess-ask-poll');
  check('poll: snapshot entry renders in the dock transcript', (doc.body.querySelector('.file-conversation__transcript')?.textContent ?? '').includes('polled answer'));
  await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS - 1); });
  check('poll: idle cadence holds just under 1500ms', commands.length === 1);
  await act(async () => { advanceClock(1); });
  check('poll: idle cadence repolls at 1500ms', commands.length === 2);
  responses.push(new Error('poll boom'));
  await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
  await flushMicrotasks();
  check('poll: business error reaches the host surface', commands.length === 3 && errors.length === 1 && errors[0] === 'poll boom');
  responses.push(snapshotFor('conv-other'));
  await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
  await flushMicrotasks();
  check('poll: identity-mismatched snapshot is rejected whole', commands.length === 4
    && errors.length === 2
    && errors[1] === 'unusable file conversation snapshot response');
  check('poll: last-good snapshot keeps rendering after rejections', (doc.body.querySelector('.file-conversation__transcript')?.textContent ?? '').includes('polled answer'));
  responses.push(Object.assign(new Error('retired'), { rpcCode: 'file_conversation_not_found' }));
  await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
  await flushMicrotasks();
  check('poll: definitive not-found retires the tab id', commands.length === 5
    && errors.length === 2
    && latestSlot?.tabs[0]?.conversationId === undefined);
  await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS * 2); });
  await flushMicrotasks();
  check('poll: retired id stops polling and resets the dock', commands.length === 5
    && !(doc.body.querySelector('.file-conversation__transcript')?.textContent ?? '').includes('polled answer'));
});

await withDelayHarness(async (doc, root) => {
  // Snapshot singleflight: a slow (never-answered) snapshot must hold the
  // poll to ONE outstanding request per tab across real timer ticks. The
  // saturation incident: pollOnce re-armed every 1500ms regardless of the
  // outstanding request, piling per-connection RPC slots until the backend
  // rejected everything with "too many concurrent RPC commands (limit 16)".
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  const singleflightSlot = openFileSlot('src/a.ts', 'conv-sf');
  fileExplorerSyncPageCacheWithSlots({ 'sess-ask-sf': singleflightSlot });
  updateFileExplorerViewCache('sess-ask-sf', () => fileExplorerWithPathSurface(emptyFileExplorerViewState(), 'src/a.ts', 'ask'));
  updateFileExplorerDirCache((prev) => fileExplorerWriteDirState(prev, 'sess-ask-sf', '', () => ({
    entries: [{ name: 'a.ts', path: 'src/a.ts', kind: 'file' }],
    truncated: false,
    loading: false,
    error: null,
    transportIncomplete: false,
    generation: 1,
  })));
  seedReadCache('sess-ask-sf', 'src/a.ts', ['const x = 1;']);

  const commands: Array<Record<string, unknown>> = [];
  const errors: string[] = [];
  let setSlotsExternal: Dispatch<SetStateAction<Record<string, FileExplorerSessionSlot>>> | null = null;
  const pendingSnapshots: Array<{ id: string; resolve: (value: Record<string, unknown>) => void }> = [];
  const send = (command: Record<string, unknown>) => {
    if (command.type !== 'file_conversation_snapshot') return new Promise<unknown>(() => {});
    commands.push(command);
    const deferred = Promise.withResolvers<unknown>();
    pendingSnapshots.push({ id: String(command.conversationId), resolve: deferred.resolve });
    return deferred.promise;
  };
  const resolveById = async (id: string, snapshot: Record<string, unknown>) => {
    await act(async () => {
      for (const entry of pendingSnapshots.filter((pending) => pending.id === id)) entry.resolve(snapshot);
    });
  };
  const snapshotCommands = () => commands.filter((command) => command.type === 'file_conversation_snapshot');

  function SingleflightHost() {
    const [slots, setSlots] = useState<Record<string, FileExplorerSessionSlot>>({ 'sess-ask-sf': singleflightSlot });
    useEffect(() => {
      setSlotsExternal = setSlots;
    }, []);
    useCommittedFileExplorerSlots(slots);
    return createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand: send,
      sessionId: 'sess-ask-sf',
      slot: slots['sess-ask-sf'] ?? emptyFileExplorerSlot(),
      onConversationError: (message: string) => {
        errors.push(message);
      },
    });
  }

  await act(async () => {
    root.render(createElement(SingleflightHost));
  });
  await flushMicrotasks();
  check('singleflight: id-bearing tab polls immediately', commands.length === 1 && commands[0]?.conversationId === 'conv-sf');

  // Slow server: several idle ticks pass while the first snapshot never
  // answers. At most ONE snapshot per tab may stay outstanding.
  for (let tick = 0; tick < 5; tick += 1) {
    await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
    await flushMicrotasks();
  }
  check('singleflight: slow snapshot holds one outstanding poll per tab across ticks', snapshotCommands().length === 1,
    `saw ${snapshotCommands().length} snapshot commands while the first was pending`);
  check('singleflight: held poll surfaces no host errors', errors.length === 0);

  // Close the tab while the first snapshot is still pending, then reopen the
  // same path under a NEW conversation id. The stale settlement must stay
  // silent, write nothing, and never block the new owner's cadence.
  await act(async () => { setSlotsExternal?.({ 'sess-ask-sf': emptyFileExplorerSlot() }); });
  await flushMicrotasks();
  await act(async () => {
    setSlotsExternal?.({ 'sess-ask-sf': openFileSlot('src/a.ts', 'conv-sf2') });
    updateFileExplorerViewCache('sess-ask-sf', (view) => fileExplorerWithPathSurface(view, 'src/a.ts', 'ask'));
  });
  await flushMicrotasks();
  check('singleflight: reopened tab polls the new id while the stale poll hangs', commands.length === 2
    && commands[1]?.conversationId === 'conv-sf2');
  await resolveById('conv-sf', snapshotFor('conv-sf', {
    entries: [wireEntry('e0', 'assistant', 'stale sf reply')],
  }));
  await flushMicrotasks();
  check('singleflight: stale settlement after reopen stays silent and writes nothing', errors.length === 0
    && !(doc.body.querySelector('.file-conversation__transcript')?.textContent ?? '').includes('stale sf reply'));
  await resolveById('conv-sf2', snapshotFor('conv-sf2', {
    entries: [wireEntry('e1', 'assistant', 'fresh sf answer')],
  }));
  await flushMicrotasks();
  check('singleflight: answered new-owner poll renders in the dock', (doc.body.querySelector('.file-conversation__transcript')?.textContent ?? '').includes('fresh sf answer'));
  await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
  await flushMicrotasks();
  const resumed = snapshotCommands().filter((command) => command.conversationId === 'conv-sf2');
  check('singleflight: cadence resumes after the held poll resolves', resumed.length === 2
    && errors.length === 0);
});

await withDelayHarness(async (doc, root) => {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  const askLateSlot = openFileSlot('src/a.ts', 'conv-late');
  fileExplorerSyncPageCacheWithSlots({ 'sess-ask-late': askLateSlot });
  updateFileExplorerViewCache('sess-ask-late', () => fileExplorerWithPathSurface(emptyFileExplorerViewState(), 'src/a.ts', 'ask'));
  updateFileExplorerDirCache((prev) => fileExplorerWriteDirState(prev, 'sess-ask-late', '', () => ({
    entries: [{ name: 'a.ts', path: 'src/a.ts', kind: 'file' }],
    truncated: false,
    loading: false,
    error: null,
    transportIncomplete: false,
    generation: 1,
  })));
  seedReadCache('sess-ask-late', 'src/a.ts', ['const x = 1;']);

  const commands: Array<Record<string, unknown>> = [];
  const errors: string[] = [];
  let resolveFirstPoll: ((value: Record<string, unknown>) => void) | null = null;
  let setSlotsExternal: Dispatch<SetStateAction<Record<string, FileExplorerSessionSlot>>> | null = null;
  const send = (command: Record<string, unknown>) => {
    if (command.type !== 'file_conversation_snapshot') return new Promise<unknown>(() => {});
    commands.push(command);
    if (commands.length === 1) {
      return new Promise<unknown>((resolve) => {
        resolveFirstPoll = resolve;
      });
    }
    if (commands.length === 2) {
      return Promise.resolve(snapshotFor('conv-late', {
        entries: [wireEntry('e1', 'assistant', 'late stale reply')],
      }));
    }
    return Promise.resolve(snapshotFor(String(command.conversationId), {
      entries: [wireEntry('e2', 'assistant', 'fresh reply')],
    }));
  };

  function LateAckHost() {
    const [slots, setSlots] = useState<Record<string, FileExplorerSessionSlot>>({ 'sess-ask-late': askLateSlot });
    useEffect(() => {
      setSlotsExternal = setSlots;
    }, []);
    useCommittedFileExplorerSlots(slots);
    return createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand: send,
      sessionId: 'sess-ask-late',
      slot: slots['sess-ask-late'] ?? emptyFileExplorerSlot(),
      onConversationError: (message: string) => {
        errors.push(message);
      },
    });
  }

  await act(async () => {
    root.render(createElement(LateAckHost));
  });
  await flushMicrotasks();
  check('late: first poll issued and held open', commands.length === 1 && resolveFirstPoll != null);
  await act(async () => { setSlotsExternal?.({ 'sess-ask-late': emptyFileExplorerSlot() }); });
  await act(async () => {
    resolveFirstPoll?.(snapshotFor('conv-late', {
      entries: [wireEntry('e1', 'assistant', 'late stale reply')],
    }));
  });
  await flushMicrotasks();
  check('late: ack for a closed tab writes nothing and unmounts the dock', doc.body.querySelector('.file-conversation') === null && errors.length === 0);
  await act(async () => {
    setSlotsExternal?.({ 'sess-ask-late': openFileSlot('src/a.ts', 'conv-new') });
    updateFileExplorerViewCache('sess-ask-late', (view) => fileExplorerWithPathSurface(view, 'src/a.ts', 'ask'));
  });
  await flushMicrotasks();
  check('late: reopened same path polls the new id', commands.length === 2 && commands[1]?.conversationId === 'conv-new');
  check('late: stale ack for the retired id is rejected whole', errors.length === 1
    && errors[0] === 'unusable file conversation snapshot response'
    && !(doc.body.querySelector('.file-conversation__transcript')?.textContent ?? '').includes('late stale reply'));
  await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
  await flushMicrotasks();
  check('late: reopened dock renders only the new conversation', (doc.body.querySelector('.file-conversation__transcript')?.textContent ?? '').includes('fresh reply'));
});

await withDelayHarness(async (doc, root) => {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  const askStopSlot = openFileSlot('src/a.ts', 'conv-stop');
  fileExplorerSyncPageCacheWithSlots({ 'sess-ask-stop': askStopSlot });
  updateFileExplorerViewCache('sess-ask-stop', () => fileExplorerWithPathSurface(emptyFileExplorerViewState(), 'src/a.ts', 'ask'));
  updateFileExplorerDirCache((prev) => fileExplorerWriteDirState(prev, 'sess-ask-stop', '', () => ({
    entries: [{ name: 'a.ts', path: 'src/a.ts', kind: 'file' }],
    truncated: false,
    loading: false,
    error: null,
    transportIncomplete: false,
    generation: 1,
  })));
  seedReadCache('sess-ask-stop', 'src/a.ts', ['const x = 1;']);

  const commands: Array<Record<string, unknown>> = [];
  const errors: string[] = [];
  let resolvePending: ((value: Record<string, unknown>) => void) | null = null;
  const send = (command: Record<string, unknown>) => {
    if (command.type !== 'file_conversation_snapshot' && command.type !== 'file_conversation_abort') {
      return new Promise<unknown>(() => {});
    }
    commands.push(command);
    if (commands.length === 1) {
      return Promise.resolve(snapshotFor(String(command.conversationId), {
        streaming: true,
        entries: [wireEntry('e1', 'assistant', 'streaming line', true)],
      }));
    }
    if (commands.length === 2) {
      return new Promise<unknown>((resolve) => {
        resolvePending = resolve;
      });
    }
    if (command.type === 'file_conversation_abort') {
      return Promise.resolve(snapshotFor(String(command.conversationId), {
        streaming: false,
        entries: [wireEntry('e2', 'assistant', 'stopped reply')],
      }));
    }
    return Promise.resolve(snapshotFor(String(command.conversationId)));
  };

  function StopAckHost() {
    const [slots] = useState<Record<string, FileExplorerSessionSlot>>({ 'sess-ask-stop': askStopSlot });
    useCommittedFileExplorerSlots(slots);
    return createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand: send,
      sessionId: 'sess-ask-stop',
      slot: slots['sess-ask-stop'] ?? emptyFileExplorerSlot(),
      onConversationError: (message: string) => {
        errors.push(message);
      },
    });
  }

  await act(async () => {
    root.render(createElement(StopAckHost));
  });
  await flushMicrotasks();
  check('stop: streaming snapshot renders the stop control', (doc.body.querySelector('.file-conversation__transcript')?.textContent ?? '').includes('streaming line')
    && doc.body.querySelector('.file-conversation__action--stop') != null);
  await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
  check('stop: second poll held open to race the stop ack', commands.length === 2 && resolvePending != null);
  const stopButton = doc.body.querySelector('.file-conversation__action--stop');
  await act(async () => { stopButton?.click(); });
  check('stop: abort targets the exact current snapshot id', commands[2]?.type === 'file_conversation_abort'
    && commands[2]?.conversationId === 'conv-stop'
    && commands[2]?.sessionId === 'sess-ask-stop');
  await flushMicrotasks();
  check('stop: abort ack applies immediately', (doc.body.querySelector('.file-conversation__transcript')?.textContent ?? '').includes('stopped reply')
    && doc.body.querySelector('.file-conversation__action--stop') === null);
  await act(async () => {
    resolvePending?.(snapshotFor('conv-stop', {
      streaming: true,
      entries: [wireEntry('e3', 'assistant', 'stale stream', true)],
    }));
  });
  await flushMicrotasks();
  check('stop: stale overlapping poll cannot resurrect streaming', (doc.body.querySelector('.file-conversation__transcript')?.textContent ?? '').includes('stopped reply')
    && !(doc.body.querySelector('.file-conversation__transcript')?.textContent ?? '').includes('stale stream'));
  check('stop: no conversation errors surfaced', errors.length === 0);
});

// ---------------------------------------------------------------------
// Ask fullscreen (focus) conversation: the REAL mounted right-hand dock
// must expose ONE accessible toggle whose id is derived from the host
// toggle id, so the shared App/Chat panel never collides. State is read
// from aria-pressed + the root focus class; Escape exits focus BEFORE the
// drawer close triage, and a fresh mount (real close → reopen) starts off.
// ---------------------------------------------------------------------
await withDelayHarness(async (doc, root) => {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  updateFileExplorerViewCache('sess-ask-focus', () => emptyFileExplorerViewState());
  const conversationSlot = openFileSlot('src/a.ts', 'conv-focus');
  fileExplorerSyncPageCacheWithSlots({ 'sess-ask-focus': conversationSlot });
  seedReadCache('sess-ask-focus', 'src/a.ts', ['const x = 1;']);

  const toggleId = 'chat-file-explorer-toggle-btn';
  const focusButtonId = fileExplorerControlId(toggleId, 'focus-btn');
  const closes: number[] = [];
  const mount = async (slot: FileExplorerSessionSlot): Promise<void> => {
    await act(async () => {
      root.render(createElement(FileExplorerPanel, {
        ...panelProps,
        sendCommand: () => new Promise<unknown>(() => {}),
        sessionId: 'sess-ask-focus',
        slot,
        toggleId,
        onClose: () => { closes.push(1); },
      }));
    });
  };
  const focusButton = () => doc.getElementById(focusButtonId);
  const focusMode = () => (doc.body.querySelector('section.file-explorer')?.className ?? '')
    .split(/\s+/).includes('file-explorer--ask-focus');
  const focusLabel = () => focusButton()?.getAttribute('aria-label') ?? '';

  // Real conversation mount: the dock is the right-hand conversation.
  await mount(conversationSlot);
  const openButton = focusButton();
  check('ask fullscreen: the mounted conversation exposes one derived, accessible focus toggle',
    openButton != null
      && openButton.tagName === 'BUTTON'
      && openButton.getAttribute('type') === 'button'
      && doc.body.querySelectorAll(`#${focusButtonId}`).length === 1
      && openButton.getAttribute('aria-pressed') === 'false'
      && /fullscreen/i.test(openButton.getAttribute('aria-label') ?? '')
      && focusMode() === false);

  // Click in.
  await act(async () => { focusButton()?.click(); });
  check('ask fullscreen: click enters focus mode and reports pressed state',
    focusMode() === true
      && focusButton()?.getAttribute('aria-pressed') === 'true'
      && /exit fullscreen/i.test(focusLabel()));
  check('ask fullscreen: entering focus keeps the drawer and dock mounted',
    closes.length === 0
      && doc.body.querySelector('[data-testid="file-explorer-ask-split"]') != null
      && doc.body.querySelector('.file-conversation') != null);

  // Click out.
  await act(async () => { focusButton()?.click(); });
  check('ask fullscreen: second click exits and restores the entry name',
    focusMode() === false
      && focusButton()?.getAttribute('aria-pressed') === 'false'
      && /fullscreen/i.test(focusLabel())
      && !/exit fullscreen/i.test(focusLabel()));

  // Escape exits focus, never the drawer (the close triage comes later).
  await act(async () => { focusButton()?.click(); });
  await act(async () => { doc.dispatchEvent({ type: 'keydown', key: 'Escape' }); });
  check('ask fullscreen: Escape exits focus before closing the drawer',
    focusMode() === false
      && focusButton()?.getAttribute('aria-pressed') === 'false'
      && closes.length === 0
      && doc.body.querySelector('[data-testid="file-explorer-ask-split"]') != null);

  // Real close → reopen: the host unmounts the panel, then mounts it again.
  await act(async () => { focusButton()?.click(); });
  check('ask fullscreen: re-enter before the close probe', focusMode() === true);
  await act(async () => { root.render(null); });
  await mount(conversationSlot);
  check('ask fullscreen: close → reopen resets focus mode',
    focusMode() === false
      && focusButton()?.getAttribute('aria-pressed') === 'false');
});

// Diff tabs are a right-hand conversation too: the dirty tree's real
// openDiff path (row click -> diff tab) must mount the dock and enable the
// same derived fullscreen toggle; no source-text assertion.
await withDelayHarness(async (doc, root) => {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  updateFileExplorerViewCache('sess-ask-diff-focus', () => emptyFileExplorerViewState());
  const send = (command: Record<string, unknown>) => {
    if (command.type === 'file_explorer_changes') {
      return Promise.resolve({ files: [{ path: 'app.rs', status: 'modified' }], isRepo: true, truncated: false });
    }
    if (command.type === 'file_explorer_list') {
      return Promise.resolve({ entries: [{ name: 'app.rs', path: 'app.rs', kind: 'file', sizeBytes: 12 }], truncated: false });
    }
    if (command.type === 'file_explorer_diff') return new Promise<unknown>(() => {});
    return Promise.resolve({});
  };
  let latestSlot: FileExplorerSessionSlot = { ...emptyFileExplorerSlot(), mode: 'open' };
  function DiffFocusHost() {
    const [slots, setSlots] = useState<Record<string, FileExplorerSessionSlot>>({
      'sess-ask-diff-focus': { ...emptyFileExplorerSlot(), mode: 'open' },
    });
    const slot = slots['sess-ask-diff-focus'] ?? emptyFileExplorerSlot();
    latestSlot = slot;
    useCommittedFileExplorerSlots(slots);
    return createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand: send,
      sessionId: 'sess-ask-diff-focus',
      slot,
      onOpenPath: (path: string, _line?: number, view: 'file' | 'diff' = 'file') => {
        setSlots({ 'sess-ask-diff-focus': { tabs: [{ path, view }], activePath: path, mode: 'open' } });
      },
      onPatchSlot: (updater: (current: FileExplorerSessionSlot) => FileExplorerSessionSlot) => {
        setSlots((prev) => ({
          'sess-ask-diff-focus': updater(prev['sess-ask-diff-focus'] ?? emptyFileExplorerSlot()),
        }));
      },
    });
  }
  await act(async () => { root.render(createElement(DiffFocusHost)); });
  await flushMicrotasks();
  const dirtyRow = doc.body.querySelector('.file-explorer__row--has-status');
  check('ask diff: the dirty tree lists the changed file', dirtyRow != null);
  await act(async () => { dirtyRow?.click(); });
  await flushMicrotasks();
  const focusButtonId = fileExplorerControlId(FILE_EXPLORER_TOGGLE_ID, 'focus-btn');
  const focusButton = doc.getElementById(focusButtonId);
  check('ask diff: opening the diff docks Ask and enables its focus toggle',
    focusButton != null
      && focusButton.hasAttribute('disabled') === false
      && focusButton.getAttribute('aria-pressed') === 'false'
      && doc.body.querySelector('[data-testid="file-explorer-ask-split"]') != null
      && doc.body.querySelector('.file-conversation') != null);
  const focusMode = () => (doc.body.querySelector('section.file-explorer')?.className ?? '')
    .split(/\s+/).includes('file-explorer--ask-focus');
  await act(async () => { focusButton?.click(); });
  const entered = focusMode() === true && focusButton?.getAttribute('aria-pressed') === 'true';
  await act(async () => { focusButton?.click(); });
  check('ask diff: fullscreen enters and exits from the diff view',
    entered && focusMode() === false && focusButton?.getAttribute('aria-pressed') === 'false');
});

await withDelayHarness(async (doc, root) => {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  const send = (command: Record<string, unknown>) => {
    if (command.type === 'file_explorer_changes') return Promise.resolve({ files: [], isRepo: false, truncated: false });
    if (command.type === 'file_explorer_list') {
      return Promise.resolve({ entries: [{ name: 'app.rs', path: 'app.rs', kind: 'file', sizeBytes: 12 }], truncated: false });
    }
    return Promise.resolve({});
  };
  await act(async () => {
    root.render(createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand: send,
      sessionId: 'sess-non-git',
      slot: { ...emptyFileExplorerSlot(), mode: 'open' },
    }));
  });
  await flushMicrotasks();
  check('non-git: normal listing renders without the retired prose',
    doc.body.querySelector('.file-explorer__row') != null
    && !(doc.body.textContent ?? '').includes('Not a git repository'));
});

// Admission repro: first click on a fresh tab loses its read response. The
// child's loadActive passive effect begins the page BEFORE the host's passive
// committed-slot sync admits the tab, so the begin write is admission-pruned
// and the successful response applies against a missing state.
const ADMISSION_MARKER = 'ADMISSION-REPRO-MARKER-7Q4';
await withDelayHarness(async (doc, root) => {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  fileExplorerSyncPageCacheWithSlots({ 'sess-adm-repro': { tabs: [], activePath: null, mode: 'open' } });
  updateFileExplorerViewCache('sess-adm-repro', () => emptyFileExplorerViewState());
  updateFileExplorerDirCache((prev) => fileExplorerWriteDirState(prev, 'sess-adm-repro', '', () => ({
    entries: [{ name: 'a.ts', path: 'src/a.ts', kind: 'file' }],
    truncated: false, loading: false, error: null, transportIncomplete: false, generation: 1,
  })));
  const commands: Array<Record<string, unknown>> = [];
  let resolveRead: ((data: unknown) => void) | null = null;
  const send = (command: Record<string, unknown>) => {
    commands.push(command);
    if (command.type === 'file_explorer_read') {
      return new Promise<unknown>((resolve) => { resolveRead = resolve; });
    }
    return new Promise<unknown>(() => {});
  };
  function AdmissionHost() {
    const [slots, setSlots] = useState<Record<string, FileExplorerSessionSlot>>({
      'sess-adm-repro': { tabs: [], activePath: null, mode: 'open' },
    });
    useCommittedFileExplorerSlots(slots);
    return createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand: send,
      sessionId: 'sess-adm-repro',
      slot: slots['sess-adm-repro'] ?? emptyFileExplorerSlot(),
      onOpenPath: (path: string, _line?: number, view: 'file' | 'diff' = 'file') => {
        setSlots((prev) => ({ ...prev, 'sess-adm-repro': { tabs: [{ path, view }], activePath: path, mode: 'open' } }));
      },
    });
  }
  await act(async () => { root.render(createElement(AdmissionHost)); });
  const fileRow = doc.body.querySelector('.file-explorer__row--typescript');
  check('admission repro: tree row for src/a.ts is present', fileRow != null);
  await act(async () => { fileRow?.click(); });
  const read = commands.find((command) => command.type === 'file_explorer_read');
  check('admission repro: clicked tab sends file_explorer_read', read != null && read.path === 'src/a.ts' && read.sessionId === 'sess-adm-repro');
  await act(async () => { resolveRead?.({ path: 'src/a.ts', lines: [ADMISSION_MARKER], totalLines: 1, truncated: false }); });
  await flushMicrotasks();
  const markerRow = [...doc.body.querySelectorAll('.file-explorer__line')].find((row) => (row.textContent ?? '').includes(ADMISSION_MARKER));
  check('admission repro: source row paints the unique marker', markerRow != null);
  const settled = readModuleFileExplorerPageCache()[fileExplorerPageCacheKey('sess-adm-repro', 'src/a.ts', 'file')];
  check('admission repro: read settles with marker lines and loading false', settled != null && settled.loading === false && settled.lines.includes(ADMISSION_MARKER));
  await act(async () => { advanceClock(FILE_EXPLORER_LOADING_DELAY_MS); });
  check('admission repro: content loading settles, no stuck shimmer', loadingOf(doc, 'content') === null);
});



await withDelayHarness(async (doc, root) => {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  fileExplorerSyncPageCacheWithSlots({ 'sess-search': emptyFileExplorerSlot() });
  updateFileExplorerViewCache('sess-search', () => emptyFileExplorerViewState());
  const opened: Array<{ path: string; view: string }> = [];
  const commands: Array<Record<string, unknown>> = [];
  const sendCommand = (command: Record<string, unknown>) => {
    commands.push(command);
    if (command.type === 'file_explorer_changes') {
      return Promise.resolve({ files: [], isRepo: true, truncated: false });
    }
    if (command.type === 'file_explorer_search') {
      return Promise.resolve({
        entries: [
          { name: 'nested', path: 'src/nested', kind: 'dir' },
          { name: 'widget.rs', path: 'src/nested/widget.rs', kind: 'file' },
        ],
        truncated: false,
      });
    }
    if (command.type === 'file_explorer_list') {
      const path = typeof command.path === 'string' ? command.path : '';
      if (path === 'src') {
        return Promise.resolve({
          entries: [{ name: 'nested', path: 'src/nested', kind: 'dir' }],
          truncated: false,
        });
      }
      if (path === 'src/nested') {
        return Promise.resolve({
          entries: [{ name: 'widget.rs', path: 'src/nested/widget.rs', kind: 'file' }],
          truncated: false,
        });
      }
      return Promise.resolve({
        entries: [{ name: 'src', path: 'src', kind: 'dir' }],
        truncated: false,
      });
    }
    return Promise.resolve({});
  };
  let searchSlot: FileExplorerSessionSlot = emptyFileExplorerSlot();
  const renderSearch = () => {
    root.render(createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand,
      sessionId: 'sess-search',
      slot: searchSlot,
      onPatchSlot: (updater: (current: FileExplorerSessionSlot) => FileExplorerSessionSlot) => {
        searchSlot = updater(searchSlot);
        renderSearch();
      },
      onOpenPath: (path: string, _line?: number, view: 'file' | 'diff' = 'file') => {
        opened.push({ path, view });
      },
    }));
  };
  await act(async () => { renderSearch(); });
  await act(async () => {});
  const input = doc.body.querySelector('[data-testid="file-explorer-search"]');
  check('search box is mounted', input != null);
  // Node has no native input event here, so React uses the keyup polyfill.
  // That polyfill only watches the element after focusin, and focusin calls
  // the legacy attachEvent hook.
  await act(async () => {
    if (!input) return;
    const field = input as MiniNode & { value?: string; type?: string; attachEvent?: () => void; detachEvent?: () => void };
    field.type = 'search';
    field.attachEvent = () => {};
    field.detachEvent = () => {};
    field.dispatchEvent({ type: 'focusin', target: field, preventDefault() {}, stopPropagation() {} });
    field.value = 'widget';
    field.dispatchEvent({ type: 'keyup', target: field, preventDefault() {}, stopPropagation() {} });
  });
  await act(async () => { advanceClock(200); });
  await act(async () => {});
  const labels = [...doc.body.querySelectorAll('.file-explorer__row')]
    .map((node) => (node.textContent ?? '').trim());
  check(
    'search lists the directory and the nested file',
    labels.includes('src/nested') && labels.includes('src/nested/widget.rs'),
    labels.join('|'),
  );
  check('search does not stop after a match cap', !(doc.body.textContent ?? '').includes('Search stopped early'));
  const file = [...doc.body.querySelectorAll('.file-explorer__row')]
    .find((node) => (node.textContent ?? '').includes('widget.rs'));
  await act(async () => { file?.click(); });
  check(
    'clicking a search file opens it',
    opened.some((entry) => entry.path === 'src/nested/widget.rs' && entry.view === 'file'),
  );
  const dir = [...doc.body.querySelectorAll('.file-explorer__row')]
    .find((node) => (node.textContent ?? '').trim() === 'src/nested');
  await act(async () => { dir?.click(); });
  const view = fileExplorerViewForOwner(getFileExplorerViewCacheSnapshot(), 'sess-search');
  check(
    'clicking a search directory expands its parents',
    view.expandedDirs.src === true && view.expandedDirs['src/nested'] === true,
  );
  check(
    'clicking a search directory returns to the tree',
    doc.body.querySelector('[data-testid="file-explorer-search-results"]') == null
      && doc.body.querySelector('.file-explorer__row--dir') != null,
  );
  check(
    'search command carried the query',
    commands.some((command) => command.type === 'file_explorer_search' && command.query === 'widget'),
  );
});

await withDelayHarness(async (doc, root) => {
  let slot: FileExplorerSessionSlot = {
    ...emptyFileExplorerSlot(),
    mode: 'open',
    tabs: [{ path: 'src/nested', view: 'dir' }],
    activePath: 'src/nested',
    treeQuery: 'daemon.rs',
  };
  await act(async () => {
    root.render(createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand: (command: Record<string, unknown>) => {
        if (command.type === 'file_explorer_list' || command.type === 'file_explorer_changes') {
          return Promise.resolve(command.type === 'file_explorer_list'
            ? { entries: [], truncated: false }
            : { files: [], isRepo: true, truncated: false });
        }
        if (command.type === 'file_explorer_search') {
          return Promise.resolve({ entries: [], truncated: false });
        }
        return Promise.resolve({});
      },
      sessionId: 'session-owner',
      slot,
      onPatchSlot: (updater: (current: FileExplorerSessionSlot) => FileExplorerSessionSlot) => {
        slot = updater(slot);
      },
    }));
  });
  const search = doc.body.querySelector('[data-testid="file-explorer-search"]') as (MiniNode & { value?: string }) | null;
  check(
    'passive directory restore keeps the injected search query',
    search?.value === 'daemon.rs' && slot.treeQuery === 'daemon.rs',
    JSON.stringify({ value: search?.value, query: slot.treeQuery }),
  );
});

await withDelayHarness(async (doc, root) => {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  fileExplorerSyncPageCacheWithSlots({ 'session-reveal': emptyFileExplorerSlot() });
  updateFileExplorerViewCache('session-reveal', () => emptyFileExplorerViewState());
  const commands: Array<Record<string, unknown>> = [];
  const listed: string[] = [];
  let slot: FileExplorerSessionSlot = {
    ...emptyFileExplorerSlot(),
    mode: 'open',
    tabs: [{ path: 'src/nested', view: 'dir' }],
    activePath: 'src/nested',
    treeQuery: 'daemon.rs',
  };
  const renderReveal = () => {
    root.render(createElement(FileExplorerPanel, {
      ...panelProps,
      exclusivePanes: true,
      sendCommand: (command: Record<string, unknown>) => {
        commands.push(command);
        if (command.type === 'file_explorer_list') {
          const path = typeof command.path === 'string' ? command.path : '';
          listed.push(path);
          if (path === 'src') {
            return Promise.resolve({
              entries: [{ name: 'nested', path: 'src/nested', kind: 'dir' }],
              truncated: false,
            });
          }
          if (path === 'src/nested') {
            return Promise.resolve({
              entries: [{ name: 'widget.rs', path: 'src/nested/widget.rs', kind: 'file' }],
              truncated: false,
            });
          }
          return Promise.resolve({
            entries: [{ name: 'src', path: 'src', kind: 'dir' }],
            truncated: false,
          });
        }
        if (command.type === 'file_explorer_changes') {
          return Promise.resolve({ files: [], isRepo: true, truncated: false });
        }
        if (command.type === 'file_explorer_search') {
          return Promise.resolve({ entries: [], truncated: false });
        }
        return Promise.resolve({});
      },
      sessionId: 'session-reveal',
      slot,
      onPatchSlot: (updater: (current: FileExplorerSessionSlot) => FileExplorerSessionSlot) => {
        slot = updater(slot);
        renderReveal();
      },
      onOpenPath: () => {},
    }));
  };
  await act(async () => { renderReveal(); });
  await act(async () => {});
  await flushMicrotasks();
  const view = fileExplorerViewForOwner(getFileExplorerViewCacheSnapshot(), 'session-reveal');
  const tree = doc.body.querySelector('[data-testid="file-explorer-tree"]');
  const layout = doc.body.querySelector('[data-testid="file-explorer-layout"]')?.getAttribute('data-layout') ?? '';
  const search = doc.body.querySelector('[data-testid="file-explorer-search"]') as (MiniNode & { value?: string }) | null;
  const reads = commands.filter((command) => command.type === 'file_explorer_read');
  check(
    'external nested dir tab loads and expands its ancestor chain',
    view.expandedDirs.src === true
      && view.expandedDirs['src/nested'] === true
      && listed.includes('src')
      && listed.includes('src/nested'),
    JSON.stringify({ expandedDirs: view.expandedDirs, listed }),
  );
  check(
    'external nested dir tab keeps the injected search and does not read the directory as a file',
    search?.value === 'daemon.rs'
      && slot.treeQuery === 'daemon.rs'
      && slot.tabs.length === 1
      && slot.tabs[0]?.path === 'src/nested'
      && slot.tabs[0]?.view === 'dir'
      && slot.activePath === 'src/nested'
      && reads.length === 0,
    JSON.stringify({ value: search?.value, treeQuery: slot.treeQuery, tabs: slot.tabs, activePath: slot.activePath, reads }),
  );
  check(
    'mobile directory destination keeps the tree visible',
    tree?.hasAttribute('hidden') === false
      && layout === 'tree'
      && (doc.body.textContent ?? '').includes('Directory expanded in the tree.'),
    JSON.stringify({ hidden: tree?.hasAttribute('hidden'), layout }),
  );
}, { matchMediaMatches: true });


await withDelayHarness(async (doc, root) => {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  updateFileExplorerViewCache('session-mobile', () => emptyFileExplorerViewState());
  const commands: Array<Record<string, unknown>> = [];
  let resolveRead: ((data: unknown) => void) | null = null;
  const initial: FileExplorerSessionSlot = {
    ...emptyFileExplorerSlot(),
    mode: 'open',
    tabs: [{ path: 'unique.rs', view: 'file' }],
    activePath: null,
    treeQuery: 'homonym.rs',
  };
  const slots = { 'session-mobile': initial };
  let latest = initial;
  function MobileSearchHost() {
    const [state, setState] = useState(slots);
    latest = state['session-mobile'] ?? initial;
    useCommittedFileExplorerSlots(state);
    return createElement(FileExplorerPanel, {
      ...panelProps,
      exclusivePanes: true,
      sessionId: 'session-mobile',
      slot: latest,
      onOpenPath: (path: string, _line?: number, view: 'file' | 'diff' = 'file') => {
        setState((current) => ({
          ...current,
          'session-mobile': {
            ...(current['session-mobile'] ?? initial),
            tabs: [...(current['session-mobile']?.tabs ?? []), { path, view }],
            activePath: path,
            mode: 'open',
          },
        }));
      },
      sendCommand: (command: Record<string, unknown>) => {
        commands.push(command);
        if (command.type === 'file_explorer_search') {
          return Promise.resolve({
            entries: [
              { name: 'homonym.rs', path: 'pkg/a/bridge/homonym.rs', kind: 'file' },
              { name: 'homonym.rs', path: 'pkg/b/bridge/homonym.rs', kind: 'file' },
            ],
            truncated: false,
          });
        }
        if (command.type === 'file_explorer_read') {
          return new Promise<unknown>((resolve) => { resolveRead = resolve; });
        }
        return Promise.resolve(command.type === 'file_explorer_list'
          ? { entries: [], truncated: false }
          : { files: [], isRepo: true, truncated: false });
      },
    });
  }
  await act(async () => { root.render(createElement(MobileSearchHost)); });
  await act(async () => { advanceClock(200); });
  await flushMicrotasks();
  const tree = doc.body.querySelector('[data-testid="file-explorer-tree"]');
  const layout = doc.body.querySelector('[data-testid="file-explorer-layout"]')?.getAttribute('data-layout') ?? '';
  const labels = [...doc.body.querySelectorAll('.file-explorer__row')]
    .map((node) => (node.textContent ?? '').trim());
  check(
    'mobile search is the visible tree while the previous file tab stays retained',
    tree?.hasAttribute('hidden') === false
      && layout === 'tree'
      && labels.includes('pkg/a/bridge/homonym.rs')
      && labels.includes('pkg/b/bridge/homonym.rs')
      && latest.tabs.some((tab) => tab.path === 'unique.rs')
      && !(doc.body.textContent ?? '').includes('CITE-UNIQUE'),
    JSON.stringify({ hidden: tree?.hasAttribute('hidden'), layout, labels, tabs: latest.tabs, commands }),
  );
  const chosen = [...doc.body.querySelectorAll('.file-explorer__row')]
    .find((node) => (node.textContent ?? '').includes('pkg/a/bridge/homonym.rs'));
  await act(async () => { chosen?.click(); });
  await act(async () => {
    resolveRead?.({ path: 'pkg/a/bridge/homonym.rs', lines: ['HOMONYM-A'], totalLines: 1, truncated: false });
  });
  await flushMicrotasks();
  const content = doc.body.querySelector('[data-testid="file-explorer-content"]');
  const after = doc.body.querySelector('[data-testid="file-explorer-layout"]')?.getAttribute('data-layout') ?? '';
  const reads = commands.filter((command) => command.type === 'file_explorer_read');
  const cacheKey = fileExplorerPageCacheKey('session-mobile', 'pkg/a/bridge/homonym.rs', 'file');
  check(
    'selecting a mobile search row shows that file and keeps the previous tab',
    latest.activePath === 'pkg/a/bridge/homonym.rs'
      && latest.tabs.some((tab) => tab.path === 'unique.rs')
      && latest.tabs.some((tab) => tab.path === 'pkg/a/bridge/homonym.rs')
      && reads.some((command) => command.path === 'pkg/a/bridge/homonym.rs')
      && readModuleFileExplorerPageCache()[cacheKey]?.lines.includes('HOMONYM-A') === true
      && after === 'content'
      && content?.hasAttribute('hidden') === false
      && (content?.textContent ?? '').includes('HOMONYM-A')
      && !(doc.body.textContent ?? '').includes('CITE-UNIQUE'),
    JSON.stringify({ activePath: latest.activePath, tabs: latest.tabs, reads, cache: readModuleFileExplorerPageCache()[cacheKey], after, hidden: content?.hasAttribute('hidden'), text: (content?.textContent ?? '').slice(0, 300) }),
  );
}, { matchMediaMatches: true });

/** Mounted Files panel: a slot treeQuery is the only search authority. Two
 *  same-basename hits stay listed until the user clicks one row. This does
 *  not execute the App click interceptor; the browser lane owns that. */
const AMBIGUOUS_BASENAMES = [
  {
    query: 'deposit_logs.rs',
    paths: [
      'psy-node/psy_cli/psy_relayer_cli/src/bridge/deposit_logs.rs',
      'psy-node-fix/psy_cli/psy_relayer_cli/src/bridge/deposit_logs.rs',
    ],
  },
  {
    query: 'daemon.rs',
    paths: [
      'psy-node/psy_cli/psy_relayer_cli/src/bridge/daemon.rs',
      'psy-node-fix/psy_cli/psy_relayer_cli/src/bridge/daemon.rs',
    ],
  },
  {
    query: 'prove_bridge.rs',
    paths: [
      'psy-node/psy_cli/psy_relayer_cli/src/bridge/prove_bridge.rs',
      'psy-node-fix/psy_cli/psy_relayer_cli/src/bridge/prove_bridge.rs',
    ],
  },
] as const;

function fileExplorerSearchHits(query: string): { name: string; path: string; kind: 'file' }[] {
  const spec = AMBIGUOUS_BASENAMES.find((candidate) => candidate.query === query);
  return (spec?.paths ?? []).map((path) => ({ name: query, path, kind: 'file' as const }));
}

await withDelayHarness(async (doc, root) => {
  const ownerSessionId = 'session-owner';
  const opened: { path: string; view: string }[] = [];
  const commands: Record<string, unknown>[] = [];
  let slot: FileExplorerSessionSlot = {
    ...emptyFileExplorerSlot(),
    mode: 'open',
    treeQuery: 'daemon.rs',
  };
  const renderSlot = () => {
    root.render(createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand: (command: Record<string, unknown>) => {
        commands.push(command);
        if (command.type === 'file_explorer_list') {
          return Promise.resolve({ entries: [], truncated: false });
        }
        if (command.type === 'file_explorer_changes') {
          return Promise.resolve({ files: [], isRepo: true, truncated: false });
        }
        if (command.type === 'file_explorer_search') {
          const query = typeof command.query === 'string' ? command.query : '';
          return Promise.resolve({ entries: fileExplorerSearchHits(query), truncated: false });
        }
        return Promise.resolve({});
      },
      sessionId: ownerSessionId,
      workspaceCwd: '/workspace/mainnet-beta',
      slot,
      onPatchSlot: (updater: (current: FileExplorerSessionSlot) => FileExplorerSessionSlot) => {
        slot = updater(slot);
        renderSlot();
      },
      onOpenPath: (path: string, _line?: number, view: 'file' | 'diff' = 'file') => {
        opened.push({ path, view });
        slot = { ...slot, tabs: [{ path, view }], activePath: path, mode: 'open' };
        renderSlot();
      },
    }));
  };
  await act(async () => { renderSlot(); });
  await act(async () => { advanceClock(200); });
  await act(async () => {});
  const search = doc.body.querySelector('[data-testid="file-explorer-search"]') as (MiniNode & { value?: string }) | null;
  const labels = [...doc.body.querySelectorAll('.file-explorer__row')]
    .map((node) => (node.textContent ?? '').trim());
  const searches = commands.filter((command) => command.type === 'file_explorer_search');
  check(
    'ambiguous daemon basename shows the slot query without a file tab',
    search?.value === 'daemon.rs'
      && searches.length === 1
      && searches[0]?.query === 'daemon.rs'
      && searches[0]?.sessionId === ownerSessionId
      && labels.length === 2
      && labels.includes('psy-node/psy_cli/psy_relayer_cli/src/bridge/daemon.rs')
      && labels.includes('psy-node-fix/psy_cli/psy_relayer_cli/src/bridge/daemon.rs')
      && doc.body.querySelector('.file-explorer__tab') == null
      && opened.length === 0,
    JSON.stringify({ value: search?.value, searches, labels, opened }),
  );
  const chosen = [...doc.body.querySelectorAll('.file-explorer__row')]
    .find((node) => (node.textContent ?? '').includes('psy-node/psy_cli/psy_relayer_cli/src/bridge/daemon.rs'));
  await act(async () => { chosen?.click(); });
  check(
    'selecting the psy-node daemon row opens only that exact file',
    opened.length === 1
      && opened[0]?.path === 'psy-node/psy_cli/psy_relayer_cli/src/bridge/daemon.rs'
      && opened[0]?.view === 'file'
      && slot.activePath === opened[0]?.path
      && slot.tabs.length === 1,
    JSON.stringify({ opened, activePath: slot.activePath, tabs: slot.tabs }),
  );
});

function graphFixtureMap(dir: string, node: string): Record<string, unknown> {
  return {
    path: dir,
    nodes: [{ path: node, language: 'rust', parse: 'ok', declarations: [], purpose: [] }],
    outgoing: [],
    incoming: [],
    coverage: {
      scanned_files: 1,
      unsupported_files: 0,
      unreadable_files: 0,
      unreadable_directories: 0,
      incomplete_files: 0,
      dynamic_omitted: 0,
      truncated: false,
      visible_entries_only: false,
      incomplete: false,
    },
  };
}

function graphMapPaths(commands: Array<Record<string, unknown>>): string[] {
  return commands.filter((command) => command.type === 'file_explorer_map').map((command) => String(command.path));
}

function graphPainted(doc: MiniDocument): { path: string | null; text: string } {
  const node = doc.body.querySelector('[data-testid="file-dependency-map"]');
  return { path: node?.getAttribute('data-map-path') ?? null, text: node?.textContent ?? '' };
}

function graphRow(doc: MiniDocument, label: string, rootTestId?: string): MiniNode | undefined {
  const root = rootTestId ? doc.body.querySelector(`[data-testid="${rootTestId}"]`) : doc.body;
  return [...(root?.querySelectorAll('.file-explorer__row') ?? [])]
    .find((node) => (node.textContent ?? '').trim() === label);
}

async function mountGraph(
  doc: MiniDocument,
  root: Root,
  sessionId: string,
  initial: FileExplorerSessionSlot,
  listings: Record<string, Array<{ name: string; path: string; kind: 'dir' | 'file' }>>,
  hitsFor: (query: string) => Array<{ name: string; path: string; kind: 'dir' | 'file' }>,
): Promise<{
  commands: Array<Record<string, unknown>>;
  replies: Array<{ path: string; cwd: string; resolve: (data: unknown) => void }>;
  render: (next?: FileExplorerSessionSlot, cwd?: string) => void;
  slot: () => FileExplorerSessionSlot;
}> {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  fileExplorerSyncPageCacheWithSlots({ [sessionId]: initial });
  updateFileExplorerViewCache(sessionId, () => emptyFileExplorerViewState());
  const commands: Array<Record<string, unknown>> = [];
  const replies: Array<{ path: string; cwd: string; resolve: (data: unknown) => void }> = [];
  let slot = initial;
  let cwd = '/workspace/alpha-root';
  const sendCommand = (command: Record<string, unknown>) => {
    commands.push(command);
    if (command.type === 'file_explorer_changes') {
      return Promise.resolve({ files: [], isRepo: true, truncated: false });
    }
    if (command.type === 'file_explorer_list') {
      const path = typeof command.path === 'string' ? command.path : '';
      return Promise.resolve({ entries: listings[path] ?? [], truncated: false });
    }
    if (command.type === 'file_explorer_search') {
      const query = typeof command.query === 'string' ? command.query : '';
      return Promise.resolve({ entries: hitsFor(query), truncated: false });
    }
    if (command.type === 'file_explorer_read') {
      return Promise.resolve({ path: command.path, lines: ['fn commit() {}'], totalLines: 1, truncated: false });
    }
    if (command.type === 'file_explorer_map') {
      const path = typeof command.path === 'string' ? command.path : '';
      const deferred = Promise.withResolvers<unknown>();
      replies.push({ path, cwd, resolve: deferred.resolve });
      return deferred.promise;
    }
    return Promise.resolve({});
  };
  const render = (next?: FileExplorerSessionSlot, nextCwd?: string) => {
    if (next) slot = next;
    if (nextCwd != null) cwd = nextCwd;
    root.render(createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand,
      sessionId,
      workspaceCwd: cwd,
      slot,
      onPatchSlot: (updater: (current: FileExplorerSessionSlot) => FileExplorerSessionSlot) => {
        slot = updater(slot);
        render();
      },
      onOpenPath: (path: string, _line?: number, view: 'file' | 'diff' = 'file') => {
        slot = { ...slot, tabs: [{ path, view }], activePath: path, mode: 'open', treeQuery: undefined };
        render();
      },
    }));
  };
  await act(async () => { render(); });
  await flushMicrotasks();
  return { commands, replies, render: (next, nextCwd) => { render(next, nextCwd); }, slot: () => slot };
}

await withDelayHarness(async (doc, root) => {
  const mounted = await mountGraph(doc, root, 'sess-graph-search', {
    ...emptyFileExplorerSlot(),
    mode: 'open',
    tabs: [{ path: 'alpha/commit.rs', view: 'file' }],
    activePath: 'alpha/commit.rs',
    treeQuery: 'beta',
  }, {}, (query) => (query === 'beta' ? [{ name: 'beta', path: 'beta', kind: 'dir' }] : []));
  await act(async () => { advanceClock(200); });
  await flushMicrotasks();
  const beta = graphRow(doc, 'beta', 'file-explorer-search-results');
  check('graph search: beta row is listed while the alpha file stays active', beta != null && mounted.slot().activePath === 'alpha/commit.rs');
  await act(async () => { beta?.click(); });
  await flushMicrotasks();
  const before = graphMapPaths(mounted.commands).length;
  await act(async () => { doc.body.querySelector('[data-testid="file-explorer-map-toggle"]')?.click(); });
  await flushMicrotasks();
  check(
    'graph search: selected beta requests beta once',
    graphMapPaths(mounted.commands).length === before + 1 && graphMapPaths(mounted.commands).at(-1) === 'beta',
    JSON.stringify(graphMapPaths(mounted.commands)),
  );
  check('graph search: dialog names beta', (doc.body.querySelector('.file-explorer__map-dialog-title')?.textContent ?? '').trim() === 'Graph · beta');
});

await withDelayHarness(async (doc, root) => {
  const mounted = await mountGraph(doc, root, 'sess-graph-tree', emptyFileExplorerSlot(), {
    '': [{ name: 'beta', path: 'beta', kind: 'dir' }],
  }, () => []);
  await flushMicrotasks();
  const beta = graphRow(doc, 'beta');
  check('graph tree: beta directory row is present with no active file', beta != null && mounted.slot().activePath == null);
  await act(async () => { beta?.click(); });
  await flushMicrotasks();
  const before = graphMapPaths(mounted.commands).length;
  await act(async () => { doc.body.querySelector('[data-testid="file-explorer-map-toggle"]')?.click(); });
  await flushMicrotasks();
  check(
    'graph tree: clicking beta with no open file requests beta once',
    graphMapPaths(mounted.commands).length === before + 1 && graphMapPaths(mounted.commands).at(-1) === 'beta',
    JSON.stringify(graphMapPaths(mounted.commands)),
  );
});

await withDelayHarness(async (doc, root) => {
  const mounted = await mountGraph(doc, root, 'sess-graph-host', {
    ...emptyFileExplorerSlot(),
    mode: 'open',
    tabs: [{ path: 'alpha/commit.rs', view: 'file' }],
    activePath: 'alpha/commit.rs',
    treeQuery: 'beta',
  }, {
    '': [
      { name: 'alpha', path: 'alpha', kind: 'dir' },
      { name: 'beta', path: 'beta', kind: 'dir' },
    ],
    alpha: [{ name: 'commit.rs', path: 'alpha/commit.rs', kind: 'file' }],
  }, (query) => (query === 'beta' ? [{ name: 'beta', path: 'beta', kind: 'dir' }] : []));
  await act(async () => { advanceClock(200); });
  await flushMicrotasks();
  await act(async () => { graphRow(doc, 'beta', 'file-explorer-search-results')?.click(); });
  await flushMicrotasks();
  await act(async () => { doc.body.querySelector('[data-testid="file-explorer-map-toggle"]')?.click(); });
  await flushMicrotasks();
  const heldBeta = mounted.replies.at(-1);
  check('graph host: beta map is held', heldBeta?.path === 'beta');
  await act(async () => {
    mounted.render({
      ...mounted.slot(),
      tabs: [{ path: 'alpha/commit.rs', view: 'file' }, { path: 'gamma/note.rs', view: 'file' }],
      activePath: 'gamma/note.rs',
      mode: 'open',
    });
  });
  check(
    'graph host: opening another file requests that file parent',
    graphMapPaths(mounted.commands).at(-1) === 'gamma' && graphMapPaths(mounted.commands).filter((path) => path === 'beta').length === 1,
    JSON.stringify(graphMapPaths(mounted.commands)),
  );
  heldBeta?.resolve(graphFixtureMap('beta', 'beta/old.rs'));
  await flushMicrotasks();
  check(
    'graph host: stale beta folder reply does not paint over the opened file parent',
    !graphPainted(doc).text.includes('old.rs'),
    graphPainted(doc).path ?? 'none',
  );
  const parent = mounted.replies.at(-1);
  await act(async () => { parent?.resolve(graphFixtureMap('gamma', 'gamma/note.rs')); });
  await flushMicrotasks();
  check('graph host: authoritative file-parent map settles', graphPainted(doc).path === 'gamma' && graphPainted(doc).text.includes('note.rs'));
});

await withDelayHarness(async (doc, root) => {
  const mounted = await mountGraph(doc, root, 'sess-graph-cwd', {
    ...emptyFileExplorerSlot(),
    mode: 'open',
    tabs: [{ path: 'alpha/commit.rs', view: 'file' }],
    activePath: 'alpha/commit.rs',
  }, {}, () => []);
  await act(async () => { doc.body.querySelector('[data-testid="file-explorer-map-toggle"]')?.click(); });
  await flushMicrotasks();
  const first = mounted.replies.at(-1);
  check('graph cwd: first map is alpha', first?.path === 'alpha' && first?.cwd === '/workspace/alpha-root');
  await act(async () => { mounted.render(undefined, '/workspace/beta-root'); });
  await flushMicrotasks();
  const second = mounted.replies.at(-1);
  check(
    'graph cwd: same relative parent is requested again for the new workspace',
    graphMapPaths(mounted.commands).length === 2 && second?.path === 'alpha' && second?.cwd === '/workspace/beta-root',
  );
  first?.resolve(graphFixtureMap('alpha', 'alpha/old-root.rs'));
  await flushMicrotasks();
  check('graph cwd: old workspace reply does not paint', !graphPainted(doc).text.includes('old-root.rs'), graphPainted(doc).path ?? 'none');
  await act(async () => { second?.resolve(graphFixtureMap('alpha', 'alpha/new-root.rs')); });
  await flushMicrotasks();
  check('graph cwd: new workspace map settles', graphPainted(doc).path === 'alpha' && graphPainted(doc).text.includes('new-root.rs'));
});

await withDelayHarness(async (doc, root) => {
  const mounted = await mountGraph(doc, root, 'sess-graph-order', {
    ...emptyFileExplorerSlot(),
    mode: 'open',
    tabs: [{ path: 'alpha/commit.rs', view: 'file' }],
    activePath: 'alpha/commit.rs',
    treeQuery: 'beta',
  }, {}, (query) => (query === 'beta' ? [{ name: 'beta', path: 'beta', kind: 'dir' }] : []));
  await act(async () => { advanceClock(200); });
  await flushMicrotasks();
  await act(async () => { graphRow(doc, 'beta', 'file-explorer-search-results')?.click(); });
  await flushMicrotasks();
  await act(async () => { doc.body.querySelector('[data-testid="file-explorer-map-toggle"]')?.click(); });
  await flushMicrotasks();
  const betaReply = mounted.replies.at(-1);
  await act(async () => {
    mounted.render({
      ...mounted.slot(),
      tabs: [{ path: 'alpha/commit.rs', view: 'file' }, { path: 'gamma/note.rs', view: 'file' }],
      activePath: 'gamma/note.rs',
    });
  });
  const gammaReply = mounted.replies.at(-1);
  check('graph order: file open requests gamma', gammaReply?.path === 'gamma');
  await act(async () => { gammaReply?.resolve(graphFixtureMap('gamma', 'gamma/note.rs')); });
  await flushMicrotasks();
  await act(async () => { betaReply?.resolve(graphFixtureMap('beta', 'beta/ledger.rs')); });
  await flushMicrotasks();
  check(
    'graph order: late beta after settled gamma stays gamma',
    graphPainted(doc).path === 'gamma' && graphPainted(doc).text.includes('note.rs') && !graphPainted(doc).text.includes('ledger.rs'),
  );
});

await withDelayHarness(async (doc, root) => {
  const mounted = await mountGraph(doc, root, 'sess-graph-race', {
    ...emptyFileExplorerSlot(),
    mode: 'open',
    tabs: [{ path: 'alpha/commit.rs', view: 'file' }],
    activePath: 'alpha/commit.rs',
    treeQuery: 'beta',
  }, {}, (query) => (query === 'beta' ? [{ name: 'beta', path: 'beta', kind: 'dir' }] : []));
  await act(async () => { advanceClock(200); });
  await flushMicrotasks();
  await act(async () => { graphRow(doc, 'beta', 'file-explorer-search-results')?.click(); });
  await flushMicrotasks();
  await act(async () => { doc.body.querySelector('[data-testid="file-explorer-map-toggle"]')?.click(); });
  await flushMicrotasks();
  const betaReply = mounted.replies.at(-1);
  await act(async () => {
    mounted.render({
      ...mounted.slot(),
      tabs: [{ path: 'alpha/commit.rs', view: 'file' }, { path: 'gamma/note.rs', view: 'file' }],
      activePath: 'gamma/note.rs',
    });
    betaReply?.resolve(graphFixtureMap('beta', 'beta/stale.rs'));
    await Promise.resolve();
    await Promise.resolve();
  });
  check(
    'graph race: reply settled before the passive effect cannot paint beta',
    !graphPainted(doc).text.includes('stale.rs') && graphPainted(doc).path !== 'beta',
    graphPainted(doc).path ?? 'none',
  );
});

await withDelayHarness(async (doc, root) => {
  const mounted = await mountGraph(doc, root, 'sess-graph-dir-tab', {
    ...emptyFileExplorerSlot(),
    mode: 'open',
    tabs: [{ path: 'beta', view: 'dir' }],
    activePath: 'beta',
  }, {}, () => []);
  const before = graphMapPaths(mounted.commands).length;
  await act(async () => { doc.body.querySelector('[data-testid="file-explorer-map-toggle"]')?.click(); });
  await flushMicrotasks();
  check(
    'graph dir tab: active directory tab requests itself once',
    graphMapPaths(mounted.commands).length === before + 1 && graphMapPaths(mounted.commands).at(-1) === 'beta',
    JSON.stringify(graphMapPaths(mounted.commands)),
  );
});

await withDelayHarness(async (doc, root) => {
  const mounted = await mountGraph(doc, root, 'sess-graph-local-file', emptyFileExplorerSlot(), {
    '': [
      { name: 'beta', path: 'beta', kind: 'dir' },
      { name: 'note.rs', path: 'gamma/note.rs', kind: 'file' },
    ],
  }, () => []);
  await flushMicrotasks();
  await act(async () => { graphRow(doc, 'beta')?.click(); });
  await flushMicrotasks();
  await act(async () => { doc.body.querySelector('[data-testid="file-explorer-map-toggle"]')?.click(); });
  await flushMicrotasks();
  const before = graphMapPaths(mounted.commands).length;
  await act(async () => { graphRow(doc, 'note.rs')?.click(); });
  await flushMicrotasks();
  check(
    'graph local file: opening a file after beta requests that file parent once',
    graphMapPaths(mounted.commands).length === before + 1 && graphMapPaths(mounted.commands).at(-1) === 'gamma',
    JSON.stringify(graphMapPaths(mounted.commands)),
  );
});

await withDelayHarness(async (doc, root) => {
  const listings: Record<string, Array<{ name: string; path: string; kind: 'dir' | 'file' }>> = {
    '': [{ name: 'beta', path: 'beta', kind: 'dir' }, { name: 'commit.rs', path: 'alpha/commit.rs', kind: 'file' }],
    beta: [{ name: 'ledger.rs', path: 'beta/ledger.rs', kind: 'file' }],
  };
  const mounted = await mountGraph(doc, root, 'sess-graph-search-tree', {
    ...emptyFileExplorerSlot(),
    mode: 'open',
    tabs: [{ path: 'alpha/commit.rs', view: 'file' }],
    activePath: 'alpha/commit.rs',
    treeQuery: 'beta',
  }, listings, (query) => (query === 'beta' ? [{ name: 'beta', path: 'beta', kind: 'dir' }] : []));
  await act(async () => { advanceClock(200); });
  await flushMicrotasks();
  await act(async () => { graphRow(doc, 'beta', 'file-explorer-search-results')?.click(); });
  await flushMicrotasks();
  await act(async () => { graphRow(doc, 'beta')?.click(); });
  await flushMicrotasks();
  const before = graphMapPaths(mounted.commands).length;
  await act(async () => { doc.body.querySelector('[data-testid="file-explorer-map-toggle"]')?.click(); });
  await flushMicrotasks();
  const reply = mounted.replies.at(-1);
  check(
    'graph search-tree: the click sequence requests beta once while the file tab stays',
    mounted.slot().activePath === 'alpha/commit.rs'
      && graphMapPaths(mounted.commands).length === before + 1
      && reply?.path === 'beta',
    JSON.stringify(graphMapPaths(mounted.commands)),
  );
  await act(async () => { reply?.resolve(graphFixtureMap('beta', 'beta/ledger.rs')); });
  await flushMicrotasks();
  check(
    'graph search-tree: the beta response paints while the old file stays active',
    graphPainted(doc).path === 'beta' && graphPainted(doc).text.includes('ledger.rs'),
    graphPainted(doc).path ?? 'none',
  );
});

await withDelayHarness(async (doc, root) => {
  const commands: Record<string, unknown>[] = [];
  const pathA = 'psy-node/psy_node_redis/src/store/rewards.rs';
  const pathB = 'psy-node-fix/psy_node_redis/src/store/rewards.rs';
  let slot: FileExplorerSessionSlot = {
    ...emptyFileExplorerSlot(),
    mode: 'open',
    treeQuery: 'rewards.rs',
    citationChoices: {
      path: 'psy_node_redis/src/store/rewards.rs',
      candidates: [pathA, pathB],
    },
  };
  const renderSlot = () => {
    fileExplorerSyncPageCacheWithSlots({ 'session-a': slot });
    root.render(createElement(FileExplorerPanel, {
      ...panelProps,
      exclusivePanes: true,
      sessionId: 'session-a',
      slot,
      sendCommand: (command: Record<string, unknown>) => {
        commands.push(command);
        if (command.type === 'file_explorer_read') {
          return Promise.resolve({ path: command.path, lines: ['REWARDS-NODE-A'], totalLines: 1, truncated: false });
        }
        return Promise.resolve(command.type === 'file_explorer_list'
          ? { entries: [], truncated: false }
          : { files: [], isRepo: true, truncated: false });
      },
      onPatchSlot: (updater: (current: FileExplorerSessionSlot) => FileExplorerSessionSlot) => {
        slot = updater(slot);
        renderSlot();
      },
      onOpenPath: (path: string, line?: number, view: 'file' | 'diff' = 'file') => {
        slot = { ...slot, tabs: [{ path, line, view }], activePath: path, mode: 'open' };
        renderSlot();
      },
    }));
  };
  await act(async () => { renderSlot(); });
  await flushMicrotasks();
  const choices = [...doc.body.querySelectorAll('[data-file-ambiguous-choice]')]
    .map((node) => node.getAttribute('data-file-ambiguous-choice'));
  const search = doc.body.querySelector('[data-testid="file-explorer-search"]') as { value?: string } | null;
  check(
    'citation choices render both relative paths without a search query',
    choices.includes(pathA) && choices.includes(pathB) && search?.value === ''
      && commands.every((command) => command.type !== 'file_explorer_search'),
    JSON.stringify({ choices, value: search?.value, commands }),
  );
  const chosen = [...doc.body.querySelectorAll('[data-file-ambiguous-choice]')]
    .find((node) => node.getAttribute('data-file-ambiguous-choice') === pathA);
  await act(async () => { chosen?.click(); });
  await flushMicrotasks();
  const reads = commands.filter((command) => command.type === 'file_explorer_read');
  check(
    'choosing A reads only A and keeps the stored choices',
    slot.activePath === pathA
      && slot.citationChoices?.candidates.length === 2
      && slot.citationChoices?.path === 'psy_node_redis/src/store/rewards.rs'
      && reads.length === 1 && reads[0]?.path === pathA
      && commands.every((command) => command.type !== 'file_explorer_search' && command.type !== 'get_state'),
    JSON.stringify({ activePath: slot.activePath, commands }),
  );
  await act(async () => { await Promise.resolve(); });
  await flushMicrotasks();
  const content = doc.body.querySelector('[data-testid="file-explorer-content"]');
  check(
    'choosing A shows that file text',
    content?.hasAttribute('hidden') === false
      && (content?.textContent ?? '').includes('REWARDS-NODE-A')
      && !(content?.textContent ?? '').includes('REWARDS-NODE-FIX-B'),
    content?.textContent ?? '',
  );
});

// Mounted regression: a non-empty file and a settled empty diff share one
// path; a different path is a real zero-line file. Diff must say No changes,
// never Empty file. Open file on the diff toolbar reuses openFile and reads
// the already-cached file body.
await withDelayHarness(async (doc, root) => {
  const sessionId = 'sess-empty-diff';
  const path = 'src/same.ts';
  const emptyPath = 'src/empty.ts';
  const fileLines = ['export const same = 1;'];
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  updateFileExplorerViewCache(sessionId, () => emptyFileExplorerViewState());
  fileExplorerSyncPageCacheWithSlots({
    [sessionId]: {
      tabs: [{ path, view: 'diff' }, { path: emptyPath, view: 'file' }],
      activePath: path,
      mode: 'open',
    },
  });
  const fileRequest = { sessionId, path, view: 'file' as const, generation: 1 };
  const diffRequest = { sessionId, path, view: 'diff' as const, generation: 2 };
  const emptyRequest = { sessionId, path: emptyPath, view: 'file' as const, generation: 3 };
  let cache = emptyFileExplorerPageCache();
  cache = fileExplorerApplyCachedPage(fileExplorerBeginCachedPage(cache, fileRequest), fileRequest, {
    path, lines: fileLines, truncated: false, totalLines: fileLines.length,
  });
  cache = fileExplorerApplyCachedPage(fileExplorerBeginCachedPage(cache, diffRequest), diffRequest, {
    path, lines: [], truncated: false, totalLines: 0,
  });
  cache = fileExplorerApplyCachedPage(fileExplorerBeginCachedPage(cache, emptyRequest), emptyRequest, {
    path: emptyPath, lines: [], truncated: false, totalLines: 0,
  });
  writeModuleFileExplorerPageCache(cache);
  let slot: FileExplorerSessionSlot = {
    tabs: [{ path, view: 'diff' }, { path: emptyPath, view: 'file' }],
    activePath: path,
    mode: 'open',
  };
  const reads: string[] = [];
  const renderSlot = () => {
    root.render(createElement(FileExplorerPanel, {
      ...panelProps,
      sessionId,
      slot,
      sendCommand: (command: Record<string, unknown>) => {
        if (command.type === 'file_explorer_read' && command.path === path) {
          reads.push(path);
          return Promise.resolve({ path, lines: fileLines, totalLines: fileLines.length, truncated: false });
        }
        if (command.type === 'file_explorer_read' && command.path === emptyPath) {
          reads.push(emptyPath);
          return Promise.resolve({ path: emptyPath, lines: [], totalLines: 0, truncated: false });
        }
        if (command.type === 'file_explorer_diff' && command.path === path) {
          return Promise.resolve({ path, lines: [], totalLines: 0, truncated: false });
        }
        if (command.type === 'file_explorer_list') return Promise.resolve({ entries: [], truncated: false });
        if (command.type === 'file_explorer_changes') return Promise.resolve({ files: [], isRepo: true, truncated: false });
        return Promise.resolve({});
      },
      onOpenPath: (nextPath: string, line?: number, view: 'file' | 'diff' | 'dir' = 'file') => {
        const tabs = slot.tabs.map((tab) => tab.path === nextPath
          ? { ...tab, ...(line != null ? { line } : {}), view }
          : tab);
        slot = { ...slot, tabs, activePath: nextPath, mode: 'open' };
        renderSlot();
      },
    }));
  };
  await act(async () => { renderSlot(); });
  await flushMicrotasks();
  const contentText = () => doc.body.querySelector('[data-testid="file-explorer-content"]')?.textContent ?? '';
  const diffState = fileExplorerReadCachedPage(readModuleFileExplorerPageCache(), sessionId, path, 'diff');
  const diffView = contentText();
  const openFile = doc.body.querySelector('[data-testid="file-explorer-open-file"]');
  check(
    'mounted empty diff keeps lines=[] and totalLines=0',
    diffState != null && diffState.loading === false && diffState.lines.length === 0 && diffState.totalLines === 0,
    JSON.stringify(diffState),
  );
  check(
    'mounted empty diff says No changes and not Empty file',
    diffView.includes('No changes') && !diffView.includes('Empty file'),
    diffView,
  );
  check(
    'diff toolbar exposes Open file',
    openFile != null && openFile.getAttribute('aria-label') === 'Open file' && (openFile.textContent ?? '').includes('Open file'),
    openFile?.textContent ?? '',
  );
  await act(async () => { openFile?.click(); });
  await flushMicrotasks();
  const opened = contentText();
  const openedState = fileExplorerReadCachedPage(readModuleFileExplorerPageCache(), sessionId, path, 'file');
  check(
    'Open file click reads the cached file body',
    slot.activePath === path
      && slot.tabs.find((tab) => tab.path === path)?.view === 'file'
      && openedState != null && openedState.lines.join('\n') === fileLines[0]
      && opened.includes(fileLines[0]!)
      && !opened.includes('Empty file')
      && !opened.includes('No changes')
      && reads.includes(path) === false,
    JSON.stringify({ view: slot.tabs.find((tab) => tab.path === path)?.view, reads, text: opened }),
  );
  await act(async () => {
    slot = { ...slot, activePath: emptyPath };
    renderSlot();
  });
  await flushMicrotasks();
  const zeroView = contentText();
  const zeroState = fileExplorerReadCachedPage(readModuleFileExplorerPageCache(), sessionId, emptyPath, 'file');
  check(
    'mounted zero-line file on its own path says Empty file',
    slot.activePath === emptyPath
      && zeroState != null && zeroState.loading === false && zeroState.lines.length === 0 && zeroState.totalLines === 0
      && zeroView.includes('Empty file')
      && !zeroView.includes('No changes')
      && doc.body.querySelector('[data-testid="file-explorer-open-file"]') == null,
    JSON.stringify({ lines: zeroState?.lines.length, total: zeroState?.totalLines, text: zeroView }),
  );
});

// ---------------------------------------------------------------------
// Module snapshot singleflight + failure-toast latch. Shared seed: one
// admitted tab carrying a conversation id, per session.
// ---------------------------------------------------------------------
function seedAskSession(sessionId: string, conversationId: string): FileExplorerSessionSlot {
  const slot = openFileSlot('src/a.ts', conversationId);
  fileExplorerSyncPageCacheWithSlots({ [sessionId]: slot });
  updateFileExplorerViewCache(sessionId, () => fileExplorerWithPathSurface(emptyFileExplorerViewState(), 'src/a.ts', 'ask'));
  updateFileExplorerDirCache((prev) => fileExplorerWriteDirState(prev, sessionId, '', () => ({
    entries: [{ name: 'a.ts', path: 'src/a.ts', kind: 'file' }],
    truncated: false,
    loading: false,
    error: null,
    transportIncomplete: false,
    generation: 1,
  })));
  seedReadCache(sessionId, 'src/a.ts', ['const x = 1;']);
  return slot;
}

function resetModuleFlightState(): void {
  writeModuleFileExplorerPageCache(emptyFileExplorerPageCache());
  updateFileExplorerDirCache(() => ({}));
  fileExplorerResetSnapshotFlights();
  fileExplorerConversationToastLatchesResetAll();
}

// Double mount, same flight identity: TWO panels polling the SAME
// authority+session+canonical path+conversation+generation key must share
// ONE request per tick (never two global RPC slots) and BOTH must apply
// the shared result — a `.has → continue` join would drop it for the
// second mount.
await withDelayHarness(async (doc, root) => {
  resetModuleFlightState();
  const dmSlot = seedAskSession('sess-dm', 'conv-dm');
  const commands: Array<Record<string, unknown>> = [];
  const errors: string[] = [];
  const firstFlight = Promise.withResolvers<unknown>();
  let holdFirst = true;
  const send = (command: Record<string, unknown>) => {
    if (command.type !== 'file_conversation_snapshot') return new Promise<unknown>(() => {});
    commands.push(command);
    if (holdFirst) return firstFlight.promise;
    return Promise.resolve(snapshotFor(String(command.conversationId), {
      entries: [wireEntry('e1', 'assistant', 'dm shared answer')],
    }));
  };
  const snapshotCommands = () => commands.filter((command) => command.type === 'file_conversation_snapshot');

  function DoubleMountHost() {
    return createElement('div', null,
      createElement(FileExplorerPanel, {
        ...panelProps,
        sendCommand: send,
        sessionId: 'sess-dm',
        slot: dmSlot,
        onConversationError: (message: string) => { errors.push(message); },
      }),
      createElement(FileExplorerPanel, {
        ...panelProps,
        sendCommand: send,
        sessionId: 'sess-dm',
        slot: dmSlot,
        onConversationError: (message: string) => { errors.push(message); },
      }),
    );
  }

  await act(async () => { root.render(createElement(DoubleMountHost)); });
  await flushMicrotasks();
  check('double mount: first tick issues exactly ONE shared send', snapshotCommands().length === 1,
    `saw ${snapshotCommands().length}`);
  for (let tick = 0; tick < 4; tick += 1) {
    await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
    await flushMicrotasks();
  }
  check('double mount: held flight holds BOTH mounts across ticks (still one send)', snapshotCommands().length === 1,
    `saw ${snapshotCommands().length} sends while the shared flight was pending`);
  check('double mount: no errors while the shared flight is held', errors.length === 0);
  await act(async () => {
    holdFirst = false;
    firstFlight.resolve(snapshotFor('conv-dm', { entries: [wireEntry('e1', 'assistant', 'dm shared answer')] }));
  });
  await flushMicrotasks();
  const docks = Array.from(doc.body.querySelectorAll('.file-conversation__transcript'));
  check('double mount: BOTH mounts applied the shared result', docks.length === 2
    && docks.every((dock) => (dock.textContent ?? '').includes('dm shared answer')),
    `docks=${docks.length}`);
  await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
  await flushMicrotasks();
  check('double mount: next tick resumes with exactly one send', snapshotCommands().length === 2,
    `saw ${snapshotCommands().length}`);
  await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
  await flushMicrotasks();
  check('double mount: cadence keeps exactly one send per tick', snapshotCommands().length === 3,
    `saw ${snapshotCommands().length}`);
});

// Two DIFFERENT sessions with the same path+conversation never share a
// flight: A and B each send (and keep sending) their own request.
await withDelayHarness(async (doc, root) => {
  resetModuleFlightState();
  const slotA = seedAskSession('sess-ab', 'conv-ab');
  const slotB = seedAskSession('sess-bb', 'conv-ab');
  const commands: Array<Record<string, unknown>> = [];
  const send = (command: Record<string, unknown>) => {
    if (command.type !== 'file_conversation_snapshot') return new Promise<unknown>(() => {});
    commands.push(command);
    return Promise.resolve(snapshotFor(String(command.conversationId), {
      entries: [wireEntry('e1', 'assistant', `answer for ${String(command.sessionId)}`)],
    }));
  };
  const bySession = (sid: string) => commands.filter((command) => command.type === 'file_conversation_snapshot' && command.sessionId === sid);

  function TwoSessionHost() {
    return createElement('div', null,
      createElement(FileExplorerPanel, {
        ...panelProps,
        sendCommand: send,
        sessionId: 'sess-ab',
        slot: slotA,
        onConversationError: () => {},
      }),
      createElement(FileExplorerPanel, {
        ...panelProps,
        sendCommand: send,
        sessionId: 'sess-bb',
        slot: slotB,
        onConversationError: () => {},
      }),
    );
  }

  await act(async () => { root.render(createElement(TwoSessionHost)); });
  await flushMicrotasks();
  check('A/B sessions: first tick sends once per session', bySession('sess-ab').length === 1 && bySession('sess-bb').length === 1,
    `a=${bySession('sess-ab').length} b=${bySession('sess-bb').length}`);
  check('A/B sessions: each session renders its own answer', (doc.body.textContent ?? '').includes('answer for sess-ab')
    && (doc.body.textContent ?? '').includes('answer for sess-bb'));
  await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
  await flushMicrotasks();
  check('A/B sessions: next tick still sends exactly once per session', bySession('sess-ab').length === 2 && bySession('sess-bb').length === 2,
    `a=${bySession('sess-ab').length} b=${bySession('sess-bb').length}`);
});

// Reconnect (new connection generation): a new epoch MUST start a NEW
// request even while the old epoch's flight hangs, the old flight's
// release must never delete the NEW flight, and the cancelled
// old-epoch subscriber must apply nothing.
await withDelayHarness(async (doc, root) => {
  resetModuleFlightState();
  const rcSlot = seedAskSession('sess-rc', 'conv-rc');
  const commands: Array<Record<string, unknown>> = [];
  const errors: string[] = [];
  const oldFlight = Promise.withResolvers<unknown>();
  const newFlight = Promise.withResolvers<unknown>();
  const send = (command: Record<string, unknown>) => {
    if (command.type !== 'file_conversation_snapshot') return new Promise<unknown>(() => {});
    commands.push(command);
    if (commands.length === 1) return oldFlight.promise;
    if (commands.length === 2) return newFlight.promise;
    return Promise.resolve(snapshotFor(String(command.conversationId), {
      entries: [wireEntry('e2', 'assistant', 'resumed reply')],
    }));
  };
  const snapshotCommands = () => commands.filter((command) => command.type === 'file_conversation_snapshot');
  let setEpoch: Dispatch<SetStateAction<number>> | null = null;
  function ReconnectHost() {
    const [epoch, setEpochState] = useState(0);
    useEffect(() => { setEpoch = setEpochState; }, []);
    return createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand: send,
      sessionId: 'sess-rc',
      slot: rcSlot,
      connectionEpoch: epoch,
      onConversationError: (message: string) => { errors.push(message); },
    });
  }

  await act(async () => { root.render(createElement(ReconnectHost)); });
  await flushMicrotasks();
  check('reconnect: epoch 0 polls once', snapshotCommands().length === 1);
  await act(async () => { setEpoch?.(1); });
  await flushMicrotasks();
  check('reconnect: new generation starts a NEW request while the old hangs', snapshotCommands().length === 2,
    `saw ${snapshotCommands().length}`);
  const oldKey = fileExplorerSnapshotFlightKey('127.0.0.1:1', 'sess-rc', 'src/a.ts', 'conv-rc', 0);
  const newKey = fileExplorerSnapshotFlightKey('127.0.0.1:1', 'sess-rc', 'src/a.ts', 'conv-rc', 1);
  check('reconnect: old and new flights live under distinct identities',
    fileExplorerPeekSnapshotFlight(oldKey) != null && fileExplorerPeekSnapshotFlight(newKey) != null);
  for (let tick = 0; tick < 3; tick += 1) {
    await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
    await flushMicrotasks();
  }
  check('reconnect: new flight gates the cadence (no stacked sends)', snapshotCommands().length === 2,
    `saw ${snapshotCommands().length}`);
  await act(async () => {
    oldFlight.resolve(snapshotFor('conv-rc', { entries: [wireEntry('e0', 'assistant', 'old epoch reply')] }));
  });
  await flushMicrotasks();
  check('reconnect: old flight settle never deletes the NEW flight', fileExplorerPeekSnapshotFlight(newKey) != null,
    'the new flight was released by the old finally');
  check('reconnect: cancelled old-epoch subscriber applies nothing', errors.length === 0
    && !(doc.body.querySelector('.file-conversation__transcript')?.textContent ?? '').includes('old epoch reply'));
  await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
  await flushMicrotasks();
  check('reconnect: new flight still gates before it resolves', snapshotCommands().length === 2);
  await act(async () => {
    newFlight.resolve(snapshotFor('conv-rc', { entries: [wireEntry('e1', 'assistant', 'new epoch reply')] }));
  });
  await flushMicrotasks();
  check('reconnect: new-generation result renders', (doc.body.querySelector('.file-conversation__transcript')?.textContent ?? '').includes('new epoch reply'));
  await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
  await flushMicrotasks();
  check('reconnect: cadence resumes after the new flight resolves', snapshotCommands().length === 3
    && errors.length === 0
    && (doc.body.querySelector('.file-conversation__transcript')?.textContent ?? '').includes('resumed reply'),
    `saw ${snapshotCommands().length}`);
});

// Failure-toast latch: the SAME owner+message failing repeatedly collapses
// to ONE toast; a DIFFERENT message still reports; only a valid, accepted
// same-owner success resets the latch so the identical failure reports
// again. Failures never clear the last-good snapshot.
await withDelayHarness(async (doc, root) => {
  resetModuleFlightState();
  const latchSlot = seedAskSession('sess-latch', 'conv-latch');
  const commands: Array<Record<string, unknown>> = [];
  const errors: string[] = [];
  const responses: Array<Record<string, unknown> | Error> = [
    snapshotFor('conv-latch', { entries: [wireEntry('e1', 'assistant', 'latch good answer')] }),
    new Error('boom one'),
    new Error('boom one'),
    new Error('boom two'),
    snapshotFor('conv-latch', { entries: [wireEntry('e2', 'assistant', 'latch good answer 2')] }),
    new Error('boom one'),
    new Error('boom one'),
  ];
  const send = (command: Record<string, unknown>) => {
    if (command.type !== 'file_conversation_snapshot') return new Promise<unknown>(() => {});
    commands.push(command);
    const next = responses.shift();
    if (next === undefined) throw new Error('missing snapshot fixture response');
    return next instanceof Error ? Promise.reject(next) : Promise.resolve(next);
  };
  const snapshotCommands = () => commands.filter((command) => command.type === 'file_conversation_snapshot');
  const transcriptText = () => doc.body.querySelector('.file-conversation__transcript')?.textContent ?? '';

  function LatchHost() {
    return createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand: send,
      sessionId: 'sess-latch',
      slot: latchSlot,
      onConversationError: (message: string) => { errors.push(message); },
    });
  }

  await act(async () => { root.render(createElement(LatchHost)); });
  await flushMicrotasks();
  check('toast latch: good poll renders and no error surfaced', transcriptText().includes('latch good answer') && errors.length === 0);
  await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
  await flushMicrotasks();
  await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
  await flushMicrotasks();
  check('toast latch: duplicate same-message failures toast once', errors.length === 1 && errors[0] === 'boom one',
    JSON.stringify(errors));
  check('toast latch: failure keeps the last-good snapshot', transcriptText().includes('latch good answer'));
  await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
  await flushMicrotasks();
  check('toast latch: different error still reports', errors.length === 2 && errors[1] === 'boom two',
    JSON.stringify(errors));
  await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
  await flushMicrotasks();
  check('toast latch: accepted success resets the latch and renders', errors.length === 2
    && transcriptText().includes('latch good answer 2'));
  await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
  await flushMicrotasks();
  await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
  await flushMicrotasks();
  check('toast latch: same error after a success toasts again, then folds', errors.length === 3 && errors[2] === 'boom one',
    JSON.stringify(errors));
  check('toast latch: repeated identical failure stays folded (one send per tick)', snapshotCommands().length === 7
    && errors.length === 3,
    `sends=${snapshotCommands().length} errors=${JSON.stringify(errors)}`);
});

// Per-conversation latch scope: A healthy + B failing in ONE session.
// B folds its identical repeats after its OWN first report; A's accepted
// successes never reset B's latch; the same message from A and B is
// first-reported independently per conversation.
await withDelayHarness(async (doc, root) => {
  resetModuleFlightState();
  const abSlot: FileExplorerSessionSlot = {
    tabs: [
      { path: 'src/a.ts', view: 'file', conversationId: 'conv-a' },
      { path: 'src/b.ts', view: 'file', conversationId: 'conv-b' },
    ],
    activePath: 'src/a.ts',
    mode: 'open',
  };
  fileExplorerSyncPageCacheWithSlots({ 'sess-ab-latch': abSlot });
  updateFileExplorerViewCache('sess-ab-latch', () => fileExplorerWithPathSurface(emptyFileExplorerViewState(), 'src/a.ts', 'ask'));
  updateFileExplorerDirCache((prev) => fileExplorerWriteDirState(prev, 'sess-ab-latch', '', () => ({
    entries: [
      { name: 'a.ts', path: 'src/a.ts', kind: 'file' },
      { name: 'b.ts', path: 'src/b.ts', kind: 'file' },
    ],
    truncated: false,
    loading: false,
    error: null,
    transportIncomplete: false,
    generation: 1,
  })));
  seedReadCache('sess-ab-latch', 'src/a.ts', ['const a = 1;']);

  const commands: Array<Record<string, unknown>> = [];
  const errors: string[] = [];
  let failA = false;
  const send = (command: Record<string, unknown>) => {
    if (command.type !== 'file_conversation_snapshot') return new Promise<unknown>(() => {});
    commands.push(command);
    if (command.conversationId === 'conv-a' && !failA) {
      return Promise.resolve(snapshotFor('conv-a', { entries: [wireEntry('e1', 'assistant', 'healthy a')] }));
    }
    return Promise.reject(new Error('boom b'));
  };
  const pollCount = (id: string) => commands.filter((command) => command.conversationId === id).length;

  function AbLatchHost() {
    return createElement(FileExplorerPanel, {
      ...panelProps,
      sendCommand: send,
      sessionId: 'sess-ab-latch',
      slot: abSlot,
      onConversationError: (message: string) => { errors.push(message); },
    });
  }

  await act(async () => { root.render(createElement(AbLatchHost)); });
  await flushMicrotasks();
  check('latch scope: healthy A renders and failing B reports its first episode', errors.length === 1
    && errors[0] === 'boom b'
    && (doc.body.querySelector('.file-conversation__transcript')?.textContent ?? '').includes('healthy a'),
    JSON.stringify(errors));
  for (let tick = 0; tick < 3; tick += 1) {
    await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
    await flushMicrotasks();
  }
  check('latch scope: B folds its repeats across ticks while A succeeds each tick', errors.length === 1
    && pollCount('conv-a') === 4 && pollCount('conv-b') === 4,
    `errors=${JSON.stringify(errors)} a=${pollCount('conv-a')} b=${pollCount('conv-b')}`);
  // A now fails with the SAME message: its own first episode reports even
  // though B already reported the identical text.
  await act(async () => { failA = true; });
  await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
  await flushMicrotasks();
  check('latch scope: same message from A and B is first-reported per conversation', errors.length === 2
    && errors[0] === 'boom b' && errors[1] === 'boom b',
    JSON.stringify(errors));
  // A recovers (accepted write resets ONLY A's latch), then fails again.
  await act(async () => { failA = false; });
  await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
  await flushMicrotasks();
  await act(async () => { failA = true; });
  await act(async () => { advanceClock(FILE_CONVERSATION_POLL_IDLE_MS); });
  await flushMicrotasks();
  check('latch scope: A reports again after its own success; B stays folded', errors.length === 3
    && errors[2] === 'boom b',
    `errors=${JSON.stringify(errors)} a=${pollCount('conv-a')} b=${pollCount('conv-b')}`);
});




console.log(`\nFileExplorerPanel.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length > 0) {
  for (const failure of failures) console.error(`  FAIL ${failure}`);
  process.exit(1);
}
process.exit(0);
