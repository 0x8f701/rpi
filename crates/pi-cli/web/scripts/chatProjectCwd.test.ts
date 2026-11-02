#!/usr/bin/env node
// Chat project-cwd completion wiring: binding selection (active session or
// first connected mapping when the project does not exist yet), sessionId
// augmentation through the existing registry command socket, exact-binding
// staleness, no-runner manual fallback, and keyboard select/submit through
// the shared PathAutocomplete controller. Component-internal debounce and
// generation behavior live in pathAutocomplete.test.ts.
/* Type-only imports stay static (no runtime evaluation). Every value import
 * is dynamic, below, and runs after installGlobals(): react-dom snapshots
 * `canUseDOM` and `isInputEventSupported` (isEventSupported('input')) at
 * module-init time. With globals absent at init, isInputEventSupported is
 * false and ChangeEventPlugin routes every text-input event through its IE
 * polyfill, which dereferences `activeElementInst` (always null here) on the
 * first keydown. Installing the mini DOM globals first selects the modern
 * getTargetInstForInputOrChangeEvent path; prototype-less nodes keep React's
 * value tracker absent, so updateValueIfChanged reports every change. */
import type { Root } from 'react-dom/client';
import type { ChatSessionBinding, ChatProjectFormController, ChatTransportEpoch } from '../src/chat/ChatApp.tsx';
import type { ProjectRecord, ProjectRunnerMapping, RunnerCommandClient } from '../src/chat/mapping';
import type { RpcResponse } from '../src/types';
import type { RunnerView } from '../src/chat/runners';
import type { PathCompleteRequest, PathCompleteResponse } from '../src/PathAutocomplete';
import type { StorageLike } from '../src/hostToken';

let ran = 0;
const failures: string[] = [];
function check(name: string, condition: boolean, detail?: unknown): void {
  ran += 1;
  if (!condition) failures.push(`${name}${detail !== undefined ? `: ${JSON.stringify(detail)}` : ''}`);
}

// ---- fixtures ----
const project: ProjectRecord = {
  id: 'p1',
  label: 'Product',
  cwd: '/workspace/product',
  createdAt: 1,
  updatedAt: 1,
};

function mappingRow(overrides: Partial<ProjectRunnerMapping> = {}): ProjectRunnerMapping {
  return {
    id: 'm1',
    projectId: project.id,
    runnerId: 'r1',
    operationId: 'op1',
    sourceSessionId: 'sess-1',
    canaryId: 'c1',
    createId: 'cr1',
    sessionName: 'session',
    createdAt: 1,
    updatedAt: 1,
    state: 'active',
    sessionId: 'sess-1',
    sessionPath: '/sessions/sess-1',
    ...overrides,
  } as ProjectRunnerMapping;
}

function runnerRow(state: RunnerView['state']): RunnerView {
  return {
    id: 'r1',
    label: 'Runner',
    origin: 'http://127.0.0.1:9000',
    authority: '127.0.0.1:9000',
    state,
    connectionEpoch: 1,
    error: '',
  } as RunnerView;
}

const connected = runnerRow('connected');
const offline = runnerRow('offline');
const activeMapping = mappingRow();

function bindingOf(
  runners: readonly RunnerView[],
  mappings: readonly ProjectRunnerMapping[],
  active: ChatSessionBinding | null = null,
): ChatSessionBinding | null {
  return chatProjectCompletionBinding(active, [project], runners, mappings);
}

interface CommandCall {
  runnerId: string;
  command: Record<string, unknown>;
  options: { sourceSessionId: string };
}

const completions: PathCompleteResponse = {
  base: '/workspace',
  prefix: 'prod',
  entries: [{ name: 'product', path: '/workspace/product', kind: 'directory' }],
  truncated: false,
};

function okResponse(data: unknown): RpcResponse {
  return { type: 'response', command: 'path_complete', success: true, data };
}

function fakeCommand(respond: () => Promise<RpcResponse> = async () => okResponse(completions)) {
  const calls: CommandCall[] = [];
  return {
    calls,
    command(runnerId: string, command: Record<string, unknown>, options: { sourceSessionId: string }): Promise<RpcResponse> {
      calls.push({ runnerId, command, options });
      return respond();
    },
  };
}

/* Stable dummy iframe constructor for the shim window: ReactDOM's selection
 * restore (getActiveElementDeep) evaluates `element instanceof
 * win.HTMLIFrameElement` on the active element around every controlled
 * update. A missing constructor makes that instanceof itself throw; the
 * dummy's prototype sits outside every mini-DOM node's chain (plain object
 * literals), so the check is a deterministic false and the loop exits
 * immediately — no iframe content is ever read. */
class DummyHTMLIFrameElement {}

