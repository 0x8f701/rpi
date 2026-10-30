#!/usr/bin/env node
// Workflow detail single-flight + remove-tombstone regression for
// src/panels/WorkflowPanel.tsx + workflowPanelState.ts, over a real createRoot
// render on a mini DOM (same harness pattern as copyButton.test.ts /
// sessionPanelAutocomplete.test.ts) with a deterministic fake clock and a
// fully deferred sendCommand.
//
// P0 regressions covered (user-reported ground truth):
//   * workflow_detail single-flight per exact sessionId+workflowId+generation:
//     repeated poll ticks while a request is on the wire coalesce — 20 ticks
//     produce ONE detail request, max in-flight 1, no concurrency-limit error.
//   * Cancel admission with a detail request in flight: the in-flight request
//     is invalidated (late success is discarded silently), zero further detail
//     RPCs until the cancel settles; settlement refreshes the list once and
//     then issues exactly one detail refresh after the list authority accepts
//     it (workflow still exists + selected).
//   * Remove success is a terminal tombstone: zero detail RPCs after
//     admission/success, list-only refresh, late not-found detail failures
//     ignored, selection + detail cleared, removed UI. A workflow_removed
//     event tombstones the same way and never details the removed id.
//   * workflow_* RPC failures are panel-owned: the generic pending layer never
//     toasts them (rpcFailureToastOwned stays false).
//
// The modules under test are imported dynamically ON PURPOSE: react-dom must
// evaluate AFTER the mini-DOM globals are installed, or its canUseDOM snapshot
// freezes without a document (same justified pattern as
// sessionPanelAutocomplete.test.ts).
//
// Focused run:
//   esbuild scripts/workflowDetailFlight.test.ts --bundle --platform=node \
//     --format=esm --loader:.css=empty \
//     --outfile=.web-test-build/workflowDetailFlight.test.mjs \
//   && node .web-test-build/workflowDetailFlight.test.mjs
//
// Exit codes: 0 = every assertion held; 1 = a regression.

/* Type-only imports are erased, so they cannot init the modules. The value
 * bindings below are assigned by the top-level dynamic import that runs
 * after installGlobals(). */
import type { act as Act, createElement as CreateElement } from 'react';
import type { createRoot as CreateRoot, Root } from 'react-dom/client';
import type { WorkflowPanel as WorkflowPanelComponent } from '../src/panels/WorkflowPanel.tsx';
import type { WorkflowPanelAuthority as Authority } from '../src/panels/workflowPanelState.ts';
import type { rpcFailureToastOwned as RpcFailureToastOwned } from '../src/pending.ts';

let createElement: CreateElement;
let act: Act;
let createRoot: CreateRoot;
let WorkflowPanel: WorkflowPanelComponent;
let WorkflowPanelAuthority: new (sessionId: string) => Authority;
let rpcFailureToastOwned: RpcFailureToastOwned;

