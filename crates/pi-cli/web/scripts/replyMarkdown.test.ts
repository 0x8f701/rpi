#!/usr/bin/env node
// Reply copy/save source + filename contract.
//
// `sourceMarkdownFromBlocks` is the single raw-markdown source shared by the
// reply's copy and save actions: text blocks only, verbatim, in wire order,
// with no separator and no trimming. `replyMarkdownFileName` derives a
// bounded filesystem-safe name from the first non-empty body line and the
// stable wire timestamp (local wall clock, never the browser clock), keeps
// ordinary characters, strips common markdown markers, replaces only
// control/Windows-illegal/path characters, and throws a FIXED-context error
// (no reply text) for an unusable timestamp.
//
// Run through `npm run build`/`postbuild` (esbuild-bundled like the other
// pure-module tests).
import type { ContentBlock } from '../src/types';
import { replyMarkdownFileName, sourceMarkdownFromBlocks } from '../src/replyMarkdown';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: unknown, detail?: string): void {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

function textBlocks(...texts: string[]): ContentBlock[] {
  return texts.map((text) => ({ type: 'text', text }));
}

/** Milliseconds for an explicit LOCAL wall-clock instant, so the expected
 *  filename below is timezone-independent (local constructor -> local
 *  getters round-trips). */
function localMs(year: number, month: number, day: number, hour: number, minute: number, second: number): number {
  return new Date(year, month - 1, day, hour, minute, second).getTime();
}

const TS = localMs(2026, 11, 12, 9, 5, 3);
const STAMP = '20261112-090503';
const nameFor = (source: string, timestampMs: number = TS): string => replyMarkdownFileName(source, timestampMs);

/* ------------------------------------------------------------------ *
 * Raw source: text blocks only, verbatim, no separator, no trimming.
 * ------------------------------------------------------------------ */
check(
  'text blocks concatenate in wire order with no separator',
  sourceMarkdownFromBlocks([
    { type: 'text', text: 'Deploy' },
    { type: 'thinking', thinking: 'secret plan' },
    { type: 'image', data: 'AAAA', mimeType: 'image/png' },
    { type: 'toolCall', toolCall: { id: 't1', name: 'bash', arguments: {} } },
    { type: 'text', text: ' notes' },
  ]) === 'Deploy notes',
);

check(
  'source is not trimmed and keeps CRLF, fences, tables and indentation',
  (() => {
    const raw = '  # Heading\r\n\r\n| A | B |\r\n| - | - |\r\n```ts\r\nconst x = 1;\r\n```\r\n';
    return sourceMarkdownFromBlocks(textBlocks(raw)) === raw;
  })(),
);

check(
  'blocks without string text contribute nothing',
  sourceMarkdownFromBlocks([
    { type: 'text' },
    { type: 'thinking', thinking: 'only thinking' },
    { type: 'text', text: '' },
  ]) === '',
);

check('an empty block list yields an empty source', sourceMarkdownFromBlocks([]) === '');

/* ------------------------------------------------------------------ *
 * Filename: first non-empty body line with common markdown markers gone.
 * ------------------------------------------------------------------ */
check(
  'heading and emphasis markers do not leak into the name',
  nameFor('# Ship **now**\n\n```ts\nconst x = 1;\n```') === `Ship now-${STAMP}.md`,
  nameFor('# Ship **now**\n\n```ts\nconst x = 1;\n```'),
);

check(
  'a leading fence is skipped and its code body remains ordinary text',
  nameFor('```ts\nconst x = 1;\n```') === `const x = 1;-${STAMP}.md`,
  nameFor('```ts\nconst x = 1;\n```'),
);

check(
  'links and images keep their visible label only',
  nameFor('See [the guide](https://example.com/a/b) and ![diagram](x.png) now')
    === `See the guide and diagram now-${STAMP}.md`,
  nameFor('See [the guide](https://example.com/a/b) and ![diagram](x.png) now'),
);

check(
  'multi-block table source names the file from its header row',
  (() => {
    const blocks = textBlocks('| Step | Cmd |\n', '| --- | --- |\n', '| Build | `npm run build` |\n');
    check(
      'table blocks concatenate verbatim',
      sourceMarkdownFromBlocks(blocks) === '| Step | Cmd |\n| --- | --- |\n| Build | `npm run build` |\n',
    );
    return nameFor(sourceMarkdownFromBlocks(blocks)) === `Step Cmd-${STAMP}.md`;
  })(),
  nameFor('| Step | Cmd |\n| --- | --- |\n'),
);

check(
  'blockquote, list, code-span and emphasis framing is stripped',
  nameFor('> **Hotfix** for `auth` now') === `Hotfix for auth now-${STAMP}.md`,
  nameFor('> **Hotfix** for `auth` now'),
);

check(
  'ordinary spaces, hyphens and punctuation are preserved',
  nameFor('v1.2 quick-fix, ready') === `v1.2 quick-fix, ready-${STAMP}.md`,
  nameFor('v1.2 quick-fix, ready'),
);

check(
  'trailing dots and spaces are trimmed',
  nameFor('Report...  ') === `Report-${STAMP}.md`,
  nameFor('Report...  '),
);

check(
  'no-separator concatenation also drives the first body line',
  nameFor(sourceMarkdownFromBlocks(textBlocks('Deploy', ' notes'))) === `Deploy notes-${STAMP}.md`,
);

check(
  'whitespace-only source falls back to the fixed slug',
  nameFor('   \n\t\n\r\n') === `reply-${STAMP}.md`,
);

check(
  'empty source falls back to the fixed slug',
  nameFor('') === `reply-${STAMP}.md`,
);

check(
  'a line that is only markdown markers falls through to the next body line',
  nameFor('***\nReal **title**') === `Real title-${STAMP}.md`,
  nameFor('***\nReal **title**'),
);

/* ------------------------------------------------------------------ *
 * Filename: 30 Unicode code points (not UTF-16 units).
 * ------------------------------------------------------------------ */
check(
  'slug is capped at exactly 30 Unicode code points (astral letters)',
  (() => {
    const astral = '\u{1D400}'.repeat(40);
    const name = nameFor(astral);
    const expected = `${'\u{1D400}'.repeat(30)}-${STAMP}.md`;
    check('astral slug equals 30 code points', name === expected, name);
    check(
      'astral filename exceeds 30 UTF-16 units while the slug is 30 code points',
      name.length > 30 + STAMP.length + 1,
      String(name.length),
    );
    return true;
  })(),
);

check(
  'long CJK body lines are also capped at 30 code points',
  nameFor('汉'.repeat(45)) === `${'汉'.repeat(30)}-${STAMP}.md`,
);

check(
  'plain ASCII body lines are also capped at 30 code points',
  nameFor('# ' + 'A'.repeat(50)) === `${'A'.repeat(30)}-${STAMP}.md`,
);

/* ------------------------------------------------------------------ *
 * Filename: only control/Windows-illegal/path characters are replaced.
 * ------------------------------------------------------------------ */
check(
  'path separators become hyphens',
  nameFor('a/b') === `a-b-${STAMP}.md`,
  nameFor('a/b'),
);

check(
  'control characters become hyphens',
  nameFor('a' + String.fromCharCode(1) + 'b') === `a-b-${STAMP}.md`,
  nameFor('a' + String.fromCharCode(1) + 'b'),
);

check(
  'the full Windows-reserved/path set never reaches the name',
  (() => {
    const source = 'a/b' + String.fromCharCode(92) + 'c:d"e<f>g|h?i*j' + String.fromCharCode(1);
    const name = nameFor(source);
    check('no illegal or control characters', !/[\u0000-\u001f\u007f-\u009f<>:"/\\|?*]/.test(name), name);
    check('no whitespace other than the one space run', !/\s/.test(name.replace(' ', '')), name);
    check('body words survive', name.startsWith('a-b-c-d-e-f-g h-ij'), name);
    return true;
  })(),
);

/* ------------------------------------------------------------------ *
 * Filename: stable wire timestamp in local wall-clock form.
 * ------------------------------------------------------------------ */
check(
  'timestamp argument formats as local YYYYMMDD-HHmmss',
  nameFor('x', localMs(2024, 1, 2, 3, 4, 5)) === 'x-20240102-030405.md',
  nameFor('x', localMs(2024, 1, 2, 3, 4, 5)),
);

check(
  'local timestamp pads month/day/hour/minute/second',
  nameFor('x', localMs(2026, 11, 12, 23, 59, 59)) === 'x-20261112-235959.md',
);

check(
  'the wire timestamp (not the browser clock) defines the suffix',
  nameFor('x', localMs(2000, 1, 1, 0, 0, 0)) === 'x-20000101-000000.md',
);

check(
  'the same source and timestamp produce a stable name',
  nameFor('Stable reply') === nameFor('Stable reply')
    && nameFor('Stable reply') === `Stable reply-${STAMP}.md`,
);

check(
  'the same timestamp with different sources changes only the slug',
  nameFor('Alpha') === `Alpha-${STAMP}.md` && nameFor('Beta') === `Beta-${STAMP}.md`,
);

/* ------------------------------------------------------------------ *
 * Filename: invalid timestamps throw fixed context without the source.
 * ------------------------------------------------------------------ */
check(
  'invalid timestamps throw one fixed, source-free error',
  (() => {
    const secretSource = 'SECRET-REPLY-BODY';
    const expectedError = 'reply markdown filename: invalid timestamp';
    const badValues: unknown[] = [
      Number.NaN,
      Number.POSITIVE_INFINITY,
      Number.NEGATIVE_INFINITY,
      1.5,
      -1,
      8_640_000_000_000_000,
      undefined,
      '1700000000000',
    ];
    for (const bad of badValues) {
      let message = '';
      try {
        replyMarkdownFileName(secretSource, bad as number);
      } catch (error) {
        message = error instanceof Error ? error.message : String(error);
      }
      check(
        `invalid timestamp ${String(bad)} throws the fixed context`,
        message === expectedError,
        message || '(no throw)',
      );
      check(
        `invalid timestamp ${String(bad)} never leaks the reply source`,
        !message.includes('SECRET'),
        message,
      );
    }
    return true;
  })(),
);

console.log(`\nreplyMarkdown.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
