import { useCallback, useEffect, useRef, type UIEvent } from 'react';

/** Distance from the bottom (px) that still counts as "pinned". Absorbs wheel
 *  jitter and sub-frame layout rounding without fighting a deliberate
 *  scroll-away. */
export const PIN_TOLERANCE_PX = 80;

/** The live geometry the pin logic reads (and, for the scrollTop writer,
 *  writes). Structural, DOM-free type so the decision core below is
 *  unit-testable in plain Node with fake elements. */
export interface ScrollPinElement {
  scrollTop: number;
  scrollHeight: number;
  clientHeight: number;
}

/** Remaining scrollable distance to the bottom of a container. Pure
 *  arithmetic: 0 at the bottom, negative during overscroll/bounce, positive
 *  above it. */
export function remainingToBottom(scrollTop: number, scrollHeight: number, clientHeight: number): number {
  return scrollHeight - scrollTop - clientHeight;
}

/** Pure bottom-pin decision: is the container at/near the bottom? The
 *  tolerance boundary is EXCLUSIVE — exactly `tolerancePx` remaining is not
 *  pinned, `tolerancePx - 1` is. */
export function isPinned(scrollTop: number, scrollHeight: number, clientHeight: number, tolerancePx: number = PIN_TOLERANCE_PX): boolean {
  return remainingToBottom(scrollTop, scrollHeight, clientHeight) < tolerancePx;
}

/** Plain (non-React, non-DOM) pin state machine — the decision core shared
 *  by the host transcript (App) and the collab guest view, unit-testable
 *  without a browser.
 *
 *  One coherent bottom-pin state for the transcript scroll container
 *  (`#transcript` with its `.transcript-content` wrapper):
 *
 *  - Pinned (user at/near the bottom): every content growth — streamed text
 *    deltas, React item commits, async layout (images, mermaid/KaTeX
 *    hydration, tool-card expansion) — and every container `clientHeight`
 *    change (header/footer/composer wrapping, especially mobile) keeps the
 *    view glued to the bottom.
 *  - A genuine user scroll away from the bottom unpins. While unpinned the
 *    viewport is NEVER moved again: deltas append below the viewport, so the
 *    reading position is preserved by geometry, and every re-pin trigger
 *    checks the pin state first.
 *  - Returning to the bottom re-pins.
 *  - `forcePin` (session activation / initial bootstrap) pins
 *    unconditionally: the newly activated session never inherits the
 *    previous session's scroll position or pin state.
 *
 *  The controller is null-tolerant: `null` elements (a consumer that renders
 *  the transcript conditionally) make every write a no-op instead of a
 *  crash — the one guard the real app never trips, kept testable here. */
export function createScrollPin(initialPinned = true): ScrollPinController {
  let pinned = initialPinned;
  let scrollHeight: number | null = null;
  let clientHeight: number | null = null;

  function rememberGeometry(el: ScrollPinElement): void {
    scrollHeight = el.scrollHeight;
    clientHeight = el.clientHeight;
  }

  function onScroll(el: ScrollPinElement): void {
    const geometryChanged = scrollHeight !== null
      && (scrollHeight !== el.scrollHeight || clientHeight !== el.clientHeight);
    if (pinned && geometryChanged) {
      el.scrollTop = el.scrollHeight;
      rememberGeometry(el);
      return;
    }
    pinned = isPinned(el.scrollTop, el.scrollHeight, el.clientHeight);
    rememberGeometry(el);
  }

  function followIfPinned(el: ScrollPinElement | null): boolean {
    if (!el) return false;
    if (!pinned) {
      rememberGeometry(el);
      return false;
    }
    el.scrollTop = el.scrollHeight;
    rememberGeometry(el);
    return true;
  }

  function forcePin(el: ScrollPinElement | null): boolean {
    if (!el) return false;
    pinned = true;
    el.scrollTop = el.scrollHeight;
    rememberGeometry(el);
    return true;
  }

  function handleResize(el: ScrollPinElement | null): boolean {
    return followIfPinned(el);
  }

  return { onScroll, followIfPinned, forcePin, handleResize };
}

export interface ScrollPinController {
  /** Scroll-event handler: re-derive the pin state from live geometry. The
   *  caller supplies the element (the scroll event's currentTarget), so it
   *  is never null here. */
  onScroll(el: ScrollPinElement): void;
  /** Follow the bottom when pinned. Returns true when the viewport moved. */
  followIfPinned(el: ScrollPinElement | null): boolean;
  /** Pin unconditionally (session activation / initial bootstrap). */
  forcePin(el: ScrollPinElement | null): boolean;
  /** ResizeObserver delegate: content growth or a container clientHeight
   *  change — pinned stays glued to the bottom, unpinned freezes. */
  handleResize(el: ScrollPinElement | null): boolean;
}

/**
 * One coherent bottom-pin state for the transcript scroll container. There
 * is exactly ONE writer of `scrollTop` (the controller, reached through
 * followIfPinned / forcePin) and the scroll handler is the only reader of
 * the pin state. Browser scroll anchoring is disabled on the container
 * (`overflow-anchor: none` in styles.css) so a heuristic can never fight the
 * pin state, and programmatic pins always land AT the bottom — the scroll
 * event they produce re-computes `true`, so no suppression flag is needed
 * and no feedback loop is possible.
 */
export function useScrollPin() {
  const transcriptRef = useRef<HTMLDivElement>(null);
  const transcriptContentRef = useRef<HTMLDivElement>(null);
  // Lazy-init: the controller instance is created once per hook mount.
  const pinRef = useRef<ScrollPinController | null>(null);
  if (pinRef.current === null) pinRef.current = createScrollPin();
  const ctrl = pinRef.current;

  const pinIfPinned = useCallback(() => {
    ctrl.followIfPinned(transcriptRef.current);
  }, [ctrl]);

  const forcePin = useCallback(() => {
    ctrl.forcePin(transcriptRef.current);
  }, [ctrl]);

  // The ONLY reader of pin state. Called from the container's onScroll, so
  // only genuine user scrolling (or the event produced by a programmatic
  // pin, which always lands at the bottom) can re-evaluate it. The event's
  // currentTarget IS the container, so no ref lookup or null guard is
  // needed.
  const onTranscriptScroll = useCallback((event: UIEvent<HTMLDivElement>) => {
    ctrl.onScroll(event.currentTarget);
  }, [ctrl]);

  // Async content growth changes scrollHeight with no delta and no React
  // commit (images, mermaid/KaTeX hydration, tool-card expansion); header
  // stream badge / composer action state / wrapping change the container's
  // clientHeight with no content change at all — especially on mobile. Both
  // move the visible window relative to the content, so ONE bounded observer
  // watches BOTH the scroll container and its content wrapper: pinned stays
  // glued to the bottom, unpinned freezes (never moved again). The guard
  // covers consumers that render the transcript conditionally
  // (CollabGuestView's malformed-link early return still runs this effect
  // with null refs). Scroll writes never resize either element, so the
  // observer cannot feed back into itself.
  useEffect(() => {
    const container = transcriptRef.current;
    const content = transcriptContentRef.current;
    if (!container || !content) return;
    const observer = new ResizeObserver(() => ctrl.handleResize(transcriptRef.current));
    observer.observe(container);
    observer.observe(content);
    return () => observer.disconnect();
  }, [ctrl]);

  return { transcriptRef, transcriptContentRef, onTranscriptScroll, pinIfPinned, forcePin };
}
