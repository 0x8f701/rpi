import type { ComposerAttachment } from './attachments';
import { MAX_FILE_BYTES, utf8ByteLength } from './attachments';
import { nextId } from './transcript';

/** Large pasted text is stored as a text attachment instead of materializing
 * the glyphs in the textarea. Chromium otherwise lays out the entire value
 * to compute editing geometry. Short composer drafts — including slash
 * commands such as `/code-review <from> <to>` pasted with a newline — stay
 * native. Synthesized Enter keydowns from a paste are blocked by
 * COMPOSER_PASTE_ENTER_GUARD_MS, not by this line threshold. */
export const LARGE_TEXT_PASTE_THRESHOLD = 8 * 1024;
export const LARGE_TEXT_PASTE_LINE_THRESHOLD = 32;
export const PASTED_TEXT_ATTACHMENT_NAME = 'pasted-text.txt';
export const LARGE_TEXT_PREVIEW_CHARS = 8 * 1024;
export const COMPOSER_PASTE_ENTER_GUARD_MS = 250;

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

export function logicalLineBreaks(text: string): number {
  let breaks = 0;
  for (let index = 0; index < text.length; index += 1) {
    const code = text.charCodeAt(index);
    if (code === 13) {
      breaks += 1;
      if (text.charCodeAt(index + 1) === 10) index += 1;
      continue;
    }
    if (code === 10) breaks += 1;
  }
  return breaks;
}

export function logicalLineCount(text: string): number {
  if (text.length === 0) return 0;
  return logicalLineBreaks(text) + 1;
}

function isMultilineOrLargePaste(text: string): number | null {
  if (logicalLineCount(text) >= LARGE_TEXT_PASTE_LINE_THRESHOLD) {
    return utf8ByteLength(text);
  }
  return largeTextBytes(text);
}

export function planLargeTextPaste(text: string): LargeTextPastePlan {
  const size = isMultilineOrLargePaste(text);
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

export function nextPasteEnterGuard(now: number): number {
  return now + COMPOSER_PASTE_ENTER_GUARD_MS;
}

export function nativeEventShiftKey(event: Event): boolean {
  return Boolean((event as { shiftKey?: boolean }).shiftKey);
}

export function isComposerEnterKey(options: {
  key: string;
  keyCode?: number;
  which?: number;
}): boolean {
  if (options.key === 'Enter' || options.key === 'NumpadEnter') return true;
  if (options.key !== '' && options.key !== 'Unidentified') return false;
  const code = options.keyCode ?? options.which;
  return code === 13;
}

export type ComposerEnterDecision = {
  key: string;
  shiftKey: boolean;
  isComposing?: boolean;
  keyCode?: number;
  which?: number;
  compositionActive?: boolean;
  now: number;
  pasteGuardUntil: number;
};

function composerEnterIsCandidate(options: ComposerEnterDecision): boolean {
  if (options.shiftKey) return false;
  const code = options.keyCode ?? options.which;
  if (code === 229) return false;
  if (options.compositionActive === true) return false;
  if (options.compositionActive !== false && options.isComposing) return false;
  return isComposerEnterKey(options);
}

export function composerEnterShouldPreventDefault(options: ComposerEnterDecision): boolean {
  return composerEnterIsCandidate(options);
}

export function composerEnterShouldSubmit(options: ComposerEnterDecision): boolean {
  if (!composerEnterIsCandidate(options)) return false;
  if (options.now < options.pasteGuardUntil) return false;
  return true;
}

export function composerBeforeInputShouldSubmit(options: {
  inputType: string;
  shiftKey: boolean;
  compositionActive?: boolean;
  keydownHandled?: boolean;
  now: number;
  pasteGuardUntil: number;
}): boolean {
  if (options.shiftKey) return false;
  if (options.compositionActive === true) return false;
  if (options.keydownHandled === true) return false;
  if (options.inputType !== 'insertLineBreak' && options.inputType !== 'insertParagraph') return false;
  if (options.now < options.pasteGuardUntil) return false;
  return true;
}
