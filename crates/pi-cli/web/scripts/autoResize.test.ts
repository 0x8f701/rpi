#!/usr/bin/env node
// Focused unit regression for src/autoResize.ts — the coalescing composer
// resize controller shared by the host composer (App.tsx) and the collab
// guest view (CollabGuestView.tsx). Bundled by `npm run build` with Vite's
// esbuild into a disposable Node module and executed before the production
// bundle, so a composer-resize regression fails the build.
//
// The controller is a DOM-free seam: the browser is represented by an
// injected frame scheduler (rAF stand-in) and fake elements
// ({style.height, scrollHeight}) — exactly the fields the controller touches,
// plus an injected metrics reader standing in for getComputedStyle.
// Real-browser behavior is measured separately by the E2E coverage gate and
// the performance probe; this file is the fast regression seam.
//
// Scenarios mirror the bugs this gate must catch:
//   - rapid keystrokes coalesce into ONE layout pass per animation frame
//     (the old code forced a synchronous reflow per onInput event — measured
//     as ~130 layout passes + 43 scrollHeight reads + 43 getComputedStyle
//     calls per 44-keystroke burst in the real-Chromium probe);
//   - the frame measures the LATEST content, so fast typing never applies a
//     stale height;
//   - deleting content shrinks the composer (height:auto re-measure);
//   - the height cap (3 measured lines host / 180px guest) is preserved;
//   - submit clears flush immediately; unmount cancels pending work;
//   - static metrics are cached, not re-measured per keystroke.
import {
  createAutoResizeController,
  lineCapFor,
  resizeHeight,
  type AutoResizeController,
  type AutoResizeElement,
  type FrameScheduler,
  type StaticMetrics,
} from '../src/autoResize.ts';

const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

/** Fake rAF: no timers, one manually-drained queue. */
function fakeScheduler() {
  const pending = new Map();
  let nextId = 1;
  const scheduler = {
    request(cb) {
      const id = nextId++;
      pending.set(id, cb);
      return id;
    },
    cancel(id) {
      pending.delete(id);
    },
  };
  return {
    scheduler,
    frameCount: () => pending.size,
    runFrame() {
      const id = pending.keys().next().value;
      if (id === undefined) throw new Error('runFrame: no pending frame');
      const cb = pending.get(id);
      pending.delete(id);
      cb();
    },
  };
}

/** Host textarea metrics: line-height 18.85px + 6px top/bottom padding. */
const METRICS = { lineHeight: 18.85, paddingVertical: 12 };
// lineCapFor(3, METRICS) = round(18.85*3 + 12) = 69
const HOST_CAP = 69;

/** Fake textarea: plain fields the controller writes/reads. */
function el(scrollHeight) {
  return { style: { height: '' }, scrollHeight };
}

function hostController() {
  const frames = fakeScheduler();
  const controller = createAutoResizeController({
    maxLines: 3,
    measure: () => METRICS,
    scheduler: frames.scheduler,
  });
  return { controller, frames };
}

// ---- pure core: cap math + clamp ----
check('lineCapFor(3) caps at 3 measured lines', lineCapFor(3, METRICS) === HOST_CAP);
check('lineCapFor(1) is a single measured line', lineCapFor(1, METRICS) === Math.round(18.85 + 12));
check('resizeHeight below the cap is untouched', resizeHeight(40, HOST_CAP) === 40);
check('resizeHeight at the cap is the cap', resizeHeight(HOST_CAP, HOST_CAP) === HOST_CAP);
check('resizeHeight above the cap clamps', resizeHeight(500, HOST_CAP) === HOST_CAP);
check('resizeHeight of empty content is 0', resizeHeight(0, HOST_CAP) === 0);

// ---- coalescing: N keystrokes per frame == 1 layout pass ----
{
  const { controller, frames } = hostController();
  const input = el(40);
  controller.resize(input);
  controller.resize(input);
  controller.resize(input);
  controller.resize(input);
  check('four rapid resizes schedule ONE frame', frames.frameCount() === 1);
  frames.runFrame();
  check('the frame applies the resize', input.style.height === '40px');
  check('the frame is drained', frames.frameCount() === 0);
}

// ---- per-frame cadence: typing after an applied frame schedules a new one ----
{
  const { controller, frames } = hostController();
  const input = el(40);
  controller.resize(input);
  controller.resize(input);
  frames.runFrame();
  controller.resize(input);
  check('typing after an applied frame schedules a fresh frame', frames.frameCount() === 1);
  frames.runFrame();
  check('second frame applied too', input.style.height === '40px');
}

// ---- latest value: the frame measures content as of frame time ----
{
  const { controller, frames } = hostController();
  const input = el(20);
  controller.resize(input);
  input.scrollHeight = 55; // content grew before the frame ran
  frames.runFrame();
  check(
    'frame applies the LATEST scrollHeight, not a scheduled snapshot',
    input.style.height === '55px'
  );
}

