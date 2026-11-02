#!/usr/bin/env node
// Focused regression for src/CopyButton.tsx — clipboard write is a boolean
// Promise (true only on actual success) and the button tracks mount + attempt
// generation so a stale write, a rapid re-click, a payload/kind change, or
// unmount can never flash the wrong label.
//
// Run through `npm run build`, which bundles this file with Vite's installed
// esbuild into a disposable Node-compatible module before executing the
// focused assertions (same pattern as scripts/transcript.test.ts).
//
// Exit codes: 0 = every assertion held; 1 = a regression.
import { createElement } from 'react';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import { COPY_FLASH_MS, CopyButton, copyTextToClipboard } from '../src/CopyButton.tsx';
import { redactedClipboardText } from '../src/ansi.ts';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: boolean, detail?: string) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

function restoreProperty(target: object, key: string, had: boolean, previous: unknown) {
  if (had) {
    Object.defineProperty(target, key, { configurable: true, writable: true, value: previous });
  } else {
    delete (target as Record<string, unknown>)[key];
  }
}

async function withClipboardWrite<T>(writeText: ((text: string) => Promise<void>) | undefined, fn: () => Promise<T>): Promise<T> {
  const hadNavigator = Object.prototype.hasOwnProperty.call(globalThis, 'navigator');
  const previousNavigator = (globalThis as { navigator?: unknown }).navigator;
  Object.defineProperty(globalThis, 'navigator', {
    configurable: true,
    writable: true,
    value: writeText === undefined ? {} : { clipboard: { writeText } },
  });
  try {
    return await fn();
  } finally {
    restoreProperty(globalThis, 'navigator', hadNavigator, previousNavigator);
  }
}

type ExecCommandResult = boolean | ((command: string) => boolean);

async function withExecCommand<T>(
  result: ExecCommandResult,
  fn: () => Promise<T>,
  hooks?: {
    createElement?: () => never;
    appendChild?: () => never;
    select?: () => never;
    remove?: () => never;
  },
): Promise<T> {
  const hadDocument = Object.prototype.hasOwnProperty.call(globalThis, 'document');
  const previousDocument = (globalThis as { document?: unknown }).document;
  const hadNavigator = Object.prototype.hasOwnProperty.call(globalThis, 'navigator');
  const previousNavigator = (globalThis as { navigator?: unknown }).navigator;
  Object.defineProperty(globalThis, 'navigator', {
    configurable: true,
    writable: true,
    value: {},
  });
  Object.defineProperty(globalThis, 'document', {
    configurable: true,
    writable: true,
    value: {
      body: {
        appendChild() {
          if (hooks?.appendChild) hooks.appendChild();
        },
      },
      createElement() {
        if (hooks?.createElement) hooks.createElement();
        return {
          value: '',
          style: {},
          setAttribute() {},
          select() {
            if (hooks?.select) hooks.select();
          },
          remove() {
            if (hooks?.remove) hooks.remove();
          },
        };
      },
      execCommand(command: string) {
        return typeof result === 'function' ? result(command) : result;
      },
    },
  });
  try {
    return await fn();
  } finally {
    restoreProperty(globalThis, 'document', hadDocument, previousDocument);
    restoreProperty(globalThis, 'navigator', hadNavigator, previousNavigator);
  }
}

