/** Pure Ctrl-only editing shortcuts for the web/Chat message composers.
 *
 *  The composers own the textarea, the selection and the attachment state.
 *  This module only maps a key press to an edit and an edit to the next
 *  `{ value, caret }`; `clear` therefore clears TEXT only — callers must never
 *  reset attachments. Ctrl-only: meta/alt/shift are left to the browser, and
 *  an IME composition (isComposing or keyCode 229) is never intercepted.
 */

export type ComposerEdit = 'clear' | 'line-start' | 'line-end' | 'delete-word-backward';

export interface ComposerKeyEvent {
  key: string;
  ctrlKey: boolean;
  metaKey: boolean;
  altKey: boolean;
  shiftKey: boolean;
  isComposing?: boolean;
  keyCode?: number;
}

export interface ComposerEditResult {
  value: string;
  caret: number;
}

export function composerEditKey(event: ComposerKeyEvent): ComposerEdit | null {
  if (!event.ctrlKey || event.metaKey || event.altKey || event.shiftKey) return null;
  if (event.isComposing || event.keyCode === 229) return null;
  switch (event.key.toLowerCase()) {
    case 'u':
      return 'clear';
    case 'a':
      return 'line-start';
    case 'e':
      return 'line-end';
    case 'w':
      return 'delete-word-backward';
    default:
      return null;
  }
}

/** One grapheme cluster is the atomic deletion unit, so surrogate pairs and
 *  ZWJ emoji sequences are never split. */
const graphemeSegmenter = new Intl.Segmenter(undefined, { granularity: 'grapheme' });

type WordKind = 'whitespace' | 'delimiter' | 'cjk' | 'word' | 'other';

// Same coarse Unicode classes as the local OMP word navigation: whitespace,
// punctuation/symbol, CJK scripts, letters/numbers/underscore, everything else
// (one grapheme). CJK is its own run, so `abc中文` deletes `中文` first.
const WHITESPACE = /^\p{White_Space}$/u;
const PUNCTUATION = /^\p{P}$/u;
const SYMBOL = /^\p{S}$/u;
const LETTER = /^\p{L}$/u;
const NUMBER = /^\p{N}$/u;
const HAN = /^\p{Script=Han}$/u;
const HIRAGANA = /^\p{Script=Hiragana}$/u;
const KATAKANA = /^\p{Script=Katakana}$/u;
const HANGUL = /^\p{Script=Hangul}$/u;

// Joiners stay inside a word run: `don't` and `foo-bar` delete as one word.
const WORD_JOINERS: Record<string, true> = { "'": true, '’': true, '-': true, '‐': true, '‑': true };

function firstCodePoint(grapheme: string): string {
  const codePoint = grapheme.codePointAt(0);
  return codePoint === undefined ? '' : String.fromCodePoint(codePoint);
}

function wordKind(grapheme: string): WordKind {
  const char = firstCodePoint(grapheme);
  if (!char) return 'other';
  if (WHITESPACE.test(char)) return 'whitespace';
  if (char === '_') return 'word';
  if (PUNCTUATION.test(char) || SYMBOL.test(char)) return 'delimiter';
  if (HAN.test(char) || HIRAGANA.test(char) || KATAKANA.test(char) || HANGUL.test(char)) return 'cjk';
  if (LETTER.test(char) || NUMBER.test(char)) return 'word';
  return 'other';
}

/** Delete-word-backward boundary inside one physical line, as a UTF-16 offset
 *  into `line`: skip trailing whitespace, then delete the run of the class
 *  found at the caret (one grapheme for `other`). */
function wordBoundaryBackward(line: string): number {
  const graphemes = Array.from(graphemeSegmenter.segment(line), (part) => ({
    start: part.index,
    text: part.segment,
  }));
  let index = graphemes.length;
  while (index > 0 && wordKind(graphemes[index - 1].text) === 'whitespace') index -= 1;
  if (index === 0) return 0;

  const kind = wordKind(graphemes[index - 1].text);
  if (kind === 'delimiter' || kind === 'cjk') {
    while (index > 0 && wordKind(graphemes[index - 1].text) === kind) index -= 1;
  } else if (kind === 'word') {
    let hasRightWord = false;
    while (index > 0) {
      const grapheme = graphemes[index - 1];
      const current = wordKind(grapheme.text);
      if (current === 'word') {
        hasRightWord = true;
        index -= 1;
        continue;
      }
      if (current === 'delimiter' && hasRightWord && WORD_JOINERS[firstCodePoint(grapheme.text)] === true) {
        const left = graphemes[index - 2];
        if (left !== undefined && wordKind(left.text) === 'word') {
          index -= 1;
          continue;
        }
      }
      break;
    }
  } else {
    index -= 1;
  }
  return index === 0 ? 0 : graphemes[index].start;
}

export function applyComposerEdit(
  value: string,
  selectionStart: number,
  selectionEnd: number,
  edit: ComposerEdit,
): ComposerEditResult {
  const selectionFrom = Math.min(Math.max(selectionStart, 0), value.length);
  const selectionTo = Math.min(Math.max(selectionEnd, 0), value.length);
  const start = Math.min(selectionFrom, selectionTo);
  const end = Math.max(selectionFrom, selectionTo);

  switch (edit) {
    case 'clear':
      return { value: '', caret: 0 };
    case 'line-start':
      return { value, caret: value.lastIndexOf('\n', start - 1) + 1 };
    case 'line-end': {
      const endOfLine = value.indexOf('\n', start);
      return { value, caret: endOfLine === -1 ? value.length : endOfLine };
    }
    case 'delete-word-backward': {
      if (end > start) {
        return { value: value.slice(0, start) + value.slice(end), caret: start };
      }
      const lineStart = value.lastIndexOf('\n', start - 1) + 1;
      if (start === lineStart) {
        if (start === 0) return { value, caret: 0 };
        // At a line start, Ctrl+W joins the previous line (deletes the newline).
        return { value: value.slice(0, start - 1) + value.slice(start), caret: start - 1 };
      }
      const boundary = lineStart + wordBoundaryBackward(value.slice(lineStart, start));
      return { value: value.slice(0, boundary) + value.slice(start), caret: boundary };
    }
  }
}
