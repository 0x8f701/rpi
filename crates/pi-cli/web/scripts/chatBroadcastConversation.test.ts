#!/usr/bin/env node
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import type { act as Act, createElement as CreateElement } from 'react';
import type { createRoot as CreateRoot, Root } from 'react-dom/client';
import type { BroadcastConversation as BroadcastConversationComponent } from '../src/chat/BroadcastConversation.tsx';
import type { ActiveProjectMember, BroadcastRun } from '../src/chat/team';
import type { ProjectRecord } from '../src/chat/mapping';
import { replyMarkdownFileName, sourceMarkdownFromBlocks } from '../src/replyMarkdown.ts';
import { channelLogStorageKey, loadChannelLogs, saveChannelLogs } from '../src/chat/chatChannelLog.ts';

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
  contains(candidate: MiniNode): boolean;
  scrollIntoView(): void;
  getBoundingClientRect(): {
    top: number; left: number; right: number; bottom: number;
    width: number; height: number; x: number; y: number;
    toJSON(): Record<string, number>;
  };
  readonly isConnected: boolean;
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
  // Real compound selector support for the mini tree (tag*, .class, #id,
  // [attr], [attr=value]) — the harness must query the actual rendered tree,
  // never echo a result. Descendant combinators are not used by this suite.
  const input = selector.trim();
  if (input === '') return false;
  let rest = input;
  let tag: string | null = null;
  const tagMatch = /^([A-Za-z][A-Za-z0-9_-]*|\*)/.exec(rest);
  if (tagMatch !== null) {
    tag = tagMatch[1];
    rest = rest.slice(tagMatch[0].length);
  }
  const classes: string[] = [];
  const ids: string[] = [];
  const attributes: Array<{ name: string; value?: string }> = [];
  while (rest.length > 0) {
    if (rest.startsWith('.')) {
      const classMatch = /^\.([A-Za-z0-9_-]+)/.exec(rest);
      if (classMatch === null) return false;
      classes.push(classMatch[1]);
      rest = rest.slice(classMatch[0].length);
      continue;
    }
    if (rest.startsWith('#')) {
      const idMatch = /^#([A-Za-z0-9_-]+)/.exec(rest);
      if (idMatch === null) return false;
      ids.push(idMatch[1]);
      rest = rest.slice(idMatch[0].length);
      continue;
    }
    if (rest.startsWith('[')) {
      const close = rest.indexOf(']');
      if (close < 0) return false;
      const body = rest.slice(1, close);
      rest = rest.slice(close + 1);
      const eq = body.indexOf('=');
      if (eq < 0) {
        attributes.push({ name: body.trim() });
      } else {
        attributes.push({
          name: body.slice(0, eq).trim(),
          value: body.slice(eq + 1).trim().replace(/^["']|["']$/g, ''),
        });
      }
      continue;
    }
    return false;
  }
  if (tag !== null && tag !== '*' && node.tagName !== tag.toUpperCase()) return false;
  for (const className of classes) {
    if (!node.className.split(/\s+/).includes(className)) return false;
  }
  for (const id of ids) {
    if (node.id !== id) return false;
  }
  for (const attribute of attributes) {
    const actual = node.getAttribute(attribute.name);
    if (actual === null) return false;
    if (attribute.value !== undefined && actual !== attribute.value) return false;
  }
  return true;
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
    contains(candidate: MiniNode): boolean {
      let current: MiniNode | null = candidate;
      while (current !== null) {
        if (current === node) return true;
        current = current.parentNode;
      }
      return false;
    },
    scrollIntoView(): void {
      // Mini DOM has no layout; a scroll request is a legal no-op here.
    },
    getBoundingClientRect() {
      return {
        top: 0, left: 0, right: 0, bottom: 0,
        width: 0, height: 0, x: 0, y: 0,
        toJSON: () => ({ top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0, x: 0, y: 0 }),
      };
    },
    get isConnected() {
      let current: MiniNode | null = node;
      while (current !== null) {
        if (current.nodeType === 9) return true;
        current = current.parentNode;
      }
      return false;
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
  const windowListeners = new Map<string, Set<Listener>>();
  interface MiniWindow {
    document: MiniDoc;
    event: Record<string, unknown> | undefined;
    HTMLIFrameElement: typeof DummyHTMLIFrameElement;
    addEventListener(type: string, fn: Listener, options?: boolean | { capture?: boolean }): void;
    removeEventListener(type: string, fn: Listener, options?: boolean | { capture?: boolean }): void;
    dispatchEvent(raw: Record<string, unknown>): boolean;
    requestAnimationFrame(callback: (time: number) => void): number;
    cancelAnimationFrame(): void;
    getComputedStyle(): { lineHeight: string; paddingTop: string; paddingBottom: string };
  }
  const windowObj: MiniWindow = {
    document: doc,
    event: undefined as Record<string, unknown> | undefined,
    HTMLIFrameElement: DummyHTMLIFrameElement,
    // Real EventTarget surface: mermaid's module init registers a window
    // "load" listener; dispatch actually invokes captured/target listeners.
    addEventListener(type: string, fn: Listener, options?: boolean | { capture?: boolean }): void {
      const capture = options === true || (typeof options === 'object' && options.capture === true);
      const key = capture ? `${type}-capture` : type;
      const set = windowListeners.get(key) ?? new Set<Listener>();
      set.add(fn);
      windowListeners.set(key, set);
    },
    removeEventListener(type: string, fn: Listener, options?: boolean | { capture?: boolean }): void {
      const capture = options === true || (typeof options === 'object' && options.capture === true);
      const key = capture ? `${type}-capture` : type;
      windowListeners.get(key)?.delete(fn);
    },
    dispatchEvent(raw: Record<string, unknown>): boolean {
      const event = { ...raw, target: windowObj };
      const type = String(raw.type);
      for (const fn of [...(windowListeners.get(`${type}-capture`) ?? [])]) fn(event);
      for (const fn of [...(windowListeners.get(type) ?? [])]) fn(event);
      return true;
    },
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
      onReplyActionError: () => {},
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

// ---- channel-log reply: raw source, real wire clock, persistence ----
// bg2442: the terminal channel log row must expose the shared reply actions,
// copying/saving the RAW assistant source (no trim/newline normalization) and
// naming the file from the settled item's real wire END timestamp
// (`generatedAt`, distinct from the log append clock `at`). No star is
// fabricated; legacy rows without a wire clock fail save through the sink.
{
  const doc = globalThis.document as unknown as MiniDoc;
  const originalCreate = doc.createElement.bind(doc);
  const createdAnchors: MiniNode[] = [];
  doc.createElement = (tag: string) => {
    const node = originalCreate(tag);
    if (tag.toLowerCase() === 'a') {
      createdAnchors.push(node);
      // Real DOM anchors expose remove(); add it to the mini node so the
      // component's cleanup path runs unmodified.
      Object.assign(node, {
        remove: () => {
          const parent: MiniNode | null = node.parentNode;
          if (parent) parent.removeChild(node);
        },
      });
    }
    return node;
  };
  const blobs: Array<{ url: string; blob: Blob }> = [];
  const originalCreateObjectURL = URL.createObjectURL;
  const originalRevokeObjectURL = URL.revokeObjectURL;
  URL.createObjectURL = ((blob: Blob) => {
    const url = `blob:channel-log/${blobs.length}`;
    blobs.push({ url, blob });
    return url;
  }) as typeof URL.createObjectURL;
  URL.revokeObjectURL = (() => {}) as typeof URL.revokeObjectURL;
  try {
    // Raw source = sourceMarkdownFromBlocks: leading/trailing whitespace,
    // CRLF and multiple text blocks preserved byte-for-byte.
    const runnerText = sourceMarkdownFromBlocks([
      { type: 'text', text: '  **channel** reply\r\n' },
      { type: 'text', text: '- first\r\n- second  ' },
    ]);
    const wireEnd = 1750000050000;
    const logAt = 1750000000123;
    const humanMessage = { id: 'log-h', at: 1750000000000, kind: 'human' as const, mappingId: null, author: 'You', text: 'hello team' };
    const runnerMessage = {
      id: 'log-r', at: logAt, kind: 'runner' as const, mappingId: 'm1', author: 'alpha', text: runnerText, generatedAt: wireEnd,
    };
    const { root, container } = await mount({ messages: [humanMessage, runnerMessage], members });
    const reply = container.querySelectorAll('.chat-session-message--assistant')
      .find((node) => node.querySelector('button[data-reply-action="save-markdown"]') !== null) ?? null;
    check('channel log reply renders the shared action strip in the row band',
      reply !== null && reply.className.includes('msg--assistant')
        && reply.querySelector('.msg--assistant__actions') !== null,
      reply === null ? 'missing reply article' : reply.className);
    const buttons = reply === null ? [] : reply.querySelectorAll('button');
    check('channel log reply exposes copy+save and never a fabricated star',
      reply !== null
        && buttons.length === 2
        && reply.querySelector('button.assistant-star-toggle') === null
        && reply.querySelector('button[data-reply-action="copy-markdown"]') !== null
        && reply.querySelector('button[data-reply-action="save-markdown"]') !== null,
      String(buttons.length));
    check('short log reply keeps the author header inside the row band',
      reply !== null
        && (reply.querySelector('.chat-session-message__role')?.textContent ?? '') === '@alpha'
        && reply.querySelector('.msg--assistant__actions')?.parentNode === reply,
      reply?.querySelector('.chat-session-message__role')?.textContent ?? 'missing header');
    const human = container.querySelectorAll('.chat-session-message--user')[0] ?? null;
    check('human channel message has no reply-action strip',
      human !== null && human.querySelector('button[data-reply-action="save-markdown"]') === null);
    await act(async () => {
      reply?.querySelector('button[data-reply-action="save-markdown"]')?.click();
    });
    const anchor = createdAnchors.at(-1);
    const downloadName = anchor !== undefined && 'download' in anchor && typeof anchor.download === 'string'
      ? anchor.download
      : undefined;
    const saved = blobs.at(-1)?.blob ?? null;
    const wireName = replyMarkdownFileName(runnerText, wireEnd);
    check('channel log filename derives from the wire END timestamp, not the log clock',
      wireEnd !== logAt && downloadName === wireName && downloadName !== replyMarkdownFileName(runnerText, logAt),
      String(downloadName));
    check('channel log save writes the raw source byte-for-byte',
      saved !== null && (await saved.text()) === runnerText,
      saved === null ? 'no blob created' : 'source mismatch');
    await act(async () => { root.unmount(); });

    // Legacy row (persisted before generatedAt): no fabricated clock — save
    // fails visibly through the central sink, copy stays available.
    const legacy = { id: 'log-legacy', at: logAt, kind: 'runner' as const, mappingId: 'm1', author: 'alpha', text: 'legacy reply' };
    const legacyErrors: string[] = [];
    const legacyAnchorsBefore = createdAnchors.length;
    const legacyBlobsBefore = blobs.length;
    const legacyMount = await mount({
      messages: [legacy],
      members,
      onReplyActionError: (message: string) => { legacyErrors.push(message); },
    });
    const legacyArticle = legacyMount.container.querySelector('.chat-session-message--assistant');
    const legacySave = legacyArticle?.querySelector('button[data-reply-action="save-markdown"]');
    check('legacy log reply still offers copy+save', legacySave !== null && legacySave !== undefined);
    await act(async () => { legacySave?.click(); });
    check('legacy log reply save fails through the central sink (no clock fabricated)',
      legacyErrors.length === 1 && legacyErrors[0] === 'Failed to save reply markdown'
        && createdAnchors.length === legacyAnchorsBefore && blobs.length === legacyBlobsBefore,
      JSON.stringify(legacyErrors));
    await act(async () => { legacyMount.root.unmount(); });

    // Persistence: generatedAt round-trips and the raw runner source is stored
    // complete (>8000 chars, no truncation).
    const overBound = `  ${'x'.repeat(9_000)}\r\n  `;
    const store = new Map<string, string>();
    const storage = {
      getItem: (key: string) => store.get(key) ?? null,
      setItem: (key: string, value: string) => { store.set(key, value); },
      removeItem: (key: string) => { store.delete(key); },
    };
    const savedLog = saveChannelLogs(storage, 'auth', {
      p1: [{ ...runnerMessage, text: overBound }, legacy],
    });
    const loadedLog = loadChannelLogs(storage, 'auth');
    const rows = loadedLog.logs.p1 ?? [];
    check('channel log persistence keeps the complete raw runner source',
      savedLog.ok && rows.length === 2
        && rows[0].text === overBound && rows[0].text.length === overBound.length,
      String(rows[0]?.text.length));
    check('channel log persistence round-trips the wire END timestamp',
      rows[0].generatedAt === wireEnd, String(rows[0]?.generatedAt));
    check('legacy persisted row stays without a wire clock (never fabricated)',
      rows[1].generatedAt === undefined);

    // A corrupt generatedAt must never corrupt the whole log: the raw reply
    // survives and only the unusable clock is dropped (no `at` fallback).
    const corruptStore = new Map<string, string>();
    const corruptStorage = {
      getItem: (key: string) => corruptStore.get(key) ?? null,
      setItem: (key: string, value: string) => { corruptStore.set(key, value); },
      removeItem: (key: string) => { corruptStore.delete(key); },
    };
    corruptStore.set(channelLogStorageKey('auth'), JSON.stringify({
      version: 1,
      projects: {
        p1: [{
          id: 'log-bad-clock', at: logAt, kind: 'runner', mappingId: 'm1', author: 'alpha',
          text: runnerText, generatedAt: 'not-a-clock',
        }],
      },
    }));
    const recovered = loadChannelLogs(corruptStorage, 'auth');
    const recoveredRow = recovered.logs.p1?.[0];
    check('invalid generatedAt is dropped but the raw reply text survives',
      recovered.ok && recoveredRow !== undefined
        && recoveredRow.text === runnerText && recoveredRow.generatedAt === undefined
        && recoveredRow.at === logAt,
      JSON.stringify(recoveredRow));
    if (recoveredRow !== undefined) {
      const recoveredErrors: string[] = [];
      const recoveredMount = await mount({
        messages: [recoveredRow],
        members,
        onReplyActionError: (message: string) => { recoveredErrors.push(message); },
      });
      const recoveredSave = recoveredMount.container
        .querySelector('.chat-session-message--assistant')?.querySelector('button[data-reply-action="save-markdown"]');
      await act(async () => { recoveredSave?.click(); });
      check('recovered row without a wire clock fails save through the central sink',
        recoveredErrors.length === 1 && recoveredErrors[0] === 'Failed to save reply markdown',
        JSON.stringify(recoveredErrors));
      await act(async () => { recoveredMount.root.unmount(); });
    }
  } finally {
    doc.createElement = originalCreate;
    URL.createObjectURL = originalCreateObjectURL;
    URL.revokeObjectURL = originalRevokeObjectURL;
  }
}

uninstallGlobals();
console.log(`\nchatBroadcastConversation.test: ${assertions} assertions, ${failures.length} failure(s)`);
if (failures.length > 0) {
  for (const failure of failures) console.error(`  FAIL ${failure}`);
  process.exit(1);
}
process.exit(0);