const failures: string[] = [];
let ran = 0;
function check(name: string, condition: unknown, detail = ''): void {
  ran += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

/* ------------------------------------------------------------------ *
 * Deterministic clock: setTimeout + setInterval routed into a queue
 * the test fires explicitly via advanceClock. act() itself only uses
 * microtasks, so nothing else observes the patched globals.
 * ------------------------------------------------------------------ */
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

/* ------------------------------------------------------------------ *
 * Mini DOM — the surface React 18 client rendering + this panel use
 * (same proven shape as copyButton.test.ts).
 * ------------------------------------------------------------------ */
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
  text: string;
  get className(): string;
  set className(value: string);
  get id(): string;
  set id(value: string);
  get textContent(): string;
  set textContent(value: string);
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
  setAttributeNS(_ns: string | null, name: string, value: string): void;
  getAttributeNS(_ns: string | null, name: string): string | null;
  addEventListener(type: string, fn: Listener, options?: boolean | { capture?: boolean }): void;
  removeEventListener(type: string, fn: Listener, options?: boolean | { capture?: boolean }): void;
  dispatchEvent(event: { type: string; target?: MiniNode }): boolean;
  click(): void;
  focus(): void;
  blur(): void;
  contains(node: MiniNode | null): boolean;
  querySelector(selector: string): MiniNode | null;
  querySelectorAll(selector: string): MiniNode[];
  cloneNode(deep?: boolean): MiniNode;
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
  /* Real documents expose on* IDL slots; React's isEventSupported('input')
   * probe at module init checks `'oninput' in document` — without it the
   * input event falls back to the IE polyfill and controlled inputs never
   * fire onChange (same as sessionPanelAutocomplete.test.ts). */
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
      // React's ChangeEventPlugin classifies a text input via
      // supportedInputTypes[node.type]; without it the 'input' event is
      // ignored and controlled inputs never fire onChange.
      type: nodeName === 'INPUT' ? 'text' : nodeType === 1 ? '' : undefined,
      value: '',
      listeners: new Map(),
      text: '',
      style: {},
      get options() {
        return node.childNodes.filter((child) => child.nodeName === 'OPTION');
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
      dispatchEvent(raw: { type: string; target?: MiniNode }) {
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
  // React's isEventSupported('input') probe checks `'oninput' in document` —
  // without the IDL slot the input event falls back to the IE polyfill and
  // controlled inputs never fire onChange (same as
  // sessionPanelAutocomplete.test.ts).
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
  (globalThis as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
}

/* ------------------------------------------------------------------ *
 * Deferred RPC surface: every sendCommand parks a deferred the test
 * resolves/rejects explicitly. Detail in-flight concurrency is counted
 * at dispatch and settlement so the max can be asserted.
 * ------------------------------------------------------------------ */
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
let detailInFlight = 0;
let detailInFlightMax = 0;

function harnessSendCommand(command: Record<string, unknown>): Promise<unknown> {
  const type = typeof command.type === 'string' ? command.type : 'unknown';
  sent.push({ type, command });
  const { promise, resolve, reject } = Promise.withResolvers<unknown>();
  const deferred: DeferredCommand = { type, command, resolve, reject, settled: false };
  deferreds.push(deferred);
  if (type === 'workflow_detail') {
    detailInFlight += 1;
    detailInFlightMax = Math.max(detailInFlightMax, detailInFlight);
    void promise.catch(() => {}).finally(() => {
      detailInFlight -= 1;
    });
  }
  return promise;
}

let eventHandler: ((frame: Record<string, unknown>) => void) | null = null;
function harnessSubscribeEvents(handler: (frame: Record<string, unknown>) => void): () => void {
  eventHandler = handler;
  return () => {
    eventHandler = null;
  };
}

function countType(type: string): number {
  return sent.filter((entry) => entry.type === type).length;
}

function settleNext(type: string, data: unknown): boolean {
  const deferred = deferreds.find((entry) => entry.type === type && !entry.settled);
  if (!deferred) return false;
  deferred.settled = true;
  deferred.resolve(data);
  return true;
}

function failNext(type: string, message: string): boolean {
  const deferred = deferreds.find((entry) => entry.type === type && !entry.settled);
  if (!deferred) return false;
  deferred.settled = true;
  deferred.reject(new Error(message));
  return true;
}

function containerText(): string {
  return mini.document.body.textContent;
}

function errorRowCount(): number {
  return mini.document.body.querySelectorAll('.workflow-panel__error').length;
}

function fireWorkflowEvent(frame: Record<string, unknown>): void {
  if (!eventHandler) throw new Error('no event handler installed');
  eventHandler(frame);
}

const wfListSnapshot = (status: string, generation: number) => ({
  workflowId: 'wf-1',
  name: 'Storm workflow',
  objective: 'prove single-flight',
  status,
  generation,
});

const wfDetailSnapshot = (status: string, generation: number, supervisor?: string) => ({
  id: 'wf-1',
  generation,
  name: 'Storm workflow',
  objective: 'prove single-flight',
  status,
  ...(supervisor === undefined ? {} : { supervisor: { name: supervisor, status: 'idle' } }),
});

const POLL_TICK_MS = 2000; // one interval fire per advance (1500 < 2000 < 3000)

interface Panel {
  root: Root;
  container: MiniNode;
  cleanup: () => Promise<void>;
}

const mountedPanels: Panel[] = [];

async function bootPanel(): Promise<void> {
  sent = [];
  deferreds = [];
  detailInFlight = 0;
  detailInFlightMax = 0;
  const container = mini.document.createElement('div');
  mini.document.body.appendChild(container);
  const root = createRoot(container as unknown as Element);
  await act(async () => {
    root.render(createElement(WorkflowPanel, {
      sessionId: 'sess-wf',
      sendCommand: harnessSendCommand,
      subscribeEvents: harnessSubscribeEvents,
      onClose: () => {},
    }));
    await realTick();
  });
  check('boot sends exactly one workflow_list', countType('workflow_list') === 1, String(countType('workflow_list')));
  check('boot sends no workflow_detail yet', countType('workflow_detail') === 0);
  check('boot list request settles', settleNext('workflow_list', { workflows: [wfListSnapshot('running', 1)] }));
  await act(async () => {
    await realTick();
  });
  check('selection effect issues exactly one detail request', countType('workflow_detail') === 1, String(countType('workflow_detail')));
  const panel: Panel = {
    root,
    container,
    cleanup: async () => {
      await act(async () => {
        root.unmount();
      });
      container.remove();
    },
  };
  mountedPanels.push(panel);
}

async function advanceTicks(count: number): Promise<void> {
  for (let tick = 0; tick < count; tick += 1) {
    await act(async () => {
      advanceClock(POLL_TICK_MS);
      await realTick();
    });
  }
}

async function clickButton(buttonId: string): Promise<MiniNode> {
  const button = mini.document.body.querySelector(`#${buttonId}`);
  if (!button) throw new Error(`missing #${buttonId}`);
  await act(async () => {
    (button as MiniNode).click();
    await realTick();
  });
  return button;
}

async function setInputValue(selector: string, next: string): Promise<void> {
  const input = mini.document.body.querySelector(selector);
  if (!input) throw new Error(`missing ${selector}`);
  await act(async () => {
    (input as MiniNode).value = next;
    (input as MiniNode).dispatchEvent({ type: 'input', target: input, preventDefault() {}, stopPropagation() {} });
  });
}

/* ------------------------------------------------------------------ *
 * Run (dynamic imports land here, after installGlobals).
 * ------------------------------------------------------------------ */
installGlobals();

const react = await import('react');
const reactDomClient = await import('react-dom/client');
act = react.act as Act;
createElement = react.createElement as CreateElement;
createRoot = reactDomClient.createRoot as CreateRoot;
const panelModule = await import('../src/panels/WorkflowPanel.tsx');
WorkflowPanel = panelModule.WorkflowPanel as WorkflowPanelComponent;
const stateModule = await import('../src/panels/workflowPanelState.ts');
WorkflowPanelAuthority = stateModule.WorkflowPanelAuthority as new (sessionId: string) => Authority;
const pendingModule = await import('../src/pending.ts');
rpcFailureToastOwned = pendingModule.rpcFailureToastOwned as RpcFailureToastOwned;

/* ---------------- authority unit: physical slot + desired + tombstone + gate --- */
{
  const authority = new WorkflowPanelAuthority('sess-wf');

  // Physical single-flight: an identical live request coalesces.
  const p1 = authority.beginDetail({ sessionId: 'sess-wf', workflowId: 'wf-9', generation: 3 });
  check('authority admits the first detail request', p1 !== null);
  check('authority coalesces an identical in-flight request', authority.beginDetail({ sessionId: 'sess-wf', workflowId: 'wf-9', generation: 3 }) === null);

  // A different identity records the desired and waits for the physical.
  check('a different identity records the desired, not a send', authority.beginDetail({ sessionId: 'sess-wf', workflowId: 'wf-10', generation: 1 }) === null);
  const launch = p1 !== null ? authority.settleDetail(p1) : null;
  check('physical settle launches the latest desired identity', launch !== null && launch.workflowId === 'wf-10' && launch.generation === 1);
  const p2 = authority.beginDetail({ sessionId: 'sess-wf', workflowId: 'wf-10', generation: 1 });
  check('the launched identity admits a fresh physical', p2 !== null);

  // Invalidation never frees the physical slot; it only stales acceptance.
  authority.cancelDetail();
  check('invalidation stales the open physical', p2 !== null && !authority.isCurrentDetail(p2));
  const reDesired = authority.beginDetail({ sessionId: 'sess-wf', workflowId: 'wf-10', generation: 1 });
  check('same-identity desire against a stale physical records a launch', reDesired === null);
  const launch2 = p2 !== null ? authority.settleDetail(p2) : null;
  check('physical settle after invalidation launches the re-desired identity', launch2 !== null && launch2.generation === 1);

  authority.tombstoneWorkflow('wf-10');
  check('tombstoned identity is refused at beginDetail', authority.beginDetail({ sessionId: 'sess-wf', workflowId: 'wf-10', generation: 1 }) === null);
  const listAfterTombstone = authority.acceptList(authority.beginList(), [], { workflows: [{ workflowId: 'wf-10', name: 'zombie', objective: 'o', status: 'running', generation: 9 }] });
  check('tombstone rejects list generations', listAfterTombstone !== null && listAfterTombstone.length === 0);
  check('tombstone rejects events', authority.applyEvent({
    sessionId: 'sess-wf',
    type: 'workflow_status_changed',
    workflowId: 'wf-10',
    generation: 9,
    status: 'running',
  }, []) === null);

  const preAction = authority.beginDetail({ sessionId: 'sess-wf', workflowId: 'wf-11', generation: 1 });
  check('a detail is admitted before the action', preAction !== null);
  authority.beginAction('wf-11');
  check('beginAction invalidates the detail admitted before it', preAction !== null && !authority.isCurrentDetail(preAction));
  check('beginDetail is refused while the action is busy', authority.beginDetail({ sessionId: 'sess-wf', workflowId: 'wf-11', generation: 1 }) === null);
  check('isActionBusy gates the poll', authority.isActionBusy('wf-11') === true);
  authority.endAction('wf-11');
  check('endAction releases the action gate', authority.isActionBusy('wf-11') === false);
  // endAction does NOT free the held physical: the fresh desire records a
  // launch that fires when the held promise settles.
  check('post-action desire records a launch against the held physical', authority.beginDetail({ sessionId: 'sess-wf', workflowId: 'wf-11', generation: 1 }) === null);
  const launch3 = preAction !== null ? authority.settleDetail(preAction) : null;
  check('held physical settle launches the post-action desire', launch3 !== null && launch3.workflowId === 'wf-11');
}

/* ---------------- 20 poll ticks: max in-flight 1, single detail request ------- */
{
  sent = [];
  deferreds = [];
  detailInFlight = 0;
  detailInFlightMax = 0;
  await bootPanel();

  // Leave the first detail request unanswered on the wire; every poll tick
  // must coalesce into it instead of spawning a duplicate.
  await advanceTicks(20);
  check('20 poll ticks with an unanswered detail produce ONE detail request', countType('workflow_detail') === 1, String(countType('workflow_detail')));
  check('max concurrent workflow_detail in flight is 1', detailInFlightMax === 1, String(detailInFlightMax));
  check('no concurrency-limit error row rendered', errorRowCount() === 0);
  check('panel text never mentions the concurrency limit', !containerText().includes('concurrent'));

  // Different generation while the physical request is open: the poll/effect
  // records ONE latest desired identity — no second physical request.
  await act(async () => {
    fireWorkflowEvent({
      sessionId: 'sess-wf',
      type: 'workflow_status_changed',
      workflowId: 'wf-1',
      generation: 2,
      status: 'paused',
      sequence: 31,
    });
    await realTick();
  });
  await advanceTicks(2);
  check('different-generation desire sends ZERO extra physical details', countType('workflow_detail') === 1, String(countType('workflow_detail')));
  check('physical still capped at 1 across a generation change', detailInFlightMax === 1, String(detailInFlightMax));

  // Settling the open physical launches exactly one desired follow-up.
  check('held detail settles', settleNext('workflow_detail', wfDetailSnapshot('running', 1, 'sup-a')));
  await act(async () => {
    await realTick();
  });
  check('desired launch fires exactly once on physical settle', countType('workflow_detail') === 2, String(countType('workflow_detail')));
  check('max physical in flight stays 1 after the launch', detailInFlightMax === 1, String(detailInFlightMax));
  await mountedPanels[0].cleanup();
}

/* ------- Cancel while detail in flight: invalidate, zero detail, settle once -- */
{
  sent = [];
  deferreds = [];
  await bootPanel();
  const detailBefore = countType('workflow_detail');

  const cancelBtn = mini.document.body.querySelector('#workflow-cancel-btn');
  check('Cancel button rendered', cancelBtn !== null);
  await act(async () => {
    (cancelBtn as MiniNode).click();
    await realTick();
  });
  check('Cancel admits exactly one workflow_cancel', countType('workflow_cancel') === 1, String(countType('workflow_cancel')));

  // During the action: poll ticks and effects must send ZERO detail.
  await advanceTicks(5);
  check('zero further detail RPCs while cancel is in flight', countType('workflow_detail') === detailBefore, String(countType('workflow_detail')));


  // Settlement: resolve the cancel → the list refreshes once…
  check('cancel settles', settleNext('workflow_cancel', {}));
  await act(async () => {
    await realTick();
  });
  check('cancel settlement sends exactly one workflow_list', countType('workflow_list') === 2, String(countType('workflow_list')));

  // RACE WINDOW: the action response landed but the list is still unsettled.
  // The action gate must hold until the authoritative list settles — poll
  // ticks in this window send ZERO detail.
  await advanceTicks(3);
  check('zero detail between cancel response and list settlement', countType('workflow_detail') === detailBefore, String(countType('workflow_detail')));

  // The pre-cancel physical stays UNRESOLVED through the authoritative list:
  // the post-list desire is recorded against it and the total stays put.
  check('cancel list settles with the workflow still present', settleNext('workflow_list', { workflows: [wfListSnapshot('cancelled', 2)] }));
  await act(async () => {
    await realTick();
  });
  check('total detail stays at the held physical after list acceptance', countType('workflow_detail') === detailBefore, String(countType('workflow_detail')));

  // Settle the stale physical with a late SUCCESS: the response is discarded
  // (the cancel bumped the acceptance epoch) and the desired launch fires
  // EXACTLY once — one new current-generation detail.
  const staleDetail = deferreds.find((entry) => entry.type === 'workflow_detail' && !entry.settled);
  check('the held physical deferred exists', staleDetail !== undefined);
  if (staleDetail) staleDetail.resolve(wfDetailSnapshot('running', 1, 'STALE-SUPERVISOR'));
  await act(async () => {
    await realTick();
  });
  check('STALE-SUPERVISOR from the invalidated request never rendered', !containerText().includes('STALE-SUPERVISOR'));
  check('exactly one new current-generation detail after the held settle', countType('workflow_detail') === detailBefore + 1, String(countType('workflow_detail')));
  check('no error row after cancel settlement', errorRowCount() === 0, containerText().slice(0, 200));
  await mountedPanels[1].cleanup();
}

/* ---------------- Remove success: terminal tombstone, list-only refresh ------- */
{
  sent = [];
  deferreds = [];
  await bootPanel();
  const detailBefore = countType('workflow_detail');
  // The boot detail request stays UNSETTLED through the whole remove flow:
  // it is the in-flight request the tombstone must invalidate. (The Remove
  // button is not gated by an open detail request.)
  check('pre-remove detail deferred is in flight', countType('workflow_detail') === 1);
  await act(async () => {
    await realTick();
  });

  const removeBtn = mini.document.body.querySelector('#workflow-remove-btn');
  check('Remove button rendered', removeBtn !== null);
  await act(async () => {
    (removeBtn as MiniNode).click();
    await realTick();
  });
  check('Remove admits exactly one workflow_remove', countType('workflow_remove') === 1, String(countType('workflow_remove')));

  // During the action: zero detail.
  await advanceTicks(3);
  check('zero detail RPCs during remove action', countType('workflow_detail') === detailBefore, String(countType('workflow_detail')));

  check('remove settles', settleNext('workflow_remove', {}));
  await act(async () => {
    await realTick();
  });
  check('remove settlement refreshes the list', countType('workflow_list') === 2, String(countType('workflow_list')));
  check('remove settlement sends ZERO detail RPCs', countType('workflow_detail') === detailBefore, String(countType('workflow_detail')));
  await advanceTicks(3);
  check('poll ticks after remove still send ZERO detail', countType('workflow_detail') === detailBefore, String(countType('workflow_detail')));

  // The list authority accepts a world without wf-1: selection + detail clear.
  check('post-remove list settles without the workflow', settleNext('workflow_list', { workflows: [] }));
  await act(async () => {
    await realTick();
  });
  check('removed UI shows the empty-detail pane', containerText().includes('Select a workflow to inspect it.'));
  check('no error row after removal', errorRowCount() === 0, containerText().slice(0, 200));

  // A workflow_removed event for the tombstoned id is a no-op…
  await act(async () => {
    fireWorkflowEvent({
      sessionId: 'sess-wf',
      type: 'workflow_removed',
      workflowId: 'wf-1',
      generation: 1,
      sequence: 1,
    });
    await realTick();
  });
  // …and the racing pre-remove detail request failing with not-found is
  // silent: the rejection lands on the UNRESOLVED pre-remove deferred
  // (asserted), and the tombstone + request epoch discard it.
  let lateRejected = false;
  await act(async () => {
    lateRejected = failNext('workflow_detail', 'command workflow_detail failed: workflow was not found');
    await realTick();
  });
  check('late not-found rejection landed on the unresolved pre-remove deferred', lateRejected === true);
  check('late not-found detail failure after removal renders no error', errorRowCount() === 0, containerText().slice(0, 200));
  await mountedPanels[2].cleanup();
}

/* ---------------- workflow_removed event while selected: list-only refresh ---- */
{
  sent = [];
  deferreds = [];
  await bootPanel();
  const detailBefore = countType('workflow_detail');
  // The boot detail request stays UNSETTLED through the removal event: it is
  // the in-flight request the event tombstone must invalidate.
  check('pre-event detail deferred is in flight', countType('workflow_detail') === 1);

  await act(async () => {
    fireWorkflowEvent({
      sessionId: 'sess-wf',
      type: 'workflow_removed',
      workflowId: 'wf-1',
      generation: 1,
      sequence: 7,
    });
    await realTick();
  });
  check('removed event refreshes the list', countType('workflow_list') === 2, String(countType('workflow_list')));
  check('removed event sends ZERO detail for the removed id', countType('workflow_detail') === detailBefore, String(countType('workflow_detail')));
  await advanceTicks(3);
  check('poll never details the removed id', countType('workflow_detail') === detailBefore, String(countType('workflow_detail')));
  check('removed event leaves no panel error', errorRowCount() === 0);

  // Late not-found from the pre-event detail request is silent: the
  // rejection lands on the UNRESOLVED pre-event deferred (asserted) and the
  // event tombstone + request epoch discard it.
  let lateRejected = false;
  await act(async () => {
    lateRejected = failNext('workflow_detail', 'command workflow_detail failed: workflow was not found');
    await realTick();
  });
  check('late not-found rejection landed on the unresolved pre-event deferred', lateRejected === true);
  check('late not-found after removed event renders no error', errorRowCount() === 0, containerText().slice(0, 200));
  await mountedPanels[3].cleanup();
}

/* ------- negative: an unrelated not-found ACTION error stays visible ------- */
{
  sent = [];
  deferreds = [];
  await bootPanel();

  await act(async () => {
    (mini.document.body.querySelector('#workflow-cancel-btn') as MiniNode).click();
    await realTick();
  });
  let rejected = false;
  await act(async () => {
    rejected = failNext('workflow_cancel', 'command workflow_cancel failed: workflow was not found');
    await realTick();
  });
  check('the unrelated not-found action rejection landed on the cancel deferred', rejected === true);
  // The message text must NEVER suppress a current action rejection — the
  // tombstone/epoch identity decides, not a regex.
  check('unrelated not-found action error renders a bounded panel error row', errorRowCount() === 1 && containerText().includes('workflow was not found'), containerText().slice(0, 200));
  // The list still refreshes once, and the still-live selected workflow gets
  // its single post-list detail; a successful detail clears the stale error.
  // The list still refreshes once; with the boot detail physically held, the
  // post-list desire WAITS — total detail stays 1 until the held request
  // settles, then exactly one launch fires; its success clears the error.
  check('failed action still refreshes the list once', countType('workflow_list') === 2, String(countType('workflow_list')));
  check('failed-action list settles with the workflow live', settleNext('workflow_list', { workflows: [wfListSnapshot('running', 1)] }));
  await act(async () => {
    await realTick();
  });
  check('post-list desire holds total detail at the held request', countType('workflow_detail') === 1, String(countType('workflow_detail')));
  check('held boot detail settles', settleNext('workflow_detail', wfDetailSnapshot('running', 1, 'sup-n')));
  await act(async () => {
    await realTick();
  });
  check('post-list detail refresh fires after the held request settles', countType('workflow_detail') === 2, String(countType('workflow_detail')));
  check('successful detail clears the stale action error', errorRowCount() === 0, containerText().slice(0, 200));
  await mountedPanels[4].cleanup();
}

/* ---- removed event absolute tombstone: higher generations cannot resurrect ---- */
{
  sent = [];
  deferreds = [];
  await bootPanel();
  await act(async () => {
    fireWorkflowEvent({
      sessionId: 'sess-wf',
      type: 'workflow_removed',
      workflowId: 'wf-1',
      generation: 1,
      sequence: 11,
    });
    await realTick();
  });
  // A late, HIGHER-generation event must not resurrect the removed workflow…
  await act(async () => {
    fireWorkflowEvent({
      sessionId: 'sess-wf',
      type: 'workflow_updated',
      workflowId: 'wf-1',
      generation: 9,
      sequence: 12,
      snapshot: { workflowId: 'wf-1', name: 'zombie', objective: 'resurrect', status: 'running', generation: 9 },
    });
    await realTick();
  });
  await act(async () => {
    fireWorkflowEvent({
      sessionId: 'sess-wf',
      type: 'workflow_status_changed',
      workflowId: 'wf-1',
      generation: 10,
      status: 'running',
      sequence: 13,
    });
    await realTick();
  });
  check('higher-generation events never resurrect a removed workflow', mini.document.body.querySelector('#workflow-row-wf-1') === null);
  // …and a higher-generation list snapshot cannot either.
  check('post-remove list settles without the workflow', settleNext('workflow_list', { workflows: [{ workflowId: 'wf-1', name: 'zombie', objective: 'resurrect', status: 'running', generation: 99 }] }));
  await act(async () => {
    await realTick();
  });
  check('higher-generation list snapshot never resurrects a removed workflow', mini.document.body.querySelector('#workflow-row-wf-1') === null);
  check('resurrection attempts send ZERO detail', countType('workflow_detail') === 1, String(countType('workflow_detail')));
  await mountedPanels[5].cleanup();
}

/* ---- stale old-generation action rejection: suppressed by exact identity ---- */
{
  sent = [];
  deferreds = [];
  await bootPanel();

  await act(async () => {
    (mini.document.body.querySelector('#workflow-cancel-btn') as MiniNode).click();
    await realTick();
  });
  // While the gen-1 cancel is in flight, the selected workflow's generation
  // advances (a status event is the normal cancel flow)…
  await act(async () => {
    fireWorkflowEvent({
      sessionId: 'sess-wf',
      type: 'workflow_status_changed',
      workflowId: 'wf-1',
      generation: 2,
      status: 'paused',
      sequence: 21,
    });
    await realTick();
  });
  // …then the OLD-generation action rejects with a transport-style plain
  // Error: the exact sessionId+workflowId+generation comparison suppresses it.
  let rejected = false;
  await act(async () => {
    rejected = failNext('workflow_cancel', 'send failed: not connected');
    await realTick();
  });
  check('the old-generation rejection landed on the cancel deferred', rejected === true);
  check('stale old-generation action rejection renders ZERO panel errors', errorRowCount() === 0, containerText().slice(0, 200));
  // The list still refreshes once through the gate…
  check('stale-action list still refreshes once', countType('workflow_list') === 2, String(countType('workflow_list')));
  check('stale-action list settles', settleNext('workflow_list', { workflows: [wfListSnapshot('paused', 2)] }));
  await act(async () => {
    await realTick();
  });
  // …and the CURRENT generation gets its single post-list detail refresh —
  // but only once the physically held boot request settles: total stays 1
  // until then.
  check('current-gen desire holds total detail at the held request', countType('workflow_detail') === 1, String(countType('workflow_detail')));
  check('stale-action held detail settles', settleNext('workflow_detail', wfDetailSnapshot('running', 1, 'sup-s')));
  await act(async () => {
    await realTick();
  });
  check('current-generation post-list detail fires after stale rejection', countType('workflow_detail') === 2, String(countType('workflow_detail')));
  await mountedPanels[6].cleanup();
}

/* ---- physical/logical split: hold boot detail through cancel + list settle ---- */
{
  sent = [];
  deferreds = [];
  await bootPanel();
  const detailBefore = countType('workflow_detail');

  await act(async () => {
    (mini.document.body.querySelector('#workflow-cancel-btn') as MiniNode).click();
    await realTick();
  });
  check('cancel admits exactly one workflow_cancel', countType('workflow_cancel') === 1, String(countType('workflow_cancel')));
  // Hold the boot detail UNRESOLVED through the cancel response…
  check('cancel settles', settleNext('workflow_cancel', {}));
  await act(async () => {
    await realTick();
  });
  await advanceTicks(3);
  // …and through the list settlement: the post-list desire is recorded but
  // the PHYSICAL slot stays held — total detail requests stay at 1.
  check('cancel list settles', settleNext('workflow_list', { workflows: [wfListSnapshot('cancelled', 2)] }));
  await act(async () => {
    await realTick();
  });
  check('total detail stays 1 with the physical held through list settle', countType('workflow_detail') === detailBefore, String(countType('workflow_detail')));
  check('no error row while the desire waits', errorRowCount() === 0, containerText().slice(0, 200));

  // Reject the OLD physical with a real late not-found failure: silent (the
  // cancel bumped the acceptance epoch), and the desired launch fires
  // EXACTLY once — one new current-generation detail.
  let rejected = false;
  await act(async () => {
    rejected = failNext('workflow_detail', 'command workflow_detail failed: workflow was not found');
    await realTick();
  });
  check('the not-found rejection landed on the held physical deferred', rejected === true);
  check('late not-found on the held physical renders no error', errorRowCount() === 0, containerText().slice(0, 200));
  check('exactly one new current-generation detail after the physical settles', countType('workflow_detail') === detailBefore + 1, String(countType('workflow_detail')));
  check('held physical settle launched the desired only once', detailInFlightMax === 1, String(detailInFlightMax));
  await mountedPanels[7].cleanup();
}

/* ---- create: every current rejection type surfaces bounded, exactly once ---- */
{
  sent = [];
  deferreds = [];
  await bootPanel();
  await setInputValue('#workflow-create-name', 'neg-create');
  await setInputValue('#workflow-create-objective', 'neg-objective');
  await act(async () => {
    (mini.document.body.querySelector('#workflow-create-btn') as MiniNode).click();
    await realTick();
  });
  check('create admits exactly one workflow_create', countType('workflow_create') === 1, String(countType('workflow_create')));
  // A timeout-style rejection is NOT an rpc error — the create catch must
  // still surface it exactly once, bounded, with zero global toasts.
  let rejected = false;
  await act(async () => {
    rejected = failNext('workflow_create', 'workflow_create timed out after 30s (no response from server)');
    await realTick();
  });
  check('the plain create rejection landed on the deferred', rejected === true);
  check('plain create rejection renders a bounded error row', errorRowCount() === 1 && containerText().includes('timed out after 30s'), containerText().slice(0, 200));
  check('create failures stay panel-owned', rpcFailureToastOwned({ type: 'workflow_create', boot: false }) === false);
  await mountedPanels[8].cleanup();
}

/* ---------------- toast ownership stays panel-side ---------------- */
{
  check('workflow_detail failure never toasts from the generic layer', rpcFailureToastOwned({ type: 'workflow_detail', boot: false }) === false);
  check('workflow_cancel failure never toasts from the generic layer', rpcFailureToastOwned({ type: 'workflow_cancel', boot: false }) === false);
  check('workflow_remove failure never toasts from the generic layer', rpcFailureToastOwned({ type: 'workflow_remove', boot: false }) === false);
}

// React's async act() leaves a MessageChannel port pair open in node (the
// settingsPanel.test.ts note): print the verdict, then force a deterministic
// exit so the build chain never stalls on the open handle.
console.log(`workflowDetailFlight: ${ran} checks, ${failures.length} failures`);
for (const failure of failures) console.error(`  FAIL ${failure}`);
process.exit(failures.length > 0 ? 1 : 0);