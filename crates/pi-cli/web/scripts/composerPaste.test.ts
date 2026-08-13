#!/usr/bin/env node
import { MAX_FILE_BYTES } from '../src/attachments.ts';
import {
  LARGE_TEXT_PASTE_THRESHOLD,
  LARGE_TEXT_PREVIEW_CHARS,
  PASTED_TEXT_ATTACHMENT_NAME,
  largeTextDisplay,
  planLargeTextPaste,
} from '../src/composerPaste.ts';

const failures: string[] = [];
let ran = 0;
function check(name: string, condition: boolean, detail?: string): void {
  ran += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

const below = planLargeTextPaste('x'.repeat(LARGE_TEXT_PASTE_THRESHOLD - 1));
check('text below the threshold stays native', below.type === 'native');

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

const display = largeTextDisplay('x'.repeat(LARGE_TEXT_PASTE_THRESHOLD));
check('large display metadata is produced', display !== null);
if (display) {
  check('large display preserves full character count', display.characters === LARGE_TEXT_PASTE_THRESHOLD);
  check('large display preserves full byte count', display.bytes === LARGE_TEXT_PASTE_THRESHOLD);
  check('large display preview is bounded', display.preview.length < LARGE_TEXT_PREVIEW_CHARS + 100);
  check('large display reports omitted characters', display.omittedCharacters === LARGE_TEXT_PASTE_THRESHOLD - LARGE_TEXT_PREVIEW_CHARS);
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

console.log(`\ncomposerPaste.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
