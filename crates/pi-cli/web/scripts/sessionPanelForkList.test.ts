#!/usr/bin/env node
// SessionPanel fork-candidate listbox — rendered behavioral regression.
//
// The fork candidate surface is an accessible bounded scroll listbox
// (role="listbox") that replaces the old native select + Load older button:
// near-bottom scrolling pages older candidates in, paging is single-flight
// with exact stale guards, pages merge with dedupe while the selection stays
// sticky, paging stops terminally when the wire cursor is exhausted, a
// failed page load latches an explicit Retry while retaining the loaded
// list, and a window too short to scroll pulls exactly one extra page per
// fresh page state (short-list prefetch).
//
// Rendered over a mini DOM (same pattern as sessionPanelAutocomplete.test.ts
// / codeReviewPanel.test.ts): nodes carry plain
// scrollTop/scrollHeight/clientHeight fields and React's delegated root
// listeners receive dispatched scroll/keydown/click events.
//
// Run via `npm run build` (esbuild + node). Focused:
//   esbuild scripts/sessionPanelForkList.test.ts --bundle --platform=node \
//     --format=esm --outfile=.web-test-build/sessionPanelForkList.test.mjs \
//   && node .web-test-build/sessionPanelForkList.test.mjs
//
// Exit codes: 0 = every assertion held; 1 = a regression.
/* Type-only imports are erased, so they cannot init the modules. The value
 * bindings below are assigned by the top-level dynamic import that runs
 * after installGlobals(): static value imports would hoist and freeze
 * react-dom's canUseDOM snapshot with globals absent. This is a deliberate
 * module-loading-boundary exercise, shared by every rendered-panel test in
 * scripts/. */
import type { act as Act, createElement as CreateElement } from 'react';
import type { createRoot as CreateRoot, Root } from 'react-dom/client';
import type { SessionPanel as SessionPanelComponent } from '../src/panels/SessionPanel.tsx';
import type { ForkMessageWire } from '../src/panels/SessionPanel';
let createElement: CreateElement;
let act: Act;
let createRoot: CreateRoot;
let SessionPanel: SessionPanelComponent;

const failures: string[] = [];
let ran = 0;
function check(name: string, condition: boolean, detail?: string): void {
  ran += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

const realSetTimeout = globalThis.setTimeout.bind(globalThis);
const realTick = (): Promise<void> => {
  const { promise, resolve } = Promise.withResolvers<void>();
  realSetTimeout(resolve, 0);
  return promise;
};

/* ------------------------------------------------------------------ *
 * Mini DOM — the surface React 18 client rendering + this panel use.
 * Plain scroll-geometry fields (scrollTop/scrollHeight/clientHeight) back
 * the near-bottom and short-list decisions, exactly like
 * codeReviewPanel.test.ts. Prototype-less nodes keep React's value tracker
 * neutral and delegated dispatch reaches the container root listeners.
 * ------------------------------------------------------------------ */
type Listener = (event: Record<string, unknown>) => void;

interface MiniDoc {
  nodeType: number;
  nodeName: string;
  body: MiniNode;
  activeElement: MiniNode | null;
  createElement(tag: string): MiniNode;
  createTextNode(text: string): MiniNode;
  createComment(text: string): MiniNode;
  createDocumentFragment(): MiniNode;
  getElementById(id: string): MiniNode | null;
  querySelector(selector: string): MiniNode | null;
  oninput: null;
}

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
  text: string;
  style: Record<string, unknown>;
  value: string;
  scrollTop: number;
  scrollHeight: number;
  clientHeight: number;
  get className(): string;
  set className(value: string);
  get textContent(): string;
  set textContent(value: string);
  get nodeValue(): string | null;
  set nodeValue(value: string | null);
  appendChild(child: MiniNode): MiniNode;
  insertBefore(child: MiniNode, before: MiniNode | null): MiniNode;
  removeChild(child: MiniNode): MiniNode;
  addEventListener(type: string, fn: Listener, options?: boolean | { capture?: boolean }): void;
  removeEventListener(type: string, fn: Listener, options?: boolean | { capture?: boolean }): void;
  setAttribute(name: string, value: string): void;
  getAttribute(name: string): string | null;
  removeAttribute(name: string): void;
  hasAttribute(name: string): boolean;
  querySelector(selector: string): MiniNode | null;
  querySelectorAll(selector: string): MiniNode[];
  dispatchEvent(raw: Record<string, unknown>): boolean;
  click(): void;
  focus(): void;
}

