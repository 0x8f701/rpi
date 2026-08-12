#!/usr/bin/env node
// Focused unit regression for src/scrollPin.ts — the pure pinned-state /
// geometry decision core shared by the host transcript (App.tsx) and the
// collab guest view (CollabGuestView.tsx). Bundled by `npm run build` with
// Vite's esbuild into a disposable Node module and executed before the
// production bundle, so a scroll-pinning regression fails the build.
//
// The hook (useScrollPin) is a thin React wrapper; every decision it makes
// delegates to the DOM-free seam below (remainingToBottom / isPinned /
// createScrollPin), so these assertions run in plain Node with fake elements
// ({scrollTop, scrollHeight, clientHeight}) — exactly the fields the
// controller touches. Real-browser coverage is measured separately by the
// E2E coverage gate; this file is the fast regression seam.
//
// The scenarios deliberately mirror the bugs this gate must catch:
//   - stream deltas appended directly to the DOM (no React commit) still pin;
//   - a viewport scrolled away from the bottom is NEVER moved again;
//   - session activation force-pins the newly active transcript;
//   - container clientHeight changes (header/footer/composer wrapping) follow
//     the same pinned re-pin / unpinned freeze contract as content growth.
import {
  PIN_TOLERANCE_PX,
  remainingToBottom,
  isPinned,
  createScrollPin,
  type ScrollPinElement,
} from '../src/scrollPin.ts';

const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

/** Fresh fake scroll container (plain geometry fields, no DOM). */
function el(scrollTop = 0, scrollHeight = 100, clientHeight = 20): ScrollPinElement {
  return { scrollTop, scrollHeight, clientHeight };
}

// ---- remainingToBottom: pure arithmetic ----
check('remainingToBottom at the bottom is 0', remainingToBottom(80, 100, 20) === 0);
check('remainingToBottom above the bottom is positive', remainingToBottom(30, 100, 20) === 50);
check('remainingToBottom overscroll is negative', remainingToBottom(90, 100, 20) === -10);

// ---- isPinned: threshold boundary (PIN_TOLERANCE_PX is EXCLUSIVE) ----
check('PIN_TOLERANCE_PX == 80', PIN_TOLERANCE_PX === 80);
check('exactly at the tolerance is NOT pinned', !isPinned(0, 100, 20)); // remaining 80
check('one px inside the tolerance is pinned', isPinned(1, 100, 20)); // remaining 79
check('one px outside the tolerance is NOT pinned', !isPinned(0, 100, 18)); // remaining 82
check('at the bottom exactly is pinned', isPinned(80, 100, 20)); // remaining 0
check('custom tolerance is honored', isPinned(50, 100, 20, 40) && !isPinned(50, 100, 20, 20)); // remaining 30
check('zero tolerance pins only strictly below the boundary', !isPinned(80, 100, 20, 0) && isPinned(81, 100, 20, 0));

// ---- programmatic pin: the write lands AT the bottom ----
{
  const ctrl = createScrollPin();
  const t = el(0, 100, 20);
  check('fresh controller starts pinned', ctrl.followIfPinned(t) === true);
  check('programmatic pin writes scrollTop == scrollHeight (not - clientHeight)', t.scrollTop === 100, `scrollTop=${t.scrollTop}`);
  check('followIfPinned reports the write', ctrl.followIfPinned(t) === true);
}

// ---- pinned growth: direct DOM deltas (no React commit) keep the view glued ----
{
  const ctrl = createScrollPin();
  const t = el(0, 100, 20);
  ctrl.followIfPinned(t); // bootstrap pin
  // stream delta appends text: scrollHeight grows with no activeItems change
  t.scrollHeight = 140;
  check('delta growth while pinned re-pins (no scroll-away)', ctrl.followIfPinned(t) === true && t.scrollTop === 140, `scrollTop=${t.scrollTop}`);
  t.scrollHeight = 200;
  check('repeated deltas keep the view glued', ctrl.followIfPinned(t) === true && t.scrollTop === 200, `scrollTop=${t.scrollTop}`);
}

