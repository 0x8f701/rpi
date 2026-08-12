// Composer auto-grow controller shared by the host composer (App.tsx) and the
// collab guest view (CollabGuestView.tsx).
//
// WHY THIS EXISTS — typing/backspace lag root cause:
// The old per-keystroke handler ran synchronously on every onInput event:
//   input.style.height = 'auto';                 // 1. style write (invalidate)
//   const s = window.getComputedStyle(input);    // 2. style read (recalc)
//   ... input.scrollHeight ...                   // 3. layout read → FORCED
//                                                //    SYNCHRONOUS reflow
//   input.style.height = ...px;                  // 4. style write (invalidate)
// Reading scrollHeight after the height:auto write forces the browser to run
// style + layout synchronously in the middle of the input event, so every
// keystroke and backspace pays a reflow. The getComputedStyle reads are pure
// waste too — line-height and vertical padding are static CSS. A real-browser
// burst probe measured 43 getComputedStyle calls, 43 scrollHeight reads, and
// roughly 130 layout passes for 44 keystrokes, all inside input events.
//
// FIX: the controller below keeps the exact same algorithm (reset to auto →
// read natural scrollHeight → clamp to the cap, so delete/backspace shrinks
// back down) but (a) COALESCES all per-frame resize requests into ONE layout
// pass at the next animation frame, and (b) caches the static metrics after
// the first measure. Rapid keystrokes therefore cost one forced layout per
// per frame instead of one per event. flush(input) applies the given element
// synchronously for the submit/send-clear path (the cleared composer must
// collapse immediately even if the last scheduled frame already ran);
// cancel() drops pending work on unmount.
//
// The core is a DOM-free seam (fake elements {style.height, scrollHeight} plus
// an injected frame scheduler and metrics reader) so it is exercised by the
// focused Node regression in scripts/autoResize.test.ts; real-browser
// behavior is covered by the separate E2E coverage gate.

/** Static per-textarea metrics, measured once (static CSS, never per key). */
export interface StaticMetrics {
  lineHeight: number;
  paddingVertical: number;
}

/** The minimum element surface the controller touches (real DOM or fake). */
export interface AutoResizeElement {
  style: { height: string };
  scrollHeight: number;
}

/** Frame scheduler seam; the real browser adapter uses requestAnimationFrame. */
export interface FrameScheduler {
  request(callback: () => void): number;
  cancel(id: number): void;
}

/** requestAnimationFrame adapter (the browser default scheduler). */
export const rafScheduler: FrameScheduler = {
  request(callback) {
    return requestAnimationFrame(callback);
  },
  cancel(id) {
    cancelAnimationFrame(id);
  },
};

export interface AutoResizeOptions<E extends AutoResizeElement = AutoResizeElement> {
  /** Line-based grow cap (host: 3 lines). Ignored when `maxHeight` is set. */
  maxLines?: number;
  /** Hard pixel cap (guest: 180) that overrides the line-based cap. */
  maxHeight?: number;
  /** Reads the element's static metrics; injected so tests need no DOM. */
  measure: (input: E) => StaticMetrics;
  /** Frame scheduler; defaults to requestAnimationFrame. Injectable for tests. */
  scheduler?: FrameScheduler;
}

export interface AutoResizeController<E extends AutoResizeElement = AutoResizeElement> {
  /** Schedule a coalesced resize: N calls per frame cost one layout pass. */
  resize(input: E): void;
  /** Cancel any pending frame and measure now. `input` (submit-clear path:
   *  the element was just emptied, so it must be remeasured even when the
   *  last scheduled frame already ran) or the latest pending element when
   *  omitted. */
  flush(input?: E): void;
  /** Drop pending work without applying it (component unmount). */
  cancel(): void;
  /** Drop the cached metrics so the next apply re-measures (theme/font change). */
  resetMetrics(): void;
}

/** Pixel cap for `maxLines` textarea lines at the measured metrics. */
export function lineCapFor(maxLines: number, metrics: StaticMetrics): number {
  return Math.round(metrics.lineHeight * maxLines + metrics.paddingVertical);
}

/** Final height: natural content height clamped to the cap. Clamping at the
 *  reset-to-auto re-measure is what collapses the composer on backspace. */
export function resizeHeight(scrollHeight: number, cap: number): number {
  return Math.min(scrollHeight, cap);
}

export function createAutoResizeController<E extends AutoResizeElement>(
  options: AutoResizeOptions<E>
): AutoResizeController<E> {
  const { maxLines, maxHeight, measure, scheduler = rafScheduler } = options;
  if (maxHeight === undefined && maxLines === undefined) {
    throw new Error(
      'createAutoResizeController: provide maxLines (line cap) or maxHeight (pixel cap)'
    );
  }

  let frameId: number | null = null;
  let pending: E | null = null;
  let metrics: StaticMetrics | null = null;

  const capFor = (m: StaticMetrics): number => {
    if (maxHeight !== undefined) return maxHeight;
    // The construction guard above guarantees maxLines whenever maxHeight is absent.
    return lineCapFor(maxLines!, m);
  };

  const apply = (input: E): void => {
    if (metrics === null) metrics = measure(input);
    const cap = capFor(metrics);
    // Reset to `auto` so scrollHeight reports the natural content height, then
    // clamp to the cap. The scrollHeight read right after the height:auto write
    // forces ONE synchronous reflow per APPLIED frame — coalescing per-keystroke
    // work into that single pass is the whole point of this controller.
    input.style.height = 'auto';
    input.style.height = `${resizeHeight(input.scrollHeight, cap)}px`;
  };

  const applyPending = (): void => {
    frameId = null;
    const input = pending;
    pending = null;
    if (input !== null) apply(input);
  };

  return {
    resize(input) {
      // Keep only the LATEST element; the frame measures its scrollHeight as of
      // frame time, so rapid keystrokes collapse to one layout pass over the
      // final content.
      pending = input;
      if (frameId === null) {
        frameId = scheduler.request(applyPending);
      }
    },
    flush(input) {
      if (frameId !== null) {
        scheduler.cancel(frameId);
        frameId = null;
      }
      const target = input ?? pending;
      pending = null;
      if (target !== null) apply(target);
    },
    cancel() {
      if (frameId !== null) {
        scheduler.cancel(frameId);
        frameId = null;
      }
      pending = null;
    },
    resetMetrics() {
      metrics = null;
    },
  };
}
