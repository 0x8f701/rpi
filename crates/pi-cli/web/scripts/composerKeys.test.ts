#!/usr/bin/env node
// Composer editing shortcuts: Ctrl+U clears the whole text, Ctrl+A/E move to
// the current PHYSICAL line's start/end, Ctrl+W deletes the selection or the
// previous OMP-style word (whitespace / CJK / latin word / punctuation-symbol /
// other grapheme), joining the previous line at a line start. The helper is
// pure; attachment preservation and event wiring belong to the composers.
import { applyComposerEdit, composerEditKey } from '../src/composerKeys.ts';
import type { ComposerEdit } from '../src/composerKeys.ts';

const failures: string[] = [];
let ran = 0;
function check(name: string, condition: unknown, detail?: string): void {
  ran += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

function expectEdit(
  name: string,
  value: string,
  selectionStart: number,
  selectionEnd: number,
  edit: ComposerEdit,
  expectedValue: string,
  expectedCaret: number,
): void {
  const result = applyComposerEdit(value, selectionStart, selectionEnd, edit);
  check(
    name,
    result.value === expectedValue && result.caret === expectedCaret,
    `got ${JSON.stringify(result)}, want ${JSON.stringify({ value: expectedValue, caret: expectedCaret })}`,
  );
}

const plain = { metaKey: false, altKey: false, shiftKey: false };

// ---------------------------------------------------------------------------
// Key mapping: Ctrl-only, nothing for meta/alt/shift or plain keys.
check('Ctrl+U maps to clear', composerEditKey({ ...plain, key: 'u', ctrlKey: true }) === 'clear');
check('Ctrl+A maps to line start', composerEditKey({ ...plain, key: 'a', ctrlKey: true }) === 'line-start');
check('Ctrl+E maps to line end', composerEditKey({ ...plain, key: 'e', ctrlKey: true }) === 'line-end');
check('Ctrl+W maps to delete word backward', composerEditKey({ ...plain, key: 'w', ctrlKey: true }) === 'delete-word-backward');
check('key casing is normalized', composerEditKey({ ...plain, key: 'W', ctrlKey: true }) === 'delete-word-backward');
check('a plain key is not a composer edit', composerEditKey({ ...plain, key: 'u', ctrlKey: false }) === null);
check('Cmd+A stays with the browser (no meta)', composerEditKey({ ...plain, key: 'a', ctrlKey: true, metaKey: true }) === null);
check('Ctrl+Alt+W stays with the browser (no alt)', composerEditKey({ ...plain, key: 'w', ctrlKey: true, altKey: true }) === null);
check('Ctrl+Shift+W stays with the browser (no shift)', composerEditKey({ ...plain, key: 'w', ctrlKey: true, shiftKey: true }) === null);
check('unrelated Ctrl chords are ignored', composerEditKey({ ...plain, key: 'c', ctrlKey: true }) === null);
check(
  'an IME composition (isComposing) is never intercepted',
  composerEditKey({ ...plain, key: 'u', ctrlKey: true, isComposing: true }) === null,
);
check(
  'an IME composition (keyCode 229) is never intercepted',
  composerEditKey({ ...plain, key: 'a', ctrlKey: true, keyCode: 229 }) === null,
);

// ---------------------------------------------------------------------------
// Ctrl+U: clear TEXT only (attachments are the caller's state, out of scope).
expectEdit('Ctrl+U clears a multi-line value and its selection', 'one\ntwo', 0, 7, 'clear', '', 0);
expectEdit('Ctrl+U on an empty composer stays empty', '', 0, 0, 'clear', '', 0);

// ---------------------------------------------------------------------------
// Ctrl+A / Ctrl+E: current physical line, not the whole value.
const lines = 'abc\ndef\nghi';

expectEdit('Ctrl+A mid line moves to that line start', lines, 6, 6, 'line-start', lines, 4);
expectEdit('Ctrl+A on the last line start stays put', lines, 8, 8, 'line-start', lines, 8);
expectEdit('Ctrl+A on the first line start stays put', lines, 0, 0, 'line-start', lines, 0);
expectEdit('Ctrl+A when the caret sits on a newline goes to the line it ends', lines, 3, 3, 'line-start', lines, 0);
expectEdit('Ctrl+A uses the selection start', lines, 6, 9, 'line-start', lines, 4);

expectEdit('Ctrl+E mid line moves to that line end', lines, 1, 1, 'line-end', lines, 3);
expectEdit('Ctrl+E on the middle line moves to its end', lines, 4, 4, 'line-end', lines, 7);
expectEdit('Ctrl+E on the last line moves to the value end', lines, 8, 8, 'line-end', lines, 11);
expectEdit('Ctrl+E when the caret sits on a newline stays at that line end', lines, 3, 3, 'line-end', lines, 3);
expectEdit('Ctrl+E uses the selection start', lines, 1, 6, 'line-end', lines, 3);

// ---------------------------------------------------------------------------
// Ctrl+W: selection first, then OMP word classes.
expectEdit('Ctrl+W deletes a forward selection', 'hello world', 0, 5, 'delete-word-backward', ' world', 0);
expectEdit('Ctrl+W deletes a reversed selection', 'hello world', 5, 0, 'delete-word-backward', ' world', 0);
expectEdit('Ctrl+W deletes a multi-line selection', 'ab\ncd', 1, 4, 'delete-word-backward', 'ad', 1);
expectEdit('Ctrl+W on an empty value is a no-op', '', 0, 0, 'delete-word-backward', '', 0);

expectEdit('Ctrl+W deletes the latin word before the caret', 'hello world', 11, 11, 'delete-word-backward', 'hello ', 6);
expectEdit('Ctrl+W at a word end deletes only that word', 'hello world', 5, 5, 'delete-word-backward', ' world', 0);
expectEdit('Ctrl+W at the document start is a no-op', 'abc', 0, 0, 'delete-word-backward', 'abc', 0);
expectEdit(
  'Ctrl+W over trailing spaces deletes the spaces and the word before them',
  'foo   ',
  6,
  6,
  'delete-word-backward',
  '',
  0,
);
expectEdit(
  'Ctrl+W at a space run deletes the run plus the word before it',
  'foo   bar',
  6,
  6,
  'delete-word-backward',
  'bar',
  0,
);
expectEdit('Ctrl+W keeps the joiners inside a word', "don't stop", 5, 5, 'delete-word-backward', ' stop', 0);

expectEdit('Ctrl+W deletes a CJK run after latin text', 'abc中文', 5, 5, 'delete-word-backward', 'abc', 3);
expectEdit('Ctrl+W deletes a latin word after CJK text', '中文abc', 5, 5, 'delete-word-backward', '中文', 2);
expectEdit('Ctrl+W never merges CJK into the neighbouring latin word', '你好world', 7, 7, 'delete-word-backward', '你好', 2);
expectEdit('Ctrl+W deletes a punctuation run', 'foo!!!', 6, 6, 'delete-word-backward', 'foo', 3);
expectEdit('Ctrl+W deletes a latin word before punctuation', 'a..b', 4, 4, 'delete-word-backward', 'a..', 3);

expectEdit('Ctrl+W deletes one emoji grapheme', 'hi😀', 4, 4, 'delete-word-backward', 'hi', 2);
const family = 'x👨‍👩‍👧‍👦';
expectEdit(
  'Ctrl+W deletes a ZWJ emoji family as one grapheme (no surrogate split)',
  family,
  family.length,
  family.length,
  'delete-word-backward',
  'x',
  1,
);

expectEdit('Ctrl+W at a line start deletes the preceding newline', 'ab\ncd', 3, 3, 'delete-word-backward', 'abcd', 2);
expectEdit('Ctrl+W at a blank line joins the previous line', 'ab\n\ncd', 3, 3, 'delete-word-backward', 'ab\ncd', 2);
expectEdit(
  'Ctrl+W stays inside the current line',
  'one\ntwo three',
  13,
  13,
  'delete-word-backward',
  'one\ntwo ',
  8,
);
expectEdit(
  'Ctrl+W deletes the word after leading indentation without touching it',
  'ab\n  cd',
  7,
  7,
  'delete-word-backward',
  'ab\n  ',
  5,
);
expectEdit(
  'Ctrl+W on an all-whitespace line deletes up to the line start',
  'ab\n   ',
  6,
  6,
  'delete-word-backward',
  'ab\n',
  3,
);

console.log(`\ncomposerKeys.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
