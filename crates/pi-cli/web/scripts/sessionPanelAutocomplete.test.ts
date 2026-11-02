#!/usr/bin/env node
// SessionPanel × PathAutocomplete — rendered integration regression.
//
// Replaces the old source-scanning block in sessionLocation.test.ts with a
// real createRoot render of the owning panel over a mini DOM (same pattern
// as copyButton.test.ts). A fake sendCommand serves
// get_state / get_session_stats / get_fork_messages at mount and parks each
// path_complete reply in a deferred, so the test drives behavior end to end:
//   * mount leaves the New-session input EMPTY despite the current info cwd
//     (no prefill/inherit: the button is disabled and Enter emits nothing)
//   * controlled multi-character typing of an arbitrary absolute directory
//     emits debounced path_complete probes pinned to kind=directory + sessionId
//   * closed-menu Enter -> exactly one new_session with the exact typed cwd
//   * open-menu ArrowDown/Enter accepts a directory suggestion and does NOT
//     create until a later, menu-closed Enter
//   * a stale (superseded-generation) completion reply never opens the menu
//   * whitespace-only input keeps New disabled; Enter still emits nothing
//   * an unauthorized (unknown-session / path_outside_workspace) completion
//     failure stays fail-closed and quiet: menu never opens, zero toasts,
//     zero panel status, manual entry unaffected, later valid reply recovers
//   * a session change resets the input to empty
//   * rejected (unauthorized) new_session -> exactly one onToast error,
//     zero panel status above Current session, current session unchanged
//   * local validation (blank rename, fork with no selection) -> exactly
//     one toast each, zero panel status, zero wire commands
//   * immediate user-action transport failure (no toastOwned) -> exactly one
//     panel toast; App-owned (toastOwned) outage stays silent

// Time is a deterministic fake clock: the completion debounce schedules
// through the patched global setTimeout and only fires when the test
// advances the clock, so no wall-clock sleeps and no races.
//
// Focused run:
//   esbuild scripts/sessionPanelAutocomplete.test.ts --bundle --platform=node \
//     --format=esm --outfile=.web-test-build/sessionPanelAutocomplete.test.mjs \
//   && node .web-test-build/sessionPanelAutocomplete.test.mjs
//
// Exit codes: 0 = every assertion held; 1 = a regression.
/* Type-only imports are erased, so they cannot init the modules. The value
 * bindings below are assigned by the top-level dynamic import that runs
 * after installGlobals(): static value imports would hoist and freeze
 * react-dom's canUseDOM snapshot with globals absent. */
import type { act as Act, createElement as CreateElement } from 'react';
import type { createRoot as CreateRoot, Root } from 'react-dom/client';
import type { SessionPanel as SessionPanelComponent } from '../src/panels/SessionPanel.tsx';
let createElement: CreateElement;
let act: Act;
let createRoot: CreateRoot;
let PATH_COMPLETE_DEBOUNCE_MS: number;
let SessionPanel: SessionPanelComponent;

