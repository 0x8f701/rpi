#!/usr/bin/env node

import type { act as Act, createElement as CreateElement } from 'react';
import type { createRoot as CreateRoot, Root } from 'react-dom/client';
import type { CodeReviewPanel as CodeReviewPanelComponent } from '../src/panels/CodeReviewPanel.tsx';
import type { CommitPicker as CommitPickerComponent } from '../src/panels/CommitPicker.tsx';
import type { MarkdownPathOwner } from '../src/markdown.ts';
import { createHash } from 'node:crypto';

let createElement: CreateElement;
let act: Act;
let createRoot: CreateRoot;
let CodeReviewPanel: CodeReviewPanelComponent;

const failures: string[] = [];
let ran = 0;
function check(name: string, condition: unknown, detail = ''): void {
  ran += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

const realSetTimeout = globalThis.setTimeout.bind(globalThis);
let clockNow = 0;
let clockNextId = 1;
let clockPending: Array<{ id: number; at: number; fn: () => void; intervalMs: number | null }> = [];

function fakeSetTimeout(fn: () => void, ms?: number): unknown {
  const id = clockNextId;
  clockNextId += 1;
  clockPending.push({ id, at: clockNow + (ms ?? 0), fn, intervalMs: null });
  return id;
}

function fakeSetInterval(fn: () => void, ms?: number): unknown {
  const id = clockNextId;
  clockNextId += 1;
  clockPending.push({ id, at: clockNow + (ms ?? 0), fn, intervalMs: ms ?? 0 });
  return id;
}

function fakeClear(id: unknown): void {
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
    if (next.intervalMs !== null) {
      clockPending.push({ ...next, at: next.at + next.intervalMs });
    }
  }
  clockNow = until;
}

const realTick = (): Promise<void> => {
  const { promise, resolve } = Promise.withResolvers<void>();
  realSetTimeout(resolve, 0);
  return promise;
};

type Listener = (event: Record<string, unknown>) => void;

interface MiniNode {
  nodeType: number;
  nodeName: string;
  tagName: string | undefined;
  namespaceURI: string | null;
  ownerDocument: MiniDoc;
  parentNode: MiniNode | null;
  childNodes: MiniNode[];
  firstChild: MiniNode | null;
  lastChild: MiniNode | null;
  nextSibling: MiniNode | null;
  previousSibling: MiniNode | null;
  attributes: Map<string, string>;
  listeners: Map<string, Set<Listener>>;
  style: Record<string, unknown>;
  value: string;
  type: string;
  text: string;
  /** File input payload the dock picker reads back through its change event. */
  files?: FileList;
  scrollHeight: number;
  clientHeight: number;
  get className(): string;
  set className(value: string);
  classList: {
    contains(name: string): boolean;
    add(...names: string[]): void;
    remove(...names: string[]): void;
    toggle(name: string, force?: boolean): boolean;
  };
  get id(): string;
  set id(value: string);
  get textContent(): string;
  set textContent(value: string);
  get innerHTML(): string;
  set innerHTML(value: string);
  get nodeValue(): string | null;
  set nodeValue(value: string | null);
  appendChild(child: MiniNode): MiniNode;
  insertBefore(child: MiniNode, before: MiniNode | null): MiniNode;
  removeChild(child: MiniNode): MiniNode;
  remove(): void;
  setAttribute(name: string, value: string): void;
  getAttribute(name: string): string | null;
  removeAttribute(name: string): void;
  hasAttribute(name: string): boolean;
  addEventListener(type: string, fn: Listener, options?: boolean | { capture?: boolean }): void;
  removeEventListener(type: string, fn: Listener, options?: boolean | { capture?: boolean }): void;
  dispatchEvent(event: { type: string; target?: MiniNode } & Record<string, unknown>): boolean;
  click(): void;
  focus(): void;
  blur(): void;
  contains(node: MiniNode | null): boolean;
  closest(selector: string): MiniNode | null;
  querySelector(selector: string): MiniNode | null;
  querySelectorAll(selector: string): MiniNode[];
}

interface MiniDoc extends MiniNode {
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
  oninput: null;
}

interface MiniWindow {
  document: MiniDoc;
  [key: string]: unknown;
}