// ---- latest element: only the last scheduled element is resized ----
{
  const { controller, frames } = hostController();
  const first = el(80);
  const second = el(40);
  controller.resize(first);
  controller.resize(second);
  frames.runFrame();
  check('only the latest element is resized', second.style.height === '40px');
  check('the earlier element is untouched', first.style.height === '');
}

// ---- shrink on delete: backspace collapses back down ----
{
  const { controller, frames } = hostController();
  const input = el(120); // 3+ lines of content
  controller.resize(input);
  frames.runFrame();
  check('content above the cap grows to the 3-line cap', input.style.height === '69px');
  input.scrollHeight = 30; // user deleted down to a single line
  controller.resize(input);
  frames.runFrame();
  check('backspace shrinks the composer back down', input.style.height === '30px');
}

// ---- cap preserved: overflowing content never exceeds the cap ----
{
  const { controller, frames } = hostController();
  const input = el(1000);
  controller.resize(input);
  frames.runFrame();
  check('host composer never exceeds the 3-line cap', input.style.height === '69px');
}

// ---- guest cap: maxHeight wins over the line cap ----
{
  const frames = fakeScheduler();
  const controller = createAutoResizeController({
    maxHeight: 180,
    measure: () => METRICS,
    scheduler: frames.scheduler,
  });
  const tall = el(500);
  controller.resize(tall);
  frames.runFrame();
  check('guest cap (maxHeight 180) wins over the line cap', tall.style.height === '180px');
  const short = el(25);
  controller.resize(short);
  frames.runFrame();
  check('guest still shrinks below its cap', short.style.height === '25px');
}

// ---- flush on submit: immediate synchronous collapse ----
{
  const { controller, frames } = hostController();
  const input = el(90);
  controller.resize(input); // frame pending
  controller.flush(input);
  check('flush(input) applies the cleared element synchronously', input.style.height === '69px');
  check('flush(input) cancels the pending frame (no double apply)', frames.frameCount() === 0);
  input.scrollHeight = 30; // value changed again with nothing pending
  controller.flush(input);
  check('flush(input) re-measures even with no pending frame', input.style.height === '30px');
  controller.flush(); // no pending element and no argument
  check('flush() with no pending work is a no-op', frames.frameCount() === 0);
}
{
  // THE submit-clear bug: the scheduled frame already ran while the user
  // paused, so `pending` is empty — submit must still collapse the cleared
  // composer, which requires flushing WITH the cleared element.
  const { controller, frames } = hostController();
  const input = el(200);
  controller.resize(input);
  frames.runFrame();
  check('grown before submit (pending already drained)', input.style.height === '69px');
  input.scrollHeight = 20; // send-clear emptied the value
  controller.flush(input);
  check('submit flush collapses a drained composer immediately', input.style.height === '20px');
  check('no frame left pending after the submit flush', frames.frameCount() === 0);
}
{
  // flush(input) overrides a still-pending resize instead of double-applying.
  const { controller, frames } = hostController();
  const input = el(90);
  controller.resize(input); // frame still pending
  input.scrollHeight = 10; // cleared before the frame ran
  controller.flush(input);
  check('flush(input) overrides the pending frame', input.style.height === '10px');
  check('pending frame canceled by flush(input)', frames.frameCount() === 0);
}
{
  // flush() without an argument still applies the latest pending element
  // (resize → flush with no intervening frame).
  const { controller, frames } = hostController();
  const input = el(50);
  controller.resize(input);
  controller.flush();
  check('flush() applies the pending element', input.style.height === '50px');
  check('flush() drains the pending frame', frames.frameCount() === 0);
}

// ---- cancel on unmount: pending work is dropped, controller stays usable ----
{
  const { controller, frames } = hostController();
  const input = el(50);
  controller.resize(input);
  controller.cancel();
  check('cancel drops the pending frame', frames.frameCount() === 0);
  check('cancel never styled the element', input.style.height === '');
  // the component was unmounted in StrictMode's simulated remount: a later
  // resize must still work on the remount
  controller.resize(input);
  frames.runFrame();
  check('controller remains usable after cancel', input.style.height === '50px');
}

// ---- static metrics are cached, not re-measured per keystroke ----
{
  const frames = fakeScheduler();
  let measureCalls = 0;
  const controller = createAutoResizeController({
    maxLines: 3,
    measure: () => {
      measureCalls += 1;
      return METRICS;
    },
    scheduler: frames.scheduler,
  });
  const input = el(40);
  controller.resize(input);
  frames.runFrame();
  controller.resize(input);
  frames.runFrame();
  controller.resize(input);
  frames.runFrame();
  check('metrics measured ONCE across many keystrokes', measureCalls === 1);
  controller.resetMetrics();
  controller.resize(input);
  frames.runFrame();
  check('resetMetrics forces exactly one re-measure', measureCalls === 2);
}

// ---- construction validation ----
{
  let threw = false;
  try {
    createAutoResizeController({ measure: () => METRICS });
  } catch {
    threw = true;
  }
  check('controller without a cap source throws at construction', threw);
}

console.log(`\nautoResize.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