{
  const written: string[] = [];
  const ok = await withClipboardWrite(async (text) => { written.push(text); }, async () => (
    copyTextToClipboard(redactedClipboardText('export token = supersecret'))
  ));
  check('clipboard writeText success returns true', ok === true);
  check(
    'clipboard writeText receives the redacted command, never the raw secret',
    written.length === 1 && written[0] === 'export token=[REDACTED]' && !written[0].includes('supersecret'),
    JSON.stringify(written),
  );

  const rejected = await withClipboardWrite(async () => { throw new Error('denied'); }, async () => (
    copyTextToClipboard('echo hi')
  ));
  check('clipboard writeText rejection returns false, never success', rejected === false);

  const fallbackTrue = await withExecCommand(true, async () => copyTextToClipboard('echo hi'));
  check('execCommand true fallback returns true', fallbackTrue === true);

  const fallbackFalse = await withExecCommand(false, async () => copyTextToClipboard('echo hi'));
  check('execCommand false fallback returns false', fallbackFalse === false);

  const fallbackThrow = await withExecCommand(() => { throw new Error('blocked'); }, async () => (
    copyTextToClipboard('echo hi')
  ));
  check('execCommand throw fallback returns false', fallbackThrow === false);

  const createThrow = await withExecCommand(true, async () => copyTextToClipboard('echo hi'), {
    createElement: () => { throw new Error('create'); },
  });
  check('createElement throw fallback returns false', createThrow === false);

  const appendThrow = await withExecCommand(true, async () => copyTextToClipboard('echo hi'), {
    appendChild: () => { throw new Error('append'); },
  });
  check('appendChild throw fallback returns false', appendThrow === false);

  const selectThrow = await withExecCommand(true, async () => copyTextToClipboard('echo hi'), {
    select: () => { throw new Error('select'); },
  });
  check('select throw fallback returns false', selectThrow === false);

  const removeThrow = await withExecCommand(true, async () => copyTextToClipboard('echo hi'), {
    remove: () => { throw new Error('remove'); },
  });
  check('remove throw still resolves boolean true after successful copy', removeThrow === true);
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
  style: Record<string, string>;
  listeners: Map<string, Set<MiniListener>>;
  text: string;
  value?: string;
  checked?: boolean;
  selected?: boolean;
  disabled?: boolean;
  innerHTML: string;
  textContent: string;
  className: string;
  id: string;
  host?: MiniNode;
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
};

function createMiniDom(): { window: MiniWindow; document: MiniDocument } {
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
      style: {},
      listeners: new Map(),
      text: '',
      innerHTML: '',
      get nodeValue() {
        return nodeType === 3 || nodeType === 8 ? node.text : null;
      },
      set nodeValue(value: string | null) {
        if (nodeType === 3 || nodeType === 8) node.text = value ?? '';
      },
      get data() {
        return nodeType === 3 || nodeType === 8 ? node.text : '';
      },
      set data(value: string) {
        if (nodeType === 3 || nodeType === 8) node.text = value;
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
    } as MiniNode;
    return node;
  };

  const matches = (node: MiniNode, selector: string): boolean => {
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
    setTimeout: (fn: () => void) => globalThis.setTimeout(fn, 0) as unknown as number,
    clearTimeout: (id: number) => globalThis.clearTimeout(id),
    addEventListener() {},
    removeEventListener() {},
  };
  document.defaultView = windowObj;
  return { window: windowObj, document };
}

const pendingTimers: Array<{ id: number; fn: () => void }> = [];
let nextTimerId = 1;
let nativeSetTimeout = globalThis.setTimeout;
let nativeClearTimeout = globalThis.clearTimeout;
function installTestTimers() {
  pendingTimers.length = 0;
  nextTimerId = 1;
  nativeSetTimeout = globalThis.setTimeout;
  nativeClearTimeout = globalThis.clearTimeout;
  globalThis.setTimeout = ((fn: (...args: unknown[]) => void, ms?: number, ...args: unknown[]) => {
    if (ms !== undefined && ms > 0) {
      const id = nextTimerId;
      nextTimerId += 1;
      pendingTimers.push({ id, fn: () => fn(...args) });
      return id;
    }
    return nativeSetTimeout(fn as TimerHandler, ms, ...args);
  }) as typeof setTimeout;
  globalThis.clearTimeout = ((id: number) => {
    const index = pendingTimers.findIndex((timer) => timer.id === id);
    if (index >= 0) pendingTimers.splice(index, 1);
    else nativeClearTimeout(id);
  }) as typeof clearTimeout;
}
function flushTimers() {
  const due = pendingTimers.splice(0);
  for (const timer of due) timer.fn();
}