// ---- layout-induced scroll event: geometry changed while pinned ----
{
  const ctrl = createScrollPin();
  const t = el(380, 400, 20);
  ctrl.followIfPinned(t);
  // Final markdown replaces the streaming node. The browser clamps scrollTop
  // and emits scroll before ResizeObserver; this must not be mistaken for a
  // deliberate user scroll-away.
  t.scrollHeight = 760;
  t.scrollTop = 380;
  ctrl.onScroll(t);
  check('layout-induced scroll while pinned follows new bottom immediately', t.scrollTop === 760, `scrollTop=${t.scrollTop}`);
  t.scrollHeight = 820;
  check('layout-induced scroll preserves pinned state for later growth', ctrl.handleResize(t) === true && t.scrollTop === 820, `scrollTop=${t.scrollTop}`);
}

// ---- unpin + freeze: a viewport scrolled away is NEVER moved again ----
{
  const ctrl = createScrollPin();
  const t = el(380, 400, 20); // at the bottom of a long transcript
  ctrl.followIfPinned(t);
  t.scrollTop = 200; // deliberate scroll-away: remaining 180 >= tolerance
  ctrl.onScroll(t);
  check('scroll-away unpins (remaining >= tolerance)', ctrl.followIfPinned(t) === false, `scrollTop=${t.scrollTop}`);
  // stream deltas keep arriving below the frozen viewport
  t.scrollHeight = 480;
  check('delta while unpinned freezes scrollTop exactly', ctrl.followIfPinned(t) === false && t.scrollTop === 200, `scrollTop=${t.scrollTop}`);
  t.scrollHeight = 560;
  check('continued growth never moves an unpinned viewport', ctrl.followIfPinned(t) === false && t.scrollTop === 200, `scrollTop=${t.scrollTop}`);
}

// ---- return to the bottom re-pins ----
{
  const ctrl = createScrollPin();
  const t = el(380, 400, 20);
  ctrl.followIfPinned(t);
  t.scrollTop = 200;
  ctrl.onScroll(t); // unpinned
  t.scrollHeight = 480;
  ctrl.followIfPinned(t); // freeze
  check('unpinned state holds after growth', t.scrollTop === 200, `scrollTop=${t.scrollTop}`);
  t.scrollTop = 460; // 480 - 20: back at the bottom
  ctrl.onScroll(t); // remaining 0 -> pinned
  check('returning to the bottom re-pins', ctrl.followIfPinned(t) === true, `scrollTop=${t.scrollTop}`);
  t.scrollHeight = 540;
  check('post-re-pin deltas glue again', ctrl.followIfPinned(t) === true && t.scrollTop === 540, `scrollTop=${t.scrollTop}`);
}

// ---- force pin: session activation never inherits the old scroll position ----
{
  const ctrl = createScrollPin();
  const t = el(380, 400, 20);
  ctrl.followIfPinned(t); // session A pinned
  t.scrollTop = 120; // A's reading position (far from the bottom)
  ctrl.onScroll(t); // unpinned
  t.scrollHeight = 500;
  ctrl.followIfPinned(t); // freeze
  check('pre-switch state is unpinned and frozen', t.scrollTop === 120, `scrollTop=${t.scrollTop}`);
  check('forcePin reports the write', ctrl.forcePin(t) === true);
  check('forcePin pins an unpinned transcript to the bottom', t.scrollTop === 500, `scrollTop=${t.scrollTop}`);
  t.scrollHeight = 600;
  check('after forcePin, deltas keep the view glued', ctrl.followIfPinned(t) === true && t.scrollTop === 600, `scrollTop=${t.scrollTop}`);
  // a second switch is still unconditional
  t.scrollTop = 60;
  ctrl.onScroll(t); // unpinned again
  check('forcePin is unconditional on every switch', ctrl.forcePin(t) === true && t.scrollTop === 600, `scrollTop=${t.scrollTop}`);
}

