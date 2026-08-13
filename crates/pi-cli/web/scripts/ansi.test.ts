#!/usr/bin/env node
// Focused regression for src/ansi.ts (the one-pass ANSI SGR parser) and the
// safe React run renderer src/AnsiText.tsx. Covers the exact user-visible
// fragments (`\x1b[96mnew zk sign circuit\x1b[0m`,
// `\x1b[94mbuild inner circuit\x1b[0m`,
// `\x1b[38;5;230m\x1b[48;5;34m 11ms \x1b[0m`), reset isolation, OSC8/cursor
// stripping, malformed/incomplete sequences, plain-text equality, palette
// mapping, and React rendering with no raw ESC / literal `[96m`.
//
// Run through `npm run build`, which bundles this file with Vite's installed
// esbuild into a disposable Node-compatible module before executing the
// focused assertions (same pattern as scripts/transcript.test.ts).
//
// Exit codes: 0 = every assertion held; 1 = a regression.
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { parseAnsi, ansiToPlainText, ansiRgb, type AnsiRun } from '../src/ansi.ts';
import { AnsiText } from '../src/AnsiText.tsx';
import { redactSecrets, safeText } from '../src/redact.ts';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: boolean, detail?: string) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

function styleOf(run: AnsiRun): string {
  return JSON.stringify({
    fg: run.fg,
    bg: run.bg,
    fgRgb: run.fgRgb,
    bgRgb: run.bgRgb,
    bold: run.bold,
    dim: run.dim,
    italic: run.italic,
    underline: run.underline,
  });
}

// ---- exact user-visible fragments ----
{
  const runs = parseAnsi('\x1b[96mnew zk sign circuit\x1b[0m');
  check(
    '96m fragment → one cyan run with exact text',
    runs.length === 1 && runs[0].text === 'new zk sign circuit' && runs[0].fg === 14,
    JSON.stringify(runs.map(styleOf)),
  );
  check(
    '96m fragment → no bg, no modifiers',
    runs.length === 1 && runs[0].bg === undefined && !runs[0].bold && !runs[0].dim && !runs[0].italic && !runs[0].underline,
  );

  const blue = parseAnsi('\x1b[94mbuild inner circuit\x1b[0m');
  check(
    '94m fragment → one light-blue run with exact text',
    blue.length === 1 && blue[0].text === 'build inner circuit' && blue[0].fg === 12,
    JSON.stringify(blue.map(styleOf)),
  );

  const timing = parseAnsi('\x1b[38;5;230m\x1b[48;5;34m 11ms \x1b[0m');
  check(
    '38;5;230/48;5;34 fragment → indexed fg+bg, spacing preserved',
    timing.length === 1 && timing[0].text === ' 11ms ' && timing[0].fg === 230 && timing[0].bg === 34,
    JSON.stringify(timing.map(styleOf)),
  );
}

// ---- reset isolation ----
{
  const runs = parseAnsi('\x1b[96mcyanish\x1b[0mplain\x1b[31mred');
  check(
    'reset splits runs and clears style',
    runs.length === 3
      && runs[0].text === 'cyanish' && runs[0].fg === 14
      && runs[1].text === 'plain' && runs[1].fg === undefined
      && runs[2].text === 'red' && runs[2].fg === 1,
    JSON.stringify(runs.map(styleOf)),
  );
  // No reset: the color carries to the end — including across a newline.
  const carried = parseAnsi('\x1b[96mcyan\ntext');
  check(
    'style carries across newline until reset',
    carried.length === 1 && carried[0].text === 'cyan\ntext' && carried[0].fg === 14,
    JSON.stringify(carried.map(styleOf)),
  );
  // 22 clears bold/dim; 23/24 clear italic/underline.
  const off = parseAnsi('\x1b[1;3;4mstyled\x1b[22;23;24mplain');
  check(
    'off codes clear modifiers',
    off.length === 2
      && off[0].text === 'styled' && off[0].bold && off[0].italic && off[0].underline
      && off[1].text === 'plain' && !off[1].bold && !off[1].italic && !off[1].underline,
    JSON.stringify(off.map(styleOf)),
  );
}