const installed = new Set<string>();
function installGlobals(): MiniDoc {
  const doc = makeNode(9, '#document', null as unknown as MiniDoc) as unknown as MiniDoc & MiniNode;
  const html = makeNode(1, 'HTML', doc as unknown as MiniDoc);
  const body = makeNode(1, 'BODY', doc as unknown as MiniDoc);
  html.appendChild(body);
  doc.appendChild(html);
  doc.createElement = (tag: string) => makeNode(1, tag.toUpperCase(), doc as unknown as MiniDoc);
  doc.createTextNode = (text: string) => {
    const textNode = makeNode(3, '#text', doc as unknown as MiniDoc);
    textNode.text = text;
    return textNode;
  };
  doc.createComment = (text: string) => {
    const comment = makeNode(8, '#comment', doc as unknown as MiniDoc);
    comment.text = text;
    return comment;
  };
  doc.createDocumentFragment = () => makeNode(11, '#document-fragment', doc as unknown as MiniDoc);
  doc.getElementById = (id: string) => doc.querySelector(`#${id}`);
  doc.body = body;
  doc.activeElement = body;
  /* 'oninput' must exist as an own property: see the MiniDoc.oninput note. */
  doc.oninput = null;
  /* ReactDOM's client render/unmount reads `window.event` for event-priority
   * lane selection (getCurrentEventPriority) before/without any native event
   * flowing — the mini-DOM drives React's delegated listeners directly; a
   * bare `event` slot (undefined outside any real dispatch) is the whole
   * surface createRoot needs there. The dummy HTMLIFrameElement covers
   * getActiveElementDeep's `element instanceof win.HTMLIFrameElement` — see
   * the module-scope note above. */
  const windowObj: {
    document: MiniDoc;
    event: Record<string, unknown> | undefined;
    HTMLIFrameElement: typeof DummyHTMLIFrameElement;
  } = {
    document: doc,
    event: undefined,
    HTMLIFrameElement: DummyHTMLIFrameElement,
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
  return doc as unknown as MiniDoc;
}
function uninstallGlobals(): void {
  for (const key of installed) delete (globalThis as Record<string, unknown>)[key];
  installed.clear();
}

/* Globals must be live before react-dom initializes (see the import note
 * above). Each form block below re-installs a fresh document; react-dom
 * only snapshots canUseDOM/isInputEventSupported at module init. Static
 * value imports hoist above installGlobals() and would freeze that snapshot
 * with globals absent — that ordering is the reason these imports are
 * dynamic. */
/* Captured before the first installGlobals() below: that call replaces
 * globalThis.setTimeout with the fake clock, and this binding must stay the
 * platform timer flushWork/realTick await on (see the fake-clock note). */
const realSetTimeout = globalThis.setTimeout.bind(globalThis);

installGlobals();
const { act, createElement, useState, Fragment } = await import('react');
const { createRoot } = await import('react-dom/client');
const { renderToStaticMarkup } = await import('react-dom/server.browser');
const {
  ChatProjectForm,
  chatPathCompletionResponse,
  chatProjectCompletionBinding,
  createChatProjectFormController,
  createChatProjectPathComplete,
  mountChatTransports,
} = await import('../src/chat/ChatApp.tsx');
const { useRef } = await import('react');
const { ChatMappingStore } = await import('../src/chat/mapping');
const { safeText } = await import('../src/redact');
const {
  PathAutocomplete,
  PATH_COMPLETE_LIMIT,
  createPathCompletionController,
  joinCompletion,
  nextHighlight,
  pathKeyAction,
  PATH_COMPLETE_DEBOUNCE_MS,
} = await import('../src/PathAutocomplete');

// ---- binding selection ----
{
  const binding = bindingOf([connected], [activeMapping]);
  check('active session binding is preferred', binding?.mapping.id === 'm1' && binding?.runner.id === 'r1');

  const fallback = bindingOf([connected], [activeMapping], null);
  check('first active mapping with a connected runner is the fallback', fallback?.mapping.id === 'm1');

  const skipped = bindingOf(
    [connected],
    [mappingRow({ id: 'm0', runnerId: 'r2', state: 'active', sessionId: 'sess-0', sourceSessionId: 'sess-0' }), activeMapping],
    null,
  );
  check('offline-runner mapping is skipped', skipped?.mapping.id === 'm1');

  check('provisioning mapping is not completion authority',
    bindingOf([connected], [mappingRow({ state: 'provisioning' })], null) === null);
  check('no connected runner yields no binding', bindingOf([offline], [activeMapping], null) === null);
}

// ---- sendComplete wiring ----
{
  const fake = fakeCommand();
  const bindingRef: { current: ChatSessionBinding | null } = { current: null };
  const send = createChatProjectPathComplete({ bindingRef, command: fake.command });

  const none = await send({ type: 'path_complete', path: '/work', kind: 'directory', limit: 100 });
  check('no runner resolves to no suggestions without sending', none === null && fake.calls.length === 0);

  bindingRef.current = bindingOf([connected], [activeMapping], null);
  const result = await send({ type: 'path_complete', path: '/work', kind: 'directory', limit: 100 });
  check('suggestions resolve through the registry command', result !== null && result.entries[0]?.path === '/workspace/product', result);
  const call = fake.calls[0];
  check('exactly one command was sent', fake.calls.length === 1 && call !== undefined);
  if (call) {
    check('command rides the binding runner', call.runnerId === 'r1');
    check('command carries path_complete shape with the binding session',
      call.command.type === 'path_complete'
      && call.command.path === '/work'
      && call.command.kind === 'directory'
      && call.command.limit === 100
      && call.command.sessionId === 'sess-1');
    check('command options carry the source session', call.options.sourceSessionId === 'sess-1');
  }
}

{
  // Stale exact binding: the binding changed while the command was in flight.
  const calls: CommandCall[] = [];
  let release: (value: RpcResponse) => void = () => {};
  const gated = new Promise<RpcResponse>((resolve) => { release = resolve; });
  const bindingRef: { current: ChatSessionBinding | null } = {
    current: bindingOf([connected], [activeMapping], null),
  };
  const send = createChatProjectPathComplete({
    bindingRef,
    command: (runnerId, command, options) => {
      calls.push({ runnerId, command, options });
      return gated;
    },
  });
  const inFlight = send({ type: 'path_complete', path: '/work', kind: 'directory', limit: 100 });
  bindingRef.current = {
    project,
    runner: connected,
    mapping: mappingRow({ id: 'm2', runnerId: 'r1', sessionId: 'sess-2', sourceSessionId: 'sess-2' }),
  };
  release(okResponse(completions));
  const stale = await inFlight;
  check('response for a superseded binding is dropped', stale === null && calls.length === 1);
}

{
  // End-to-end through the shared controller: debounced request reaches the
  // chat sendComplete (sessionId augmented), suggestions surface, and the
  // keyboard contract accepts a directory with a trailing slash.
  const fake = fakeCommand();
  const bindingRef: { current: ChatSessionBinding | null } = {
    current: bindingOf([connected], [activeMapping], null),
  };
  const send = createChatProjectPathComplete({ bindingRef, command: fake.command });
  let firePending: (() => void) | null = null;
  const timers = {
    delay(fn: () => void, _ms: number): () => void {
      firePending = fn;
      return () => { firePending = null; };
    },
  };
  let shown: PathCompleteResponse['entries'] | null = null;
  let resolveShown: (entries: PathCompleteResponse['entries']) => void = () => {};
  const shownPromise = new Promise<PathCompleteResponse['entries']>((resolve) => { resolveShown = resolve; });
  const controller = createPathCompletionController({
    send,
    show: (menu) => {
      shown = menu.entries;
      resolveShown(menu.entries);
    },
    hide: () => { shown = null; },
  }, timers);

  controller.request('/work', 'directory');
  firePending?.();
  const entries = await shownPromise;
  check('controller reaches the chat sendComplete with sessionId augmented',
    fake.calls[0]?.command.sessionId === 'sess-1' && fake.calls[0]?.command.kind === 'directory');
  check('connected suggestions surface as menu entries', entries[0]?.path === '/workspace/product', entries);

  // Keyboard select through the shared contract: menu open -> Enter accepts.
  const highlight = nextHighlight(-1, 1, entries.length);
  check('ArrowDown highlights the first entry', highlight === 0);
  check('Enter with the menu open accepts instead of submitting', pathKeyAction('Enter', true).type === 'accept');
  check('accept joins the directory with a trailing slash', joinCompletion(entries[0]) === '/workspace/product/');
  check('Enter with the menu closed submits the form', pathKeyAction('Enter', false).type === 'submit');
  controller.dispose();
}

{
  // Failures and malformed payloads degrade to "no suggestions".
  const failing = fakeCommand(async () => {
    throw new Error('Runner is not connected.');
  });
  const failingRef: { current: ChatSessionBinding | null } = {
    current: bindingOf([connected], [activeMapping], null),
  };
  const failed = await createChatProjectPathComplete({
    bindingRef: failingRef,
    command: failing.command,
  })({ type: 'path_complete', path: '/work', kind: 'directory', limit: 100 });
  check('transport failure yields no suggestions', failed === null && failing.calls.length === 1);

  // A well-formed body wrapped in a failed envelope must never reach the
  // autocomplete surface — the envelope gate fires before the body is read.
  const envelope = fakeCommand(async () => ({
    ...okResponse(completions),
    success: false,
    error: 'Runner is not connected.',
  }));
  const envelopeFailed = await createChatProjectPathComplete({
    bindingRef: failingRef,
    command: envelope.command,
  })({ type: 'path_complete', path: '/work', kind: 'directory', limit: 100 });
  check('failed response envelope yields no suggestions', envelopeFailed === null && envelope.calls.length === 1);

  for (const broken of [
    null,
    {},
    { base: 1, prefix: '', entries: [], truncated: false },
    { base: '', prefix: '', entries: [{ name: 'x', path: 2, kind: 'directory' }], truncated: false },
    { base: '', prefix: '', entries: [], truncated: 'no' },
  ]) {
    check(`malformed response rejected: ${JSON.stringify(broken)?.slice(0, 40)}`, chatPathCompletionResponse(broken) === null);
  }
  const valid = chatPathCompletionResponse({
    base: '/workspace',
    prefix: '',
    entries: [
      { name: 'product', path: '/workspace/product', kind: 'directory' },
      { name: 'notes.md', path: '/workspace/notes.md', kind: 'file' },
    ],
    truncated: true,
  });
  check('valid response passes through validated',
    valid !== null && valid.entries.length === 2 && valid.entries[1]?.kind === 'file' && valid.truncated);
  const oversized = chatPathCompletionResponse({
    base: '/workspace',
    prefix: '',
    entries: Array.from({ length: PATH_COMPLETE_LIMIT + 1 }, (_, index) => ({
      name: `entry-${index}`,
      path: `/workspace/entry-${index}`,
      kind: 'file',
    })),
    truncated: false,
  });
  check('oversized response is capped to the limit and flagged truncated',
    oversized !== null
    && oversized.entries.length === PATH_COMPLETE_LIMIT
    && oversized.truncated
    && oversized.entries[0]?.path === '/workspace/entry-0');
}

// ---- transport lifecycle (mountChatTransports, no DOM needed) ----
{
  // StrictMode-safe teardown: setup→cleanup→setup on the same epoch ref must
  // not stop the stores between setups (disposed transports are not
  // restartable), pagehide stops synchronously and invalidates a pending
  // deferred stop, and a real unmount (cleanup with no follow-up setup)
  // stops once the deferred microtask runs.
  const epoch: ChatTransportEpoch = { current: 0 };
  const started: number[] = [];
  const stopped: number[] = [];
  const pagehideListeners: Array<() => void> = [];
  const win = {
    addEventListener(_type: string, listener: () => void): void {
      pagehideListeners.push(listener);
    },
    removeEventListener(_type: string, listener: () => void): void {
      const at = pagehideListeners.indexOf(listener);
      if (at >= 0) pagehideListeners.splice(at, 1);
    },
  };
  const start = () => { started.push(started.length); };
  const stop = () => { stopped.push(stopped.length); };
  const flushMicrotasks = () => new Promise<void>((resolve) => queueMicrotask(resolve));

  // StrictMode double-mount: setup -> cleanup -> setup on the same epoch
  // ref. The cleanup's deferred stop must be cancelled by the follow-up
  // setup instead of tearing the shared stores down between the setups.
  const cleanupOne = mountChatTransports(epoch, start, stop, win);
  check('setup starts the transports', started.length === 1 && stopped.length === 0);
  cleanupOne();
  const cleanupTwo = mountChatTransports(epoch, start, stop, win);
  await flushMicrotasks();
  check('StrictMode setup→cleanup→setup keeps the stores alive',
    started.length === 2 && stopped.length === 0);
  cleanupTwo();
  await flushMicrotasks();
  check('real unmount with no follow-up setup stops the stores', stopped.length === 1);

  // Pagehide path on a fresh lifecycle.
  const pagehide = mountChatTransports(epoch, start, stop, win);
  check('setup registers exactly one pagehide listener', pagehideListeners.length === 1);
  pagehideListeners[0]?.();
  check('pagehide stops synchronously', stopped.length === 2);
  pagehide();
  await flushMicrotasks();
  check('cleanup after pagehide does not stop again', stopped.length === 2);
  check('cleanup removed the pagehide listener', pagehideListeners.length === 0);
}

// ---- rendered chat surface (react-dom/server on the real component) ----
{
  // Effects never run in SSR, so the menu stays closed: this asserts the
  // closed-state form path — required, aria identity, and the manual
  // controlled value the addProject submit consumes.
  const markup = renderToStaticMarkup(createElement(PathAutocomplete, {
    id: 'chat-project-cwd',
    value: '/workspace/manual',
    onChange: () => {},
    sendComplete: async () => null,
    kind: 'directory',
    placeholder: '/workspace/project',
    ariaLabel: 'Absolute directory',
    required: true,
  }));
  check('rendered input carries the chat id and combobox role',
    markup.includes('id="chat-project-cwd"') && markup.includes('role="combobox"'), markup);
  check('rendered input is required', markup.includes('required=""'));
  check('rendered input keeps the manual value through the controlled surface',
    markup.includes('value="/workspace/manual"'));
  check('rendered aria label and placeholder preserved',
    markup.includes('aria-label="Absolute directory"') && markup.includes('placeholder="/workspace/project"'));
  check('closed menu renders no listbox and reports collapsed state',
    markup.includes('aria-expanded="false"') && !markup.includes('role="listbox"'));
  const manualMarkup = renderToStaticMarkup(createElement(PathAutocomplete, {
    id: 'chat-project-cwd',
    value: '',
    onChange: () => {},
    sendComplete: async () => null,
    kind: 'directory',
    ariaLabel: 'Absolute directory',
    required: true,
  }));
  check('empty manual input renders with no suggestions', manualMarkup.includes('value=""') && !manualMarkup.includes('role="listbox"'));
}

// ---- rendered ChatProjectForm (createRoot on the real form) ----

/* Deterministic clock: the completion controller reads setTimeout at delay()
 * time, so patching the global routes every debounce into a queue this test
 * fires explicitly (same pattern as sessionPanelAutocomplete.test.ts). act()
 * itself only uses microtasks, so nothing else observes the patch. The real
 * timer binding is captured near the top, before the first installGlobals()
 * replaces globalThis.setTimeout — capturing it here would alias the fake
 * clock and leave every realTick awaiting a timer nothing ever fires. */
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

/* Mini DOM — the surface React 18 client rendering + the project form use:
 * tree ops, attributes, delegated event dispatch (capture -> target ->
 * bubble). Prototype-less nodes make React's value tracker bail, so every
 * dispatched input event reports a change (deterministic typing). */
type Listener = (event: Record<string, unknown>) => void;

interface MiniDoc {
  nodeType: number;
  nodeName: string;
  /* Real documents expose on* IDL slots; React's isEventSupported('input')
   * probe at module init checks `'oninput' in document` to decide whether
   * input events are supported (without it ChangeEventPlugin falls back to
   * its IE polyfill, which crashes on keydown here). */
  oninput: null;
  body: MiniNode;
  activeElement: MiniNode | null;
  createElement(tag: string): MiniNode;
  createTextNode(text: string): MiniNode;
  createComment(text: string): MiniNode;
  createDocumentFragment(): MiniNode;
  getElementById(id: string): MiniNode | null;
  querySelector(selector: string): MiniNode | null;
}

interface MiniNode {
  nodeType: number;
  nodeName: string;
  tagName: string | undefined;
  namespaceURI: string | null;
  /* React's getVendorPrefixedEventName probes `document.createElement('div')
   * .style` at module init (canUseDOM is now true); every element carries a
   * plain style object so the probe and style-prop writes never hit
   * undefined. */
  style: Record<string, unknown>;
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
    style: {},
    ownerDocument: owner,
    parentNode: null,
    childNodes: [],
    firstChild: null,
    lastChild: null,
    nextSibling: null,
    previousSibling: null,
    attributes: new Map(),
    listeners: new Map(),
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
      // Real inputs default their IDL type to 'text'; react-dom's
      // ChangeEventPlugin uses isTextInputElement(type) to route input
      // events, so a typeless input must still read as a text field.
      if (nodeType === 1 && nodeName === 'INPUT') return node.getAttribute('type') ?? 'text';
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
    // phases on the container) to receive input/keydown/submit dispatches.
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
    // autoFocus lands here on mount; the modal label input uses it.
    focus() {
      owner.activeElement = node;
    },
  };
  return node;
}

