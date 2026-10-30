#!/usr/bin/env node
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createElement, useState, act } from 'react';
import { createRoot } from 'react-dom/client';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import { SteeringQueue } from '../src/SteeringQueue.tsx';
import {
  applyQueueUpdatePreviews,
  applyStateQueuedSteering,
  collapseSteeringPreview,
  dropConsumedPreview,
  dropLastMatchingPreview,
  dropSteeringAt,
  dropQueuedPreviewAfterCancel,
  settleQueueCommand,
  runExclusiveQueueCommand,
  steeringPreviewsFromMessages,
  steeringPreviewsFromState,
  userContentPreview,
} from '../src/steeringQueueHelpers.ts';

const failures: string[] = [];
let assertions = 0;
function check(name: string, condition: boolean, detail = '') {
  assertions += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

check('text block preview', userContentPreview([{ type: 'text', text: 'fix the test' }]) === 'fix the test');
check('image-only preview', userContentPreview([{ type: 'image', mimeType: 'image/png', data: 'xx' }]) === '(image)');
check('empty content is blank', userContentPreview([]) === '');
check('plain string content', userContentPreview('hello') === 'hello');

const fromUpdate = steeringPreviewsFromMessages([
  { role: 'user', content: [{ type: 'text', text: 'first' }] },
  { role: 'assistant', content: [{ type: 'text', text: 'nope' }] },
  { role: 'user', content: [{ type: 'text', text: 'second\nline' }] },
]);
check('queue_update keeps user order', fromUpdate.length === 2 && fromUpdate[0] === 'first' && fromUpdate[1] === 'second\nline');
check('non-array messages are empty', steeringPreviewsFromMessages(null).length === 0);

check('get_state queuedSteering', JSON.stringify(steeringPreviewsFromState({ queuedSteering: ['a', 'b'] })) === JSON.stringify(['a', 'b']));
check('missing queuedSteering is null', steeringPreviewsFromState({ pendingMessageCount: 2 }) === null);
check('non-strings dropped', JSON.stringify(steeringPreviewsFromState({ queuedSteering: ['ok', 1, null] })) === JSON.stringify(['ok']));

check('collapse whitespace', collapseSteeringPreview('first\n  second') === 'first second');

const live = applyQueueUpdatePreviews({ s1: ['old'] }, 's1', []);
check('empty queue_update clears the tray', JSON.stringify(live.s1) === JSON.stringify([]));

const afterLive = applyStateQueuedSteering(live, 's1', ['resurrect'], true);
check('stale get_state cannot resurrect after empty queue_update', JSON.stringify(afterLive.s1) === JSON.stringify([]));

const beforeLive = applyStateQueuedSteering({ s1: ['old'] }, 's1', ['a'], false);
check('get_state queuedSteering applies before any queue_update', JSON.stringify(beforeLive.s1) === JSON.stringify(['a']));

const omitted = applyStateQueuedSteering({ s1: ['old'] }, 's1', null, false);
check('omitted queuedSteering does not write', JSON.stringify(omitted.s1) === JSON.stringify(['old']));

const emptied = applyStateQueuedSteering({ s1: ['old'] }, 's1', [], false);
check('empty get_state queuedSteering clears when no live queue_update', JSON.stringify(emptied.s1) === JSON.stringify([]));
check('reconnect generation applies queuedSteering after a live latch', JSON.stringify(applyStateQueuedSteering(live, 's1', [], false).s1) === JSON.stringify([]));

const optimistic = applyStateQueuedSteering({ s1: ['steer-a', 'steer-b'] }, 's1', [], false, true);
check('empty get_state does not wipe optimistic rows while the turn is streaming', JSON.stringify(optimistic.s1) === JSON.stringify(['steer-a', 'steer-b']));
const settledClear = applyStateQueuedSteering({ s1: ['steer-a'] }, 's1', [], false, false);
check('empty get_state clears optimistic rows after the turn settles', JSON.stringify(settledClear.s1) === JSON.stringify([]));
const liveEmpty = applyQueueUpdatePreviews({ s1: ['steer-a', 'steer-b'] }, 's1', ['steer-a', 'steer-b']);
check('queue_update with N user messages keeps N previews', JSON.stringify(liveEmpty.s1) === JSON.stringify(['steer-a', 'steer-b']));

check('drop in-range index', JSON.stringify(dropSteeringAt(['a', 'b'], 0)) === JSON.stringify(['b']));
const outOfRange = ['a'];
check('drop out-of-range is identity', dropSteeringAt(outOfRange, 4) === outOfRange);
check('cancel on empty queue is identity', dropSteeringAt([], 0).length === 0);
check('drop index 0 clears UI', JSON.stringify(dropSteeringAt(['consumed'], 0)) === JSON.stringify([]));
check('failed steer drops the last matching optimistic row', JSON.stringify(dropLastMatchingPreview(['a', 'b', 'a'], 'a')) === JSON.stringify(['a', 'b']));
check('failed steer with no matching row is identity', dropLastMatchingPreview(['kept'], 'other')[0] === 'kept');

// The tray must drop a row the moment the queued message is CONSUMED — i.e.
// it enters the chat as a user message — not only when the next authoritative
// queue_update happens to arrive (the consuming turn boundary publishes the
// pre-drain queue, so without an active drop the row lingers for the whole
// turn).
check('consumed steer drops the first matching row (FIFO head)', JSON.stringify(dropConsumedPreview(['a', 'b'], 'a')) === JSON.stringify(['b']));
check('consumed steer with duplicate texts drops only the head', JSON.stringify(dropConsumedPreview(['x', 'x', 'y'], 'x')) === JSON.stringify(['x', 'y']));
check('consumed steer with no matching row is identity', dropConsumedPreview(['kept'], 'other') === null || JSON.stringify(dropConsumedPreview(['kept'], 'other')) === JSON.stringify(['kept']));
check('consumed steer on an empty tray is identity', JSON.stringify(dropConsumedPreview([], 'a')) === JSON.stringify([]));

check(
  'cancel ACK at the original index drops that slot',
  JSON.stringify(dropQueuedPreviewAfterCancel(['a', 'b', 'c'], 1, 'b')) === JSON.stringify(['a', 'c']),
);
check(
  'cancel ACK relocates the captured preview after a concurrent consume',
  JSON.stringify(dropQueuedPreviewAfterCancel(['b', 'c'], 0, 'c')) === JSON.stringify(['b']),
);
check(
  'cancel ACK is identity when the captured preview is already gone',
  JSON.stringify(dropQueuedPreviewAfterCancel(['kept'], 0, 'gone')) === JSON.stringify(['kept']),
);

{
  let rows = ['keep after throw'];
  let reported = '';
  let dropped = false;
  await settleQueueCommand(
    async () => { throw new Error('disconnected'); },
    1,
    () => 1,
    (error) => { reported = error instanceof Error ? error.message : String(error); },
    () => { dropped = true; rows = []; },
  );
  check('throwing cancel never drops the row', rows.length === 1 && rows[0] === 'keep after throw' && dropped === false);
  check('throwing cancel reports the failure', reported === 'disconnected');
}

{
  let rows = ['keep after refuse'];
  let reported = '';
  await settleQueueCommand(
    async () => { throw new Error('refused'); },
    3,
    () => 3,
    (error) => { reported = error instanceof Error ? error.message : String(error); },
    () => { rows = []; },
  );
  check('refused cancel keeps the local row', rows[0] === 'keep after refuse' && reported === 'refused');
}

{
  let rows = ['old', 'extra'];
  await settleQueueCommand(
    async () => {},
    1,
    () => 2,
    () => { throw new Error('must not report'); },
    () => { rows = dropQueuedPreviewAfterCancel(rows, 0, 'old'); },
  );
  check(
    'successful cancel does not drop after a newer queue_update',
    JSON.stringify(rows) === JSON.stringify(['old', 'extra']),
  );
}

{
  let rows = ['queued'];
  await settleQueueCommand(
    async () => {},
    4,
    () => 4,
    () => { throw new Error('must not report'); },
    () => { rows = dropQueuedPreviewAfterCancel(rows, 0, 'queued'); },
  );
  check('successful cancel drops only when generation is unchanged', JSON.stringify(rows) === JSON.stringify([]));
}

{
  const busyRef = { current: false };
  let busy = false;
  const setBusy = (next: boolean) => { busy = next; };
  const commands: Array<{ type: string; index: number }> = [];
  let settle: (() => void) | null = null;
  const send = (index: number) => new Promise<void>((resolve) => {
    commands.push({ type: 'queue_cancel', index });
    settle = () => resolve();
  });
  const cancelQueuedAt = (index: number) => runExclusiveQueueCommand(busyRef, setBusy, async () => {
    const gen = 0;
    await settleQueueCommand(
      () => send(index),
      gen,
      () => 0,
      () => { throw new Error('must not report'); },
      () => {},
    );
  });
  const first = cancelQueuedAt(0);
  cancelQueuedAt(1);
  check(
    'overlapping cancel A then B sends only one queue_cancel',
    commands.length === 1 && commands[0]?.type === 'queue_cancel' && commands[0]?.index === 0,
    JSON.stringify(commands),
  );
  check('busy lock is held until ACK', busy === true && busyRef.current === true);
  settle?.();
  await first;
  check('lock releases after settle', busy === false && busyRef.current === false);
  const second = cancelQueuedAt(0);
  check(
    'after settle a second cancel may send',
    commands.length === 2 && commands[1]?.type === 'queue_cancel' && commands[1]?.index === 0,
    JSON.stringify(commands),
  );
  settle?.();
  await second;
}

{
  const busyRef = { current: false };
  let busy = false;
  let rows = ['A', 'B', 'C'];
  let reported = '';
  await runExclusiveQueueCommand(busyRef, (next) => { busy = next; }, async () => {
    await settleQueueCommand(
      async () => { throw new Error('refused'); },
      1,
      () => 1,
      (error) => { reported = error instanceof Error ? error.message : String(error); },
      () => { rows = []; },
    );
  });
  check('refused exclusive cancel keeps the local row', rows.length === 3 && rows[0] === 'A');
  check('refused exclusive cancel reports the failure', reported === 'refused');
  check('refused exclusive cancel releases the lock', busy === false && busyRef.current === false);
  let second = false;
  await runExclusiveQueueCommand(busyRef, (next) => { busy = next; }, async () => {
    second = true;
  });
  check('after refused settle a later cancel may proceed', second === true && busy === false);
}

{
  const busyRef = { current: false };
  let busy = false;
  const commands: string[] = [];
  let settle: (() => void) | null = null;
  const send = (type: string) => new Promise<void>((resolve) => {
    commands.push(type);
    settle = () => resolve();
  });
  const sendQueuedNow = () => runExclusiveQueueCommand(busyRef, (next) => { busy = next; }, async () => {
    await settleQueueCommand(() => send('queue_send_now'), 0, () => 0, () => {});
  });
  const cancelQueuedAt = () => runExclusiveQueueCommand(busyRef, (next) => { busy = next; }, async () => {
    await settleQueueCommand(() => send('queue_cancel'), 0, () => 0, () => {});
  });
  const first = sendQueuedNow();
  cancelQueuedAt();
  check(
    'overlapping send-now then cancel emits only send-now',
    commands.length === 1 && commands[0] === 'queue_send_now',
    JSON.stringify(commands),
  );
  settle?.();
  await first;
  check('send-now lock releases after settle', busy === false && busyRef.current === false);
}


{
  const tray = { s: ['my steer'] };
  const wiped = applyStateQueuedSteering(tray, 's', [], false, false, false);
  check('settled empty get_state still clears an acked tray', JSON.stringify(wiped.s) === JSON.stringify([]));
  const kept = applyStateQueuedSteering(tray, 's', [], false, false, true);
  check('settled empty get_state cannot wipe an optimistic row while its submit is in flight',
    JSON.stringify(kept.s) === JSON.stringify(['my steer']));
  const filled = applyStateQueuedSteering(tray, 's', ['my steer'], false, false, true);
  check('authoritative list still reconciles once the submit settles',
    JSON.stringify(filled.s) === JSON.stringify(['my steer']));
}

{
  // Full tray lifecycle for one steering message, driven only by the wire
  // signals the client actually receives: optimistic submit add,
  // authoritative queue_update reconciles, the consumed message entering the
  // chat (user message_start), and the settle queue_update.
  let tray: string[] = [];
  tray = [...tray, 'fix the test']; // optimistic submit add
  tray = applyQueueUpdatePreviews({ s: tray }, 's', ['fix the test', 'keep going']).s ?? [];
  check(
    'queue_update with the queued entries keeps them',
    JSON.stringify(tray) === JSON.stringify(['fix the test', 'keep going']),
    JSON.stringify(tray),
  );
  const consumed = userContentPreview([{ type: 'text', text: 'fix the test' }]);
  tray = dropConsumedPreview(tray, consumed);
  check(
    'queue_update consuming an entry removes it from the tray when it enters the chat',
    JSON.stringify(tray) === JSON.stringify(['keep going']),
    JSON.stringify(tray),
  );
  tray = applyQueueUpdatePreviews({ s: tray }, 's', ['keep going']).s ?? [];
  check('the consuming boundary queue_update keeps the already-dropped row dropped', JSON.stringify(tray) === JSON.stringify(['keep going']));
  tray = applyQueueUpdatePreviews({ s: tray }, 's', []).s ?? [];
  check('settle queue_update clears the remaining row', JSON.stringify(tray) === JSON.stringify([]));
}


const here = dirname(fileURLToPath(import.meta.url));
const styles = readFileSync(join(here, '../src/styles.css'), 'utf8');

{
  const appSource = readFileSync(join(here, '../src/App.tsx'), 'utf8');
  check(
    'App drops the matching tray row when a steered message enters the chat',
    appSource.includes('dropConsumedPreview')
      && appSource.includes('writeSteering')
      && !appSource.includes('planConsumedSteeringTranscriptInsert')
      && !appSource.includes('insertConsumedSteeringTexts'),
  );
}

const rowRuleStart = styles.indexOf('.steering-queue__row {');
const rowRule = rowRuleStart >= 0 ? styles.slice(rowRuleStart, styles.indexOf('}', rowRuleStart)) : '';
const textRuleStart = styles.indexOf('.steering-queue__text {');
const textRule = textRuleStart >= 0 ? styles.slice(textRuleStart, styles.indexOf('}', textRuleStart)) : '';
const phoneMqStart = styles.indexOf('@media (max-width: 768px) {');
const phoneMq = phoneMqStart >= 0 ? styles.slice(phoneMqStart, styles.indexOf('\n}\n\n@media (max-width: 480px)', phoneMqStart)) : '';
check('steering rows are one compact 36-44px line', rowRule.includes('min-height: 36px') && rowRule.includes('max-height: 44px') && rowRule.includes('display: flex'));
check('steering preview ellipsizes on one line', textRule.includes('text-overflow: ellipsis') && textRule.includes('white-space: nowrap') && textRule.includes('min-width: 0'));
check('no stacked steering head card', !styles.includes('.steering-queue__head'));
check('phone 390 steering rows are 44px tap targets', phoneMq.includes('.steering-queue__row') && phoneMq.includes('min-height: 44px') && phoneMq.includes('min-width: 44px'));
const footerPhoneStart = phoneMq.indexOf('\n  footer {');
const footerPhone = footerPhoneStart >= 0 ? phoneMq.slice(footerPhoneStart, phoneMq.indexOf('}', footerPhoneStart)) : '';
check('phone footer overflow stays visible so the tray overlay is not clipped', footerPhone.includes('overflow: visible') && !footerPhone.includes('overflow-y: auto'));
{
  const rowStart = styles.indexOf('.composer-status-row {');
  const row = rowStart >= 0 ? styles.slice(rowStart, styles.indexOf('}', rowStart)) : '';
  const queueStart = styles.indexOf('#steering-queue {');
  const queue = queueStart >= 0 ? styles.slice(queueStart, styles.indexOf('}', queueStart)) : '';
  const stripStart = styles.indexOf('.composer-status-row #activity-strip {');
  const strip = stripStart >= 0 ? styles.slice(stripStart, styles.indexOf('}', stripStart)) : '';
  check('composer status row is one flex line', row.includes('display: flex') && row.includes('align-items: flex-start'));
  check('activity strip stays on the left of the status row', strip.includes('flex: 0 1 auto') && strip.includes('max-width: min(42%, 16rem)'));
  check('steering tray is in-flow on the status row, not an overlay', queue.includes('flex: 1 1 auto') && !queue.includes('position: absolute') && !queue.includes('bottom: calc(100%'));
}
{
  const appSource = readFileSync(join(here, '../src/App.tsx'), 'utf8');
  const rowAt = appSource.indexOf('id="composer-status-row"');
  const stripAt = appSource.indexOf('id="activity-strip"');
  const queueAt = appSource.indexOf('<SteeringQueue');
  const rowEnd = appSource.indexOf('id="composer-row"', rowAt);
  check(
    '/web mounts Working and Steering on one composer status row',
    rowAt >= 0 && stripAt > rowAt && queueAt > stripAt && queueAt < rowEnd && rowEnd > 0,
  );
  const sendAt = appSource.indexOf('const sendSteeringNow');
  const cancelAt = appSource.indexOf('const cancelSteeringAt');
  const attachAt = appSource.indexOf('composer: file attachments', cancelAt);
  const handlers = sendAt >= 0 && attachAt > sendAt ? appSource.slice(sendAt, attachAt) : '';
  check(
    '/web send/cancel serialize through exclusive settle, no optimistic drop',
    handlers.includes('runExclusiveQueueCommand')
      && handlers.includes('settleQueueCommand')
      && handlers.includes('dropQueuedPreviewAfterCancel')
      && handlers.includes("type: 'queue_cancel'")
      && handlers.includes("type: 'queue_send_now'")
      && !handlers.includes('dropSteeringAt')
      && !handlers.includes("void sendCommand({ type: 'queue_cancel'"),
  );
  check(
    '/web queue_update advances the cancel generation fence',
    appSource.includes('steeringQueueGenRef.current += 1'),
  );
}

const trayHtml = renderToStaticMarkup(createElement(SteeringQueue, {
  previews: ['queued steer that is quite long'],
  busy: false,
  onSendNow: () => {},
  onCancelAt: () => {},
}));
check('tray is one row with inline send-all and remove', trayHtml.includes('steering-queue__row') && trayHtml.includes('aria-label="Send all now"') && trayHtml.includes('aria-label="Remove steering message"') && trayHtml.includes('Steering') && trayHtml.includes('queued steer'));
check('tray has no stacked head', !trayHtml.includes('steering-queue__head') && !trayHtml.includes('steering-queue__count'));
const nTrayHtml = renderToStaticMarkup(createElement(SteeringQueue, {
  previews: ['first steer', 'second steer', 'third steer'],
  busy: false,
  onSendNow: () => {},
  onCancelAt: () => {},
}));
check(
  'N queued messages render N tray rows with send-now and cancel',
  (nTrayHtml.match(/steering-queue__row"/g) || []).length === 3
    && (nTrayHtml.match(/aria-label="Send all now"/g) || []).length === 3
    && (nTrayHtml.match(/aria-label="Remove steering message"/g) || []).length === 3
    && nTrayHtml.includes('first steer')
    && nTrayHtml.includes('second steer')
    && nTrayHtml.includes('third steer'),
);

const failedTrayHtml = renderToStaticMarkup(createElement(SteeringQueue, {
  previews: ['queued steer'],
  failedPreviews: ['rejected steer'],
  onDismissFailedAt: () => {},
  busy: false,
  onSendNow: () => {},
  onCancelAt: () => {},
}));
check(
  'rejected steer renders a dismissible failed row next to the live queue',
  failedTrayHtml.includes('steering-queue__row--failed')
    && failedTrayHtml.includes('aria-label="Dismiss failed steering message"')
    && failedTrayHtml.includes('Failed')
    && failedTrayHtml.includes('rejected steer')
    && failedTrayHtml.includes('queued steer'),
);

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
  stopImmediatePropagation(): void;
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
  innerHTML: string;
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
  setAttributeNS(_ns: string | null, name: string, value: string): void;
  getAttributeNS(_ns: string | null, name: string): string | null;
  addEventListener(type: string, fn: MiniListener, options?: boolean | { capture?: boolean }): void;
  removeEventListener(type: string, fn: MiniListener, options?: boolean | { capture?: boolean }): void;
  dispatchEvent(event: { type: string; target?: MiniNode }): boolean;
  click(): void;
  focus(): void;
  blur(): void;
  contains(node: MiniNode | null): boolean;
  querySelector(selector: string): MiniNode | null;
  querySelectorAll(selector: string): MiniNode[];
  cloneNode(deep?: boolean): MiniNode;
};
type MiniDocument = MiniNode & {
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
  setTimeout: (fn: () => void, ms?: number) => number;
  clearTimeout: (id: number) => void;
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
      listeners: new Map(),
      text: '',
      innerHTML: '',
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
        if (value !== '') node.appendChild(ownerDocument.createTextNode(value));
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
        const set = node.listeners.get(key) ?? new Set();
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
          preventDefault() { payload.defaultPrevented = true; },
          stopPropagation() { stopped = true; },
          stopImmediatePropagation() { stopped = true; immediate = true; },
        };
        let stopped = false;
        let immediate = false;
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
          for (const fn of [...set]) {
            fn(payload);
            if (immediate) return;
          }
        };
        for (let i = path.length - 1; i >= 0; i -= 1) {
          fire(path[i], `${raw.type}-capture`);
          if (stopped) return !payload.defaultPrevented;
        }
        fire(node, raw.type);
        if (stopped) return !payload.defaultPrevented;
        for (let i = 1; i < path.length; i += 1) {
          fire(path[i], raw.type);
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
      cloneNode(deep = false) {
        const clone = makeNode(node.nodeType, node.nodeName, ownerDocument);
        clone.namespaceURI = node.namespaceURI;
        clone.text = node.text;
        for (const [name, value] of node.attributes) clone.attributes.set(name, value);
        if (deep) {
          for (const child of node.childNodes) clone.appendChild(child.cloneNode(true));
        }
        return clone;
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
      const name = body.slice(0, eq);
      const raw = body.slice(eq + 1).replace(/^["']|["']$/g, '');
      return node.getAttribute(name) === raw;
    }
    return node.tagName === selector.toUpperCase();
  };
  const document = makeNode(9, '#document', null) as MiniDocument;
  const ownerDocument = document;
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
    const node = makeNode(1, tag.toUpperCase(), document);
    node.namespaceURI = ns;
    return node;
  };
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
    setTimeout: (fn: () => void) => globalThis.setTimeout(fn, 0) as unknown as number,
    clearTimeout: (id: number) => globalThis.clearTimeout(id),
    addEventListener() {},
    removeEventListener() {},
  };
  document.defaultView = windowObj;
  return { window: windowObj, document };
}

