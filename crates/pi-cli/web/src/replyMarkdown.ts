/**
 * Raw reply markdown shared by the reply's copy and save actions.
 *
 * `sourceMarkdownFromBlocks` is the ONE public source for both actions: it
 * concatenates every `text` ContentBlock verbatim, in wire order, with no
 * separator and no trimming (thinking/tool/image blocks are not markdown
 * body text). The UI owns the clipboard/Blob/download mechanics and must
 * copy/save this exact string instead of re-reading the rendered DOM.
 *
 * `replyMarkdownFileName` derives a concise, filesystem-safe name from the
 * first non-empty body line plus the stable wire timestamp — never the
 * browser's current clock — so the same reply and timestamp always yield
 * the same name.
 */
import type { ContentBlock } from './types';

/** Documented filename slug budget, in Unicode code points (not UTF-16
 *  units): long replies still produce short, predictable names. */
const MAX_SLUG_CODE_POINTS = 30;

/** Used when the reply has no body text to name the file after. */
const FALLBACK_SLUG = 'reply';

/** Fixed, source-free context for an unusable wire timestamp. The reply
 *  body is never interpolated into this message. */
const TIMESTAMP_ERROR = 'reply markdown filename: invalid timestamp';

/** ECMAScript Date range bound (exclusive); anything at or beyond is an
 *  invalid Date and cannot format a local timestamp. */
const MAX_DATE_MS = 8_640_000_000_000_000;

/** Control characters plus the Windows-reserved/path set. Ordinary
 *  punctuation (spaces, hyphens, dots, commas, ...) is preserved. */
const ILLEGAL_FILENAME_CHARS = /[\u0000-\u001f\u007f-\u009f<>:"/\\|?*]+/g;

/** Raw reply markdown: `text` blocks only, concatenated byte-for-byte in
 *  wire order with no separator. Never trims or normalizes line endings. */
export function sourceMarkdownFromBlocks(blocks: readonly ContentBlock[]): string {
  let source = '';
  for (const block of blocks) {
    if (block && block.type === 'text' && typeof block.text === 'string') source += block.text;
  }
  return source;
}

/** Remove the common markdown framing so the line reads as body text. This
 *  is deliberately not a parser: links/images keep their visible label,
 *  ATX/list/quote prefixes and table pipes are dropped, fenced-code marker
 *  lines contribute no name, and emphasis/code-span markers are removed. */
function stripMarkdownLine(line: string): string {
  if (/^\s*(?:```|~~~)/.test(line)) return '';
  let text = line;
  // Inline images/links: keep the label, drop the destination.
  text = text.replace(/!?\[([^\]]*)\]\([^)]*\)/g, '$1');
  // Autolinks carry no useful short name.
  text = text.replace(/<((?:https?|mailto):[^>\s]+)>/g, '');
  // Leading blockquote(s), list marker, then heading hashes.
  text = text.replace(/^\s*(?:>+\s*)*(?:[-*+]\s+|\d+[.)]\s+)?/, '');
  text = text.replace(/^\s*#{1,6}\s+/, '');
  // Table framing is not part of the header label.
  text = text.replace(/\|/g, ' ');
  // Emphasis, strikethrough and code-span markers.
  text = text.replace(/[*_~`]+/g, '');
  return text;
}

/** First non-empty body line as a filename slug: markdown markers removed,
 *  control/Windows-illegal/path characters replaced with `-`, whitespace
 *  runs collapsed to one space, bounded to 30 code points, trailing dots
 *  and spaces trimmed. */
function replySlug(source: string): string {
  for (const line of source.split(/\r?\n/)) {
    const text = stripMarkdownLine(line)
      .trim()
      .normalize('NFC')
      .replace(ILLEGAL_FILENAME_CHARS, '-')
      .replace(/ +/g, ' ');
    if (text === '') continue;
    const slug = Array.from(text)
      .slice(0, MAX_SLUG_CODE_POINTS)
      .join('')
      .replace(/[. ]+$/g, '');
    if (slug !== '') return slug;
  }
  return FALLBACK_SLUG;
}

/** Local wall clock for the wire timestamp, `YYYYMMDD-HHmmss`. */
function localTimestamp(date: Date): string {
  const pad2 = (value: number): string => String(value).padStart(2, '0');
  return `${date.getFullYear()}${pad2(date.getMonth() + 1)}${pad2(date.getDate())}`
    + `-${pad2(date.getHours())}${pad2(date.getMinutes())}${pad2(date.getSeconds())}`;
}

/** Filesystem-safe `.md` name: `<slug>-<YYYYMMDD-HHmmss>.md`. The timestamp
 *  must be the reply's stable wire timestamp in epoch milliseconds; a
 *  non-integer, negative, non-finite or out-of-Date-range value throws a
 *  fixed-context error that never contains the reply source. */
export function replyMarkdownFileName(source: string, timestampMs: number): string {
  if (
    typeof timestampMs !== 'number'
    || !Number.isSafeInteger(timestampMs)
    || timestampMs < 0
    || timestampMs >= MAX_DATE_MS
  ) {
    throw new Error(TIMESTAMP_ERROR);
  }
  const date = new Date(timestampMs);
  if (Number.isNaN(date.getTime())) throw new Error(TIMESTAMP_ERROR);
  return `${replySlug(source)}-${localTimestamp(date)}.md`;
}