function installMiniDom() {
  const mini = createMiniDom();
  Object.defineProperty(globalThis, 'window', { configurable: true, writable: true, value: mini.window });
  Object.defineProperty(globalThis, 'document', { configurable: true, writable: true, value: mini.document });
  Object.defineProperty(globalThis, 'navigator', {
    configurable: true,
    writable: true,
    value: { clipboard: { writeText: async () => undefined } },
  });
  globalThis.HTMLElement = mini.window.HTMLElement as unknown as typeof HTMLElement;
  globalThis.SVGElement = mini.window.SVGElement as unknown as typeof SVGElement;
  globalThis.Element = mini.window.Element as unknown as typeof Element;
  globalThis.Node = mini.window.Node as unknown as typeof Node;
  globalThis.Text = mini.window.Text as unknown as typeof Text;
  (globalThis as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  mini.window.setTimeout = ((fn: () => void, ms?: number) => globalThis.setTimeout(fn, ms)) as MiniWindow['setTimeout'];
  mini.window.clearTimeout = ((id: number) => globalThis.clearTimeout(id)) as MiniWindow['clearTimeout'];
  return mini;
}

function buttonOf(container: MiniNode): MiniNode {
  const button = container.querySelector('button');
  if (!button) throw new Error('missing button');
  return button;
}

{
  const ssr = renderToStaticMarkup(createElement(CopyButton, {
    className: 'tool-card__command-copy',
    kind: 'command',
    text: 'export token = supersecret',
  }));
  check(
    'SSR markup keeps Copy command idle label',
    ssr.includes('class="tool-card__command-copy"')
      && ssr.includes('aria-label="Copy command"')
      && ssr.includes('aria-live="polite"')
      && ssr.includes('type="button"')
      && ssr.includes('>copy</button>'),
    ssr,
  );
}

{
  const previousWindow = (globalThis as { window?: unknown }).window;
  const previousDocument = (globalThis as { document?: unknown }).document;
  const previousNavigator = (globalThis as { navigator?: unknown }).navigator;
  const previousSetTimeout = globalThis.setTimeout;
  const previousClearTimeout = globalThis.clearTimeout;
  const hadWindow = Object.prototype.hasOwnProperty.call(globalThis, 'window');
  const hadDocument = Object.prototype.hasOwnProperty.call(globalThis, 'document');
  const hadNavigator = Object.prototype.hasOwnProperty.call(globalThis, 'navigator');
  installTestTimers();
  const mini = installMiniDom();

  try {
    const container = mini.document.createElement('div');
    mini.document.body.appendChild(container);
    const root: Root = createRoot(container as unknown as Element);

    let latestText = '';
    const writeQueue: Array<{ text: string; resolve: () => void; reject: (error: Error) => void }> = [];
    (mini.window.document.defaultView as MiniWindow);
    Object.defineProperty(globalThis, 'navigator', {
      configurable: true,
      writable: true,
      value: {
        clipboard: {
          writeText(text: string) {
            latestText = text;
            return new Promise<void>((resolve, reject) => {
              writeQueue.push({
                text,
                resolve: () => resolve(),
                reject: (error: Error) => reject(error),
              });
            });
          },
        },
      },
    });

    await act(async () => {
      root.render(createElement(CopyButton, { className: 'bash-copy', kind: 'command', text: 'export token = supersecret' }));
    });
    let button = buttonOf(container);
    check('client idle label is copy / Copy command', button.textContent === 'copy' && button.getAttribute('aria-label') === 'Copy command');

    await act(async () => { button.click(); });
    check('in-flight click keeps idle until the write settles', button.textContent === 'copy');
    check(
      'click writes the redacted command, never the raw secret',
      latestText === 'export token=[REDACTED]' && !latestText.includes('supersecret'),
      latestText,
    );
    await act(async () => { writeQueue.shift()?.resolve(); await Promise.resolve(); await Promise.resolve(); });
    check('success flash is copied', button.textContent === 'copied' && button.getAttribute('aria-label') === 'Command copied');
    await act(async () => { flushTimers(); });
    check('success flash resets to copy after COPY_FLASH_MS', button.textContent === 'copy' && button.getAttribute('aria-label') === 'Copy command');
    check('reset uses the 1200ms flash window', COPY_FLASH_MS === 1200);

    await act(async () => { button.click(); });
    await act(async () => { writeQueue.shift()?.reject(new Error('denied')); await Promise.resolve(); await Promise.resolve(); });
    check('rejection flash is copy failed, never copied', button.textContent === 'copy failed' && button.getAttribute('aria-label') === 'Copy failed');
    await act(async () => { flushTimers(); });
    check('failure flash resets to copy', button.textContent === 'copy');

    await act(async () => {
      root.render(createElement(CopyButton, { className: 'bash-copy', kind: 'output', text: 'stdout sk-ABCDEFGHIJKLMNOPQRST' }));
    });
    button = buttonOf(container);
    check('kind/text change resets to idle Copy output', button.textContent === 'copy' && button.getAttribute('aria-label') === 'Copy output');

    await act(async () => { button.click(); });
    await act(async () => { writeQueue.shift()?.resolve(); await Promise.resolve(); await Promise.resolve(); });
    check('output success aria-label is Output copied', button.textContent === 'copied' && button.getAttribute('aria-label') === 'Output copied');
    await act(async () => {
      root.render(createElement(CopyButton, { className: 'tool-card__process-copy', kind: 'command', text: 'export token = supersecret' }));
    });
    button = buttonOf(container);
    check('process-copy idle label is copy / Copy command', button.textContent === 'copy' && button.getAttribute('aria-label') === 'Copy command');
    await act(async () => { button.click(); });
    check(
      'process-copy click writes the redacted command, never the raw secret',
      latestText === 'export token=[REDACTED]' && !latestText.includes('supersecret'),
      latestText,
    );
    await act(async () => { writeQueue.shift()?.resolve(); await Promise.resolve(); await Promise.resolve(); });
    check('process-copy success flash is copied / Command copied', button.textContent === 'copied' && button.getAttribute('aria-label') === 'Command copied');
    await act(async () => { flushTimers(); });
    check('process-copy success flash resets to idle Copy command', button.textContent === 'copy' && button.getAttribute('aria-label') === 'Copy command');

    await act(async () => { button.click(); });
    await act(async () => { writeQueue.shift()?.reject(new Error('denied')); await Promise.resolve(); await Promise.resolve(); });
    check('process-copy rejection flash is copy failed / Copy failed', button.textContent === 'copy failed' && button.getAttribute('aria-label') === 'Copy failed');
    await act(async () => { flushTimers(); });
    check('process-copy failure flash resets to copy', button.textContent === 'copy');

    await act(async () => { button.click(); });
    const first = writeQueue.shift();
    await act(async () => { button.click(); });
    const second = writeQueue.shift();
    await act(async () => { second?.reject(new Error('denied')); await Promise.resolve(); await Promise.resolve(); });
    check('rapid second rejection wins while first write is outstanding', button.textContent === 'copy failed');
    await act(async () => { first?.resolve(); await Promise.resolve(); await Promise.resolve(); });
    check('stale earlier success is ignored after a newer attempt', button.textContent === 'copy failed');
    await act(async () => { flushTimers(); });
    check('newer attempt still resets after its own flash window', button.textContent === 'copy');
    await act(async () => { button.click(); });
    await act(async () => { writeQueue.shift()?.resolve(); await Promise.resolve(); await Promise.resolve(); });
    check('successful copy schedules exactly one flash timer', pendingTimers.length === 1, String(pendingTimers.length));
    await act(async () => { root.unmount(); });
    check('unmount clears the active flash timer', pendingTimers.length === 0, String(pendingTimers.length));
    const remount = mini.document.createElement('div');
    mini.document.body.appendChild(remount);
    const remountRoot = createRoot(remount as unknown as Element);
    await act(async () => {
      remountRoot.render(createElement(CopyButton, { className: 'bash-copy', kind: 'command', text: 'echo hi' }));
    });
    check('fresh mount after active-timer unmount stays idle copy', buttonOf(remount).textContent === 'copy');
    await act(async () => { remountRoot.unmount(); });

    const pendingContainer = mini.document.createElement('div');
    mini.document.body.appendChild(pendingContainer);
    const pendingRoot = createRoot(pendingContainer as unknown as Element);
    await act(async () => {
      pendingRoot.render(createElement(CopyButton, { className: 'bash-copy', kind: 'command', text: 'echo pending' }));
    });
    button = buttonOf(pendingContainer);

    await act(async () => { button.click(); });
    const unmounted = writeQueue.shift();
    await act(async () => { pendingRoot.unmount(); });
    let unmountThrew = false;
    try {
      await act(async () => { unmounted?.resolve(); await Promise.resolve(); await Promise.resolve(); });
    } catch {
      unmountThrew = true;
    }
    check('unmount before write settles does not throw', unmountThrew === false);
  } finally {
    globalThis.setTimeout = previousSetTimeout;
    globalThis.clearTimeout = previousClearTimeout;
    restoreProperty(globalThis, 'window', hadWindow, previousWindow);
    restoreProperty(globalThis, 'document', hadDocument, previousDocument);
    restoreProperty(globalThis, 'navigator', hadNavigator, previousNavigator);
  }
}

if (failures.length > 0) {
  console.error(`copyButton: ${failures.length} assertion(s) failed:`);
  for (const f of failures) console.error(`  - ${f}`);
  process.exit(1);
}
console.log(`copyButton: ${ran} assertions passed`);
process.exit(0);
