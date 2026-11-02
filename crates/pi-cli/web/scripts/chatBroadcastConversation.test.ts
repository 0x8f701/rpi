#!/usr/bin/env node
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import type { act as Act, createElement as CreateElement } from 'react';
import type { createRoot as CreateRoot, Root } from 'react-dom/client';
import type { BroadcastConversation as BroadcastConversationComponent } from '../src/chat/BroadcastConversation.tsx';
import type { ActiveProjectMember, BroadcastRun } from '../src/chat/team';
import type { ProjectRecord } from '../src/chat/mapping';

const failures: string[] = [];
let assertions = 0;
function check(name: string, condition: boolean, detail = ''): void {
  assertions += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

const here = dirname(fileURLToPath(import.meta.url));
const source = readFileSync(join(here, '../src/chat/BroadcastConversation.tsx'), 'utf8');
check('sendingRef is the submit lock', source.includes('sendingRef.current || composerLocked') && source.includes('sendingRef.current = true'));
check('submit catch keeps await onSend after slash parse', source.includes('parseSlashLine(message)')
  && source.indexOf('parseSlashLine(message)') < source.indexOf('await onSend(')
  && source.includes('setMentionError(commandErrorMessage(error))'));
check('Retry keeps scanned call text', source.includes('onRetryTarget(run.id, card.mappingId)'));
check('Cancel keeps scanned call text', source.includes('onCancelTarget(run.id, card.mappingId)'));
check('Retry/Cancel catch via Promise.resolve', source.includes('void Promise.resolve(onRetryTarget(run.id, card.mappingId))')
  && source.includes('void Promise.resolve(onCancelTarget(run.id, card.mappingId))'));

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
    requestAnimationFrame: (callback: (time: number) => void) => {
      callback(0);
      return 1;
    },
    cancelAnimationFrame: () => {},
    getComputedStyle: () => ({ lineHeight: '20px', paddingTop: '0', paddingBottom: '0' }),
  };
  const raf = (callback: (time: number) => void): number => {
    callback(0);
    return 1;
  };
  const overrides: Record<string, unknown> = {
    document: doc,
    window: windowObj,
    IS_REACT_ACT_ENVIRONMENT: true,
    requestAnimationFrame: raf,
    cancelAnimationFrame: () => {},
    getComputedStyle: () => ({ lineHeight: '20px', paddingTop: '0', paddingBottom: '0' }),
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
const { act, createElement } = await import('react') as { act: Act; createElement: CreateElement };
const { createRoot } = await import('react-dom/client') as { createRoot: CreateRoot };
const { BroadcastConversation } = await import('../src/chat/BroadcastConversation.tsx') as {
  BroadcastConversation: typeof BroadcastConversationComponent;
};

const project: ProjectRecord = { id: 'p1', label: 'Demo', cwd: '/tmp', createdAt: 1, updatedAt: 1 };
const failedRun = {
  id: 'run1',
  projectId: 'p1',
  prompt: 'hello',
  createdAt: 1,
  isHandoff: false,
  targetMappingIds: ['m1'],
  responses: [{
    mappingId: 'm1',
    runnerId: 'r1',
    sessionId: 's1',
    sessionPath: '/s',
    runnerLabel: 'alpha',
    currentAttemptId: 'a1',
    attempts: [{
      id: 'a1',
      number: 1,
      promptCommandId: 'c1',
      abortCommandId: null,
      abortSendCount: 0,
      startedAt: 1,
      endedAt: 2,
      startSequence: 1,
      turnId: 't1',
      modelLabel: null,
      status: 'failed',
      failureKind: 'rpc',
      error: 'boom',
      cancelRequested: false,
      connectionLost: false,
      streamIncomplete: false,
      transcript: { items: [] },
    }],
  }],
} as unknown as BroadcastRun;
const members = [{
  mapping: { id: 'm1', state: 'active' },
  runner: { state: 'connected', name: 'alpha', label: 'alpha', codename: 'alpha' },
  session: {},
}] as unknown as ActiveProjectMember[];

async function mount(props: Record<string, unknown>): Promise<{ root: Root; container: MiniNode }> {
  const doc = globalThis.document as unknown as MiniDoc;
  const container = doc.createElement('div');
  doc.body.appendChild(container);
  const root = createRoot(container as unknown as Element);
  await act(async () => {
    root.render(createElement(BroadcastConversation, {
      project,
      runs: [],
      messages: [],
      members: [],
      disabledReason: '',
      sendCommand: async () => ({}),
      onSend: async () => {},
      onCancelTarget: async () => {},
      onRetryTarget: async () => {},
      onViewRunner: () => {},
      ...props,
    }));
  });
  return { root, container };
}

async function typeDraft(container: MiniNode, text: string): Promise<void> {
  const input = container.querySelector('[aria-label="Channel message"]');
  if (input === null) throw new Error('missing composer');
  await act(async () => {
    input.value = text;
    input.dispatchEvent({ type: 'input', target: input, currentTarget: input, preventDefault() {}, stopPropagation() {} });
    input.dispatchEvent({ type: 'change', target: input, currentTarget: input, preventDefault() {}, stopPropagation() {} });
  });
}

{
  let sends = 0;
  const pending = Promise.withResolvers<void>();
  const { root, container } = await mount({
    onSend: async () => {
      sends += 1;
      await pending.promise;
    },
  });
  await typeDraft(container, 'keep this draft');
  const send = container.querySelector('.chat-button--primary');
  if (send === null) throw new Error('missing Send');
  await act(async () => {
    send.click();
    send.click();
  });
  check('overlapping Send calls onSend once', sends === 1, String(sends));
  await act(async () => { pending.reject(new Error('A Broadcast is still running.')); });
  await act(async () => { await Promise.resolve(); });
  const alert = container.querySelector('[role="alert"]');
  check('rejected send surfaces mentionError', (alert?.textContent ?? '') === 'A Broadcast is still running.', alert?.textContent ?? 'missing');
  check('rejected send keeps the draft', container.querySelector('[aria-label="Channel message"]')?.value === 'keep this draft');
  await act(async () => { root.unmount(); });
}

{
  const { root, container } = await mount({
    runs: [failedRun],
    members,
    onRetryTarget: async () => {
      throw new Error('Runner is not connected.');
    },
  });
  const retry = container.querySelectorAll('button').find((node) => node.textContent === 'Retry');
  check('failed card shows Retry', retry !== undefined);
  await act(async () => { retry?.click(); });
  await act(async () => { await Promise.resolve(); });
  const alert = container.querySelector('[role="alert"]');
  check('rejected Retry surfaces mentionError', (alert?.textContent ?? '') === 'Runner is not connected.', alert?.textContent ?? 'missing');
  await act(async () => { root.unmount(); });
}

uninstallGlobals();
console.log(`\nchatBroadcastConversation.test: ${assertions} assertions, ${failures.length} failure(s)`);
if (failures.length > 0) {
  for (const failure of failures) console.error(`  FAIL ${failure}`);
  process.exit(1);
}
process.exit(0);
