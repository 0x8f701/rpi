#!/usr/bin/env node
import { act, createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import {
  SettingControl,
  SettingRequestGate,
  SettingsPanel,
  dirtySettingKeys,
  editSettingLocalDraft,
  mergeSettingSnapshotDrafts,
  reconcileSettingLocalDrafts,
  shouldSubmitSettingEnter,
  stagedSettingKeys,
  type SettingLocalDrafts,
  type SettingValueViewWire,
} from '../src/panels/SettingsPanel.tsx';

const failures: string[] = [];
let ran = 0;
function check(name: string, condition: unknown, detail = ''): void {
  ran += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

function view(key: string, value: unknown, kind = 'string'): SettingValueViewWire {
  return {
    definition: {
      key,
      category: 'models',
      valueType: { kind },
      defaultJson: JSON.stringify(value),
      description: key,
      enumValues: [],
      scopes: 'global',
      behavior: 'live',
      secret: false,
      trustSensitive: false,
    },
    effectiveValue: value,
    source: 'global',
    editableGlobal: true,
    editableProject: false,
    redacted: false,
  };
}

// Multi-character editing is entirely local. The component's onChange prop is
// the local setter seam; no transport callback is part of SettingControl.
{
  const model = view('model', 'old');
  let drafts = reconcileSettingLocalDrafts(new Map(), [{ view: model, value: model.effectiveValue }], true);
  for (const raw of ['n', 'ne', 'new', 'new-', 'new-model']) {
    drafts = editSettingLocalDraft(drafts, 'model', raw, 'old');
  }
  check('multi-character input stays local', drafts.get('model')?.raw === 'new-model', JSON.stringify(drafts.get('model')));

  let changed = '';
  let committed = 0;
  const editableHtml = renderToStaticMarkup(createElement(SettingControl, {
    view: model,
    raw: 'new-model',
    editable: true,
    onChange: (raw: string) => { changed = raw; },
    onCommit: () => { committed += 1; },
  }));
  check('editable control is not disabled by request state', !editableHtml.includes('disabled=""'), editableHtml);
  const element = SettingControl({
    view: model,
    raw: 'new-model',
    editable: true,
    onChange: (raw) => { changed = raw; },
    onCommit: () => { committed += 1; },
  }) as { props: { onChange: (event: { target: { value: string } }) => void; onKeyDown: (event: unknown) => void } };
  element.props.onChange({ target: { value: 'new-model-2' } });
  check('SettingControl onChange updates only local callback', changed === 'new-model-2' && committed === 0, `${changed}/${committed}`);
  element.props.onKeyDown({ key: 'Enter', nativeEvent: { isComposing: true }, preventDefault() {} });
  check('SettingControl IME Enter does not commit', committed === 0, String(committed));
  element.props.onKeyDown({ key: 'Enter', nativeEvent: { isComposing: false }, preventDefault() {} });
  check('SettingControl plain Enter commits once', committed === 1, String(committed));
  let settingsSetCalls = 0;
  const gate = new SettingRequestGate();
  const request = deferred<unknown>();
  const revision = drafts.get('model')!.revision;
  const submitted = gate.submit('model', revision, 'new-model', () => drafts.get('model')?.revision, () => {
    settingsSetCalls += 1;
    return request.promise;
  });
  check('explicit submit sends exactly one command', settingsSetCalls === 1, String(settingsSetCalls));

  const duplicate = gate.submit('model', revision, 'new-model', () => drafts.get('model')?.revision, () => {
    settingsSetCalls += 1;
    return Promise.resolve({});
  });
  check('duplicate pending submit shares one promise', duplicate === submitted);
  check('duplicate pending submit sends no second command', settingsSetCalls === 1, String(settingsSetCalls));

  request.resolve({ effectiveValue: 'new-model' });
  const result = await submitted;
  check('current success is accepted', result.ok && result.accepted, JSON.stringify(result));
  drafts = mergeSettingSnapshotDrafts(drafts, [{ view: model, value: 'new-model' }], 'model');
  check('acknowledged success marks the field clean', dirtySettingKeys(drafts).length === 0, JSON.stringify(drafts.get('model')));
}

// Inputs stay editable while a request is pending. Editing invalidates that
// generation so its late success cannot overwrite the newer local draft.
{
  const model = view('model', 'old');
  let drafts: SettingLocalDrafts = reconcileSettingLocalDrafts(new Map(), [{ view: model, value: 'old' }], true);
  drafts = editSettingLocalDraft(drafts, 'model', 'first', 'old');
  const gate = new SettingRequestGate();
  const first = deferred<unknown>();
  const firstRevision = drafts.get('model')!.revision;
  const firstResult = gate.submit('model', firstRevision, 'first', () => drafts.get('model')?.revision, () => first.promise);

  gate.invalidate('model');
  drafts = editSettingLocalDraft(drafts, 'model', 'first plus typing', 'old');
  check('pending request does not block editing', drafts.get('model')?.raw === 'first plus typing');
  first.resolve({ effectiveValue: 'first' });
  const settled = await firstResult;
  check('edit during pending rejects stale success', settled.ok && !settled.accepted, JSON.stringify(settled));
  check('stale success leaves newer draft intact', drafts.get('model')?.raw === 'first plus typing');
}

// Two explicit generations may resolve in reverse order. Only the latest
// success/error is accepted; stale results cannot mutate UI status or values.
{
  const gate = new SettingRequestGate();
  let modelRevision = 1;
  let themeRevision = 1;
  const oldRequest = deferred<unknown>();
  const newRequest = deferred<unknown>();
  const oldResult = gate.submit('model', modelRevision, 'old-submit', () => modelRevision, () => oldRequest.promise);
  const newResult = gate.submit('theme', themeRevision, 'new-submit', () => themeRevision, () => newRequest.promise);

  newRequest.resolve({ effectiveValue: 'new-submit' });
  const newest = await newResult;
  check('newer cross-key success is accepted', newest.ok && newest.accepted, JSON.stringify(newest));
  oldRequest.reject(new Error('stale failure'));
  const stale = await oldResult;
  check('older cross-key error is generation-rejected', !stale.ok && !stale.accepted, JSON.stringify(stale));
}

// A current failure is surfaced but never clears the local dirty value.
{
  const model = view('model', 'old');
  let drafts = reconcileSettingLocalDrafts(new Map(), [{ view: model, value: 'old' }], true);
  drafts = editSettingLocalDraft(drafts, 'model', 'rejected', 'old');
  const gate = new SettingRequestGate();
  const request = deferred<unknown>();
  const revision = drafts.get('model')!.revision;
  const resultPromise = gate.submit('model', revision, 'rejected', () => drafts.get('model')?.revision, () => request.promise);
  request.reject(new Error('server rejected'));
  const result = await resultPromise;
  check('current failure is accepted for display', !result.ok && result.accepted, JSON.stringify(result));
  check('failure retains dirty draft text', drafts.get('model')?.dirty && drafts.get('model')?.raw === 'rejected', JSON.stringify(drafts.get('model')));
}

// External snapshots refresh clean fields only and preserve dirty fields.
{
  const model = view('model', 'old');
  const theme = view('theme', 'dark');
  let drafts = reconcileSettingLocalDrafts(new Map(), [
    { view: model, value: 'old' },
    { view: theme, value: 'dark' },
  ], true);
  drafts = editSettingLocalDraft(drafts, 'model', 'local-model', 'old');
  drafts = mergeSettingSnapshotDrafts(drafts, [
    { view: model, value: 'external-model' },
    { view: theme, value: 'light' },
  ]);
  check('external snapshot preserves dirty field', drafts.get('model')?.raw === 'local-model', JSON.stringify(drafts.get('model')));
  check('external snapshot updates clean field', drafts.get('theme')?.raw === 'light', JSON.stringify(drafts.get('theme')));

  const catalog = { projectTrusted: true, globalPath: '', projectPath: '', values: [model, theme] };
  const stagedViews = new Map([['theme', { ...theme, effectiveValue: 'light' }]]);
  check('dirty badge keys include only staged server field', Array.from(stagedSettingKeys(catalog, stagedViews)).join(',') === 'theme');

  drafts = mergeSettingSnapshotDrafts(drafts, [{ view: model, value: 'local-model' }], 'model');
  check('acknowledged snapshot marks submitted field clean', drafts.get('model')?.dirty === false, JSON.stringify(drafts.get('model')));
}
// IME confirmation Enter never submits. Plain Enter does; JSON textarea uses
// an explicit Ctrl/Cmd+Enter chord so normal newlines remain editable.
check('plain Enter submits', shouldSubmitSettingEnter('Enter', false));
check('IME Enter does not submit', !shouldSubmitSettingEnter('Enter', true));
check('ordinary key does not submit', !shouldSubmitSettingEnter('a', false));
check('textarea Enter inserts newline', !shouldSubmitSettingEnter('Enter', false, true, false));
check('textarea modified Enter submits', shouldSubmitSettingEnter('Enter', false, true, true));
check('textarea IME modified Enter does not submit', !shouldSubmitSettingEnter('Enter', true, true, true));
// Catalog gate regression (token_required boot hold): a real client render
// (createRoot + act) because the gate lives in an effect. While
// catalogEnabled=false the panel mounts with a usable Connection token UI,
// Edit/Refresh pinned disabled, zero settings_inspect, and no error banner;
// the first flip to true loads the catalog exactly once and stays quiet.
{
  type MiniListener = (event: Record<string, unknown>) => void;

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
    listeners: Map<string, Set<MiniListener>>;
    text: string;
    className: string;
    textContent: string;
    text: string;
    style: Record<string, unknown>;
    appendChild(child: MiniNode): MiniNode;
    insertBefore(child: MiniNode, before: MiniNode | null): MiniNode;
    removeChild(child: MiniNode): MiniNode;
    addEventListener(type: string, fn: MiniListener, options?: unknown): void;
    removeEventListener(type: string, fn: MiniListener, options?: unknown): void;
    setAttribute(name: string, value: string): void;
    getAttribute(name: string): string | null;
    removeAttribute(name: string): void;
    hasAttribute(name: string): boolean;
    querySelector(selector: string): MiniNode | null;
    querySelectorAll(selector: string): MiniNode[];
  }

  interface MiniDoc extends MiniNode {
    createElement(tag: string): MiniNode;
    createTextNode(text: string): MiniNode;
    createComment(text: string): MiniNode;
    createDocumentFragment(): MiniNode;
    body: MiniNode;
    activeElement: MiniNode | null;
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

  /* Mini DOM — only the surface React 18 client rendering + this panel use:
   * tree ops, attributes, delegated-listener registration. Prototype-less
   * nodes make React's value tracker bail (same as the sibling harnesses).
   * No event dispatch: the gate is exercised through mount/re-render. */
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
      text: '',
      /* react-dom's module init probes `element.style` with `in` during its
       * vendor-prefix setup, so every element needs a style object. */
      style: {},
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
        const capture = options === true || (typeof options === 'object' && (options as { capture?: boolean }).capture === true);
        const key = capture ? `${type}-capture` : type;
        const set = node.listeners.get(key) ?? new Set();
        set.add(fn);
        node.listeners.set(key, set);
      },
      removeEventListener(type, fn, options) {
        const capture = options === true || (typeof options === 'object' && (options as { capture?: boolean }).capture === true);
        node.listeners.get(capture ? `${type}-capture` : type)?.delete(fn);
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
    };
    return node;
  }

  const installed = new Set<string>();
  function installGlobals(): MiniDoc {
    const doc = makeNode(9, '#document', null as unknown as MiniDoc) as unknown as MiniDoc;
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
    doc.body = makeNode(1, 'BODY', doc);
    doc.appendChild(doc.body);
    doc.activeElement = doc.body;
    /* `event` must exist as an own window slot: ReactDOM reads window.event
     * for event-priority lane selection (see chatProjectCwd.test.ts). The
     * dummy HTMLIFrameElement covers getActiveElementDeep's instanceof probe. */
    const windowObj = {
      document: doc,
      event: undefined as Record<string, unknown> | undefined,
      HTMLIFrameElement: class DummyHTMLIFrameElement {},
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

  const doc = installGlobals();
  /* react-dom/client snapshots canUseDOM at module init, so it must be
   * imported only after the globals are live (same ordering note as
   * chatProjectCwd.test.ts; the static imports above never touch it). */
  const { createRoot } = await import('react-dom/client');

  /* In this esbuild bundle react's async `act` cannot reach
   * `module.require("timers")` (the __commonJS shim passes a bare
   * `{exports:{}}`), so `enqueueTask` falls back to creating a fresh
   * MessageChannel for every async act() and never closes it. A port with an
   * `onmessage` handler keeps the event loop alive forever, which is exactly
   * the hang after the final assertion. Wrap the constructor so each one-shot
   * flush channel closes both ports once its single message is consumed, and
   * restore the real constructor with the other globals. */
  const realMessageChannel = globalThis.MessageChannel;
  class OneShotMessageChannel extends realMessageChannel {
    constructor() {
      super();
      const port1 = this.port1;
      const port2 = this.port2;
      const port1Proxy = new Proxy(port1, {
        get(target, prop) {
          const value = Reflect.get(target, prop, target);
          return typeof value === 'function' ? value.bind(target) : value;
        },
        set(target, prop, value) {
          if (prop === 'onmessage' && typeof value === 'function') {
            return Reflect.set(target, prop, (event: MessageEvent) => {
              try {
                value(event);
              } finally {
                target.close();
                port2.close();
              }
            });
          }
          return Reflect.set(target, prop, value);
        },
      });
      Object.defineProperty(this, 'port1', { value: port1Proxy });
    }
  }
  Object.defineProperty(globalThis, 'MessageChannel', { configurable: true, writable: true, value: OneShotMessageChannel });

  const sent: Array<Record<string, unknown>> = [];
  const sendCommand = async (command: Record<string, unknown>): Promise<unknown> => {
    sent.push(command);
    if (command.type === 'settings_inspect') {
      return {
        projectTrusted: true,
        globalPath: '/global/settings.toml',
        projectPath: '/project/.pi/settings.toml',
        values: [view('model', 'from-server')],
      };
    }
    throw new Error(`unexpected command ${String(command.type)}`);
  };
  const realTick = (): Promise<void> => {
    const { promise, resolve } = Promise.withResolvers<void>();
    setTimeout(resolve, 0);
    return promise;
  };

  const panelProps = (catalogEnabled: boolean) => ({
    sendCommand,
    refreshState: async (): Promise<void> => {},
    catalogEnabled,
    token: '',
    onTokenChange: (): void => {},
    onClose: (): void => {},
  });

  const host = doc.createElement('div');
  doc.body.appendChild(host);
  const root = createRoot(host);

  /* Cleanup runs even when an act/check throws: unmount, drop the mini-DOM
   * tree, and put every overridden global (constructor included) back so the
   * focused process can settle on its own. */
  try {
    // Gate closed: the token_required boot hold mounts the panel with no
    // socket, so the catalog auto-load must be a silent no-op.
    await act(async () => {
      root.render(createElement(SettingsPanel, panelProps(false)));
      await realTick();
    });
    check('gate closed mounts Connection token UI', !!host.querySelector('#settings-token-input') && !!host.querySelector('#settings-token-save-btn'));
    check('gate closed sends zero settings_inspect', sent.length === 0, JSON.stringify(sent));
    check('gate closed surfaces no catalog error', !host.querySelector('.panel__status'));
    check('gate closed pins Edit settings disabled', host.querySelector('#settings-edit-btn')?.hasAttribute('disabled') === true);
    check('gate closed pins Refresh disabled', host.querySelector('#settings-refresh-btn')?.hasAttribute('disabled') === true);

    // First flip to enabled (token saved, authoritative bootstrap done).
    await act(async () => {
      root.render(createElement(SettingsPanel, panelProps(true)));
      await realTick();
    });
    await act(async () => {
      await realTick();
    });
    const inspectCalls = sent.filter((command) => command.type === 'settings_inspect');
    check('first gate-open sends exactly one settings_inspect', inspectCalls.length === 1, JSON.stringify(sent));
    check('first gate-open sends no other command', sent.length === inspectCalls.length, JSON.stringify(sent));
    check('gate-open surfaces no catalog error', !host.querySelector('.panel__status'));
    check('gate-open renders catalog rows', !!host.querySelector('[data-setting-key="model"]'));
    check('gate-open enables Refresh', host.querySelector('#settings-refresh-btn')?.hasAttribute('disabled') === false);

    // A same-props rerender must not re-send: the gate loads exactly once.
    await act(async () => {
      root.render(createElement(SettingsPanel, panelProps(true)));
      await realTick();
    });
    check('gate-open rerender sends no second settings_inspect', sent.filter((command) => command.type === 'settings_inspect').length === 1, JSON.stringify(sent));

    await act(async () => {
      root.unmount();
    });
  } finally {
    host.parentNode?.removeChild(host);
    Object.defineProperty(globalThis, 'MessageChannel', { configurable: true, writable: true, value: realMessageChannel });
    uninstallGlobals();
  }

}

console.log(`\nsettingsPanel.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length > 0) {
  console.error(failures.join('\n'));
  process.exit(1);
}
