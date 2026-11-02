#!/usr/bin/env node
// Chat tmux PTY overlay regression for src/chat/TmuxTerminal.tsx +
// TmuxPanel "Open terminal" + ChatApp overlay wiring. Mini-DOM + mocked
// xterm (scripts/__mocks__/xterm.ts) so attach/input/resize/exit/detach
// are observable without a real emulator. Exit 0 = every assertion held.
import type { act as Act, createElement as CreateElement } from 'react';
import type { createRoot as CreateRoot, Root } from 'react-dom/client';
import type * as TmuxTerminalModule from '../src/chat/TmuxTerminal.tsx';
import type * as TmuxPanelModule from '../src/chat/TmuxPanel.tsx';
import type * as ProtocolModule from '../src/chat/protocol.ts';
import type { RunnerCommand } from '../src/chat/protocol.ts';
import type { RpcResponse } from '../src/types.ts';
import { type StorageLike } from '../src/hostToken.ts';
import { subscribeRunnerEvents } from '../src/chat/events.ts';
import { RunnerRegistry } from '../src/chat/runners.ts';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { resetXtermHarness, xtermHarness, Terminal } from './__mocks__/xterm.ts';

let createElement: CreateElement;
let act: Act;
let createRoot: CreateRoot;
let TmuxTerminal: TmuxTerminalModule.TmuxTerminal;
let TmuxPanel: TmuxPanelModule.TmuxPanel;
let protocol: typeof ProtocolModule;
let tmuxTerminalModule: typeof TmuxTerminalModule;
let tmuxPanelModule: typeof TmuxPanelModule;

const realSetTimeout = globalThis.setTimeout.bind(globalThis);
let clockNextId = 1;
const clockPending: Array<{ id: number; fn: () => void; ms: number }> = [];

function fakeSetTimeout(fn: () => void, ms?: number): unknown {
  const id = clockNextId;
  clockNextId += 1;
  clockPending.push({ id, fn, ms: ms ?? 0 });
  return id;
}

function fakeClear(id: unknown): void {
  const index = clockPending.findIndex((entry) => entry.id === id);
  if (index >= 0) clockPending.splice(index, 1);
}

function flushClock(maxMs: number): void {
  const due = clockPending.filter((entry) => entry.ms <= maxMs);
  for (const entry of due) {
    const index = clockPending.indexOf(entry);
    if (index >= 0) clockPending.splice(index, 1);
    entry.fn();
  }
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
  ownerDocument: MiniDoc;
  parentNode: MiniNode | null;
  childNodes: MiniNode[];
  firstChild: MiniNode | null;
  nextSibling: MiniNode | null;
  previousSibling: MiniNode | null;
  attributes: Map<string, string>;
  listeners: Map<string, Set<Listener>>;
  style: Record<string, unknown>;
  value: string;
  type: string;
  text: string;
  scrollHeight: number;
  clientHeight: number;
  clientWidth: number;
  get className(): string;
  set className(value: string);
  get id(): string;
  set id(value: string);
  get textContent(): string;
  set textContent(value: string);
  appendChild(child: MiniNode): MiniNode;
  insertBefore(child: MiniNode, before: MiniNode | null): MiniNode;
  removeChild(child: MiniNode): MiniNode;
  remove(): void;
  setAttribute(name: string, value: string): void;
  getAttribute(name: string): string | null;
  removeAttribute(name: string): void;
  hasAttribute(name: string): boolean;
  addEventListener(type: string, fn: Listener): void;
  removeEventListener(type: string, fn: Listener): void;
  dispatchEvent(event: { type: string; target?: MiniNode } & Record<string, unknown>): boolean;
  click(): void;
  disabled: boolean;
  focus(): void;
  blur(): void;
  contains(node: MiniNode | null): boolean;
  querySelector(selector: string): MiniNode | null;
  querySelectorAll(selector: string): MiniNode[];
  getBoundingClientRect(): {
    top: number;
    left: number;
    width: number;
    height: number;
    bottom: number;
    right: number;
  };
}

interface MiniDoc extends MiniNode {
  documentElement: MiniNode;
  head: MiniNode;
  body: MiniNode;
  activeElement: MiniNode | null;
  defaultView: MiniWindow;
  visibilityState: 'visible' | 'hidden';
  createElement(tag: string): MiniNode;
  createTextNode(text: string): MiniNode;
  createComment(text: string): MiniNode;
  createDocumentFragment(): MiniNode;
  oninput: null;
}

interface MiniWindow {
  document: MiniDoc;
  [key: string]: unknown;
}

class FakeResizeObserver {
  static instances: FakeResizeObserver[] = [];
  callback: () => void;
  observes = 0;
  disconnects = 0;
  constructor(callback: () => void) {
    this.callback = callback;
    FakeResizeObserver.instances.push(this);
  }
  observe(_node: unknown): void {
    this.observes += 1;
  }
  disconnect(): void {
    this.disconnects += 1;
  }
  trigger(): void {
    if (this.disconnects > 0) return;
    this.callback();
  }
}

function triggerHostResize(): void {
  for (const observer of FakeResizeObserver.instances) observer.trigger();
}


function createMiniDom(): { window: MiniWindow; document: MiniDoc } {
  class FakeCtor {}
  const windowListeners = new Map<string, Set<Listener>>();
  const relink = (parent: MiniNode) => {
    parent.firstChild = parent.childNodes[0] ?? null;
    for (let i = 0; i < parent.childNodes.length; i += 1) {
      const child = parent.childNodes[i]!;
      child.parentNode = parent;
      child.previousSibling = parent.childNodes[i - 1] ?? null;
      child.nextSibling = parent.childNodes[i + 1] ?? null;
    }
  };

  const makeNode = (nodeType: number, nodeName: string, owner: MiniDoc | null): MiniNode => {
    const node = {
      nodeType,
      nodeName,
      tagName: nodeType === 1 ? nodeName : undefined,
      ownerDocument: owner as MiniDoc,
      parentNode: null,
      childNodes: [] as MiniNode[],
      firstChild: null,
      nextSibling: null,
      previousSibling: null,
      attributes: new Map<string, string>(),
      listeners: new Map(),
      text: '',
      style: {},
      value: '',
      type: '',
      scrollHeight: 0,
      clientHeight: 0,
      clientWidth: 0,
      disabled: false,
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
      get textContent(): string {
        if (nodeType === 3 || nodeType === 8) return node.text;
        return node.childNodes.map((child) => child.textContent).join('');
      },
      set textContent(value: string) {
        node.childNodes = [];
        node.firstChild = null;
        if (nodeType === 3 || nodeType === 8) {
          node.text = value;
          return;
        }
        if (value !== '') node.appendChild((owner as MiniDoc).createTextNode(value));
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
        if (name === 'type') node.type = String(value);
        if (name === 'disabled') node.disabled = true;
      },
      getAttribute(name: string) {
        return node.attributes.has(name) ? node.attributes.get(name)! : null;
      },
      removeAttribute(name: string) {
        node.attributes.delete(name);
        if (name === 'disabled') node.disabled = false;
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
          preventDefault() {
            payload.defaultPrevented = true;
          },
          stopPropagation() {
            stopped = true;
          },
          stopImmediatePropagation() {
            stopped = true;
          },
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
          fire(path[i]!, `${raw.type}-capture`);
          if (stopped) return !payload.defaultPrevented;
        }
        fire(node, raw.type);
        if (stopped) return !payload.defaultPrevented;
        for (let i = 1; i < path.length; i += 1) {
          fire(path[i]!, raw.type);
          if (stopped) return !payload.defaultPrevented;
        }
        return !payload.defaultPrevented;
      },
      click() {
        if (node.disabled) return;
        node.dispatchEvent({ type: 'click', target: node });
      },
      focus() {},
      blur() {},
      contains(other: MiniNode | null) {
        if (other === node) return true;
        return node.childNodes.some((child) => child.contains(other));
      },
      getBoundingClientRect() {
        const top = Number.parseFloat(String(node.style.top ?? '')) || 0;
        const left = Number.parseFloat(String(node.style.left ?? '')) || 0;
        const height = node.style.height
          ? Number.parseFloat(String(node.style.height))
          : node.clientHeight;
        const width = node.style.width
          ? Number.parseFloat(String(node.style.width))
          : node.clientWidth;
        return { top, left, width, height, bottom: top + height, right: left + width };
      },
      querySelector(selector: string) {
        return node.querySelectorAll(selector)[0] ?? null;
      },
      querySelectorAll(selector: string) {
        const out: MiniNode[] = [];
        const matches = (candidate: MiniNode, selector: string): boolean => {
          const bracket = selector.indexOf('[');
          if (bracket > 0) {
            return matches(candidate, selector.slice(0, bracket).trim()) && matches(candidate, selector.slice(bracket));
          }
          if (selector.startsWith('.')) return candidate.className.split(/\s+/).includes(selector.slice(1));
          if (selector.startsWith('#')) return candidate.id === selector.slice(1);
          if (selector.startsWith('[') && selector.endsWith(']')) {
            const body = selector.slice(1, -1);
            const eq = body.indexOf('=');
            if (eq < 0) return candidate.hasAttribute(body);
            return candidate.getAttribute(body.slice(0, eq)) === body.slice(eq + 1).replace(/^["']|["']$/g, '');
          }
          return candidate.tagName === selector.toUpperCase();
        };
        const visit = (current: MiniNode) => {
          if (current.nodeType === 1 && matches(current, selector)) out.push(current);
          for (const child of current.childNodes) visit(child);
        };
        for (const child of node.childNodes) visit(child);
        return out;
      },
    } as MiniNode;
    return node;
  };

  const document = makeNode(9, '#document', null) as MiniDoc;
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
  document.visibilityState = 'visible';
  document.createElement = (tag: string) => makeNode(1, tag.toUpperCase(), document);
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
  document.querySelector = function (this: MiniNode, selector: string) {
    return (this as unknown as MiniDoc).querySelectorAll(selector)[0] ?? null;
  } as MiniNode['querySelector'];
  document.querySelectorAll = function (this: MiniNode, selector: string) {
    const out: MiniNode[] = [];
    const matches = (candidate: MiniNode, selector: string): boolean => {
      if (selector.startsWith('.')) return candidate.className.split(/\s+/).includes(selector.slice(1));
      if (selector.startsWith('#')) return candidate.id === selector.slice(1);
      return candidate.tagName === selector.toUpperCase();
    };
    const visit = (current: MiniNode) => {
      if (current.nodeType === 1 && matches(current, selector)) out.push(current);
      for (const child of current.childNodes) visit(child);
    };
    visit(document);
    return out;
  } as MiniNode['querySelectorAll'];
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
    ResizeObserver: FakeResizeObserver,
    setTimeout: fakeSetTimeout,
    clearTimeout: fakeClear,
    setInterval: fakeSetTimeout,
    clearInterval: fakeClear,
    requestAnimationFrame: (fn: () => void) => fakeSetTimeout(fn, 0),
    cancelAnimationFrame: (id: unknown) => fakeClear(id),
    getComputedStyle: () => ({}),
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
    innerWidth: 390,
    innerHeight: 844,
    scrollY: 0,
    pageYOffset: 0,
    visualViewport: {
      width: 390,
      height: 844,
      offsetTop: 0,
      offsetLeft: 0,
      scale: 1,
      listeners: new Map<string, Set<Listener>>(),
      addEventListener(type: string, fn: Listener) {
        const set = this.listeners.get(type) ?? new Set();
        set.add(fn);
        this.listeners.set(type, set);
      },
      removeEventListener(type: string, fn: Listener) {
        this.listeners.get(type)?.delete(fn);
      },
      dispatchEvent(raw: { type: string }) {
        for (const fn of [...(this.listeners.get(raw.type) ?? [])]) fn(raw);
        return true;
      },
    },
    addEventListener(type: string, fn: Listener) {
      const set = windowListeners.get(type) ?? new Set();
      set.add(fn);
      windowListeners.set(type, set);
    },
    removeEventListener(type: string, fn: Listener) {
      windowListeners.get(type)?.delete(fn);
    },
    dispatchEvent(raw: { type: string } & Record<string, unknown>) {
      for (const fn of [...(windowListeners.get(raw.type) ?? [])]) fn(raw);
      return true;
    },
  };
  document.defaultView = windowObj;
  return { window: windowObj, document };
}

