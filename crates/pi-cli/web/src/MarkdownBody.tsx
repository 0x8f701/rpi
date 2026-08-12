import { useCallback, useEffect, useMemo, useRef } from 'react';
import type { Ref } from 'react';
import { renderMarkdown, hydrateMermaid } from './markdown';

/** Shared safe-Markdown body renderer (no fold): renders a plain string
 *  through the SHARED renderMarkdown pipeline (escapeHtml-first, whitelisted
 *  links/images, code fences, Mermaid hosts hydrated after commit) — never as
 *  raw pre-wrapped text. Used by the user caption (host App.tsx and the
 *  collab guest view render the same component so a prompt's text can never
 *  drift) and by prose card surfaces (IRC / custom / summary bodies), so
 *  every markdown surface shares ONE renderer and hostile HTML stays literal.
 *
 *  `className` lands on the root div (callers size the surface — bubble
 *  caption vs. card body); markdown blocks carry the shared `.md-*` layout
 *  classes. Mermaid and markdown-image mutations change layout with no React
 *  commit and no delta, so `onLayoutChange` (the transcript pin controller's
 *  pinIfPinned) fires synchronously after each host mutation and on every
 *  image decode — the same contract as FinalAssistant / HubMarkdownFold. */
export function MarkdownBody({
  text,
  className,
  onLayoutChange,
  bodyRef,
}: {
  text: string;
  className?: string;
  onLayoutChange?: () => void;
  /** Optional caller-owned ref: fold wrappers (IRC / hub) observe the root
   *  div for their line-clamp overflow check. Defaults to the internal ref;
   *  hydration and image listeners always scope to the same root element. */
  bodyRef?: Ref<HTMLDivElement>;
}) {
  const innerRef = useRef<HTMLDivElement | null>(null);
  const setRootRef = useCallback(
    (el: HTMLDivElement | null) => {
      innerRef.current = el;
      if (typeof bodyRef === 'function') bodyRef(el);
      else if (bodyRef) (bodyRef as { current: HTMLDivElement | null }).current = el;
    },
    [bodyRef],
  );
  const html = useMemo(() => renderMarkdown(text), [text]);
  useEffect(() => {
    const node = innerRef.current;
    if (!node) return;
    // Mermaid fences render asynchronously: hydrate the hosts after
    // dangerouslySetInnerHTML commits. `onLayoutChange` fires synchronously
    // after each host mutation so the transcript pin re-glues before the
    // next frame.
    void hydrateMermaid(node, onLayoutChange);
    // Markdown images (`![..](..)`) decode asynchronously and grow layout
    // with no React commit and no delta. A scoped capture load handler
    // re-pins synchronously with the decode; the pin controller preserves a
    // deliberate user scroll-away (unpinned freeze). Data-URL decodes can
    // race ahead of this layout effect, so a complete image pins once
    // immediately instead of waiting for a load event that already fired.
    const imgs = Array.from(node.querySelectorAll<HTMLImageElement>('img.md-image'));
    const listeners: Array<[HTMLImageElement, () => void]> = [];
    for (const img of imgs) {
      if (img.complete) {
        onLayoutChange?.();
        continue;
      }
      const onLoad = () => onLayoutChange?.();
      img.addEventListener('load', onLoad, { capture: true, once: true });
      listeners.push([img, onLoad]);
    }
    return () => {
      for (const [img, onLoad] of listeners) img.removeEventListener('load', onLoad, { capture: true });
    };
  }, [html, onLayoutChange]);
  return <div ref={setRootRef} className={className} dangerouslySetInnerHTML={{ __html: html }} />;
}