// ---- OSC8 / cursor / control stripping ----
{
  const bel = ansiToPlainText('go\x1b]8;;https://example.com\x07link\x1b]8;;\x07end');
  check('OSC8 BEL-terminated stripped', bel === 'golinkend', `got '${bel}'`);
  const st = ansiToPlainText('go\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\end');
  check('OSC8 ST-terminated stripped', st === 'golinkend', `got '${st}'`);
  const cursor = ansiToPlainText('a\x1b[2;5Hb\x1b[?25lc');
  check('cursor CSI and ?-mode CSI stripped', cursor === 'abc', `got '${cursor}'`);
  const controls = ansiToPlainText('x\x1by\x07z\x08w');
  check('bare ESC + C0 controls dropped', controls === 'xyzw', `got '${controls}'`);
  const tabs = ansiToPlainText('a\tb');
  check('tabs expand to four spaces', tabs === 'a    b', `got '${tabs}'`);
  const newlines = ansiToPlainText('a\nb');
  check('newlines survive', newlines === 'a\nb', `got '${newlines}'`);
}

// ---- malformed / incomplete sequences ----
{
  check('incomplete CSI at EOF stripped', ansiToPlainText('tail\x1b[96') === 'tail');
  check('bare CSI opener stripped', ansiToPlainText('tail\x1b[') === 'tail');
  check('bare ESC stripped, next char kept', ansiToPlainText('x\x1by') === 'xy');
  check('incomplete 256-color payload stripped', ansiToPlainText('\x1b[38;5;mX') === 'X');
  check('incomplete truecolor payload stripped', ansiToPlainText('\x1b[38;2;1;2mX') === 'X');
  // A non-SGR CSI consumes its final byte too: `X` here terminates the
  // sequence, so nothing of it survives.
  check('non-SGR CSI with trailing byte fully stripped', ansiToPlainText('\x1b[38;2;1;2X') === '');
  check('bare 38 with no submode stripped', ansiToPlainText('\x1b[38mX') === 'X');
  check('unterminated OSC consumes to end', ansiToPlainText('a\x1b]8;;https://x') === 'a');
}

// ---- plain text equality / run concatenation ----
{
  const samples = [
    '\x1b[96mnew zk sign circuit\x1b[0m',
    '\x1b[94mbuild inner circuit\x1b[0m',
    '\x1b[38;5;230m\x1b[48;5;34m 11ms \x1b[0m',
    'pre\x1b[31mRED\x1b[0m\x1b[2;5Hpost',
    'go\x1b]8;;https://example.com\x07link\x1b]8;;\x07end',
    'a\tb\n你好\x1b[1mc',
    'x\x1by\x07z\x08w',
    'plain text, no sequences',
    '\x1b[96',
    '\x1b[38;5;',
    '\x1b',
    '',
  ];
  for (const sample of samples) {
    const plain = ansiToPlainText(sample);
    const joined = parseAnsi(sample).map((run) => run.text).join('');
    check(`run concatenation equals plain for ${JSON.stringify(sample)}`, joined === plain);
  }
}

// ---- truecolor / palette mapping ----
{
  const truecolor = parseAnsi('\x1b[38;2;255;0;128mA\x1b[48;2;10;20;30mB\x1b[0m');
  check(
    'truecolor fg/bg map to rgb strings',
    truecolor.length === 2
      && truecolor[0].text === 'A' && truecolor[0].fgRgb === 'rgb(255, 0, 128)'
      && truecolor[1].text === 'B' && truecolor[1].bgRgb === 'rgb(10, 20, 30)',
    JSON.stringify(truecolor.map(styleOf)),
  );
  check('ansiRgb 14 (bright cyan) → rgb(0, 255, 255)', ansiRgb(14) === 'rgb(0, 255, 255)');
  check('ansiRgb 0 (black) → rgb(0, 0, 0)', ansiRgb(0) === 'rgb(0, 0, 0)');
  check('ansiRgb 230 → cube rgb(255, 255, 215)', ansiRgb(230) === 'rgb(255, 255, 215)');
  check('ansiRgb 34 → cube rgb(0, 175, 0)', ansiRgb(34) === 'rgb(0, 175, 0)');
  check('ansiRgb 232 → gray rgb(8, 8, 8)', ansiRgb(232) === 'rgb(8, 8, 8)');
  check('ansiRgb 255 → gray rgb(238, 238, 238)', ansiRgb(255) === 'rgb(238, 238, 238)');
  check('ansiRgb clamps out-of-range', ansiRgb(999) === ansiRgb(255) && ansiRgb(-3) === ansiRgb(0));
}

