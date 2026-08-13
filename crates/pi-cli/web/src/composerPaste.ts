import type { ComposerAttachment } from './attachments';
import { MAX_FILE_BYTES, utf8ByteLength } from './attachments';
import { nextId } from './transcript';

/** Large pasted text is stored as a text attachment instead of materializing
 * millions of glyphs in the textarea. Chromium otherwise lays out the entire
 * textarea value to compute editing geometry, blocking the main thread even
 * though the composer is visually capped at three lines. */
export const LARGE_TEXT_PASTE_THRESHOLD = 256 * 1024;
export const PASTED_TEXT_ATTACHMENT_NAME = 'pasted-text.txt';
export const LARGE_TEXT_PREVIEW_CHARS = 8 * 1024;

export interface LargeTextDisplay {
  characters: number;
  bytes: number;
  omittedCharacters: number;
  preview: string;
}

function largeTextBytes(text: string): number | null {
  if (text.length < Math.ceil(LARGE_TEXT_PASTE_THRESHOLD / 3)) return null;
  const size = utf8ByteLength(text);
  return size >= LARGE_TEXT_PASTE_THRESHOLD ? size : null;
}

/** Render cache: a large message's display metadata is recomputed on every
 *  transcript re-render (streaming turns, optimistic reconcile), and each
 *  recompute UTF-8-encodes the whole multi-hundred-KiB text to prove it is
 *  large. The bounded FIFO makes the bounded-render gate O(1) after the first
 *  render without retaining unbounded memory. Only LARGE texts are cached —
 *  a small caption re-checks via the cheap length early-out. */
const MAX_LARGE_TEXT_CACHE_ENTRIES = 8;
const LARGE_TEXT_CACHE = new Map<string, LargeTextDisplay>();

export function largeTextDisplay(text: string): LargeTextDisplay | null {
  const cached = LARGE_TEXT_CACHE.get(text);
  if (cached) return cached;

  const bytes = largeTextBytes(text);
  if (bytes === null) return null;

  const headLength = Math.floor(LARGE_TEXT_PREVIEW_CHARS * 0.75);
  const tailLength = LARGE_TEXT_PREVIEW_CHARS - headLength;
  const omittedCharacters = Math.max(0, text.length - LARGE_TEXT_PREVIEW_CHARS);
  const preview = omittedCharacters === 0
    ? text
    : `${text.slice(0, headLength)}\n… ${omittedCharacters} characters omitted …\n${text.slice(-tailLength)}`;
  const display: LargeTextDisplay = { characters: text.length, bytes, omittedCharacters, preview };
  if (LARGE_TEXT_CACHE.size >= MAX_LARGE_TEXT_CACHE_ENTRIES) {
    LARGE_TEXT_CACHE.delete(LARGE_TEXT_CACHE.keys().next().value as string);
  }
  LARGE_TEXT_CACHE.set(text, display);
  return display;
}

export type LargeTextPastePlan =
  | { type: 'native' }
  | { type: 'oversize'; size: number }
  | { type: 'attachment'; attachment: ComposerAttachment };

export function planLargeTextPaste(text: string): LargeTextPastePlan {
  const size = largeTextBytes(text);
  if (size === null) return { type: 'native' };
  if (size > MAX_FILE_BYTES) return { type: 'oversize', size };

  return {
    type: 'attachment',
    attachment: {
      id: nextId('a'),
      name: PASTED_TEXT_ATTACHMENT_NAME,
      size,
      mimeType: 'text/plain',
      kind: 'code',
      text,
      language: 'text',
    },
  };
}