/* ------------------------------------------------------------------ *
 * Chat project-form harness: ChatProjectForm under a stateful parent so
 * controlled typing flows exactly like ChatApp's wiring, and submission
 * runs through the EXPORTED PRODUCTION createChatProjectFormController
 * bound to a REAL ChatMappingStore (no surrogate handler, no fake-pass
 * report): store rejections land raw in the error slot and the form's
 * render-time safeText is the only sanitization pass. The backend is the
 * chat cwd completion surface: probes are recorded and parked, or
 * (no-runner mode) answered with null so manual input is the only path.
 * ------------------------------------------------------------------ */
interface ChatFormBackend {
  probes: PathCompleteRequest[];
  pending: Map<string, (value: PathCompleteResponse | null) => void>;
  sendComplete(request: PathCompleteRequest): Promise<PathCompleteResponse | null>;
}

function createChatFormBackend(noRunner: boolean): ChatFormBackend {
  const probes: PathCompleteRequest[] = [];
  const pending = new Map<string, (value: PathCompleteResponse | null) => void>();
  const sendComplete = async (request: PathCompleteRequest): Promise<PathCompleteResponse | null> => {
    if (noRunner) return null;
    probes.push({ ...request });
    const { promise, resolve } = Promise.withResolvers<PathCompleteResponse | null>();
    pending.set(request.path, resolve);
    return promise;
  };
  return { probes, pending, sendComplete };
}

