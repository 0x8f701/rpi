#!/usr/bin/env node
import { MAX_FILE_BYTES } from '../src/attachments.ts';
import {
  COMPOSER_PASTE_ENTER_GUARD_MS,
  LARGE_TEXT_PASTE_LINE_THRESHOLD,
  LARGE_TEXT_PASTE_THRESHOLD,
  LARGE_TEXT_PREVIEW_CHARS,
  PASTED_TEXT_ATTACHMENT_NAME,
  composerBeforeInputShouldSubmit,
  composerEnterShouldPreventDefault,
  composerEnterShouldSubmit,
  largeTextDisplay,
  logicalLineCount,
  nativeEventShiftKey,
  nextPasteEnterGuard,
  planLargeTextPaste,
} from '../src/composerPaste.ts';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const failures: string[] = [];
let ran = 0;
function check(name: string, condition: boolean, detail?: string): void {
  ran += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

const below = planLargeTextPaste('x'.repeat(LARGE_TEXT_PASTE_THRESHOLD - 1));
check('single-line text below the byte threshold stays native', below.type === 'native');

const twoLines = planLargeTextPaste('line one\nline two');
check('two-line paste stays native', twoLines.type === 'native');
check('one-line paste stays native', planLargeTextPaste('just one line').type === 'native');
check('crlf two-line paste stays native', planLargeTextPaste('one\r\ntwo').type === 'native');
check('code-review two-commit paste stays native', planLargeTextPaste('/code-review abc123 def456').type === 'native');
check('code-review newline-separated commits stay native', planLargeTextPaste('/code-review abc123\ndef456').type === 'native');
check('line-count helper treats a\\nb as two lines', logicalLineCount('a\nb') === 2);
check('empty text is zero lines', logicalLineCount('') === 0);

const manyLines = Array.from({ length: LARGE_TEXT_PASTE_LINE_THRESHOLD }, (_, index) => `line ${index}`).join('\n');
const manyPlan = planLargeTextPaste(manyLines);
check('threshold-line paste becomes an attachment', manyPlan.type === 'attachment');
if (manyPlan.type === 'attachment') {
  check('threshold-line attachment keeps every line', manyPlan.attachment.text === manyLines);
}
const justUnderLines = Array.from({ length: LARGE_TEXT_PASTE_LINE_THRESHOLD - 1 }, (_, index) => `line ${index}`).join('\n');
check('just-under-threshold lines stay native', planLargeTextPaste(justUnderLines).type === 'native');

const boundary = planLargeTextPaste('x'.repeat(LARGE_TEXT_PASTE_THRESHOLD));
check('threshold text becomes an attachment', boundary.type === 'attachment');
if (boundary.type === 'attachment') {
  check('attachment has the stable pasted-text name', boundary.attachment.name === PASTED_TEXT_ATTACHMENT_NAME);
  check('attachment is a code/text attachment', boundary.attachment.kind === 'code');
  check('attachment carries the complete text', boundary.attachment.text?.length === LARGE_TEXT_PASTE_THRESHOLD);
  check('attachment byte size is exact', boundary.attachment.size === LARGE_TEXT_PASTE_THRESHOLD);
  check('attachment uses text/plain', boundary.attachment.mimeType === 'text/plain');
  check('attachment uses the text fence hint', boundary.attachment.language === 'text');
}

const unicode = '界'.repeat(Math.ceil(LARGE_TEXT_PASTE_THRESHOLD / 3));
const unicodePlan = planLargeTextPaste(unicode);

const displaySource = 'x'.repeat(LARGE_TEXT_PREVIEW_CHARS * 2);
const display = largeTextDisplay(displaySource);
check('large display metadata is produced', display !== null);
if (display) {
  check('large display preserves full character count', display.characters === displaySource.length);
  check('large display preserves full byte count', display.bytes === displaySource.length);
  check('large display preview is bounded', display.preview.length < LARGE_TEXT_PREVIEW_CHARS + 100);
  check('large display reports omitted characters', display.omittedCharacters === displaySource.length - LARGE_TEXT_PREVIEW_CHARS);
  check('large display includes omission marker', display.preview.includes('characters omitted'));
}
check('small display stays on normal Markdown path', largeTextDisplay('small') === null);
check('large unicode text becomes an attachment', unicodePlan.type === 'attachment');
if (unicodePlan.type === 'attachment') {
  check('unicode size uses UTF-8 bytes', unicodePlan.attachment.size === unicode.length * 3);
}

const oversize = planLargeTextPaste('x'.repeat(MAX_FILE_BYTES + 1));
check('text above the attachment limit is rejected', oversize.type === 'oversize');
if (oversize.type === 'oversize') {
  check('oversize reports exact UTF-8 bytes', oversize.size === MAX_FILE_BYTES + 1);
}

check(
  'plain Enter submits',
  composerEnterShouldSubmit({ key: 'Enter', shiftKey: false, now: 1_000, pasteGuardUntil: 0 }),
);
check(
  'Shift+Enter does not submit',
  !composerEnterShouldSubmit({ key: 'Enter', shiftKey: true, now: 1_000, pasteGuardUntil: 0 }),
);
check(
  'IME composition does not submit',
  !composerEnterShouldSubmit({ key: 'Enter', shiftKey: false, isComposing: true, now: 1_000, pasteGuardUntil: 0 }),
);
check(
  'GBoard Unidentified keyCode 13 submits',
  composerEnterShouldSubmit({
    key: 'Unidentified',
    shiftKey: false,
    keyCode: 13,
    now: 1_000,
    pasteGuardUntil: 0,
    compositionActive: false,
  }),
);
check(
  'GBoard empty key which 13 submits',
  composerEnterShouldSubmit({
    key: '',
    shiftKey: false,
    which: 13,
    now: 1_000,
    pasteGuardUntil: 0,
    compositionActive: false,
  }),
);
check(
  'IME keyCode 229 does not submit',
  !composerEnterShouldSubmit({
    key: 'Enter',
    shiftKey: false,
    keyCode: 229,
    now: 1_000,
    pasteGuardUntil: 0,
    compositionActive: false,
  }),
);
check(
  'active composition blocks Enter even after native isComposing drops',
  !composerEnterShouldSubmit({
    key: 'Enter',
    shiftKey: false,
    isComposing: false,
    keyCode: 13,
    compositionActive: true,
    now: 1_000,
    pasteGuardUntil: 0,
  }),
);
check(
  'sticky isComposing after compositionend still submits',
  composerEnterShouldSubmit({
    key: 'Enter',
    shiftKey: false,
    isComposing: true,
    keyCode: 13,
    compositionActive: false,
    now: 1_000,
    pasteGuardUntil: 0,
  }),
);
check(
  'beforeinput insertParagraph submits when keydown did not handle Enter',
  composerBeforeInputShouldSubmit({
    inputType: 'insertParagraph',
    shiftKey: false,
    compositionActive: false,
    keydownHandled: false,
    now: 1_000,
    pasteGuardUntil: 0,
  }),
);
check(
  'beforeinput insertLineBreak with Shift stays a newline',
  !composerBeforeInputShouldSubmit({
    inputType: 'insertLineBreak',
    shiftKey: true,
    now: 1_000,
    pasteGuardUntil: 0,
  }),
);
check(
  'beforeinput does not double-submit after keydown already handled Enter',
  !composerBeforeInputShouldSubmit({
    inputType: 'insertParagraph',
    shiftKey: false,
    keydownHandled: true,
    now: 1_000,
    pasteGuardUntil: 0,
  }),
);
const guardUntil = nextPasteEnterGuard(1_000);
check('paste guard lasts the documented window', guardUntil === 1_000 + COMPOSER_PASTE_ENTER_GUARD_MS);
check(
  'Enter during the paste guard does not submit',
  !composerEnterShouldSubmit({ key: 'Enter', shiftKey: false, now: 1_000 + COMPOSER_PASTE_ENTER_GUARD_MS - 1, pasteGuardUntil: guardUntil }),
);
check(
  'Enter during the paste guard is still preventDefaulted',
  composerEnterShouldPreventDefault({ key: 'Enter', shiftKey: false, now: 1_000 + COMPOSER_PASTE_ENTER_GUARD_MS - 1, pasteGuardUntil: guardUntil }),
);
check(
  'Enter after the paste guard submits',
  composerEnterShouldSubmit({ key: 'Enter', shiftKey: false, now: guardUntil, pasteGuardUntil: guardUntil }),
);
check(
  'Enter after the paste guard is preventDefaulted',
  composerEnterShouldPreventDefault({ key: 'Enter', shiftKey: false, now: guardUntil, pasteGuardUntil: guardUntil }),
);
check(
  'Shift+Enter is not preventDefaulted',
  !composerEnterShouldPreventDefault({ key: 'Enter', shiftKey: true, now: 1_000, pasteGuardUntil: 0 }),
);
check(
  'native beforeinput Shift is read from the InputEvent',
  nativeEventShiftKey({ shiftKey: true } as unknown as Event)
    && !nativeEventShiftKey({} as Event),
);

{
  const appSource = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../src/App.tsx'), 'utf8');
  const promptAt = appSource.indexOf('id="prompt-input"');
  const slice = promptAt >= 0 ? appSource.slice(promptAt, promptAt + 2800) : '';
  const preventHelperAt = slice.indexOf('composerEnterShouldPreventDefault');
  const preventAt = slice.indexOf('e.preventDefault()');
  const submitAt = slice.indexOf('composerEnterShouldSubmit');
  check(
    'App preventDefault follows composerEnterShouldPreventDefault before submit',
    preventHelperAt >= 0 && preventAt > preventHelperAt && submitAt > preventAt,
    String({ preventHelperAt, preventAt, submitAt }),
  );
  check(
    'App GBoard beforeinput and send enterKeyHint are wired on the composer',
    slice.includes('composerBeforeInputShouldSubmit')
      && slice.includes('enterKeyHint="send"')
      && slice.includes('compositionActive: composerComposingRef.current'),
    slice.slice(0, 200),
  );
}

console.log(`\ncomposerPaste.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
