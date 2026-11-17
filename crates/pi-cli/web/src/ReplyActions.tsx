/**
 * Shared reply action strip: favorite star (existing chrome preserved), raw
 * markdown copy, and raw markdown `.md` save.
 *
 * The caller owns the reply data and all effects:
 *   - `markdown` is the EXACT `sourceMarkdownFromBlocks` output — copied and
 *     saved verbatim (never DOM-scraped, never redacted, never trimmed);
 *   - `timestampMs` is the reply's stable wire timestamp; when absent/invalid
 *     only the save action reports an error (the component never throws
 *     while rendering and copy/star stay available);
 *   - `onError` plugs the app's central toast; messages here are fixed
 *     context and never include reply text;
 *   - `star` mirrors the existing row-star shape; omitted → no star chrome.
 *
 * This file deliberately imports no App code (no cycles) and owns no CSS:
 * `.msg--assistant__actions` / `.assistant-reply-action` are the frozen
 * class contract.
 */
import { useEffect, useRef, useState } from 'react';
import { COPY_FLASH_MS, copyTextToClipboard } from './CopyButton';
import { replyMarkdownFileName } from './replyMarkdown';

/** Same shape FinalAssistant already passes to the row star. */
export type ReplyActionResultStar = {
  entryId: string | null;
  starred: boolean;
  onToggle: (entryId: string, starred: boolean) => void;
};

export type ReplyActionsProps = {
  markdown: string;
  timestampMs?: number;
  star?: ReplyActionResultStar;
  onError: (message: string) => void;
};

type FlashAction = 'copy' | 'save';
type FlashTimerHandle = number | NodeJS.Timeout;

/** Fixed, source-free failure context for the central toast. */
const COPY_ERROR = 'Failed to copy reply markdown';
const SAVE_ERROR = 'Failed to save reply markdown';

/** Shared glyph set (the success tick is reused by both actions). */
const GLYPH_COPY = '⧉';
const GLYPH_SAVE = '⤓';
const GLYPH_DONE = '✓';

export function ReplyActions({ markdown, timestampMs, star, onError }: ReplyActionsProps) {
  const [flash, setFlash] = useState<FlashAction | null>(null);
  const flashTimer = useRef<FlashTimerHandle | null>(null);
  const mountedRef = useRef(true);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      clearTimeout(flashTimer.current ?? undefined);
    };
  }, []);

  const flashFor = (action: FlashAction): void => {
    clearTimeout(flashTimer.current ?? undefined);
    setFlash(action);
    flashTimer.current = setTimeout(() => {
      flashTimer.current = null;
      setFlash(null);
    }, COPY_FLASH_MS);
  };

  const copy = async (): Promise<void> => {
    // The try covers ONLY the clipboard result: a thrown clipboard call is a
    // failed copy, not a second notification. onError runs once, outside the
    // try, so its own throw can never be caught and re-notified.
    let ok = false;
    try {
      ok = await copyTextToClipboard(markdown);
    } catch {
      ok = false;
    }
    // An await can land after unmount; no flash timer may be created then and
    // the caller's toast must not fire for a dead row.
    if (!mountedRef.current) return;
    if (ok) flashFor('copy');
    else onError(COPY_ERROR);
  };

  const save = (): void => {
    let url: string | null = null;
    let anchor: HTMLAnchorElement | null = null;
    try {
      // Missing/unusable timestamps fail ONLY this action; nothing throws
      // during render and the copy/star buttons stay present.
      if (typeof timestampMs !== 'number') throw new Error('missing timestamp');
      const filename = replyMarkdownFileName(markdown, timestampMs);
      const blob = new Blob([markdown], { type: 'text/markdown;charset=utf-8' });
      url = URL.createObjectURL(blob);
      anchor = document.createElement('a');
      anchor.href = url;
      anchor.download = filename;
      anchor.hidden = true;
      document.body.appendChild(anchor);
      anchor.click();
      flashFor('save');
    } catch {
      onError(SAVE_ERROR);
    } finally {
      // Remove the anchor before revoking; if removal throws, the nested
      // finally still revokes the created URL and the error propagates.
      try {
        if (anchor !== null) anchor.remove();
      } finally {
        if (url !== null) URL.revokeObjectURL(url);
      }
    }
  };

  const copied = flash === 'copy';
  const saved = flash === 'save';

  return (
    <div className="msg--assistant__actions">
      {star && star.entryId !== null ? (
        <button
          type="button"
          data-entry-id={star.entryId}
          className="assistant-star-toggle"
          aria-pressed={star.starred}
          aria-label={star.starred ? 'Unstar' : 'Star'}
          title={star.starred ? 'Unstar reply' : 'Star reply'}
          onClick={() => {
            if (star.entryId !== null) star.onToggle(star.entryId, !star.starred);
          }}
        >
          <span aria-hidden="true">{star.starred ? '★' : '☆'}</span>
          <span className="assistant-star-toggle__label">{star.starred ? 'Unstar' : 'Star'}</span>
        </button>
      ) : null}
      <button
        type="button"
        className="assistant-reply-action"
        data-reply-action="copy-markdown"
        aria-label={copied ? 'Copied' : 'Copy markdown'}
        title={copied ? 'Copied' : 'Copy raw markdown'}
        onClick={() => { void copy(); }}
      >
        <span aria-hidden="true">{copied ? GLYPH_DONE : GLYPH_COPY}</span>
      </button>
      <button
        type="button"
        className="assistant-reply-action"
        data-reply-action="save-markdown"
        aria-label={saved ? 'Saved' : 'Save markdown'}
        title={saved ? 'Saved' : 'Save raw markdown as .md'}
        onClick={save}
      >
        <span aria-hidden="true">{saved ? GLYPH_DONE : GLYPH_SAVE}</span>
      </button>
    </div>
  );
}