let harnessBytes = 0;
function harnessRandomBytes(length: number): Uint8Array {
  harnessBytes += 1;
  return new Uint8Array(length).fill(harnessBytes);
}

/** In-memory StorageLike for the real ChatMappingStore under the form. */
function harnessStorage(): StorageLike {
  const values = new Map<string, string>();
  return {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => {
      values.set(key, value);
    },
    removeItem: (key: string) => {
      values.delete(key);
    },
  };
}

/** Runner-command guard: the project form's addProject flow is purely
 *  local — any runner command reaching the store through the form is a
 *  wiring bug and fails the block. */
class GuardRunnerClient implements RunnerCommandClient {
  calls = 0;
  snapshot(): RunnerView[] {
    return [];
  }
  async command(): Promise<RpcResponse> {
    this.calls += 1;
    throw new Error('project form submit must not send runner commands');
  }
}

function harnessMappingStore(guard?: GuardRunnerClient): ChatMappingStore {
  return new ChatMappingStore({
    storage: harnessStorage(),
    runners: guard ?? new GuardRunnerClient(),
    withExclusiveLock: (callback: () => Promise<unknown>) => callback(),
    randomBytes: harnessRandomBytes,
  });
}


function ChatFormHarness(props: {
  sendComplete: (request: PathCompleteRequest) => Promise<PathCompleteResponse | null>;
  mappingStore: ChatMappingStore;
  addProjectImpl?: (label: string, cwd: string) => Promise<ProjectRecord>;
  accepted: Array<{ label: string; cwd: string }>;
}) {
  /* Production wiring: ChatProjectForm + the exported controller, exactly
   * like ChatApp — one busy gate, the store rejection stored RAW in the
   * error slot, and accepted projects recorded from onAccepted. */
  const [label, setLabel] = useState('');
  const [cwd, setCwd] = useState('');
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const controllerRef = useRef<ChatProjectFormController | null>(null);
  if (controllerRef.current === null) {
    controllerRef.current = createChatProjectFormController({
      addProject: props.addProjectImpl ?? ((nextLabel, nextCwd) => props.mappingStore.addProject(nextLabel, nextCwd)),
      onBusy: setBusy,
      onError: setError,
      onAccepted: (project) => {
        props.accepted.push({ label: project.label, cwd: project.cwd });
        setLabel('');
        setCwd('');
      },
    });
  }
  return createElement(Fragment, null,
    createElement(ChatProjectForm, {
      projectLabel: label,
      setProjectLabel: (next: string) => setLabel(next),
      projectCwd: cwd,
      setProjectCwd: (next: string) => setCwd(next),
      addProject: (event) => {
        void controllerRef.current!.submit(event, { label, cwd });
      },
      closeProjectForm: () => {},
      sendProjectPathComplete: props.sendComplete,
      isProjectBusy: busy,
      storageError: '',
    }),
    error !== ''
      ? createElement('p', { className: 'chat-form-error chat-session-toast', role: 'alert' }, safeText(error))
      : null,
  );
}

