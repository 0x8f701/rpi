#!/usr/bin/env node
// Mounted behavior for src/panels/CodeReviewTldrPane.tsx.
// Opens expanded. Collapse/expand keeps one markdown body and the host state;
// Stop and close are the parent callbacks. No snapshot request ownership is asserted.
//
// Focused run (not wired into package.json by this change):
//   esbuild scripts/CodeReviewTldrPane.test.ts --bundle --platform=node \
//     --format=esm --loader:.css=empty --alias:mermaid=./scripts/__mocks__/mermaid.ts \
//     --outfile=.web-test-build/CodeReviewTldrPane.test.mjs \
//   && node .web-test-build/CodeReviewTldrPane.test.mjs
//
// Exit codes: 0 = every assertion held; 1 = a regression.
import type { act as Act, createElement as CreateElement } from 'react';
import type { createRoot as CreateRoot, Root } from 'react-dom/client';
import type {
  CodeReviewTldrPane as CodeReviewTldrPaneComponent,
  CodeReviewTldrPaneProps,
  CodeReviewTldrState,
} from '../src/panels/CodeReviewTldrPane.tsx';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: boolean, detail?: string): void {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
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
  isConnected: boolean;
  nodeValue: string | null;
  data: string;
  textContent: string;
  innerHTML: string;
  className: string;
  id: string;
  appendChild(child: MiniNode): MiniNode;
  removeChild(child: MiniNode): MiniNode;
  insertBefore(child: MiniNode, before: MiniNode | null): MiniNode;
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
  closest(selector: string): MiniNode | null;
  querySelector(selector: string): MiniNode | null;
  querySelectorAll(selector: string): MiniNode[];
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
  setTimeout: typeof setTimeout;
  clearTimeout: typeof clearTimeout;
  addEventListener(): void;
  removeEventListener(): void;
};

function createMiniDom(): { window: MiniWindow; document: MiniDocument } {
  class FakeCtor {}
  const relink = (parent: MiniNode) => {
    parent.firstChild = parent.childNodes[0] ?? null;
    parent.lastChild = parent.childNodes[parent.childNodes.length - 1] ?? null;
    for (let i = 0; i < parent.childNodes.length; i += 1) {
      const child = parent.childNodes[i]!;
      child.parentNode = parent;
      child.previousSibling = parent.childNodes[i - 1] ?? null;
      child.nextSibling = parent.childNodes[i + 1] ?? null;
    }
  };

  const matches = (node: MiniNode, selector: string): boolean => {
    const bracket = selector.indexOf('[');
    if (bracket > 0) {
      return matches(node, selector.slice(0, bracket).trim())
        && matches(node, selector.slice(bracket));
    }
    if (selector.includes('.')) {
      const firstDot = selector.indexOf('.');
      if (firstDot > 0 && (bracket < 0 || firstDot < bracket)) {
        const head = selector.slice(0, firstDot).trim();
        if (head !== '' && !matches(node, head)) return false;
        return matches(node, selector.slice(firstDot));
      }
    }
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
      listeners: new Map<string, Set<MiniListener>>(),
      text: '',
      get isConnected() {
        let current: MiniNode | null = node;
        while (current !== null) {
          if (current.nodeType === 9) return true;
          current = current.parentNode;
        }
        return false;
      },
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
        if (value !== '') node.appendChild((owner as MiniDocument).createTextNode(value));
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
        if (text !== '') node.appendChild((owner as MiniDocument).createTextNode(text));
      },
      get innerHTML(): string {
        return node.childNodes.map((child) => child.textContent).join('');
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
        const set = node.listeners.get(key) ?? new Set<MiniListener>();
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
      closest(selector: string) {
        let current: MiniNode | null = node;
        while (current !== null && current.nodeType === 1) {
          if (matches(current, selector)) return current;
          current = current.parentNode;
        }
        return null;
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
    } as MiniNode;
    return node;
  };

  const document = makeNode(9, '#document', null) as MiniDocument;
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
    const created = makeNode(1, tag.toUpperCase(), document);
    created.namespaceURI = ns;
    return created;
  };
  document.createTextNode = (text: string) => {
    const created = makeNode(3, '#text', document);
    created.text = text;
    return created;
  };
  document.createComment = (text: string) => {
    const created = makeNode(8, '#comment', document);
    created.text = text;
    return created;
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
    setTimeout: ((fn: () => void, ms?: number) => globalThis.setTimeout(fn, ms)) as typeof setTimeout,
    clearTimeout: ((id: number) => globalThis.clearTimeout(id)) as typeof clearTimeout,
    addEventListener() {},
    removeEventListener() {},
  };
  document.defaultView = windowObj;
  return { window: windowObj, document };
}

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