const failures: string[] = [];
let ran = 0;
function check(name: string, condition: boolean, detail?: string): void {
  ran += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

/* ------------------------------------------------------------------ *
 * Deterministic clock. The completion controller captures setTimeout
 * dynamically at delay() time, so patching the global routes every
 * debounce into a queue the test fires explicitly. act() itself only
 * uses microtasks (verified against react.development.js), so nothing
 * else ever observes the patched globals.
 * ------------------------------------------------------------------ */
const realSetTimeout = globalThis.setTimeout.bind(globalThis);
let clockNow = 0;
let clockNextId = 1;
let clockPending: Array<{ id: number; at: number; fn: () => void }> = [];

function fakeSetTimeout(fn: () => void, ms?: number): unknown {
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

const realTick = (): Promise<void> => {
  const { promise, resolve } = Promise.withResolvers<void>();
  realSetTimeout(resolve, 0);
  return promise;
};

/* ------------------------------------------------------------------ *
 * Mini DOM — the surface React 18 client rendering + this panel use:
 * tree ops, attributes, delegated event dispatch (capture -> target ->
 * bubble), text nodes, and form accessors (input.value/type, select
 * .options). Prototype-less nodes make React's value tracker bail, so
 * every dispatched input event reports a change (deterministic typing).
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
  /* Real documents expose on* IDL slots; React's isEventSupported('input')
   * probe at module init checks `'oninput' in document` to decide whether
   * input events are supported (without it ChangeEventPlugin falls back to
   * its IE polyfill, which crashes on the first keydown here). */
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
  type: string;
  options: MiniNode[] | undefined;
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
    /* React's getVendorPrefixedEventName probes `document.createElement('div')
     * .style` with `in` at module init (canUseDOM is true by then); every
     * element carries a plain style object so the probe and style-prop
     * writes never hit undefined. */
    style: {},
    text: '',
    get value() {
      if (nodeType === 1) return node.attributes.get('value') ?? '';
      return node.text;
    },
    set value(next: string) {
      if (nodeType === 1) node.setAttribute('value', String(next));
      else node.text = String(next);
    },
    get type() {
      return node.getAttribute('type') ?? '';
    },
    get options() {
      if (nodeType !== 1 || nodeName !== 'SELECT') return undefined;
      return node.childNodes.filter((child) => child.tagName === 'OPTION');
    },
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
    // phases on the container) to receive input/keydown/click dispatches.
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
    // React's commitMount calls focus() for autoFocus inputs; the mini DOM
    // records the focused element so tests can assert mount focus.
    focus() {
      owner.activeElement = node;
    },
  };
  return node;
}

/* Stable dummy iframe constructor for the shim window: ReactDOM's selection
 * restore (getActiveElementDeep) evaluates `element instanceof
 * win.HTMLIFrameElement` around every controlled update. The dummy's
 * prototype sits outside every mini-DOM node's chain, so the check is a
 * deterministic false and the loop exits immediately. */
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
  /* ReactDOM's client render/unmount reads `window.event` for event-priority
   * lane selection (getCurrentEventPriority) before/without any native event
   * flowing — the mini-DOM drives React's delegated listeners directly; a
   * bare `event` slot (undefined outside any real dispatch) is the whole
   * surface createRoot needs there. */
  const windowObj = {
    document: doc,
    event: undefined as Record<string, unknown> | undefined,
    HTMLIFrameElement: DummyHTMLIFrameElement as unknown as typeof DummyHTMLIFrameElement,
  };
  const overrides: Record<string, unknown> = {
    document: doc,
    window: windowObj,
    IS_REACT_ACT_ENVIRONMENT: true,
    setTimeout: fakeSetTimeout,
    clearTimeout: fakeClearTimeout,
  };
  for (const [key, value] of Object.entries(overrides)) {
    Object.defineProperty(globalThis, key, { configurable: true, writable: true, value });
    installed.add(key);
  }
  return doc;
}
/* Globals must be live before react-dom initializes (see the import note at
 * the top of the file). Each test block below re-installs a fresh document;
 * react-dom only snapshots canUseDOM/isInputEventSupported at module init. */
installGlobals();
({ createElement, act } = await import('react'));
({ createRoot } = await import('react-dom/client'));
({ PATH_COMPLETE_DEBOUNCE_MS } = await import('../src/PathAutocomplete'));
({ SessionPanel } = await import('../src/panels/SessionPanel.tsx'));
function uninstallGlobals(): void {
  for (const key of installed) delete (globalThis as Record<string, unknown>)[key];
  installed.clear();
}

/* ------------------------------------------------------------------ *
 * Fake control-plane socket: get_state / get_session_stats /
 * get_fork_messages answer immediately; each path_complete parks its
 * reply in a deferred keyed by the requested path so the test controls
 * completion timing (needed for the stale-generation scenario).
 * ------------------------------------------------------------------ */
type Command = Record<string, unknown>;

interface DeferredReply {
  resolve(value: unknown): void;
}

interface Backend {
  commands: Command[];
  pendingComplete: Map<string, DeferredReply>;
  sendCommand: (command: Command) => Promise<unknown>;
  onLifecycleResult: (result: unknown) => Promise<void>;
  lifecycleResults: unknown[];
  /** App-provided onToast capture: user action/lifecycle/local-validation
   *  failures must surface here exactly once, never as panel status. */
  toasts: Array<{ message: string; error: boolean }>;
}

