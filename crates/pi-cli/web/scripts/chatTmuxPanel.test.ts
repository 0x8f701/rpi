#!/usr/bin/env node
// Chat tmux panel regression for src/chat/TmuxPanel.tsx + the tmux_* wire
// shapes in src/chat/ChatApp.tsx / src/chat/protocol.ts. Run through the
// esbuild+node pattern used by the other chat scripts (`npm run build`).
// Exit codes: 0 = every assertion held; 1 = a regression.
//
// The component imports below are deliberately dynamic: react-dom snapshots
// `canUseDOM`/`isInputEventSupported` at module-init, so the mini-DOM
// globals MUST be installed before react-dom is evaluated (the same
// ordering contract chatProjectCwd.test.ts documents). A static value
// import would hoist above installGlobals() and freeze that snapshot with
// globals absent.
//
// Covers: the three fail-closed response parsers (parseTmuxSessionList /
// parseTmuxPaneSnapshot / parseTmuxSendResult — good payloads, garbage
// payloads, and whole-payload rejection when one row drifts), the frozen
// RunnerCommand shapes the panel actually sends (list on mount, snapshot
// with lines:120, send-keys with text / key), panel behavior through a
// mini-DOM React mount (placeholders while loading, fixed neutral lines on
// drift, select/deselect, Send disabled on empty input, Enter-in-input send,
// the exact Enter/Esc/Ctrl-C aria-labels and key payloads, safeText-bounded
// error line, remote text through safeText), ChatApp's connected-only mount
// gate, and the .chat-tmux* stylesheet contract.
import type { act as Act, createElement as CreateElement } from 'react';
import type { createRoot as CreateRoot, Root } from 'react-dom/client';
import type * as TmuxPanelModule from '../src/chat/TmuxPanel.tsx';
import type * as ProtocolModule from '../src/chat/protocol.ts';
import type { RunnerCommandError as RunnerCommandErrorClass } from '../src/chat/runners.ts';
import type { RunnerCommand } from '../src/chat/protocol.ts';
import type { RpcResponse } from '../src/types.ts';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

let createElement: CreateElement;
let act: Act;
let createRoot: CreateRoot;
let TmuxPanel: TmuxPanelModule.TmuxPanel;
let protocol: typeof ProtocolModule;
let tmuxPanelModule: typeof TmuxPanelModule;
let RunnerCommandError: typeof RunnerCommandErrorClass;

const realSetTimeout = globalThis.setTimeout.bind(globalThis);
let clockNextId = 1;
const clockPending: Array<{ id: number; fn: () => void }> = [];

function fakeSetTimeout(fn: () => void, _ms?: number): unknown {
  const id = clockNextId;
  clockNextId += 1;
  clockPending.push({ id, fn });
  return id;
}

