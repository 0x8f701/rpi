#!/usr/bin/env node
// Tmux preview board regression for src/chat/TmuxPreviewBoard.tsx. Run through
// the esbuild+node pattern used by the other chat scripts. Exit codes:
// 0 = every assertion held; 1 = a regression. react-dom snapshots `canUseDOM`
// at module-init, so the mini-DOM globals MUST be installed before react-dom
// is evaluated — hence the dynamic imports below.
import type { act as Act, createElement as CreateElement } from 'react';
import type { createRoot as CreateRoot, Root } from 'react-dom/client';
import type * as TmuxPreviewBoardModule from '../src/chat/TmuxPreviewBoard.tsx';
import type * as TmuxPanelModule from '../src/chat/TmuxPanel.tsx';
import type * as ProtocolModule from '../src/chat/protocol.ts';
import type { RunnerCommandError as RunnerCommandErrorClass } from '../src/chat/runners.ts';
import type { RunnerCommand } from '../src/chat/protocol.ts';
import type { RpcResponse } from '../src/types.ts';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));


let createElement: CreateElement;
let act: Act;
let createRoot: CreateRoot;
let Board: TmuxPreviewBoardModule.TmuxPreviewBoard;
let protocol: typeof ProtocolModule;
let boardModule: typeof TmuxPreviewBoardModule;
let panelModule: typeof TmuxPanelModule;
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