const PANEL_SESSION_ID = 'sess-1';
const MOUNT_CWD = '/home/u/proj';

function createBackend(): Backend {
  const commands: Command[] = [];
  const pendingComplete = new Map<string, DeferredReply>();
  const lifecycleResults: unknown[] = [];
  const toasts: Array<{ message: string; error: boolean }> = [];
  const sendCommand = async (command: Command): Promise<unknown> => {
    commands.push({ ...command });
    if (command.type === 'get_state') {
      return {
        sessionId: PANEL_SESSION_ID,
        sessionName: 'main',
        model: { provider: 'test', id: 'm1' },
        thinkingLevel: 'off',
        cwd: MOUNT_CWD,
        sessionFile: '/home/u/proj/.pi/sessions/sess-1.jsonl',
        messageCount: 2,
        pendingMessageCount: 0,
      };
    }
    if (command.type === 'get_session_stats') return null;
    if (command.type === 'get_fork_messages') return { messages: [], truncated: false };
    if (command.type === 'path_complete') {
      const deferred = Promise.withResolvers<unknown>();
      pendingComplete.set(String(command.path), { resolve: deferred.resolve });
      return deferred.promise;
    }
    return { ok: true };
  };
  return {
    commands,
    pendingComplete,
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
    // The mount effect's floating load promise settles in microtasks; drain
    // it inside this act scope so its setStates never fire un-wrapped.
    await realTick();
  });
  return { container, root };
}

/** Fire the completion debounce, then drain the resulting promise chains
 *  inside one act scope (deterministic: no wall-clock waits). */
async function flushDebounce(): Promise<void> {
  await act(async () => {
    advanceClock(PATH_COMPLETE_DEBOUNCE_MS + 1);
    await realTick();
  });
}

/** Drain pending promise chains (e.g. a deferred completion reply or the
 *  async lifecycle run after Enter) inside one act scope. */
async function flushWork(): Promise<void> {
  await act(async () => {
    await realTick();
  });
}

/** Controlled write: set the whole value, then dispatch the input event
 *  React's ChangeEventPlugin listens for (act-flushed so onChange lands). */
async function setInputValue(input: MiniNode, next: string): Promise<void> {
  await act(async () => {
    input.value = next;
    input.dispatchEvent({ type: 'input', target: input, preventDefault() {}, stopPropagation() {} });
  });
}

/** Controlled multi-character typing: every character is its own act flush
 *  so each intermediate value reaches the component's onChange. */
async function typeInto(input: MiniNode, text: string): Promise<void> {
  for (const ch of text) {
    await setInputValue(input, input.value + ch);
  }
}

/** Synchronous key dispatch inside one act scope: keydown handlers run
 *  synchronously, so any setState they trigger is act-tracked and the
 *  harness stays free of React act(...) warnings. */
function pressKey(input: MiniNode, key: string): void {
  act(() => {
    input.dispatchEvent({
      type: 'keydown',
      key,
      target: input,
      preventDefault() {},
      stopPropagation() {},
      stopImmediatePropagation() {},
    });
  });
}

/** Click dispatch inside one act scope (same warning discipline). */
function clickInAct(node: MiniNode): void {
  act(() => {
    node.click();
  });
}

/** A well-formed path_complete reply (parsePathCompleteResponse wire shape). */
function replyFor(entries: Array<{ name: string; path: string; kind: 'directory' | 'file' }>): unknown {
  return { base: '/', prefix: '', entries: entries.map((entry) => ({ ...entry })), truncated: false };
}

function newSessionCommands(commands: Command[]): Command[] {
  return commands.filter((command) => command.type === 'new_session');
}