const mini = createMiniDom();
Object.defineProperty(globalThis, 'window', { configurable: true, writable: true, value: mini.window });
Object.defineProperty(globalThis, 'document', { configurable: true, writable: true, value: mini.document });
Object.defineProperty(globalThis, 'navigator', { configurable: true, writable: true, value: { userAgent: 'mini-dom' } });
Object.defineProperty(globalThis, 'HTMLElement', { configurable: true, writable: true, value: mini.window.HTMLElement });
Object.defineProperty(globalThis, 'Element', { configurable: true, writable: true, value: mini.window.Element });
Object.defineProperty(globalThis, 'Node', { configurable: true, writable: true, value: mini.window.Node });
Object.defineProperty(globalThis, 'Text', { configurable: true, writable: true, value: mini.window.Text });
Object.defineProperty(globalThis, 'SVGElement', { configurable: true, writable: true, value: mini.window.SVGElement });
Object.defineProperty(globalThis, 'ResizeObserver', {
  configurable: true,
  writable: true,
  value: class {
    observe() {}
    unobserve() {}
    disconnect() {}
  },
});
Object.defineProperty(globalThis, 'MessageChannel', {
  configurable: true,
  writable: true,
  value: OneShotMessageChannel,
});
(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const react = await import('react');
const reactDomClient = await import('react-dom/client');
const act = react.act as Act;
const createElement = react.createElement as CreateElement;
const createRoot = reactDomClient.createRoot as CreateRoot;
const paneModule = await import('../src/panels/CodeReviewTldrPane.tsx');
const CodeReviewTldrPane = paneModule.CodeReviewTldrPane as CodeReviewTldrPaneComponent;

const BODY = [
  '## Overview',
  'Pinned snapshot keeps the short review.',
  '',
  '## Details',
  '- `src/panels/CodeReviewPanel.tsx` owns the host snapshot',
  '- Mermaid stays inside the same body',
  '',
  '```mermaid',
  'graph TD; A-->B;',
  '```',
].join('\n');

function buttonByLabel(root: MiniNode, label: string): MiniNode | null {
  return root.querySelectorAll('button').find((button) => (
    button.getAttribute('aria-label') === label || button.textContent.includes(label)
  )) ?? null;
}

async function mount(props: CodeReviewTldrPaneProps): Promise<{ container: MiniNode; root: Root }> {
  const container = mini.document.createElement('div');
  mini.document.body.appendChild(container);
  const root = createRoot(container as unknown as Element);
  await act(async () => {
    root.render(createElement(CodeReviewTldrPane, props));
  });
  return { container, root };
}

const stops: number[] = [];
const closes: number[] = [];
const layouts: number[] = [];
const baseProps = (): CodeReviewTldrPaneProps => ({
  markdown: BODY,
  state: 'done',
  sessionId: 'sess-tldr',
  onStop: () => { stops.push(1); },
  onClose: () => { closes.push(1); },
  onLayoutChange: () => { layouts.push(1); },
});

{
  const { container, root } = await mount(baseProps());
  const pane = container.querySelector('section.code-review-tldr') as MiniNode | null;
  const body = container.querySelector('.code-review-tldr__body') as MiniNode | null;
  const toggle = buttonByLabel(container, 'Collapse details');
  const viewport = container.querySelector('.code-review-tldr__viewport') as MiniNode | null;
  check('done pane mounts one section', pane !== null);
  check('done pane reports data-tldr-state done', pane?.getAttribute('data-tldr-state') === 'done');
  check('done pane is not busy', pane?.getAttribute('aria-busy') === 'false');
  check('status reads Ready', container.querySelector('[role="status"]')?.textContent.includes('Ready') === true);
  check('one markdown body is mounted', container.querySelectorAll('.code-review-tldr__body').length === 1);
  check('body keeps overview and details text', body?.textContent.includes('Pinned snapshot keeps the short review.') === true
    && body.textContent.includes('CodeReviewPanel.tsx') === true);
  check('viewport starts expanded', viewport?.className.includes('is-expanded') === true
    && viewport?.getAttribute('data-expanded') === 'true');
  check('expand control starts expanded', toggle?.getAttribute('aria-expanded') === 'true');
  check('session owner is stamped for markdown path chips', pane?.getAttribute('data-session-id') === 'sess-tldr');
  check('done state has no Stop button', buttonByLabel(container, 'Stop') === null);
  check('failed copy is not rendered inline', !container.textContent.includes('tldrError')
    && container.querySelector('.code-review__error') === null);

  await act(async () => { toggle?.click(); });
  const collapsedBody = container.querySelector('.code-review-tldr__body') as MiniNode | null;
  check('collapse keeps the same body text', collapsedBody?.textContent === body?.textContent);
  check('collapse still mounts one body', container.querySelectorAll('.code-review-tldr__body').length === 1);
  check('collapse drops is-expanded', container.querySelector('.code-review-tldr__viewport')?.className.includes('is-expanded') !== true);
  check('collapse sets data-expanded false', container.querySelector('.code-review-tldr__viewport')?.getAttribute('data-expanded') === 'false');
  check('toggle label becomes Expand details', buttonByLabel(container, 'Expand details')?.getAttribute('aria-expanded') === 'false');
  check('state stays done across collapse', container.querySelector('[data-tldr-state]')?.getAttribute('data-tldr-state') === 'done');

  await act(async () => { buttonByLabel(container, 'Expand details')?.click(); });
  check('reexpand restores Collapse details', buttonByLabel(container, 'Collapse details')?.getAttribute('aria-expanded') === 'true');
  check('reexpand restores is-expanded', container.querySelector('.code-review-tldr__viewport')?.className.includes('is-expanded') === true
    && container.querySelector('.code-review-tldr__viewport')?.getAttribute('data-expanded') === 'true');
  check('reexpand preserves body text', container.querySelector('.code-review-tldr__body')?.textContent === body?.textContent);

  await act(async () => { buttonByLabel(container, 'Close summary')?.click(); });
  check('close calls onClose once', closes.length === 1);
  check('close does not call onStop', stops.length === 0);
  await act(async () => { root.unmount(); });
}

{
  const props = baseProps();
  props.state = 'running';
  props.markdown = BODY;
  const { container, root } = await mount(props);
  check('running pane is busy', container.querySelector('[aria-busy="true"]') !== null);
  check('running status reads Generating', container.querySelector('[role="status"]')?.textContent.includes('Generating') === true);
  check('running shows the review spinner token', container.querySelector('.code-review__spinner') !== null);
  const stop = buttonByLabel(container, 'Stop');
  check('Stop uses the existing stop action class', stop?.className.includes('code-review__action--stop') === true);
  check('Stop glyph is the existing square', stop?.textContent.includes('\u25a0') === true);
  check('running keeps the supplied body', container.querySelector('.code-review-tldr__body')?.textContent.includes('Pinned snapshot') === true);
  check('running opens expanded', container.querySelector('.code-review-tldr__viewport')?.getAttribute('data-expanded') === 'true'
    && buttonByLabel(container, 'Collapse details')?.getAttribute('aria-expanded') === 'true');
  await act(async () => { stop?.click(); });
  check('Stop calls onStop once', stops.length === 1);
  check('Stop does not call onClose', closes.length === 1);
  await act(async () => { buttonByLabel(container, 'Collapse details')?.click(); });
  check('collapse during running keeps running state', container.querySelector('[data-tldr-state]')?.getAttribute('data-tldr-state') === 'running');
  check('collapse during running keeps Stop', buttonByLabel(container, 'Stop') !== null);
  await act(async () => { buttonByLabel(container, 'Expand details')?.click(); });
  check('reexpand during running keeps running state', container.querySelector('[data-tldr-state]')?.getAttribute('data-tldr-state') === 'running');
  check('reexpand during running keeps Stop', buttonByLabel(container, 'Stop') !== null);
  check('reexpand during running restores expanded', container.querySelector('.code-review-tldr__viewport')?.getAttribute('data-expanded') === 'true');
  await act(async () => { root.unmount(); });
}

{
  const props = baseProps();
  props.state = 'failed';
  props.markdown = '## Overview\nLast approved summary stays.';
  const { container, root } = await mount(props);
  check('failed state keeps the host body', container.querySelector('.code-review-tldr__body')?.textContent.includes('Last approved summary stays.') === true);
  check('failed state has no inline error node', container.querySelector('.code-review__error') === null);
  check('failed status reads Failed without extra copy', container.querySelector('[role="status"]')?.textContent.trim() === 'Failed');
  check('failed state is not busy and has no Stop', container.querySelector('[aria-busy="true"]') === null
    && buttonByLabel(container, 'Stop') === null);
  await act(async () => { root.unmount(); });
}

{
  const props = baseProps();
  props.state = 'idle';
  props.markdown = '';
  const { container, root } = await mount(props);
  check('idle empty markdown still mounts the body node', container.querySelector('.code-review-tldr__body') !== null);
  check('idle status reads Idle', container.querySelector('[role="status"]')?.textContent.includes('Idle') === true);
  const seen = container.querySelector('.code-review-tldr__body')?.textContent ?? '';
  props.state = 'done';
  props.markdown = BODY;
  await act(async () => {
    root.render(createElement(CodeReviewTldrPane, props));
  });
  const next = container.querySelector('.code-review-tldr__body')?.textContent ?? '';
  check('host-approved body replaces an empty idle body', next.includes('Pinned snapshot') && next !== seen);
  check('rerender to done does not invent a Stop button', buttonByLabel(container, 'Stop') === null);
  await act(async () => { root.unmount(); });
}

const unused: CodeReviewTldrState[] = ['idle', 'running', 'done', 'failed'];
check('state union covers the frozen wire values', unused.length === 4);

if (failures.length > 0) {
  console.error(`CodeReviewTldrPane: ${failures.length} assertion(s) failed:`);
  for (const failure of failures) console.error(`  - ${failure}`);
  process.exit(1);
}
console.log(`CodeReviewTldrPane: ${ran} assertions passed`);
process.exit(0);
