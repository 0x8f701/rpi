#!/usr/bin/env node
// Focused regression for src/slashDispatch.ts + parseSupportedCommand —
// the submit-path decision table that intercepts /compact, /skill, and
// /code-review without optimistic user bubbles. Run through `npm run build`.
//
// Exit codes: 0 = every assertion held; 1 = a regression.
import { parseSupportedCommand } from '../src/commands.ts';
import {
  formatCompactReport,
  formatSkillResult,
  isSnapCompactArgs,
  resolveSlashAction,
} from '../src/slashDispatch.ts';

const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

// ---- parseSupportedCommand: only the Web surface ----
{
  check('bare compact', (() => {
    const p = parseSupportedCommand('/compact');
    return p && p.name === 'compact' && p.args === '';
  })());
  check('compact with instructions', (() => {
    const p = parseSupportedCommand('/compact keep decisions');
    return p && p.name === 'compact' && p.args === 'keep decisions';
  })());
  check('compact --snap', (() => {
    const p = parseSupportedCommand('/compact --snap');
    return p && p.name === 'compact' && p.args === '--snap';
  })());
  check('skill with name', (() => {
    const p = parseSupportedCommand('/skill research');
    return p && p.name === 'skill' && p.args === 'research';
  })());
  check('skill bare still parses (dispatch errors)', (() => {
    const p = parseSupportedCommand('/skill');
    return p && p.name === 'skill' && p.args === '';
  })());
  check('code-review bare', (() => {
    const p = parseSupportedCommand('/code-review');
    return p && p.name === 'code-review' && p.args === '';
  })());
  check('code-review two revs', (() => {
    const p = parseSupportedCommand('/code-review main feature');
    return p && p.name === 'code-review' && p.args === 'main feature';
  })());
  check('unknown slash -> null (stays normal prompt)', parseSupportedCommand('/goal ship') === null);
  check('no slash -> null', parseSupportedCommand('hello') === null);
  check('leading spaces trimmed', (() => {
    const p = parseSupportedCommand('  /compact  --snap  ');
    return p && p.name === 'compact' && p.args === '--snap';
  })());
}

// ---- isSnapCompactArgs ----
{
  check('--snap alone is snap', isSnapCompactArgs('--snap') === true);
  check('--snap with trailing is snap', isSnapCompactArgs('--snap ignored') === true);
  check('empty is not snap', isSnapCompactArgs('') === false);
  check('instructions not snap', isSnapCompactArgs('keep decisions') === false);
  check('--snapshot is NOT snap', isSnapCompactArgs('--snapshot') === false);
  check('snap without dashes is not snap', isSnapCompactArgs('snap') === false);
}

// ---- resolveSlashAction decision table ----
{
  const snap = resolveSlashAction('compact', '--snap');
  check('compact --snap -> snap mode', snap.type === 'compact' && snap.mode === 'snap');

  const snapTrail = resolveSlashAction('compact', '--snap please ignore');
  check(
    'compact --snap trail -> snap',
    snapTrail.type === 'compact' && snapTrail.mode === 'snap',
  );

  const llm = resolveSlashAction('compact', 'keep the plan');
  check(
    'compact instructions -> llm',
    llm.type === 'compact' &&
      llm.mode === 'llm' &&
      llm.customInstructions === 'keep the plan',
  );

  const bareCompact = resolveSlashAction('compact', '');
  check(
    'bare compact -> llm empty instructions',
    bareCompact.type === 'compact' &&
      bareCompact.mode === 'llm' &&
      bareCompact.customInstructions === '',
  );

  const skill = resolveSlashAction('skill', 'research');
  check('skill name', skill.type === 'skill' && skill.name === 'research');

  const skillExtra = resolveSlashAction('skill', 'research extra words');
  check(
    'skill takes first token',
    skillExtra.type === 'skill' && skillExtra.name === 'research',
  );

  const skillBare = resolveSlashAction('skill', '');
  check(
    'skill bare -> error',
    skillBare.type === 'error' && /usage: \/skill/.test(skillBare.message),
  );

  const crBare = resolveSlashAction('code-review', '');
  check(
    'code-review bare -> open no revs',
    crBare.type === 'code-review' && !crBare.from && !crBare.to,
  );

  const crTwo = resolveSlashAction('code-review', 'abc def');
  check(
    'code-review two revs',
    crTwo.type === 'code-review' && crTwo.from === 'abc' && crTwo.to === 'def',
  );

  const crOne = resolveSlashAction('code-review', 'only');
  check(
    'code-review one rev -> error',
    crOne.type === 'error' && /usage: \/code-review/.test(crOne.message),
  );

  const crThree = resolveSlashAction('code-review', 'a b c');
  check('code-review three revs -> error', crThree.type === 'error');

  const unknown = resolveSlashAction('goal', 'x');
  check('unknown name -> error', unknown.type === 'error');
}

// ---- format helpers (visible bubble text) ----
{
  check(
    'compact report with numbers',
    formatCompactReport({ tokensBefore: 100, estimatedTokensAfter: 40 }, 'Compact') ===
      'Compact: 100 → 40 estimated tokens (shrank)',
  );
  check(
    'compact report missing after',
    formatCompactReport({ tokensBefore: 10 }, 'Snapcompact') ===
      'Snapcompact: 10 → ? estimated tokens',
  );
  check(
    'compact report missing before',
    formatCompactReport({}, 'Compact') === 'Compact: done',
  );

  check(
    'skill result with summary',
    formatSkillResult({ name: 'research', summary: 'name: research\ndescription: x' }, 'research') ===
      'name: research\ndescription: x',
  );
  check(
    'skill result empty summary',
    formatSkillResult({ name: 'research', summary: '' }, 'research') ===
      'skill research: (no summary)',
  );
  check(
    'skill result bare string',
    formatSkillResult('plain summary', 'x') === 'plain summary',
  );
}

console.log(`\nslashParse.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