function pathCompleteCommands(commands: Command[]): Command[] {
  return commands.filter((command) => command.type === 'path_complete');
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
 * 1. Mount leaves the New-directory input EMPTY despite the current info
 *    cwd; New is disabled and empty Enter emits zero commands.
 * ------------------------------------------------------------------ */
{
  const doc = installGlobals();
  const backend = createBackend();
  const { container, root } = await mountPanel(backend, doc);
  roots.push(root);

  const input = container.querySelector('#session-new-cwd-input');
  check('mount leaves the New-directory input empty despite the current info cwd', input !== null && input.value === '');
  check('panel mount focuses the empty directory picker', input !== null && doc.activeElement === input);
  check('input placeholder demands an explicit directory selection', input?.getAttribute('placeholder') === 'Select a working directory…');
  check('mount renders the project label from get_state cwd', container.querySelector('[data-testid="session-project-value"]')?.textContent === 'proj');
  check('mount renders the working directory (informational only)', container.querySelector('[data-testid="session-cwd-value"]')?.textContent === MOUNT_CWD);
  const mountProbes = backend.commands.slice(0, 3).map((command) => command.type).sort().join(',');
  check('mount probes get_state/get_session_stats/get_fork_messages', mountProbes === 'get_fork_messages,get_session_stats,get_state');
  check('empty input probes zero path_complete commands', pathCompleteCommands(backend.commands).length === 0);

  const newBtn = container.querySelector('#session-new-btn');
  check('New button starts disabled while the input is empty', newBtn !== null && newBtn.hasAttribute('disabled'));
  pressKey(input!, 'Enter');
  await flushWork();
  check('empty Enter emits zero new_session commands', newSessionCommands(backend.commands).length === 0);
  check('empty Enter hands off no lifecycle result', backend.lifecycleResults.length === 0);
  check('empty Enter never toasts and renders no panel status', backend.toasts.length === 0 && container.querySelector('.panel__status') === null);

  await setInputValue(input!, MOUNT_CWD.slice(0, -1));
  check('typed value enables the New button', !newBtn!.hasAttribute('disabled'));
}

/* ------------------------------------------------------------------ *
 * 2. Closed-menu Enter after multi-character typing emits exactly one
 *    new_session carrying the exact typed cwd.
 * ------------------------------------------------------------------ */
{
  const doc = installGlobals();
  const backend = createBackend();
  const { container, root } = await mountPanel(backend, doc);
  roots.push(root);
  await flushDebounce();

  const input = container.querySelector('#session-new-cwd-input')!;
  const typed = '/home/u/proj-x'; // arbitrary sibling absolute directory
  await typeInto(input, typed);
  await flushDebounce();

  const probes = pathCompleteCommands(backend.commands);
  check(
    'every completion probe is a typed prefix (debounce collapse)',
    probes.length >= 1 && probes.every((probe) => typeof probe.path === 'string' && typed.startsWith(String(probe.path))),
  );
  const lastProbe = probes[probes.length - 1];
  check(
    'final probe carries the exact typed value, directory kind, session id',
    lastProbe !== undefined && lastProbe.path === typed && lastProbe.kind === 'directory' && lastProbe.sessionId === PANEL_SESSION_ID,
  );
  check('unanswered probe keeps the menu closed', input.getAttribute('aria-expanded') === 'false' && container.querySelector('[role="listbox"]') === null);

  pressKey(input, 'Enter');
  await flushWork();
  const created = newSessionCommands(backend.commands);
  check('typed cwd Enter emits exactly one new_session', created.length === 1);
  check('new_session carries the exact typed cwd', created.length === 1 && created[0].cwd === typed);
  check('no duplicate lifecycle handoff', backend.lifecycleResults.length === 1);
}

/* ------------------------------------------------------------------ *
 * 3. Open menu: stale generation drops, fresh reply opens, ArrowDown +
 *    Enter accept a directory suggestion without creating; only the
 *    next, menu-closed Enter creates with the accepted path.
 * ------------------------------------------------------------------ */
{
  const doc = installGlobals();
  const backend = createBackend();
  const { container, root } = await mountPanel(backend, doc);
  roots.push(root);
  await flushDebounce();

  const input = container.querySelector('#session-new-cwd-input')!;
  await typeInto(input, '/ho');
  await flushDebounce();
  check('first probe parks a pending reply for /ho', backend.pendingComplete.has('/ho'));
  await typeInto(input, 'm');
  await flushDebounce();
  check('second probe parks a pending reply for /hom', backend.pendingComplete.has('/hom'));

  // The superseded '/ho' generation resolves late: it must never touch the menu.
  backend.pendingComplete.get('/ho')!.resolve(
    replyFor([
      { name: 'stale-a', path: '/ho/stale-a', kind: 'directory' },
      { name: 'stale-b', path: '/ho/stale-b', kind: 'directory' },
    ]),
  );
  await flushWork();
  check(
    'stale reply never opens the menu',
    container.querySelector('[role="listbox"]') === null && input.getAttribute('aria-expanded') === 'false',
  );

  backend.pendingComplete.get('/hom')!.resolve(
    replyFor([
      { name: 'u', path: '/home/u', kind: 'directory' },
      { name: 'x', path: '/home/x', kind: 'directory' },
    ]),
  );
  await flushWork();
  const listbox = container.querySelector('[role="listbox"]');
  check('fresh reply opens the listbox with both entries', listbox !== null && listbox.querySelectorAll('[role="option"]').length === 2);
  check('first entry highlighted by default', input.getAttribute('aria-activedescendant') === 'session-new-cwd-input-option-0');

  pressKey(input, 'ArrowDown');
  await flushWork();
  check('ArrowDown moves the highlight to the second entry', input.getAttribute('aria-activedescendant') === 'session-new-cwd-input-option-1');

  pressKey(input, 'Enter'); // menu open -> accept, never submit
  await flushWork();
  check('open-menu Enter accepts the highlighted directory', input.value === '/home/x/', input.value);
  check('accepting a suggestion creates nothing', newSessionCommands(backend.commands).length === 0);
  // The accepted directory re-probes (drill); its fresh reply re-opens the
  // menu over the drilled path — directories stay open, files close.
  await flushDebounce();
  check('accepting a directory re-probes the drilled path', backend.pendingComplete.has('/home/x/'));
  backend.pendingComplete.get('/home/x/')!.resolve(
    replyFor([{ name: 'sub', path: '/home/x/sub', kind: 'directory' }]),
  );
  await flushWork();
  check('directory suggestions keep the menu open for drilling', input.getAttribute('aria-expanded') === 'true');

  pressKey(input, 'Escape'); // close the menu without touching the value
  await flushWork();
  pressKey(input, 'Enter'); // menu closed -> submit
  await flushWork();
  const created = newSessionCommands(backend.commands);
  check('next Enter after acceptance emits exactly one new_session', created.length === 1);
}
/* ------------------------------------------------------------------ *
 * 4. Whitespace-only input: New stays disabled and Enter emits nothing.
 *    A real session cut REMOUNTS the panel (App key = session id): the
 *    freshly mounted panel starts with an empty, focused input and New
 *    disabled — no response-driven clear, no inherited value.
 * ------------------------------------------------------------------ */
{
  const doc = installGlobals();
  const backend = createBackend();
  const { container, root } = await mountPanel(backend, doc);
  roots.push(root);
  await flushDebounce();

  const input = container.querySelector('#session-new-cwd-input')!;
  await typeInto(input, '   ');
  await flushDebounce();
  const newBtn = container.querySelector('#session-new-btn');
  check('whitespace-only input keeps New disabled', newBtn !== null && newBtn.hasAttribute('disabled'));
  pressKey(input, 'Enter');
  await flushWork();
  check('whitespace-only Enter emits zero new_session commands', newSessionCommands(backend.commands).length === 0);
  check('whitespace-only Enter never toasts and renders no panel status', backend.toasts.length === 0 && container.querySelector('.panel__status') === null);

  // Model the App's session-cut remount: unmount the panel, mount a fresh
  // one (different active session id reported by get_state), and prove the
  // New-directory input starts empty again.
  await act(async () => {
    root.unmount();
  });
  const remountBackend = createBackend();
  remountBackend.sendCommand = async (command: Command) => {
    if (command.type === 'get_state') {
      const state = await backend.sendCommand(command) as Record<string, unknown>;
      return { ...state, sessionId: 'sess-2', cwd: '/home/u/other' };
    }
    return backend.sendCommand(command);
  };
  const remounted = await mountPanel(remountBackend, doc);
  roots.push(remounted.root);
  const freshInput = remounted.container.querySelector('#session-new-cwd-input');
  check('remounted panel starts the New-directory input empty', freshInput !== null && freshInput.value === '');
  check('remounted panel focuses the empty directory picker', freshInput !== null && doc.activeElement === freshInput);
  check('remounted panel keeps New disabled', remounted.container.querySelector('#session-new-btn')?.hasAttribute('disabled') === true);
  check('remounted panel shows the new session id', remounted.container.querySelector('[data-testid="session-cwd-value"]')?.textContent === '/home/u/other');
}


/* ------------------------------------------------------------------ *
 * 5. Rejected (unauthorized) new_session: exactly one error toast via
 *    onToast, ZERO panel status above Current session, current session
 *    unchanged, no lifecycle handoff, busy released.
 * ------------------------------------------------------------------ */
{
  const doc = installGlobals();
  const backend = createBackend();
  const inner = backend.sendCommand;
  backend.sendCommand = async (command: Command) => {
    if (command.type === 'new_session') {
      throw new Error('unauthorized: cwd denied by trust policy');
    }
    return inner(command);
  };
  const { container, root } = await mountPanel(backend, doc);
  roots.push(root);
  await flushDebounce();

  const input = container.querySelector('#session-new-cwd-input')!;
  await typeInto(input, '/home/u/proj-x');
  const nameBefore = container.querySelector('[data-testid="session-name-value"]')?.textContent ?? '';
  const fileBefore = container.querySelector('[data-testid="session-file-value"]')?.getAttribute('title') ?? '';
  pressKey(input, 'Enter');
  await flushWork();
  check('rejected new_session toasts exactly once', backend.toasts.length === 1 && backend.toasts[0].error === true);
  check(
    'rejection toast carries the failure context',
    backend.toasts[0]?.message === 'new session failed: unauthorized: cwd denied by trust policy',
  );
  check('rejected new_session renders zero panel status', container.querySelector('.panel__status') === null);
  check('rejected new_session never hands off a lifecycle result', backend.lifecycleResults.length === 0);
  const nameAfter = container.querySelector('[data-testid="session-name-value"]')?.textContent ?? '';
  const fileAfter = container.querySelector('[data-testid="session-file-value"]')?.getAttribute('title') ?? '';
  check('current session unchanged after rejection', nameAfter === nameBefore && fileAfter === fileBefore);
  check('busy releases after the rejection', container.querySelector('#session-new-btn')?.hasAttribute('disabled') === false);
}

/* ------------------------------------------------------------------ *
 * 6. Immediate user-action transport failure (never connected /
 *    CONNECTING hold): the rejection carries `transport: true` and NO
 *    `toastOwned` — the panel toasts EXACTLY once. The App-owned variant
 *    (transport + toastOwned, mid-session outage) stays silent.
 * ------------------------------------------------------------------ */
{
  const doc = installGlobals();
  const backend = createBackend();
  const inner = backend.sendCommand;
  backend.sendCommand = async (command: Command) => {
    if (command.type === 'new_session') {
      // App.sendCommand's immediate reject shape (App.tsx 'not connected').
      const error: Error & { transport?: boolean } = new Error('not connected');
      error.transport = true;
      throw error;
    }
    return inner(command);
  };
  const { container, root } = await mountPanel(backend, doc);
  roots.push(root);
  await flushDebounce();

  const input = container.querySelector('#session-new-cwd-input')!;
  await typeInto(input, '/home/u/proj-x');
  pressKey(input, 'Enter');
  await flushWork();
  check('immediate user-action transport failure toasts exactly once', backend.toasts.length === 1 && backend.toasts[0].error === true, JSON.stringify(backend.toasts));
  check('immediate transport toast names the failure', backend.toasts[0]?.message === 'new session failed: not connected');
  check('transport rejection renders zero panel status', container.querySelector('.panel__status') === null);
  check('transport rejection never hands off a lifecycle result', backend.lifecycleResults.length === 0);
  check('busy releases after the transport rejection', container.querySelector('#session-new-btn')?.hasAttribute('disabled') === false);
}
{
  const doc = installGlobals();
  const backend = createBackend();
  const inner = backend.sendCommand;
  backend.sendCommand = async (command: Command) => {
    if (command.type === 'new_session') {
      // The App-owned mid-session outage shape: the persistent transport
      // toast already surfaced this reason, stamped toastOwned.
      const error: Error & { transport?: boolean; toastOwned?: boolean } = new Error('connection closed (code 1006)');
      error.transport = true;
      (error as Error & { toastOwned?: boolean }).toastOwned = true;
      throw error;
    }
    return inner(command);
  };
  const { container, root } = await mountPanel(backend, doc);
  roots.push(root);
  await flushDebounce();

  const input = container.querySelector('#session-new-cwd-input')!;
  await typeInto(input, '/home/u/proj-x');
  pressKey(input, 'Enter');
  await flushWork();
  check('App-owned (toastOwned) transport failure is never re-toasted', backend.toasts.length === 0, JSON.stringify(backend.toasts));
  check('App-owned transport rejection renders zero panel status', container.querySelector('.panel__status') === null);
  check('App-owned transport rejection never hands off a lifecycle result', backend.lifecycleResults.length === 0);
  check('busy releases after the App-owned rejection', container.querySelector('#session-new-btn')?.hasAttribute('disabled') === false);
}

/* ------------------------------------------------------------------ *
 * 7. Local validation (rename blank, fork with no selection): exactly
 *    one toast each, zero panel status, zero wire commands.
 * ------------------------------------------------------------------ */
{
  const doc = installGlobals();
  const backend = createBackend();
  const inner = backend.sendCommand;
  // One forkable message with an empty entryId keeps the fork controls
  // enabled while the sticky selection stays empty — the local guard path.
  backend.sendCommand = async (command: Command) => {
    if (command.type === 'get_fork_messages') {
      return { messages: [{ entryId: '', text: 'forkable user turn' }], truncated: false };
    }
    return inner(command);
  };
  const { container, root } = await mountPanel(backend, doc);
  roots.push(root);
  await flushDebounce();

  pressKey(container.querySelector('#session-rename-input')!, 'Enter');
  await flushWork();
  check('blank rename toasts exactly once', backend.toasts.length === 1 && backend.toasts[0].message === 'session name cannot be empty' && backend.toasts[0].error === true);
  check('blank rename renders no panel status', container.querySelector('.panel__status') === null);
  check('blank rename sends no set_session_name', !backend.commands.some((command) => command.type === 'set_session_name'));

  const forkBtn = container.querySelector('#session-fork-btn');
  check('fork button stays enabled with an empty sticky selection', forkBtn !== null && !forkBtn.hasAttribute('disabled'));
  clickInAct(forkBtn!);
  await flushWork();
  check('fork with no selection toasts exactly once more', backend.toasts.length === 2 && backend.toasts[1].message === 'no forkable message selected' && backend.toasts[1].error === true);
  check('fork local validation renders no panel status', container.querySelector('.panel__status') === null);
  check('fork local validation sends no fork command', !backend.commands.some((command) => command.type === 'fork'));
}

/* 8. path_complete unauthorized failure: fail-closed, quiet, recoverable. */
{
  const doc = installGlobals();
  const backend = createBackend();
  const inner = backend.sendCommand;
  const completions: Array<{ sessionId: unknown; error: string }> = [];
  const policy = { fail: 'unknown' as 'unknown' | 'outside' | 'ok' };
  backend.sendCommand = async (command: Command) => {
    if (command.type === 'path_complete') {
      if (policy.fail === 'unknown') completions.push({ sessionId: command.sessionId, error: 'unknown session sess-1' });
      else if (policy.fail === 'outside') completions.push({ sessionId: command.sessionId, error: 'path is outside the workspace' });
      if (policy.fail !== 'ok') throw new Error(completions[completions.length - 1]!.error);
      completions.push({ sessionId: command.sessionId, error: '' });
      return replyFor([{ name: 'proj-x', path: '/home/u/proj-x', kind: 'directory' }]);
    }
    return inner(command);
  };
  const { container, root } = await mountPanel(backend, doc);
  roots.push(root);
  await flushDebounce();

  const input = container.querySelector('#session-new-cwd-input')!;
  await typeInto(input, '/home/u/proj-x');
  await flushDebounce();
  check('unknown-session probe carries the exact owner session id', completions[0]?.sessionId === PANEL_SESSION_ID && completions[0]?.error === 'unknown session sess-1', JSON.stringify(completions));
  check('unknown-session completion keeps the menu closed (fail-closed)', input.getAttribute('aria-expanded') === 'false' && container.querySelector('[role="listbox"]') === null);
  check('unknown-session completion never toasts (panel-owned quiet)', backend.toasts.length === 0, JSON.stringify(backend.toasts));
  check('unknown-session completion renders no panel status', container.querySelector('.panel__status') === null);

  policy.fail = 'outside';
  await typeInto(input, 'y');
  await flushDebounce();
  check(
    'outside-workspace probe is its own typed request with the exact owner id',
    completions[1]?.sessionId === PANEL_SESSION_ID && completions[1]?.error === 'path is outside the workspace',
    JSON.stringify(completions),
  );
  check('outside-workspace completion keeps the menu closed (fail-closed)', input.getAttribute('aria-expanded') === 'false' && container.querySelector('[role="listbox"]') === null);
  check('both rejections toast zero errors and render no panel status', backend.toasts.length === 0 && container.querySelector('.panel__status') === null, JSON.stringify(backend.toasts));

  pressKey(input, 'Enter');
  await flushWork();
  const created = newSessionCommands(backend.commands);
  check('manual entry survives both completion rejections', created.length === 1 && created[0].cwd === '/home/u/proj-xy');
  check('manual entry after rejections toasts zero path_complete errors', backend.toasts.length === 0, JSON.stringify(backend.toasts));

  // Bounded recovery: a later probe whose valid existing workdir reply
  // succeeds still opens the menu — the failures never latch.
  policy.fail = 'ok';
  await typeInto(input, '/');
  await flushDebounce();
  const listbox = container.querySelector('[role="listbox"]');
  check(
    'valid existing workdir reply after failures returns suggestions',
    listbox !== null && listbox.querySelectorAll('[role="option"]').length === 1,
  );
  check('recovered menu never toasts and renders no panel status', backend.toasts.length === 0 && container.querySelector('.panel__status') === null);
}

/* ---- session handoff: the panel action calls App's onSessionHandoff ---- */
{
  const doc = installGlobals();
  const backend = createBackend();
  const handoffCalls: Array<string | null> = [];
  let allowHandoff = true;
  const container0 = doc.createElement('div');
  doc.body.appendChild(container0);
  const root0 = createRoot(container0 as unknown as Element);
  await act(async () => {
    root0.render(
      createElement(SessionPanel, {
        sessionId: PANEL_SESSION_ID,
        sendCommand: backend.sendCommand,
        waitForReady: async () => {},
        refreshState: async () => {},
        onLifecycleResult: backend.onLifecycleResult,
        onToast: backend.onToast,
        onSessionHandoff: (customInstructions: string | null): boolean => {
          if (!allowHandoff) return false;
          handoffCalls.push(customInstructions);
          return true;
        },
        onClose: () => {},
      }),
    );
    await realTick();
  });
  const handoffBtn = container0.querySelector('#session-handoff-btn');
  check('the handoff action renders in the Session panel', handoffBtn !== null);
  clickInAct(handoffBtn!);
  await flushWork();
  check('the handoff click calls App onSessionHandoff (default: no custom instructions)',
    handoffCalls.length === 1 && handoffCalls[0] === null, JSON.stringify(handoffCalls));
  /* In-progress refusal: the callback returns false — the panel must not
   * crash and must not double-invoke. */
  allowHandoff = false;
  clickInAct(handoffBtn!);
  await flushWork();
  check('a refused (in-progress) handoff click does not enqueue a second handoff',
    handoffCalls.length === 1, JSON.stringify(handoffCalls));
  roots.push(root0);
}


await unmountAll();
uninstallGlobals();
console.log(`\nsessionPanelAutocomplete.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);