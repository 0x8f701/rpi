#!/usr/bin/env node
// Standalone Add runner form: no project, paste via parseRunnerInviteLink,
// overlay dialog, no LAN discovered section, submit id chat-runner-add.
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
/* Type-only imports stay static. Value imports of react-dom run after
 * installGlobals(): react-dom snapshots canUseDOM at module-init. */
import type { act as Act, createElement as CreateElement } from 'react';
import type { createRoot as CreateRoot, Root } from 'react-dom/client';
import type { AddRunnerForm as AddRunnerFormComponent, AddRunnerFormProps } from '../src/chat/AddRunnerForm.tsx';

const failures: string[] = [];
let assertions = 0;
function check(name: string, condition: boolean, detail = ''): void {
  assertions += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

const here = dirname(fileURLToPath(import.meta.url));
const source = readFileSync(join(here, '../src/chat/AddRunnerForm.tsx'), 'utf8');

check('no project prop', !source.includes('projectId') && !source.includes('ProjectRecord') && !source.includes('project:'));
check('parseRunnerInviteLink on paste', source.includes('parseRunnerInviteLink(address)')
  && source.includes('address: parsed.endpoint.origin')
  && source.includes('token: token.trim() || parsed.token'));
check('submit label Add runner', source.includes("busy ? 'Connecting…' : 'Add runner'"));
check('id chat-runner-add', source.includes('id="chat-runner-add"'));
check('overlay dialog', source.includes("role={overlay ? 'dialog' : undefined}")
  && source.includes('aria-modal={overlay ? true : undefined}')
  && source.includes("placement === 'overlay'"));
check('no mapProject', !source.includes('mapProject') && !source.includes('onInviteExisting') && !source.includes('onAddAndInvite'));
check('title Add runner without project label', source.includes('<h3>Add runner</h3>')
  && source.includes('id="chat-add-runner-title"')
  && !source.includes('Add runner ·'));
check('placeholder is a chat hash link', source.includes('placeholder="https://192.0.2.10:8765/chat#token"'));
check('no LAN discovered section', !source.includes('Discovered on LAN')
  && !source.includes('chat-rail-lan')
  && !source.includes('chat-added-pill'));
check('has invite link, label, token, and submit', source.includes('id="chat-add-runner-address"')
  && source.includes('Runner link')
  && source.includes('id="chat-add-runner-label"')
  && source.includes('id="chat-add-runner-token"')
  && source.includes('id="chat-runner-add"'));
check('no inline form error', !source.includes('chat-form-error'));
check('parse failures report via onError', source.includes('onError(')
  && source.includes('parseRunnerInviteLink(address)'));

type Listener = (event: Record<string, unknown>) => void;

interface MiniDoc {
  nodeType: number;
  nodeName: string;
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
  tagName?: string;
  namespaceURI: string | null;
  style: Record<string, string>;
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
  className: string;
  textContent: string;
  nodeValue: string | null;
  id: string;
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
      if (nodeType === 1 && nodeName === 'INPUT') return node.getAttribute('type') ?? 'text';
      return node.getAttribute('type') ?? '';
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
  doc.oninput = null;
  const windowObj = {
    document: doc,
    event: undefined as Record<string, unknown> | undefined,
    HTMLIFrameElement: DummyHTMLIFrameElement,
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
  return doc as unknown as MiniDoc;
}

function uninstallGlobals(): void {
  for (const key of installed) delete (globalThis as Record<string, unknown>)[key];
  installed.clear();
}

installGlobals();
/* Static import would hoist react-dom above installGlobals and freeze canUseDOM false. */
const { act, createElement } = await import('react') as { act: Act; createElement: CreateElement };
const { createRoot } = await import('react-dom/client') as { createRoot: CreateRoot };
const { renderToStaticMarkup } = await import('react-dom/server.browser');
const { AddRunnerForm } = await import('../src/chat/AddRunnerForm.tsx') as { AddRunnerForm: AddRunnerFormComponent };

const noopAsync = async (): Promise<boolean> => true;
const baseProps: AddRunnerFormProps = {
  placement: 'lobby',
  busyKey: null,
  error: '',
  onError: () => {},
  onAdd: noopAsync,
  onClose: () => {},
};

{
  const lobby = renderToStaticMarkup(createElement(AddRunnerForm, baseProps));
  check('lobby is not a dialog', !lobby.includes('role="dialog"') && !lobby.includes('aria-modal="true"'));
  check('lobby has no Cancel', !lobby.includes('Cancel'));
  check('lobby still has Add runner heading and submit', lobby.includes('>Add runner</h3>') && lobby.includes('id="chat-runner-add"') && lobby.includes('>Add runner</button>'));
  check('lobby has invite link and label inputs', lobby.includes('id="chat-add-runner-address"') && lobby.includes('id="chat-add-runner-label"') && lobby.includes('Runner link'));
  check('lobby has no LAN discovered section', !lobby.includes('Discovered on LAN') && !lobby.includes('chat-rail-lan') && !lobby.includes('chat-added-pill'));
}

{
  const overlay = renderToStaticMarkup(createElement(AddRunnerForm, { ...baseProps, placement: 'overlay' }));
  check('overlay is a dialog', overlay.includes('role="dialog"') && overlay.includes('aria-modal="true"') && overlay.includes('id="chat-add-runner-title"'));
  check('overlay title is Add runner', overlay.includes('>Add runner</h2>') && !overlay.includes('Add runner ·') && !overlay.includes('>Add runner</h3>'));
  check('overlay close is an X icon', overlay.includes('chat-panel__close') && overlay.includes('aria-label="Close add runner"') && !overlay.includes('Cancel'));
  check('overlay has invite link, label, and submit', overlay.includes('id="chat-add-runner-address"') && overlay.includes('id="chat-add-runner-label"') && overlay.includes('id="chat-runner-add"') && overlay.includes('Runner link'));
  check('overlay has no LAN discovered section', !overlay.includes('Discovered on LAN') && !overlay.includes('chat-rail-lan') && !overlay.includes('chat-added-pill'));
}

async function setInputValue(input: MiniNode, next: string): Promise<void> {
  await act(async () => {
    input.value = next;
    input.dispatchEvent({ type: 'input', target: input, preventDefault() {}, stopPropagation() {} });
  });
}

{
  const doc = globalThis.document as unknown as MiniDoc;
  const container = doc.createElement('div');
  doc.body.appendChild(container);
  const root: Root = createRoot(container as unknown as Element);
  const added: Array<{ label: string; address: string; token: string }> = [];
  await act(async () => {
    root.render(createElement(AddRunnerForm, {
      ...baseProps,
      onAdd: async (input) => {
        added.push(input);
        return true;
      },
    }));
  });
  const label = container.querySelector('#chat-add-runner-label');
  const address = container.querySelector('#chat-add-runner-address');
  const form = container.querySelector('#chat-add-runner-form');
  if (label === null || address === null || form === null) throw new Error('missing form fields');
  await setInputValue(label, 'Studio');
  await setInputValue(address, 'https://192.0.2.10:8765/chat#secret-token');
  await act(async () => {
    form.dispatchEvent({ type: 'submit', target: form, preventDefault() {}, stopPropagation() {} });
  });
  check(
    'paste submit calls onAdd with origin+token',
    added.length === 1
      && added[0]?.label === 'Studio'
      && added[0]?.address === 'https://192.0.2.10:8765'
      && added[0]?.token === 'secret-token',
    JSON.stringify(added),
  );
  check('paste submit does not mention mapProject', !JSON.stringify(added).includes('mapProject'));
  await act(async () => { root.unmount(); });
}

{
  const doc = globalThis.document as unknown as MiniDoc;
  const container = doc.createElement('div');
  doc.body.appendChild(container);
  const root: Root = createRoot(container as unknown as Element);
  const added: Array<{ label: string; address: string; token: string }> = [];
  const reported: string[] = [];
  await act(async () => {
    root.render(createElement(AddRunnerForm, {
      ...baseProps,
      onError: (message) => { reported.push(message); },
      onAdd: async (input) => {
        added.push(input);
        return true;
      },
    }));
  });
  const address = container.querySelector('#chat-add-runner-address');
  const form = container.querySelector('#chat-add-runner-form');
  if (address === null || form === null) throw new Error('missing form fields');
  await setInputValue(address, '');
  await act(async () => {
    form.dispatchEvent({ type: 'submit', target: form, preventDefault() {}, stopPropagation() {} });
  });
  check('invalid link reports via onError', reported.length === 1 && reported[0] === 'Runner address is required.', JSON.stringify(reported));
  check('invalid link does not call onAdd', added.length === 0, JSON.stringify(added));
  check('invalid link does not render an inline form error', container.querySelector('.chat-form-error') === null);
  await act(async () => { root.unmount(); });
}

uninstallGlobals();

console.log(`\nchatAddRunnerForm.test: ${assertions} assertions, ${failures.length} failure(s)`);
if (failures.length > 0) {
  for (const failure of failures) console.error(`  FAIL ${failure}`);
  process.exit(1);
}
process.exit(0);
