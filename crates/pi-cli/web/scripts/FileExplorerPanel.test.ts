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
  fileExplorerBodyLayout,
  fileExplorerContentLoadingIdentity,
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
  check('a11y: Refresh keeps its accessible name and has no text label', head.includes(`id="${FILE_EXPLORER_REFRESH_BTN_ID}"`) && head.includes('aria-label="Refresh"') && head.includes('title="Refresh"') && head.includes('aria-hidden="true"') && !head.includes('file-explorer__refresh-label') && !head.includes('Refreshing'));
  check('a11y: close button names Close file explorer', head.includes(`id="${FILE_EXPLORER_CLOSE_BTN_ID}"`) && head.includes('aria-label="Close file explorer"') && head.includes('title="Close file explorer"'));
  check('a11y: fullscreen is absent without onToggleExpanded', !head.includes('expand-btn'));
  check('a11y: expanded search is the tree toolbar and precedes the only toggle', html.includes('data-testid="file-explorer-search"') && html.includes('aria-label="Search workspace files"') && !html.includes('file-explorer__tree-edge') && html.indexOf('data-testid="file-explorer-tree"') < html.indexOf('file-explorer-search') && html.indexOf('file-explorer-search') < html.indexOf(`id="${FILE_EXPLORER_TREE_TOGGLE_ID}"`) && html.includes('aria-label="Collapse file tree"') && (html.match(new RegExp(`id="${FILE_EXPLORER_TREE_TOGGLE_ID}"`, 'g')) ?? []).length === 1);
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
  const html = renderExplorer({ onToggleExpanded: () => {} });
  const head = headerSlice(html);
  check('a11y: fullscreen keeps its name', head.includes('aria-label="Expand file explorer to the full app area"') && head.includes('title="Expand to the full app area"') && !head.includes(FILE_EXPLORER_TREE_TOGGLE_ID));
  check('a11y: fullscreen stays in the header actions', head.includes('expand-btn') && head.includes(FILE_EXPLORER_CLOSE_BTN_ID));
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
    onToggleExpanded: () => {},
  }));
  const head = headerSlice(html);
  const content = html.slice(html.indexOf('data-testid="file-explorer-content"'));
  check('a11y: Source|Preview radiogroup is outside the header', !head.includes('File surface') && content.includes('role="radiogroup"') && content.includes('aria-label="File surface"') && content.includes('>Source<') && content.includes('>Preview<'));
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
      && /file-explorer__expand-btn[\s\S]{0,220}min-(?:width|height):\s*44px/.test(css)
      && /file-explorer__close[\s\S]{0,220}min-(?:width|height):\s*44px/.test(css),
  );
  check(
    'mobile Files drawer sits above the footer',
    /@media \(max-width: 768px\) \{[\s\S]*?\.file-explorer,[\s\S]*?bottom:\s*var\(--app-footer-height/.test(css)
      && /@media \(max-width: 720px\) \{[\s\S]*?\.file-explorer \{[\s\S]*?bottom:\s*var\(--app-footer-height/.test(css)
      && !/@media \(max-width: 720px\) \{[\s\S]*?\.file-explorer \{[^}]*bottom:\s*0\s*;/.test(css),
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
  dispatchEvent(event: { type: string; target?: MiniNode }): boolean;
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
      dispatchEvent(raw: { type: string; target?: MiniNode }) {
        const payload: MiniEvent = {
          type: raw.type,
          target: raw.target ?? node,
          currentTarget: null,
          bubbles: true,
          cancelable: true,
          defaultPrevented: false,
          button: 0,
          preventDefault() { payload.defaultPrevented = true; },
          stopPropagation() { stopped = true; },
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
  ];
  const sendCommand = (command: Record<string, unknown>) => {
    if (command.type === 'file_explorer_changes') {
      return Promise.resolve({ files: [], isRepo: true, truncated: false });
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
  check('ask: no toggle without an active tab', !html.includes('file-explorer-ask-toggle'));
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
  const askAt = content.indexOf('file-explorer-ask-toggle');
  check('ask: toggle renders on the content toolbar', askAt >= 0 && content.includes('aria-label="Ask about this file"') && content.includes('>Ask<') && !head.includes('file-explorer-ask-toggle'));
  check('ask: non-previewable tab offers Source and Ask only', content.includes('>Source<') && !content.includes('>Preview<'));
  check('ask: Source precedes Ask', content.indexOf('>Source<') >= 0 && content.indexOf('>Source<') < askAt);
  check('ask: toggle starts unchecked on the source surface', /file-explorer-ask-toggle[^>]*aria-checked="false"/.test(content));
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
  const sourceAt = content.indexOf('>Source<');
  const previewAt = content.indexOf('>Preview<');
  const askAt = content.indexOf('file-explorer-ask-toggle');
  check('ask: previewable content toolbar orders Source, Preview, Ask', sourceAt >= 0 && sourceAt < previewAt && previewAt < askAt);
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
  check('ask: active surface checks the toggle', /file-explorer-ask-toggle[^>]*aria-checked="true"/.test(html));
  check('ask: dock mounts in the content split beside source', html.includes('data-testid="file-explorer-ask-split"')
    && html.includes('class="file-conversation"')
    && html.includes('file-explorer__content-split')
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
  check('ask: diff view never offers the Ask toggle', !html.includes('file-explorer-ask-toggle'));
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
      && popupBelow.popup.maxHeight === viewport.height - edge - barBottom,
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
      && popupAbove.popup.maxHeight === above.top - viewport.top - edge,
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
      && popupCapped.popup.maxHeight === viewport.height - edge - barBottom
      && popupCapped.popup.maxHeight < 500
      && popupCapped.popup.top === barBottom + edge
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
  const askToggle = doc.body.querySelector('[data-testid="file-explorer-ask-toggle"]');
  check('ask send: toggle is visible and unchecked after open', askToggle != null && askToggle.getAttribute('aria-checked') === 'false');
  await act(async () => { askToggle?.click(); });
  check('ask send: activating the toggle mounts the dock split with source', askToggle?.getAttribute('aria-checked') === 'true'
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



console.log(`\nFileExplorerPanel.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length > 0) {
  for (const failure of failures) console.error(`  FAIL ${failure}`);
  process.exit(1);
}
process.exit(0);