// ---- React rendering: no raw ESC, no literal [96m, safe text nodes ----
{
  const html = renderToStaticMarkup(
    createElement(
      AnsiText,
      { text: '\x1b[96mnew zk sign circuit\x1b[0m\n\x1b[38;5;230m\x1b[48;5;34m 11ms \x1b[0m' },
    ),
  );
  check('rendered HTML has no raw ESC byte', !html.includes('\u001b'), JSON.stringify(html));
  check('rendered HTML has no literal [96m/[0m fragments', !html.includes('[96m') && !html.includes('[0m'), JSON.stringify(html));
  check(
    'bright-cyan run uses the stable ansi-fg-14 class',
    html.includes('ansi-fg-14') && /class="[^"]*ansi-fg-14[^"]*"[^>]*>new zk sign circuit</.test(html),
    JSON.stringify(html),
  );
  check(
    '256-color run uses inline sanitized rgb',
    html.includes('rgb(255, 255, 215)') && html.includes('rgb(0, 175, 0)'),
    JSON.stringify(html),
  );
  const textContent = html.replace(/<[^>]*>/g, '');
  check(
    'rendered textContent equals the plain projection',
    textContent === 'new zk sign circuit\n 11ms ',
    JSON.stringify(textContent),
  );
}

// ---- hostile input stays inert + fail-closed redaction ----
{
  const hostile = '<img src=x onerror=alert(1)>\x1b[96mok\x1b[0m';
  const html = renderToStaticMarkup(createElement(AnsiText, { text: hostile }));
  check(
    'hostile markup stays escaped text',
    html.includes('&lt;img') && !html.includes('<img'),
    JSON.stringify(html),
  );
  // Fail-closed redaction: parse raw -> plain -> redact the full plain text.
  // Any redaction collapses the whole output to ONE base-style plain run (no
  // ANSI classes) so no styled span can carry a credential.
  const secret = 'sk-ABCDEFGHIJKLMNOPQRST';
  const redacted = renderToStaticMarkup(createElement(AnsiText, { text: `\x1b[96m${secret}\x1b[0m` }));
  const redactedText = redacted.replace(/<[^>]*>/g, '');
  check(
    'credential collapses to a plain redacted run (no ANSI classes)',
    redactedText === '[REDACTED]' && !redacted.includes('ansi-fg') && !redacted.includes(secret) && !redacted.includes('\u001b'),
    JSON.stringify(redacted),
  );
  // A credential split across an SGR boundary in the raw input is contiguous
  // in the plain text, so the fail-closed path still catches it.
  const split = ['sk-', 'ABCDEFGHIJKLMNOPQRST'].join('\x1b[31m');
  const splitHtml = renderToStaticMarkup(
    createElement(AnsiText, { text: `prefix \x1b[96m${split}\x1b[0m suffix` }),
  );
  const splitText = splitHtml.replace(/<[^>]*>/g, '');
  check(
    'credential split across an SGR boundary fails closed to plain [REDACTED]',
    splitText === 'prefix [REDACTED] suffix'
      && !splitHtml.includes('ansi-fg')
      && !splitHtml.includes('\u001b')
      && !splitHtml.includes('sk-'),
    JSON.stringify(splitHtml),
  );
  // The clipboard path is the redacted plain text of the parser output.
  const copy = redactSecrets(ansiToPlainText('\x1b[96mnew zk sign circuit\x1b[0m'));
  check(
    'copy text is plain (no escapes)',
    copy === 'new zk sign circuit',
    JSON.stringify(copy),
  );
  const splitCopy = redactSecrets(ansiToPlainText(`prefix \x1b[96m${split}\x1b[0m suffix`));
  check(
    'copy of a split credential is the redacted plain text',
    splitCopy === 'prefix [REDACTED] suffix',
    JSON.stringify(splitCopy),
  );
  check(
    // A space before the credential gives `\b` a real boundary (an SGR 'm'
    // glued directly to `sk-` is a word char and would not trigger the
    // pattern — the fail-closed plain-text path above still catches that
    // case, since the plain text starts the credential at a boundary).
    'safeText redacts a credential wrapped in ANSI',
    safeText('\x1b[31m sk-ABCDEFGHIJKLMNOPQRST\x1b[0m') === '\x1b[31m [REDACTED]\x1b[0m',
  );
}

if (failures.length > 0) {
  console.error(`ansi: ${failures.length} assertion(s) failed:`);
  for (const f of failures) console.error(`  - ${f}`);
  process.exit(1);
}
console.log(`ansi: ${ran} assertions passed`);