function fakeClear(id: unknown): void {
  const index = clockPending.findIndex((entry) => entry.id === id);
  if (index >= 0) clockPending.splice(index, 1);
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
  focus(): void;
  blur(): void;
  contains(node: MiniNode | null): boolean;
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
  createTextNode(text: string): MiniNode;
  createComment(text: string): MiniNode;
  createDocumentFragment(): MiniNode;
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
      addEventListener(type: string, fn: Listener) {
        const set = node.listeners.get(type) ?? new Set();
        set.add(fn);
        node.listeners.set(type, set);
      },
      removeEventListener(type: string, fn: Listener) {
        node.listeners.get(type)?.delete(fn);
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
    setTimeout: fakeSetTimeout,
    clearTimeout: fakeClear,
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
  (globalThis as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
}

// ---- fixtures ----
interface DeferredCommand {
  command: RunnerCommand;
  resolve: (response: RpcResponse) => void;
  reject: (error: Error) => void;
  settled: boolean;
}

let sent: RunnerCommand[] = [];
let deferreds: DeferredCommand[] = [];

function okResponse(command: string, data: unknown): RpcResponse {
  return { type: 'response', command, success: true, data, sessionId: 'sess-1' };
}

let optionCalls: Array<{ sourceSessionId?: string } | undefined> = [];
function harnessSend(command: RunnerCommand, options?: { sourceSessionId?: string }): Promise<RpcResponse> {
  sent.push(command);
  optionCalls.push(options);
  const { promise, resolve, reject } = Promise.withResolvers<RpcResponse>();
  deferreds.push({ command, resolve, reject, settled: false });
  return promise;
}

function optionsOf(type: string): Array<{ sourceSessionId?: string } | undefined> {
  return optionCalls.filter((_, index) => sent[index]?.type === type);
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

function settleNextFailure(type: string, error: Error): boolean {
  const deferred = deferreds.find((entry) => entry.command.type === type && !entry.settled);
  if (!deferred) return false;
  deferred.settled = true;
  deferred.reject(error);
  return true;
}

function listPayload(tmuxAvailable = true): Record<string, unknown> {
  return {
    tmuxAvailable,
    sessions: [
      { name: 'dev', windows: 2, attached: true, createdEpoch: 1757400000, paneCount: 3 },
      { name: 'ci', windows: 1, attached: false, createdEpoch: 1757400100, paneCount: 1 },
    ],
  };
}

const failures: string[] = [];
let ran = 0;
function check(name: string, condition: boolean, detail = ''): void {
  ran += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

function isDisabled(node: MiniNode | null): boolean {
  if (!node) return false;
  return node.getAttribute('disabled') !== null || node.disabled === true;
}

let container: MiniNode;
let root: Root | null = null;

async function mountPanel(sessionId: string | null = 'sess-1'): Promise<void> {
  sent = [];
  deferreds = [];
  optionCalls = [];
  container = mini.document.createElement('div');
  mini.document.body.appendChild(container);
  root = createRoot(container as unknown as Element);
  await act(async () => {
    root!.render(
      createElement(TmuxPanel, {
        runnerId: 'r1',
        runnerLabel: 'Runner r1',
        sessionId,
        send: harnessSend,
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

async function click(node: MiniNode): Promise<void> {
  await act(async () => {
    node.click();
    await realTick();
  });
}

async function typeText(node: MiniNode, value: string): Promise<void> {
  await act(async () => {
    node.value = value;
    node.dispatchEvent({ type: 'input', target: node });
    await realTick();
  });
}

async function settleAndTick(type: string, data: unknown): Promise<void> {
  check(`${type} request settles`, settleNext(type, data));
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

function textOf(selector: string): string {
  const node = mini.document.body.querySelector(selector);
  return node ? node.textContent : '';
}

installGlobals();

const react = await import('react');
const reactDomClient = await import('react-dom/client');
const chatRunners = await import('../src/chat/runners.ts');
protocol = await import('../src/chat/protocol.ts');
tmuxPanelModule = await import('../src/chat/TmuxPanel.tsx');
RunnerCommandError = chatRunners.RunnerCommandError;
act = react.act as Act;
createElement = react.createElement as CreateElement;
createRoot = reactDomClient.createRoot as CreateRoot;
TmuxPanel = tmuxPanelModule.TmuxPanel;

// ============================================================ //
// ---- fail-closed parsers (protocol.ts) ----
// ============================================================ //
{
  const parseSessionList = protocol.parseTmuxSessionList;
  const good = listPayload();
  const parsedGood = parseSessionList(good);
  check('list parser accepts a well-formed payload', parsedGood !== null, JSON.stringify(good));
  check('list availability flag survives', parsedGood?.tmuxAvailable === true);
  check('list rows survive intact', parsedGood?.sessions.length === 2 && parsedGood.sessions[0]?.name === 'dev');
  check('list row scalars survive', parsedGood?.sessions[0]?.windows === 2 && parsedGood.sessions[0]?.paneCount === 3);
  check('list unavailable:false stays parseable', parseSessionList({ tmuxAvailable: false, sessions: [] })?.tmuxAvailable === false);

  const badRows: Array<[string, Record<string, unknown>]> = [
    ['missing tmuxAvailable', { sessions: [] }],
    ['string tmuxAvailable', { tmuxAvailable: 'true', sessions: [] }],
    ['missing sessions', { tmuxAvailable: true }],
    ['non-array sessions', { tmuxAvailable: true, sessions: {} }],
    ['non-object row', { tmuxAvailable: true, sessions: ['dev'] }],
    ['row missing name', { tmuxAvailable: true, sessions: [{ windows: 1, attached: false, createdEpoch: 1, paneCount: 1 }] }],
    ['row empty name', { tmuxAvailable: true, sessions: [{ name: '', windows: 1, attached: false, createdEpoch: 1, paneCount: 1 }] }],
    ['row missing windows', { tmuxAvailable: true, sessions: [{ name: 'dev', attached: false, createdEpoch: 1, paneCount: 1 }] }],
    ['row string windows', { tmuxAvailable: true, sessions: [{ name: 'dev', windows: '2', attached: false, createdEpoch: 1, paneCount: 1 }] }],
    ['row float windows', { tmuxAvailable: true, sessions: [{ name: 'dev', windows: 1.5, attached: false, createdEpoch: 1, paneCount: 1 }] }],
    ['row NaN windows', { tmuxAvailable: true, sessions: [{ name: 'dev', windows: Number.NaN, attached: false, createdEpoch: 1, paneCount: 1 }] }],
    ['row negative windows', { tmuxAvailable: true, sessions: [{ name: 'dev', windows: -1, attached: false, createdEpoch: 1, paneCount: 1 }] }],
    ['row missing attached', { tmuxAvailable: true, sessions: [{ name: 'dev', windows: 1, createdEpoch: 1, paneCount: 1 }] }],
    ['row string attached', { tmuxAvailable: true, sessions: [{ name: 'dev', windows: 1, attached: 'true', createdEpoch: 1, paneCount: 1 }] }],
    ['row missing createdEpoch', { tmuxAvailable: true, sessions: [{ name: 'dev', windows: 1, attached: false, paneCount: 1 }] }],
    ['row Infinity createdEpoch', { tmuxAvailable: true, sessions: [{ name: 'dev', windows: 1, attached: false, createdEpoch: Number.POSITIVE_INFINITY, paneCount: 1 }] }],
    ['row missing paneCount', { tmuxAvailable: true, sessions: [{ name: 'dev', windows: 1, attached: false, createdEpoch: 1 }] }],
  ];
  for (const [name, payload] of badRows) {
    check(`list rejects: ${name}`, parseSessionList(payload) === null, JSON.stringify(payload));
  }
  const rows = good.sessions as Array<Record<string, unknown>>;
  check('list rejects the WHOLE payload when one late row drifts',
    parseSessionList({ tmuxAvailable: true, sessions: [rows[0], rows[1], { name: 'bad' }] }) === null,
    'one malformed row must not yield a partial list');
  check('list rejects null data', parseSessionList(null) === null);
  check('list rejects non-object data', parseSessionList('nope') === null);

  const parseSnapshot = protocol.parseTmuxPaneSnapshot;
  const snapshotGood = { session: 'dev', pane: '0', width: 120, height: 40, content: '$ ls\ndev ci' };
  check('snapshot accepts a good payload', parseSnapshot(snapshotGood) !== null);
  check('snapshot geometry survives', parseSnapshot(snapshotGood)?.width === 120 && parseSnapshot(snapshotGood)?.height === 40);
  check('snapshot whitespace content survives verbatim', parseSnapshot({ ...snapshotGood, content: '  \n  ' })?.content === '  \n  ');
  const snapshotBad: Array<[string, unknown]> = [
    ['missing session', { pane: '0', width: 1, height: 1, content: 'x' }],
    ['empty session', { session: '', pane: '0', width: 1, height: 1, content: 'x' }],
    ['missing pane', { session: 'dev', width: 1, height: 1, content: 'x' }],
    ['empty pane', { session: 'dev', pane: '', width: 1, height: 1, content: 'x' }],
    ['missing width', { session: 'dev', pane: '0', height: 40, content: 'x' }],
    ['zero width', { session: 'dev', pane: '0', width: 0, height: 40, content: 'x' }],
    ['negative width', { session: 'dev', pane: '0', width: -1, height: 40, content: 'x' }],
    ['fractional width', { session: 'dev', pane: '0', width: 1.5, height: 40, content: 'x' }],
    ['string width', { session: 'dev', pane: '0', width: '120', height: 40, content: 'x' }],
    ['NaN width', { session: 'dev', pane: '0', width: Number.NaN, height: 40, content: 'x' }],
    ['Infinity height', { session: 'dev', pane: '0', width: 120, height: Number.POSITIVE_INFINITY, content: 'x' }],
    ['missing height', { session: 'dev', pane: '0', width: 120, content: 'x' }],
    ['missing content', { session: 'dev', pane: '0', width: 120, height: 40 }],
    ['empty content', { session: 'dev', pane: '0', width: 120, height: 40, content: '' }],
    ['non-string content', { session: 'dev', pane: '0', width: 120, height: 40, content: 42 }],
  ];
  for (const [name, payload] of snapshotBad) {
    check(`snapshot rejects: ${name}`, parseSnapshot(payload) === null, JSON.stringify(payload));
  }
  check('snapshot rejects null data', parseSnapshot(null) === null);

  const parseSendResult = protocol.parseTmuxSendResult;
  check('send result accepts the good payload', parseSendResult({ session: 'dev', sent: true })?.sent === true);
  const sendBad: Array<[string, unknown]> = [
    ['missing sent', { session: 'dev' }],
    ['string sent', { session: 'dev', sent: 'true' }],
    ['false sent', { session: 'dev', sent: false }],
    ['missing session', { sent: true }],
    ['empty session', { session: '', sent: true }],
  ];
  for (const [name, payload] of sendBad) {
    check(`send result rejects: ${name}`, parseSendResult(payload) === null, JSON.stringify(payload));
  }
  check('send result rejects null data', parseSendResult(null) === null);
}

// ============================================================ //
// ---- mounted component behavior (mini-DOM + React) ----
// ============================================================ //

// ---- mount: exactly one list request; `--` placeholder while loading ----
{
  await mountPanel();
  check('mount sends exactly one tmux_list_sessions', commandsOf('tmux_list_sessions').length === 1);
  const listCommand = commandsOf('tmux_list_sessions')[0];
  check('list command is the bare contract shape', JSON.stringify(listCommand) === '{"type":"tmux_list_sessions"}', JSON.stringify(listCommand));
  check('list request forwards the probe sessionId for the WS transport',
    optionsOf('tmux_list_sessions')[0]?.sourceSessionId === 'sess-1', JSON.stringify(optionsOf('tmux_list_sessions')));
  check('mount renders the loading placeholder, not rows', mini.document.body.querySelector('.chat-tmux__status--placeholder') !== null && (mini.document.body.querySelectorAll('.chat-tmux__row') as MiniNode[]).length === 0);
  check('loading placeholder is the literal -- marker', mini.document.body.querySelector('.chat-tmux__status--placeholder')?.textContent === '--');
  check('mount sends no snapshot or keys command', commandsOf('tmux_pane_snapshot').length === 0 && commandsOf('tmux_send_keys').length === 0);
}

// ---- good list: rows, counts, chip, safeText ----
{
  await settleAndTick('tmux_list_sessions', listPayload());
  const rows = mini.document.body.querySelectorAll('.chat-tmux__row') as MiniNode[];
  check('good list renders both rows', rows.length === 2, String(rows.length));
  check('first row shows the name', rows[0]?.querySelector('.chat-tmux__name')?.textContent === 'dev');
  check('row shows the window count', rows[0]?.querySelector('.chat-tmux__meta')?.textContent === '2 windows');
  check('attached chip only on attached sessions', rows[0]?.querySelector('.chat-tmux__chip') !== null && rows[1]?.querySelector('.chat-tmux__chip') === null);
  const secretRow = listPayload();
  (secretRow.sessions as Array<Record<string, unknown>>)[0] = {
    name: 'ghp_aaaaaaaaaaaaaaaaaaaa',
    windows: 1,
    attached: false,
    createdEpoch: 1,
    paneCount: 1,
  };
  await unmountPanel();
  await mountPanel();
  await settleAndTick('tmux_list_sessions', secretRow);
  const redactedRow = (mini.document.body.querySelectorAll('.chat-tmux__row') as MiniNode[])[0];
  check('remote session name passes through safeText redaction', redactedRow?.querySelector('.chat-tmux__name')?.textContent === '[REDACTED]', redactedRow?.querySelector('.chat-tmux__name')?.textContent ?? 'missing');
}

// ---- no server / drift / transport failure: fixed neutral lines ----
{
  await unmountPanel();
  await mountPanel();
  await settleAndTick('tmux_list_sessions', { tmuxAvailable: false, sessions: [] });
  check('unavailable tmux shows the fixed line', mini.document.body.textContent?.includes(tmuxPanelModule.TMUX_NO_SERVER) === true);

  await unmountPanel();
  await mountPanel();
  await settleAndTick('tmux_list_sessions', { tmuxAvailable: true, sessions: [{ name: 'x' }] });
  check('drifted list payload shows the fixed unavailable line', mini.document.body.textContent?.includes(tmuxPanelModule.TMUX_STATUS_UNAVAILABLE) === true);
  check('drifted list renders no rows', (mini.document.body.querySelectorAll('.chat-tmux__row') as MiniNode[]).length === 0);

  await unmountPanel();
  await mountPanel();
  await act(async () => {
    settleNextFailure('tmux_list_sessions', new RunnerCommandError('socket gone', 'transport'));
    await realTick();
  });
  check('transport failure shows the fixed unavailable line', mini.document.body.textContent?.includes(tmuxPanelModule.TMUX_STATUS_UNAVAILABLE) === true);

  await unmountPanel();
  await mountPanel();
  await act(async () => {
    const deferred = deferreds.find((entry) => entry.command.type === 'tmux_list_sessions' && !entry.settled);
    deferred!.resolve({ type: 'response', command: 'tmux_list_sessions', success: false, data: null, sessionId: 'sess-1' });
    await realTick();
  });
  check('unsuccessful response shows the fixed unavailable line', mini.document.body.textContent?.includes(tmuxPanelModule.TMUX_STATUS_UNAVAILABLE) === true);
}

// ---- select → snapshot fetch, placeholders, refresh, deselect ----
{
  await unmountPanel();
  await mountPanel();
  await settleAndTick('tmux_list_sessions', listPayload());
  const rows = mini.document.body.querySelectorAll('.chat-tmux__row') as MiniNode[];
  await click(rows[0]!);
  const snapshotCommands = commandsOf('tmux_pane_snapshot');
  check('snapshot request forwards the probe sessionId for the WS transport',
    optionsOf('tmux_pane_snapshot')[0]?.sourceSessionId === 'sess-1', JSON.stringify(optionsOf('tmux_pane_snapshot')));
  check('row click sends exactly one tmux_pane_snapshot', snapshotCommands.length === 1, JSON.stringify(snapshotCommands));
  check('snapshot command carries the frozen shape (session + lines 120)',
    JSON.stringify(snapshotCommands[0]) === '{"type":"tmux_pane_snapshot","session":"dev","lines":120}'
    && tmuxPanelModule.TMUX_PANE_LINES === 120,
    JSON.stringify(snapshotCommands[0]));
  check('pane id placeholders while snapshot loads', mini.document.body.querySelector('.chat-tmux__pane-id')?.textContent === '--' && mini.document.body.querySelector('.chat-tmux__dims')?.textContent === '--');
  check('pane pre placeholder while loading', mini.document.body.querySelector('.chat-tmux__pre--placeholder')?.textContent === '--');

  await settleAndTick('tmux_pane_snapshot', { session: 'dev', pane: '0', width: 120, height: 40, content: '$ ls\ndev ci' });
  check('snapshot renders the pane id', mini.document.body.querySelector('.chat-tmux__pane-id')?.textContent === '0');
  check('snapshot renders the WxH header', mini.document.body.querySelector('.chat-tmux__dims')?.textContent === '120\u00d740');
  check('snapshot renders the pane content', mini.document.body.querySelector('.chat-tmux__pre')?.textContent === '$ ls\ndev ci');
  check('selected row is marked active', rows[0]?.getAttribute('data-active') === 'true');

  const paneRefresh = buttonByAriaLabel('Refresh tmux pane');
  check('pane Refresh button exists', paneRefresh !== null);
  await click(paneRefresh!);
  check('pane Refresh re-fetches the snapshot', commandsOf('tmux_pane_snapshot').length === 2, JSON.stringify(commandsOf('tmux_pane_snapshot')));
  check('pane refetch shows the placeholder, not the stale frame',
    mini.document.body.querySelector('.chat-tmux__pre--placeholder') !== null
    && mini.document.body.querySelector('.chat-tmux__pre')?.textContent === '--'
    && mini.document.body.querySelector('.chat-tmux__pane-id')?.textContent === '--');
  check('pane Refresh is disabled only while the fetch is in flight', isDisabled(mini.document.body.querySelector('[aria-label="Refresh tmux pane"]')));
  await settleAndTick('tmux_pane_snapshot', { session: 'dev', pane: '0', width: 120, height: 40, content: '$ ls\ndev ci' });
  check('settled refetch restores the pane frame', mini.document.body.querySelector('.chat-tmux__pre')?.textContent === '$ ls\ndev ci');

  await click(rows[0]!);
  check('clicking the selected row closes the viewer', mini.document.body.querySelector('.chat-tmux__viewer') === null);
}

// ---- snapshot drift + failure show the fixed pane line ----
{
  await unmountPanel();
  await mountPanel();
  await settleAndTick('tmux_list_sessions', listPayload());
  const rows = mini.document.body.querySelectorAll('.chat-tmux__row') as MiniNode[];
  await click(rows[0]!);
  await act(async () => {
    settleNextFailure('tmux_pane_snapshot', new RunnerCommandError('gone', 'transport'));
    await realTick();
  });
  check('snapshot transport failure shows the fixed pane line', mini.document.body.textContent?.includes(tmuxPanelModule.TMUX_PANE_UNAVAILABLE) === true);

  check('pane Refresh stays clickable after a failure', !isDisabled(mini.document.body.querySelector('[aria-label="Refresh tmux pane"]')));
  await click(mini.document.body.querySelector('[aria-label="Refresh tmux pane"]')!);
  check('retry fetch replaces the failure with the placeholder',
    mini.document.body.querySelector('.chat-tmux__pre--placeholder') !== null && mini.document.body.textContent?.includes(tmuxPanelModule.TMUX_PANE_UNAVAILABLE) === false);
  await settleAndTick('tmux_pane_snapshot', { session: 'dev', pane: '0', width: 80, height: 24, content: '$ ok' });
  check('successful retry restores the pane frame', mini.document.body.querySelector('.chat-tmux__pre')?.textContent === '$ ok');

  await click(rows[0]!);
  await click(rows[0]!);
  await settleAndTick('tmux_pane_snapshot', { session: 'dev', pane: '0', width: 0, height: 40, content: 'x' });
  check('drifted snapshot (zero width) shows the fixed pane line', mini.document.body.textContent?.includes(tmuxPanelModule.TMUX_PANE_UNAVAILABLE) === true);
}

// ---- safeText on pane content ----
{
  await unmountPanel();
  await mountPanel();
  await settleAndTick('tmux_list_sessions', listPayload());
  const rows = mini.document.body.querySelectorAll('.chat-tmux__row') as MiniNode[];
  await click(rows[0]!);
  await settleAndTick('tmux_pane_snapshot', {
    session: 'dev',
    pane: '0',
    width: 80,
    height: 24,
    content: 'export GITHUB_TOKEN=ghp_aaaaaaaaaaaaaaaaaaaa',
  });
  check('pane content passes through safeText redaction', mini.document.body.querySelector('.chat-tmux__pre')?.textContent === 'export GITHUB_TOKEN=[REDACTED]', mini.document.body.querySelector('.chat-tmux__pre')?.textContent ?? 'missing');
}

// ---- send-keys controls ----
{
  await unmountPanel();
  await mountPanel();
  await settleAndTick('tmux_list_sessions', listPayload());
  const rows = mini.document.body.querySelectorAll('.chat-tmux__row') as MiniNode[];
  await click(rows[0]!);
  await settleAndTick('tmux_pane_snapshot', { session: 'dev', pane: '0', width: 80, height: 24, content: '$ ' });
  sent = [];
  deferreds = [];

  const sendButton = mini.document.body.querySelector('.chat-tmux__send');
  check('Send button is disabled on empty input', isDisabled(sendButton), sendButton ? String(sendButton.getAttribute('disabled')) : 'missing');
  const input = mini.document.body.querySelector('.chat-tmux__input');
  check('input carries the literal-text placeholder', input?.getAttribute('placeholder') === 'Send literal text…', input?.getAttribute('placeholder') ?? 'missing');

  await typeText(input!, 'ls -la\n');
  check('Send button is enabled once the input has text', !isDisabled(mini.document.body.querySelector('.chat-tmux__send')));

  await click(mini.document.body.querySelector('.chat-tmux__send')!);
  const textCommands = commandsOf('tmux_send_keys');
  check('Send click sends the literal text', textCommands.length === 1 && JSON.stringify(commandsOf('tmux_send_keys')[0]) === '{"type":"tmux_send_keys","session":"dev","text":"ls -la\\n"}', JSON.stringify(textCommands));
  await act(async () => {
    settleNext('tmux_send_keys', { session: 'dev', sent: true });
    await realTick();
  });
  check('input clears after a send', mini.document.body.querySelector('.chat-tmux__input')?.value === '');
  check('Send re-disables after the cleared send', isDisabled(mini.document.body.querySelector('.chat-tmux__send')));

  await typeText(input!, 'echo done');
  await act(async () => {
    mini.document.body.querySelector('.chat-tmux__input')!.dispatchEvent({ type: 'keydown', key: 'Enter' });
    await realTick();
  });
  const enterTextCommands = commandsOf('tmux_send_keys');
  check('Enter in the input sends the text', commandsOf('tmux_send_keys').length === 2 && JSON.stringify(commandsOf('tmux_send_keys')[1]) === '{"type":"tmux_send_keys","session":"dev","text":"echo done"}', JSON.stringify(enterTextCommands));
  await act(async () => {
    settleNext('tmux_send_keys', { session: 'dev', sent: true });
    await realTick();
  });

  const ctrlC = buttonByAriaLabel('Ctrl-C');
  const enterKey = buttonByAriaLabel('Enter');
  const escKey = buttonByAriaLabel('Esc');
  check('Ctrl-C button exists with the exact aria-label', ctrlC !== null && ctrlC.textContent === 'Ctrl-C');
  await click(ctrlC!);
  check('Ctrl-C sends key ctrl_c on the selected session', JSON.stringify(commandsOf('tmux_send_keys').at(-1)) === '{"type":"tmux_send_keys","session":"dev","key":"ctrl_c"}', JSON.stringify(commandsOf('tmux_send_keys')));
  check('keys request forwards the probe sessionId for the WS transport',
    optionsOf('tmux_send_keys').at(-1)?.sourceSessionId === 'sess-1', JSON.stringify(optionsOf('tmux_send_keys')));
  await act(async () => {
    settleNext('tmux_send_keys', { session: 'dev', sent: true });
    await realTick();
  });
  await click(enterKey!);
  check('Enter button sends key enter', JSON.stringify(commandsOf('tmux_send_keys').at(-1)) === '{"type":"tmux_send_keys","session":"dev","key":"enter"}', JSON.stringify(commandsOf('tmux_send_keys')));
  await act(async () => {
    settleNext('tmux_send_keys', { session: 'dev', sent: true });
    await realTick();
  });
  await click(escKey!);
  check('Esc button sends key escape', JSON.stringify(commandsOf('tmux_send_keys').at(-1)) === '{"type":"tmux_send_keys","session":"dev","key":"escape"}', JSON.stringify(commandsOf('tmux_send_keys')));
  await act(async () => {
    settleNext('tmux_send_keys', { session: 'dev', sent: true });
    await realTick();
  });
}

// ---- send-keys failures: remote error through safeText; drift through fixed copy ----
{
  await unmountPanel();
  await mountPanel();
  await settleAndTick('tmux_list_sessions', listPayload());
  const rows = mini.document.body.querySelectorAll('.chat-tmux__row') as MiniNode[];
  await click(rows[0]!);
  await settleAndTick('tmux_pane_snapshot', { session: 'dev', pane: '0', width: 80, height: 24, content: '$ ' });
  sent = [];
  deferreds = [];

  await click(buttonByAriaLabel('Ctrl-C')!);
  await act(async () => {
    settleNextFailure('tmux_send_keys', new RunnerCommandError('tmux_send_keys refused: <boom> ghp_bbbbbbbbbbbbbbbbbbbb', 'remote'));
    await realTick();
  });
  const errorLine = mini.document.body.querySelector('.chat-tmux__error');
  check('send-keys failure renders an inline error line', errorLine !== null);
  check('remote error text is safeText-bounded (redacted, no raw token)', errorLine?.textContent === 'tmux_send_keys refused: <boom> [REDACTED]', errorLine?.textContent ?? 'missing');

  await click(buttonByAriaLabel('Ctrl-C')!);
  await act(async () => {
    const deferred = deferreds.find((entry) => entry.command.type === 'tmux_send_keys' && !entry.settled);
    deferred!.resolve({ type: 'response', command: 'tmux_send_keys', success: false, data: undefined, sessionId: 'sess-1' });
    await realTick();
  });
  check('unsuccessful send shows the fixed send-failed line', mini.document.body.querySelector('.chat-tmux__error')?.textContent === tmuxPanelModule.TMUX_SEND_FAILED, mini.document.body.querySelector('.chat-tmux__error')?.textContent ?? 'missing');
  await unmountPanel();
}

// ---- send-keys literal failures keep the composer draft ----
{
  await mountPanel();
  await settleAndTick('tmux_list_sessions', listPayload());
  const rows = mini.document.body.querySelectorAll('.chat-tmux__row') as MiniNode[];
  await click(rows[0]!);
  await settleAndTick('tmux_pane_snapshot', { session: 'dev', pane: '0', width: 80, height: 24, content: '$ ' });
  sent = [];
  deferreds = [];

  const draftInput = mini.document.body.querySelector('.chat-tmux__input');
  await typeText(draftInput!, 'keep-this-draft');
  await click(mini.document.body.querySelector('.chat-tmux__send')!);
  await act(async () => {
    settleNextFailure('tmux_send_keys', new RunnerCommandError('tmux_send_keys refused: <boom> ghp_bbbbbbbbbbbbbbbbbbbb', 'remote'));
    await realTick();
  });
  check('literal send failure keeps the composer draft', mini.document.body.querySelector('.chat-tmux__input')?.value === 'keep-this-draft', mini.document.body.querySelector('.chat-tmux__input')?.value ?? 'missing');
  check('literal send failure still shows the error line', mini.document.body.querySelector('.chat-tmux__error') !== null);

  await click(mini.document.body.querySelector('.chat-tmux__send')!);
  await act(async () => {
    const deferred = deferreds.find((entry) => entry.command.type === 'tmux_send_keys' && !entry.settled);
    deferred!.settled = true;
    deferred!.resolve({ type: 'response', command: 'tmux_send_keys', success: false, data: undefined, sessionId: 'sess-1' });
    await realTick();
  });
  check('unsuccessful literal send keeps the composer draft', mini.document.body.querySelector('.chat-tmux__input')?.value === 'keep-this-draft', mini.document.body.querySelector('.chat-tmux__input')?.value ?? 'missing');
  check('unsuccessful literal send shows the fixed send-failed line', mini.document.body.querySelector('.chat-tmux__error')?.textContent === tmuxPanelModule.TMUX_SEND_FAILED, mini.document.body.querySelector('.chat-tmux__error')?.textContent ?? 'missing');
  await unmountPanel();
}

// ---- list Refresh button ----
{
  await mountPanel();
  await settleAndTick('tmux_list_sessions', listPayload());
  sent = [];
  deferreds = [];
  await click(buttonByAriaLabel('Refresh tmux sessions')!);
  check('list Refresh re-issues tmux_list_sessions', commandsOf('tmux_list_sessions').length === 1, JSON.stringify(sent));
  check('last-good rows stay mounted while listLoading',
    (mini.document.body.querySelectorAll('.chat-tmux__row') as MiniNode[]).length === 2
    && mini.document.body.querySelector('.chat-tmux__sessions') !== null
    && mini.document.body.querySelector('.chat-tmux__status--placeholder') === null
    && isDisabled(mini.document.body.querySelector('[aria-label="Refresh tmux sessions"]')));
  check('list Refresh is disabled only while the fetch is in flight', isDisabled(mini.document.body.querySelector('[aria-label="Refresh tmux sessions"]')));
  await settleAndTick('tmux_list_sessions', listPayload());
  check('settled refetch restores the rows', (mini.document.body.querySelectorAll('.chat-tmux__row') as MiniNode[]).length === 2);

  await click(buttonByAriaLabel('Refresh tmux sessions')!);
  await act(async () => {
    settleNextFailure('tmux_list_sessions', new RunnerCommandError('socket gone', 'transport'));
    await realTick();
  });
  check('failed refresh keeps the last-good list',
    (mini.document.body.querySelectorAll('.chat-tmux__row') as MiniNode[]).length === 2
    && mini.document.body.querySelector('.chat-tmux__sessions') !== null
    && mini.document.body.querySelector('.chat-tmux__status--placeholder') === null
    && mini.document.body.textContent?.includes(tmuxPanelModule.TMUX_STATUS_UNAVAILABLE) === false);
  await unmountPanel();
}

// ---- runner switch must remount (ChatApp keys on runner.id): no selection,
// draft, or stale snapshot of runner A survives into runner B's panel ----
{
  await mountPanel();
  await settleAndTick('tmux_list_sessions', listPayload());
  const rows = mini.document.body.querySelectorAll('.chat-tmux__row') as MiniNode[];
  await click(rows[0]!);
  const input = mini.document.body.querySelector('.chat-tmux__input');
  await typeText(input!, 'draft for runner A');
  check('precondition: draft and selection exist for runner A',
    mini.document.body.querySelector('.chat-tmux__input')?.value === 'draft for runner A'
    && mini.document.body.querySelector('.chat-tmux__viewer') !== null);

  // ChatApp keys the panel on `runner.id:probe.sessionId`, so switching the
  // gear target (or its primary session) replaces the instance outright.
  const listsBefore = commandsOf('tmux_list_sessions').length;
  const snapshotBefore = commandsOf('tmux_pane_snapshot').length;
  await act(async () => {
    root!.render(
      createElement(TmuxPanel, {
        key: 'r2',
        runnerId: 'r2',
        runnerLabel: 'Runner r2',
        sessionId: 'sess-2',
        send: harnessSend,
      }),
    );
    await realTick();
  });
  check('runner switch remounts the panel with a fresh list request',
    commandsOf('tmux_list_sessions').length === listsBefore + 1
    && commandsOf('tmux_pane_snapshot').length === snapshotBefore,
    JSON.stringify(sent));
  check('remounted instance routes with the NEW runner session identity',
    optionsOf('tmux_list_sessions').at(-1)?.sourceSessionId === 'sess-2', JSON.stringify(optionsOf('tmux_list_sessions')));
  check('remount clears runner A selection and draft', mini.document.body.querySelector('.chat-tmux__viewer') === null && mini.document.body.querySelector('.chat-tmux__input') === null);

  // The in-flight runner-A snapshot settles after the switch: it must not
  // commit into the new (unmounted) instance or bleed into runner B's panel.
  await act(async () => {
    settleNext('tmux_pane_snapshot', { session: 'dev', pane: '0', width: 120, height: 40, content: 'STALE-A' });
    await realTick();
  });
  check('stale runner-A snapshot commits nothing after the switch',
    mini.document.body.querySelector('.chat-tmux__pre') === null
    && !mini.document.body.textContent?.includes('STALE-A'));
  await settleAndTick('tmux_list_sessions', listPayload());
  check('fresh instance loads its own list', (mini.document.body.querySelectorAll('.chat-tmux__row') as MiniNode[]).length === 2);
  await unmountPanel();
}

// ---- empty session list keeps a neutral line, never an empty frame ----
{
  await mountPanel();
  await settleAndTick('tmux_list_sessions', { tmuxAvailable: true, sessions: [] });
  check('available server with zero sessions shows the empty line', mini.document.body.textContent?.includes(tmuxPanelModule.TMUX_NO_SESSIONS) === true);
  await unmountPanel();
}

// ---- null probe sessionId: fixed neutral line, zero wire traffic ----
{
  await mountPanel(null);
  check('null sessionId renders the fixed unavailable line', mini.document.body.textContent?.includes(tmuxPanelModule.TMUX_STATUS_UNAVAILABLE) === true);
  check('null sessionId fires no request of any kind', sent.length === 0, JSON.stringify(sent));
  check('null sessionId keeps the list Refresh disabled', isDisabled(mini.document.body.querySelector('[aria-label="Refresh tmux sessions"]')));
  check('null sessionId renders no rows and no viewer',
    (mini.document.body.querySelectorAll('.chat-tmux__row') as MiniNode[]).length === 0 && mini.document.body.querySelector('.chat-tmux__viewer') === null);
  await unmountPanel();
}

// ============================================================ //
// ---- source assertions (ChatApp wiring + component + styles) ----
// ============================================================ //
{
  const here = dirname(fileURLToPath(import.meta.url));
  const app = readFileSync(join(here, '../src/chat/ChatApp.tsx'), 'utf8');
  const panel = readFileSync(join(here, '../src/chat/TmuxPanel.tsx'), 'utf8');
  const styles = readFileSync(join(here, '../src/chat/styles.css'), 'utf8');
  const runners = readFileSync(join(here, '../src/chat/runners.ts'), 'utf8');

  check('ChatApp no longer mounts TmuxPanel in the project gear', !app.includes('<TmuxPanel'));
  check('ChatApp mounts TmuxPreviewBoard on a connected runner', app.includes('<TmuxPreviewBoard')
    && app.includes('focusedRunner.state === \'connected\''));
  {
    const helperStart = runners.indexOf('function isListenLocalTmuxCommand');
    const helperEnd = runners.indexOf('export class RunnerRegistry');
    const helper = helperStart >= 0 && helperEnd > helperStart ? runners.slice(helperStart, helperEnd) : '';
    const commandStart = runners.indexOf('private async commandOnRuntime');
    const subscribeStart = runners.indexOf('await this.ensureSubscription', commandStart);
    const commandBody = commandStart >= 0 && subscribeStart > commandStart
      ? runners.slice(commandStart, subscribeStart)
      : '';
    check('preview list RPCs are listen-local and skip chat subscription',
      helper.includes("case 'tmux_list_sessions'")
      && helper.includes("case 'tmux_pane_snapshot'")
      && helper.includes("case 'tmux_send_keys'")
      && helper.includes("case 'tmux_session_create'")
      && helper.includes("case 'tmux_session_delete'")
      && commandBody.includes('if (isListenLocalTmuxCommand(command))')
      && !/command\.type === 'tmux_terminal_attach'/.test(commandBody));
  }


  check('panel renders pane content through safeText', panel.includes('{safeText(snapshot.content)}'));
  check('panel renders session names through safeText', panel.includes('{safeText(row.name)}'));
  check('panel renders the key error through safeText', panel.includes('{safeText(keysError)}'));
  check('runnerLabel passes safeText at the DOM title boundary', panel.includes('title={`Tmux sessions on ${safeText(runnerLabel)}`}') && !panel.includes('title={`Tmux sessions on ${runnerLabel}`}'));
  check('selected session passes safeText at the data-session boundary', panel.includes('data-session={safeText(selected)}') && !panel.includes('data-session={selected}'));
  check('panel Send button gates on the empty input', panel.includes('disabled={text.trim() === \'\' || keysBusy}'));
  check('panel Ctrl-C button sends key ctrl_c', panel.includes("aria-label=\"Ctrl-C\"") && panel.includes("sendKey('ctrl_c')"));
  check('panel Enter button sends key enter', panel.includes("sendKey('enter')"));
  check('panel Esc button sends key escape', panel.includes("sendKey('escape')"));
  check('panel input Enter sends the literal text', panel.includes("event.key === 'Enter'"));
  check('panel sends every command with the source-session options',
    panel.includes("send({ type: 'tmux_list_sessions' }, commandOptions)")
    && panel.includes("send({ type: 'tmux_pane_snapshot', session, lines: TMUX_PANE_LINES }, commandOptions)")
    && panel.includes('await send(command, commandOptions)'));
  check('panel never fires without the probe sessionId',
    panel.includes('if (sessionId === null) return;')
    && panel.includes('{sessionId === null ? ('));
  check('mount effect skips the list fetch without a probe sessionId',
    panel.includes('if (sessionId === null) return;\n    void refreshList()'));
  check('panel validates every response through the protocol parsers',
    panel.includes('parseTmuxSessionList(response.data)')
    && panel.includes('parseTmuxPaneSnapshot(response.data)')
    && panel.includes('parseTmuxSendResult(response.data)'));
  const refreshStart = panel.indexOf('const refreshList = useCallback');
  const refreshEnd = panel.indexOf('}, [send, commandOptions]);');
  const refreshBody = refreshStart >= 0 && refreshEnd > refreshStart ? panel.slice(refreshStart, refreshEnd) : '';
  check('refreshList never setList(null)', refreshBody.includes('setListFailed(true)') && !refreshBody.includes('setList(null)'));
  check('list placeholder is only for the initial empty load', panel.includes('listLoading && list === null') && panel.includes('{sessionId === null ? ('));
  check('failed list without last-good stays unavailable', panel.includes('listFailed && list === null'));
  check('pane refetch gate replaces the stale frame with the placeholder', panel.includes('{snapshotLoading ? ('));
  check('pane Refresh disables only while a fetch is in flight, never after failure', panel.includes('disabled={snapshotLoading}') && !panel.includes('disabled={snapshotFailed}'));

  check('styles define the tmux panel root', /\.chat-tmux \{/.test(styles));
  check('tmux rows have hover styles', /\.chat-tmux__row:hover/.test(styles));
  check('tmux control buttons have hover styles',
    /\.chat-tmux__send:hover:not\(:disabled\)/.test(styles) && /\.chat-tmux__key:hover:not\(:disabled\)/.test(styles));
  check('tmux pane is a bounded monospace scroller', /\.chat-tmux__pre \{[^}]*max-height:[^}]*overflow:\s*auto/s.test(styles));
  check('tmux pane uses a monospace stack', /\.chat-tmux__pre \{[^}]*font-family:[^}]*monospace/s.test(styles));
  check('tmux styles stay square (no border-radius)', !/\.chat-tmux[^{]*\{[^}]*border-radius/.test(styles));
  check('tmux styles use theme tokens only', !/\.chat-tmux(?:\s|\{|,|:|__)[^{]*\{[^}]*#[0-9a-fA-F]{3,8}/.test(styles));
}

await unmountPanel();

if (failures.length > 0) {
  console.error(`chatTmuxPanel: ${ran} checks, ${failures.length} FAILURES`);
  for (const failure of failures) console.error(`  - ${failure}`);
  process.exit(1);
}
console.log(`chatTmuxPanel: ${ran} checks, 0 failures`);
process.exit(0);