// ---- no element: conditional consumers (guest malformed link) no-op ----
{
  const ctrl = createScrollPin();
  check('followIfPinned(null) is a no-op', ctrl.followIfPinned(null) === false);
  check('forcePin(null) is a no-op', ctrl.forcePin(null) === false);
  check('handleResize(null) is a no-op', ctrl.handleResize(null) === false);
  const t = el(0, 100, 20);
  check('controller stays functional after null calls', ctrl.followIfPinned(t) === true && t.scrollTop === 100, `scrollTop=${t.scrollTop}`);
}

// ---- ResizeObserver delegate: content growth AND container clientHeight ----
{
  const ctrl = createScrollPin();
  const t = el(0, 100, 20);
  ctrl.followIfPinned(t);
  // async content growth (image/mermaid/KaTeX hydration, tool-card expansion)
  t.scrollHeight = 160;
  check('pinned content growth re-pins via the observer', ctrl.handleResize(t) === true && t.scrollTop === 160, `scrollTop=${t.scrollTop}`);
  // container clientHeight change (header badge wraps / footer Abort appears /
  // composer expands on mobile) with NO content change
  t.clientHeight = 10; // viewport shrank: pinned must stay glued
  check('pinned container shrink re-pins via the observer', ctrl.handleResize(t) === true && t.scrollTop === 160, `scrollTop=${t.scrollTop}`);
  t.clientHeight = 30; // viewport grew (footer collapsed): still glued
  check('pinned container growth stays glued', ctrl.handleResize(t) === true && t.scrollTop === 160, `scrollTop=${t.scrollTop}`);
  // Now unpin a separate initialized controller, then container changes must
  // freeze the viewport. Reusing the pinned controller with a new element
  // would model a session switch without the required forcePin contract.
  const unpinnedCtrl = createScrollPin();
  const u = el(220, 240, 20);
  unpinnedCtrl.followIfPinned(u);
  u.scrollTop = 100; // remaining 120 >= tolerance
  unpinnedCtrl.onScroll(u);
  check('unpinned before container change', unpinnedCtrl.followIfPinned(u) === false);
  u.clientHeight = 10; // viewport shrank while unpinned
  check('unpinned container shrink freezes scrollTop', unpinnedCtrl.handleResize(u) === false && u.scrollTop === 100, `scrollTop=${u.scrollTop}`);
  u.scrollHeight = 300;
  u.clientHeight = 30;
  check('unpinned growth + container change never moves the viewport', unpinnedCtrl.handleResize(u) === false && u.scrollTop === 100, `scrollTop=${u.scrollTop}`);
}

// ---- full lifecycle: bootstrap -> stream -> away -> freeze -> return -> switch ----
{
  const ctrl = createScrollPin();
  const t = el(0, 200, 20);
  ctrl.followIfPinned(t); // bootstrap
  t.scrollHeight = 240;
  ctrl.handleResize(t); // async growth while pinned
  check('lifecycle: bootstrap + async growth stay pinned', t.scrollTop === 240, `scrollTop=${t.scrollTop}`);
  t.scrollTop = 100;
  ctrl.onScroll(t); // user scrolls away
  check('lifecycle: scroll-away unpins', ctrl.followIfPinned(t) === false);
  t.scrollHeight = 320;
  ctrl.followIfPinned(t); // delta while unpinned
  ctrl.handleResize(t); // resize while unpinned
  check('lifecycle: unpinned deltas + resizes freeze the viewport', t.scrollTop === 100, `scrollTop=${t.scrollTop}`);
  t.scrollTop = 300; // 320 - 20: back at the bottom
  ctrl.onScroll(t);
  t.scrollHeight = 380;
  check('lifecycle: return re-pins and deltas glue', ctrl.followIfPinned(t) === true && t.scrollTop === 380, `scrollTop=${t.scrollTop}`);
  check('lifecycle: switch force-pins to the bottom', ctrl.forcePin(t) === true && t.scrollTop === 380, `scrollTop=${t.scrollTop}`);
  t.scrollHeight = 420;
  check('lifecycle: post-switch growth glues', ctrl.followIfPinned(t) === true && t.scrollTop === 420, `scrollTop=${t.scrollTop}`);
}

console.log(`\nscrollPin.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  for (const f of failures) console.log(`  FAIL ${f}`);
  process.exit(1);
}
process.exit(0);