function createMiniDom(): { window: MiniWindow; document: MiniDoc } {
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

  const makeNode = (nodeType: number, nodeName: string, owner: MiniDoc | null): MiniNode => {
    let scrollTopValue = 0;
    const node = {
      nodeType,
      nodeName,
      tagName: nodeType === 1 ? nodeName : undefined,
      namespaceURI: nodeType === 1 ? 'http://www.w3.org/1999/xhtml' : null,
      ownerDocument: owner as MiniDoc,
      parentNode: null,
      childNodes: [] as MiniNode[],
      firstChild: null,
      lastChild: null,
      nextSibling: null,
      previousSibling: null,
      attributes: new Map<string, string>(),
      // Real DOM semantics: a scrollTop write clamps to the scrollable range.
      get scrollTop() {
        return scrollTopValue;
      },
      set scrollTop(next: number) {
        const max = Math.max(0, (node.scrollHeight || 0) - (node.clientHeight || 0));
        scrollTopValue = Math.max(0, Math.min(Number.isFinite(next) ? next : 0, max));
      },
      scrollHeight: 0,
      clientHeight: 0,
      value: '',
      type: '',
      selected: false,
      get options() {
        return node.querySelectorAll('option');
      },
      selectedIndex: 0,
      listeners: new Map(),
      text: '',
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
        if (value !== '') node.appendChild((owner as MiniDoc).createTextNode(value));
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
        if (text !== '') node.appendChild((owner as MiniDoc).createTextNode(text));
      },
      get innerHTML(): string {
        return node.childNodes.map((child) => (child as MiniNode).textContent).join('');
      },
      get className() {
        return node.getAttribute('class') ?? '';
      },
      set className(value: string) {
        node.setAttribute('class', value);
      },
      classList: {
        contains(name: string) {
          return node.className.split(/\s+/).includes(name);
        },
        add(...names: string[]) {
          const classes = node.className.split(/\s+/).filter((token) => token !== '');
          for (const name of names) {
            if (name !== '' && !classes.includes(name)) classes.push(name);
          }
          node.className = classes.join(' ');
        },
        remove(...names: string[]) {
          const drop = new Set(names);
          node.className = node.className
            .split(/\s+/)
            .filter((token) => token !== '' && !drop.has(token))
            .join(' ');
        },
        toggle(name: string, force?: boolean) {
          const has = node.classList.contains(name);
          const next = force ?? !has;
          if (next) node.classList.add(name);
          else node.classList.remove(name);
          return next;
        },
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
        const text = String(value);
        node.attributes.set(name, text);
        if (name === 'type') node.type = text;
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
      addEventListener(type: string, fn: Listener, options?: boolean | { capture?: boolean }) {
        const capture = options === true || (typeof options === 'object' && options.capture === true);
        const key = capture ? `${type}-capture` : type;
        const set = node.listeners.get(key) ?? new Set();
        set.add(fn);
        node.listeners.set(key, set);
      },
      removeEventListener(type: string, fn: Listener, options?: boolean | { capture?: boolean }) {
        const capture = options === true || (typeof options === 'object' && options.capture === true);
        const key = capture ? `${type}-capture` : type;
        node.listeners.get(key)?.delete(fn);
      },
      dispatchEvent(raw: { type: string; target?: MiniNode } & Record<string, unknown>) {
        const payload: Record<string, unknown> = {
          type: raw.type,
          target: raw.target ?? node,
          currentTarget: null,
          bubbles: true,
          cancelable: true,
          defaultPrevented: false,
          button: 0,
          preventDefault() { payload.defaultPrevented = true; },
          stopPropagation() { stopped = true; },
          stopImmediatePropagation() { stopped = true; },
          ...raw,
        };
        let stopped = false;
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
          for (const fn of [...set]) fn(payload);
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
      /** Nearest self-or-ancestor element matching `selector` — the lookup the
       *  shared markdown click dispatcher resolves its rendered owner with
       *  (`closest('[data-session-id]')`, the chip, the fence, 'a'). */
      closest(selector: string) {
        let current: MiniNode | null = node;
        while (current !== null && current.nodeType === 1) {
          if (matches(current, selector)) return current;
          current = current.parentNode as MiniNode | null;
        }
        return null;
      },
      querySelector(selector: string) {
        return node.querySelectorAll(selector)[0] ?? null;
      },
      querySelectorAll(selector: string) {
        const out: MiniNode[] = [];
        // Descendant combinator (`.a .b button`): the LAST part must match
        // the node and the earlier parts must match ancestors, outer first.
        // Splitting is quote-aware: spaces inside `[title="..."]` stay in
        // their part.
        const parts: string[] = [];
        let part = '';
        let quote: string | null = null;
        for (const ch of selector.trim()) {
          if (quote !== null) {
            part += ch;
            if (ch === quote) quote = null;
            continue;
          }
          if (ch === '"' || ch === "'") {
            quote = ch;
            part += ch;
            continue;
          }
          if (/\s/.test(ch)) {
            if (part !== '') {
              parts.push(part);
              part = '';
            }
            continue;
          }
          part += ch;
        }
        if (part !== '') parts.push(part);
        const ancestorsMatch = (start: MiniNode, chain: string[]): boolean => {
          let ancestor = start.parentNode as MiniNode | null;
          for (let index = chain.length - 1; index >= 0; index -= 1) {
            while (ancestor !== null && ancestor.nodeType === 1 && !matches(ancestor, chain[index]!)) {
              ancestor = ancestor.parentNode as MiniNode | null;
            }
            if (ancestor === null || ancestor.nodeType !== 1) return false;
            ancestor = ancestor.parentNode as MiniNode | null;
          }
          return true;
        };
        const visit = (current: MiniNode) => {
          if (current.nodeType === 1
            && matches(current, parts[parts.length - 1]!)
            && (parts.length === 1 || ancestorsMatch(current, parts.slice(0, -1)))) {
            out.push(current);
          }
          for (const child of current.childNodes) visit(child);
        };
        for (const child of node.childNodes) visit(child);
        return out;
      },
    } as MiniNode;
    if (nodeName === 'OPTION') {
      Object.defineProperty(node, 'value', {
        get() {
          return node.getAttribute('value') ?? '';
        },
        set(v: string) {
          node.setAttribute('value', v);
        },
      });
      let selectedFlag = false;
      Object.defineProperty(node, 'selected', {
        get() {
          return selectedFlag;
        },
        set(next: boolean) {
          selectedFlag = !!next;
          if (!selectedFlag) return;
          const parent = node.parentNode;
          if (parent == null) return;
          const select = parent.nodeName === 'OPTGROUP' ? parent.parentNode : parent;
          if (select == null || select.nodeName !== 'SELECT') return;
          for (const option of select.querySelectorAll('option')) {
            if (option !== node) option.selected = false;
          }
        },
      });
    }
    if (nodeName === 'SELECT') {
      Object.defineProperty(node, 'options', {
        get() {
          return node.querySelectorAll('option');
        },
      });
      Object.defineProperty(node, 'value', {
        get() {
          const selected = node.querySelectorAll('option').find((o) => o.selected);
          return selected ? (selected.getAttribute('value') ?? '') : '';
        },
        set(v: string) {
          for (const option of node.querySelectorAll('option')) {
            option.selected = (option.getAttribute('value') ?? '') === v;
          }
        },
      });
    }
    return node;
  };

  const matches = (node: MiniNode, selector: string): boolean => {
    // Compound form (e.g. `button[title="Reload the diff snapshot"]`): match
    // the head part and the bracketed attribute part separately.
    const bracket = selector.indexOf('[');
    if (bracket > 0) {
      return matches(node, selector.slice(0, bracket).trim())
        && matches(node, selector.slice(bracket));
    }
    if (selector.includes('.')) {
      // Tag-plus-class compound (e.g. `button.code-review__close`): the tag
      // head (when present) and the class tail must both match.
      // Bracketed selectors never split here: their values may contain dots
      // (`[data-file-path="b.txt"]`), and the bracket branch owns them.
      const firstDot = selector.indexOf('.');
      const firstBracket = selector.indexOf('[');
      if (firstDot > 0 && (firstBracket < 0 || firstDot < firstBracket)) {
        const head = selector.slice(0, firstDot).trim();
        if (head !== '' && !matches(node, head)) return false;
        return matches(node, selector.slice(firstDot));
      }
    }
    if (selector.startsWith('.')) {
      const classes = node.className.split(/\s+/);
      return selector.slice(1).split('.').every((name) => classes.includes(name));
    }
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

  const document = makeNode(9, '#document', null) as MiniDoc;
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
  document.oninput = null;

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
    setInterval: fakeSetInterval,
    clearInterval: fakeClear,
    setTimeout: fakeSetTimeout,
    clearTimeout: fakeClear,
    requestAnimationFrame: (fn: () => void) => fakeSetTimeout(fn, 0) as number,
    cancelAnimationFrame: (id: unknown) => fakeClear(id),
    getComputedStyle: () => ({ lineHeight: '20px', paddingTop: '0px', paddingBottom: '0px' }),
    matchMedia: () => ({
      matches: false,
      addEventListener() {},
      removeEventListener() {},
    }),
    localStorage: {
      store: new Map<string, string>(),
      getItem(key: string) {
        return this.store.has(key) ? this.store.get(key)! : null;
      },
      setItem(key: string, value: string) {
        this.store.set(key, String(value));
      },
      removeItem(key: string) {
        this.store.delete(key);
      },
      clear() {
        this.store.clear();
      },
    },
    addEventListener() {},
    removeEventListener() {},
  };
  document.defaultView = windowObj;
  return { window: windowObj, document };
}

let mini: { window: MiniWindow; document: MiniDoc };
function installGlobals(): void {
  mini = createMiniDom();
  Object.defineProperty(globalThis, 'window', { configurable: true, writable: true, value: mini.window });
  Object.defineProperty(globalThis, 'document', { configurable: true, writable: true, value: mini.document });
  Object.defineProperty(globalThis, 'navigator', { configurable: true, writable: true, value: { userAgent: 'mini-dom' } });
  globalThis.HTMLElement = mini.window.HTMLElement as unknown as typeof HTMLElement;
  globalThis.SVGElement = mini.window.SVGElement as unknown as typeof SVGElement;
  globalThis.Element = mini.window.Element as unknown as typeof Element;
  globalThis.Node = mini.window.Node as unknown as typeof Node;
  globalThis.Text = mini.window.Text as unknown as typeof Text;
  globalThis.requestAnimationFrame = mini.window.requestAnimationFrame as unknown as typeof requestAnimationFrame;
  globalThis.cancelAnimationFrame = mini.window.cancelAnimationFrame as unknown as typeof cancelAnimationFrame;
  Object.defineProperty(globalThis, 'ResizeObserver', {
    configurable: true,
    writable: true,
    value: class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  });
  (globalThis as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
}

interface SentCommand {
  type: string;
  command: Record<string, unknown>;
}

interface DeferredCommand {
  type: string;
  command: Record<string, unknown>;
  resolve: (data: unknown) => void;
  reject: (error: Error) => void;
  settled: boolean;
}

let sent: SentCommand[] = [];
let deferreds: DeferredCommand[] = [];
let toasts: Array<{ message: string; error: boolean }> = [];

function harnessSendCommand(command: Record<string, unknown>): Promise<unknown> {
  const type = typeof command.type === 'string' ? command.type : 'unknown';
  sent.push({ type, command });
  const { promise, resolve, reject } = Promise.withResolvers<unknown>();
  deferreds.push({ type, command, resolve, reject, settled: false });
  return promise;
}

function countType(type: string): number {
  return sent.filter((entry) => entry.type === type).length;
}

function typedNotOpenError(): Error {
  return Object.assign(new Error('no open code review; call code_review_open first'), {
    rpc: true,
    rpcCode: 'code_review_not_open',
  });
}

function toastMentionsNoOpenReview(): boolean {
  return toasts.some((entry) => entry.message.includes('no open code review'));
}

function rejectUnsettledSnapshotsAsNotOpen(): void {
  for (const entry of deferreds) {
    if (entry.type !== 'code_review_snapshot' || entry.settled) continue;
    entry.settled = true;
    entry.reject(typedNotOpenError());
  }
}

function commandsFor(type: string, path: string): SentCommand[] {
  return sent.filter((entry) => entry.type === type && entry.command.path === path);
}

function selectedOption(select: MiniNode): MiniNode | null {
  return ((select.options as MiniNode[]) ?? []).find((option) => option.selected) ?? null;
}

function elementChildren(node: MiniNode): MiniNode[] {
  return node.childNodes.filter((child) => child.nodeType === 1);
}

function panelButtons(): MiniNode[] {
  return mini.document.body.querySelectorAll('button') as MiniNode[];
}

function autoReviewActionButton(): MiniNode | null {
  return panelButtons().find((button) => {
    const text = button.textContent ?? '';
    const title = button.getAttribute('title') ?? '';
    const label = button.getAttribute('aria-label') ?? '';
    if (text.includes('Auto Review')) return true;
    return text.includes('■ Stop') && (title.includes('Auto') || label.includes('Auto'));
  }) ?? null;
}

function highlightActionButton(): MiniNode | null {
  return panelButtons().find((button) => {
    const text = button.textContent ?? '';
    const title = button.getAttribute('title') ?? '';
    const label = button.getAttribute('aria-label') ?? '';
    if (text.includes('Highlight') && !text.includes('Key changes')) return true;
    return text.includes('■ Stop') && (title.includes('Highlight') || label.includes('Highlight'));
  }) ?? null;
}

function headerRefreshButton(): MiniNode | null {
  return (mini.document.body.querySelectorAll('button.code-review__action') as MiniNode[])
    .find((button) => {
      const text = button.textContent ?? '';
      return text.includes('Refresh') && !text.includes('Refreshing') && !text.includes('Refresh now');
    }) ?? null;
}

async function pressPanelEscape(): Promise<void> {
  const panel = mini.document.body.querySelector('#code-review-panel') as MiniNode | null;
  if (!panel) throw new Error('missing #code-review-panel');
  await act(async () => {
    panel.dispatchEvent({ type: 'keydown', key: 'Escape' });
    await realTick();
  });
}
function settleNext(type: string, data: unknown): boolean {
  const deferred = deferreds.find((entry) => entry.type === type && !entry.settled);
  if (!deferred) return false;
  deferred.settled = true;
  deferred.resolve(data);
  return true;
}

/** SHA-256 over the exact complete unified hunk bytes HunkIdentity::new hashes
 *  (header, then '\n' + kind prefix + raw text per line); 64 lowercase hex. */
const UNIFIED_PREFIX: Record<string, string> = {
  context: ' ',
  addition: '+',
  deletion: '-',
  meta: '',
};
const unifiedHunkSha256 = (
  header: string,
  lines: Array<{ kind: string; text: string }>,
): string =>
  createHash('sha256')
    .update(
      header
        + lines.map((line) => `\n${UNIFIED_PREFIX[line.kind] ?? ' '}${line.text}`).join(''),
    )
    .digest('hex');

const singleLine = (text: string): Array<{ kind: string; newNo: number; text: string }> => [
  { kind: 'addition', newNo: 1, text },
];

const onDemandFile = (path: string, hash: string, insertions = 1) => ({
  path,
  status: 'added',
  binary: false,
  insertions,
  deletions: 0,
  bodyState: 'onDemand',
  truncated: false,
  hunks: [],
  contentHash: hash,
});

const aFreshHash = unifiedHunkSha256('@@ -0,0 +1 @@', singleLine('+FRESH-APPEND'));
const bSeedHash = unifiedHunkSha256('@@ -0,0 +1 @@', singleLine('+B-APPEND'));

const snapshotPayload = () => ({
  comparisonLabel: 'HEAD→working tree',
  snapshotId: 'snap-1',
  reviewId: 'rev-1',
  generation: 1,
  revision: 1,
  truncated: false,
  error: null,
  totalInsertions: 2,
  totalDeletions: 0,
  totalsComplete: true,
  untrackedNotShown: 0,
  files: [onDemandFile('a.txt', aFreshHash), onDemandFile('b.txt', bSeedHash)],
  threads: [],
  isStreaming: false,
  activeCount: 0,
  internalBusy: false,
  autoReviewState: 'idle',
  autoReviewCounts: { submitted: 0, completed: 0, failed: 0 },
  autoReviewBatchId: 1,
  highlightState: 'idle',
  highlightGeneration: 1,
  tldrState: 'idle',
  tldrGeneration: 0,
  tldrSnapshotId: null,
  tldrError: null,
  tldrMarkdown: '',
});

const pagePayload = (path: string, lineText: string) => {
  const lines = singleLine(lineText);
  return {
    snapshotId: 'snap-1',
    path,
    status: 'added',
    binary: false,
    cursor: 0,
    hasMore: false,
    totalLines: 1,
    hunkCount: 1,
    hunks: [
      {
        index: 0,
        header: '@@ -0,0 +1 @@',
        oldStart: 0,
        oldCount: 0,
        newStart: 1,
        newCount: 1,
        contentHash: unifiedHunkSha256('@@ -0,0 +1 @@', lines),
        totalLines: 1,
        complete: true,
        lineStart: 0,
        lines,
      },
    ],
  };
};

const navigationPagePayload = (path: string) => {
  const first = pagePayload(path, 'NAV-FIRST');
  const secondLines = [{ kind: 'addition' as const, newNo: 20, text: 'NAV-SECOND' }];
  return {
    ...first,
    totalLines: 2,
    hunkCount: 2,
    hunks: [
      first.hunks[0],
      {
        index: 1,
        header: '@@ -19,0 +20 @@',
        oldStart: 19,
        oldCount: 0,
        newStart: 20,
        newCount: 1,
        contentHash: unifiedHunkSha256('@@ -19,0 +20 @@', secondLines),
        totalLines: 1,
        complete: true,
        lineStart: 1,
        lines: secondLines,
      },
    ],
  };
};

async function scrollDiffToBottom(): Promise<void> {
  const viewport = mini.document.body.querySelector('.code-review__diff');
  if (!viewport) throw new Error('missing .code-review__diff viewport');
  viewport.scrollHeight = 100;
  viewport.clientHeight = 40;
  viewport.scrollTop = 70; // 100 - 70 - 40 = -10 ≤ threshold → near bottom
  await act(async () => {
    viewport.dispatchEvent({ type: 'scroll' });
    await realTick();
  });
}


async function clickFileRow(path: string): Promise<void> {
  const row = mini.document.body.querySelector(`[data-file-path="${path}"]`);
  if (!row) throw new Error(`missing file row for ${path}`);
  await act(async () => {
    row.click();
    await realTick();
  });
}


let container: MiniNode;
let root: Root | null = null;
let closeCalls = 0;
async function mountPanel(
  snapshot: () => unknown = snapshotPayload,
  firstPath: string | null = 'a.txt',
  extraProps: Record<string, unknown> = {},
): Promise<void> {
  sent = [];
  deferreds = [];
  toasts = [];
  closeCalls = 0;
  container = mini.document.createElement('div');
  mini.document.body.appendChild(container);
  root = createRoot(container as unknown as Element);
  await act(async () => {
    root!.render(
      createElement(CodeReviewPanel, {
        sendCommand: harnessSendCommand,
        sessionId: 'sess-cr',
        openArgs: {},
        connectionEpoch: 1,
        liveSettings: { sttConfigured: true },
        onToast: (message: string, error: boolean) => {
          toasts.push({ message, error });
        },
        onClose: () => {
          closeCalls += 1;
        },
        ...extraProps,
      }),
    );
    await realTick();
  });
  check('boot sends exactly one code_review_open', countType('code_review_open') === 1, String(countType('code_review_open')));
  const entryLoader = mini.document.body.querySelector('.code-review__loader');
  check(
    'pending open renders the dedicated entry loader without an action spinner',
    entryLoader !== null
      && (entryLoader as MiniNode).getAttribute('aria-label') === 'Loading diff'
      && (entryLoader as MiniNode).querySelector('.code-review__spinner') === null,
  );
  check('open request settles', settleNext('code_review_open', snapshot()));
  await act(async () => {
    await realTick();
  });
  check('post-open snapshot poll settles', settleNext('code_review_snapshot', snapshot()));
  await act(async () => {
    await realTick();
  });
  check(
    'auto-selected first file eager-fetches one page',
    firstPath === null
      || (countType('code_review_file_diff') === 1
        && commandsFor('code_review_file_diff', firstPath).length === 1
        && (commandsFor('code_review_file_diff', firstPath)[0]?.command.cursor ?? -1) === 0),
    JSON.stringify(sent),
  );
}

async function rerenderPanel(extraProps: Record<string, unknown>): Promise<void> {
  if (!root) return;
  await act(async () => {
    root!.render(
      createElement(CodeReviewPanel, {
        sendCommand: harnessSendCommand,
        sessionId: 'sess-cr',
        openArgs: {},
        connectionEpoch: 1,
        liveSettings: { sttConfigured: true },
        onToast: (message: string, error: boolean) => {
          toasts.push({ message, error });
        },
        onClose: () => {
          closeCalls += 1;
        },
        ...extraProps,
      }),
    );
    await realTick();
  });
}

async function unmountPanel(): Promise<void> {
  if (!root) return;
  await act(async () => {
    root!.unmount();
  });
  container.remove();
  root = null;
}

const retainedReconnectOpenArgs = {
  from: 'origin/mainnet-beta',
  includeWorkingTree: true as const,
  alreadyOpen: true as const,
};

async function mountAlreadyOpenPanel(
  openArgs: Record<string, unknown> = retainedReconnectOpenArgs,
  snapshot: () => unknown = snapshotPayload,
  firstPath: string | null = 'a.txt',
): Promise<void> {
  sent = [];
  deferreds = [];
  toasts = [];
  closeCalls = 0;
  container = mini.document.createElement('div');
  mini.document.body.appendChild(container);
  root = createRoot(container as unknown as Element);
  await act(async () => {
    root!.render(
      createElement(CodeReviewPanel, {
        sendCommand: harnessSendCommand,
        sessionId: 'sess-cr',
        openArgs,
        connectionEpoch: 1,
        liveSettings: { sttConfigured: true },
        onToast: (message: string, error: boolean) => {
          toasts.push({ message, error });
        },
        onClose: () => {
          closeCalls += 1;
        },
      }),
    );
    await realTick();
  });
  check(
    'alreadyOpen boot snapshots instead of opening',
    countType('code_review_open') === 0 && countType('code_review_snapshot') >= 1,
    String(countType('code_review_open')),
  );
  check('alreadyOpen snapshot settles', settleNext('code_review_snapshot', snapshot()));
  await act(async () => { await realTick(); });
  check('alreadyOpen poll snapshot settles', settleNext('code_review_snapshot', snapshot()));
  await act(async () => { await realTick(); });
  check(
    'alreadyOpen auto-selected first file eager-fetches one page',
    firstPath === null
      || (countType('code_review_file_diff') === 1
        && commandsFor('code_review_file_diff', firstPath).length === 1
        && (commandsFor('code_review_file_diff', firstPath)[0]?.command.cursor ?? -1) === 0),
    JSON.stringify(sent),
  );
}

installGlobals();

const react = await import('react');
const reactDomClient = await import('react-dom/client');
act = react.act as Act;
createElement = react.createElement as CreateElement;
createRoot = reactDomClient.createRoot as CreateRoot;
const panelModule = await import('../src/panels/CodeReviewPanel.tsx');
CodeReviewPanel = panelModule.CodeReviewPanel as CodeReviewPanelComponent;
// Dynamic on purpose: every src module here binds globals (document, window)
// at load, so it must be evaluated AFTER installGlobals() put the mini DOM in
// place — a static import would run at bundle init, when no document exists.
const codeReviewModule = await import('../src/codeReview.ts');
const estimateLoadedDiffBytes = codeReviewModule.estimateLoadedDiffBytes;
const cacheCap = codeReviewModule.LOADED_DIFF_CACHE_MAX_BYTES;
const markdownModule = await import('../src/markdown.ts');
const dispatchMarkdownPathClick = markdownModule.dispatchMarkdownPathClick;
// Dynamic on purpose (same exception as the panel module above): both modules
// bind document/window at load, so a static import would evaluate before
// installGlobals() places the mini DOM.
const commitPickerModule = await import('../src/panels/CommitPicker.tsx');
const CommitPicker = commitPickerModule.CommitPicker as CommitPickerComponent;
const normalizeCodeReviewCommits = codeReviewModule.normalizeCodeReviewCommits;

{
  await mountPanel();

  check(
    'file list renders the B row',
    mini.document.body.querySelector('[data-file-path="b.txt"]') !== null,
  );
  await clickFileRow('b.txt');
  check(
    'selecting B eager-fetches exactly one B page',
    commandsFor('code_review_file_diff', 'b.txt').length === 1
      && (commandsFor('code_review_file_diff', 'b.txt')[0]?.command.cursor ?? -1) === 0,
  );

  check(
    'file list still renders the A row',
    mini.document.body.querySelector('[data-file-path="a.txt"]') !== null,
  );
  await clickFileRow('a.txt');
  const pendingA = deferreds.filter(
    (entry) => entry.type === 'code_review_file_diff'
      && entry.command.path === 'a.txt'
      && !entry.settled,
  );
  const staleFirst = pendingA[0];
  const freshA = pendingA[pendingA.length - 1];
  check(
    're-selecting A eagerly launches a fresh A request automatically',
    commandsFor('code_review_file_diff', 'a.txt').length === 2
      && pendingA.length === 2
      && staleFirst !== freshA,
    String(pendingA.length),
  );
  check(
    'the fresh A generation request starts at cursor 0 of the SAME snapshot',
    freshA !== undefined
      && freshA.command.snapshotId === 'snap-1'
      && (freshA.command.cursor ?? -1) === 0,
    JSON.stringify(freshA?.command ?? {}),
  );
  check('the stale first-A deferred is still pending', staleFirst !== undefined && !staleFirst.settled);

  if (staleFirst) {
    staleFirst.resolve(pagePayload('a.txt', '+STALE-APPEND'));
  }
  await act(async () => {
    await realTick();
  });
  check(
    'the resolved stale response must NOT paint or append its lines',
    !mini.document.body.textContent.includes('STALE-APPEND'),
    mini.document.body.textContent.slice(0, 200),
  );
  check(
    'the resolved stale response must not append or re-issue any request',
    commandsFor('code_review_file_diff', 'a.txt').length === 2
      && commandsFor('code_review_file_diff', 'a.txt').every(
        (entry) => (entry.command.cursor ?? -1) === 0,
      ),
    String(commandsFor('code_review_file_diff', 'a.txt').length),
  );
  check(
    'the fresh A deferred is still pending after the stale discard',
    freshA !== undefined && !freshA.settled,
  );

  if (freshA) {
    freshA.settled = true;
    freshA.resolve(pagePayload('a.txt', '+FRESH-APPEND'));
  }
  await act(async () => {
    await realTick();
  });
  await scrollDiffToBottom();
  check(
    'the fresh A generation response paints its lines',
    mini.document.body.textContent.includes('FRESH-APPEND'),
    mini.document.body.textContent.slice(0, 200),
  );

  await act(async () => {
    advanceClock(1500);
    await realTick();
  });
  check('the poll tick settles', settleNext('code_review_snapshot', snapshotPayload()));
  await act(async () => {
    await realTick();
  });
  check(
    'the poll neither repaints the stale page nor re-requests',
    !mini.document.body.textContent.includes('STALE-APPEND')
      && commandsFor('code_review_file_diff', 'a.txt').length === 2,
    JSON.stringify({
      text: mini.document.body.textContent.slice(0, 120),
      aRequests: commandsFor('code_review_file_diff', 'a.txt').length,
    }),
  );
  check(
    'the fresh A chain is exactly the two automatic requests (stale + eager fresh, no scroll)',
    commandsFor('code_review_file_diff', 'a.txt').length === 2
      && commandsFor('code_review_file_diff', 'a.txt').every(
        (entry) => entry.command.snapshotId === 'snap-1' && (entry.command.cursor ?? -1) === 0,
      ),
    String(commandsFor('code_review_file_diff', 'a.txt').length),
  );
  check(
    'no transport-stall or inline error rendered by the stale discard',
    !mini.document.body.textContent.includes('Retry'),
    mini.document.body.textContent.slice(0, 200),
  );

  await unmountPanel();
}

{
  await mountPanel();
  check(
    'two-hunk diff settles for navigation',
    settleNext('code_review_file_diff', navigationPagePayload('a.txt')),
  );
  await act(async () => {
    await realTick();
  });
  const navigation = () => mini.document.body.querySelector('.code-review__hunk-nav');
  const navigationButton = (label: string) =>
    navigation()?.querySelector(`button[aria-label="${label}"]`) as MiniNode | null;
  const navigationPosition = () =>
    navigation()?.querySelector('.code-review__hunk-nav-position')?.textContent ?? '';
  const selectedHeader = () =>
    mini.document.body.querySelector('.code-review__hunk-header[aria-pressed="true"]');

  check(
    'hunk navigation starts before the first hunk with only Next enabled',
    navigationPosition() === '0 / 2'
      && navigationButton('Previous hunk')?.getAttribute('disabled') !== null
      && navigationButton('Next hunk')?.getAttribute('disabled') === null,
  );
  await act(async () => {
    navigationButton('Next hunk')?.click();
    advanceClock(1);
    await realTick();
  });
  check(
    'Next selects the first hunk and advances the boundary state',
    navigationPosition() === '1 / 2'
      && selectedHeader()?.textContent.includes('@@ -0,0 +1 @@') === true
      && navigationButton('Previous hunk')?.getAttribute('disabled') !== null
      && navigationButton('Next hunk')?.getAttribute('disabled') === null,
  );
  await act(async () => {
    navigationButton('Next hunk')?.click();
    advanceClock(1);
    await realTick();
  });
  check(
    'Next selects the second hunk and disables the terminal direction',
    navigationPosition() === '2 / 2'
      && selectedHeader()?.textContent.includes('@@ -19,0 +20 @@') === true
      && navigationButton('Previous hunk')?.getAttribute('disabled') === null
      && navigationButton('Next hunk')?.getAttribute('disabled') !== null,
  );
  await act(async () => {
    navigationButton('Previous hunk')?.click();
    advanceClock(1);
    await realTick();
  });
  check(
    'Previous returns to the first hunk',
    navigationPosition() === '1 / 2'
      && selectedHeader()?.textContent.includes('@@ -0,0 +1 @@') === true,
  );
  await unmountPanel();
}

{
  const FILLER_PAD_CHARS = 8175;
  const FILLER_LINES = 985;
  const M_PAD_CHARS = 4080;
  const M_TOTAL_LINES = 508;
  interface FixtureLine {
    kind: 'addition';
    newNo: number;
    text: string;
  }
  interface FixturePageHunk {
    index: number;
    header: string;
    oldStart: number;
    oldCount: number;
    newStart: number;
    newCount: number;
    contentHash: string;
    totalLines: number;
    complete: boolean;
    lineStart: number;
    lines: FixtureLine[];
  }
  interface FixturePage {
    snapshotId: string;
    path: string;
    status: string;
    binary: boolean;
    cursor: number;
    nextCursor?: number;
    hasMore: boolean;
    byteCapped: boolean;
    totalLines: number;
    hunkCount: number;
    hunks: FixturePageHunk[];
  }
  const textEncoder = new TextEncoder();
  const utf8Bytes = (text: string): number => textEncoder.encode(text).length;
  const jsonEscapeCost = (byte: number): number =>
    byte === 0x22 || byte === 0x5c || byte === 0x08 || byte === 0x09 || byte === 0x0a || byte === 0x0c || byte === 0x0d
      ? 2
      : byte < 0x20
        ? 6
        : 1;
  const jsonEscapedLen = (text: string): number => {
    let total = 0;
    for (const byte of textEncoder.encode(text)) total += jsonEscapeCost(byte);
    return total;
  };
  const fillerLine = (path: string, index: number): FixtureLine => ({
    kind: 'addition',
    newNo: index + 1,
    text: `+H-${path}-${index}${'y'.repeat(FILLER_PAD_CHARS)}`,
  });
  const mLine = (index: number): FixtureLine => ({
    kind: 'addition',
    newNo: index + 1,
    text: `+m-${index}${'y'.repeat(M_PAD_CHARS)}`,
  });
  const PAGE_JSON_OVERHEAD = 1024;
  const LINE_JSON_OVERHEAD = 64;
  const HUNK_DESCRIPTOR = 640;
  function slicePages(
    path: string,
    lines: FixtureLine[],
  ): { pages: FixturePage[]; wireBytes: number[]; hash: string } {
    const total = lines.length;
    const header = `@@ -0,0 +1,${total} @@`;
    const hash = unifiedHunkSha256(header, lines);
    const pages: FixturePage[] = [];
    const wireBytes: number[] = [];
    let cursor = 0;
    while (cursor < total) {
      let pageLines = 0;
      let pageBytes = PAGE_JSON_OVERHEAD + 6 * (utf8Bytes('snap-1') + utf8Bytes(path));
      let take = 0;
      while (take < total - cursor && pageLines + take < 1000) {
        const lineBytes = jsonEscapedLen(lines[cursor + take].text) + LINE_JSON_OVERHEAD;
        const descriptor = take === 0 ? HUNK_DESCRIPTOR : 0;
        if (pageLines + take > 0 && pageBytes + descriptor + lineBytes > 1024 * 1024) break;
        pageBytes += descriptor + lineBytes;
        take += 1;
      }
      if (take === 0) throw new Error(`slicePages(${path}): production always takes the page first line`);
      const next = cursor + take;
      const hasMore = next < total;
      const page: FixturePage = {
        snapshotId: 'snap-1',
        path,
        status: 'added',
        binary: false,
        cursor,
        hasMore,
        byteCapped: false,
        totalLines: total,
        hunkCount: 1,
        hunks: [
          {
            index: 0,
            header,
            oldStart: 0,
            oldCount: 0,
            newStart: 1,
            newCount: total,
            contentHash: hash,
            totalLines: total,
            complete: true,
            lineStart: cursor,
            lines: lines.slice(cursor, next),
          },
        ],
      };
      if (hasMore) page.nextCursor = next;
      pages.push(page);
      wireBytes.push(pageBytes);
      cursor = next;
    }
    return { pages, wireBytes, hash };
  }
  const fillerLines = (path: string): FixtureLine[] =>
    Array.from({ length: FILLER_LINES }, (_, i) => fillerLine(path, i));
  const mLines = (): FixtureLine[] =>
    Array.from({ length: M_TOTAL_LINES }, (_, i) => ({
      kind: 'addition' as const,
      newNo: i + 1,
      text: `+m-${i}${'y'.repeat(M_PAD_CHARS)}`,
    }));
  const heavy1Stream = slicePages('heavy1.txt', fillerLines('heavy1.txt'));
  const heavy2Stream = slicePages('heavy2.txt', fillerLines('heavy2.txt'));
  const mStream = slicePages('m.txt', mLines());
  const multiSnapshot = () => ({
    ...snapshotPayload(),
    totalInsertions: 2478,
    totalDeletions: 0,
    files: [
      onDemandFile('heavy1.txt', heavy1Stream.hash, FILLER_LINES),
      onDemandFile('heavy2.txt', heavy2Stream.hash, FILLER_LINES),
      onDemandFile('m.txt', mStream.hash, M_TOTAL_LINES),
    ],
  });

  async function settlePageStream(
    path: string,
    pages: FixturePage[],
    firstRequestIndex = 0,
  ): Promise<void> {
    for (let i = 0; i < pages.length; i++) {
      const requests = commandsFor('code_review_file_diff', path);
      check(
        `${path} request ${firstRequestIndex + i} carries exactly the production page cursor`,
        requests.length === firstRequestIndex + i + 1
          && (requests[firstRequestIndex + i]?.command.cursor ?? -1) === pages[i].cursor,
        JSON.stringify(requests.map((entry) => entry.command.cursor)),
      );
      check(`${path} page ${pages[i].cursor} settles`, settleNext('code_review_file_diff', pages[i]));
      await act(async () => {
        await realTick();
      });
    }
  }

  const streams = [heavy1Stream, heavy2Stream, mStream];
  check(
    'every fixture page is a production slice (≤1000 lines, ≤1 MiB incl. descriptor, exact cursor chain)',
    streams.every(({ pages, wireBytes }) =>
      pages.every((page, i) =>
        page.hunks[0].lines.length <= 1000
        && wireBytes[i] <= 1024 * 1024
        && page.hasMore === (i < pages.length - 1)
        && (page.hasMore ? page.nextCursor === pages[i + 1].cursor : !('nextCursor' in page)))),
    JSON.stringify(streams.map(({ pages }) => pages.map((p) => p.cursor))),
  );
  check(
    'the fixture pages are byte-bound (the 1 MiB accumulator — not 1000 lines — ends each page)',
    heavy1Stream.pages.length === 8
      && heavy1Stream.pages[0].hunks[0].lines.length === 126
      && heavy1Stream.pages[7].hunks[0].lines.length === FILLER_LINES - 7 * 126
      && mStream.pages.length === 3
      && mStream.pages[0].hunks[0].lines.length === 252
      && mStream.pages[1].hunks[0].lines.length === 252,
      JSON.stringify(streams.map(({ pages }) => pages.map((p) => p.hunks[0].lines.length))),
  );

  const seedCoherent = [
    { stream: heavy1Stream, total: FILLER_LINES },
    { stream: heavy2Stream, total: FILLER_LINES },
    { stream: mStream, total: M_TOTAL_LINES },
  ].every(({ stream, total }) => {
    if (!/^[0-9a-f]{64}$/.test(stream.hash)) return false;
    const header = `@@ -0,0 +1,${total} @@`;
    return stream.pages.every((page) => {
      const hunk = page.hunks[0];
      return (
        page.status === 'added'
        && !page.binary
        && page.hunkCount === 1
        && hunk.index === 0
        && page.totalLines === total
        && hunk.contentHash === stream.hash
        && hunk.totalLines === total
        && hunk.complete
        && hunk.lineStart === page.cursor
        && hunk.lines.every(
          (line, i) => line.kind === 'addition' && line.newNo === page.cursor + i + 1,
        )
      );
    })
    && unifiedHunkSha256(
      header,
      stream.pages.flatMap((page) => page.hunks[0].lines),
    ) === stream.hash;
  });
  check(
    'seed and page hunk identities are one production hash with coherent added-file ranges/status/totals and explicit index 0',
    seedCoherent,
    JSON.stringify(streams.map(({ hash }) => hash)),
  );
  const multi = multiSnapshot();
  check(
    'the snapshot seed matches the streamed additions (985/985/508, total 2478)',
    multi.totalInsertions === 2478
      && multi.totalDeletions === 0
      && multi.files.length === 3
      && multi.files.every((file) => file.status === 'added' && file.deletions === 0)
      && multi.files[0].insertions === FILLER_LINES
      && multi.files[1].insertions === FILLER_LINES
      && multi.files[2].insertions === M_TOTAL_LINES
      && multi.files[0].contentHash === heavy1Stream.hash
      && multi.files[1].contentHash === heavy2Stream.hash
      && multi.files[2].contentHash === mStream.hash,
    JSON.stringify({
      totalInsertions: multi.totalInsertions,
      insertions: multi.files.map((file) => file.insertions),
    }),
  );

  const weightOf = (path: string, hash: string, lines: FixtureLine[], totalLines: number): number =>
    estimateLoadedDiffBytes({
      snapshotId: 'snap-1',
      path,
      hunks: lines.length === 0 ? [] : [
        {
          index: 0,
          header: `@@ -0,0 +1,${totalLines} @@`,
          oldStart: 0,
          oldCount: 0,
          newStart: 1,
          newCount: totalLines,
          contentHash: hash,
          totalLines,
          complete: true,
          lineStart: 0,
          lines,
        },
      ],
      lines: [],
      nextCursor: 0,
      hasMoreBackend: false,
      totalLines,
      backendTruncated: false,
      byteCapped: false,
      hunksComplete: true,
      loading: false,
      error: null,
    });
  const fillerWeight = weightOf('heavy1.txt', heavy1Stream.hash, fillerLines('heavy1.txt'), FILLER_LINES);
  const mSeedWeight = weightOf('m.txt', mStream.hash, [], 0);
  const mPage1Weight = weightOf('m.txt', mStream.hash, mStream.pages[0].hunks[0].lines, M_TOTAL_LINES);
  check(
    'both settled fillers + the m.txt seed fit the 32 MiB cache cap (no premature eviction)',
    2 * fillerWeight + mSeedWeight <= cacheCap,
    JSON.stringify({ fillerWeight, mSeedWeight, cacheCap }),
  );
  check(
    'the named nonterminal m page 1 crosses the 32 MiB cap when it lands (eviction is reachable)',
    2 * fillerWeight + mPage1Weight > cacheCap,
    JSON.stringify({
      mPage1Weight,
      atPage1: 2 * fillerWeight + mPage1Weight,
      cacheCap,
    }),
  );
  check(
    'the post-eviction pair (surviving filler + the landing page) fits the cap (exactly one eviction)',
    fillerWeight + mPage1Weight <= cacheCap,
    JSON.stringify({ afterEviction: fillerWeight + mPage1Weight, cacheCap }),
  );

  await mountPanel(multiSnapshot, 'heavy1.txt');

  await settlePageStream('heavy1.txt', heavy1Stream.pages);
  await clickFileRow('heavy2.txt');
  await settlePageStream('heavy2.txt', heavy2Stream.pages);

  await clickFileRow('m.txt');
  check(
    'selecting m.txt eagerly issues exactly one page request (cursor 0)',
    commandsFor('code_review_file_diff', 'm.txt').length === 1
      && (commandsFor('code_review_file_diff', 'm.txt')[0]?.command.cursor ?? -1) === 0,
    String(commandsFor('code_review_file_diff', 'm.txt').length),
  );

  check('m.txt page 1 settles', settleNext('code_review_file_diff', mStream.pages[0]));
  await act(async () => {
    await realTick();
  });
  let mRequests = commandsFor('code_review_file_diff', 'm.txt');
  check(
    'page 1 advances the selected fetch to page 2 without staling it',
    mRequests.length === 2
      && (mRequests[1]?.command.cursor ?? -1) === mStream.pages[1].cursor,
    JSON.stringify(mRequests.map((entry) => entry.command.cursor)),
  );
  check(
    'the evicted filler is not refetched by its own eviction (still its full settled stream)',
    commandsFor('code_review_file_diff', 'heavy1.txt').length === heavy1Stream.pages.length,
    String(commandsFor('code_review_file_diff', 'heavy1.txt').length),
  );

  check('m.txt page 2 settles', settleNext('code_review_file_diff', mStream.pages[1]));
  await act(async () => {
    await realTick();
  });
  mRequests = commandsFor('code_review_file_diff', 'm.txt');
  check('page 2 advances to the terminal page', mRequests.length === 3
    && (mRequests[2]?.command.cursor ?? -1) === mStream.pages[2].cursor,
  JSON.stringify(mRequests.map((entry) => entry.command.cursor)));
  check('m.txt terminal page settles', settleNext('code_review_file_diff', mStream.pages[2]));
  await act(async () => {
    await realTick();
  });
  check(
    'the selected fetch reached its terminal page (no further requests)',
    commandsFor('code_review_file_diff', 'm.txt').length === mStream.pages.length,
    String(commandsFor('code_review_file_diff', 'm.txt').length),
  );
  check(
    'page 1 of the selected multi-page fetch paints',
    mini.document.body.textContent.includes('m-0'),
    mini.document.body.textContent.slice(0, 200),
  );
  await scrollDiffToBottom();
  check(
    'page 2 (terminal page) paints its merged lines',
    mini.document.body.textContent.includes('m-254'),
    mini.document.body.textContent.slice(0, 200),
  );
  check(
    'no inline error was rendered for the mid-fetch eviction',
    !mini.document.body.textContent.includes('Retry'),
    mini.document.body.textContent.slice(0, 200),
  );

  await act(async () => {
    advanceClock(1500);
    await realTick();
  });
  check('the poll tick settles', settleNext('code_review_snapshot', multiSnapshot()));
  await act(async () => {
    await realTick();
  });
  await clickFileRow('heavy1.txt');
  const heavy1Refetch = commandsFor('code_review_file_diff', 'heavy1.txt').length;
  if (heavy1Refetch > heavy1Stream.pages.length) {
    await settlePageStream('heavy1.txt', heavy1Stream.pages, heavy1Stream.pages.length);
  }
  check(
    'reselecting the filler either retains its settled cache or refetches one exact stream',
    commandsFor('code_review_file_diff', 'heavy1.txt').length === heavy1Stream.pages.length
      || commandsFor('code_review_file_diff', 'heavy1.txt').length === 2 * heavy1Stream.pages.length,
    JSON.stringify(commandsFor('code_review_file_diff', 'heavy1.txt').map((entry) => entry.command.cursor)),
  );
  await clickFileRow('m.txt');
  check(
    're-selecting the completed selected path issues no further page requests',
    commandsFor('code_review_file_diff', 'm.txt').length === mStream.pages.length,
    String(commandsFor('code_review_file_diff', 'm.txt').length),
  );

  await unmountPanel();
}

// ---- drafts survive a refresh whose hunk changed; oversized comments never
// reach the wire ----
{
  await mountPanel();
  check('fresh page settles for the draft scenario', settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')));
  await act(async () => {
    await realTick();
  });
  const header = mini.document.body.querySelector('.code-review__hunk-header');
  check('hunk header renders for selection', header !== null);
  await act(async () => {
    (header as MiniNode).click();
    await realTick();
  });
  const composer = () =>
    mini.document.body.querySelector('.code-review__comment-input') as MiniNode | null;
  check('composer renders for the selected hunk', composer() !== null);
  const composerActions = mini.document.body.querySelector('.code-review__composer-actions') as MiniNode | null;
  const actionKids = composerActions ? elementChildren(composerActions) : [];
  const modelChrome = actionKids.find((child) => child.className.includes('code-review__model'));
  const attachChrome = actionKids.find((child) => child.className.includes('code-review__composer-btn--attach'));
  const micChrome = actionKids.find((child) => child.className.includes('code-review__composer-btn--mic'));
  const sendChrome = actionKids.find((child) => child.className.includes('code-review__action--send'));
  check(
    'composer order is attach then mic then Comment with no per-hunk model select',
    modelChrome == null
      && attachChrome != null
      && micChrome != null
      && sendChrome != null
      && actionKids.indexOf(attachChrome) < actionKids.indexOf(micChrome)
      && actionKids.indexOf(micChrome) < actionKids.indexOf(sendChrome),
    actionKids.map((child) => child.className).join('|'),
  );
  composer()!.value = 'changed hunk draft';

  const refreshButton = mini.document.body.querySelector(
    'button[title="Reload the diff snapshot"]',
  ) as MiniNode;
  await act(async () => {
    refreshButton.click();
    await realTick();
  });
  check(
    'refreshing with a live draft surfaces the confirmation',
    mini.document.body.querySelector('.code-review__confirm') !== null,
  );
  const refreshPrimary = mini.document.body.querySelector(
    '.code-review__confirm-actions button',
  ) as MiniNode;
  await act(async () => {
    refreshPrimary.click();
    await realTick();
  });
  check('confirmed refresh went to the wire', countType('code_review_refresh') === 1);

  const snap2 = () => {
    const base = snapshotPayload() as Record<string, unknown>;
    return { ...base, snapshotId: 'snap-2', reviewId: 'rev-2' };
  };
  check('refresh response settles', settleNext('code_review_refresh', snap2()));
  await act(async () => {
    await realTick();
  });
  const changedPage = { ...pagePayload('a.txt', '+CHANGED-APPEND'), snapshotId: 'snap-2' };
  check(
    'the refreshed changed hunk eagerly refetches',
    settleNext('code_review_file_diff', changedPage),
  );
  await act(async () => {
    await realTick();
  });

  const closeBtn = mini.document.body.querySelector('button.code-review__close') as MiniNode;
  await act(async () => {
    closeBtn.click();
    await realTick();
  });
  check(
    'a draft committed for a changed hunk is retained across the refresh',
    mini.document.body.querySelector('.code-review__confirm') !== null,
  );
  await unmountPanel();
}

{
  await mountPanel();
  check('fresh page settles for the cap scenario', settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')));
  await act(async () => {
    await realTick();
  });
  const capHeader = mini.document.body.querySelector('.code-review__hunk-header') as MiniNode;
  await act(async () => {
    capHeader.click();
    await realTick();
  });
  const capComposer = mini.document.body.querySelector(
    '.code-review__comment-input',
  ) as MiniNode;
  capComposer.value = '\u{e9}'.repeat(4200);
  const capButton = mini.document.body.querySelector(
    '.code-review__action--send',
  ) as MiniNode;
  await act(async () => {
    capButton.click();
    await realTick();
  });
  check(
    'the byte-cap rejection surfaces one App toast',
    toasts.length === 1 && toasts[0].message.includes('-byte limit') && toasts[0].error === true,
    JSON.stringify(toasts),
  );
  check('the byte-cap rejection renders no error banner', mini.document.body.querySelector('.code-review__error') === null);
  check(
    'the oversized draft is kept in the composer',
    (mini.document.body.querySelector('.code-review__comment-input') as MiniNode).value.length
      === 4200,
  );
  await unmountPanel();
}

// ---- composer parity (frozen contract): live thinking block, dead-air
// "Thinking…" shimmer, final assistant comment thinking, and the diff-pane
// streaming-dot phase label ----
{
  await mountPanel();
  check(
    'fresh page settles for the thinking scenario',
    settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')),
  );
  await act(async () => {
    await realTick();
  });
  const hunkHeader = mini.document.body.querySelector('.code-review__hunk-header') as MiniNode;
  check('hunk header renders for the thinking scenario', hunkHeader !== null);
  await act(async () => {
    hunkHeader.click();
    await realTick();
  });
  check(
    'thread dock opens for the selected hunk',
    mini.document.body.querySelector('.code-review__thread') !== null,
  );
  const threadViewport = mini.document.body.querySelector('.code-review__thread') as MiniNode;
  threadViewport.scrollHeight = 200;
  threadViewport.clientHeight = 50;
  threadViewport.scrollTop = 0;

  const threadIdentity = {
    snapshotId: 'snap-1',
    path: 'a.txt',
    oldStart: 0,
    oldCount: 0,
    newStart: 1,
    newCount: 1,
    contentHash: aFreshHash,
  };
  const liveSnap = (over: Record<string, unknown>) => {
    const base = snapshotPayload() as Record<string, unknown>;
    return {
      ...base,
      threads: [{ identity: threadIdentity, comments: [], isStreaming: true, ...over }],
      isStreaming: true,
      activeCount: 1,
    };
  };
  const settlePoll = async (data: unknown): Promise<void> => {
    await act(async () => {
      advanceClock(1500);
      await realTick();
    });
    check('poll tick settles', settleNext('code_review_snapshot', data));
    await act(async () => {
      await realTick();
    });
  };
  const hunkDot = () => {
    const node = mini.document.body.querySelector(
      '.code-review__streaming-dot[title="Thinking…"]',
    ) ?? mini.document.body.querySelector('.code-review__streaming-dot[title="Reply streaming"]');
    return node ? (node as MiniNode).getAttribute('title') : null;
  };

  // Phase 1: only thinking has arrived — live thinking block, no placeholder.
  await settlePoll(liveSnap({ streamingText: '', streamingThinking: 'silent reasoning step' }));
  const liveThinking = mini.document.body.querySelector('.code-review__thread details.thinking');
  check('live thinking block renders while streaming', liveThinking !== null);
  check(
    'live thinking summary uses the shared header',
    mini.document.body.querySelector('.code-review__thread .thinking__summary') !== null,
  );
  check(
    'no placeholder while thinking streams',
    mini.document.body.querySelector('.code-review__thinking-shimmer') === null,
  );
  check(
    'no partial text card before text arrives',
    mini.document.body.querySelector('.code-review__comment-text') === null,
  );
  check('dot reads Thinking… before text streams', hunkDot() === 'Thinking…');
  check('new hunk reply follows the thread tail', threadViewport.scrollTop >= threadViewport.scrollHeight - threadViewport.clientHeight);
  threadViewport.scrollTop = 20;
  await act(async () => {
    threadViewport.dispatchEvent({ type: 'scroll' });
    await realTick();
  });
  threadViewport.scrollHeight = 260;

  // Phase 2: text starts streaming under the still-visible thinking block.
  await settlePoll(liveSnap({ streamingText: 'partial reply text', streamingThinking: 'silent reasoning step' }));
  check('thinking block stays above the streaming text', liveThinking !== null
    && mini.document.body.querySelector('.code-review__thread details.thinking') !== null);
  check(
    'streaming text card paints',
    (mini.document.body.querySelector('.code-review__comment-text') as MiniNode | null)
      ?.textContent.includes('partial reply text') === true,
  );
  check('dot reads Reply streaming once text streams', hunkDot() === 'Reply streaming');
  check(
    'no placeholder while text streams',
    mini.document.body.querySelector('.code-review__thinking-shimmer') === null,
  );
  check('deliberate thread scroll up suspends tail follow', threadViewport.scrollTop === 20);
  threadViewport.scrollTop = threadViewport.scrollHeight - threadViewport.clientHeight;
  await act(async () => {
    threadViewport.dispatchEvent({ type: 'scroll' });
    await realTick();
  });
  threadViewport.scrollHeight = 300;
  await settlePoll(liveSnap({
    streamingText: 'partial reply text continued',
    streamingThinking: 'silent reasoning step',
  }));
  check('returning to thread bottom resumes tail follow', threadViewport.scrollTop >= threadViewport.scrollHeight - threadViewport.clientHeight);

  // Phase 3: dead air — neither buffer has content → shimmer card.
  await settlePoll(liveSnap({ streamingText: '', streamingThinking: '' }));
  const shimmer = mini.document.body.querySelector('.code-review__thinking-shimmer') as MiniNode | null;
  check('dead-air shimmer renders', shimmer !== null);
  check('dead-air shimmer reads Thinking…', shimmer?.textContent === 'Thinking…');
  check(
    'dead-air shimmer lives in an assistant partial card',
    shimmer !== null
      && (shimmer as MiniNode).parentNode !== null
      && (((shimmer as MiniNode).parentNode as MiniNode | null)?.className ?? '').includes(
          'code-review__comment--assistant',
        )
      && ((shimmer as MiniNode).parentNode as MiniNode).className.includes('is-partial'),
  );
  check('no thinking block on dead air', mini.document.body.querySelector('.code-review__thread details.thinking') === null);
  check('no comment text on dead air', mini.document.body.querySelector('.code-review__comment-text') === null);
  check('dot reads Thinking… on dead air', hunkDot() === 'Thinking…');

  // Phase 4: terminal snapshot — final comments carry thinking above text.
  await settlePoll({
    ...(snapshotPayload() as Record<string, unknown>),
    threads: [
      {
        identity: threadIdentity,
        comments: [
          { role: 'user', text: 'what changed here?', partial: false },
          { role: 'assistant', text: 'final reply', partial: false, thinking: 'settled reasoning' },
          { role: 'assistant', text: 'plain reply', partial: false },
        ],
        isStreaming: false,
      },
    ],
  });
  const commentThinking = mini.document.body.querySelectorAll(
    '.code-review__comment--assistant details.thinking',
  );
  check('exactly the thinking assistant comment renders a thinking block', commentThinking.length === 1);
  check(
    'comment thinking body paints',
    (commentThinking[0]!.querySelector('.thinking__body') as MiniNode | null)
      ?.textContent.includes('settled reasoning') === true,
  );
  check(
    'comment thinking renders above the comment text',
    (commentThinking[0] as MiniNode).nextSibling !== null
      && ((commentThinking[0] as MiniNode).nextSibling as MiniNode).className.includes(
        'code-review__comment-text',
      ),
  );
  check(
    'plain assistant comment has no thinking block',
    mini.document.body.textContent.includes('plain reply') === true
      && mini.document.body.querySelectorAll('details.thinking').length === 1,
  );
  check('placeholder gone after terminal', mini.document.body.querySelector('.code-review__thinking-shimmer') === null);
  check('streaming dot gone after terminal', mini.document.body.querySelector('.code-review__streaming-dot') === null);

  await unmountPanel();
}




// ---- Wave 2: Auto Review button state machine, files-column resizer, and
// file/directory issues badges + danger highlights ----
{
  const deepHash = unifiedHunkSha256('@@ -0,0 +1,2 @@', [
    { kind: 'addition', newNo: 1, text: '+DEEP-ONE' },
    { kind: 'addition', newNo: 2, text: '+DEEP-TWO' },
  ]);
  const topHash = unifiedHunkSha256('@@ -0,0 +1 @@', singleLine('+TOP-ONE'));

  const autoSnapshot = () => ({
    ...snapshotPayload(),
    reviewLanguageDefault: 'de',
    files: [
      onDemandFile('a.txt', aFreshHash),
      onDemandFile('src/lib/deep.rs', deepHash, 2),
      onDemandFile('src/top.ts', topHash),
    ],
    threads: [{
      identity: { snapshotId: 'snap-1', path: 'a.txt', oldStart: 0, oldCount: 0, newStart: 1, newCount: 1, contentHash: aFreshHash },
      comments: [{ role: 'user', text: 'auto review', partial: false, auto: true }],
      isStreaming: false,
    }],
    reviewFindings: [
      {
        identity: {
          snapshotId: 'snap-1',
          path: 'a.txt',
          oldStart: 0,
          oldCount: 0,
          newStart: 1,
          newCount: 1,
          contentHash: aFreshHash,
        },
        findings: Array.from({ length: 3 }, (_, index) => ({
          category: 'critical', side: 'new', startLine: 1, endLine: 1,
          summary: `critical issue ${index + 1}`, evidence: 'FRESH-APPEND',
        })),
      },
      {
        identity: {
          snapshotId: 'snap-1',
          path: 'src/lib/deep.rs',
          oldStart: 0,
          oldCount: 0,
          newStart: 1,
          newCount: 2,
          contentHash: deepHash,
        },
        findings: [1, 2].map((line) => ({
          category: 'critical', side: 'new', startLine: line, endLine: line,
          summary: `deep issue ${line}`, evidence: line === 1 ? 'DEEP-ONE' : 'DEEP-TWO',
        })),
      },
      {
        identity: {
          snapshotId: 'snap-1',
          path: 'src/top.ts',
          oldStart: 0,
          oldCount: 0,
          newStart: 1,
          newCount: 1,
          contentHash: topHash,
        },
        findings: [{ category: 'overdesign', side: 'new', startLine: 1, endLine: 1, summary: 'unnecessary indirection', evidence: 'TOP-ONE' }],
      },
    ],
    autoReviewState: 'idle',
    autoReviewCounts: { submitted: 0, completed: 0, failed: 0 },
  });

  await mountPanel(autoSnapshot);
  check(
    'fresh page settles for the auto review scenario',
    settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')),
  );
  await act(async () => {
    await realTick();
  });

  const autoButton = (): MiniNode | null => autoReviewActionButton();
  const pollWith = async (data: unknown): Promise<void> => {
    await act(async () => {
      advanceClock(1500);
      await realTick();
      check('auto scenario poll settles', settleNext('code_review_snapshot', data));
    });
    // The settle's render + effects flush at act exit, arming the 151ms
    // spinner gate; advance past it in a fresh act so progress assertions
    // are deterministic.
    await act(async () => {
      advanceClock(200);
      await realTick();
    });
  };

  // Badges count precise findings; tones cover only their source rows.
  const aRow = mini.document.body.querySelector('[data-file-path="a.txt"]') as MiniNode | null;
  check('critical file row renders the danger class', aRow !== null && (aRow as MiniNode).className.includes('code-review__file-row--danger'));
  const aBadge = aRow ? (aRow as MiniNode).querySelector('.code-review__file-issues') : null;
  check('critical file row renders its issue sum', aBadge !== null && (aBadge as MiniNode).textContent === '3');
  const deepRow = mini.document.body.querySelector('[data-file-path="src/lib/deep.rs"]') as MiniNode | null;
  check('nested critical file row renders its badge', deepRowBadgeCheck());
  function deepRowBadgeCheck(): boolean {
    if (!deepRow) return false;
    const badge = (deepRow as MiniNode).querySelector('.code-review__file-issues') as MiniNode | null;
    return (deepRow as MiniNode).className.includes('code-review__file-row--danger') && badge !== null && badge.textContent === '2';
  }
  const srcDir = mini.document.body.querySelector('[data-dir-path="src"]') as MiniNode | null;
  const srcLibDir = mini.document.body.querySelector('[data-dir-path="src/lib"]') as MiniNode | null;
  check(
    'directory rows aggregate the subtree issues and danger',
    srcDir !== null
      && (srcDir as MiniNode).className.includes('code-review__file-row--dir')
      && (srcDir as MiniNode).className.includes('code-review__file-row--danger')
      && ((srcDir as MiniNode).querySelector('.code-review__file-issues') as MiniNode | null)?.textContent === '3',
    srcDir ? (srcDir as MiniNode).className : 'no src dir row',
  );
  check(
    'nested directory row aggregates its own subtree',
    srcLibDir !== null
      && ((srcLibDir as MiniNode).querySelector('.code-review__file-issues') as MiniNode | null)?.textContent === '2'
      && (srcLibDir as MiniNode).className.includes('code-review__file-row--danger'),
  );
  const topRow = mini.document.body.querySelector('[data-file-path="src/top.ts"]') as MiniNode | null;
  check('overdesign file row renders the warn tone, never danger', topRow !== null
    && ((topRow as MiniNode).querySelector('.code-review__file-issues') as MiniNode | null)?.textContent === '1'
    && (topRow as MiniNode).className.includes('code-review__file-row--warn')
    && !(topRow as MiniNode).className.includes('code-review__file-row--danger'));
  check(
    'critical findings paint their existing new-side row',
    (mini.document.body.querySelector('.code-review__line.has-finding--danger') as MiniNode | null)?.textContent.includes('FRESH-APPEND') === true,
  );

  // Language: the snapshot's settings default preselects; an explicit pick
  // rides the RPC `language`; the Default sentinel sends nothing.
  const autoLang = () => mini.document.body.querySelector('.code-review__auto-lang') as MiniNode | null;
  const idleButton = autoButton();
  check('auto review button renders icon plus label', idleButton !== null && (idleButton as MiniNode).textContent === '◐Auto Review');
  check('idle auto review button is enabled', idleButton !== null && (idleButton as MiniNode).getAttribute('disabled') === null);

  await act(async () => {
    (idleButton as MiniNode).click();
    await realTick();
  });
  check('click sends exactly one code_review_auto_review', countType('code_review_auto_review') === 1);
  check(
    'auto review command stamps the owning session',
    sent.filter((entry) => entry.type === 'code_review_auto_review').every((entry) => entry.command.sessionId === 'sess-cr'),
    JSON.stringify(sent.filter((entry) => entry.type === 'code_review_auto_review')),
  );
  const pendingButton = autoButton();
  check(
    'the pending request owner keeps the button actionable (never disabled)',
    pendingButton !== null && (pendingButton as MiniNode).getAttribute('disabled') === null,
  );
  check('pre-gate pending keeps the label (no flash)', pendingButton !== null && (pendingButton as MiniNode).textContent === '◐Auto Review');
  await act(async () => {
    advanceClock(150);
    await realTick();
  });
  check(
    '150ms pending still hides the spinner and keeps Auto Review',
    pendingButton !== null
      && (pendingButton as MiniNode).textContent === '◐Auto Review'
      && (pendingButton as MiniNode).querySelector('.code-review__spinner') === null
      && !(pendingButton as MiniNode).textContent.includes('0/0'),
  );
  await act(async () => {
    advanceClock(1);
    await realTick();
  });
  check(
    '151ms pending shows captionless spinner and ■ Stop',
    pendingButton !== null
      && (pendingButton as MiniNode).textContent.includes('■ Stop')
      && (pendingButton as MiniNode).querySelector('.code-review__spinner') !== null
      && !(pendingButton as MiniNode).textContent.includes('0/0')
      && !(pendingButton as MiniNode).textContent.includes('Auto Review'),
    pendingButton ? (pendingButton as MiniNode).textContent : 'no button',
  );
  check(
    '151ms pending click screens in the header while submitted is 0',
    (mini.document.body.querySelector('.code-review__auto-progress') as MiniNode | null)?.textContent === 'Screening…',
  );

  const autoAck = deferreds.find((entry) => entry.type === 'code_review_auto_review' && !entry.settled);
  check('auto review ack is pending before settling', autoAck !== undefined);
  if (autoAck) {
    autoAck.settled = true;
    autoAck.resolve({ ok: true, state: 'triaging', batchId: 1 });
  }
  await act(async () => {
    await realTick();
  });
  const postAckButton = autoButton();
  check(
    'the accepted ACK keeps the owner busy/Stop through the ACK→snapshot gap',
    postAckButton !== null
      && (postAckButton as MiniNode).getAttribute('disabled') === null
      && (postAckButton as MiniNode).className.includes('code-review__action--stop'),
  );

  await pollWith({ ...autoSnapshot(), autoReviewState: 'running', autoReviewCounts: { submitted: 5, completed: 2, failed: 0 } });
  const runningButton = autoButton();
  check(
    'running snapshot shows ■ Stop and keeps the button actionable',
    runningButton !== null
      && (runningButton as MiniNode).getAttribute('disabled') === null
      && (runningButton as MiniNode).textContent.includes('■ Stop')
      && !(runningButton as MiniNode).textContent.includes('2/5'),
    runningButton ? (runningButton as MiniNode).textContent : 'no button',
  );
  check(
    'running snapshot paints Review completed/submitted in the header, not the Stop button',
    (mini.document.body.querySelector('.code-review__head') as MiniNode | null)?.textContent?.includes('Review 2/5') === true
      && (mini.document.body.querySelector('.code-review__auto-progress') as MiniNode | null)?.textContent === 'Review 2/5'
      && !(runningButton as MiniNode).textContent.includes('Review 2/5'),
  );

  await pollWith({ ...autoSnapshot(), autoReviewState: 'done', autoReviewCounts: { submitted: 5, completed: 5, failed: 0 } });
  const doneButton = autoButton();
  check('done snapshot returns the button to idle', doneButton !== null
    && (doneButton as MiniNode).getAttribute('disabled') === null
    && (doneButton as MiniNode).textContent === '◐Auto Review');
  check(
    'terminal snapshot keeps Review completed/submitted in the header after Stop chrome clears',
    (mini.document.body.querySelector('.code-review__head') as MiniNode | null)?.textContent?.includes('Review 5/5') === true
      && (mini.document.body.querySelector('.code-review__auto-progress') as MiniNode | null)?.textContent === 'Review 5/5'
      && (doneButton as MiniNode).textContent === '◐Auto Review',
  );

  // Typed reject (code_review_auto_running-style race): non-destructive — no
  // error banner, no snapshot clobber, button immediately actionable.
  await act(async () => {
    (doneButton as MiniNode).click();
    await realTick();
  });
  check('second run sends a fresh auto review command', countType('code_review_auto_review') === 2);
  const rejected = deferreds.filter((entry) => entry.type === 'code_review_auto_review' && !entry.settled);
  const rejectedLast = rejected[rejected.length - 1];
  if (rejectedLast) {
    rejectedLast.settled = true;
    rejectedLast.reject(Object.assign(new Error('code review auto review is already running'), { rpc: true }));
  }
  await act(async () => {
    await realTick();
  });
  check('typed reject renders no inline error', mini.document.body.querySelector('.code-review__error') === null);
  check('typed reject leaves the last snapshot painted', mini.document.body.querySelector('[data-file-path="a.txt"]') !== null);
  check(
    'typed reject surfaces one App toast and no inline status',
    toasts.length === 1 && toasts[0].message === 'code review auto review is already running'
      && mini.document.body.querySelector('.code-review__auto-status') === null,
    JSON.stringify(toasts),
  );
  check('typed reject re-enables the button immediately', autoButton() !== null
    && (autoButton() as MiniNode).getAttribute('disabled') === null
    && (autoButton() as MiniNode).textContent === '◐Auto Review');

  await act(async () => {
    (autoButton() as MiniNode).click();
    await realTick();
  });
  check('button remains usable after the typed reject (not stuck)', countType('code_review_auto_review') === 3);
  const thirdBatch = deferreds.find((entry) => entry.type === 'code_review_auto_review' && !entry.settled);
  if (thirdBatch) {
    thirdBatch.settled = true;
    thirdBatch.resolve({ ok: true, state: 'triaging', batchId: 1 });
  }
  await act(async () => {
    await realTick();
  });
  check('the settled third batch frees the button for the language run', (autoButton() as MiniNode).getAttribute('disabled') === null);

  // Explicit language pick rides the RPC; the Default sentinel sends nothing.
  const autoLangNode = autoLang();
  check('language select still mounted after the batch churn', autoLangNode !== null);
  await act(async () => {
    autoLangNode!.value = 'fr';
    autoLangNode!.dispatchEvent({ type: 'change', target: autoLangNode!, preventDefault() {}, stopPropagation() {} });
    await realTick();
  });
  // The accepted ACK keeps the owner busy until the next snapshot with a
  // matching-or-newer batch id; release it so the language run can start.
  await pollWith({ ...autoSnapshot(), autoReviewState: 'idle' });
  await act(async () => {
    (autoButton() as MiniNode).click();
    await realTick();
  });
  check(
    'an explicit language pick rides code_review_auto_review.language',
    sent.filter((entry) => entry.type === 'code_review_auto_review').at(-1)?.command.language === 'fr',
    JSON.stringify(sent.filter((entry) => entry.type === 'code_review_auto_review').at(-1)?.command),
  );
  const frBatch = deferreds.find((entry) => entry.type === 'code_review_auto_review' && !entry.settled);
  if (frBatch) {
    frBatch.settled = true;
    frBatch.resolve({ ok: true, state: 'triaging', batchId: 1 });
  }
  await act(async () => {
    await realTick();
  });
  await pollWith({ ...autoSnapshot(), autoReviewState: 'idle' });
  await act(async () => {
    autoLangNode!.value = '';
    autoLangNode!.dispatchEvent({ type: 'change', target: autoLangNode!, preventDefault() {}, stopPropagation() {} });
    await realTick();
  });
  await act(async () => {
    (autoButton() as MiniNode).click();
    await realTick();
  });
  check(
    'the Default sentinel sends no language field',
    !('language' in (sent.filter((entry) => entry.type === 'code_review_auto_review').at(-1)?.command ?? {})),
    JSON.stringify(sent.filter((entry) => entry.type === 'code_review_auto_review').at(-1)?.command),
  );

  // Files-column resizer: separator semantics + keyboard stepping.
  const filesResizer = mini.document.body.querySelector('.code-review__files-resizer') as MiniNode | null;
  check('files resizer mounts on desktop', filesResizer !== null);
  check('files resizer is an accessible vertical separator', filesResizer !== null
    && (filesResizer as MiniNode).getAttribute('role') === 'separator'
    && (filesResizer as MiniNode).getAttribute('aria-orientation') === 'vertical');
  check('files resizer exposes the frozen bounds', filesResizer !== null
    && (filesResizer as MiniNode).getAttribute('aria-valuemin') === '180'
    && (filesResizer as MiniNode).getAttribute('aria-valuemax') === '480'
    && (filesResizer as MiniNode).getAttribute('aria-valuenow') === '224');
  check('files resizer is keyboard focusable', filesResizer !== null && (filesResizer as MiniNode).getAttribute('tabindex') === '0');

  const codeReviewRoot = mini.document.body.querySelector('.code-review') as MiniNode;
  await act(async () => {
    (filesResizer as MiniNode).dispatchEvent({ type: 'keydown', key: 'ArrowRight' });
    await realTick();
  });
  check(
    'ArrowRight grows the files column by one step and persists',
    window.localStorage.getItem('rpi-code-review-files-width') === '248'
      && (filesResizer as MiniNode).getAttribute('aria-valuenow') === '248'
      && codeReviewRoot.style['--code-review-files-width'] === '248px',
    JSON.stringify({
      stored: window.localStorage.getItem('rpi-code-review-files-width'),
      now: (filesResizer as MiniNode).getAttribute('aria-valuenow'),
      style: codeReviewRoot.style['--code-review-files-width'],
    }),
  );
  await act(async () => {
    (filesResizer as MiniNode).dispatchEvent({ type: 'keydown', key: 'ArrowLeft' });
    await realTick();
  });
  check(
    'ArrowLeft shrinks the files column by one step',
    window.localStorage.getItem('rpi-code-review-files-width') === '224'
      && (filesResizer as MiniNode).getAttribute('aria-valuenow') === '224',
  );

  await unmountPanel();
}

// ---- Review model switch: catalog load, set_model wire shape, ack label,
// ---- typed-reject revert, and bounded inline status ----
{
  await mountPanel(() => ({
    ...snapshotPayload(),
    reviewModelProvider: 'prov',
    reviewModelId: 'm-1',
    reviewModelLabel: 'Model One',
  }));
  check('fresh page settles for the model scenario', settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')));
  await act(async () => {
    await realTick();
  });
  const hunkHeader = mini.document.body.querySelector('.code-review__hunk-header') as MiniNode;
  check('hunk header renders for the model scenario', hunkHeader !== null);
  check(
    'the panel head model select exists before any hunk is selected',
    mini.document.body.querySelector('.code-review__actions .code-review__model-select') !== null
      && mini.document.body.querySelectorAll('.code-review__model-select').length === 1,
  );
  await act(async () => {
    hunkHeader.click();
    await realTick();
  });
  const modelSelect = () =>
    mini.document.body.querySelector('.code-review__model-select') as MiniNode | null;
  check('model select mounts in the panel head actions row', modelSelect() !== null
    && mini.document.body.querySelector('.code-review__actions .code-review__model-select') !== null);
  check(
    'exactly one model select exists in the panel',
    mini.document.body.querySelectorAll('.code-review__model-select').length === 1,
    String(mini.document.body.querySelectorAll('.code-review__model-select').length),
  );
  check(
    'the hunk dock renders no model select',
    mini.document.body.querySelector('.code-review__comments .code-review__model-select') === null
      && mini.document.body.querySelector('.code-review__hunk .code-review__model-select') === null,
  );
  check(
    'model catalog request stamps the owning session',
    sent.some((entry) => entry.type === 'get_available_models' && entry.command.sessionId === 'sess-cr'),
    JSON.stringify(sent.filter((entry) => entry.type === 'get_available_models')),
  );
  check('model select is disabled until the catalog settles', modelSelect() !== null
    && modelSelect()!.getAttribute('disabled') !== null);
  check(
    'model catalog settles',
    settleNext('get_available_models', {
      models: [
        { id: 'm-1', name: 'Model One', provider: 'prov' },
        { id: 'm-2', name: 'Model Two', provider: 'prov' },
        { id: 'm-1', name: 'Other One', provider: 'other' },
      ],
    }),
  );
  await act(async () => {
    await realTick();
  });
  check('model select enables after the catalog settles', modelSelect() !== null
    && modelSelect()!.getAttribute('disabled') === null);
  check(
    'default select value is provider/id identity, not the first matching label',
    modelSelect() !== null && modelSelect()!.value === 'prov/m-1',
    modelSelect() ? String(modelSelect()!.value) : 'no select',
  );

  await act(async () => {
    const select = modelSelect()!;
    select.value = 'prov/m-2';
    select.dispatchEvent({ type: 'change', target: select, preventDefault() {}, stopPropagation() {} });
    await realTick();
  });
  const setModelCommands = sent.filter((entry) => entry.type === 'code_review_set_model');
  check(
    'picking a model sends code_review_set_model with the qualified provider/id',
    setModelCommands.length === 1
      && setModelCommands[0].command.modelId === 'prov/m-2'
      && setModelCommands[0].command.sessionId === 'sess-cr',
    JSON.stringify(setModelCommands),
  );
  const setModelAck = deferreds.find((entry) => entry.type === 'code_review_set_model' && !entry.settled);
  check('set_model is pending before its ack', setModelAck !== undefined);
  if (setModelAck) {
    setModelAck.settled = true;
    setModelAck.resolve({ ok: true, model: 'Model Two' });
  }
  await act(async () => {
    await realTick();
  });
  check('the ack keeps the picked model selected', modelSelect() !== null && modelSelect()!.value === 'prov/m-2', modelSelect() ? `value=${modelSelect()!.value}` : 'no select');

  await act(async () => {
    const select = modelSelect()!;
    select.value = 'other/m-1';
    select.dispatchEvent({ type: 'change', target: select, preventDefault() {}, stopPropagation() {} });
    await realTick();
  });
  const secondSwitch = deferreds.find((entry) => entry.type === 'code_review_set_model' && !entry.settled);
  check('the second switch reaches the wire with the other provider identity',
    secondSwitch !== undefined && secondSwitch.command.modelId === 'other/m-1',
    JSON.stringify(secondSwitch?.command),
  );
  if (secondSwitch) {
    secondSwitch.settled = true;
    secondSwitch.reject(Object.assign(new Error('unknown model "other/m-1"'), { rpc: true }));
  }
  await act(async () => {
    await realTick();
  });
  check(
    'a typed reject keeps the previously applied model on the wire and toasts once',
    sent.filter((entry) => entry.type === 'code_review_set_model').map((entry) => entry.command.modelId).join(',') === 'prov/m-2,other/m-1'
      && toasts.length === 1
      && toasts[0].message === 'unknown model "other/m-1"',
    JSON.stringify({ toasts, models: sent.filter((entry) => entry.type === 'code_review_set_model').map((entry) => entry.command.modelId) }),
  );
  check(
    'a typed reject reverts the select to the applied identity',
    modelSelect() !== null && modelSelect()!.value === 'prov/m-2',
    modelSelect() ? `value=${modelSelect()!.value}` : 'no select',
  );
  check(
    'a typed model reject surfaces one App toast and no inline status',
    toasts.length === 1 && toasts[0].message === 'unknown model "other/m-1"'
      && mini.document.body.querySelector('.code-review__composer-status') === null,
    JSON.stringify(toasts),
  );
  check('the composer stays usable after the reject', mini.document.body.querySelector('.code-review__comment-input') !== null);
  check('no model-switch error banner', mini.document.body.querySelector('.code-review__error') === null);

  await unmountPanel();
}

// ---- the single head select with no catalog yet shows the snapshot's fork
// ---- model label (never a bare placeholder), and still exactly one select ----
{
  await mountPanel(() => ({
    ...snapshotPayload(),
    reviewModelLabel: 'Fork Model',
  }));
  await act(async () => {
    await realTick();
  });
  const headSelect = mini.document.body.querySelector('.code-review__actions .code-review__model-select') as MiniNode | null;
  check('head select exists without a settled catalog', headSelect !== null);
  const forkOptions = (() => {
    if (!headSelect) return [] as MiniNode[];
    return (headSelect.options as MiniNode[]) ?? [];
  })();
  const forkFirst = forkOptions[0];
  check(
    'the no-catalog select shows the real fork model, never a bare placeholder',
    forkFirst !== undefined
      && forkFirst.getAttribute('value') === ''
      && forkFirst.textContent === 'Fork Model',
    forkOptions.map((option) => `${option.getAttribute('value')}=${option.textContent}`).join('|'),
  );
  check(
    'the no-catalog select never renders the bare model placeholder',
    forkOptions.every((option) => option.textContent !== 'model…'),
  );
  check(
    'the no-catalog panel still mounts exactly one model select',
    mini.document.body.querySelectorAll('.code-review__model-select').length === 1,
  );
  await unmountPanel();
}

// ---- Source|Preview: renderable files gain the segmented control; Preview
// ---- renders the NEW file content through the explorer's renderers; binary
// ---- files and failed reads fail closed ----
{
  const mdHash = unifiedHunkSha256('@@ -0,0 +1 @@', singleLine('+BODY'));
  const htmlHash = unifiedHunkSha256('@@ -0,0 +1 @@', singleLine('+<p>x</p>'));
  const binHash = unifiedHunkSha256('@@ -0,0 +1 @@', singleLine('+bytes'));
  const surfaceSnapshot = () => ({
    ...snapshotPayload(),
    files: [
      onDemandFile('README.md', mdHash),
      onDemandFile('page.html', htmlHash),
      onDemandFile('a.txt', aFreshHash),
      onDemandFile('broken.md', unifiedHunkSha256('@@ -0,0 +1 @@', singleLine('+broken'))),
      { ...onDemandFile('blob.bin', binHash), binary: true },
    ],
  });
  await mountPanel(surfaceSnapshot, 'README.md');
  check('fresh md page settles for the surface scenario', settleNext('code_review_file_diff', pagePayload('README.md', '+BODY')));
  await act(async () => {
    await realTick();
  });
  const surface = () => mini.document.body.querySelector('.code-review__surface') as MiniNode | null;
  const previewBtn = () => surface()?.querySelectorAll('button')[1] as MiniNode | null;
  check('renderable files mount the Source|Preview control', surface() !== null);
  check('Source starts active', (surface()?.querySelectorAll('button')[0] as MiniNode | null)?.className.includes('is-active') === true);
  check('Preview is enabled for a renderable file', previewBtn() !== null && previewBtn()!.getAttribute('disabled') === null);

  await act(async () => {
    previewBtn()!.click();
    await realTick();
  });
  const reads = sent.filter((entry) => entry.type === 'file_explorer_read');
  check(
    'Preview fetches the NEW file content via the explorer read RPC',
    reads.length === 1 && reads[0].command.path === 'README.md' && reads[0].command.sessionId === 'sess-cr',
    JSON.stringify(reads),
  );
  check('preview loading shows animation only (no caption)', mini.document.body.querySelector('.code-review__preview-loading') !== null
    && mini.document.body.textContent.includes('Loading preview…') === false);
  check(
    'the preview read settles',
    settleNext('file_explorer_read', { path: 'README.md', lines: ['# Hi', '', 'body text'], totalLines: 3, sizeBytes: 12, truncated: false }),
  );
  await act(async () => {
    await realTick();
  });
  check(
    'markdown preview renders through the shared markdown renderer',
    mini.document.body.querySelector('.code-review__preview-markdown') !== null
      && mini.document.body.textContent.includes('body text'),
  );
  check('preview mode marks the Preview button active', (previewBtn() as MiniNode).className.includes('is-active'));
  check('the diff hunks are replaced by the preview surface', mini.document.body.querySelector('.code-review__hunks') === null);

  // HTML preview: sandboxed iframe srcDoc, same as the file explorer.
  await clickFileRow('page.html');
  check('fresh html page settles for the surface scenario', settleNext('code_review_file_diff', pagePayload('page.html', '+<p>x</p>')));
  await act(async () => {
    await realTick();
  });
  check('switching files resets the surface to Source', (surface()?.querySelectorAll('button')[0] as MiniNode | null)?.className.includes('is-active') === true);
  await act(async () => {
    previewBtn()!.click();
    await realTick();
  });
  check(
    'the html preview read settles',
    settleNext('file_explorer_read', { path: 'page.html', lines: ['<p>x</p>'], totalLines: 1, sizeBytes: 8, truncated: false }),
  );
  await act(async () => {
    await realTick();
  });
  const dbgFrame = mini.document.body.querySelector('.code-review__preview-frame') as MiniNode | null;
  const frame = mini.document.body.querySelector('.code-review__preview-frame') as MiniNode | null;
  check('html preview renders a sandboxed iframe', frame !== null
    && frame.getAttribute('sandbox') === ''
    // The mini DOM stores React's camelCase prop verbatim; a real browser
    // DOM lowercases it to referrerpolicy.
    && (frame.getAttribute('referrerpolicy') ?? frame.getAttribute('referrerPolicy')) === 'no-referrer'
    && frame.getAttribute('srcDoc') === '<p>x</p>');

  // Binary files fail closed: no surface control at all.
  await clickFileRow('blob.bin');
  check('binary files never mount the surface control', mini.document.body.querySelector('.code-review__surface') === null);
  // A failed read fails closed: Preview disables and the surface reverts.
  await clickFileRow('broken.md');
  check('broken.md settles its diff', settleNext('code_review_file_diff', pagePayload('broken.md', '+broken')));
  await act(async () => {
    await realTick();
  });
  check('a failing file still mounts the control', surface() !== null);
  await act(async () => {
    previewBtn()!.click();
    await realTick();
  });
  const failedRead = deferreds.find((entry) => entry.type === 'file_explorer_read' && !entry.settled);
  check('a fresh preview read starts for the failing file', failedRead !== undefined);
  if (failedRead) {
    failedRead.settled = true;
    failedRead.reject(Object.assign(new Error('file_explorer_read failed'), { rpc: true }));
  }
  await act(async () => {
    await realTick();
  });
  check('a failed read disables Preview fail-closed', previewBtn() !== null && previewBtn()!.getAttribute('disabled') !== null);
  check('a failed read falls back to the diff', mini.document.body.querySelector('.code-review__preview') === null);
  check('a failed read renders no error banner', mini.document.body.querySelector('.code-review__error') === null);

  // Reselecting the cached md file shows the preview without a refetch.
  await clickFileRow('README.md');
  const readsBeforeReplay = sent.filter((entry) => entry.type === 'file_explorer_read').length;
  await act(async () => {
    previewBtn()!.click();
    await realTick();
  });
  check(
    'the cached preview replays without a new read',
    sent.filter((entry) => entry.type === 'file_explorer_read').length === readsBeforeReplay
      && mini.document.body.querySelector('.code-review__preview-markdown') !== null,
  );

  await unmountPanel();
}

{
  const aIdentity = {
    snapshotId: 'snap-1',
    path: 'a.txt',
    oldStart: 0,
    oldCount: 0,
    newStart: 1,
    newCount: 1,
    contentHash: aFreshHash,
  };
  const highlightSnapshot = () => ({
    ...snapshotPayload(),
    reviewLanguageDefault: 'zh',
    highlightState: 'idle',
    reviewFindings: [
      { identity: aIdentity, findings: [{ category: 'critical', side: 'new', startLine: 1, endLine: 1, summary: 'hunk auth bypass', evidence: 'FRESH-APPEND' }] },
      {
        identity: {
          snapshotId: 'snap-1',
          path: 'b.txt',
          oldStart: 0,
          oldCount: 0,
          newStart: 1,
          newCount: 1,
          contentHash: bSeedHash,
        },
        findings: [{ category: 'overdesign', side: 'new', startLine: 1, endLine: 1, summary: 'other file only', evidence: 'b seed' }],
      },
    ],
    highlightError: null,
  });

  await mountPanel(highlightSnapshot);
  check(
    'fresh page settles for the highlight contract',
    settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')),
  );
  await act(async () => {
    await realTick();
  });

  const treeToggle = mini.document.body.querySelector('#code-review-tree-toggle') as MiniNode | null;
  const focusToggle = mini.document.body.querySelector('#code-review-focus-toggle') as MiniNode | null;
  const closeBtn = mini.document.body.querySelector('button.code-review__close') as MiniNode | null;
  check('tree toggle, focus, and close mount', treeToggle !== null && focusToggle !== null && closeBtn !== null);
  const chromeParent = treeToggle?.parentNode as MiniNode | null;
  const chromeKids = chromeParent ? elementChildren(chromeParent) : [];
  check(
    'header right group children are exactly ▾,⛶,✕',
    chromeParent !== null
      && chromeKids.length === 3
      && chromeKids[0] === treeToggle
      && chromeKids[1] === focusToggle
      && chromeKids[2] === closeBtn
      && (treeToggle as MiniNode).textContent === '▾'
      && (focusToggle as MiniNode).textContent === '⛶'
      && (closeBtn as MiniNode).textContent === '✕',
    chromeKids.map((child) => `${child.id || child.className}:${JSON.stringify(child.textContent)}`).join('|'),
  );
  check(
    'header chrome buttons are accessible',
    treeToggle !== null
      && (treeToggle.getAttribute('aria-label') ?? '').length > 0
      && focusToggle !== null
      && (focusToggle.getAttribute('aria-label') ?? '').length > 0
      && closeBtn !== null
      && (closeBtn.getAttribute('aria-label') ?? '').length > 0,
  );

  const lang = mini.document.body.querySelector('.code-review__auto-lang') as MiniNode | null;
  const zhOptions = lang
    ? (lang.querySelectorAll('option') as MiniNode[]).filter((option) => option.textContent === '中文')
    : [];
  check(
    'language default exposes exactly one 中文 option and no Default row',
    lang !== null && zhOptions.length === 1 && zhOptions[0]?.getAttribute('value') === 'zh'
      && (lang.querySelectorAll('option') as MiniNode[]).every((option) => option.textContent !== 'Default'),
    String(zhOptions.length),
  );

  const painted = mini.document.body.querySelector('.code-review__line.has-finding--danger') as MiniNode | null;
  check('precise finding paints its existing new-side row', painted !== null && painted.textContent.includes('FRESH-APPEND'));
  check('another file finding does not tint this row', painted !== null && !painted.className.includes('has-finding--warn'));
  check('unselected finding does not manufacture a legacy summary card', mini.document.body.querySelector('.code-review__hunk-highlight-summary') === null);
  check('no file-level Key changes card', !mini.document.body.textContent.includes('Key changes'));

  // Stale poll rejection must come from the request sequence, not mirror state.
  let olderPoll: DeferredCommand | undefined;
  await act(async () => {
    advanceClock(1500);
    await realTick();
    olderPoll = deferreds.find((entry) => entry.type === 'code_review_snapshot' && !entry.settled);
  });
  check('an older snapshot poll is left in flight', olderPoll !== undefined);
  await act(async () => {
    (headerRefreshButton() as MiniNode).click();
    await realTick();
  });
  check('the refresh response with highlights settles', settleNext('code_review_refresh', highlightSnapshot()));
  await act(async () => {
    await realTick();
  });
  if (olderPoll) {
    olderPoll.settled = true;
    olderPoll.resolve({
      ...highlightSnapshot(),
      highlightState: 'idle',
      reviewFindings: [],
    });
  }
  await act(async () => {
    await realTick();
  });
  check(
    'the stale empty poll is rejected by the request sequence and keeps the newer highlight',
    mini.document.body.querySelector('.code-review__line.has-finding--danger') !== null
      && mini.document.body.querySelector('.code-review__highlight-card') === null
      && highlightActionButton() !== null
      && !(highlightActionButton() as MiniNode).textContent.includes('■ Stop'),
  );
  await act(async () => {
    await realTick();
    settleNext('code_review_snapshot', highlightSnapshot());
  });
  await act(async () => {
    await realTick();
  });

  await act(async () => {
    (autoReviewActionButton() as MiniNode).click();
    await realTick();
  });
  const autoAck = deferreds.find((entry) => entry.type === 'code_review_auto_review' && !entry.settled);
  if (autoAck) {
    autoAck.settled = true;
    autoAck.resolve({ ok: true, state: 'triaging', batchId: 1 });
  }
  await act(async () => {
    await realTick();
  });
  await act(async () => {
    advanceClock(1500);
    await realTick();
    settleNext('code_review_snapshot', {
      ...highlightSnapshot(),
      autoReviewState: 'running',
      autoReviewCounts: { submitted: 1, completed: 0, failed: 0 },
    });
  });
  await act(async () => {
    await realTick();
  });
  const autoStopsBefore = countType('code_review_stop');
  await act(async () => {
    (autoReviewActionButton() as MiniNode).click();
    await realTick();
  });
  check(
    'Auto Review Stop sends typed code_review_stop autoReview',
    countType('code_review_stop') === autoStopsBefore + 1
      && sent.filter((entry) => entry.type === 'code_review_stop').at(-1)?.command.operation === 'autoReview'
      && sent.filter((entry) => entry.type === 'code_review_stop').at(-1)?.command.sessionId === 'sess-cr',
    JSON.stringify(sent.filter((entry) => entry.type === 'code_review_stop')),
  );
  const autoStopAck = deferreds.find((entry) => entry.type === 'code_review_stop' && !entry.settled);
  const snapshotsBeforeAutoStopAck = countType('code_review_snapshot');
  if (autoStopAck) {
    autoStopAck.settled = true;
    autoStopAck.resolve({ ok: true });
  }
  await act(async () => {
    await realTick();
  });
  check(
    'Auto Review Stop fetches snapshot immediately after ACK',
    countType('code_review_snapshot') > snapshotsBeforeAutoStopAck,
    `before=${snapshotsBeforeAutoStopAck} after=${countType('code_review_snapshot')}`,
  );
  await act(async () => {
    settleNext('code_review_snapshot', {
      ...highlightSnapshot(),
      autoReviewState: 'failed',
      autoReviewError: { message: 'Auto review stopped', code: 'cancelled' },
      autoReviewBatchId: 1,
    });
    await realTick();
  });
  check(
    'Auto Review Stop restores the Auto Review label without waiting for the idle poll',
    autoReviewActionButton() !== null
      && (autoReviewActionButton() as MiniNode).textContent.includes('Auto Review')
      && !(autoReviewActionButton() as MiniNode).textContent.includes('■ Stop'),
    autoReviewActionButton() ? (autoReviewActionButton() as MiniNode).textContent : 'no auto button',
  );
  check(
    'Auto Review Stop does not toast the cancelled stop',
    !toasts.some((toast) => toast.message.includes('Auto review: Auto review stopped')),
    JSON.stringify(toasts),
  );
  await act(async () => {
    advanceClock(1500);
    await realTick();
    settleNext('code_review_snapshot', highlightSnapshot());
  });
  await act(async () => {
    await realTick();
  });

  const highlightIdle = highlightActionButton();
  check('Highlight starts idle', highlightIdle !== null && (highlightIdle as MiniNode).textContent.includes('Highlight'));
  await act(async () => {
    (highlightIdle as MiniNode).click();
    await realTick();
  });
  check('Highlight click sends code_review_highlight', countType('code_review_highlight') === 1);
  await act(async () => {
    advanceClock(150);
    await realTick();
  });
  check(
    'Highlight spinner stays hidden at 150ms',
    highlightActionButton() !== null
      && (highlightActionButton() as MiniNode).querySelector('.code-review__spinner') === null
      && !(highlightActionButton() as MiniNode).textContent.includes('Highlight…'),
  );
  await act(async () => {
    advanceClock(1);
    await realTick();
  });
  check(
    '151ms Highlight shows captionless spinner, ■ Stop, and Identifying… in the header',
    highlightActionButton() !== null
      && (highlightActionButton() as MiniNode).textContent.includes('■ Stop')
      && (highlightActionButton() as MiniNode).querySelector('.code-review__spinner') !== null
      && !(highlightActionButton() as MiniNode).textContent.includes('Highlight…')
      && (mini.document.body.querySelector('.code-review__highlight-progress') as MiniNode | null)?.textContent === 'Identifying…',
    highlightActionButton() ? (highlightActionButton() as MiniNode).textContent : 'no highlight button',
  );
  const highlightAck = deferreds.find((entry) => entry.type === 'code_review_highlight' && !entry.settled);
  if (highlightAck) {
    highlightAck.settled = true;
    highlightAck.resolve({ ok: true, state: 'running', highlightGeneration: 1 });
  }
  await act(async () => {
    await realTick();
  });
  await act(async () => {
    advanceClock(1500);
    await realTick();
    check(
      'highlight running poll settles',
      settleNext('code_review_snapshot', { ...highlightSnapshot(), highlightState: 'running' }),
    );
  });
  await act(async () => {
    await realTick();
  });
  const highlightStopsBefore = countType('code_review_stop');
  await act(async () => {
    (highlightActionButton() as MiniNode).click();
    await realTick();
  });
  check(
    'Highlight Stop sends typed code_review_stop highlight',
    countType('code_review_stop') === highlightStopsBefore + 1
      && sent.filter((entry) => entry.type === 'code_review_stop').at(-1)?.command.operation === 'highlight'
      && sent.filter((entry) => entry.type === 'code_review_stop').at(-1)?.command.sessionId === 'sess-cr',
    JSON.stringify(sent.filter((entry) => entry.type === 'code_review_stop')),
  );
  const highlightStopAck = deferreds.find((entry) => entry.type === 'code_review_stop' && !entry.settled);
  const snapshotsBeforeHighlightStopAck = countType('code_review_snapshot');
  if (highlightStopAck) {
    highlightStopAck.settled = true;
    highlightStopAck.resolve({ ok: true });
  }
  await act(async () => {
    await realTick();
  });
  check(
    'Highlight Stop fetches snapshot immediately after ACK',
    countType('code_review_snapshot') > snapshotsBeforeHighlightStopAck,
    `before=${snapshotsBeforeHighlightStopAck} after=${countType('code_review_snapshot')}`,
  );
  await act(async () => {
    settleNext('code_review_snapshot', { ...highlightSnapshot(), highlightState: 'idle' });
    await realTick();
  });
  check(
    'Highlight Stop restores the Highlight label without waiting for the idle poll',
    highlightActionButton() !== null
      && (highlightActionButton() as MiniNode).textContent.includes('Highlight')
      && !(highlightActionButton() as MiniNode).textContent.includes('■ Stop'),
    highlightActionButton() ? (highlightActionButton() as MiniNode).textContent : 'no highlight button',
  );
  await act(async () => {
    await realTick();
  });

  await act(async () => {
    (highlightActionButton() as MiniNode).click();
    await realTick();
  });
  const highlightReject = deferreds.find((entry) => entry.type === 'code_review_highlight' && !entry.settled);
  check('highlight reject is pending', highlightReject !== undefined);
  if (highlightReject) {
    highlightReject.settled = true;
    highlightReject.reject(Object.assign(new Error('highlight already running'), { rpc: true }));
  }
  await act(async () => {
    await realTick();
  });
  check('highlight action error has no panel error node', mini.document.body.querySelector('.code-review__error') === null);

  const refreshBtn = headerRefreshButton();
  const refreshesBefore = countType('code_review_refresh');
  check('Refresh label is exactly Refresh', refreshBtn !== null && (refreshBtn.textContent ?? '').endsWith('Refresh') && !refreshBtn.textContent.includes('Refreshing'));
  await act(async () => {
    (refreshBtn as MiniNode).click();
    await realTick();
  });
  check('refresh sends code_review_refresh', countType('code_review_refresh') === refreshesBefore + 1);
  const busyRefresh = headerRefreshButton();
  check(
    'Refresh keeps the Refresh label while busy',
    busyRefresh !== null
      && (busyRefresh.textContent ?? '').includes('Refresh')
      && !busyRefresh.textContent.includes('Refreshing'),
    busyRefresh ? busyRefresh.textContent : 'no refresh button',
  );
  check('no Refreshing banner node', mini.document.body.querySelector('.code-review__busy') === null);
  check(
    'Refresh busy animation is button-only',
    busyRefresh !== null && busyRefresh.className.includes('is-refreshing'),
  );
  await act(async () => {
    advanceClock(150);
    await realTick();
  });
  check(
    'Refresh spinner hidden at 150ms',
    headerRefreshButton() !== null
      && (headerRefreshButton() as MiniNode).querySelector('.code-review__spinner') === null,
  );
  await act(async () => {
    advanceClock(1);
    await realTick();
  });
  check(
    '151ms Refresh spinner is on the button only',
    headerRefreshButton() !== null
      && (headerRefreshButton() as MiniNode).querySelector('.code-review__spinner') !== null
      && mini.document.body.querySelector('.code-review__busy') === null
      && !(headerRefreshButton() as MiniNode).textContent.includes('Refreshing'),
  );
  const refreshAck = deferreds.find((entry) => entry.type === 'code_review_refresh' && !entry.settled);
  if (refreshAck) {
    refreshAck.settled = true;
    refreshAck.resolve(highlightSnapshot());
  }
  await act(async () => {
    await realTick();
  });
  check('settled Refresh drops button busy animation', headerRefreshButton() !== null && !headerRefreshButton()!.className.includes('is-refreshing'));

  await unmountPanel();
}

{
  const escSnapshot = () => ({
    ...snapshotPayload(),
    autoReviewState: 'idle',
    autoReviewCounts: { submitted: 0, completed: 0, failed: 0 },
    highlightState: 'idle',
    reviewFindings: [],
  });
  await mountPanel(escSnapshot);
  check('fresh page settles for Esc stop order', settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')));
  await act(async () => {
    await realTick();
  });

  await act(async () => {
    (autoReviewActionButton() as MiniNode).click();
    await realTick();
  });
  const autoStart = deferreds.find((entry) => entry.type === 'code_review_auto_review' && !entry.settled);
  if (autoStart) {
    autoStart.settled = true;
    autoStart.resolve({ ok: true, state: 'triaging', batchId: 1 });
  }
  await act(async () => {
    await realTick();
  });
  await act(async () => {
    advanceClock(1500);
    await realTick();
    settleNext('code_review_snapshot', {
      ...escSnapshot(),
      autoReviewState: 'running',
      autoReviewCounts: { submitted: 1, completed: 0, failed: 0 },
    });
  });
  await act(async () => {
    await realTick();
  });

  await act(async () => {
    (highlightActionButton() as MiniNode).click();
    await realTick();
  });
  const highlightStart = deferreds.find((entry) => entry.type === 'code_review_highlight' && !entry.settled);
  if (highlightStart) {
    highlightStart.settled = true;
    highlightStart.resolve({ ok: true, state: 'running', highlightGeneration: 1 });
  }
  await act(async () => {
    await realTick();
  });
  await act(async () => {
    advanceClock(1500);
    await realTick();
    settleNext('code_review_snapshot', {
      ...escSnapshot(),
      autoReviewState: 'running',
      autoReviewCounts: { submitted: 1, completed: 0, failed: 0 },
      highlightState: 'running',
    });
  });
  await act(async () => {
    await realTick();
  });

  const stopsBeforeEsc = countType('code_review_stop');
  await pressPanelEscape();
  check('Esc while both run does not close', closeCalls === 0);
  check(
    'Esc stops autoReview first',
    countType('code_review_stop') === stopsBeforeEsc + 1
      && sent.filter((entry) => entry.type === 'code_review_stop').at(-1)?.command.operation === 'autoReview',
    JSON.stringify(sent.filter((entry) => entry.type === 'code_review_stop')),
  );
  const escAutoStop = deferreds.find((entry) => entry.type === 'code_review_stop' && !entry.settled);
  if (escAutoStop) {
    escAutoStop.settled = true;
    escAutoStop.resolve({ ok: true });
  }
  await act(async () => {
    await realTick();
  });
  await act(async () => {
    advanceClock(1500);
    await realTick();
    settleNext('code_review_snapshot', {
      ...escSnapshot(),
      autoReviewState: 'idle',
      highlightState: 'running',
    });
  });
  await act(async () => {
    await realTick();
  });

  await pressPanelEscape();
  check('Esc after auto idle stops highlight', sent.filter((entry) => entry.type === 'code_review_stop').at(-1)?.command.operation === 'highlight');
  check('Esc highlight stop does not close', closeCalls === 0);
  const escHighlightStop = deferreds.find((entry) => entry.type === 'code_review_stop' && !entry.settled);
  if (escHighlightStop) {
    escHighlightStop.settled = true;
    escHighlightStop.resolve({ ok: true });
  }
  await act(async () => {
    await realTick();
  });
  await act(async () => {
    advanceClock(1500);
    await realTick();
    settleNext('code_review_snapshot', escSnapshot());
  });
  await act(async () => {
    await realTick();
  });

  await pressPanelEscape();
  check('Esc after both idle closes the panel', closeCalls === 1);

  await unmountPanel();
}

// ---- outside-focus document Escape: typed Stop while a review runs; close
// ---- only on a later Escape with neither operation running ----
{
  const escSnapshot = () => ({
    ...snapshotPayload(),
    autoReviewState: 'idle',
    autoReviewCounts: { submitted: 0, completed: 0, failed: 0 },
    highlightState: 'idle',
    reviewFindings: [],
  });
  await mountPanel(escSnapshot);
  check('fresh page settles for the outside-focus Escape order', settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')));
  await act(async () => {
    await realTick();
  });

  await act(async () => {
    (autoReviewActionButton() as MiniNode).click();
    await realTick();
  });
  const docAutoStart = deferreds.find((entry) => entry.type === 'code_review_auto_review' && !entry.settled);
  if (docAutoStart) {
    docAutoStart.settled = true;
    docAutoStart.resolve({ ok: true, state: 'triaging', batchId: 1 });
  }
  await act(async () => {
    await realTick();
  });
  await act(async () => {
    advanceClock(1500);
    await realTick();
    settleNext('code_review_snapshot', {
      ...escSnapshot(),
      autoReviewState: 'running',
      autoReviewCounts: { submitted: 1, completed: 0, failed: 0 },
    });
  });
  await act(async () => {
    await realTick();
  });

  const stopsBeforeDocEsc = countType('code_review_stop');
  await act(async () => {
    // Focus outside the panel: a keydown reaches the document listener with a
    // target outside #code-review-panel (the document itself).
    mini.document.dispatchEvent({ type: 'keydown', key: 'Escape' });
    await realTick();
  });
  check('outside-focus Escape with a running Auto Review keeps the review open', closeCalls === 0);
  check(
    'outside-focus Escape sends typed code_review_stop autoReview for the owning session',
    countType('code_review_stop') === stopsBeforeDocEsc + 1
      && sent.filter((entry) => entry.type === 'code_review_stop').at(-1)?.command.operation === 'autoReview'
      && sent.filter((entry) => entry.type === 'code_review_stop').at(-1)?.command.sessionId === 'sess-cr',
    JSON.stringify(sent.filter((entry) => entry.type === 'code_review_stop')),
  );
  const docAutoStop = deferreds.find((entry) => entry.type === 'code_review_stop' && !entry.settled);
  if (docAutoStop) {
    docAutoStop.settled = true;
    docAutoStop.resolve({ ok: true });
  }
  await act(async () => {
    await realTick();
  });
  await act(async () => {
    advanceClock(1500);
    await realTick();
    settleNext('code_review_snapshot', {
      ...escSnapshot(),
      autoReviewState: 'idle',
      autoReviewCounts: { submitted: 1, completed: 1, failed: 0 },
      highlightState: 'running',
    });
  });
  await act(async () => {
    await realTick();
  });

  await act(async () => {
    mini.document.dispatchEvent({ type: 'keydown', key: 'Escape' });
    await realTick();
  });
  check('outside-focus Escape with a running Highlight keeps the review open', closeCalls === 0);
  check(
    'outside-focus Escape after auto idle sends typed code_review_stop highlight',
    sent.filter((entry) => entry.type === 'code_review_stop').at(-1)?.command.operation === 'highlight',
    JSON.stringify(sent.filter((entry) => entry.type === 'code_review_stop')),
  );
  const docHighlightStop = deferreds.find((entry) => entry.type === 'code_review_stop' && !entry.settled);
  if (docHighlightStop) {
    docHighlightStop.settled = true;
    docHighlightStop.resolve({ ok: true });
  }
  await act(async () => {
    await realTick();
  });
  await act(async () => {
    advanceClock(1500);
    await realTick();
    settleNext('code_review_snapshot', escSnapshot());
  });
  await act(async () => {
    await realTick();
  });

  const stopsBeforeClose = countType('code_review_stop');
  await act(async () => {
    mini.document.dispatchEvent({ type: 'keydown', key: 'Escape' });
    await realTick();
  });
  check('outside-focus Escape with neither operation running closes the review', closeCalls === 1);
  check('the closing outside-focus Escape sends no code_review_stop', countType('code_review_stop') === stopsBeforeClose);

  await unmountPanel();
}
// ---- per-hunk image intake: image-only, hunk-bound, wire-bounded ----
{
  const PNG_HEADER = new Uint8Array([
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a,
    0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02,
  ]);
  class FakeFileReader {
    result: string | ArrayBuffer | null = null;
    onload: (() => void) | null = null;
    onerror: (() => void) | null = null;
    readAsDataURL(blob: Blob): void {
      void blob.arrayBuffer().then((buffer) => {
        this.result = `data:${(blob as Blob & { type?: string }).type || 'application/octet-stream'};base64,${Buffer.from(buffer).toString('base64')}`;
        this.onload?.();
      });
    }
    readAsArrayBuffer(blob: Blob): void {
      void blob.arrayBuffer().then((buffer) => {
        this.result = buffer;
        this.onload?.();
      });
    }
  }
  class FakeImage {
    decoding = '';
    naturalWidth = 2;
    naturalHeight = 2;
    src = '';
    decode(): Promise<void> {
      return Promise.resolve();
    }
  }
  const previousFileReader = (globalThis as Record<string, unknown>).FileReader;
  const previousImage = (globalThis as Record<string, unknown>).Image;
  const previousCreateObjectURL = (URL as unknown as Record<string, unknown>).createObjectURL;
  const previousRevokeObjectURL = (URL as unknown as Record<string, unknown>).revokeObjectURL;
  (globalThis as Record<string, unknown>).FileReader = FakeFileReader;
  (globalThis as Record<string, unknown>).Image = FakeImage;
  (URL as unknown as Record<string, unknown>).createObjectURL = () => 'blob:panel-fixture';
  (URL as unknown as Record<string, unknown>).revokeObjectURL = () => {};
  const pngFile = () => new File([PNG_HEADER], 'shot.png', { type: 'image/png' });
  const fileListOf = (file: File) => ({
    0: file,
    length: 1,
    item(index: number) { return index === 0 ? file : null; },
    *[Symbol.iterator]() { yield file; },
  } as unknown as FileList);
  const picker = () => mini.document.body.querySelector('input[type="file"]') as MiniNode | null;
  const chips = () => mini.document.body.querySelectorAll('.composer-attachment') as MiniNode[];
  const composer = () => mini.document.body.querySelector('.code-review__comment-input') as MiniNode | null;
  const submitButton = () => mini.document.body.querySelector(
    '.code-review__action--send',
  ) as MiniNode | null;
  const flushReads = async (): Promise<void> => {
    await act(async () => {
      for (let i = 0; i < 25; i += 1) {
        await realTick();
      }
    });
  };
  const selectHunk = async (): Promise<void> => {
    const header = mini.document.body.querySelector('.code-review__hunk-header');
    if (!header) throw new Error('missing hunk header');
    await act(async () => {
      (header as MiniNode).click();
      await realTick();
    });
  };
  const deliverFiles = async (files: FileList): Promise<void> => {
    const input = picker();
    if (!input) throw new Error('missing image picker');
    input.files = files;
    await act(async () => {
      input.dispatchEvent({ type: 'change', target: input });
      await realTick();
    });
  };

  try {
    await mountPanel();
    check(
      'image composer fixture page settles',
      settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')),
    );
    await act(async () => { await realTick(); });
    await selectHunk();
    check('composer mounts for hunk A', composer() !== null);
    check(
      'the hunk picker accept is the image-only MIME set',
      picker()?.getAttribute('accept') === 'image/png,image/jpeg,image/gif,image/webp',
      picker()?.getAttribute('accept') ?? 'no picker',
    );
    check('no attachments before intake', chips().length === 0);

    await deliverFiles(fileListOf(new File(['x'], 'notes.txt', { type: 'text/plain' })));
    check(
      'a non-image intake is rejected with one bounded toast and no chip',
      toasts.length === 1 && toasts[0].message.startsWith('Skipped 1 file(s):')
        && toasts[0].message.includes('unsupported') && chips().length === 0,
      JSON.stringify(toasts),
    );

    await deliverFiles(fileListOf(pngFile()));
    check('a placeholder chip queues immediately in intake order', chips().length === 1);
    await flushReads();
    check('the read image chip paints its thumbnail', chips().length === 1
      && (chips()[0]?.querySelector('.composer-attachment__thumb') as MiniNode | null)?.getAttribute('src')
        === `data:image/png;base64,${Buffer.from(PNG_HEADER).toString('base64')}`);

    await clickFileRow('b.txt');
    check('fresh b.txt page settles', settleNext('code_review_file_diff', pagePayload('b.txt', '+B-APPEND')));
    await act(async () => { await realTick(); });
    await selectHunk();
    check('composer mounts for hunk B', composer() !== null);
    check('hunk B carries no attachments (binding is per hunk)', chips().length === 0);
    await clickFileRow('a.txt');
    await selectHunk();
    check('returning to hunk A restores its chip', chips().length === 1);

    composer()!.value = 'with image';
    await act(async () => { submitButton()?.click(); await realTick(); });
    const commentCommand = sent.filter((entry) => entry.type === 'code_review_comment').at(-1);
    check(
      'submit sends the comment with the hunk image as `images`',
      commentCommand !== undefined
        && Array.isArray(commentCommand.command.images)
        && (commentCommand.command.images as unknown[]).length === 1
        && (commentCommand.command.images as Array<{ type: string; mimeType: string } & { data?: string }>)[0]?.type === 'image'
        && commentCommand.command.comment === 'with image'
        && commentCommand.command.sessionId === 'sess-cr',
      JSON.stringify(commentCommand?.command),
    );
    check('the comment success settles', settleNext('code_review_comment', snapshotPayload()));
    await act(async () => { await realTick(); });
    check('success clears the submitted image chip', chips().length === 0);
    check('success clears the unchanged submitted text', composer()?.value === '');
    const commentToasts = toasts.length;

    await deliverFiles(fileListOf(pngFile()));
    await flushReads();
    check('the re-attached chip queues for the retry', chips().length === 1);
    composer()!.value = 'retry draft';
    await act(async () => { submitButton()?.click(); await realTick(); });
    const retryCommand = deferreds.find((entry) => entry.type === 'code_review_comment' && !entry.settled);
    if (retryCommand) {
      retryCommand.settled = true;
      retryCommand.reject(Object.assign(new Error('comment failed'), { rpc: true }));
    }
    await act(async () => { await realTick(); });
    check(
      'a rejection preserves the draft text and the image and toasts once',
      chips().length === 1 && composer()?.value === 'retry draft'
        && toasts.length === commentToasts + 1 && toasts[toasts.length - 1].message === 'comment failed',
      JSON.stringify(toasts),
    );
  } finally {
    await unmountPanel();
    if (previousFileReader === undefined) delete (globalThis as Record<string, unknown>).FileReader;
    else (globalThis as Record<string, unknown>).FileReader = previousFileReader;
    if (previousImage === undefined) delete (globalThis as Record<string, unknown>).Image;
    else (globalThis as Record<string, unknown>).Image = previousImage;
    if (previousCreateObjectURL === undefined) delete (URL as unknown as Record<string, unknown>).createObjectURL;
    else (URL as unknown as Record<string, unknown>).createObjectURL = previousCreateObjectURL;
    if (previousRevokeObjectURL === undefined) delete (URL as unknown as Record<string, unknown>).revokeObjectURL;
    else (URL as unknown as Record<string, unknown>).revokeObjectURL = previousRevokeObjectURL;
  }
}
// ---- shared STT recorder: transcript appends to the captured hunk ----
{
  class FakeMediaRecorder {
    static isTypeSupported(): boolean { return true; }
    state = 'inactive';
    mimeType = 'audio/webm';
    ondataavailable: ((event: { data: Blob }) => void) | null = null;
    onstop: (() => void) | null = null;
    start(): void { this.state = 'recording'; }
    stop(): void { this.state = 'inactive'; this.onstop?.(); }
  }
  class FakeAudioContext {
    decodeAudioData(): Promise<{ numberOfChannels: number; sampleRate: number; getChannelData: (index: number) => Float32Array }> {
      return Promise.resolve({
        numberOfChannels: 1,
        sampleRate: 16000,
        getChannelData: () => new Float32Array([0.5, -0.5]),
      });
    }
    close(): Promise<void> { return Promise.resolve(); }
  }
  const fakeStream = { getTracks: () => [{ stop: () => {} }] };
  const previousNavigator = (globalThis as Record<string, unknown>).navigator;
  const previousMediaRecorder = (globalThis as Record<string, unknown>).MediaRecorder;
  const previousAudioContext = (globalThis as Record<string, unknown>).AudioContext;
  let recorder: FakeMediaRecorder | null = null;
  (globalThis as Record<string, unknown>).navigator = {
    userAgent: 'mini-dom',
    mediaDevices: { getUserMedia: async () => fakeStream },
  };
  (globalThis as Record<string, unknown>).MediaRecorder = class extends FakeMediaRecorder {
    constructor() {
      super();
      recorder = this;
    }
  };
  (globalThis as Record<string, unknown>).AudioContext = FakeAudioContext;
  const micButton = () => mini.document.body.querySelector('.code-review__composer-btn--mic') as MiniNode | null;
  const composer = () => mini.document.body.querySelector('.code-review__comment-input') as MiniNode | null;
  const settleTranscribe = async (text: string): Promise<void> => {
    const deferred = deferreds.find((entry) => entry.type === 'stt_transcribe' && !entry.settled);
    if (!deferred) throw new Error('missing stt_transcribe');
    deferred.settled = true;
    deferred.resolve({ text });
    await act(async () => {
      for (let i = 0; i < 5; i += 1) {
        await realTick();
      }
    });
  };
  try {
    await mountPanel();
    check(
      'STT composer fixture page settles',
      settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')),
    );
    await act(async () => { await realTick(); });
    const header = mini.document.body.querySelector('.code-review__hunk-header') as MiniNode;
    await act(async () => {
      header.click();
      await realTick();
    });
    check('the mic affordance mounts in the composer row', micButton() !== null);
    await act(async () => {
      micButton()!.click();
      for (let i = 0; i < 10; i += 1) {
        await realTick();
      }
    });
    check(
      'startRecording captures the hunk target',
      micButton() !== null && (micButton() as MiniNode).getAttribute('aria-pressed') === 'true',
    );
    recorder?.ondataavailable?.({ data: new Blob([new Uint8Array([1, 2, 3, 4])], { type: 'audio/webm' }) });
    await act(async () => { micButton()!.click(); await realTick(); });
    await act(async () => {
      for (let i = 0; i < 10; i += 1) {
        await realTick();
      }
    });
    const transcribeCommands = sent.filter((entry) => entry.type === 'stt_transcribe');
    const wireSessionId = transcribeCommands[0]?.command.sessionId;
    check(
      'stt_transcribe.sessionId is the owning session, never a hunk key',
      transcribeCommands.length === 1
        && wireSessionId === 'sess-cr'
        && !String(wireSessionId).includes('\0'),
      JSON.stringify(transcribeCommands.map((entry) => entry.command)),
    );
    await settleTranscribe('voice note');
    check(
      'the transcript appends to the captured hunk without auto-submit',
      composer() !== null && (composer()!.value ?? '').includes('voice note')
        && countType('code_review_comment') === 0,
      composer() ? String(composer()!.value) : 'no composer',
    );

    await act(async () => {
      micButton()!.click();
      for (let i = 0; i < 10; i += 1) {
        await realTick();
      }
    });
    recorder?.ondataavailable?.({ data: new Blob([new Uint8Array([1])], { type: 'audio/webm' }) });
    await act(async () => { micButton()!.click(); await realTick(); });
    await act(async () => {
      for (let i = 0; i < 10; i += 1) {
        await realTick();
      }
    });
    const secondCommands = sent.filter((entry) => entry.type === 'stt_transcribe');
    check(
      'a second capture keeps the owning session id on the wire',
      secondCommands.length === 2 && secondCommands[1]?.command.sessionId === 'sess-cr'
        && !String(secondCommands[1]?.command.sessionId).includes('\0'),
      JSON.stringify(secondCommands.map((entry) => entry.command)),
    );
    await clickFileRow('b.txt');
    check('fresh b.txt page settles for the STT scenario', settleNext('code_review_file_diff', pagePayload('b.txt', '+B-APPEND')));
    await act(async () => { await realTick(); });
    const bHeader = mini.document.body.querySelector('.code-review__hunk-header');
    await act(async () => { (bHeader as MiniNode).click(); await realTick(); });
    const secondDeferred = deferreds.find((entry) => entry.type === 'stt_transcribe' && !entry.settled);
    if (secondDeferred) {
      secondDeferred.settled = true;
      secondDeferred.resolve({ text: 'late voice note' });
    }
    await act(async () => {
      for (let i = 0; i < 5; i += 1) {
        await realTick();
      }
    });
    check(
      'the current hunk does not receive a transcript captured for another hunk',
      countType('code_review_comment') === 0 && toasts.length === 0
        && !(composer()!.value ?? '').includes('late voice note'),
      composer() ? String(composer()!.value) : 'no composer',
    );
    await clickFileRow('a.txt');
    const aHeader = mini.document.body.querySelector('.code-review__hunk-header');
    await act(async () => { (aHeader as MiniNode).click(); await realTick(); });
    check(
      'switching hunks during transcription appends to the original hunk draft',
      countType('code_review_comment') === 0 && toasts.length === 0
        && (composer()!.value ?? '').includes('voice note')
        && (composer()!.value ?? '').includes('late voice note'),
      composer() ? String(composer()!.value) : 'no composer',
    );
  } finally {
    await unmountPanel();
    if (previousNavigator === undefined) delete (globalThis as Record<string, unknown>).navigator;
    else (globalThis as Record<string, unknown>).navigator = previousNavigator;
    if (previousMediaRecorder === undefined) delete (globalThis as Record<string, unknown>).MediaRecorder;
    else (globalThis as Record<string, unknown>).MediaRecorder = previousMediaRecorder;
    if (previousAudioContext === undefined) delete (globalThis as Record<string, unknown>).AudioContext;
    else (globalThis as Record<string, unknown>).AudioContext = previousAudioContext;
  }
}


// ---------------------------------------------------------------------------
// Final-audit blockers on the REAL panel component: ACK-gap Stop ownership,
// stale-identity protection, malformed-ACK rejection, identity-deduped async
// error toasts, and the accepted-comment follow re-pin.
// ---------------------------------------------------------------------------

// ---- ACK gap: Esc and a second button click issue typed code_review_stop ----
{
  const gapSnapshot = () => ({ ...snapshotPayload() });
  await mountPanel(gapSnapshot);
  check('fresh page settles for the ACK gap scenario', settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')));
  await act(async () => { await realTick(); });

  await act(async () => {
    (autoReviewActionButton() as MiniNode).click();
    await realTick();
  });
  const gapAutoAck = deferreds.find((entry) => entry.type === 'code_review_auto_review' && !entry.settled);
  if (gapAutoAck) {
    gapAutoAck.settled = true;
    gapAutoAck.resolve({ ok: true, state: 'triaging', batchId: 1 });
  }
  await act(async () => { await realTick(); });
  check(
    'the ACK keeps the button in the Stop state with no poll',
    (autoReviewActionButton() as MiniNode).className.includes('code-review__action--stop'),
  );

  const stopsBeforeGapEsc = countType('code_review_stop');
  const closesBeforeGapEsc = closeCalls;
  await pressPanelEscape();
  check(
    'Esc during the ACK gap sends typed code_review_stop, not a close',
    countType('code_review_stop') === stopsBeforeGapEsc + 1
      && sent.filter((entry) => entry.type === 'code_review_stop').at(-1)?.command.operation === 'autoReview'
      && closeCalls === closesBeforeGapEsc,
    JSON.stringify(sent.filter((entry) => entry.type === 'code_review_stop')),
  );
  check(
    'the gap Esc starts no duplicate batch',
    countType('code_review_auto_review') === 1,
  );
  const gapStopAck = deferreds.find((entry) => entry.type === 'code_review_stop' && !entry.settled);
  if (gapStopAck) {
    gapStopAck.settled = true;
    gapStopAck.resolve({ ok: true, operation: 'autoReview' });
  }
  await act(async () => { await realTick(); });
  // A second click during the gap is a Stop, never a duplicate start.
  await act(async () => {
    (autoReviewActionButton() as MiniNode).click();
    await realTick();
  });
  check(
    'a second button click during the gap sends typed code_review_stop',
    countType('code_review_stop') === stopsBeforeGapEsc + 2
      && sent.filter((entry) => entry.type === 'code_review_stop').at(-1)?.command.operation === 'autoReview',
    JSON.stringify(sent.filter((entry) => entry.type === 'code_review_stop')),
  );
  check('the second gap click never duplicate-starts', countType('code_review_auto_review') === 1);

  // Highlight ACK gap: the same contract, released by the first snapshot
  // carrying the accepted generation.
  await act(async () => {
    (highlightActionButton() as MiniNode).click();
    await realTick();
  });
  const gapHighlightAck = deferreds.find((entry) => entry.type === 'code_review_highlight' && !entry.settled);
  if (gapHighlightAck) {
    gapHighlightAck.settled = true;
    gapHighlightAck.resolve({ ok: true, state: 'running', highlightGeneration: 1 });
  }
  await act(async () => { await realTick(); });
  check(
    'the highlight ACK keeps its Stop through the gap',
    (highlightActionButton() as MiniNode).className.includes('code-review__action--stop'),
  );
  const highlightGapStops = countType('code_review_stop');
  await act(async () => {
    (highlightActionButton() as MiniNode).click();
    await realTick();
  });
  check(
    'a second highlight click during its gap stops without a duplicate start',
    countType('code_review_stop') === highlightGapStops + 1
      && sent.filter((entry) => entry.type === 'code_review_stop').at(-1)?.command.operation === 'highlight'
      && countType('code_review_highlight') === 1,
  );

  // The matching snapshot identity releases both owners.
  await act(async () => {
    advanceClock(1500);
    await realTick();
    settleNext('code_review_snapshot', {
      ...gapSnapshot(),
      autoReviewState: 'idle',
      autoReviewBatchId: 2,
      highlightState: 'idle',
      highlightGeneration: 2,
    });
  });
  await act(async () => { await realTick(); });
  check(
    'matching-or-newer snapshot identities release the owners',
    (autoReviewActionButton() as MiniNode).className.includes('code-review__action--stop') === false
      && (highlightActionButton() as MiniNode).className.includes('code-review__action--stop') === false,
  );
  await unmountPanel();
}

// ---- stale idle snapshots and malformed ACKs cannot break the owners ----
{
  await mountPanel(snapshotPayload);
  check('fresh page settles for the stale-identity scenario', settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')));
  await act(async () => { await realTick(); });
  await act(async () => {
    (autoReviewActionButton() as MiniNode).click();
    await realTick();
  });
  const staleAck = deferreds.find((entry) => entry.type === 'code_review_auto_review' && !entry.settled);
  if (staleAck) {
    staleAck.settled = true;
    staleAck.resolve({ ok: true, state: 'triaging', batchId: 9 });
  }
  await act(async () => { await realTick(); });
  // A stale idle snapshot (older batch id) must not clear the accepted owner.
  await act(async () => {
    advanceClock(1500);
    await realTick();
    settleNext('code_review_snapshot', {
      ...snapshotPayload(),
      autoReviewState: 'idle',
      autoReviewBatchId: 3,
    });
  });
  await act(async () => { await realTick(); });
  check(
    'a stale idle snapshot keeps the owner busy (no close gap, no duplicate start)',
    (autoReviewActionButton() as MiniNode).className.includes('code-review__action--stop'),
  );
  const stopsAfterStale = countType('code_review_stop');
  await pressPanelEscape();
  check(
    'Esc after a stale idle snapshot still stops the accepted batch',
    countType('code_review_stop') === stopsAfterStale + 1
      && sent.filter((entry) => entry.type === 'code_review_stop').at(-1)?.command.operation === 'autoReview',
  );
  await act(async () => {
    advanceClock(1500);
    await realTick();
    settleNext('code_review_snapshot', { ...snapshotPayload(), autoReviewBatchId: 9 });
  });
  await act(async () => { await realTick(); });

  // A malformed ACK rejects: only its own owner clears, exactly one toast.
  const toastsBeforeBadAck = toasts.length;
  await act(async () => {
    (autoReviewActionButton() as MiniNode).click();
    await realTick();
  });
  const badAck = deferreds.find((entry) => entry.type === 'code_review_auto_review' && !entry.settled);
  if (badAck) {
    badAck.settled = true;
    badAck.resolve({ ok: true, submitted: 3, skipped: 0 });
  }
  await act(async () => { await realTick(); });
  check(
    'a malformed ACK clears its owner with exactly one contextual toast',
    toasts.length === toastsBeforeBadAck + 1
      && (autoReviewActionButton() as MiniNode).className.includes('code-review__action--stop') === false,
    JSON.stringify(toasts),
  );
  await unmountPanel();
}

// ---- a delayed idle snapshot with an older revision cannot unbusy the wire ----
{
  await mountPanel(snapshotPayload);
  check('fresh page settles for the revision-staleness scenario', settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')));
  await act(async () => { await realTick(); });
  await act(async () => {
    (autoReviewActionButton() as MiniNode).click();
    await realTick();
  });
  const revisionAck = deferreds.find((entry) => entry.type === 'code_review_auto_review' && !entry.settled);
  if (revisionAck) {
    revisionAck.settled = true;
    revisionAck.resolve({ ok: true, state: 'triaging', batchId: 7 });
  }
  await act(async () => { await realTick(); });
  // The matching snapshot releases the owner and lifts the revision floor.
  await act(async () => {
    advanceClock(1500);
    await realTick();
    settleNext('code_review_snapshot', { ...snapshotPayload(), revision: 12, autoReviewState: 'triaging', autoReviewBatchId: 7 });
  });
  await act(async () => { await realTick(); });
  // A delayed pre-ACK poll response (older revision) lands after the
  // release: it must not regress the projected wire state to idle.
  await act(async () => {
    advanceClock(1500);
    await realTick();
    settleNext('code_review_snapshot', { ...snapshotPayload(), revision: 5, autoReviewState: 'idle', autoReviewBatchId: 0 });
  });
  await act(async () => { await realTick(); });
  check(
    'an older-revision idle snapshot cannot unbusy the released batch',
    (autoReviewActionButton() as MiniNode).className.includes('code-review__action--stop')
      && (autoReviewActionButton() as MiniNode).textContent?.includes('■ Stop'),
  );
  // A newer-revision snapshot still updates normally.
  await act(async () => {
    advanceClock(1500);
    await realTick();
    settleNext('code_review_snapshot', { ...snapshotPayload(), revision: 13, autoReviewState: 'done', autoReviewBatchId: 7 });
  });
  await act(async () => { await realTick(); });
  check(
    'a newer-revision terminal snapshot returns the button to idle',
    (autoReviewActionButton() as MiniNode).textContent === '◐Auto Review',
  );
  await unmountPanel();
}

// ---- restart/reopen: a new reviewId releases pending Stop owners ----
{
  await mountPanel(snapshotPayload);
  check('fresh page settles for the restart-latch scenario', settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')));
  await act(async () => { await realTick(); });
  await act(async () => {
    (autoReviewActionButton() as MiniNode).click();
    (highlightActionButton() as MiniNode).click();
    await realTick();
  });
  const restartAutoAck = deferreds.find((entry) => entry.type === 'code_review_auto_review' && !entry.settled);
  if (restartAutoAck) {
    restartAutoAck.settled = true;
    restartAutoAck.resolve({ ok: true, state: 'triaging', batchId: 9 });
  }
  const restartHighlightAck = deferreds.find((entry) => entry.type === 'code_review_highlight' && !entry.settled);
  if (restartHighlightAck) {
    restartHighlightAck.settled = true;
    restartHighlightAck.resolve({ ok: true, state: 'running', highlightGeneration: 4 });
  }
  await act(async () => { await realTick(); });
  check(
    'accepted owners stay Stop before the restarted snapshot',
    (autoReviewActionButton() as MiniNode).className.includes('code-review__action--stop')
      && (highlightActionButton() as MiniNode).className.includes('code-review__action--stop'),
  );
  await act(async () => {
    advanceClock(1500);
    await realTick();
    settleNext('code_review_snapshot', {
      ...snapshotPayload(),
      reviewId: 'rev-2',
      autoReviewState: 'idle',
      autoReviewBatchId: 1,
      highlightState: 'idle',
      highlightGeneration: 1,
    });
  });
  await act(async () => { await realTick(); });
  check(
    'a new reviewId idle snapshot releases both Stop owners (restart latch)',
    (autoReviewActionButton() as MiniNode).className.includes('code-review__action--stop') === false
      && (highlightActionButton() as MiniNode).className.includes('code-review__action--stop') === false,
  );
  await unmountPanel();
}

// ---- current model identity; typed no-open keeps last-good; unmount stamps ----
{
  await mountPanel(() => ({
    ...snapshotPayload(),
    reviewId: 'rev-stamp-1',
    reviewModelProvider: 'prov',
    reviewModelId: 'nine',
    reviewModelLabel: 'Mock Model Nine',
  }));
  check('fresh page settles for the placeholder scenario', settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')));
  await act(async () => { await realTick(); });
  const hunkHeader = mini.document.body.querySelector('.code-review__hunk-header') as MiniNode;
  await act(async () => { hunkHeader.click(); await realTick(); });
  const modelSelect = () =>
    mini.document.body.querySelector('.code-review__model-select') as MiniNode | null;
  check(
    'the no-catalog selectedLabel shows the current review model',
    modelSelect() !== null
      && (modelSelect()!.options as MiniNode[])[0]?.textContent === 'Mock Model Nine',
    modelSelect() ? String((modelSelect()!.options as MiniNode[])[0]?.textContent) : 'no select',
  );

  const opensBeforePoll = countType('code_review_open');
  const closesBeforePoll = countType('code_review_close');
  await act(async () => {
    advanceClock(1500);
    await realTick();
    const pollDeferred = deferreds.find((entry) => entry.type === 'code_review_snapshot' && !entry.settled);
    if (pollDeferred) {
      pollDeferred.settled = true;
      pollDeferred.reject(typedNotOpenError());
    }
  });
  await act(async () => { await realTick(); });
  await act(async () => { advanceClock(1500); await realTick(); });
  check(
    'typed no-open poll reopens once without toasting ReviewNotOpen',
    countType('code_review_open') === opensBeforePoll + 1
      && countType('code_review_close') === closesBeforePoll
      && toastMentionsNoOpenReview() === false,
    JSON.stringify({ opens: countType('code_review_open') - opensBeforePoll, toasts }),
  );
  const comparison = mini.document.body.querySelector('.code-review__label');
  check(
    'typed no-open poll keeps the last-good snapshot on screen',
    comparison !== null && comparison.textContent === 'HEAD→working tree',
    comparison ? String(comparison.textContent) : 'no label',
  );
  check(
    'poll reopen settles onto the last-good review id',
    settleNext('code_review_open', {
      ...snapshotPayload(),
      reviewId: 'rev-stamp-1',
      reviewModelProvider: 'prov',
      reviewModelId: 'nine',
      reviewModelLabel: 'Mock Model Nine',
    }),
  );
  await act(async () => { await realTick(); });

  const closesBeforeStamp = countType('code_review_close');
  await unmountPanel();
  const stampedCloses = sent.filter((entry) => entry.type === 'code_review_close').slice(closesBeforeStamp);
  check(
    'the unmount close is stamped with the observed review id',
    stampedCloses.length === 1 && stampedCloses[0].command.reviewId === 'rev-stamp-1',
    JSON.stringify(stampedCloses),
  );
}

// ---- reconnect epoch: typed not-open reopens retained args, no ReviewNotOpen toast ----
{
  await mountAlreadyOpenPanel();
  check('fresh page settles for reconnect not-open', settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')));
  await act(async () => { await realTick(); });
  const opensBeforeEpoch = countType('code_review_open');
  toasts = [];
  await rerenderPanel({
    connectionEpoch: 2,
    openArgs: retainedReconnectOpenArgs,
  });
  await act(async () => {
    await realTick();
    rejectUnsettledSnapshotsAsNotOpen();
    await realTick();
  });
  const reopenCommands = sent.filter((entry) => entry.type === 'code_review_open').slice(opensBeforeEpoch);
  const reconnectLabel = mini.document.body.querySelector('.code-review__label');
  check(
    'epoch typed not-open sends exactly one code_review_open with retained from/working-tree args',
    reopenCommands.length === 1
      && reopenCommands[0].command.from === 'origin/mainnet-beta'
      && reopenCommands[0].command.includeWorkingTree === true
      && reopenCommands[0].command.to === undefined,
    JSON.stringify(reopenCommands.map((entry) => entry.command)),
  );
  check(
    'epoch typed not-open does not toast ReviewNotOpen',
    toastMentionsNoOpenReview() === false,
    JSON.stringify(toasts),
  );
  check(
    'epoch typed not-open keeps the last-good comparison label',
    reconnectLabel !== null && reconnectLabel.textContent === 'HEAD→working tree',
    reconnectLabel ? String(reconnectLabel.textContent) : 'no label',
  );
  check(
    'epoch reopen snapshot settles',
    settleNext('code_review_open', {
      ...snapshotPayload(),
      snapshotId: 'snap-reopen',
      reviewId: 'rev-reopen',
      comparisonLabel: 'origin/mainnet-beta→working tree',
    }),
  );
  await act(async () => { await realTick(); });
  const afterReopenLabel = mini.document.body.querySelector('.code-review__label');
  check(
    'reopen snapshot applies the new comparison label',
    afterReopenLabel !== null && afterReopenLabel.textContent === 'origin/mainnet-beta→working tree',
    afterReopenLabel ? String(afterReopenLabel.textContent) : 'no label',
  );
  await unmountPanel();
}

// ---- same-process epoch probe with a live snapshotId preserves, no second open ----
{
  await mountAlreadyOpenPanel();
  check('fresh page settles for reconnect preserve', settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')));
  await act(async () => { await realTick(); });
  const opensBeforePreserve = countType('code_review_open');
  toasts = [];
  await rerenderPanel({
    connectionEpoch: 2,
    openArgs: retainedReconnectOpenArgs,
  });
  await act(async () => {
    await realTick();
    while (settleNext('code_review_snapshot', { ...snapshotPayload(), snapshotId: 'snap-live' })) {
      await realTick();
    }
  });
  check(
    'live epoch snapshot sends zero code_review_open',
    countType('code_review_open') === opensBeforePreserve,
    String(countType('code_review_open') - opensBeforePreserve),
  );
  check(
    'live epoch snapshot does not toast ReviewNotOpen',
    toastMentionsNoOpenReview() === false,
    JSON.stringify(toasts),
  );
  await unmountPanel();
}

// ---- poll not-open while a reopen owner is in flight does not open twice ----
{
  await mountPanel();
  check('fresh page settles for in-flight reopen owner', settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')));
  await act(async () => { await realTick(); });
  const opensBeforeOwner = countType('code_review_open');
  toasts = [];
  await rerenderPanel({ connectionEpoch: 2 });
  await act(async () => {
    await realTick();
    rejectUnsettledSnapshotsAsNotOpen();
    await realTick();
  });
  check(
    'first not-open claims exactly one reopen',
    countType('code_review_open') === opensBeforeOwner + 1,
    String(countType('code_review_open') - opensBeforeOwner),
  );
  await act(async () => {
    advanceClock(1500);
    await realTick();
    rejectUnsettledSnapshotsAsNotOpen();
    await realTick();
  });
  check(
    'poll not-open while reopen is in flight does not open a second time',
    countType('code_review_open') === opensBeforeOwner + 1,
    String(countType('code_review_open') - opensBeforeOwner),
  );
  check(
    'poll not-open while reopen is in flight does not toast ReviewNotOpen',
    toastMentionsNoOpenReview() === false,
    JSON.stringify(toasts),
  );
  await unmountPanel();
}

// ---- onToast identity change must not close/open; remount stamps then opens ----
{
  await mountPanel(() => ({ ...snapshotPayload(), reviewId: 'rev-live-1' }));
  check('fresh page settles for the churn scenario', settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')));
  await act(async () => { await realTick(); });
  const opensBeforeChurn = countType('code_review_open');
  const closesBeforeChurn = countType('code_review_close');
  await rerenderPanel({
    onToast: (message: string, error: boolean) => {
      toasts.push({ message, error });
    },
  });
  await act(async () => { await realTick(); });
  check(
    'a new onToast identity does not close or reopen the controller',
    countType('code_review_open') === opensBeforeChurn
      && countType('code_review_close') === closesBeforeChurn,
    JSON.stringify({
      opens: countType('code_review_open') - opensBeforeChurn,
      closes: countType('code_review_close') - closesBeforeChurn,
    }),
  );

  await unmountPanel();
  const firstClose = sent.filter((entry) => entry.type === 'code_review_close').at(-1);
  check(
    'first unmount close carries the live review id',
    firstClose?.command.reviewId === 'rev-live-1',
    JSON.stringify(firstClose?.command),
  );
  await mountPanel(() => ({ ...snapshotPayload(), reviewId: 'rev-live-2' }));
  check('fresh page settles for the remount scenario', settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')));
  await act(async () => { await realTick(); });
  check(
    'a real remount opens a new review after the previous close',
    countType('code_review_open') === 1,
  );
  await unmountPanel();
  const remountClose = sent.filter((entry) => entry.type === 'code_review_close').at(-1);
  check(
    'second unmount close carries the remounted review id',
    remountClose?.command.reviewId === 'rev-live-2',
    JSON.stringify(remountClose?.command),
  );
}

// ---- unmount before open ack must close the returned controller ----
{
  sent = [];
  deferreds = [];
  toasts = [];
  closeCalls = 0;
  container = mini.document.createElement('div');
  mini.document.body.appendChild(container);
  root = createRoot(container as unknown as Element);
  await act(async () => {
    root!.render(
      createElement(CodeReviewPanel, {
        sendCommand: harnessSendCommand,
        sessionId: 'sess-cr',
        openArgs: {},
        connectionEpoch: 1,
        liveSettings: { sttConfigured: true },
        onToast: (message: string, error: boolean) => {
          toasts.push({ message, error });
        },
        onClose: () => {
          closeCalls += 1;
        },
      }),
    );
    await realTick();
  });
  check('deferred-open boot sends one code_review_open', countType('code_review_open') === 1);
  const opensBeforeOrphan = countType('code_review_open');
  const closesBeforeOrphan = countType('code_review_close');
  await unmountPanel();
  check(
    'unmount before the open ack sends no close',
    countType('code_review_close') === closesBeforeOrphan,
  );
  check(
    'late open after unmount settles',
    settleNext('code_review_open', { ...snapshotPayload(), reviewId: 'rev-orphan-1' }),
  );
  await act(async () => { await realTick(); });
  const orphanCloses = sent.filter((entry) => entry.type === 'code_review_close' && entry.command.reviewId === 'rev-orphan-1');
  check(
    'late open after unmount stamps a close for that review',
    orphanCloses.length === 1 && orphanCloses[0].command.sessionId === 'sess-cr',
    JSON.stringify(sent.filter((entry) => entry.type === 'code_review_close')),
  );
  check(
    'late open after unmount does not reopen',
    countType('code_review_open') === opensBeforeOrphan,
  );
  check(
    'late open after unmount does not toast',
    toasts.length === 0,
    JSON.stringify(toasts),
  );
}
// ---- async error toasts: once per identity; cancelled is quiet ----
{
  await mountPanel(snapshotPayload);
  check('fresh page settles for the error toast scenario', settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')));
  await act(async () => { await realTick(); });

  await act(async () => {
    advanceClock(1500);
    await realTick();
    settleNext('code_review_snapshot', {
      ...snapshotPayload(),
      autoReviewState: 'failed',
      autoReviewError: { message: 'triage returned invalid JSON', code: 'invalid-triage' },
      autoReviewBatchId: 3,
      highlightState: 'failed',
      highlightError: 'provider down',
      highlightGeneration: 4,
    });
  });
  await act(async () => { await realTick(); });
  check(
    'batch and highlight failures own exactly one contextual App toast each',
    toasts.length === 2
      && toasts[0].message === 'Auto review: triage returned invalid JSON'
      && toasts[1].message === 'Highlight: provider down',
    JSON.stringify(toasts),
  );
  check('failed operations paint no inline error', mini.document.body.querySelector('.code-review__error') === null);
  // Repeated polls of the same failing identities stay quiet.
  await act(async () => {
    advanceClock(1500);
    await realTick();
    settleNext('code_review_snapshot', {
      ...snapshotPayload(),
      autoReviewState: 'failed',
      autoReviewError: { message: 'triage returned invalid JSON', code: 'invalid-triage' },
      autoReviewBatchId: 3,
      highlightState: 'failed',
      highlightError: 'provider down',
      highlightGeneration: 4,
    });
  });
  await act(async () => { await realTick(); });
  check('repeated polls of the same identity never re-toast', toasts.length === 2, JSON.stringify(toasts));

  // The typed user-stop cancellation is the ONLY quiet terminal category.
  await act(async () => {
    advanceClock(1500);
    await realTick();
    settleNext('code_review_snapshot', {
      ...snapshotPayload(),
      autoReviewState: 'failed',
      autoReviewError: { message: 'Auto review stopped', code: 'cancelled' },
      autoReviewBatchId: 5,
    });
  });
  await act(async () => { await realTick(); });
  check(
    'the typed cancelled category stays quiet and never surfaces',
    toasts.length === 2
      && toasts.every((toast) => !toast.message.includes('Auto review stopped')),
    JSON.stringify(toasts),
  );
  await unmountPanel();
}

// ---- live InvalidTriage: exact Display copy toasts; cancelled stays the only quiet terminal ----
{
  await mountPanel(snapshotPayload);
  check('fresh page settles for the live InvalidTriage toast scenario', settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')));
  await act(async () => { await realTick(); });
  const toastsBeforeClick = toasts.length;
  await act(async () => {
    (autoReviewActionButton() as MiniNode).click();
    await realTick();
  });
  const liveTriageAck = deferreds.find((entry) => entry.type === 'code_review_auto_review' && !entry.settled);
  if (liveTriageAck) {
    liveTriageAck.settled = true;
    liveTriageAck.resolve({ ok: true, state: 'triaging', batchId: 11 });
  }
  await act(async () => { await realTick(); });
  await act(async () => {
    advanceClock(1500);
    await realTick();
    settleNext('code_review_snapshot', {
      ...snapshotPayload(),
      revision: 4,
      autoReviewState: 'failed',
      autoReviewError: {
        message: 'Auto review triage returned invalid JSON',
        code: 'invalid-triage',
      },
      autoReviewBatchId: 11,
    });
  });
  await act(async () => { await realTick(); });
  check(
    'a snapshot with autoReviewError.message Auto review triage returned invalid JSON produces a toast notice',
    toasts.length === toastsBeforeClick + 1
      && toasts[toasts.length - 1].message === 'Auto review: Auto review triage returned invalid JSON'
      && toasts[toasts.length - 1].error === true,
    JSON.stringify(toasts.slice(toastsBeforeClick)),
  );
  check(
    'InvalidTriage is not the cancelled quiet category and does not hide the overlay',
    (autoReviewActionButton() as MiniNode).textContent === '◐Auto Review'
      && mini.document.body.querySelector('#code-review-auto-review') !== null
      && mini.document.body.querySelector('.code-review__error') === null
      && !(mini.document.body.textContent ?? '').includes('invalid JSON')
      && closeCalls === 0,
    JSON.stringify({
      button: (autoReviewActionButton() as MiniNode | null)?.textContent,
      closeCalls,
    }),
  );
  await unmountPanel();
}

// ---- accepted same-hunk comment follows only while pinned; rejection never retargets ----
{
  await mountPanel(snapshotPayload);
  check('fresh page settles for the follow event scenario', settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')));
  await act(async () => { await realTick(); });
  const thread = () => mini.document.body.querySelector('.code-review__thread') as MiniNode | null;
  const pinThread = async (): Promise<void> => {
    await act(async () => {
      const header = mini.document.body.querySelector('.code-review__hunk-header') as MiniNode | null;
      header?.click();
      await realTick();
    });
  };
  await pinThread();
  check('the selected hunk thread mounts', thread() !== null);
  const scrollUp = async (): Promise<void> => {
    await act(async () => {
      const node = thread();
      if (!node) return;
      node.scrollHeight = 200;
      node.clientHeight = 50;
      // The first scroll absorbs the geometry change the way a settled
      // frame does; only the second one is a genuine user scroll.
      node.dispatchEvent({ type: 'scroll' });
      node.scrollTop = 0;
      node.dispatchEvent({ type: 'scroll' });
      await realTick();
    });
  };
  await scrollUp();
  check('the user scrolled up suspends follow', thread()?.scrollTop === 0, `scrollTop=${thread()?.scrollTop} sh=${thread()?.scrollHeight} ch=${thread()?.clientHeight}`);

  const composer = () => mini.document.body.querySelector('.code-review__comment-input') as MiniNode | null;
  composer()!.value = 'accepted follow proof';
  await act(async () => { (composer() as MiniNode).click(); await realTick(); });
  await act(async () => {
    const send = (mini.document.body.querySelector('.code-review__action--send') as MiniNode | null);
    send?.click();
    await realTick();
  });
  check('the accepted comment settles', settleNext('code_review_comment', {
    ...snapshotPayload(),
    threads: [{
      identity: {
        snapshotId: 'snap-1',
        path: 'a.txt',
        oldStart: 0,
        oldCount: 0,
        newStart: 1,
        newCount: 1,
        contentHash: aFreshHash,
      },
      comments: [{ role: 'user', text: 'accepted follow proof', partial: false }],
      isStreaming: true,
      streamingText: 'reply tail is growing',
    }],
  }));
  await act(async () => { await realTick(); });
  check(
    'an accepted same-hunk comment does not jump an unpinned reading position',
    thread()?.scrollTop === 0,
    thread() ? `${thread()?.scrollTop}/${thread()?.scrollHeight}` : 'no thread',
  );

  // A rejected comment never emits a follow event: scroll up again, submit,
  // reject, and the viewport stays frozen.
  await scrollUp();
  check('the user scrolled up again before the rejection', thread()?.scrollTop === 0);
  const commentCountBeforeReject = countType('code_review_comment');
  composer()!.value = 'rejected proof';
  await act(async () => {
    (mini.document.body.querySelector('.code-review__action--send') as MiniNode | null)?.click();
    await realTick();
  });
  check('the rejected submit sends one comment request', countType('code_review_comment') === commentCountBeforeReject + 1);
  const commentDeferred = deferreds.find((entry) => entry.type === 'code_review_comment' && !entry.settled);
  check('the rejection request is in flight', commentDeferred !== undefined);
  if (commentDeferred) {
    commentDeferred.settled = true;
    commentDeferred.reject(Object.assign(new Error('stale hunk'), { rpc: true }));
  }
  await act(async () => { await realTick(); });
  check(
    'a rejected comment leaves the viewport frozen with exactly one toast',
    thread()?.scrollTop === 0 && toasts.length === 1,
    JSON.stringify(toasts),
  );
  await unmountPanel();
}

{
  await mountPanel(snapshotPayload);
  check('TLDR fixture loads real diff', settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')));
  await act(async () => { await realTick(); });
  const button = () => mini.document.body.querySelector('#code-review-tldr') as MiniNode | null;
  check('TLDR action mounts', button() !== null);
  await act(async () => { button()!.click(); await realTick(); });
  check('TLDR dispatches once', countType('code_review_tldr') === 1);
  await act(async () => {
    check('TLDR ACK settles', settleNext('code_review_tldr', { ok: true, state: 'running', tldrGeneration: 2 }));
    await realTick();
  });
  const polls = deferreds.filter((entry) => entry.type === 'code_review_snapshot' && !entry.settled);
  check('TLDR ACK requests snapshot immediately', polls.length > 0);
  await act(async () => {
    const latest = polls.at(-1);
    if (latest) { latest.settled = true; latest.resolve({ ...snapshotPayload(), tldrGeneration: 1 }); }
    advanceClock(151);
    await realTick();
  });
  check('older idle TLDR generation cannot release owner', button()!.textContent.includes('Stop'));
  await act(async () => { button()!.click(); await realTick(); });
  check('TLDR Stop carries typed operation and session', sent.filter((entry) => entry.type === 'code_review_stop').at(-1)?.command.operation === 'tldr'
    && sent.filter((entry) => entry.type === 'code_review_stop').at(-1)?.command.sessionId === 'sess-cr');
  await act(async () => {
    check('TLDR Stop ACK settles', settleNext('code_review_stop', { ok: true }));
    await realTick();
  });
  await act(async () => {
    const latest = deferreds.filter((entry) => entry.type === 'code_review_snapshot' && !entry.settled).at(-1);
    check('TLDR Stop requests reconciliation', latest !== undefined);
    if (latest) { latest.settled = true; latest.resolve({ ...snapshotPayload(), tldrGeneration: 2, tldrState: 'failed', tldrError: 'stopped' }); }
    await realTick();
  });
  check('terminal TLDR restores retry action', !button()!.textContent.includes('Stop'));
  await act(async () => { button()!.click(); await realTick(); });
  check('TLDR retries with a new request', countType('code_review_tldr') === 2);
  await act(async () => {
    check('malformed TLDR ACK settles', settleNext('code_review_tldr', { ok: true, state: 'running', tldrGeneration: '3' }));
    await realTick();
  });
  check('malformed TLDR ACK releases only its pending owner', !button()!.textContent.includes('Stop') && toasts.some((toast) => toast.message.includes('generation')));
  await unmountPanel();
}

{
  await mountPanel(snapshotPayload);
  check('fresh page settles for ACK-immediate snapshot', settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')));
  await act(async () => { await realTick(); });
  await act(async () => {
    (highlightActionButton() as MiniNode).click();
    await realTick();
  });
  const highlightAck = deferreds.find((entry) => entry.type === 'code_review_highlight' && !entry.settled);
  check('highlight ACK is in flight', highlightAck !== undefined);
  const preAckSnapshots = deferreds.filter((entry) => entry.type === 'code_review_snapshot' && !entry.settled);
  check('a pre-ACK snapshot poll is in flight', preAckSnapshots.length > 0);
  if (highlightAck) {
    highlightAck.settled = true;
    highlightAck.resolve({ ok: true, state: 'running', highlightGeneration: 1 });
  }
  await act(async () => { await realTick(); });
  const postAckSnapshot = deferreds.find(
    (entry) => entry.type === 'code_review_snapshot' && !entry.settled && !preAckSnapshots.includes(entry),
  );
  check(
    'highlight ACK requests a snapshot without waiting POLL_MS',
    postAckSnapshot !== undefined,
  );
  const staleHighlight = {
    identity: {
      snapshotId: 'snap-1',
      path: 'a.txt',
      oldStart: 0,
      oldCount: 0,
      newStart: 1,
      newCount: 1,
      contentHash: aFreshHash,
    },
    findings: [{ category: 'critical', side: 'new', startLine: 1, endLine: 1, summary: 'stale paint', evidence: 'FRESH-APPEND' }],
  };
  for (const stale of preAckSnapshots) {
    if (stale.settled) continue;
    stale.settled = true;
    stale.resolve({
      ...snapshotPayload(),
      revision: 0,
      highlightState: 'done',
      highlightGeneration: 1,
      reviewFindings: [staleHighlight],
    });
  }
  await act(async () => { await realTick(); });
  check('a stale pre-ACK snapshot cannot paint a finding row',
    mini.document.body.querySelector('.code-review__hunk') !== null
      && mini.document.body.querySelector('.code-review__line.has-finding--danger') === null
      && !mini.document.body.textContent.includes('stale paint'));
  if (postAckSnapshot) {
    postAckSnapshot.settled = true;
    postAckSnapshot.resolve({
      ...snapshotPayload(),
      highlightState: 'done',
      highlightGeneration: 1,
      reviewFindings: [{
        identity: {
          snapshotId: 'snap-1',
          path: 'a.txt',
          oldStart: 0,
          oldCount: 0,
          newStart: 1,
          newCount: 1,
          contentHash: aFreshHash,
        },
        findings: [{ category: 'critical', side: 'new', startLine: 1, endLine: 1, summary: 'ack paint', evidence: 'FRESH-APPEND' }],
      }],
    });
  }
  await act(async () => { await realTick(); });
  const painted = mini.document.body.querySelector('.code-review__line.has-finding--danger') as MiniNode | null;
  check('post-ACK snapshot paints the precise existing row', painted !== null && painted.textContent.includes('FRESH-APPEND'));
  await unmountPanel();
}


// ---- a comment's workspace-path chip dispatches against the dock's owning
//      session id, and every comment body hydrates through the shared
//      MarkdownBody (per committed body) instead of the dock's own
//      whole-thread re-scan ----
{
  const threadIdentity = {
    snapshotId: 'snap-1',
    path: 'a.txt',
    oldStart: 0,
    oldCount: 0,
    newStart: 1,
    newCount: 1,
    contentHash: aFreshHash,
  };
  await mountPanel(() => ({
    ...snapshotPayload(),
    threads: [
      {
        identity: threadIdentity,
        comments: [
          {
            role: 'assistant',
            text: 'Fixes `src/lib.rs:12` — the guard was inverted.',
            partial: false,
          },
        ],
        isStreaming: false,
      },
    ],
  }));
  check(
    'comment-chip fixture page settles',
    settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')),
  );
  await act(async () => { await realTick(); });
  const chipHunkHeader = mini.document.body.querySelector('.code-review__hunk-header') as MiniNode | null;
  check('comment-chip fixture renders the hunk header', chipHunkHeader !== null);
  await act(async () => {
    chipHunkHeader?.click();
    await realTick();
  });

  const card = mini.document.body.querySelector('.code-review__comment') as MiniNode | null;
  check('comment card renders in the dock', card !== null);
  check(
    'comment card is stamped with the dock owning session',
    card?.getAttribute('data-session-id') === 'sess-cr',
    card?.getAttribute('data-session-id') ?? 'absent',
  );
  check(
    'comment card owns the chip text it renders',
    card?.textContent.includes('src/lib.rs:12') === true,
    card?.textContent,
  );

  // This harness materializes dangerouslySetInnerHTML as TEXT, so the chip's
  // own <code> node does not exist to click. The stub carries exactly what the
  // dispatcher reads off a chip (its data-md-path/line and its ancestor walk);
  // the OWNER it resolves is the real rendered card above, whose attributes
  // the dispatcher reads itself. Without the stamped session id the same call
  // returns false and the chip stays inert.
  const opened: Array<{ path: string; line: number | undefined; owner: MarkdownPathOwner }> = [];
  const chip = {
    textContent: 'src/lib.rs:12',
    getAttribute: (name: string): string | null =>
      name === 'data-md-path' ? 'src/lib.rs' : name === 'data-md-line' ? '12' : null,
    closest: (selector: string): unknown =>
      selector === '[data-md-path]' ? chip : selector === '[data-session-id]' ? card : null,
  };
  let prevented = false;
  const handled = dispatchMarkdownPathClick(
    { target: chip, preventDefault: () => { prevented = true; } },
    (path, line, owner) => { opened.push({ path, line, owner }); },
  );
  check(
    'a comment chip dispatches against the dock owning session',
    handled
      && prevented
      && opened.length === 1
      && opened[0]!.owner.sessionId === 'sess-cr'
      && opened[0]!.path === 'src/lib.rs'
      && opened[0]!.line === 12,
    JSON.stringify(opened),
  );
  await unmountPanel();
}

// ---- live thinking has no comment card to own it: the thread itself carries
//      the owning session id, so a path reference in streamed thinking still
//      resolves instead of staying inert ----
{
  const threadIdentity = {
    snapshotId: 'snap-1',
    path: 'a.txt',
    oldStart: 0,
    oldCount: 0,
    newStart: 1,
    newCount: 1,
    contentHash: aFreshHash,
  };
  await mountPanel(() => ({
    ...snapshotPayload(),
    threads: [
      {
        identity: threadIdentity,
        comments: [],
        isStreaming: true,
        streamingText: '',
        streamingThinking: 'reading `src/lib.rs:12`',
      },
    ],
    isStreaming: true,
    activeCount: 1,
  }));
  check(
    'live-thinking fixture page settles',
    settleNext('code_review_file_diff', pagePayload('a.txt', '+FRESH-APPEND')),
  );
  await act(async () => { await realTick(); });
  const liveHunkHeader = mini.document.body.querySelector('.code-review__hunk-header') as MiniNode | null;
  await act(async () => {
    liveHunkHeader?.click();
    await realTick();
  });
  const liveThinking = mini.document.body.querySelector('.code-review__thread details.thinking') as MiniNode | null;
  check('streamed thinking renders in the dock', liveThinking !== null);
  check(
    'streamed thinking sits outside any comment card',
    liveThinking !== null && liveThinking.closest('.code-review__comment') === null,
  );
  const threadOwnerNode = mini.document.body.querySelector('.code-review__thread-content') as MiniNode | null;
  check(
    'the thread carries the owning session for markdown outside a comment card',
    threadOwnerNode?.getAttribute('data-session-id') === 'sess-cr',
    threadOwnerNode?.getAttribute('data-session-id') ?? 'absent',
  );
  const thinkingOpened: Array<{ path: string; owner: MarkdownPathOwner }> = [];
  const thinkingChip = {
    textContent: 'src/lib.rs:12',
    getAttribute: (name: string): string | null =>
      name === 'data-md-path' ? 'src/lib.rs' : name === 'data-md-line' ? '12' : null,
    closest: (selector: string): unknown =>
      selector === '[data-md-path]'
        ? thinkingChip
        : selector === '[data-session-id]'
          ? threadOwnerNode
          : null,
  };
  const handled = dispatchMarkdownPathClick(
    { target: thinkingChip, preventDefault: () => {} },
    (path, _line, owner) => { thinkingOpened.push({ path, owner }); },
  );
  check(
    'a chip in streamed thinking dispatches against the thread owner',
    handled
      && thinkingOpened.length === 1
      && thinkingOpened[0]!.owner.sessionId === 'sess-cr'
      && thinkingOpened[0]!.path === 'src/lib.rs',
    JSON.stringify(thinkingOpened),
  );
  await unmountPanel();
}


{
  await mountPanel();
  await act(async () => {
    advanceClock(1500);
    await realTick();
  });
  const firstHang = deferreds.filter((entry) => entry.type === 'code_review_snapshot' && !entry.settled);
  check('cadence starts a snapshot poll', firstHang.length > 0);
  await act(async () => {
    advanceClock(1500);
    await realTick();
  });
  const secondHang = deferreds.filter((entry) => entry.type === 'code_review_snapshot' && !entry.settled);
  check(
    'a hung snapshot poll does not block the next cadence',
    secondHang.length >= firstHang.length + 1,
    `first=${firstHang.length} second=${secondHang.length}`,
  );
  await unmountPanel();
}

// ---- /commit operand: boot + typed no-open reopen carry only the pinned commit ----
{
  const sha = 'd'.repeat(40);
  await mountPanel(snapshotPayload, 'a.txt', { openArgs: { commit: sha } });
  const bootOpens = sent.filter((entry) => entry.type === 'code_review_open');
  check(
    'commit operand boot sends code_review_open with only commit (mutual exclusion)',
    bootOpens.length === 1
      && bootOpens[0].command.commit === sha
      && !('from' in bootOpens[0].command)
      && !('to' in bootOpens[0].command)
      && !('includeWorkingTree' in bootOpens[0].command),
    JSON.stringify(bootOpens.map((entry) => entry.command)),
  );

  const opensBeforePoll = countType('code_review_open');
  await act(async () => {
    advanceClock(1500);
    await realTick();
    const pollDeferred = deferreds.find((entry) => entry.type === 'code_review_snapshot' && !entry.settled);
    if (pollDeferred) {
      pollDeferred.settled = true;
      pollDeferred.reject(typedNotOpenError());
    }
  });
  await act(async () => { await realTick(); });
  await act(async () => { advanceClock(1500); await realTick(); });
  const reopens = sent.filter((entry) => entry.type === 'code_review_open').slice(bootOpens.length);
  check(
    'typed no-open poll reopens once from the retained commit operand without toasting',
    countType('code_review_open') === opensBeforePoll + 1
      && reopens.length === 1
      && reopens[0].command.commit === sha
      && !('from' in reopens[0].command)
      && !('to' in reopens[0].command)
      && !('includeWorkingTree' in reopens[0].command)
      && !toastMentionsNoOpenReview(),
    JSON.stringify(reopens.map((entry) => entry.command)),
  );
  check('commit reopen settles onto a snapshot', settleNext('code_review_open', snapshotPayload()));
  await act(async () => { await realTick(); });
  await unmountPanel();
}

// ---- CommitPicker: search/select/typed-ref/Escape behavior over parsed rows ----
{
  const sha40 = 'a'.repeat(40);
  const sha64 = 'b'.repeat(64);
  const parent = 'c'.repeat(40);
  const parsed = normalizeCodeReviewCommits({
    commits: [
      { commit: sha40, parentCommit: null, parentCount: 0, subject: 'add commit picker', author: 'Pi Bot', authorDate: '2026-10-24T10:00:00+02:00' },
      { commit: sha64, parentCommit: parent, parentCount: 1, subject: 'fix web regression', author: 'Ada', authorDate: '2026-10-23T09:30:00+02:00' },
    ],
    truncated: false,
  });
  if (!parsed.ok) throw new Error('picker fixture rows must parse');
  const rows = parsed.list.commits;

  const picked: string[] = [];
  let closeCalls = 0;
  const errors: string[] = [];
  const pickerContainer = mini.document.createElement('div');
  mini.document.body.appendChild(pickerContainer);
  const pickerRoot = createRoot(pickerContainer as unknown as Element);
  const renderPicker = (overrides: Record<string, unknown> = {}) => {
    pickerRoot.render(createElement(CommitPicker, {
      open: true,
      rows,
      loading: false,
      onSelect: (ref: string) => { picked.push(ref); },
      onClose: () => { closeCalls += 1; },
      onError: (message: string) => { errors.push(message); },
      ...overrides,
    }));
  };
  const pickerOptions = () => mini.document.body.querySelectorAll('.command-picker__option') as MiniNode[];
  const pickerSearch = () => mini.document.body.querySelector('.command-picker__search') as MiniNode;
  const typeInSearch = async (text: string) => {
    await act(async () => {
      const input = pickerSearch();
      input.value = text;
      input.dispatchEvent({ type: 'input', target: input, preventDefault() {}, stopPropagation() {} });
      await realTick();
    });
  };
  const pressKey = async (key: string) => {
    await act(async () => {
      pickerSearch().dispatchEvent({ type: 'keydown', key, preventDefault() {}, stopPropagation() {} });
      await realTick();
    });
  };

  await act(async () => { renderPicker(); await realTick(); });
  // The mini-DOM matcher parses only ONE bracketed attribute per selector
  // (a compound `[a="x"][b="y"]` never matches), so the dialog/listbox are
  // located by class and their ARIA identity is asserted as attribute checks.
  const pickerDialog = () => mini.document.body.querySelector('.command-picker__popover') as MiniNode | null;
  const pickerList = () => mini.document.body.querySelector('.command-picker__list') as MiniNode | null;
  const dialogNode = pickerDialog();
  const listNode = pickerList();
  check(
    'open picker renders an accessible dialog with both rows',
    dialogNode !== null
      && dialogNode.getAttribute('role') === 'dialog'
      && dialogNode.getAttribute('aria-label') === 'Recent commits'
      && listNode !== null
      && listNode.getAttribute('role') === 'listbox'
      && listNode.getAttribute('aria-label') === 'Recent commits'
      && pickerSearch().getAttribute('aria-label') === 'Search commits'
      && pickerOptions().length === 2,
  );
  check(
    'rows show short sha, author/date, and subject',
    (dialogNode?.textContent ?? '').includes(sha40.slice(0, 12))
      && (dialogNode?.textContent ?? '').includes('Pi Bot · 2026-10-24T10:00:00+02:00')
      && (dialogNode?.textContent ?? '').includes('add commit picker'),
  );

  await typeInSearch('PICKER');
  check('filter narrows rows case-insensitively by subject', pickerOptions().length === 1 && pickerOptions()[0].getAttribute('data-commit') === sha40);
  await typeInSearch(sha64.slice(0, 8));
  check('filter matches a hash prefix', pickerOptions().length === 1 && pickerOptions()[0].getAttribute('data-commit') === sha64);
  await typeInSearch('HEAD~1');
  check(
    'a no-match query renders the typed-ref hint',
    pickerOptions().length === 0
      && ((mini.document.body.querySelector('.command-picker__hint') as MiniNode).textContent ?? '').includes('press Enter to review the typed ref'),
  );

  await pressKey('Enter');
  check('enter with no matching row picks the typed ref', picked.length === 1 && picked[0] === 'HEAD~1' && closeCalls === 0);
  await typeInSearch('a b c');
  await pressKey('Enter');
  check(
    'enter with a multi-token tail errors instead of picking',
    errors.length === 1 && errors[0] === 'Usage: /commit [<ref>]' && picked.length === 1,
  );

  await typeInSearch('');
  await pressKey('ArrowDown');
  await pressKey('ArrowDown');
  await pressKey('Enter');
  check(
    'arrow keys move the active row and enter picks its full hash without closing',
    picked.length === 2 && picked[1] === sha64 && closeCalls === 0 && errors.length === 1,
  );

  await pressKey('Escape');
  check('escape closes without issuing any selection or error', closeCalls === 1 && picked.length === 2 && errors.length === 1);

  await act(async () => { renderPicker({ open: false }); await realTick(); });
  check('closed picker renders nothing', pickerDialog() === null && pickerList() === null && pickerOptions().length === 0);

  await act(async () => { renderPicker({ rows: [], loading: true }); await realTick(); });
  check(
    'loading renders the hint status instead of rows',
    pickerOptions().length === 0 && (mini.document.body.querySelector('.command-picker__hint') as MiniNode).getAttribute('role') === 'status',
  );
  await pressKey('Enter');
  await pressKey('ArrowDown');
  await act(async () => { renderPicker({ loading: true }); await realTick(); });
  check('loading hides the rows even when the host still holds rows', pickerOptions().length === 0);
  await pressKey('Enter');
  check(
    'enter while loading never selects a hidden row',
    picked.length === 2 && errors.length === 1 && closeCalls === 1,
  );

  await act(async () => { renderPicker(); await realTick(); });
  await act(async () => { pickerOptions()[0].click(); await realTick(); });
  check('clicking a row picks its full hash', picked.length === 3 && picked[2] === sha40);

  await act(async () => { pickerRoot.unmount(); await realTick(); });
  pickerContainer.remove();
}

// ---- mixed commit operands are rejected before any busy state or wire traffic ----
{
  const sha = 'e'.repeat(40);
  sent = [];
  deferreds = [];
  toasts = [];
  closeCalls = 0;
  container = mini.document.createElement('div');
  mini.document.body.appendChild(container);
  root = createRoot(container as unknown as Element);
  await act(async () => {
    root!.render(
      createElement(CodeReviewPanel, {
        sendCommand: harnessSendCommand,
        sessionId: 'sess-cr',
        openArgs: { commit: sha, from: 'main' },
        connectionEpoch: 1,
        liveSettings: { sttConfigured: true },
        onToast: (message: string, error: boolean) => {
          toasts.push({ message, error });
        },
        onClose: () => {
          closeCalls += 1;
        },
      }),
    );
    await realTick();
  });
  await act(async () => { advanceClock(1500); await realTick(); });
  check(
    'conflated commit+from openArgs dispatch no code_review_open or snapshot (unrelated model load may ride)',
    countType('code_review_open') === 0 && countType('code_review_snapshot') === 0,
    JSON.stringify(sent.map((entry) => entry.type)),
  );
  check(
    'the rejected scope toasts exactly once with the exclusion message and never enters the busy state',
    toasts.length === 1
      && toasts[0].message === 'code_review_open commit excludes from, to, and includeWorkingTree'
      && toasts[0].error === true
      && mini.document.body.querySelector('.code-review__loader') === null,
    JSON.stringify(toasts),
  );
  await unmountPanel();
}

// Selecting an accepted finding past the explorer line cap must keep an
// already-expanded review window (soft cap 4000) and the finding's range end.
{
  const DEEP_LINES = 4000;
  const FINDING_START = 2500;
  const FINDING_END = 2502;
  const deepLines = Array.from({ length: DEEP_LINES }, (_, index) => ({
    kind: 'addition' as const,
    newNo: index + 1,
    text: `+deep-${index + 1}`,
  }));
  const deepHeader = `@@ -0,0 +1,${DEEP_LINES} @@`;
  const deepHash = unifiedHunkSha256(deepHeader, deepLines);
  const deepPath = 'deep.txt';
  const deepPage = (cursor: number) => {
    const next = Math.min(cursor + 1000, DEEP_LINES);
    const hasMore = next < DEEP_LINES;
    return {
      snapshotId: 'snap-1',
      path: deepPath,
      status: 'added',
      binary: false,
      cursor,
      ...(hasMore ? { nextCursor: next } : {}),
      hasMore,
      byteCapped: false,
      totalLines: DEEP_LINES,
      hunkCount: 1,
      hunks: [
        {
          index: 0,
          header: deepHeader,
          oldStart: 0,
          oldCount: 0,
          newStart: 1,
          newCount: DEEP_LINES,
          contentHash: deepHash,
          totalLines: DEEP_LINES,
          complete: true,
          lineStart: cursor,
          lines: deepLines.slice(cursor, next),
        },
      ],
    };
  };
  const deepSnapshot = () => ({
    ...snapshotPayload(),
    totalInsertions: DEEP_LINES,
    files: [onDemandFile(deepPath, deepHash, DEEP_LINES)],
    reviewFindings: [
      {
        identity: {
          snapshotId: 'snap-1',
          path: deepPath,
          oldStart: 0,
          oldCount: 0,
          newStart: 1,
          newCount: DEEP_LINES,
          contentHash: deepHash,
        },
        findings: [
          {
            category: 'critical',
            side: 'new',
            startLine: FINDING_START,
            endLine: FINDING_END,
            summary: 'deep finding past explorer cap',
            evidence: `+deep-${FINDING_END}`,
          },
        ],
      },
    ],
  });
  const lineCount = () => mini.document.body.querySelectorAll('.code-review__line').length;

  await mountPanel(deepSnapshot, deepPath);
  for (const cursor of [0, 1000, 2000, 3000]) {
    check(
      `deep page ${cursor} settles`,
      settleNext('code_review_file_diff', deepPage(cursor)),
    );
    await act(async () => {
      await realTick();
    });
  }
  await scrollDiffToBottom();
  check(
    'near-bottom growth renders the soft-cap window of 4000 lines',
    lineCount() === DEEP_LINES,
    String(lineCount()),
  );
  const beforeSelect = lineCount();
  const nextIssue = mini.document.body.querySelector(
    '[aria-label="Issue navigation"] button[aria-label="Next issue"]',
  ) as MiniNode | null;
  check('the deep finding is selectable', nextIssue !== null && nextIssue.getAttribute('disabled') === null);
  await act(async () => {
    nextIssue?.click();
    await realTick();
  });
  const endRow = (mini.document.body.querySelectorAll('.code-review__line') as MiniNode[]).find(
    (row) => row.querySelector('.code-review__line-no--new')?.textContent === String(FINDING_END),
  ) ?? null;
  check(
    'selecting the finding keeps the expanded 4000-line window',
    lineCount() === beforeSelect && lineCount() === DEEP_LINES,
    String(lineCount()),
  );
  check(
    'finding range end 2502 stays visible and selected',
    endRow !== null
      && endRow.className.includes('is-finding-selected')
      && endRow.querySelector('.code-review__line-text')?.textContent === `+deep-${FINDING_END}`,
    endRow?.textContent ?? 'missing end row',
  );
  await unmountPanel();
}

console.log(`codeReviewPanel: ${ran} checks, ${failures.length} failures`);
for (const failure of failures) console.error(`  FAIL ${failure}`);
process.exit(failures.length > 0 ? 1 : 0);