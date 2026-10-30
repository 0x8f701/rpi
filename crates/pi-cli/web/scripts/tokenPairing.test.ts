#!/usr/bin/env node
/* Node-runnable regression tests for the token_required pairing card.
 *
 * Mounts TokenPairingCard over the same scoped mini-DOM + react pattern as
 * settingsPanel.test.ts (globals installed before react-dom snapshots
 * canUseDOM; act's MessageChannel one-shot wrapped so the bundle settles).
 * Locks the user-visible recovery contract: autofocus, exact authority label,
 * the real recovery command, trim-on-submit, whitespace-only is a no-op,
 * and Enter submits.
 *
 * Static imports of react-dom/client are impossible here (see the comment at
 * the import site): react-dom snapshots canUseDOM at module init, so the
 * globals must be installed first — the dynamic import below is that
 * documented exception, same as settingsPanel.test.ts. */
import { act, createElement } from 'react';
import {
  TokenPairingCard,
} from '../src/panels/TokenPairingCard';

const failures: string[] = [];
let ran = 0;
function check(name: string, condition: unknown, detail = ''): void {
  ran += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

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
    style: Record<string, unknown>;
    insertBefore(child: MiniNode, before: MiniNode | null): MiniNode;
    removeChild(child: MiniNode): MiniNode;
    addEventListener(type: string, fn: MiniListener, options?: unknown): void;
    removeEventListener(type: string, fn: MiniListener, options?: unknown): void;
    dispatchEvent(raw: Record<string, unknown>): boolean;
    focus(): void;
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

  /* Prototype-less nodes (react-dom's value tracker bails otherwise — same
   * note as the sibling harnesses). */
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
      /* React updates text nodes through nodeValue — without this the
       * reconciliation writes are silently dropped. */
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
      dispatchEvent(raw) {
        const path: MiniNode[] = [];
        let current: MiniNode | null = node;
        while (current !== null) {
          path.push(current);
          current = current.parentNode;
        }
        const target = (raw.target ?? node) as MiniNode;
        for (let i = path.length - 1; i >= 0; i -= 1) {
          for (const fn of [...(path[i].listeners.get(`${String(raw.type)}-capture`) ?? [])]) fn({ ...raw, target });
        }
        for (const fn of [...(node.listeners.get(String(raw.type)) ?? [])]) fn({ ...raw, target });
        for (let i = 1; i < path.length; i += 1) {
          for (const fn of [...(path[i].listeners.get(String(raw.type)) ?? [])]) fn({ ...raw, target });
        }
        return true;
      },
      focus() {
        node.ownerDocument.activeElement = node;
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
    doc.body = makeNode(1, 'BODY', doc);
    doc.appendChild(doc.body);
    doc.activeElement = doc.body;
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
  /* react-dom/client snapshots canUseDOM at module init: import only after
   * the globals are live (settingsPanel.test.ts ordering note). */
  const { createRoot } = await import('react-dom/client');

  /* act's MessageChannel never closes its ports in this bundle — one-shot
   * wrap, same fix as settingsPanel.test.ts. */
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

  const realTick = (): Promise<void> => {
    const { promise, resolve } = Promise.withResolvers<void>();
    setTimeout(resolve, 0);
    return promise;
  };

  const commits: Array<string> = [];
  const host = doc.createElement('div');
  doc.body.appendChild(host);
  const root = createRoot(host);

  try {
    const authority = '192.0.2.15:8765';
    let onTokenChange = (token: string): void => {
      commits.push(token);
    };
    await act(async () => {
      root.render(createElement(TokenPairingCard, { authority, onTokenChange }));
      await realTick();
    });
    check('pairing card renders while the boot is held', !!host.querySelector('.token-pairing'));
    check('pairing card shows the exact listener authority', host.querySelector('.token-pairing__authority-value')?.textContent === authority);
    check('recovery hint names --listen-token-file', host.querySelector('.token-pairing__hint')?.textContent?.includes('--listen-token-file') === true);
    check('pairing input is autofocused on mount', doc.activeElement?.getAttribute('id') === 'token-pairing-input');

    const input = host.querySelector('#token-pairing-input');
    const connect = host.querySelector('.token-pairing__connect');
    check(input !== null && connect !== null, 'pairing input and Connect render');
    const fire = (el: MiniNode, type: string): void => {
      el.dispatchEvent({ type, target: el });
    };

    /* Whitespace-only submit is a no-op: an empty commit would connect
     * tokenless and be refused, contradicting the hold. */
    await act(async () => {
      fire(input!, 'input');
      (input as unknown as { value: string }).value = '   ';
      fire(input!, 'input');
      await realTick();
    });
    check('Connect stays disabled for a whitespace-only draft', connect?.hasAttribute('disabled') === true, connect?.outerHTML ?? '');
    const commitsAfterBlank = commits.length;
    check(commitsAfterBlank === 0, 'no token commit fired for a whitespace-only draft');

    await act(async () => {
      (input as unknown as { value: string }).value = '  abc123  ';
      fire(input!, 'input');
      await realTick();
      connect!.dispatchEvent({ type: 'click', target: connect });
      await realTick();
    });
    check(commits.length === commitsAfterBlank + 1 && commits[commits.length - 1] === 'abc123', `Connect commits the trimmed token once (got ${JSON.stringify(commits)})`);

    await act(async () => {
      (input as unknown as { value: string }).value = 'second-token';
      fire(input!, 'input');
      await realTick();
      input!.dispatchEvent({ type: 'keydown', key: 'Enter', target: input });
      await realTick();
    });
    check(commits.length === 2 && commits[1] === 'second-token', 'Enter submits the token');

    await act(async () => {
      root.render(createElement(TokenPairingCard, { authority: '10.9.8.7:9000', onTokenChange }));
      await realTick();
    });
    await act(async () => { await realTick(); });
    check('authority label follows the live listener', host.querySelector('.token-pairing__authority-value')?.textContent === '10.9.8.7:9000');

    await act(async () => {
      root.unmount();
    });
  } finally {
    host.parentNode?.removeChild(host);
    Object.defineProperty(globalThis, 'MessageChannel', { configurable: true, writable: true, value: realMessageChannel });
    uninstallGlobals();
  }
}

console.log(`\ntokenPairing.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length > 0) {
  console.error(failures.join('\n'));
  process.exit(1);
}