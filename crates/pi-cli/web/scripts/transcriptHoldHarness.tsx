import { createElement, useState, useRef, useCallback } from 'react';
import { createRoot } from 'react-dom/client';
import { TranscriptSurface } from '../src/App.tsx';
import { messagesToItems } from '../src/transcript.ts';
import { runtimeKey } from '../src/sessionSwitch.ts';
import { tailProjectionStart } from '../src/transcriptView.ts';

const identity = { sessionId: 'hold-s', runtimeIncarnation: 'inc' };
const rk = runtimeKey(identity);
const items = messagesToItems(Array.from({ length: 800 }, (_, i) => ({
  id: `m${i}`,
  message: {
    role: i % 2 === 0 ? 'user' : 'assistant',
    content: [{ type: 'text', text: `msg ${i} ${'word '.repeat(12)}` }],
    timestamp: i,
  },
})));
const indexById = new Map(items.map((item, i) => [item.id, i]));

function ids() {
  return Array.from(document.querySelectorAll('[data-item-id]')).map((el) => el.getAttribute('data-item-id'));
}
function surfaceSnap() {
  const surface = document.querySelector('.transcript-surface') as HTMLElement | null;
  const list = ids();
  return {
    anchor: surface?.dataset.liveAnchor,
    start: surface?.dataset.windowStart,
    follow: surface?.dataset.liveFollow,
    head: list[0],
    tail: list[list.length - 1],
    n: list.length,
  };
}
function frame() {
  return new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
}

function Harness() {
  const [anchorIndex, setAnchorIndex] = useState(80);
  const [active, setActive] = useState(true);
  const [follow, setFollow] = useState(false);
  const scrollRef = useRef(document.getElementById('transcript'));
  const getViewport = useCallback(() => ({
    follow,
    anchorItemId: items[anchorIndex]?.id ?? null,
    anchorOffsetPx: 0,
    fallbackScrollTop: 0,
    openFoldKeys: new Set<string>(),
  }), [anchorIndex, follow]);
  const getAnchorIndex = useCallback((_sessionId: string, anchorItemId: string) => indexById.get(anchorItemId) ?? -1, []);
  (window as unknown as { __ctl: unknown }).__ctl = { setAnchorIndex, setActive, setFollow };
  return createElement(TranscriptSurface, {
    rk,
    sessionId: identity.sessionId,
    items,
    active,
    connState: 'on',
    workspaceCwd: '/w',
    initialStart: tailProjectionStart(items.length),
    itemsVersion: 1,
    getViewport,
    scrollRef,
    pinIfPinned: () => {},
    onContentEl: () => {},
    onCommit: () => {},
    getAnchorIndex,
  });
}

const root = createRoot(document.getElementById('transcript')!);
root.render(createElement(Harness));

type Ctl = {
  setAnchorIndex: (n: number) => void;
  setActive: (n: boolean) => void;
  setFollow: (n: boolean) => void;
};
function ctl(): Ctl {
  return (window as unknown as { __ctl: Ctl }).__ctl;
}

async function runRetainRelease() {
  await frame();
  const snap0 = surfaceSnap();
  const firstItem = document.querySelector('[data-item-id]');
  const sel = document.getSelection();
  const range = document.createRange();
  if (firstItem) range.selectNodeContents(firstItem);
  sel?.removeAllRanges();
  if (firstItem) sel?.addRange(range);
  await frame();
  ctl().setAnchorIndex(400);
  await frame();
  await new Promise((r) => setTimeout(r, 50));
  const snapHold = surfaceSnap();
  sel?.removeAllRanges();
  await frame();
  ctl().setAnchorIndex(500);
  await frame();
  await new Promise((r) => setTimeout(r, 80));
  const snapRelease = surfaceSnap();
  const composer = document.getElementById('composer') as HTMLTextAreaElement;
  composer.focus();
  composer.select();
  ctl().setAnchorIndex(80);
  await frame();
  await new Promise((r) => setTimeout(r, 80));
  return {
    snap0,
    snapHold,
    snapRelease,
    snapComposer: surfaceSnap(),
    retained: snap0.head === snapHold.head && snap0.tail === snapHold.tail,
    released: snapRelease.head !== snapHold.head || snapRelease.tail !== snapHold.tail,
  };
}

async function runEdgeCollapseResume() {
  document.getSelection()?.removeAllRanges();
  ctl().setFollow(false);
  ctl().setAnchorIndex(700);
  await frame();
  await new Promise((r) => setTimeout(r, 80));
  const before = surfaceSnap();
  const scroll = document.getElementById('transcript') as HTMLElement;
  const firstItem = document.querySelector('[data-item-id]');
  const sel = document.getSelection();
  const range = document.createRange();
  if (firstItem) range.selectNodeContents(firstItem);
  sel?.removeAllRanges();
  if (firstItem) sel?.addRange(range);
  await frame();
  scroll.scrollTop = 0;
  await frame();
  const held = surfaceSnap();
  sel?.removeAllRanges();
  await frame();
  await new Promise((r) => setTimeout(r, 80));
  const after = surfaceSnap();
  return {
    before,
    held,
    after,
    scrollTop: scroll.scrollTop,
    resumedWithoutExtraScroll: Number(after.start) < Number(before.start) || (after.head ?? '') < (before.head ?? ''),
  };
}

async function runDeactivateRafCycle() {
  document.getSelection()?.removeAllRanges();
  ctl().setFollow(false);
  ctl().setAnchorIndex(700);
  ctl().setActive(false);
  await frame();
  ctl().setActive(true);
  await frame();
  await new Promise((r) => setTimeout(r, 80));
  const reset = surfaceSnap();
  const scroll = document.getElementById('transcript') as HTMLElement;
  const content = document.querySelector('.transcript-content');
  content?.dispatchEvent(new PointerEvent('pointerdown', { button: 0, pointerType: 'mouse', bubbles: true }));
  scroll.dispatchEvent(new Event('scroll'));
  ctl().setActive(false);
  await frame();
  ctl().setActive(true);
  await frame();
  await new Promise((r) => setTimeout(r, 80));
  const mid = surfaceSnap();
  scroll.scrollTop = 0;
  scroll.dispatchEvent(new Event('scroll'));
  await frame();
  await new Promise((r) => setTimeout(r, 80));
  const after = surfaceSnap();
  return {
    reset,
    mid,
    after,
    demandWorksAfterReactivate: Number(after.start) < Number(reset.start) || (after.head ?? '') < (reset.head ?? ''),
  };
}

(window as unknown as { __run: () => Promise<unknown> }).__run = async () => {
  const retain = await runRetainRelease();
  const edge = await runEdgeCollapseResume();
  const cycle = await runDeactivateRafCycle();
  return { retain, edge, cycle };
};