interface MountedForm {
  container: MiniNode;
  root: Root;
  form: MiniNode;
  labelInput: MiniNode;
  cwdInput: MiniNode;
  submissions: Array<{ label: string; cwd: string }>;
}

async function mountChatForm(
  doc: MiniDoc,
  backend: ChatFormBackend,
  mappingStore: ChatMappingStore,
  accepted: Array<{ label: string; cwd: string }>,
  addProjectImpl?: (label: string, cwd: string) => Promise<ProjectRecord>,
): Promise<MountedForm> {
  const container = doc.createElement('div');
  doc.body.appendChild(container);
  const root = createRoot(container as unknown as Element);
  await act(async () => {
    root.render(createElement(ChatFormHarness, {
      sendComplete: backend.sendComplete,
      mappingStore,
      addProjectImpl,
      accepted,
    }));
  });
  const form = container.querySelector('form');
  const labelInput = container.querySelector('#chat-project-label');
  const cwdInput = container.querySelector('#chat-project-cwd');
  if (form === null || labelInput === null || cwdInput === null) {
    throw new Error('project form did not mount');
  }
  return { container, root, form, labelInput, cwdInput, submissions: accepted };
}

async function flushDebounce(): Promise<void> {
  await act(async () => {
    advanceClock(PATH_COMPLETE_DEBOUNCE_MS + 1);
    await realTick();
  });
}