function TrayHarness(props: {
  sessionId: string;
  previews: string[];
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>;
}) {
  const [busy, setBusy] = useState(false);
  const onSendNow = () => {
    if (!props.sessionId || busy) return;
    setBusy(true);
    void props.sendCommand({ type: 'queue_send_now', sessionId: props.sessionId }).finally(() => {
      setBusy(false);
    });
  };
  return createElement(SteeringQueue, {
    previews: props.previews,
    busy,
    onSendNow,
    onCancelAt: () => {},
  });
}

{
  const mini = createMiniDom();
  Object.defineProperty(globalThis, 'window', { configurable: true, writable: true, value: mini.window });
  Object.defineProperty(globalThis, 'document', { configurable: true, writable: true, value: mini.document });
  globalThis.HTMLElement = mini.window.HTMLElement as unknown as typeof HTMLElement;
  globalThis.HTMLIFrameElement = mini.window.HTMLIFrameElement as unknown as typeof HTMLIFrameElement;
  globalThis.HTMLButtonElement = mini.window.HTMLButtonElement as unknown as typeof HTMLButtonElement;
  globalThis.Element = mini.window.Element as unknown as typeof Element;
  globalThis.Node = mini.window.Node as unknown as typeof Node;
  (globalThis as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
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

  const commands: Record<string, unknown>[] = [];
  const sessionId = 'session-steer-1';
  let previews = ['queued steer'];
  let settleSend: (() => void) | null = null;
  const sendCommand = (command: Record<string, unknown>): Promise<unknown> => {
    commands.push({ ...command });
    return new Promise((resolve) => {
      settleSend = () => resolve({});
    });
  };
  const container = mini.document.createElement('div');
  mini.document.body.appendChild(container);
  const root = createRoot(container as unknown as Element);
  const render = (next: string[]): void => {
    previews = next;
    root.render(createElement(TrayHarness, { sessionId, previews, sendCommand }));
  };
  await act(async () => {
    render(previews);
  });
  const send = container.querySelector('[aria-label="Send all now"]');
  check('send-now control is present', send !== null, container.textContent);
  await act(async () => {
    send?.click();
  });
  check(
    'click emits queue_send_now for the painted session',
    commands.length === 1 && commands[0]?.type === 'queue_send_now' && commands[0]?.sessionId === sessionId,
    JSON.stringify(commands),
  );
  check(
    'click does not clear the tray row',
    container.textContent.includes('queued steer') && container.querySelector('.steering-queue__row') !== null,
    container.textContent,
  );
  await act(async () => {
    settleSend?.();
    await Promise.resolve();
    await Promise.resolve();
  });
  check(
    'RPC ack still leaves the row until queue_update',
    container.querySelector('.steering-queue__row') !== null && container.textContent.includes('queued steer'),
    container.textContent,
  );
  const cleared = applyQueueUpdatePreviews({ [sessionId]: previews }, sessionId, []);
  await act(async () => {
    render(cleared[sessionId] ?? []);
  });
  check(
    'authoritative empty queue_update clears the tray',
    container.querySelector('.steering-queue__row') === null && !container.textContent.includes('queued steer'),
    container.textContent,
  );
  root.unmount();
}

if (failures.length > 0) {
  console.error(`FAIL ${failures.length}/${assertions}\n${failures.map((item) => `- ${item}`).join('\n')}`);
  process.exit(1);
}
console.log(`ok ${assertions}`);