let mini: { window: MiniWindow; document: MiniDoc };
function installGlobals(): void {
  FakeResizeObserver.instances = [];
  mini = createMiniDom();
  Object.defineProperty(globalThis, 'window', { configurable: true, writable: true, value: mini.window });
  Object.defineProperty(globalThis, 'document', { configurable: true, writable: true, value: mini.document });
  Object.defineProperty(globalThis, 'navigator', { configurable: true, writable: true, value: { userAgent: 'mini-dom' } });
  Object.defineProperty(globalThis, 'ResizeObserver', { configurable: true, writable: true, value: FakeResizeObserver });
  globalThis.HTMLElement = mini.window.HTMLElement as unknown as typeof HTMLElement;
  globalThis.SVGElement = mini.window.SVGElement as unknown as typeof SVGElement;
  globalThis.Element = mini.window.Element as unknown as typeof Element;
  globalThis.Node = mini.window.Node as unknown as typeof Node;
  globalThis.Text = mini.window.Text as unknown as typeof Text;
  globalThis.requestAnimationFrame = mini.window.requestAnimationFrame as unknown as typeof requestAnimationFrame;
  globalThis.cancelAnimationFrame = mini.window.cancelAnimationFrame as unknown as typeof cancelAnimationFrame;
  (globalThis as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
}

interface DeferredCommand {
  command: RunnerCommand;
  resolve: (response: RpcResponse) => void;
  reject: (error: Error) => void;
  settled: boolean;
}

let sent: RunnerCommand[] = [];
let deferreds: DeferredCommand[] = [];
let eventListeners: Array<(frame: unknown) => void> = [];
let closed = 0;
let overlayErrors: string[] = [];
let overlayNotices: string[] = [];
let overlayTokens: string[] = [];

function okResponse(command: string, data: unknown): RpcResponse {
  return { type: 'response', command, success: true, data, sessionId: 'sess-1' };
}

function harnessSend(command: RunnerCommand): Promise<RpcResponse> {
  sent.push(command);
  const { promise, resolve, reject } = Promise.withResolvers<RpcResponse>();
  deferreds.push({ command, resolve, reject, settled: false });
  return promise;
}

function commandsOf(type: string): RunnerCommand[] {
  return sent.filter((command) => command.type === type);
}

function settleNext(type: string, data: unknown): boolean {
  const deferred = deferreds.find((entry) => entry.command.type === type && !entry.settled);
  if (!deferred) return false;
  deferred.settled = true;
  deferred.resolve(okResponse(type, data));
  return true;
}

function settleNextFailure(type: string, error: string): boolean {
  const deferred = deferreds.find((entry) => entry.command.type === type && !entry.settled);
  if (!deferred) return false;
  deferred.settled = true;
  deferred.resolve({ type: 'response', command: type, success: false, error, sessionId: 'sess-1' });
  return true;
}

function rejectNext(type: string, error: Error): boolean {
  const deferred = deferreds.find((entry) => entry.command.type === type && !entry.settled);
  if (!deferred) return false;
  deferred.settled = true;
  deferred.reject(error);
  return true;
}

function dispatchEvent(frame: unknown): void {
  for (const listener of [...eventListeners]) listener(frame);
}

const failures: string[] = [];
let ran = 0;
function check(name: string, condition: boolean, detail = ''): void {
  ran += 1;
  if (!condition) failures.push(detail ? `${name}: ${detail}` : name);
}

let container: MiniNode;
let root: Root | null = null;

async function layoutTerminalHost(width = 800, height = 400): Promise<void> {
  const viewport = mini.document.body.querySelector('.chat-tmux-terminal__viewport');
  if (!viewport) return;
  viewport.clientWidth = width;
  viewport.clientHeight = height;
}

async function mountTerminal(
  session = 'agentlo-pi-rs',
  size = { cols: 120, rows: 36 },
  layoutHost = true,
  tokenLoader?: (hostAuthority: string) => string,
): Promise<void> {
  sent = [];
  deferreds = [];
  eventListeners = [];
  closed = 0;
  overlayErrors = [];
  overlayNotices = [];
  overlayTokens = [];
  FakeResizeObserver.instances = [];
  resetXtermHarness(size);
  container = mini.document.createElement('div');
  mini.document.body.appendChild(container);
  root = createRoot(container as unknown as Element);
  await act(async () => {
    root!.render(
      createElement(TmuxTerminal, {
        runnerId: 'runner-x3',
        runnerLabel: 'x3',
        session,
        authority: '192.0.2.10:8765',
        send: harnessSend,
        onEvent: (callback: (frame: unknown) => void) => {
          eventListeners.push(callback);
          return () => {
            eventListeners = eventListeners.filter((entry) => entry !== callback);
          };
        },
        onClose: () => {
          closed += 1;
        },
        onError: (message: string) => {
          overlayErrors.push(message);
        },
        loadToken: (hostAuthority: string) => {
          overlayTokens.push(hostAuthority);
          if (tokenLoader) return tokenLoader(hostAuthority);
          return hostAuthority === '192.0.2.10:8765' ? 'tok-x3' : '';
        },
      }),
    );
    await realTick();
  });
  if (!layoutHost) return;
  await act(async () => {
    await layoutTerminalHost();
    triggerHostResize();
    await realTick();
  });
}

async function unmountTerminal(): Promise<void> {
  if (!root) return;
  await act(async () => {
    root!.unmount();
  });
  container.remove();
  root = null;
}

async function settleAttach(terminalId = 'tmuxterm-1'): Promise<void> {
  const attach = commandsOf('tmux_terminal_attach')[0] as
    | { type: 'tmux_terminal_attach'; session: string; cols: number; rows: number }
    | undefined;
  check('attach request is in flight before settle', attach !== undefined);
  if (!attach) return;
  check(
    'attach settles',
    settleNext('tmux_terminal_attach', {
      terminalId,
      session: attach.session,
      cols: attach.cols,
      rows: attach.rows,
      paneKey: '%0',
      historyKey: `1:${attach.session}:%0`,
      historyBytes: 0,
      historyComplete: true,
    }),
  );
  await act(async () => {
    await realTick();
  });
  dispatchEvent({
    type: 'tmux_terminal_history',
    terminalId,
    paneKey: '%0',
    historyKey: `1:${attach.session}:%0`,
    data: '',
    final: true,
  });
  await act(async () => {
    await realTick();
  });
}

function buttonByAriaLabel(label: string): MiniNode | null {
  return (
    (mini.document.body.querySelectorAll('button') as MiniNode[]).find(
      (button) => button.getAttribute('aria-label') === label,
    ) ?? null
  );
}

function overlayFooterText(): string {
  return (mini.document.body.querySelector('.chat-tmux-terminal__foot')?.textContent ?? '').trim();
}

function overlayFooterClass(): string {
  return mini.document.body.querySelector('.chat-tmux-terminal__foot')?.className ?? '';
}

function overlayTitleText(): string {
  return (mini.document.body.querySelector('.chat-tmux-terminal__title')?.textContent ?? '').trim();
}

function isDisabled(node: MiniNode | null): boolean {
  return node !== null && (node.disabled === true || node.getAttribute('disabled') !== null);
}

async function click(node: MiniNode | null): Promise<void> {
  if (!node) {
    check('click target exists', false);
    return;
  }
  await act(async () => {
    node.click();
    await realTick();
  });
}

function decodedTerminalInputs(): string[] {
  return commandsOf('tmux_terminal_input').flatMap((command) => {
    if (command.type !== 'tmux_terminal_input' || typeof command.data !== 'string') return [];
    const bytes = tmuxTerminalModule.decodeTmuxTerminalData(command.data);
    return bytes === null ? [] : [new TextDecoder().decode(bytes)];
  });
}

const originalTermOpen = Terminal.prototype.open;
Terminal.prototype.open = function openWithStyle(element: unknown): void {
  const term = this as Terminal & { element: { style?: Record<string, string> } | null };
  if (term.element && term.element.style === undefined) {
    term.element.style = {};
  }
  originalTermOpen.call(this, element);
};

installGlobals();

const react = await import('react');
const reactDomClient = await import('react-dom/client');
protocol = await import('../src/chat/protocol.ts');
tmuxTerminalModule = await import('../src/chat/TmuxTerminal.tsx');
tmuxPanelModule = await import('../src/chat/TmuxPanel.tsx');
act = react.act as Act;
createElement = react.createElement as CreateElement;
createRoot = reactDomClient.createRoot as CreateRoot;
TmuxTerminal = tmuxTerminalModule.TmuxTerminal;
TmuxPanel = tmuxPanelModule.TmuxPanel;

// ============================================================ //
// ---- parsers + base64 round-trip ----
// ============================================================ //
{
  const good = protocol.parseTmuxTerminalAttach({
    terminalId: 'tmuxterm-1',
    session: 'agentlo-pi-rs',
    cols: 120,
    rows: 36,
    paneKey: '%0',
    historyKey: '1:agentlo-pi-rs:%0',
    historyBytes: 0,
    historyComplete: true,
  });
  check('attach parser accepts the frozen success shape',
    good !== null && good.terminalId === 'tmuxterm-1' && good.cols === 120 && good.rows === 36
    && good.paneKey === '%0' && good.historyKey === '1:agentlo-pi-rs:%0' && good.historyComplete === true);
  check('attach parser rejects zero geometry', protocol.parseTmuxTerminalAttach({
    terminalId: 'tmuxterm-1', session: 's', cols: 0, rows: 36,
    paneKey: '%0', historyKey: '1:s:%0', historyBytes: 0, historyComplete: false,
  }) === null);
  check('attach parser rejects a missing terminalId', protocol.parseTmuxTerminalAttach({
    session: 's', cols: 80, rows: 24,
    paneKey: '%0', historyKey: '1:s:%0', historyBytes: 0, historyComplete: false,
  }) === null);
  check('attach parser rejects a missing historyKey', protocol.parseTmuxTerminalAttach({
    terminalId: 'tmuxterm-1', session: 's', cols: 80, rows: 24,
    paneKey: '%0', historyBytes: 0, historyComplete: false,
  }) === null);

  const output = protocol.parseTmuxTerminalOutput({
    type: 'tmux_terminal_output',
    terminalId: 'tmuxterm-1',
    data: 'aGVsbG8=',
  });
  check('output parser accepts a matching frame',
    output !== null && output.terminalId === 'tmuxterm-1' && output.data === 'aGVsbG8=');
  check('output parser rejects a drifted type',
    protocol.parseTmuxTerminalOutput({ type: 'tmux_pane_snapshot', terminalId: 't', data: 'x' }) === null);

  const exit = protocol.parseTmuxTerminalExit({ type: 'tmux_terminal_exit', terminalId: 'tmuxterm-1', code: 0 });
  check('exit parser accepts an integer code', exit !== null && exit.code === 0);
  check('exit parser rejects a string code',
    protocol.parseTmuxTerminalExit({ type: 'tmux_terminal_exit', terminalId: 't', code: '0' }) === null);

  const history = protocol.parseTmuxTerminalHistory({
    type: 'tmux_terminal_history',
    terminalId: 'tmuxterm-1',
    paneKey: '%0',
    historyKey: '1:agentlo-pi-rs:%0',
    data: 'YQ==',
    final: true,
  });
  check('history parser accepts a final chunk',
    history !== null && history.paneKey === '%0' && history.final === true && history.data === 'YQ==');
  check('history parser rejects a drifted type',
    protocol.parseTmuxTerminalHistory({
      type: 'tmux_terminal_output', terminalId: 't', paneKey: '%0', historyKey: 'k', data: 'x', final: true,
    }) === null);

  const encoded = tmuxTerminalModule.encodeTmuxTerminalData('hello');
  const decoded = tmuxTerminalModule.decodeTmuxTerminalData(encoded);
  check('base64 encode/decode round-trips UTF-8',
    decoded !== null && new TextDecoder().decode(decoded) === 'hello', encoded);
  check('decode rejects invalid base64', tmuxTerminalModule.decodeTmuxTerminalData('@@@') === null);
}

{
  check('zero-width host is not ready', tmuxTerminalModule.tmuxTerminalHostReady({ clientWidth: 0, clientHeight: 400 }) === false);
  check('narrow host is not ready', tmuxTerminalModule.tmuxTerminalHostReady({ clientWidth: 1, clientHeight: 400 }) === false);
  check('laid-out host is ready', tmuxTerminalModule.tmuxTerminalHostReady({ clientWidth: 800, clientHeight: 400 }) === true);
  check('default xterm 80x24 is usable geometry', tmuxTerminalModule.tmuxTerminalGeometryReady(80, 24) === true);
  check('single-column geometry is not usable', tmuxTerminalModule.tmuxTerminalGeometryReady(1, 24) === false);
}


// ============================================================ //
// ---- attach uses mocked fit cols/rows ----
// ============================================================ //
{
  await mountTerminal('agentlo-pi-rs', { cols: 88, rows: 24 });
  const attach = commandsOf('tmux_terminal_attach')[0];
  check('mount sends exactly one attach', commandsOf('tmux_terminal_attach').length === 1);
  check(
    'attach frame carries the mocked fit cols/rows',
    JSON.stringify(attach) === JSON.stringify({ type: 'tmux_terminal_attach', session: 'agentlo-pi-rs', cols: 88, rows: 24 }),
    JSON.stringify(attach),
  );
  check('attach never requests fullHistory', attach !== undefined && !('fullHistory' in attach), JSON.stringify(attach));
  check('xterm opened into the overlay body', xtermHarness.opened === true);
  check('header names the runner and session',
    mini.document.body.querySelector('.chat-tmux-terminal__title')?.textContent === 'x3 · agentlo-pi-rs');
  check('in-flight attach shows Attaching in the footer', overlayFooterText() === 'Attaching…');
  check('in-flight attach footer pulses', overlayFooterClass().includes('chat-footer-pulse'), overlayFooterClass());
  await unmountTerminal();
}

{
  await mountTerminal('agentlo-pi-rs', { cols: 88, rows: 24 });
  const attach = commandsOf('tmux_terminal_attach')[0];
  check(
    'attach sends cols/rows so the server sizes before capture',
    JSON.stringify(attach) === JSON.stringify({ type: 'tmux_terminal_attach', session: 'agentlo-pi-rs', cols: 88, rows: 24 }),
    JSON.stringify(attach),
  );
  await settleAttach('tmuxterm-size-before-capture');
  const afterAttach = commandsOf('tmux_terminal_resize');
  check(
    'attach follow-up is a liveness resize without fitWindow',
    afterAttach.some((command) => (
      command.type === 'tmux_terminal_resize'
      && command.fitWindow !== true
      && command.cols === 88
      && command.rows === 24
    )) && afterAttach.every((command) => command.type !== 'tmux_terminal_resize' || command.fitWindow !== true),
    JSON.stringify(afterAttach),
  );
  await unmountTerminal();
}

{
  await mountTerminal('agentlo-pi-rs', { cols: 88, rows: 24 }, false);
  check('zero-width host does not attach', commandsOf('tmux_terminal_attach').length === 0, JSON.stringify(commandsOf('tmux_terminal_attach')));
  check('xterm still opened into the overlay body', xtermHarness.opened === true);
  await act(async () => {
    await layoutTerminalHost();
    triggerHostResize();
    await realTick();
  });
  const attach = commandsOf('tmux_terminal_attach')[0];
  check('host layout sends exactly one attach', commandsOf('tmux_terminal_attach').length === 1);
  check(
    'deferred attach uses fitted cols/rows',
    JSON.stringify(attach) === JSON.stringify({ type: 'tmux_terminal_attach', session: 'agentlo-pi-rs', cols: 88, rows: 24 }),
    JSON.stringify(attach),
  );
  await unmountTerminal();
}

{
  await mountTerminal('agentlo-pi-rs', { cols: 1, rows: 24 });
  check('one-column fit does not attach', commandsOf('tmux_terminal_attach').length === 0, JSON.stringify(commandsOf('tmux_terminal_attach')));
  check('host observer is armed before attach', FakeResizeObserver.instances.some((observer) => observer.observes > 0));
  await act(async () => {
    xtermHarness.cols = 88;
    xtermHarness.rows = 24;
    xtermHarness.baseCols = 88;
    xtermHarness.baseRows = 24;
    triggerHostResize();
    await realTick();
  });
  const attach = commandsOf('tmux_terminal_attach')[0];
  check('usable fit after one-column sends one attach', commandsOf('tmux_terminal_attach').length === 1);
  check(
    'attach after one-column uses fitted cols/rows',
    JSON.stringify(attach) === JSON.stringify({ type: 'tmux_terminal_attach', session: 'agentlo-pi-rs', cols: 88, rows: 24 }),
    JSON.stringify(attach),
  );
  await unmountTerminal();
  check('unmount disconnects the host observer', FakeResizeObserver.instances.every((observer) => observer.disconnects > 0));
}

{
  await mountTerminal('agentlo-pi-rs', { cols: 88, rows: 24 });
  await settleAttach('tmuxterm-keyboard');
  const overlay = mini.document.body.querySelector('.chat-tmux-terminal');
  const host = mini.document.body.querySelector('.chat-tmux-terminal__viewport');
  const keys = mini.document.body.querySelector('.chat-tmux-terminal__keys');
  check('keyboard test overlay is mounted', overlay !== null && host !== null && keys !== null);
  if (!overlay || !host || !keys) {
    await unmountTerminal();
  } else {
    overlay.clientHeight = 800;
    overlay.clientWidth = 390;
    host.clientWidth = 800;
    host.clientHeight = 400;
    keys.clientHeight = 53;
    const rowsBefore = xtermHarness.lastTerm?.rows ?? 0;
    check('keyboard test starts with fitted rows', rowsBefore === 24, String(rowsBefore));
    const visualViewport = mini.window.visualViewport as {
      height: number;
      width: number;
      offsetTop: number;
      offsetLeft: number;
      dispatchEvent(raw: { type: string }): boolean;
    };
    visualViewport.height = 420;
    visualViewport.width = 390;
    visualViewport.offsetTop = 0;
    visualViewport.offsetLeft = 0;
    await act(async () => {
      visualViewport.dispatchEvent({ type: 'resize' });
      await realTick();
    });
    const overlayBox = overlay.getBoundingClientRect();
    check(
      'keyboard-open overlay stays inside visualViewport',
      overlayBox.bottom <= visualViewport.height,
      `overlay.bottom=${overlayBox.bottom} vv.height=${visualViewport.height} height=${String(overlay.style.height)}`,
    );
    check(
      'keyboard-open keys bar stays inside visualViewport',
      overlayBox.bottom <= visualViewport.height && overlay.contains(keys),
      `overlay.bottom=${overlayBox.bottom} vv.height=${visualViewport.height}`,
    );
    const pinnedHeight = Number.parseFloat(String(overlay.style.height));
    if (Number.isFinite(pinnedHeight) && pinnedHeight > 0 && pinnedHeight < overlay.clientHeight) {
      host.clientHeight = Math.max(24, pinnedHeight - 114);
      const rows = Math.max(4, Math.floor(host.clientHeight / 20));
      xtermHarness.rows = rows;
      xtermHarness.baseRows = rows;
    }
    await act(async () => {
      flushClock(600);
      await realTick();
    });
    const rowsAfter = xtermHarness.lastTerm?.rows ?? 0;
    check(
      'keyboard-open settle refit reduces xterm rows',
      rowsAfter < rowsBefore,
      `before=${rowsBefore} after=${rowsAfter} height=${String(overlay.style.height)}`,
    );
    await unmountTerminal();
  }
}



// ============================================================ //
// ---- input forwarded after attach; resize on fit change ----
// ============================================================ //
{
  resetXtermHarness({ cols: 120, rows: 36 });
  await mountTerminal();
  await settleAttach('tmuxterm-live');
  check('no input is sent before typing', commandsOf('tmux_terminal_input').length === 0);
  await act(async () => {
    xtermHarness.dataHandler?.('ls\n');
    await realTick();
  });
  const input = commandsOf('tmux_terminal_input')[0] as
    | { type: 'tmux_terminal_input'; terminalId: string; data: string }
    | undefined;
  check('typed bytes are forwarded as tmux_terminal_input',
    input?.type === 'tmux_terminal_input'
      && input.terminalId === 'tmuxterm-live'
      && input.data === tmuxTerminalModule.encodeTmuxTerminalData('ls\n'),
    JSON.stringify(input));

  await act(async () => {
    xtermHarness.lastTerm?.resize(100, 30);
    await realTick();
  });
  const resizes = commandsOf('tmux_terminal_resize');
  check(
    'fit/resize change emits tmux_terminal_resize',
    resizes.some((command) => JSON.stringify(command) === JSON.stringify({ type: 'tmux_terminal_resize', terminalId: 'tmuxterm-live', cols: 100, rows: 30 })),
    JSON.stringify(resizes),
  );
  while (settleNext('tmux_terminal_resize', {})) { /* drain geometry probes so Fit is the only pending resize */ }
  await act(async () => { await realTick(); });
  const beforeFit = commandsOf('tmux_terminal_resize').length;
  const fitBtn = buttonByAriaLabel('Fit terminal to this device');
  check('Fit button is present next to Close', fitBtn !== null && buttonByAriaLabel('Close terminal') !== null);
  const header = mini.document.body.querySelector('.chat-tmux-terminal__head');
  const headerDetach = header?.querySelector('.chat-tmux-terminal__attach') ?? null;
  const footerDetach = mini.document.body.querySelector('.chat-tmux-terminal__foot .chat-tmux-terminal__attach');
  check('Detach is in the overlay header', headerDetach !== null && headerDetach.getAttribute('aria-label') === 'Detach terminal');
  check('Detach is not in the overlay footer', footerDetach === null);
  const headerButtons = header ? header.querySelectorAll('button') : [];
  const headerLabels = headerButtons.map((node) => node.getAttribute('aria-label'));
  check(
    'header order is Fit then Detach then Close',
    JSON.stringify(headerLabels) === JSON.stringify([
      'Fit terminal to this device',
      'Detach terminal',
      'Close terminal',
    ]),
    JSON.stringify(headerLabels),
  );
  await click(fitBtn!);
  check('Fit click sends tmux_terminal_resize', commandsOf('tmux_terminal_resize').length > beforeFit);
  const fitSent = commandsOf('tmux_terminal_resize').slice(beforeFit);
  check(
    'Fit click sends fitWindow',
    fitSent.some((command) => command.type === 'tmux_terminal_resize' && command.fitWindow === true),
    JSON.stringify(fitSent),
  );
  check('Fit in-flight title stays the runner and session', overlayTitleText() === 'x3 · agentlo-pi-rs', overlayTitleText());
  check('Fit in-flight footer shows Fitting', overlayFooterText() === 'Fitting…', overlayFooterText());
  check('Fit in-flight footer pulses', overlayFooterClass().includes('chat-footer-pulse'), overlayFooterClass());
  check('Fit in-flight disables the Fit button', isDisabled(fitBtn));
  const fitCountWhilePending = commandsOf('tmux_terminal_resize').length;
  await click(fitBtn!);
  check('disabled Fit does not send a second resize', commandsOf('tmux_terminal_resize').length === fitCountWhilePending, String(commandsOf('tmux_terminal_resize').length));
  check('in-flight Fit settles', settleNext('tmux_terminal_resize', {}));
  await act(async () => { await realTick(); });
  check('Fit title stays the runner and session after the RPC', overlayTitleText() === 'x3 · agentlo-pi-rs', overlayTitleText());
  check('Fit footer clears after the RPC', overlayFooterText() === '', overlayFooterText());
  check('Fit button re-enables after the RPC', !isDisabled(buttonByAriaLabel('Fit terminal to this device')));
  const afterFitAttaches = commandsOf('tmux_terminal_attach');
  const afterFitDetaches = commandsOf('tmux_terminal_detach');
  check('keyboard and Fit keep a single attach', afterFitAttaches.length === 1, JSON.stringify(afterFitAttaches));
  check('keyboard and Fit send no detach', afterFitDetaches.length === 0, JSON.stringify(afterFitDetaches));
  check(
    'the live attach still omits fullHistory',
    afterFitAttaches.every((command) => command.type === 'tmux_terminal_attach' && !('fullHistory' in command)),
    JSON.stringify(afterFitAttaches),
  );
  await unmountTerminal();
}

{
  await mountTerminal();
  await settleAttach('tmuxterm-fit-fail');
  while (settleNext('tmux_terminal_resize', {})) { /* drain geometry probes so Fit is the only pending resize */ }
  await act(async () => { await realTick(); });
  overlayErrors = [];
  await click(buttonByAriaLabel('Fit terminal to this device')!);
  check('Fit failure RPC settles', settleNextFailure('tmux_terminal_resize', 'tmux resize-window failed'));
  await act(async () => { await realTick(); });
  check('Fit failure keeps the title as runner and session', overlayTitleText() === 'x3 · agentlo-pi-rs', overlayTitleText());
  check('Fit failure is not in the overlay title', !overlayTitleText().includes('Fit failed'), overlayTitleText());
  check('Fit failure toasts on the session error layer', overlayErrors.join('\n') === tmuxTerminalModule.TMUX_TERMINAL_FIT_FAILED, JSON.stringify(overlayErrors));
  check('Fit failure is not in the overlay footer', overlayFooterText() === '', overlayFooterText());
  await unmountTerminal();
}

{
  await mountTerminal();
  overlayErrors = [];
  check('attach failure RPC settles', settleNextFailure('tmux_terminal_attach', 'tmux session not found'));
  await act(async () => { await realTick(); });
  check('attach failure keeps the title as runner and session', overlayTitleText() === 'x3 · agentlo-pi-rs', overlayTitleText());
  check('attach failure toasts on the session error layer', overlayErrors.join('\n') === tmuxTerminalModule.TMUX_TERMINAL_ATTACH_FAILED, JSON.stringify(overlayErrors));
  check('attach failure is not in the overlay footer', overlayFooterText() === '', overlayFooterText());
  await unmountTerminal();
}

{
  await mountTerminal();
  await settleAttach('tmuxterm-keys');
  const keyBytes: Array<[string, string]> = [
    ['Escape key', '\x1b'],
    ['Tab key', '\t'],
    ['Arrow up key', '\x1b[A'],
    ['Arrow down key', '\x1b[B'],
    ['Arrow right key', '\x1b[C'],
    ['Arrow left key', '\x1b[D'],
  ];
  for (const [label, bytes] of keyBytes) {
    const before = decodedTerminalInputs().length;
    const button = buttonByAriaLabel(label);
    check(`soft-key ${label} is present`, button !== null);
    await click(button!);
    const decoded = decodedTerminalInputs().slice(before);
    check(
      `soft-key ${label} dispatches the exact byte sequence`,
      decoded.length === 1 && decoded[0] === bytes,
      JSON.stringify(decoded),
    );
  }
  await click(buttonByAriaLabel('Sticky control key')!);
  check('sticky Ctrl is pressed', buttonByAriaLabel('Sticky control key')?.getAttribute('aria-pressed') === 'true');
  const beforeOther = decodedTerminalInputs().length;
  await act(async () => {
    xtermHarness.dataHandler?.('1');
    await realTick();
  });
  const otherDecoded = decodedTerminalInputs().slice(beforeOther);
  check(
    'sticky Ctrl + non-letter consumes the modifier and sends the original byte',
    otherDecoded.length === 1 && otherDecoded[0] === '1',
    JSON.stringify(otherDecoded),
  );
  check('sticky Ctrl clears after a non-letter', buttonByAriaLabel('Sticky control key')?.getAttribute('aria-pressed') === 'false');
  await click(buttonByAriaLabel('Sticky control key')!);
  check('sticky Ctrl is pressed', buttonByAriaLabel('Sticky control key')?.getAttribute('aria-pressed') === 'true');
  const beforeCtrl = decodedTerminalInputs().length;
  await act(async () => {
    xtermHarness.dataHandler?.('c');
    await realTick();
  });
  const ctrlDecoded = decodedTerminalInputs().slice(beforeCtrl);
  check(
    'sticky Ctrl + c sends 0x03',
    ctrlDecoded.length === 1 && ctrlDecoded[0] === '\x03',
    JSON.stringify(ctrlDecoded),
  );
  check('sticky Ctrl clears after one letter', buttonByAriaLabel('Sticky control key')?.getAttribute('aria-pressed') === 'false');
  const beforePlain = decodedTerminalInputs().length;
  await act(async () => {
    xtermHarness.dataHandler?.('c');
    await realTick();
  });
  const plainDecoded = decodedTerminalInputs().slice(beforePlain);
  check(
    'typing c again after sticky Ctrl sends plain c',
    plainDecoded.length === 1 && plainDecoded[0] === 'c',
    JSON.stringify(plainDecoded),
  );
  await unmountTerminal();
}

{
  await mountTerminal();
  await settleAttach('tmuxterm-da');
  const beforeDa = commandsOf('tmux_terminal_input').length;
  await act(async () => {
    xtermHarness.lastTerm?.write('\x1b[>c');
    xtermHarness.lastTerm?.write('\x1b[c');
    await realTick();
  });
  check(
    'DA queries in output do not send a DA reply as tmux_terminal_input',
    commandsOf('tmux_terminal_input').length === beforeDa,
    JSON.stringify(decodedTerminalInputs().slice(beforeDa)),
  );
  const beforeF3 = decodedTerminalInputs().length;
  await act(async () => {
    xtermHarness.dataHandler?.('\x1b[13~');
    await realTick();
  });
  const f3Decoded = decodedTerminalInputs().slice(beforeF3);
  check(
    'F3 keystrokes still go through onData as CSI ~',
    f3Decoded.length === 1 && f3Decoded[0] === '\x1b[13~',
    JSON.stringify(f3Decoded),
  );
  const beforePaste = decodedTerminalInputs().length;
  await act(async () => {
    xtermHarness.dataHandler?.('\x1b[200~hi\x1b[201~');
    await realTick();
  });
  const pasteDecoded = decodedTerminalInputs().slice(beforePaste);
  check(
    'bracketed paste still goes through onData',
    pasteDecoded.length === 1 && pasteDecoded[0] === '\x1b[200~hi\x1b[201~',
    JSON.stringify(pasteDecoded),
  );
  await unmountTerminal();
}

{
  await mountTerminal();
  await settleAttach('tmuxterm-zoom');
  const fontBefore = Number(xtermHarness.lastTerm?.options.fontSize);
  const colsBefore = xtermHarness.lastTerm?.cols ?? 0;
  const rowsBefore = xtermHarness.lastTerm?.rows ?? 0;
  check('phone overlay default fontSize is 8', fontBefore === 8, String(fontBefore));
  check('fitted geometry is the harness base', colsBefore === 120 && rowsBefore === 36, `${colsBefore}x${rowsBefore}`);
  const resizesBefore = commandsOf('tmux_terminal_resize').length;
  const viewport = mini.document.body.querySelector('.chat-tmux-terminal__viewport');
  check('overlay viewport is present', viewport !== null);
  await act(async () => {
    viewport!.dispatchEvent({
      type: 'touchstart',
      touches: [
        { clientX: 0, clientY: 0 },
        { clientX: 100, clientY: 0 },
      ],
    });
    viewport!.dispatchEvent({
      type: 'touchmove',
      touches: [
        { clientX: 0, clientY: 0 },
        { clientX: 200, clientY: 0 },
      ],
    });
    await realTick();
  });
  check(
    'two-finger move does not change fontSize',
    Number(xtermHarness.lastTerm?.options.fontSize) === fontBefore,
    String(xtermHarness.lastTerm?.options.fontSize),
  );
  check(
    'two-finger move does not change fitted cols/rows',
    (xtermHarness.lastTerm?.cols ?? 0) === colsBefore && (xtermHarness.lastTerm?.rows ?? 0) === rowsBefore,
    `${xtermHarness.lastTerm?.cols}x${xtermHarness.lastTerm?.rows}`,
  );
  check(
    'two-finger move does not send tmux_terminal_resize',
    commandsOf('tmux_terminal_resize').length === resizesBefore,
    String(commandsOf('tmux_terminal_resize').length),
  );
  const beforeFit = commandsOf('tmux_terminal_resize').length;
  await click(buttonByAriaLabel('Fit terminal to this device')!);
  check('Fit button sends tmux_terminal_resize', commandsOf('tmux_terminal_resize').length > beforeFit);
  check(
    'Fit keeps the overlay default fontSize',
    Number(xtermHarness.lastTerm?.options.fontSize) === fontBefore,
    String(xtermHarness.lastTerm?.options.fontSize),
  );
  const fitSent = commandsOf('tmux_terminal_resize').slice(beforeFit);
  const fitPayload = fitSent[fitSent.length - 1] as
    | { type: string; cols?: number; rows?: number; fitWindow?: boolean }
    | undefined;
  check(
    'Fit resize payload matches fitted cols/rows with fitWindow',
    fitPayload?.type === 'tmux_terminal_resize'
      && fitPayload.cols === colsBefore
      && fitPayload.rows === rowsBefore
      && fitPayload.fitWindow === true,
    JSON.stringify(fitPayload),
  );
  await unmountTerminal();
}

{
  resetXtermHarness({ cols: 120, rows: 36 });
  await mountTerminal();
  await settleAttach('tmuxterm-live');
  const beforeClosed = closed;
  const detachBtn = buttonByAriaLabel('Detach terminal');
  check('Detach control is present after attach', detachBtn !== null);
  await click(detachBtn!);
  check(
    'Detach sends tmux_terminal_detach for the overlay terminal',
    JSON.stringify(commandsOf('tmux_terminal_detach')[0]) === JSON.stringify({ type: 'tmux_terminal_detach', terminalId: 'tmuxterm-live' }),
    JSON.stringify(commandsOf('tmux_terminal_detach')),
  );
  check('Detach does not close the overlay', closed === beforeClosed);
  check('Detach in-flight footer shows Detaching', overlayFooterText() === 'Detaching…', overlayFooterText());
  check('Detach in-flight footer pulses', overlayFooterClass().includes('chat-footer-pulse'), overlayFooterClass());
  check('Detach in-flight disables the control', isDisabled(buttonByAriaLabel('Detach terminal')));
  check('in-flight Detach still says Detach', buttonByAriaLabel('Detach terminal') !== null);
  check('in-flight Detach settles', settleNext('tmux_terminal_detach', {}));
  await act(async () => { await realTick(); });
  check('Detach footer clears after the RPC', overlayFooterText() === '');
  const attachBtn = buttonByAriaLabel('Attach terminal');
  check('Attach is offered after Detach settles', attachBtn !== null && !isDisabled(attachBtn));
  const attachesBefore = commandsOf('tmux_terminal_attach').length;
  await click(attachBtn!);
  check('Attach click sends a second attach', commandsOf('tmux_terminal_attach').length === attachesBefore + 1, String(commandsOf('tmux_terminal_attach').length));
  check('Attach in-flight footer shows Attaching', overlayFooterText() === 'Attaching…', overlayFooterText());
  check('Attach in-flight title stays the runner and session', overlayTitleText() === 'x3 · agentlo-pi-rs', overlayTitleText());
  check('Attach in-flight disables the control', isDisabled(buttonByAriaLabel('Attach terminal')));
  await unmountTerminal();
}
{
  resetXtermHarness({ cols: 120, rows: 36 });
  await mountTerminal();
  await settleAttach('tmuxterm-live');
  const before = commandsOf('tmux_terminal_attach').length;
  await act(async () => {
    xtermHarness.dataHandler?.('a');
    if (xtermHarness.lastTerm) {
      xtermHarness.lastTerm.buffer.active.viewportY = 0;
      xtermHarness.lastTerm.buffer.active.baseY = 80;
    }
    const host = mini.document.body.querySelector('.chat-tmux-terminal__body');
    host?.dispatchEvent({ type: 'wheel', deltaY: -1 });
    xtermHarness.scrollHandler?.();
    await realTick();
  });
  const after = commandsOf('tmux_terminal_attach');
  check('keyboard and top-of-buffer scroll keep a single attach', after.length === before, JSON.stringify(after));
  check('top-of-buffer scroll sends no detach', commandsOf('tmux_terminal_detach').length === 0);
  check(
    'no attach after scroll carries fullHistory',
    after.every((command) => command.type === 'tmux_terminal_attach' && !('fullHistory' in command)),
    JSON.stringify(after),
  );
  await unmountTerminal();
}

{
  resetXtermHarness({ cols: 120, rows: 36 });
  await mountTerminal();
  await settleAttach('tmuxterm-live');
  while (settleNext('tmux_terminal_resize', {})) { /* drain attach fit */ }
  await act(async () => { await realTick(); });
  const beforeWake = commandsOf('tmux_terminal_resize').length;
  await act(async () => {
    mini.document.visibilityState = 'hidden';
    mini.document.dispatchEvent({ type: 'visibilitychange' });
    await realTick();
  });
  check('hidden visibilitychange does not probe', commandsOf('tmux_terminal_resize').length === beforeWake);
  await act(async () => {
    mini.document.visibilityState = 'visible';
    mini.document.dispatchEvent({ type: 'visibilitychange' });
    await realTick();
  });
  check('visible visibilitychange probes without waiting for the 5s timer', commandsOf('tmux_terminal_resize').length > beforeWake);
  while (settleNext('tmux_terminal_resize', {})) { /* drain wake probe */ }
  await act(async () => { await realTick(); });
  const afterVisible = commandsOf('tmux_terminal_resize').length;
  await act(async () => {
    (mini.window as { dispatchEvent: (event: { type: string }) => boolean }).dispatchEvent({ type: 'focus' });
    await realTick();
  });
  check('window focus probes immediately', commandsOf('tmux_terminal_resize').length > afterVisible);
  while (settleNext('tmux_terminal_resize', {})) { /* drain focus probe */ }
  await act(async () => { await realTick(); });
  const afterFocus = commandsOf('tmux_terminal_resize').length;
  await act(async () => {
    (mini.window as { dispatchEvent: (event: { type: string }) => boolean }).dispatchEvent({ type: 'pageshow' });
    await realTick();
  });
  check('pageshow probes immediately', commandsOf('tmux_terminal_resize').length > afterFocus);
  await unmountTerminal();
}

// ============================================================ //
// ---- matching output writes; unknown terminalId ignored ----
// ============================================================ //
{
  await mountTerminal();
  await settleAttach('tmuxterm-live');
  const payload = tmuxTerminalModule.encodeTmuxTerminalData('ready');
  await act(async () => {
    dispatchEvent({ type: 'tmux_terminal_output', terminalId: 'other-term', data: payload });
    await realTick();
  });
  check('unknown terminalId output is ignored', xtermHarness.writes.length === 0, String(xtermHarness.writes.length));
  await act(async () => {
    dispatchEvent({ type: 'tmux_terminal_output', terminalId: 'tmuxterm-live', data: payload });
    await realTick();
  });
  check('matching output is written to xterm', xtermHarness.writes.length === 1);
  const written = xtermHarness.writes[0];
  check('written bytes decode to the original text',
    written instanceof Uint8Array && new TextDecoder().decode(written) === 'ready');
  await unmountTerminal();
}
{
  await mountTerminal();
  const attach = commandsOf('tmux_terminal_attach')[0] as
    | { type: 'tmux_terminal_attach'; session: string; cols: number; rows: number }
    | undefined;
  check('history-gate attach is in flight', attach !== undefined);
  if (!attach) {
    await unmountTerminal();
  } else {
    check(
      'history-gate attach settles',
      settleNext('tmux_terminal_attach', {
        terminalId: 'tmuxterm-live',
        session: attach.session,
        cols: attach.cols,
        rows: attach.rows,
        paneKey: '%0',
        historyKey: `1:${attach.session}:%0`,
        historyBytes: 4,
        historyComplete: true,
      }),
    );
    await act(async () => { await realTick(); });
    const live = tmuxTerminalModule.encodeTmuxTerminalData('live');
    await act(async () => {
      dispatchEvent({
        type: 'tmux_terminal_history',
        terminalId: 'tmuxterm-live',
        paneKey: '%0',
        historyKey: `1:${attach.session}:%0`,
        data: tmuxTerminalModule.encodeTmuxTerminalData('old\n'),
        final: true,
      });
      dispatchEvent({ type: 'tmux_terminal_output', terminalId: 'tmuxterm-live', data: live });
      await realTick();
    });
    check('final history and the following live frame both reach xterm exactly once', xtermHarness.writes.length === 2, String(xtermHarness.writes.length));
    const decoded = xtermHarness.writes.map((bytes) => new TextDecoder().decode(bytes as Uint8Array));
    check(
      'history replay is written before the live frame',
      decoded[0] === 'old\n' && decoded[1] === 'live',
      JSON.stringify(decoded),
    );
    check('history final reveals the overlay',
      !(mini.document.body.querySelector('.chat-tmux-terminal__body')?.className.split(/\s+/).includes('chat-tmux-terminal__body--pending') ?? true)
      && (mini.document.body.querySelector('.chat-tmux-terminal__body')?.getAttribute('aria-busy') === 'false'));
    await unmountTerminal();
  }
}
{
  await mountTerminal();
  const attach = commandsOf('tmux_terminal_attach')[0] as
    | { type: 'tmux_terminal_attach'; session: string; cols: number; rows: number }
    | undefined;
  check('pending-paint attach is in flight', attach !== undefined);
  if (!attach) {
    await unmountTerminal();
  } else {
    const pendingBody = mini.document.body.querySelector('.chat-tmux-terminal__body');
    check('xterm stays pending before history.final',
      (pendingBody?.className.split(/\s+/).includes('chat-tmux-terminal__body--pending') ?? false)
      && pendingBody?.getAttribute('aria-busy') === 'true');
    check(
      'pending-paint attach settles',
      settleNext('tmux_terminal_attach', {
        terminalId: 'tmuxterm-live',
        session: attach.session,
        cols: attach.cols,
        rows: attach.rows,
        paneKey: '%0',
        historyKey: `1:${attach.session}:%0`,
        historyBytes: 4,
        historyComplete: true,
      }),
    );
    await act(async () => { await realTick(); });
    const live = tmuxTerminalModule.encodeTmuxTerminalData('live');
    await act(async () => {
      dispatchEvent({ type: 'tmux_terminal_output', terminalId: 'tmuxterm-live', data: live });
      await realTick();
    });
    check('live output before history.final is not written', xtermHarness.writes.length === 0, String(xtermHarness.writes.length));
    check('overlay stays pending while live is held',
      (mini.document.body.querySelector('.chat-tmux-terminal__body')?.className.split(/\s+/).includes('chat-tmux-terminal__body--pending') ?? false));
    await act(async () => {
      dispatchEvent({
        type: 'tmux_terminal_history',
        terminalId: 'tmuxterm-live',
        paneKey: '%0',
        historyKey: `1:${attach.session}:%0`,
        data: tmuxTerminalModule.encodeTmuxTerminalData('old\n'),
        final: true,
      });
      await realTick();
    });
    check('held live is written after history replay', xtermHarness.writes.length === 2, String(xtermHarness.writes.length));
    const decoded = xtermHarness.writes.map((bytes) => new TextDecoder().decode(bytes as Uint8Array));
    check(
      'history still precedes held live',
      decoded[0] === 'old\n' && decoded[1] === 'live',
      JSON.stringify(decoded),
    );
    check('history.final scrolls to the live prompt once', xtermHarness.scrollToBottomCalls >= 1, String(xtermHarness.scrollToBottomCalls));
    check('overlay reveals after history.final',
      !(mini.document.body.querySelector('.chat-tmux-terminal__body')?.className.split(/\s+/).includes('chat-tmux-terminal__body--pending') ?? true)
      && (mini.document.body.querySelector('.chat-tmux-terminal__body')?.getAttribute('aria-busy') === 'false'));
    await unmountTerminal();
  }
}

// ============================================================ //
// ---- exit shows Detached; Esc does not close; × detaches ----
// ============================================================ //
{
  await mountTerminal();
  await settleAttach('tmuxterm-live');
  const overlay = mini.document.body.querySelector('.chat-tmux-terminal');
  check('overlay is a labeled dialog',
    overlay?.getAttribute('role') === 'dialog' && overlay.getAttribute('aria-label') === 'Tmux terminal');
  await act(async () => {
    overlay?.dispatchEvent({ type: 'keydown', key: 'Escape', target: overlay });
    await realTick();
  });
  check('Esc does not close the overlay', mini.document.body.querySelector('.chat-tmux-terminal') !== null);
  check('Esc does not send detach', commandsOf('tmux_terminal_detach').length === 0);
  check('Esc does not call onClose', closed === 0);

  await act(async () => {
    dispatchEvent({ type: 'tmux_terminal_exit', terminalId: 'tmuxterm-live', code: 0 });
    await realTick();
  });
  check('exit event swaps the header to Attach',
    (mini.document.body.querySelector('.chat-tmux-terminal__attach')?.textContent ?? '').includes('Attach'));

  const closeBtn = buttonByAriaLabel('Close terminal');
  check('× close button is present', closeBtn !== null);
  await click(closeBtn!);
  check('× calls onClose', closed === 1);
  // Exit already cleared the terminal id, so close must not send a late detach.
  check('close after exit sends no detach', commandsOf('tmux_terminal_detach').length === 0);
  await unmountTerminal();
}

// ============================================================ //
// ---- overlay Upload posts /upload/tmux-file and toasts ----
// ============================================================ //
{
  await mountTerminal();
  await settleAttach('tmuxterm-upload');
  const keys = mini.document.body.querySelector('.chat-tmux-terminal__keys');
  const keyButtons = keys ? keys.querySelectorAll('button') : [];
  const keyLabels = keyButtons.map((node) => node.getAttribute('aria-label'));
  check(
    'Upload sits next to Ctrl in the soft-key bar',
    keyLabels.at(-2) === 'Sticky control key' && keyLabels.at(-1) === 'Upload file to terminal',
    JSON.stringify(keyLabels),
  );
  const uploadBtn = buttonByAriaLabel('Upload file to terminal');
  const uploadInput = mini.document.body.querySelector('#chat-tmux-overlay-upload') as (MiniNode & { files?: unknown; click: () => void }) | null;
  check('Upload file input is present', uploadInput !== null && uploadInput.getAttribute('type') === 'file' && uploadInput.hasAttribute('hidden'));
  let pickerOpens = 0;
  if (uploadInput) {
    const nativeClick = uploadInput.click.bind(uploadInput);
    uploadInput.click = () => {
      pickerOpens += 1;
      nativeClick();
    };
  }
  await click(uploadBtn);
  check('Upload click opens the hidden file picker', pickerOpens === 1, String(pickerOpens));
  check(
    'Upload label uses the overlay btn-text class',
    (uploadBtn?.querySelector('.chat-tmux-terminal__btn-text')?.textContent ?? '').trim() === 'Upload',
  );

  const overlayFile = new File([new Uint8Array([104, 105])], 'note.txt', { type: 'text/plain' });
  let resolveUpload: ((response: { status: number; json: () => Promise<unknown> }) => void) | null = null;
  const fetchCalls: Array<{ url: string; init: RequestInit }> = [];
  const previousFetch = globalThis.fetch;
  globalThis.fetch = (async (url: string | URL | Request, init?: RequestInit) => {
    fetchCalls.push({ url: String(url), init: init ?? {} });
    return await new Promise((resolve) => {
      resolveUpload = resolve;
    });
  }) as typeof fetch;
  try {
    if (uploadInput) uploadInput.files = [overlayFile];
    await act(async () => {
      uploadInput?.dispatchEvent({ type: 'change', target: uploadInput });
      await realTick();
    });
    check('Upload pulses while posting', (uploadBtn?.className ?? '').includes('chat-tmux-terminal__key--busy'), uploadBtn?.className ?? '');
    check('Upload is disabled while posting', isDisabled(uploadBtn));
    check('Upload posts before any toast', overlayErrors.length === 0);
    resolveUpload?.({
      status: 200,
      json: async () => ({ path: '/tmp/rpi-tmux-file-note.txt' }),
    });
    await act(async () => {
      await realTick();
      await realTick();
    });
    const headers = (fetchCalls[0]?.init.headers ?? {}) as Record<string, string>;
    check(
      'Upload posts to the overlay runner authority',
      fetchCalls.length === 1 && fetchCalls[0]?.url === 'http://192.0.2.10:8765/upload/tmux-file',
      JSON.stringify(fetchCalls.map((entry) => entry.url)),
    );
    check(
      'Upload names the overlay session and file',
      headers['X-Tmux-Session'] === encodeURIComponent('agentlo-pi-rs')
        && headers['X-File-Name'] === encodeURIComponent('note.txt')
        && headers.Authorization === 'Bearer tok-x3',
    );
    check('Upload loads the overlay authority token', overlayTokens.includes('192.0.2.10:8765'), JSON.stringify(overlayTokens));
    check(
      'Upload success stays silent',
      overlayErrors.length === 0,
      overlayErrors.join('\n'),
    );
    check('Upload idle after success', !(uploadBtn?.className ?? '').includes('chat-tmux-terminal__key--busy') && !isDisabled(uploadBtn));
  } finally {
    globalThis.fetch = previousFetch;
  }

  overlayNotices = [];
  overlayErrors = [];
  globalThis.fetch = (async () => ({
    status: 409,
    json: async () => ({ error: 'tmux session is not attached' }),
  })) as typeof fetch;
  try {
    if (uploadInput) uploadInput.files = [overlayFile];
    await act(async () => {
      uploadInput?.dispatchEvent({ type: 'change', target: uploadInput });
      await realTick();
      await realTick();
    });
    check(
      'Upload failure toasts on the error layer',
      overlayErrors.join('\n') === 'tmux session is not attached' && overlayNotices.length === 0,
      JSON.stringify({ overlayErrors, overlayNotices }),
    );
  } finally {
    globalThis.fetch = previousFetch;
  }

  overlayNotices = [];
  overlayErrors = [];
  const unreadOverlayFile = new File([new Uint8Array([104, 105])], 'note.txt', { type: 'text/plain' });
  Object.defineProperty(unreadOverlayFile, 'arrayBuffer', {
    value: async () => {
      throw new Error('read failed');
    },
  });
  let unreadFetchCalls = 0;
  globalThis.fetch = (async () => {
    unreadFetchCalls += 1;
    throw new Error('must not fetch');
  }) as typeof fetch;
  try {
    if (uploadInput) uploadInput.files = [unreadOverlayFile];
    await act(async () => {
      uploadInput?.dispatchEvent({ type: 'change', target: uploadInput });
      await realTick();
      await realTick();
    });
    check(
      'Upload file-read failure toasts on the error layer',
      overlayErrors.join('\n') === 'could not read the selected file' && overlayNotices.length === 0,
      JSON.stringify({ overlayErrors, overlayNotices }),
    );
    check('Upload file-read failure does not post', unreadFetchCalls === 0, String(unreadFetchCalls));
    check(
      'Upload idle after file-read failure',
      !(uploadBtn?.className ?? '').includes('chat-tmux-terminal__key--busy') && !isDisabled(uploadBtn),
    );
  } finally {
    globalThis.fetch = previousFetch;
  }
  await unmountTerminal();
}

{
  await mountTerminal('agentlo-pi-rs', { cols: 120, rows: 36 }, true, () => {
    throw new Error('token');
  });
  await settleAttach('tmuxterm-upload-token');
  const uploadBtn = buttonByAriaLabel('Upload file to terminal');
  const uploadInput = mini.document.body.querySelector('#chat-tmux-overlay-upload') as (MiniNode & { files?: unknown }) | null;
  const tokenFile = new File([new Uint8Array([104, 105])], 'note.txt', { type: 'text/plain' });
  let tokenFetchCalls = 0;
  const previousFetch = globalThis.fetch;
  globalThis.fetch = (async () => {
    tokenFetchCalls += 1;
    throw new Error('must not fetch');
  }) as typeof fetch;
  try {
    overlayErrors = [];
    overlayNotices = [];
    if (uploadInput) uploadInput.files = [tokenFile];
    await act(async () => {
      uploadInput?.dispatchEvent({ type: 'change', target: uploadInput });
      await realTick();
      await realTick();
    });
    check(
      'Upload token-load failure toasts a distinct error',
      overlayErrors.join('\n') === 'could not load the runner token' && overlayNotices.length === 0,
      JSON.stringify({ overlayErrors, overlayNotices }),
    );
    check('Upload token-load failure does not post', tokenFetchCalls === 0, String(tokenFetchCalls));
    check(
      'Upload idle after token-load failure',
      !(uploadBtn?.className ?? '').includes('chat-tmux-terminal__key--busy') && !isDisabled(uploadBtn),
    );
  } finally {
    globalThis.fetch = previousFetch;
  }
  await unmountTerminal();
}

{
  await mountTerminal();
  await settleAttach('tmuxterm-drop');
  const overlay = mini.document.body.querySelector('.chat-tmux-terminal');
  const dropFile = new File([new Uint8Array([137, 80])], 'shot.png', { type: 'image/png' });
  const fetchCalls: Array<{ url: string; init: RequestInit }> = [];
  const previousFetch = globalThis.fetch;
  globalThis.fetch = (async (url: string | URL | Request, init?: RequestInit) => {
    fetchCalls.push({ url: String(url), init: init ?? {} });
    return { status: 200, json: async () => ({ path: '/tmp/rpi-tmux-file-shot.png' }) };
  }) as typeof fetch;
  try {
    overlayErrors = [];
    await act(async () => {
      overlay?.dispatchEvent({
        type: 'drop',
        dataTransfer: { files: [dropFile], types: ['Files'] },
        preventDefault() {},
      });
      await realTick();
      await realTick();
    });
    const headers = (fetchCalls[0]?.init.headers ?? {}) as Record<string, string>;
    check(
      'Drop posts the image to the overlay session',
      fetchCalls.length === 1
        && fetchCalls[0]?.url === 'http://192.0.2.10:8765/upload/tmux-file'
        && headers['X-File-Name'] === encodeURIComponent('shot.png'),
      JSON.stringify({ urls: fetchCalls.map((entry) => entry.url), headers }),
    );
    check('Drop success stays silent', overlayErrors.length === 0, overlayErrors.join('\n'));
  } finally {
    globalThis.fetch = previousFetch;
  }
  await unmountTerminal();
}

{
  await mountTerminal();
  await settleAttach('tmuxterm-paste');
  const overlay = mini.document.body.querySelector('.chat-tmux-terminal');
  const pasteFile = new File([new Uint8Array([137, 80])], 'clip.png', { type: 'image/png' });
  const fetchCalls: Array<{ url: string }> = [];
  const previousFetch = globalThis.fetch;
  globalThis.fetch = (async (url: string | URL | Request) => {
    fetchCalls.push({ url: String(url) });
    return { status: 200, json: async () => ({ path: '/tmp/rpi-tmux-file-clip.png' }) };
  }) as typeof fetch;
  try {
    overlayErrors = [];
    await act(async () => {
      overlay?.dispatchEvent({
        type: 'paste',
        clipboardData: { files: [pasteFile] },
        preventDefault() {},
      });
      await realTick();
      await realTick();
    });
    check(
      'Paste posts the clipboard image to the overlay session',
      fetchCalls.length === 1 && fetchCalls[0]?.url === 'http://192.0.2.10:8765/upload/tmux-file',
      JSON.stringify(fetchCalls),
    );
    check('Paste success stays silent', overlayErrors.length === 0, overlayErrors.join('\n'));
  } finally {
    globalThis.fetch = previousFetch;
  }
  await unmountTerminal();
}

// ============================================================ //
// ---- close during in-flight attach detaches a late success ----
// ============================================================ //
{
  await mountTerminal();
  check('attach is still in flight', commandsOf('tmux_terminal_attach').length === 1 && deferreds.some((entry) => !entry.settled));
  await unmountTerminal();
  check('unmount during attach sends no detach yet', commandsOf('tmux_terminal_detach').length === 0, JSON.stringify(commandsOf('tmux_terminal_detach')));
  check('unmount disposes xterm', xtermHarness.disposed === true);
  check('unmount during attach does not call onClose', closed === 0);
  const pendingAttach = commandsOf('tmux_terminal_attach')[0] as
    | { type: 'tmux_terminal_attach'; session: string; cols: number; rows: number }
    | undefined;
  check('late attach request is still pending', pendingAttach !== undefined);
  check(
    'late attach settles after close',
    pendingAttach !== undefined && settleNext('tmux_terminal_attach', {
      terminalId: 'tmuxterm-late',
      session: pendingAttach.session,
      cols: pendingAttach.cols,
      rows: pendingAttach.rows,
      paneKey: '%0',
      historyKey: `1:${pendingAttach.session}:%0`,
      historyBytes: 0,
      historyComplete: false,
    }),
  );
  await act(async () => {
    await realTick();
  });
  const lateDetach = commandsOf('tmux_terminal_detach')[0];
  check(
    'late attach after close sends detach',
    JSON.stringify(lateDetach) === JSON.stringify({ type: 'tmux_terminal_detach', terminalId: 'tmuxterm-late' }),
    JSON.stringify(lateDetach),
  );
  check('late attach does not revive the overlay', closed === 0 && mini.document.body.querySelector('.chat-tmux-terminal') === null);
}

// ============================================================ //
// ---- TmuxPanel Open terminal action ----
// ============================================================ //
{
  sent = [];
  deferreds = [];
  let opened: string | null = null;
  container = mini.document.createElement('div');
  mini.document.body.appendChild(container);
  root = createRoot(container as unknown as Element);
  await act(async () => {
    root!.render(
      createElement(TmuxPanel, {
        runnerId: 'r1',
        runnerLabel: 'Runner r1',
        sessionId: 'sess-1',
        send: harnessSend,
        onOpenTerminal: (session: string) => {
          opened = session;
        },
      }),
    );
    await realTick();
  });
  check('panel list request is in flight', settleNext('tmux_list_sessions', {
    tmuxAvailable: true,
    sessions: [
      { name: 'dev', windows: 1, attached: false, createdEpoch: 1, paneCount: 1 },
    ],
  }));
  await act(async () => {
    await realTick();
  });
  const openBtn = buttonByAriaLabel('Open terminal');
  check('each session row exposes Open terminal', openBtn !== null && openBtn.textContent?.includes('Open terminal') === true);
  await click(openBtn!);
  check('Open terminal lifts the session name', opened === 'dev', String(opened));
  await unmountTerminal();
}

// ============================================================ //
// ---- RunnerRegistry routes machine-scoped terminal frames ----
// ============================================================ //
{
  const RUNNER_ORIGIN = 'https://runner-a.example';
  const AUTH_MARKER = 'auth-marker-a';
  const SAFE_CWD = '<workspace>/runner-a';
  const incarnationOf = (sessionId: string) => `inc-${sessionId}`;
  const values = new Map<string, string>();
  const store: StorageLike & { values: Map<string, string> } = {
    values,
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => { values.set(key, value); },
    removeItem: (key) => { values.delete(key); },
  };
  class RegistrySocket {
    static readonly CONNECTING = 0;
    static readonly OPEN = 1;
    static readonly CLOSING = 2;
    static readonly CLOSED = 3;
    readyState = RegistrySocket.CONNECTING;
    protocol = '';
    onopen: ((event: Event) => void) | null = null;
    onmessage: ((event: MessageEvent) => void) | null = null;
    onerror: ((event: Event) => void) | null = null;
    onclose: ((event: CloseEvent) => void) | null = null;
    sent: string[] = [];
    constructor(readonly url: string, readonly protocols: string[]) {}
    open(): void {
      this.readyState = RegistrySocket.OPEN;
      this.protocol = this.protocols[0] ?? '';
      this.onopen?.({ target: this } as unknown as Event);
    }
    send(frame: string): void {
      this.sent.push(frame);
    }
    respond(command: string, success: boolean, data?: unknown): void {
      const sent = this.sent.map((frame) => JSON.parse(frame) as { id?: string; type?: string; sessionId?: string })
        .find((frame) => frame.type === command);
      if (!sent?.id) throw new Error(`missing ${command}`);
      const bootstrapSessionId = command === 'get_state'
        && data && typeof data === 'object'
        && typeof (data as { sessionId?: unknown }).sessionId === 'string'
        ? (data as { sessionId: string }).sessionId
        : '';
      this.onmessage?.({
        target: this,
        data: JSON.stringify({
          type: 'response',
          id: sent.id,
          command,
          ...((sent.sessionId || bootstrapSessionId) ? { sessionId: sent.sessionId || bootstrapSessionId } : {}),
          success,
          data,
        }),
      } as MessageEvent);
    }
    emit(frame: unknown): void {
      this.onmessage?.({ target: this, data: JSON.stringify(frame) } as MessageEvent);
    }
  }
  const previousSocket = globalThis.WebSocket;
  Object.defineProperty(globalThis, 'WebSocket', { configurable: true, value: RegistrySocket });
  const sockets: RegistrySocket[] = [];
  const registry = new RunnerRegistry({
    fetchFn: (async () => ({
      ok: true,
      json: async () => ({ tokenRequired: false, version: '0.2.11', codename: null }),
    } as Response)) as typeof fetch,
    storage: store,
    webSocketFactory: (url, protocols) => {
      const socket = new RegistrySocket(url, protocols);
      sockets.push(socket);
      return socket as unknown as WebSocket;
    },
    now: () => 100,
    randomBytes: (length: number) => Uint8Array.from({ length }, (_, index) => index + 1),
  });
  const added = registry.add('Runner A', RUNNER_ORIGIN, AUTH_MARKER);
  await realTick();
  await realTick();
  sockets[0].open();
  sockets[0].respond('get_state', true, { sessionId: 'session-a', runtimeIncarnation: incarnationOf('session-a'), cwd: SAFE_CWD });
  await realTick();
  await realTick();
  sockets[0].respond('session_list', true, {
    sessions: [{ sessionId: 'session-a', loaded: true, runtimeIncarnation: incarnationOf('session-a') }],
  });
  await realTick();
  await realTick();
  const connected = await added;
  const received: unknown[] = [];
  const unsubscribe = subscribeRunnerEvents(connected.id, (frame) => { received.push(frame); });
  const output = {
    type: 'tmux_terminal_output',
    terminalId: 'tmuxterm-live',
    data: tmuxTerminalModule.encodeTmuxTerminalData('ready'),
  };
  sockets[0].emit(output);
  check('registry routes tmux_terminal_output without session identity', received.length === 1, JSON.stringify(received[0]));
  const routed = received[0];
  check('routed output keeps terminalId and omits sessionId',
    routed !== null && typeof routed === 'object'
      && 'terminalId' in routed && routed.terminalId === 'tmuxterm-live'
      && !('sessionId' in routed));
  sockets[0].emit({ type: 'tmux_terminal_exit', terminalId: 'tmuxterm-live', code: 0 });
  check('registry routes tmux_terminal_exit without session identity', received.length === 2);
  sockets[0].emit({
    type: 'tmux_terminal_history',
    terminalId: 'tmuxterm-live',
    paneKey: '%0',
    historyKey: '1:agentlo-pi-rs:%0',
    data: 'YQ==',
    final: true,
  });
  check('registry routes tmux_terminal_history without session identity', received.length === 3);
  sockets[0].emit({ type: 'tmux_terminal_output', terminalId: '', data: 'x' });
  check('invalid terminal output is not routed', received.length === 3);
  sockets[0].emit({ type: 'message_start', sessionId: 'session-a', runtimeIncarnation: incarnationOf('session-a'), sequence: 1 });
  check('session-scoped events still route after the terminal bypass', received.length === 4);
  unsubscribe();
  registry.stop();
  Object.defineProperty(globalThis, 'WebSocket', { configurable: true, value: previousSocket });
}

// ============================================================ //
// ---- ChatApp wiring + styles ----
// ============================================================ //
{
  const here = dirname(fileURLToPath(import.meta.url));
  const app = readFileSync(join(here, '../src/chat/ChatApp.tsx'), 'utf8');
  const panel = readFileSync(join(here, '../src/chat/TmuxPanel.tsx'), 'utf8');
  const terminal = readFileSync(join(here, '../src/chat/TmuxTerminal.tsx'), 'utf8');
  const styles = readFileSync(join(here, '../src/chat/styles.css'), 'utf8');

  check('ChatApp holds tmuxTerminal overlay state', app.includes('tmuxTerminal') && app.includes('setTmuxTerminal'));
  check('ChatApp mounts TmuxTerminal over the chat when set', app.includes('<TmuxTerminal'));
  check('ChatApp Open terminal lifts runnerId + session and reveals a kept overlay',
    app.includes('onOpenTerminal={(session) => setTmuxTerminal((current) => (')
      && app.includes('current.runnerId === focusedRunner.id && current.session === session && current.hidden')
      && app.includes('onClose={() => setTmuxTerminal((current) => (current ? { ...current, hidden: true } : null))}'));
  check('ChatApp overlay send rides registry.command for the overlay runner',
    app.includes('send={(command, options) => registry.command(tmuxTerminal.runnerId, command, options)}'));
  check('ChatApp overlay events subscribe per overlay runner',
    app.includes('onEvent={(callback) => subscribeRunnerEvents(tmuxTerminal.runnerId, callback)}'));
  check('ChatApp Esc closes the workspace menu before the overlay trap',
    app.includes('if (workspaceMenuOpen) {')
    && app.indexOf('if (workspaceMenuOpen) {') < app.indexOf('if (tmuxTerminal && !tmuxTerminal.hidden) return;')
    && app.includes("addEventListener('keydown', onKeyDown, true)"));
  check('ChatApp Esc keeps the visible overlay but never a hidden one',
    app.includes('if (tmuxTerminal && !tmuxTerminal.hidden) return;'));
  check('preview board and overlay stay independent of the project gear',
    app.includes('className="chat-gear-menu"')
    && !app.includes('<TmuxPanel')
    && app.includes('<TmuxPreviewBoard')
    && app.includes('<TmuxTerminal'));
  check('overlay Detach lives in the header with Close and Fit',
    terminal.includes('aria-label={attached ? \'Detach terminal\' : \'Attach terminal\'}')
      && terminal.indexOf('aria-label="Fit terminal to this device"') < terminal.indexOf('aria-label={attached ? \'Detach terminal\' : \'Attach terminal\'}')
      && terminal.indexOf('aria-label={attached ? \'Detach terminal\' : \'Attach terminal\'}') < terminal.indexOf('aria-label="Close terminal"'));
  check('overlay Detach detaches the PTY without killing the session',
    terminal.includes("type: 'tmux_terminal_detach'")
    && !/if \(attached\) \{[\s\S]*tmux_session_delete/.test(terminal));
  check('overlay Fit button is next to Close',
    terminal.includes('aria-label="Fit terminal to this device"')
    && terminal.includes('fitThisDeviceRef')
    && terminal.indexOf('aria-label="Fit terminal to this device"') < terminal.indexOf('aria-label="Close terminal"')
    && terminal.includes('<span aria-hidden="true">⤢</span>'));
  check('Fit in-flight footer and disable are wired',
    terminal.includes("'Fitting…'")
    && terminal.includes('disabled={fitting}')
    && !terminal.includes("' · Fitting…'")
    && !terminal.includes("' · Fit failed'"));
  check('overlay Fit and attach failures toast instead of the title',
    terminal.includes('TMUX_TERMINAL_FIT_FAILED')
    && terminal.includes('TMUX_TERMINAL_ATTACH_FAILED')
    && terminal.includes('onErrorRef.current?.(TMUX_TERMINAL_FIT_FAILED)')
    && terminal.includes('onErrorRef.current?.(TMUX_TERMINAL_ATTACH_FAILED)'));
  check('overlay file paste and drop use capture so xterm cannot swallow them',
    terminal.includes('onPasteCapture={onOverlayPaste}')
    && terminal.includes('onDropCapture={onOverlayDrop}')
    && terminal.includes('onDragOverCapture={onOverlayDragOver}')
    && terminal.includes('event.stopPropagation()'));

  check('ChatApp overlay errors use the session toast',
    /<TmuxTerminal[\s\S]*onError=\{setFormError\}/.test(app)
    && !/<TmuxTerminal[\s\S]*onNotice=\{setFormError\}/.test(app)
    && /loadToken=\{\(hostAuthority\) => loadTokenForAuthority\(storage, hostAuthority\)\}/.test(app));
  check('Detach and Attach in-flight footer statuses are wired',
    terminal.includes("'Detaching…'")
    && terminal.includes("'Attaching…'")
    && terminal.includes('disabled={ptyBusy !== null}'));
  check('overlay Detach and Attach pair a glyph with the label',
    terminal.includes('<span aria-hidden="true">⊟</span>')
    && terminal.includes('<span aria-hidden="true">↗</span>')
    && terminal.includes('>Detach</span>')
    && terminal.includes('>Attach</span>'));
  check('overlay pins to visualViewport when the keyboard opens',
    terminal.includes('window.visualViewport')
      && terminal.includes('overlay.style.top')
      && terminal.includes('overlay.style.height')
      && /visualViewport\.height/.test(terminal)
      && terminal.includes('preventScroll: true'));

  check('panel Open terminal uses the frozen aria-label', panel.includes('aria-label="Open terminal"'));
  check('panel Open terminal action is ⤢ Open terminal', panel.includes('⤢ Open terminal'));
  check('Esc on the overlay is stopped, never closed',
    terminal.includes("event.key === 'Escape'")
      && terminal.includes('event.stopPropagation()')
      && !/if \(event\.key === 'Escape'\) \{\s*onClose/.test(terminal));
  check('overlay soft-key bar sends terminal escape sequences',
    terminal.includes('chat-tmux-terminal__keys')
      && terminal.includes("['Esc', 'Escape', '\\x1b']")
      && terminal.includes('Sticky control key')
      && terminal.includes('sendTerminalBytesRef.current(sequence)'));
  check('overlay swallows DA queries in the parser not onData',
    terminal.includes("registerCsiHandler({ final: 'c' }, () => true)")
      && terminal.includes("registerCsiHandler({ prefix: '>', final: 'c' }, () => true)")
      && !/onData[\s\S]{0,800}84;0;0/.test(terminal)
      && !/onData[\s\S]{0,800}\\\\x1b\\\[>/.test(terminal));
  check('overlay xterm runs the Nerd Font stack',
    terminal.includes("'JetBrainsMono Nerd Font'")
      && terminal.includes("'Symbols Nerd Font'"));
  check('late attach after close detaches the terminalId',
    terminal.includes('if (cancelled || closedRef.current)') && terminal.includes("type: 'tmux_terminal_detach'"));
  check('overlay writes decoded bytes into xterm', terminal.includes('term.write(bytes, () => {'));
  check('wake probes the idle handle immediately',
    terminal.includes("document.addEventListener('visibilitychange', onVisibility)")
    && terminal.includes("window.addEventListener('focus', onWake)")
    && terminal.includes("window.addEventListener('pageshow', onWake)")
    && terminal.includes('setInterval(() => runHealthCheck(false), 2000)')
    && terminal.includes('runHealthCheck(true)'));
  check('overlay never auto-reattaches for full history',
    !terminal.includes('setFullHistory')
      && !terminal.includes('fullHistoryRequestedRef')
      && !terminal.includes('loadOlderHistory')
      && terminal.includes("{ type: 'tmux_terminal_attach', session, cols, rows }"));
  check('overlay attach payload has no fullHistory field',
    terminal.includes("{ type: 'tmux_terminal_attach', session, cols, rows }")
      && !terminal.includes('fullHistory ? { fullHistory: true }'));
  check('attach waits until the host is laid out',
    terminal.includes('tmuxTerminalHostReady(host)')
      && terminal.includes('tmuxTerminalGeometryReady(term.cols, term.rows)')
      && terminal.includes('new ResizeObserver(onHostLayout)')
      && /onHostLayout = \(\) => \{[\s\S]*tryAttach\(\);[\s\S]*new ResizeObserver\(onHostLayout\)/.test(terminal));
  check('overlay hides xterm until history.final',
    terminal.includes('chat-tmux-terminal__body--pending')
      && terminal.includes('aria-busy={!historyReady}')
      && terminal.includes('heldLiveRef')
      && terminal.includes('setHistoryReady(true)'));
  check('overlay body fills the grid cell for FitAddon',
    /\.chat-tmux-terminal__body \{[^}]*width:\s*100%[^}]*height:\s*100%/s.test(styles));
  check('overlay disables browser pinch zoom',
    /\.chat-tmux-terminal \{[^}]*touch-action:\s*none/s.test(styles)
      && /\.chat-tmux-terminal__viewport \{[^}]*touch-action:\s*none/s.test(styles));
  check('pending overlay hides xterm with visibility not display',
    /\.chat-tmux-terminal__body--pending \.xterm \{[^}]*visibility:\s*hidden/s.test(styles)
      && !/\.chat-tmux-terminal__body--pending \.xterm \{[^}]*display:\s*none/s.test(styles));

  check('overlay styles define header/body/footer',
    /\.chat-tmux-terminal__head \{/.test(styles)
      && /\.chat-tmux-terminal__body \{/.test(styles)
      && /\.chat-tmux-terminal__foot \{/.test(styles));
  check('overlay grid has four tracks for head/body/keys/footer',
    /\.chat-tmux-terminal \{[^}]*grid-template-rows:\s*auto minmax\(0,\s*1fr\) auto auto/s.test(styles));
  check('overlay terminal fills its body',
    /\.chat-tmux-terminal__viewport \{[^}]*height:\s*100%/s.test(styles)
      && /\.chat-tmux-terminal__viewport \{[^}]*place-items:\s*center/s.test(styles));
  check('overlay uses the app mono stack',
    /\.chat-tmux-terminal__foot \{[^}]*font-family:[^}]*monospace/s.test(styles));
  check('overlay stays square (no border-radius)',
    !/\.chat-tmux-terminal[^{]*\{[^}]*border-radius/.test(styles));
  check('overlay uses theme tokens only',
    !/\.chat-tmux-terminal[^{]*\{[^}]*#[0-9a-fA-F]{3,8}/.test(styles));
  check('overlay has a right border matching the rail',
    /\.chat-tmux-terminal \{[^}]*border-right:\s*1px solid var\(--border-strong\)/s.test(styles));
  check('overlay body keeps a right gutter for the scrollbar',
    /\.chat-tmux-terminal__body \{[^}]*padding:\s*0 12px 0 10px/s.test(styles));
  check('empty overlay footer takes no height',
    /\.chat-tmux-terminal__foot \{[^}]*padding:\s*0/s.test(styles)
      && /\.chat-tmux-terminal__foot:not\(:empty\) \{/.test(styles));
  check('overlay starts below the workspace bar',
    /\.chat-tmux-terminal \{[^}]*top:\s*44px/s.test(styles)
    && !/\.chat-tmux-terminal \{[^}]*inset:\s*0/s.test(styles));
}
{
  const pin = tmuxTerminalModule.planTmuxOverlayPinTop;
  check('overlay pin stays below a real workspace bar',
    pin({ visTop: 0, visBottom: 1180, barBottom: 44, barHeight: 44 }) === 44);
  check('overlay pin never covers a measured bar when visualViewport top is 0',
    pin({ visTop: 0, visBottom: 1180, barBottom: 0, barHeight: 44 }) === 44);
  check('missing bar uses the visual viewport top',
    pin({ visTop: 12, visBottom: 800, barBottom: 0, barHeight: 0 }) === 12);
}

if (failures.length > 0) {
  console.error(`chatTmuxTerminal: ${ran} checks, ${failures.length} FAILURES`);
  for (const failure of failures) console.error(`  - ${failure}`);
  process.exit(1);
}
console.log(`chatTmuxTerminal: ${ran} checks, 0 failures`);
process.exit(0);
