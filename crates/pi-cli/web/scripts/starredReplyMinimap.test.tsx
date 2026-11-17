#!/usr/bin/env node
// Handler coverage for src/StarredReplyMinimap.tsx. Real createRoot clicks:
// each jump reports its own entry id (identical previews stay distinct), an
// empty list renders nothing, and a mobile expand/collapse round-trip still
// jumps the clicked id and returns to the collapsed rail.
//
// Bundled by the web test runner. This file does not execute itself.
import { createElement, useState } from 'react';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import type { StarredReply } from '../src/replyStars.ts';
import { StarredReplyMinimap } from '../src/StarredReplyMinimap.tsx';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: boolean, detail?: string) {
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
  preventDefault(): void;
  stopPropagation(): void;
};
type MiniNode = {
  nodeType: number;
  nodeName: string;
  tagName?: string;
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
  textContent: string;
  className: string;
  id: string;
  appendChild(node: MiniNode): MiniNode;
  removeChild(node: MiniNode): MiniNode;
  insertBefore(node: MiniNode, before: MiniNode | null): MiniNode;
  remove(): void;
  setAttribute(name: string, value: string): void;
  getAttribute(name: string): string | null;
  removeAttribute(name: string): void;
  hasAttribute(name: string): boolean;
  addEventListener(type: string, fn: MiniListener): void;
  removeEventListener(type: string, fn: MiniListener): void;
  dispatchEvent(event: { type: string }): boolean;
  click(): void;
  focus(): void;
  blur(): void;
  contains(node: MiniNode | null): boolean;
  querySelector(selector: string): MiniNode | null;
  querySelectorAll(selector: string): MiniNode[];
};
type MiniDocument = MiniNode & {
  nodeType: 9;
  documentElement: MiniNode;
  body: MiniNode;
  activeElement: MiniNode | null;
  defaultView: MiniWindow;
  createElement(tag: string): MiniNode;
  createElementNS(_ns: string, tag: string): MiniNode;
  createTextNode(text: string): MiniNode;
  createComment(text: string): MiniNode;
  createDocumentFragment(): MiniNode;
};
type MiniWindow = {
  document: MiniDocument;
  HTMLElement: new () => object;
  HTMLIFrameElement: new () => object;
  Element: new () => object;
  Node: new () => object;
  Text: new () => object;
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
      const child = parent.childNodes[i];
      child.parentNode = parent;
      child.previousSibling = parent.childNodes[i - 1] ?? null;
      child.nextSibling = parent.childNodes[i + 1] ?? null;
    }
  };
  const makeNode = (nodeType: number, nodeName: string, owner: MiniDocument): MiniNode => {
    const node = {
      nodeType,
      nodeName,
      tagName: nodeType === 1 ? nodeName : undefined,
      ownerDocument: owner,
      parentNode: null,
      childNodes: [] as MiniNode[],
      firstChild: null,
      lastChild: null,
      nextSibling: null,
      previousSibling: null,
      attributes: new Map<string, string>(),
      style: {},
      listeners: new Map(),
      text: '',
      get textContent() {
        if (nodeType === 3 || nodeType === 8) return node.text;
        return node.childNodes.map((child) => child.textContent).join('');
      },
      set textContent(value: string) {
        node.childNodes = [];
        relink(node);
        if (nodeType === 3 || nodeType === 8) node.text = value;
        else if (value !== '') node.appendChild(owner.createTextNode(value));
      },
      get className() { return node.getAttribute('class') ?? ''; },
      set className(value: string) { node.setAttribute('class', value); },
      get id() { return node.getAttribute('id') ?? ''; },
      set id(value: string) { node.setAttribute('id', value); },
      appendChild(child: MiniNode) { return node.insertBefore(child, null); },
      removeChild(child: MiniNode) {
        const index = node.childNodes.indexOf(child);
        if (index < 0) throw new Error('not a child');
        node.childNodes.splice(index, 1);
        child.parentNode = null;
        relink(node);
        return child;
      },
      insertBefore(child: MiniNode, before: MiniNode | null) {
        if (child.parentNode) child.parentNode.removeChild(child);
        if (child.nodeType === 11) {
          const nested = [...child.childNodes];
          child.childNodes = [];
          relink(child);
          for (const item of nested) node.insertBefore(item, before);
          return child;
        }
        const index = before === null ? node.childNodes.length : node.childNodes.indexOf(before);
        node.childNodes.splice(index < 0 ? node.childNodes.length : index, 0, child);
        relink(node);
        return child;
      },
      remove() { node.parentNode?.removeChild(node); },
      setAttribute(name: string, value: string) { node.attributes.set(name, String(value)); },
      getAttribute(name: string) { return node.attributes.has(name) ? node.attributes.get(name)! : null; },
      removeAttribute(name: string) { node.attributes.delete(name); },
      hasAttribute(name: string) { return node.attributes.has(name); },
      addEventListener(type: string, fn: MiniListener) {
        const set = node.listeners.get(type) ?? new Set();
        set.add(fn);
        node.listeners.set(type, set);
      },
      removeEventListener(type: string, fn: MiniListener) { node.listeners.get(type)?.delete(fn); },
      dispatchEvent(raw: { type: string }) {
        const payload: MiniEvent = {
          type: raw.type,
          target: node,
          currentTarget: null,
          bubbles: true,
          cancelable: true,
          defaultPrevented: false,
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
        for (const entry of [node, ...path.slice(1)]) {
          payload.currentTarget = entry;
          for (const fn of [...(entry.listeners.get(raw.type) ?? [])]) fn(payload);
          if (stopped) break;
        }
        return !payload.defaultPrevented;
      },
      click() { node.dispatchEvent({ type: 'click' }); },
      focus() { owner.activeElement = node; },
      blur() {},
      contains(other: MiniNode | null): boolean {
        if (other === node) return true;
        return node.childNodes.some((child) => child.contains(other));
      },
      querySelector(selector: string) { return node.querySelectorAll(selector)[0] ?? null; },
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
  const matches = (node: MiniNode, selector: string): boolean => {
    if (selector.startsWith('.')) return node.className.split(/\s+/).includes(selector.slice(1));
    if (selector.startsWith('#')) return node.id === selector.slice(1);
    if (selector.startsWith('[') && selector.endsWith(']')) {
      const body = selector.slice(1, -1);
      const eq = body.indexOf('=');
      if (eq < 0) return node.hasAttribute(body);
      return node.getAttribute(body.slice(0, eq)) === body.slice(eq + 1).replace(/^["']|["']$/g, '');
    }
    return node.tagName === selector.toUpperCase();
  };
  const document = makeNode(9, '#document', null as unknown as MiniDocument) as MiniDocument;
  document.ownerDocument = document;
  const html = makeNode(1, 'HTML', document);
  const body = makeNode(1, 'BODY', document);
  document.appendChild(html);
  html.appendChild(body);
  document.documentElement = html;
  document.body = body;
  document.activeElement = body;
  document.createElement = (tag: string) => makeNode(1, tag.toUpperCase(), document);
  document.createElementNS = (_ns: string, tag: string) => makeNode(1, tag.toUpperCase(), document);
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
  const windowObj: MiniWindow = {
    document,
    HTMLElement: FakeCtor,
    HTMLIFrameElement: FakeCtor,
    Element: FakeCtor,
    Node: FakeCtor,
    Text: FakeCtor,
    setTimeout: ((fn: () => void, ms?: number) => globalThis.setTimeout(fn, ms)) as typeof setTimeout,
    clearTimeout: ((id: number) => globalThis.clearTimeout(id)) as typeof clearTimeout,
    addEventListener() {},
    removeEventListener() {},
  };
  document.defaultView = windowObj;
  return { window: windowObj, document };
}

function installMiniDom() {
  const mini = createMiniDom();
  Object.defineProperty(globalThis, 'window', { configurable: true, writable: true, value: mini.window });
  Object.defineProperty(globalThis, 'document', { configurable: true, writable: true, value: mini.document });
  Object.defineProperty(globalThis, 'navigator', { configurable: true, writable: true, value: { userAgent: 'mini-dom' } });
  globalThis.HTMLElement = mini.window.HTMLElement as unknown as typeof HTMLElement;
  globalThis.Element = mini.window.Element as unknown as typeof Element;
  globalThis.Node = mini.window.Node as unknown as typeof Node;
  globalThis.Text = mini.window.Text as unknown as typeof Text;
  (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  return mini;
}

{
  const mini = installMiniDom();
  const host = mini.document.createElement('div');
  mini.document.body.appendChild(host);
  const root: Root = createRoot(host as unknown as Element);
  const jumped: string[] = [];
  const onJump = (entryId: string) => { jumped.push(entryId); };
  const unstarred: string[] = [];
  const onUnstar = (entryId: string) => { unstarred.push(entryId); };

  await act(async () => {
    root.render(createElement(StarredReplyMinimap, { replies: [], onJump, onUnstar }));
  });
  check('empty replies render no minimap', host.querySelector('nav') === null && host.textContent === '');

  const shared = 'same preview <script>alert(1)</script>';
  const replies: readonly StarredReply[] = [
    { entryId: 'entry-early', itemId: 'item-early', preview: shared, transcriptIndex: 4 },
    { entryId: 'entry-late', itemId: 'item-late', preview: shared, transcriptIndex: 18 },
  ];
  await act(async () => {
    root.render(createElement(StarredReplyMinimap, { replies, onJump, onUnstar }));
  });
  const nav = host.querySelector('nav');
  const buttons = host.querySelectorAll('.starred-reply-minimap__jump');
  check('starred replies render one jump per entry', nav !== null && buttons.length === 2);
  check(
    'duplicate previews stay distinct entries in given order',
    buttons.map((button) => button.getAttribute('data-entry-id')).join(',') === 'entry-early,entry-late'
      && buttons.every((button) => button.textContent.includes(shared)),
  );
  check(
    'preview is text, not parsed markup',
    host.querySelector('script') === null
      && buttons[0]?.getAttribute('title') === shared
      && buttons[0]?.getAttribute('aria-label') === `Jump to ${shared}`,
  );
  check('rail starts collapsed for the narrow layout', nav?.className.split(/\s+/).includes('is-collapsed') === true);

  const expand = host.querySelectorAll('button').find((button) => button.getAttribute('aria-label') === 'Show starred replies') ?? null;
  check('collapsed rail offers Show starred replies', expand !== null && expand.getAttribute('aria-expanded') === 'false');
  await act(async () => { expand?.click(); });
  check('expand opens the rail', nav?.className.split(/\s+/).includes('is-collapsed') === false && expand?.getAttribute('aria-expanded') === 'true');
  await act(async () => { expand?.click(); });
  check('second expand click collapses the rail', nav?.className.split(/\s+/).includes('is-collapsed') === true && expand?.getAttribute('aria-expanded') === 'false');

  await act(async () => { expand?.click(); });
  await act(async () => { host.querySelectorAll('.starred-reply-minimap__jump')[1]?.click(); });
  check('jump reports the clicked entry id, not the twin preview', jumped.length === 1 && jumped[0] === 'entry-late', JSON.stringify(jumped));
  check('jump collapses the rail', nav?.className.split(/\s+/).includes('is-collapsed') === true);

  // A rail row owns TWO independent controls: the preview jumps, the gold ★
  // unstars. Unstar is a pure toggle — never a jump, never a rail collapse —
  // and the row is parent-controlled, so the component cannot fake-delete it.
  await act(async () => { expand?.click(); });
  const jumps = host.querySelectorAll('.starred-reply-minimap__jump');
  const unstars = host.querySelectorAll('.starred-reply-minimap__unstar');
  check('each starred row renders one separate unstar button', jumps.length === 2 && unstars.length === 2);
  check(
    'unstar is a pure toggle: labelled for its own preview, no entry-id stamp',
    unstars.length === 2
      && unstars.every((button, index) => button !== jumps[index]
        && button.parentNode === jumps[index]?.parentNode
        && button.getAttribute('data-entry-id') === null
        && button.getAttribute('aria-label') === `Unstar ${shared}`
        && button.getAttribute('title') === 'Unstar reply'
        && (button.textContent || '').trim() === '×'),
    JSON.stringify(unstars.map((button) => button.getAttribute('aria-label'))),
  );
  await act(async () => { unstars[1]?.click(); });
  check('unstar reports its own entry id to the parent toggle', unstarred.length === 1 && unstarred[0] === 'entry-late', JSON.stringify(unstarred));
  check('unstar never jumps', jumped.length === 1 && jumped[0] === 'entry-late', JSON.stringify(jumped));
  check('unstar leaves the rail expanded instead of collapsing like a jump', nav?.className.split(/\s+/).includes('is-collapsed') === false);
  check('an ignored unstar cannot fake-delete the row', host.querySelectorAll('.starred-reply-minimap__jump').length === 2);

  const blank: readonly StarredReply[] = [
    { entryId: 'entry-blank', itemId: 'item-blank', preview: '   ', transcriptIndex: 7 },
  ];
  await act(async () => {
    root.render(createElement(StarredReplyMinimap, { replies: blank, onJump, onUnstar }));
  });
  const blankJump = host.querySelector('.starred-reply-minimap__jump');
  check(
    'blank preview shows Reply N and Jump to Reply N',
    blankJump?.getAttribute('aria-label') === 'Jump to Reply 8'
      && blankJump.getAttribute('title') === 'Reply 8'
      && blankJump.textContent.includes('Reply 8')
      && !blankJump.textContent.includes('Jump to'),
  );
  const blankUnstar = host.querySelector('.starred-reply-minimap__unstar');
  check(
    'blank preview unstar names Reply N and carries no entry stamp',
    blankUnstar?.getAttribute('aria-label') === 'Unstar Reply 8'
      && blankUnstar.getAttribute('title') === 'Unstar reply'
      && !blankUnstar.hasAttribute('data-entry-id'),
  );
  await act(async () => { blankJump?.click(); });
  check('blank preview still jumps its entry id', jumped.length === 2 && jumped[1] === 'entry-blank', JSON.stringify(jumped));

  function Harness() {
    const [current, setCurrent] = useState(replies);
    return createElement(StarredReplyMinimap, {
      replies: current,
      onJump: (entryId: string) => { jumped.push(entryId); },
      onUnstar: (entryId: string) => {
        unstarred.push(entryId);
        setCurrent(current.filter((item) => item.entryId !== entryId));
      },
    });
  }
  await act(async () => { root.render(createElement(Harness)); });
  await act(async () => { host.querySelectorAll('.starred-reply-minimap__unstar')[0]?.click(); });
  const remaining = host.querySelectorAll('.starred-reply-minimap__jump');
  check(
    'parent-driven unstar removal leaves the other entry',
    remaining.length === 1 && remaining[0]?.getAttribute('data-entry-id') === 'entry-late',
  );
  await act(async () => { host.querySelectorAll('.starred-reply-minimap__unstar')[0]?.click(); });
  check('unstarring the last reply renders nothing', host.querySelector('nav') === null);

  await act(async () => { root.unmount(); });
}

if (failures.length > 0) {
  console.error(`starredReplyMinimap: ${failures.length} assertion(s) failed:`);
  for (const failure of failures) console.error(`  - ${failure}`);
  process.exit(1);
}
console.log(`starredReplyMinimap: ${ran} assertions passed`);
process.exit(0);