function settleSnapshot(session: string, data: unknown): boolean {
  const deferred = deferreds.find(
    (entry) => entry.command.type === 'tmux_pane_snapshot'
      && (entry.command as { session?: string }).session === session
      && !entry.settled,
  );
  if (!deferred) return false;
  deferred.settled = true;
  deferred.resolve(okResponse('tmux_pane_snapshot', data));
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

function panePayload(session: string, content: string): Record<string, unknown> {
  return { session, pane: '0', width: 80, height: 24, content };
}

const failures: string[] = [];
let ran = 0;
function check(name: string, condition: boolean, detail = ''): void {
  ran += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

let container: MiniNode;
let root: Root | null = null;
let opened: string[] = [];

async function mountBoard(sessionId: string | null = 'sess-1'): Promise<void> {
  sent = [];
  deferreds = [];
  optionCalls = [];
  opened = [];
  container = mini.document.createElement('div');
  mini.document.body.appendChild(container);
  root = createRoot(container as unknown as Element);
  await act(async () => {
    root!.render(
      createElement(Board, {
        runnerId: 'r1',
        runnerLabel: 'Runner r1',
        sessionId,
        send: harnessSend,
        onOpenTerminal: (session: string) => {
          opened.push(session);
        },
      }),
    );
    await realTick();
  });
}

async function unmountBoard(): Promise<void> {
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

async function settleAndTick(type: string, data: unknown): Promise<void> {
  check(`${type} request settles`, settleNext(type, data));
  await act(async () => {
    await realTick();
  });
}

function cards(): MiniNode[] {
  return mini.document.body.querySelectorAll('.chat-tmux-preview__card') as MiniNode[];
}

function cells(): MiniNode[] {
  return mini.document.body.querySelectorAll('.chat-tmux-preview__cell') as MiniNode[];
}

function cardBySession(name: string): MiniNode | null {
  return cards().find((card) => card.getAttribute('data-session') === name) ?? null;
}

function cellBySession(name: string): MiniNode | null {
  return cells().find((cell) => cell.getAttribute('data-session') === name) ?? null;
}

function preOfCard(name: string): MiniNode | null {
  return cardBySession(name)?.querySelector('.chat-tmux-preview__pre') ?? null;
}

installGlobals();

const react = await import('react');
const reactDomClient = await import('react-dom/client');
const chatRunners = await import('../src/chat/runners.ts');
protocol = await import('../src/chat/protocol.ts');
boardModule = await import('../src/chat/TmuxPreviewBoard.tsx');
panelModule = await import('../src/chat/TmuxPanel.tsx');
RunnerCommandError = chatRunners.RunnerCommandError;
act = react.act as Act;
createElement = react.createElement as CreateElement;
createRoot = reactDomClient.createRoot as CreateRoot;
Board = boardModule.TmuxPreviewBoard;

// ============================================================ //
// ---- mounted board behavior (mini-DOM + React) ----
// ============================================================ //
{
  await mountBoard(null);
  check('null sessionId renders the fixed unavailable line',
    mini.document.body.textContent?.includes(panelModule.TMUX_STATUS_UNAVAILABLE) === true);
  check('null sessionId fires no request of any kind', sent.length === 0, JSON.stringify(sent));
  check('null sessionId renders no cards',
    (mini.document.body.querySelectorAll('.chat-tmux-preview__card') as MiniNode[]).length === 0);
  await unmountBoard();
}

// ---- mount: one list request, placeholder while loading ----
{
  await mountBoard();
  check('mount sends exactly one tmux_list_sessions', commandsOf('tmux_list_sessions').length === 1);
  const listCommand = commandsOf('tmux_list_sessions')[0];
  check('list command is the bare contract shape', JSON.stringify(listCommand) === '{"type":"tmux_list_sessions"}', JSON.stringify(listCommand));
  check('list request forwards the probe sessionId for the WS transport',
    optionsOf('tmux_list_sessions')[0]?.sourceSessionId === 'sess-1', JSON.stringify(optionsOf('tmux_list_sessions')));
  check('mount renders the loading placeholder, not cards',
    mini.document.body.querySelector('.chat-tmux-preview__status--placeholder') !== null
    && (mini.document.body.querySelectorAll('.chat-tmux-preview__card') as MiniNode[]).length === 0);
  check('loading placeholder is the literal -- marker',
    mini.document.body.querySelector('.chat-tmux-preview__status--placeholder')?.textContent === '--');
  check('mount sends no snapshot before a good list', commandsOf('tmux_pane_snapshot').length === 0);
}

// ---- good list: cards, counts, chip, parallel snapshots with lines 24 ----
{
  await settleAndTick('tmux_list_sessions', listPayload());
  const rendered = cards();
  check('good list renders both cards', rendered.length === 2, String(rendered.length));
  check('cards are sorted by localeCompare',
    rendered.map((card) => card.getAttribute('data-session')).join(',') === 'ci,dev',
    rendered.map((card) => card.getAttribute('data-session')).join(','));
  check('each card is a button with Open terminal aria-label',
    cardBySession('dev')?.tagName === 'BUTTON'
    && cardBySession('dev')?.getAttribute('aria-label') === 'Open terminal dev'
    && cardBySession('ci')?.tagName === 'BUTTON'
    && cardBySession('ci')?.getAttribute('aria-label') === 'Open terminal ci');
  check('first card shows the session name', cellBySession('dev')?.querySelector('.chat-tmux-preview__name')?.textContent === 'dev');
  check('card shows the window count', cellBySession('dev')?.querySelector('.chat-tmux-preview__meta')?.textContent === '2 windows');
  check('attached chip only on attached sessions',
    cellBySession('dev')?.querySelector('.chat-tmux-preview__chip') !== null
    && cellBySession('ci')?.querySelector('.chat-tmux-preview__chip') === null);
  const snapshotCommands = commandsOf('tmux_pane_snapshot');
  check('a good list fires one snapshot per session', snapshotCommands.length === 2, JSON.stringify(snapshotCommands));
  check('snapshot commands carry the frozen shape with lines 24',
    JSON.stringify(snapshotCommands[0]) === '{"type":"tmux_pane_snapshot","session":"dev","lines":24}'
    && JSON.stringify(snapshotCommands[1]) === '{"type":"tmux_pane_snapshot","session":"ci","lines":24}'
    && boardModule.TMUX_PREVIEW_LINES === 24,
    JSON.stringify(snapshotCommands));
  check('snapshot requests forward the probe sessionId for the WS transport',
    optionsOf('tmux_pane_snapshot').length === 2
    && optionsOf('tmux_pane_snapshot').every((options) => options?.sourceSessionId === 'sess-1'),
    JSON.stringify(optionsOf('tmux_pane_snapshot')));
  check('both snapshots are in flight in parallel (both cards still placeholders)',
    deferreds.filter((entry) => entry.command.type === 'tmux_pane_snapshot' && !entry.settled).length === 2);
  check('every card shows the -- placeholder while its snapshot loads',
    preOfCard('dev')?.textContent === '--' && preOfCard('ci')?.textContent === '--'
    && (mini.document.body.querySelectorAll('.chat-tmux-preview__pre--placeholder') as MiniNode[]).length === 2);

  await act(async () => {
    check('dev snapshot settles', settleSnapshot('dev', panePayload('dev', '$ ls\ndev ci')));
    await realTick();
  });
  check('dev card renders its pane content', preOfCard('dev')?.textContent === '$ ls\ndev ci');
  check('ci card keeps its placeholder while its snapshot is in flight', preOfCard('ci')?.textContent === '--');

  await act(async () => {
    check('ci snapshot settles', settleSnapshot('ci', panePayload('ci', '$ tail -f ci.log')));
    await realTick();
  });
  check('ci card renders its pane content', preOfCard('ci')?.textContent === '$ tail -f ci.log');
  check('no placeholders remain after both snapshots settle',
    (mini.document.body.querySelectorAll('.chat-tmux-preview__pre--placeholder') as MiniNode[]).length === 0);
}

// ---- Open terminal callback: the card itself is the button ----
{
  const devCard = cardBySession('dev');
  const ciCard = cardBySession('ci');
  check('Open terminal cards are the preview buttons',
    devCard !== null && ciCard !== null
    && devCard.getAttribute('aria-label') === 'Open terminal dev'
    && ciCard.getAttribute('aria-label') === 'Open terminal ci');
  await click(devCard!);
  check('Open terminal callback receives the card session', opened.length === 1 && opened[0] === 'dev', JSON.stringify(opened));
  await click(ciCard!);
  check('Open terminal callback receives the other session', opened.length === 2 && opened[1] === 'ci', JSON.stringify(opened));
}

// ---- safeText redaction on pane content and session names ----
{
  await unmountBoard();
  await mountBoard();
  await settleAndTick('tmux_list_sessions', listPayload());
  await act(async () => {
    check('redacted-content snapshot settles', settleSnapshot('dev', panePayload('dev', 'export GITHUB_TOKEN=ghp_aaaaaaaaaaaaaaaaaaaa')));
    await realTick();
  });
  check('pane content passes through safeText redaction',
    preOfCard('dev')?.textContent === 'export GITHUB_TOKEN=[REDACTED]', preOfCard('dev')?.textContent ?? 'missing');

  await unmountBoard();
  await mountBoard();
  const secretRow = listPayload();
  (secretRow.sessions as Array<Record<string, unknown>>)[0] = {
    name: 'ghp_aaaaaaaaaaaaaaaaaaaa',
    windows: 1,
    attached: false,
    createdEpoch: 1,
    paneCount: 1,
  };
  await settleAndTick('tmux_list_sessions', secretRow);
  const secretCard = cellBySession('[REDACTED]');
  check('remote session name passes through safeText redaction',
    secretCard?.querySelector('.chat-tmux-preview__name')?.textContent === '[REDACTED]', secretCard?.querySelector('.chat-tmux-preview__name')?.textContent ?? 'missing');
}

// ---- drift / failure with no last-good list: fixed neutral lines ----
{
  await unmountBoard();
  await mountBoard();
  await settleAndTick('tmux_list_sessions', { tmuxAvailable: true, sessions: [{ name: 'x' }] });
  check('drifted list payload shows the fixed unavailable line',
    mini.document.body.textContent?.includes(panelModule.TMUX_STATUS_UNAVAILABLE) === true);
  check('drifted list renders no cards', cards().length === 0);
  check('drifted list fires no snapshots', commandsOf('tmux_pane_snapshot').length === 0);

  await unmountBoard();
  await mountBoard();
  await act(async () => {
    settleNextFailure('tmux_list_sessions', new RunnerCommandError('socket gone', 'transport'));
    await realTick();
  });
  check('transport failure with no last-good shows the fixed unavailable line',
    mini.document.body.textContent?.includes(panelModule.TMUX_STATUS_UNAVAILABLE) === true);
  check('transport failure renders no cards', cards().length === 0);

  await unmountBoard();
  await mountBoard();
  await act(async () => {
    const deferred = deferreds.find((entry) => entry.command.type === 'tmux_list_sessions' && !entry.settled);
    deferred!.resolve({ type: 'response', command: 'tmux_list_sessions', success: false, data: null, sessionId: 'sess-1' });
    await realTick();
  });
  check('unsuccessful response with no last-good shows the fixed unavailable line',
    mini.document.body.textContent?.includes(panelModule.TMUX_STATUS_UNAVAILABLE) === true);
}

// ---- no server / no sessions ----
{
  await unmountBoard();
  await mountBoard();
  await settleAndTick('tmux_list_sessions', { tmuxAvailable: false, sessions: [] });
  check('unavailable tmux shows the fixed no-server line',
    mini.document.body.textContent?.includes(panelModule.TMUX_NO_SERVER) === true);
  check('no-server renders no cards', cards().length === 0);
  check('no-server fires no snapshots', commandsOf('tmux_pane_snapshot').length === 0);

  await unmountBoard();
  await mountBoard();
  await settleAndTick('tmux_list_sessions', { tmuxAvailable: true, sessions: [] });
  check('empty session list shows the fixed no-sessions line',
    mini.document.body.textContent?.includes(panelModule.TMUX_NO_SESSIONS) === true);
  check('empty list renders no cards', cards().length === 0);
}

// ---- per-card pane drift + transport failure ----
{
  await unmountBoard();
  await mountBoard();
  await settleAndTick('tmux_list_sessions', listPayload());
  await act(async () => {
    check('drifted dev snapshot settles', settleSnapshot('dev', { session: 'dev', pane: '0', width: 0, height: 24, content: 'x' }));
    await realTick();
  });
  check('drifted snapshot shows the fixed pane line on its card',
    cardBySession('dev')?.querySelector('.chat-tmux-preview__status')?.textContent === panelModule.TMUX_PANE_UNAVAILABLE
    && preOfCard('dev') === null);
  check('the other card is unaffected', preOfCard('ci')?.textContent === '--');

  await act(async () => {
    check('ci snapshot settles', settleNextFailure('tmux_pane_snapshot', new RunnerCommandError('gone', 'transport')));
    await realTick();
  });
  check('snapshot transport failure shows the fixed pane line on its card',
    cardBySession('ci')?.querySelector('.chat-tmux-preview__status')?.textContent === panelModule.TMUX_PANE_UNAVAILABLE);

  await unmountBoard();
  await mountBoard();
  await settleAndTick('tmux_list_sessions', listPayload());
  await act(async () => {
    const deferred = deferreds.find(
      (entry) => entry.command.type === 'tmux_pane_snapshot' && (entry.command as { session?: string }).session === 'dev' && !entry.settled,
    );
    deferred!.resolve({ type: 'response', command: 'tmux_pane_snapshot', success: false, data: null, sessionId: 'sess-1' });
    await realTick();
  });
  check('unsuccessful snapshot shows the fixed pane line on its card',
    cardBySession('dev')?.querySelector('.chat-tmux-preview__status')?.textContent === panelModule.TMUX_PANE_UNAVAILABLE);
}



// ---- create session lives in the runner header, not the preview board ----
{
  const board = readFileSync(join(here, '../src/chat/TmuxPreviewBoard.tsx'), 'utf8');
  const app = readFileSync(join(here, '../src/chat/ChatApp.tsx'), 'utf8');
  check('board no longer owns create-session chrome',
    !board.includes('chat-tmux-preview-create') && !board.includes('Create tmux session'));
  check('runner header owns Create tmux session',
    app.includes('id="chat-runner-create-session"')
    && app.includes('aria-label="Create tmux session"')
    && app.includes('id="chat-runner-create-session-form"')
    && app.includes("type: 'tmux_session_create'"));
  check('board still owns confirmed delete',
    board.includes('Confirm delete') && board.includes('tmux_session_delete'));
}

{
  await unmountBoard();
  await mountBoard();
  await settleAndTick('tmux_list_sessions', listPayload());


  const deleteBtn = mini.document.body.querySelector('[aria-label="Delete session dev"]');
  check('card delete button exists', deleteBtn !== null);
  await click(deleteBtn!);
  check('first delete click does not send', commandsOf('tmux_session_delete').length === 0);
  const confirmBtn = mini.document.body.querySelector('[aria-label="Confirm delete dev"]');
  check('confirm delete label appears', confirmBtn !== null);
  await click(confirmBtn!);
  check('delete sends tmux_session_delete',
    JSON.stringify(commandsOf('tmux_session_delete')[0]) === JSON.stringify({ type: 'tmux_session_delete', session: 'dev' }),
    JSON.stringify(commandsOf('tmux_session_delete')));
  check('delete request settles', settleNext('tmux_session_delete', {}));
  await act(async () => {
    await realTick();
  });
  check('delete re-lists sessions', commandsOf('tmux_list_sessions').length === 2);
  check('re-list after delete settles', settleNext('tmux_list_sessions', listPayload()));
  await act(async () => {
    await realTick();
  });
  check('re-list after delete re-fires snapshots', commandsOf('tmux_pane_snapshot').length === 4);

}

// ============================================================ //
// ---- source assertions (component + wiring-free contract) ----
// ============================================================ //
{
  const board = readFileSync(join(here, '../src/chat/TmuxPreviewBoard.tsx'), 'utf8');


  check('board exports TMUX_PREVIEW_LINES = 24', /export const TMUX_PREVIEW_LINES = 24;/.test(board));
  check('board validates responses through the protocol parsers',
    board.includes('parseTmuxSessionList(response.data)') && board.includes('parseTmuxPaneSnapshot(response.data)'));
  check('board reuses the frozen status copy from TmuxPanel',
    board.includes("TMUX_NO_SERVER,\n  TMUX_NO_SESSIONS,\n  TMUX_PANE_UNAVAILABLE,\n  TMUX_STATUS_UNAVAILABLE,\n  tmuxErrorText,\n} from './TmuxPanel'"));
  check('board sends the list with the source-session options',
    board.includes("send({ type: 'tmux_list_sessions' }, commandOptions)"));
  check('board sends every snapshot with the source-session options and the preview line count',
    board.includes("{ type: 'tmux_pane_snapshot', session: row.name, lines: TMUX_PREVIEW_LINES }, commandOptions"));
  check('board never fires without the probe sessionId',
    board.includes('if (sessionId === null) return;') && board.includes('sessionId === null || (listFailed && list === null)'));
  const refreshStart = board.indexOf('const refreshList = useCallback');
  const refreshEnd = board.indexOf('}, [send, commandOptions, fetchPanes]);');
  const refreshBody = refreshStart >= 0 && refreshEnd > refreshStart ? board.slice(refreshStart, refreshEnd) : '';
  check('refreshList never setList(null)', refreshBody.includes('setListFailed(true)') && !refreshBody.includes('setList(null)'));
  check('list placeholder is only for the initial empty load', board.includes('listLoading && list === null'));
  check('failed list without last-good stays unavailable', board.includes('listFailed && list === null'));
  check('board carries the frozen class contract',
    board.includes('chat-tmux-preview__card') && board.includes('chat-tmux-preview__pre') && board.includes('className="chat-tmux-preview"'));
  check('board renders runnerLabel through safeText at the title boundary',
    board.includes('title={`Tmux sessions on ${safeText(runnerLabel)}`}') && !board.includes('title={`Tmux sessions on ${runnerLabel}`'));
  check('board renders session names through safeText',
    board.includes('{safeText(row.name)}') && board.includes('data-session={safeText(row.name)}'));
  check('board renders pane content through AnsiText', board.includes('<AnsiText text={pane.content} />'));
  check('board card is the Open terminal control',
    board.includes('aria-label={`Open terminal ${row.name}`}')
    && board.includes('onClick={() => onOpenTerminal(row.name)}')
    && !board.includes('\u2922 Open terminal')
    && !board.includes('aria-label="Refresh tmux sessions"'));
  check('board sorts cards with localeCompare',
    board.includes('left.name.localeCompare(right.name)'));
  check('board has no send-keys controls',
    !board.includes('tmux_send_keys') && !board.includes('parseTmuxSendResult') && !board.includes('Send'));
  check('board fetches a snapshot per listed session in parallel', board.includes('for (const row of rows)'));
  check('board guards per-card results with the board epoch', board.includes('if (seq === boardSeq.current)'));
  check('board does not clear pane state on a drifted refresh',
    refreshBody.includes('setListFailed(true)') && !refreshBody.includes('setPanes({})'));
}

await unmountBoard();

if (failures.length > 0) {
  console.error(`chatTmuxPreview: ${ran} checks, ${failures.length} FAILURES`);
  for (const failure of failures) console.error(`  - ${failure}`);
  process.exit(1);
}
console.log(`chatTmuxPreview: ${ran} checks, 0 failures`);
process.exit(0);