async function flushWork(): Promise<void> {
  await act(async () => {
    await realTick();
  });
}

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

/** Dispatch a keydown; returns whether any handler prevented the default,
 *  i.e. whether a conforming browser would still perform its own action
 *  (implicit form submission for an un-prevented Enter). */
function pressKey(input: MiniNode, key: string): boolean {
  let prevented = false;
  input.dispatchEvent({
    type: 'keydown',
    key,
    target: input,
    preventDefault() {
      prevented = true;
    },
    stopPropagation() {},
    stopImmediatePropagation() {},
  });
  return prevented;
}

/** Constraint validation + implicit submission stand-in: a conforming
 *  browser only fires `submit` when every required input has a value. */
function submitFormIfValid(form: MiniNode): boolean {
  for (const field of form.querySelectorAll('input')) {
    if (field.hasAttribute('required') && field.value === '') return false;
  }
  form.dispatchEvent({ type: 'submit', target: form, preventDefault() {}, stopPropagation() {} });
  return true;
}

/** A well-formed path_complete reply (PathCompleteResponse wire shape). */
function replyFor(entries: Array<{ name: string; path: string; kind: 'directory' | 'file' }>): PathCompleteResponse {
  return { base: '/', prefix: '', entries: entries.map((entry) => ({ ...entry })), truncated: false };
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

{
  // Required-empty block + no-runner manual submit: with no completion
  // backend the menu must never open, manual typing survives, and the
  // form's required gate + submit path behave like a conforming browser.
  const doc = installGlobals();
  const backend = createChatFormBackend(true);
  const mappingStore = harnessMappingStore();
  const accepted: Array<{ label: string; cwd: string }> = [];
  const mounted = await mountChatForm(doc, backend, mappingStore, accepted);
  roots.push(mounted.root);

  const dialog = mounted.container.querySelector('#chat-project-form');
  check('form renders as the project modal dialog', dialog !== null && dialog.getAttribute('role') === 'dialog');
  check('label input is required', mounted.labelInput.hasAttribute('required'));
  check('cwd autocomplete input is a required collapsed combobox',
    mounted.cwdInput.getAttribute('role') === 'combobox' &&
    mounted.cwdInput.hasAttribute('required') &&
    mounted.cwdInput.getAttribute('aria-expanded') === 'false');
  check('form owns the submit button', mounted.form.querySelector('#chat-project-add') !== null);

  check('empty required fields block the submit', submitFormIfValid(mounted.form) === false);
  check('blocked empty submit reaches no handler', mounted.submissions.length === 0);

  await typeInto(mounted.labelInput, 'Manual Proj');
  check('label typing is controlled', mounted.labelInput.value === 'Manual Proj');
  await typeInto(mounted.cwdInput, '/workspace/manual');
  await flushDebounce(); // no runner -> null reply, menu must stay closed
  check('no-runner cwd never opens the menu',
    mounted.cwdInput.getAttribute('aria-expanded') === 'false' &&
    mounted.container.querySelector('[role="listbox"]') === null);

  const prevented = pressKey(mounted.cwdInput, 'Enter');
  check('menu-closed Enter is not swallowed by the autocomplete', prevented === false);
  // Browser implicit submission follows the un-prevented Enter.
  check('manual Enter submits the form', submitFormIfValid(mounted.form) === true);
  await flushWork();
  check('manual submit carries the typed label + cwd',
    mounted.submissions.length === 1 &&
    mounted.submissions[0].label === 'Manual Proj' &&
    mounted.submissions[0].cwd === '/workspace/manual');
}

{
  // Live completion surface: controlled multi-char typing probes the
  // backend, open-menu Enter accepts without submitting, and the next
  // menu-closed Enter submits the accepted directory through addProject.
  const doc = installGlobals();
  const backend = createChatFormBackend(false);
  const mounted = await mountChatForm(doc, backend, harnessMappingStore(), []);

  await typeInto(mounted.labelInput, 'Dir Project');
  await typeInto(mounted.cwdInput, '/work/sp');
  await flushDebounce();

  const probes = backend.probes;
  check(
    'every completion probe is a typed prefix (debounce collapse)',
    probes.length >= 1 && probes.every((probe) => '/work/sp'.startsWith(probe.path)),
  );
  const lastProbe = probes[probes.length - 1];
  check(
    'final probe carries the exact typed value and directory kind',
    lastProbe !== undefined && lastProbe.path === '/work/sp' && lastProbe.kind === 'directory',
  );
  check('unanswered probe keeps the menu closed',
    mounted.cwdInput.getAttribute('aria-expanded') === 'false' &&
    mounted.container.querySelector('[role="listbox"]') === null);

  backend.pending.get('/work/sp')!(
    replyFor([{ name: 'workspace', path: '/workspace', kind: 'directory' }]),
  );
  await flushWork();
  check('fresh reply opens the listbox',
    mounted.container.querySelector('[role="listbox"]') !== null &&
    mounted.cwdInput.getAttribute('aria-expanded') === 'true');

  const openPrevented = pressKey(mounted.cwdInput, 'Enter');
  await flushWork();
  check('open-menu Enter is prevented (accept, never submit)', openPrevented === true);
  check('open-menu Enter accepts the highlighted directory', mounted.cwdInput.value === '/workspace/');
  check('accepting a suggestion submits nothing', mounted.submissions.length === 0);

  pressKey(mounted.cwdInput, 'Escape'); // close the menu without touching the value
  await flushWork();
  check('Escape closes the menu',
    mounted.cwdInput.getAttribute('aria-expanded') === 'false' &&
    mounted.container.querySelector('[role="listbox"]') === null);

  const closedPrevented = pressKey(mounted.cwdInput, 'Enter');
  check('menu-closed Enter passes through to the form', closedPrevented === false);
  check('menu-closed Enter submits the accepted directory', submitFormIfValid(mounted.form) === true);
  await flushWork();
  check('submission carries the accepted cwd and typed label',
    mounted.submissions.length === 1 &&
    mounted.submissions[0].cwd === '/workspace/' &&
    mounted.submissions[0].label === 'Dir Project');
}

{
  // Production rejection surface through the REAL store + controller: the
  // store's local validation rejects fail closed with the one actionable
  // message (no runner command is ever sent), the message lands RAW in the
  // error slot, and the form's render-time safeText is the only pass.
  const doc = installGlobals();
  const backend = createChatFormBackend(true);
  const guardClient = new GuardRunnerClient();
  const mappingStore = harnessMappingStore(guardClient);
  const accepted: Array<{ label: string; cwd: string }> = [];
  const mounted = await mountChatForm(doc, backend, mappingStore, accepted);
  roots.push(mounted.root);

  for (const [label, cwd, message] of [
    ['Rejected Proj', 'workspace/relative', 'Project directory must be absolute.'],
    ['   ', '/workspace/valid', 'Project label is required.'],
  ] as const) {
    await setInputValue(mounted.labelInput, label);
    await setInputValue(mounted.cwdInput, cwd);
    check(`${cwd}: submit passes the typed value to the controller`, submitFormIfValid(mounted.form) === true);
    await flushWork();
    check(`${cwd}: exactly one component-owned alert renders`,
      mounted.container.querySelectorAll('.chat-form-error').length === 1, String(mounted.container.querySelectorAll('.chat-form-error').length));
    const alert = mounted.container.querySelector('.chat-form-error');
    check(`${cwd}: alert carries the exact bounded store message`,
      alert?.getAttribute('role') === 'alert' && alert?.textContent === message, alert?.textContent ?? '');
    check(`${cwd}: typed values survive the rejection`,
      mounted.labelInput.value === label && mounted.cwdInput.value === cwd);
    check(`${cwd}: submit button is re-enabled after the rejection`,
      mounted.form.querySelector('#chat-project-add')?.hasAttribute('disabled') === false);
  }
  check('local rejections persisted no project', mappingStore.snapshot().projects.length === 0);
  check('local rejections sent no runner command', guardClient.calls === 0);
}

{
  // Duplicate-directory rejection: the second submit over the same cwd is
  // rejected by the real store with the bounded duplicate literal, the
  // first project survives untouched, and no second project is persisted.
  const doc = installGlobals();
  const backend = createChatFormBackend(true);
  const mappingStore = harnessMappingStore();
  const accepted: Array<{ label: string; cwd: string }> = [];
  const mounted = await mountChatForm(doc, backend, mappingStore, accepted);
  roots.push(mounted.root);

  await typeInto(mounted.labelInput, 'First Dup');
  await setInputValue(mounted.cwdInput, '/workspace/duplicate');
  check('first duplicate-cwd submit passes', submitFormIfValid(mounted.form) === true);
  await flushWork();
  check('first duplicate-cwd submit is accepted',
    accepted.length === 1 && accepted[0].label === 'First Dup' && accepted[0].cwd === '/workspace/duplicate');
  check('accepted submit clears the typed label', mounted.labelInput.value === '');

  await typeInto(mounted.labelInput, 'Second Dup');
  await setInputValue(mounted.cwdInput, '/workspace/duplicate');
  check('duplicate-cwd submit reaches the store', submitFormIfValid(mounted.form) === true);
  await flushWork();
  const alert = mounted.container.querySelector('.chat-form-error');
  check('duplicate cwd rejects with the bounded message',
    alert?.textContent === 'A project already uses this directory.', alert?.textContent ?? '');
  check('duplicate-cwd rejection keeps the accepted project', mappingStore.snapshot().projects.length === 1);
  check('duplicate-cwd rejection releases the busy gate',
    mounted.form.querySelector('#chat-project-add')?.hasAttribute('disabled') === false);
}

{
  // Production busy gate: the store's addProject is parked behind a gate,
  // the first submit disables the submit button and clears the previous
  // error, a second submit during the flight is a NO-OP (one store call,
  // one accepted project), and release clears the busy state and the error
  // slot exactly like ChatApp's wiring.
  const doc = installGlobals();
  const backend = createChatFormBackend(true);
  const mappingStore = harnessMappingStore();
  const accepted: Array<{ label: string; cwd: string }> = [];
  const pending: Array<(project: ProjectRecord) => void> = [];
  let gateCalls = 0;
  const mounted = await mountChatForm(doc, backend, mappingStore, accepted, async (label, cwd) => {
    gateCalls += 1;
    return await new Promise<ProjectRecord>((resolve) => pending.push(resolve));
  });
  roots.push(mounted.root);

  await typeInto(mounted.labelInput, 'Gated Proj');
  await setInputValue(mounted.cwdInput, '/workspace/gated');
  check('gated submit starts the store flow', submitFormIfValid(mounted.form) === true);
  await flushWork();
  check('in-flight submit disables the submit button',
    mounted.form.querySelector('#chat-project-add')?.hasAttribute('disabled') === true);
  check('no error renders while the store is parked', mounted.container.querySelector('.chat-form-error') === null);

  await setInputValue(mounted.labelInput, 'Gated Proj 2');
  await setInputValue(mounted.cwdInput, '/workspace/gated-2');
  check('second submit during flight is a no-op', submitFormIfValid(mounted.form) === true);
  await flushWork();
  check('busy gate admits exactly one submission', gateCalls === 1, String(gateCalls));
  check('second submit rendered no error', mounted.container.querySelector('.chat-form-error') === null);

  const resolveGate = pending[0];
  if (!resolveGate) throw new Error('gate was never parked');
  resolveGate({
    id: 'p-gated',
    label: 'Gated Proj',
    cwd: '/workspace/gated',
    createdAt: 1,
    updatedAt: 1,
  });
  await flushWork();
  check('released submit records exactly one accepted project',
    accepted.length === 1 && accepted[0].label === 'Gated Proj' && accepted[0].cwd === '/workspace/gated', JSON.stringify(accepted));
  check('accepted submit clears the typed values',
    mounted.labelInput.value === '' && mounted.cwdInput.value === '');
  check('released submit re-enables the button',
    mounted.form.querySelector('#chat-project-add')?.hasAttribute('disabled') === false);
}

{
  // Raw storage boundary: the store rejection is stored RAW in the error
  // slot and the render's safeText is the ONLY sanitization pass — a
  // secret-shaped bounded message reaches textContent redacted exactly
  // once (never double-redacted), and the typed values survive.
  const doc = installGlobals();
  const backend = createChatFormBackend(true);
  const mappingStore = harnessMappingStore();
  const accepted: Array<{ label: string; cwd: string }> = [];
  let releaseAdd: (error?: Error) => void = () => {};
  const gate = new Promise<ProjectRecord>((resolve, reject) => {
    releaseAdd = (error) => {
      if (error) reject(error);
      else resolve({
        id: 'p-reject',
        label: 'x',
        cwd: '/workspace/x',
        createdAt: 1,
        updatedAt: 1,
      });
    };
  });
  const mounted = await mountChatForm(doc, backend, mappingStore, accepted, () => gate);
  roots.push(mounted.root);

  await typeInto(mounted.labelInput, 'Raw Slot');
  await setInputValue(mounted.cwdInput, '/workspace/raw');
  check('parked submit passes constraint validation', submitFormIfValid(mounted.form) === true);
  await flushWork();
  check('parked store holds the busy gate', mounted.form.querySelector('#chat-project-add')?.hasAttribute('disabled') === true);
  releaseAdd(new Error('token=supersecret123 value'));
  await flushWork();
  const alert = mounted.container.querySelector('.chat-form-error');
  check('raw store rejection is redacted exactly once at render',
    alert?.textContent === '[REDACTED] value', alert?.textContent ?? '');
  check('parked rejection releases the busy gate',
    mounted.form.querySelector('#chat-project-add')?.hasAttribute('disabled') === false);
  check('parked rejection keeps the typed values',
    mounted.labelInput.value === 'Raw Slot' && mounted.cwdInput.value === '/workspace/raw');
}

await unmountAll();
uninstallGlobals();

console.log(`\nchatProjectCwd.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length > 0) {
  for (const failure of failures) console.error(`FAIL: ${failure}`);
  process.exit(1);
}
/* Explicit success exit: react-dom's scheduler MessagePorts keep the event
 * loop alive after unmountAll(), so falling through never terminates. */
process.exit(0);