function relink(parent: MiniNode): void {
  parent.firstChild = parent.childNodes[0] ?? null;
  parent.lastChild = parent.childNodes[parent.childNodes.length - 1] ?? null;
  for (let i = 0; i < parent.childNodes.length; i += 1) {
    const child = parent.childNodes[i];
    child.parentNode = parent;
    child.previousSibling = parent.childNodes[i - 1] ?? null;
    child.nextSibling = parent.childNodes[i + 1] ?? null;
  }
}

function matches(node: MiniNode, selector: string): boolean {
  if (node.nodeType !== 1) return false;
  if (selector.startsWith('.')) {
    const value = node.getAttribute('class') ?? '';
    return value.split(/\s+/).includes(selector.slice(1));
  }
  if (selector.startsWith('#')) return (node.getAttribute('id') ?? '') === selector.slice(1);
  if (selector.startsWith('[') && selector.endsWith(']')) {
    const body = selector.slice(1, -1);
    const eq = body.indexOf('=');
    if (eq < 0) return node.hasAttribute(body);
    return node.getAttribute(body.slice(0, eq)) === body.slice(eq + 1).replace(/^["']|["']$/g, '');
  }
  return node.tagName === selector.toUpperCase();
}

function makeNode(nodeType: number, nodeName: string, owner: MiniDoc): MiniNode {
  const node: MiniNode = {
    nodeType,
    nodeName,
    tagName: nodeType === 1 ? nodeName : undefined,
    namespaceURI: nodeType === 1 ? 'http://www.w3.org/1999/xhtml' : null,
    ownerDocument: owner,
    parentNode: null,
    childNodes: [],
    firstChild: null,
    lastChild: null,
    nextSibling: null,
    previousSibling: null,
    attributes: new Map(),
    listeners: new Map(),
    /* Plain style object so React's module-init `in` probes and style-prop
     * writes never hit undefined. */
    style: {},
    text: '',
    value: '',
    scrollTop: 0,
    scrollHeight: 0,
    clientHeight: 0,
    get className() {
      return node.getAttribute('class') ?? '';
    },
    set className(value: string) {
      node.setAttribute('class', value);
    },
    get textContent() {
      if (nodeType === 3 || nodeType === 8) return node.text;
      return node.childNodes.map((child) => child.textContent).join('');
    },
    set textContent(value: string) {
      node.childNodes = [];
      if (nodeType !== 3 && nodeType !== 8 && value !== '') {
        node.childNodes.push(owner.createTextNode(value));
      }
      node.text = nodeType === 3 || nodeType === 8 ? value : '';
      relink(node);
    },
    get nodeValue() {
      return nodeType === 3 || nodeType === 8 ? node.text : null;
    },
    set nodeValue(value: string | null) {
      if (nodeType === 3 || nodeType === 8) node.text = value ?? '';
    },
    appendChild(child) {
      return node.insertBefore(child, null);
    },
    insertBefore(child, before) {
      if (child.parentNode) child.parentNode.removeChild(child);
      const index = before === null ? node.childNodes.length : node.childNodes.indexOf(before);
      node.childNodes.splice(index < 0 ? node.childNodes.length : index, 0, child);
      relink(node);
      return child;
    },
    removeChild(child) {
      const index = node.childNodes.indexOf(child);
      if (index < 0) throw new Error('not a child');
      node.childNodes.splice(index, 1);
      child.parentNode = null;
      child.nextSibling = null;
      child.previousSibling = null;
      relink(node);
      return child;
    },
    addEventListener(type, fn, options) {
      const capture = options === true || (typeof options === 'object' && options.capture === true);
      const key = capture ? `${type}-capture` : type;
      const set = node.listeners.get(key) ?? new Set();
      set.add(fn);
      node.listeners.set(key, set);
    },
    removeEventListener(type, fn, options) {
      const capture = options === true || (typeof options === 'object' && options.capture === true);
      const key = capture ? `${type}-capture` : type;
      node.listeners.get(key)?.delete(fn);
    },
    setAttribute(name, value) {
      node.attributes.set(name, String(value));
    },
    getAttribute(name) {
      return node.attributes.has(name) ? node.attributes.get(name)! : null;
    },
    removeAttribute(name) {
      node.attributes.delete(name);
    },
    hasAttribute(name) {
      return node.attributes.has(name);
    },
    querySelector(selector) {
      return node.querySelectorAll(selector)[0] ?? null;
    },
    querySelectorAll(selector) {
      const out: MiniNode[] = [];
      const visit = (current: MiniNode): void => {
        for (const child of current.childNodes) {
          if (matches(child, selector)) out.push(child);
          visit(child);
        }
      };
      visit(node);
      return out;
    },
    // Capture root-down, fire the target, bubble target-up: enough of the
    // real model for React's delegated root listeners (registered in both
    // phases on the container) to receive dispatches.
    dispatchEvent(raw) {
      const path: MiniNode[] = [];
      let current: MiniNode | null = node;
      while (current !== null) {
        path.push(current);
        current = current.parentNode;
      }
      const target = (raw.target ?? node) as MiniNode;
      for (let i = path.length - 1; i >= 0; i -= 1) {
        for (const fn of [...(path[i].listeners.get(`${raw.type}-capture`) ?? [])]) fn({ ...raw, target });
      }
      for (const fn of [...(node.listeners.get(String(raw.type)) ?? [])]) fn({ ...raw, target });
      for (let i = 1; i < path.length; i += 1) {
        for (const fn of [...(path[i].listeners.get(String(raw.type)) ?? [])]) fn({ ...raw, target });
      }
      return true;
    },
    click() {
      node.dispatchEvent({ type: 'click', target: node });
    },
    // React's commitMount calls focus() for autoFocus inputs.
    focus() {
      owner.activeElement = node;
    },
  };
  return node;
}

class DummyHTMLIFrameElement {}

const installed = new Set<string>();
function installGlobals(): MiniDoc {
  const doc = makeNode(9, '#document', null as unknown as MiniDoc) as unknown as MiniDoc & MiniNode;
  const html = makeNode(1, 'HTML', doc);
  const body = makeNode(1, 'BODY', doc);
  html.appendChild(body);
  doc.appendChild(html);
  doc.createElement = (tag: string) => makeNode(1, tag.toUpperCase(), doc);
  doc.createTextNode = (text: string) => {
    const textNode = makeNode(3, '#text', doc);
    textNode.text = text;
    return textNode;
  };
  doc.createComment = (text: string) => {
    const comment = makeNode(8, '#comment', doc);
    comment.text = text;
    return comment;
  };
  doc.createDocumentFragment = () => makeNode(11, '#document-fragment', doc);
  doc.getElementById = (id: string) => doc.querySelector(`#${id}`);
  doc.body = body;
  doc.activeElement = body;
  /* 'oninput' must exist as an own property: see the MiniDoc.oninput note. */
  doc.oninput = null;
  const windowObj = {
    document: doc,
    event: undefined as Record<string, unknown> | undefined,
    HTMLIFrameElement: DummyHTMLIFrameElement as unknown as typeof DummyHTMLIFrameElement,
  };
  const overrides: Record<string, unknown> = {
    document: doc,
    window: windowObj,
    IS_REACT_ACT_ENVIRONMENT: true,
  };
  for (const [key, value] of Object.entries(overrides)) {
    Object.defineProperty(globalThis, key, { configurable: true, writable: true, value });
    installed.add(key);
  }
  return doc;
}
function uninstallGlobals(): void {
  for (const key of installed) delete (globalThis as Record<string, unknown>)[key];
  installed.clear();
}

/* Globals must be live before react-dom initializes (module-init snapshot). */
installGlobals();
({ createElement, act } = await import('react'));
({ createRoot } = await import('react-dom/client'));
({ SessionPanel } = await import('../src/panels/SessionPanel.tsx'));

/* ------------------------------------------------------------------ *
 * Fake control plane: get_state / get_session_stats answer immediately;
 * get_fork_messages serves the newest-first page and parks every cursor
 * request in a deferred keyed by the cursor so tests control timing,
 * ordering, and failures exactly.
 * ------------------------------------------------------------------ */
type Command = Record<string, unknown>;

interface DeferredReply {
  resolve(value: unknown): void;
  reject(error: unknown): void;
}

interface Backend {
  commands: Command[];
  pending: Map<string, DeferredReply>;
  sendCommand: (command: Command) => Promise<unknown>;
  onLifecycleResult: (result: unknown) => Promise<void>;
  lifecycleResults: unknown[];
  toasts: Array<{ message: string; error: boolean }>;
}

const PANEL_SESSION_ID = 'sess-1';
const MOUNT_CWD = '/home/u/proj';

function createBackend(firstPage: () => unknown, stateSessionId?: { current: string }): Backend {
  const commands: Command[] = [];
  const pending = new Map<string, DeferredReply>();
  const lifecycleResults: unknown[] = [];
  const toasts: Array<{ message: string; error: boolean }> = [];
  const sendCommand = async (command: Command): Promise<unknown> => {
    commands.push({ ...command });
    if (command.type === 'get_state') {
      return {
        sessionId: stateSessionId?.current ?? PANEL_SESSION_ID,
        sessionName: 'main',
        cwd: MOUNT_CWD,
        messageCount: 2,
        pendingMessageCount: 0,
      };
    }
    if (command.type === 'get_session_stats') return null;
    if (command.type === 'get_fork_messages') {
      if (typeof command.cursor !== 'string') return firstPage();
      const deferred = Promise.withResolvers<unknown>();
      pending.set(String(command.cursor), deferred);
      return deferred.promise;
    }
    return { ok: true };
  };
  return {
    commands,
    pending,
    sendCommand,
    onLifecycleResult: async (result: unknown) => {
      lifecycleResults.push(result);
    },
    onToast: (message: string, error = false) => {
      toasts.push({ message, error });
    },
    lifecycleResults,
    toasts,
  };
}

function message(entryId: string, text?: string): ForkMessageWire {
  return { entryId, text: text ?? `prompt ${entryId}` };
}

function page(messages: ForkMessageWire[], nextCursor?: string): Record<string, unknown> {
  return nextCursor !== undefined
    ? { messages, nextCursor, truncated: true }
    : { messages, truncated: false };
}

async function mountPanel(backend: Backend, doc: MiniDoc): Promise<{ container: MiniNode; root: Root }> {
  const container = doc.createElement('div');
  doc.body.appendChild(container);
  const root = createRoot(container as unknown as Element);
  await act(async () => {
    root.render(
      createElement(SessionPanel, {
        sessionId: PANEL_SESSION_ID,
        sendCommand: backend.sendCommand,
        waitForReady: async () => {},
        refreshState: async () => {},
        onLifecycleResult: backend.onLifecycleResult,
        onToast: backend.onToast,
        onClose: () => {},
      }),
    );
    await realTick();
  });
  return { container, root };
}

async function flushWork(): Promise<void> {
  await act(async () => {
    await realTick();
  });
}

function pressKey(node: MiniNode, key: string): void {
  act(() => {
    node.dispatchEvent({
      type: 'keydown',
      key,
      target: node,
      preventDefault() {},
      stopPropagation() {},
      stopImmediatePropagation() {},
    });
  });
}

function clickInAct(node: MiniNode): void {
  act(() => {
    node.click();
  });
}

/** Set the listbox scroll geometry and dispatch a scroll event inside one
 *  act scope; the paging decision reads the geometry from the list ref. */
async function scrollListTo(list: MiniNode, scrollTop: number, scrollHeight?: number, clientHeight?: number): Promise<void> {
  await act(async () => {
    if (scrollHeight !== undefined) list.scrollHeight = scrollHeight;
    if (clientHeight !== undefined) list.clientHeight = clientHeight;
    list.scrollTop = scrollTop;
    list.dispatchEvent({ type: 'scroll', target: list, bubbles: false, cancelable: false });
    await realTick();
  });
}

function forkCursorCommands(commands: Command[]): Command[] {
  return commands.filter((command) => command.type === 'get_fork_messages' && typeof command.cursor === 'string');
}

function forkCommand(commands: Command[]): Command | undefined {
  return commands.find((command) => command.type === 'fork');
}

const roots: Root[] = [];
async function unmountAll(): Promise<void> {
  while (roots.length > 0) {
    const root = roots.pop()!;
    await act(async () => {
      root.unmount();
    });
  }
}

/* ------------------------------------------------------------------ *
 * 1. Mount: bounded listbox contract + zero Load older remnants + click
 *    selection + Fork uses the selected entry.
 * ------------------------------------------------------------------ */
{
  const doc = installGlobals();
  const backend = createBackend(() => page([message('e4'), message('e3')], 'c1'));
  const { container, root } = await mountPanel(backend, doc);
  roots.push(root);
  await flushWork();

  const picker = container.querySelector('#session-fork-picker-btn');
  const list = container.querySelector('#session-fork-list');
  check('mount renders one compact Fork picker', picker !== null && picker.getAttribute('aria-expanded') === 'false');
  check('candidate rows stay visually collapsed until requested', list !== null && list.hasAttribute('hidden'));
  check('candidate listbox keeps the bounded-scroll CSS hook', list !== null && list.className.includes('session-fork__list'));
  check('picker summarizes the selected newest entry', picker?.textContent === 'prompt e4');
  check('Fork is enabled with a loaded page and selection', container.querySelector('#session-fork-btn')?.hasAttribute('disabled') === false);
  check('mount probes the first page with the explicit owner sessionId', backend.commands.some((command) => command.type === 'get_fork_messages' && command.sessionId === PANEL_SESSION_ID && command.cursor === undefined));
  check('overflowing default geometry issues zero automatic prefetches', forkCursorCommands(backend.commands).length === 0);

  clickInAct(picker!);
  await flushWork();
  const options = (list?.querySelectorAll('[role="option"]') ?? []) as MiniNode[];
  check('opening the picker reveals the bounded listbox', list !== null && !list.hasAttribute('hidden') && options.length === 2);
  clickInAct(options[1]);
  await flushWork();
  check('choosing a candidate collapses the listbox', list?.hasAttribute('hidden') === true);
  check('picker now summarizes the chosen entry', picker?.textContent === 'prompt e3');
  clickInAct(container.querySelector('#session-fork-btn')!);
  await flushWork();
  const forked = forkCommand(backend.commands);
  check('Fork sends exactly one fork command', forked !== undefined && backend.commands.filter((command) => command.type === 'fork').length === 1);
  check('Fork uses the selected entry id', forked?.entryId === 'e3');
  check('fork hands off exactly one lifecycle result', backend.lifecycleResults.length === 1);
}

/* ------------------------------------------------------------------ *
 * 2. Near-bottom scroll paging: one request per cycle, single-flight
 *    while in flight, merge with dedupe, sticky selection, and the
 *    terminal stop when the wire cursor is exhausted.
 * ------------------------------------------------------------------ */
{
  const doc = installGlobals();
  const backend = createBackend(() => page([message('e4'), message('e3')], 'c1'));
  const { container, root } = await mountPanel(backend, doc);
  roots.push(root);
  await flushWork();
  const list = container.querySelector('#session-fork-list')!;

  list.scrollHeight = 500;
  list.clientHeight = 100;
  await scrollListTo(list, 470);
  check('near-bottom scroll issues exactly one cursor request', forkCursorCommands(backend.commands).length === 1 && forkCursorCommands(backend.commands)[0]?.cursor === 'c1');

  await scrollListTo(list, 480);
  await scrollListTo(list, 490);
  check('single-flight: repeated near-bottom scrolls stay one request', forkCursorCommands(backend.commands).length === 1);

  backend.pending.get('c1')!.resolve(page([message('e1'), message('e0')], 'c0'));
  await flushWork();
  const merged = list.querySelectorAll('[role="option"]') as MiniNode[];
  check('older page merges newest-first with dedupe', merged.length === 4 && merged[3]?.getAttribute('id') === 'session-fork-option-3' && merged[3]?.textContent === 'prompt e0');
  check('selection stays sticky across the merge', merged[0]?.getAttribute('aria-selected') === 'true');

  // Second cycle fetches the next cursor exactly once...
  await scrollListTo(list, 500);
  check('next cycle fetches the advanced cursor', forkCursorCommands(backend.commands).length === 2 && forkCursorCommands(backend.commands)[1]?.cursor === 'c0');
  // ...and the terminal page (no nextCursor) stops paging permanently.
  backend.pending.get('c0')!.resolve(page([message('e-1')]));
  await flushWork();
  const cursorRequestsAtTerminal = forkCursorCommands(backend.commands).length;
  backend.pending.clear();
  await scrollListTo(list, 500);
  await scrollListTo(list, 500);
  check('terminal stop: exhausted cursor never re-requests', forkCursorCommands(backend.commands).length === cursorRequestsAtTerminal, String(forkCursorCommands(backend.commands).length));
}

/* ------------------------------------------------------------------ *
 * 3. Exact stale guards: a parked older-page response lands only when the
 *    exact session, cursor, request generation, and mount still own it.
 * ------------------------------------------------------------------ */
{
  const doc = installGlobals();
  const backend = createBackend(() => page([message('e4')], 'c1'));
  const { container, root } = await mountPanel(backend, doc);
  roots.push(root);
  await flushWork();
  const list = container.querySelector('#session-fork-list')!;
  list.scrollHeight = 300;
  list.clientHeight = 100;

  // Park a near-bottom request, then refresh: the full page replacement
  // bumps the older-request generation, so the parked response must drop.
  await scrollListTo(list, 300);
  check('older request parked for the stale-guard race', backend.pending.has('c1'));
  clickInAct(container.querySelector('#session-refresh-btn')!);
  await flushWork();
  backend.pending.get('c1')!.resolve(page([message('STALE-1'), message('STALE-2')], 'c9'));
  await flushWork();
  check('stale cursor page is dropped after a full replacement', !container.textContent.includes('STALE-1'), container.textContent);
  check('fresh page survives the stale drop', (list.querySelectorAll('[role="option"]') as MiniNode[]).some((node) => node.textContent === 'prompt e4'));

  // Session change: a refresh that reports a different session key makes
  // the parked page for the OLD session unownable.
  const stateSessionId = { current: PANEL_SESSION_ID };
  const backend2 = createBackend(() => page([message('f1')], 'c1'), stateSessionId);
  const mounted2 = await mountPanel(backend2, doc);
  roots.push(mounted2.root);
  await flushWork();
  const list2 = mounted2.container.querySelector('#session-fork-list')!;
  list2.scrollHeight = 300;
  list2.clientHeight = 100;
  await scrollListTo(list2, 300);
  check('second panel parks its own older request', backend2.pending.has('c1'));
  stateSessionId.current = 'sess-2';
  clickInAct(mounted2.container.querySelector('#session-refresh-btn')!);
  await flushWork();
  backend2.pending.get('c1')!.resolve(page([message('STALE-OTHER')], 'c9'));
  await flushWork();
  check('session change rejects the parked page for the old session', !mounted2.container.textContent.includes('STALE-OTHER'), mounted2.container.textContent);
  check('panel re-keys to the new session state', mounted2.container.querySelector('[data-testid="session-cwd-value"]')?.textContent === MOUNT_CWD);
  check('stale-drop kept the original cursor requests intact', forkCursorCommands(backend.commands).length === 1 && forkCursorCommands(backend2.commands).length === 1);
}

/* ------------------------------------------------------------------ *
 * 4. Error retains/retry: a failed older page retains the loaded list,
 *    latches an explicit Retry, toasts exactly once, pauses scroll
 *    paging, and Retry re-issues the SAME cursor until it succeeds.
 * ------------------------------------------------------------------ */
{
  const doc = installGlobals();
  const backend = createBackend(() => page([message('e4'), message('e3')], 'c1'));
  const { container, root } = await mountPanel(backend, doc);
  roots.push(root);
  await flushWork();
  const list = container.querySelector('#session-fork-list')!;
  list.scrollHeight = 400;
  list.clientHeight = 100;
  await scrollListTo(list, 400);
  const beforeCount = (list.querySelectorAll('[role="option"]') as MiniNode[]).length;
  backend.pending.get('c1')!.reject(new Error('backend exploded'));
  await flushWork();
  check('failed page load retains the loaded messages', (list.querySelectorAll('[role="option"]') as MiniNode[]).length === beforeCount);
  check('failure latches the explicit Retry affordance', container.querySelector('#session-fork-retry-btn') !== null);
  check('failure toasts exactly once with the failure context', backend.toasts.length === 1 && backend.toasts[0].error === true && backend.toasts[0].message === 'load more failed: backend exploded');

  await scrollListTo(list, 400);
  await scrollListTo(list, 400);
  check('scroll paging stays paused while the error is latched', forkCursorCommands(backend.commands).length === 1);

  clickInAct(container.querySelector('#session-fork-retry-btn')!);
  await flushWork();
  check('Retry re-issues the SAME cursor exactly once', forkCursorCommands(backend.commands).length === 2 && forkCursorCommands(backend.commands)[1]?.cursor === 'c1');
  backend.pending.get('c1')!.resolve(page([message('e3-older')], 'c2'));
  await flushWork();
  check('successful retry clears the latch and merges', container.querySelector('#session-fork-retry-btn') === null && (list.querySelectorAll('[role="option"]') as MiniNode[]).length === 3);
  check('retry success produces no extra toast', backend.toasts.length === 1);
}

/* ------------------------------------------------------------------ *
 * 5. At most one short-list prefetch: a window that does not overflow
 *    its bounded viewport can never scroll, so exactly one automatic
 *    page pull happens per fresh page state — never a loop.
 * ------------------------------------------------------------------ */
{
  const doc = installGlobals();
  const backend = createBackend(() => page([message('s1')], 'c1'));
  const { container, root } = await mountPanel(backend, doc);
  roots.push(root);
  await flushWork();
  const list = container.querySelector('#session-fork-list')!;
  // Overflowing window: no prefetch.
  list.scrollHeight = 600;
  list.clientHeight = 100;
  await flushWork();
  check('overflowing window never auto-prefetches', forkCursorCommands(backend.commands).length === 0);

  // Shrink the window below the content, then trigger a full page
  // replacement: the fresh page state re-arms the short-list budget and the
  // layout effect (not any scroll event — a short window cannot scroll)
  // pulls exactly one more page.
  list.scrollHeight = 100;
  list.clientHeight = 100;
  clickInAct(container.querySelector('#session-refresh-btn')!);
  await flushWork();
  check('short window issues exactly one automatic prefetch', forkCursorCommands(backend.commands).length === 1 && forkCursorCommands(backend.commands)[0]?.cursor === 'c1');
  backend.pending.get('c1')!.resolve(page([message('s0')], 'c2'));
  await flushWork();
  check('prefetch merged the next page', (list.querySelectorAll('[role="option"]') as MiniNode[]).length === 2);
  await flushWork();
  check('at most ONE short-list prefetch: no second automatic pull', forkCursorCommands(backend.commands).length === 1);

  // A full page replacement re-arms the budget for exactly one more pull.
  clickInAct(container.querySelector('#session-refresh-btn')!);
  await flushWork();
  check('refresh re-arms exactly one short-list prefetch', forkCursorCommands(backend.commands).length === 2);
  const rearmedCursor = String(forkCursorCommands(backend.commands)[1]?.cursor || '');
  backend.pending.get(rearmedCursor)!.resolve(page([message('s-1')], 'c3'));
  await flushWork();
  check('re-armed prefetch also stays single-shot', forkCursorCommands(backend.commands).length === 2);
}

/* ------------------------------------------------------------------ *
 * 6. Sticky selection + fork anchoring: the selection survives page
 *    merges, a full refresh, and a synthetic sticky row when the newest
 *    page omits the selected older entry; fork uses the sticky entry.
 * ------------------------------------------------------------------ */
{
  const doc = installGlobals();
  const backend = createBackend(() => page([message('e4'), message('e3')], 'c1'));
  const { container, root } = await mountPanel(backend, doc);
  roots.push(root);
  await flushWork();
  clickInAct(container.querySelector('#session-fork-picker-btn')!);
  await flushWork();
  const list = container.querySelector('#session-fork-list')!;
  list.scrollHeight = 600;
  list.clientHeight = 100;
  await scrollListTo(list, 600);
  backend.pending.get('c1')!.resolve(page([message('e0')], 'c0'));
  await flushWork();
  const merged = list.querySelectorAll('[role="option"]') as MiniNode[];
  check('merged window exposes every candidate in order', merged.length === 3 && merged[2]?.getAttribute('id') === 'session-fork-option-2');
  clickInAct(merged[2]);
  await flushWork();
  check('user selection lands on the oldest loaded entry', container.querySelector('#session-fork-picker-btn')?.textContent === 'prompt e0');

  // Refresh replaces the window with the newest page only: the sticky
  // selection from the older page survives as a synthetic leading row.
  clickInAct(container.querySelector('#session-refresh-btn')!);
  await flushWork();
  clickInAct(container.querySelector('#session-fork-picker-btn')!);
  await flushWork();
  const refreshedList = container.querySelector('#session-fork-list')!;
  const afterRefresh = (refreshedList.querySelectorAll('[role="option"]') as MiniNode[]).map((node) => node.getAttribute('id'));
  check('synthetic sticky row anchors the omitted selection', afterRefresh.includes('session-fork-option-sticky'));
  const sticky = refreshedList.querySelector('#session-fork-option-sticky')!;
  check('synthetic sticky row is the selected option', sticky.getAttribute('aria-selected') === 'true' && sticky.textContent === 'e0' && refreshedList.getAttribute('aria-activedescendant') === 'session-fork-option-sticky');
  check('sticky selection keeps Fork enabled', container.querySelector('#session-fork-btn')?.hasAttribute('disabled') === false);
  pressKey(refreshedList, 'Enter');
  await flushWork();
  const forked = forkCommand(backend.commands);
  check('Enter forks the sticky selected entry', forked?.entryId === 'e0', JSON.stringify(forked));
  check('fork hands off exactly one lifecycle result', backend.lifecycleResults.length === 1);
}

/* ------------------------------------------------------------------ *
 * 7. Keyboard model: ArrowDown/ArrowUp wrap, Home/End jump, Enter forks
 *    the selected entry.
 * ------------------------------------------------------------------ */
{
  const doc = installGlobals();
  const backend = createBackend(() => page([message('k1'), message('k2'), message('k3')], 'ck'));
  const { container, root } = await mountPanel(backend, doc);
  roots.push(root);
  clickInAct(container.querySelector('#session-fork-picker-btn')!);
  await flushWork();
  await flushWork();
  const list = container.querySelector('#session-fork-list')!;

  pressKey(list, 'ArrowDown');
  await flushWork();
  check('ArrowDown moves the selection to the next option', list.getAttribute('aria-activedescendant') === 'session-fork-option-1');
  pressKey(list, 'ArrowUp');
  await flushWork();
  check('ArrowUp returns to the previous option', list.getAttribute('aria-activedescendant') === 'session-fork-option-0');
  pressKey(list, 'ArrowUp');
  await flushWork();
  check('ArrowUp wraps from the head to the tail', list.getAttribute('aria-activedescendant') === 'session-fork-option-2');
  pressKey(list, 'ArrowDown');
  await flushWork();
  check('ArrowDown wraps from the tail to the head', list.getAttribute('aria-activedescendant') === 'session-fork-option-0');
  pressKey(list, 'End');
  await flushWork();
  check('End jumps to the last option', list.getAttribute('aria-activedescendant') === 'session-fork-option-2');
  pressKey(list, 'Home');
  await flushWork();
  check('Home jumps to the first option', list.getAttribute('aria-activedescendant') === 'session-fork-option-0');
  pressKey(list, 'Enter');
  await flushWork();
  const forked = forkCommand(backend.commands);
  check('Enter forks the selected entry', forked !== undefined && forked.entryId === 'k1');
  check('keyboard fork hands off exactly one lifecycle result', backend.lifecycleResults.length === 1);
}

/* ------------------------------------------------------------------ *
 * 8. Empty page: empty state renders, control surface disabled, zero
 *    cursor requests, zero fork commands.
 * ------------------------------------------------------------------ */
{
  const doc = installGlobals();
  const backend = createBackend(() => page([]));
  const { container, root } = await mountPanel(backend, doc);
  roots.push(root);
  await flushWork();
  const list = container.querySelector('#session-fork-list')!;
  check('empty page renders the no-candidates state', container.textContent.includes('No forkable messages'));
  check('empty listbox is not keyboard-focusable', list.getAttribute('tabindex') === '-1');
  check('Fork is disabled on an empty page', container.querySelector('#session-fork-btn')?.hasAttribute('disabled') === true);
  await scrollListTo(list, 0, 100);
  check('empty page never pages', forkCursorCommands(backend.commands).length === 0);
  clickInAct(container.querySelector('#session-fork-btn')!);
  await flushWork();
  check('Fork on an empty page sends zero fork commands', forkCommand(backend.commands) === undefined);
  check('empty page never toasts from background paging', backend.toasts.length === 0);
}

await unmountAll();
console.log(`sessionPanelForkList.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);