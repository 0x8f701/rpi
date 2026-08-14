#!/usr/bin/env node
// Focused regression for src/slashDispatch.ts + parseSupportedCommand —
// the submit-path decision table that intercepts /compact, /skill, and
// /code-review without optimistic user bubbles. Run through `npm run build`.
//
// Exit codes: 0 = every assertion held; 1 = a regression.
import { parseSupportedCommand } from '../src/commands.ts';
import {
  formatCompactReport,
  formatProcessList,
  formatProcessRow,
  formatSkillResult,
  isSnapCompactArgs,
  psWire,
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
  check('loop with args', (() => {
    const p = parseSupportedCommand('/loop 5m check deploy');
    return p && p.name === 'loop' && p.args === '5m check deploy';
  })());
  check('goal bare still parses (dispatch shows state)', (() => {
    const p = parseSupportedCommand('/goal');
    return p && p.name === 'goal' && p.args === '';
  })());
  check('goal with objective', (() => {
    const p = parseSupportedCommand('/goal ship safely');
    return p && p.name === 'goal' && p.args === 'ship safely';
  })());
  check('ps bare', (() => {
    const p = parseSupportedCommand('/ps');
    return p && p.name === 'ps' && p.args === '';
  })());
  check('ps with tail still parses (rejected later)', (() => {
    const p = parseSupportedCommand('/ps extra');
    return p && p.name === 'ps' && p.args === 'extra';
  })());
  check('unknown slash -> null (stays normal prompt)', parseSupportedCommand('/workflow ship') === null);
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

  const unknown = resolveSlashAction('workflow', 'x');
  check('unknown name -> error', unknown.type === 'error');

  // /loop + /goal resolve to typed actions (their full decision table lives
  // in scripts/loopGoal.test.ts).
  const loopSmoke = resolveSlashAction('loop', '5m check deploy');
  check('loop smoke -> typed loop action', loopSmoke.type === 'loop' && loopSmoke.action.op === 'create');
  const loopBare = resolveSlashAction('loop', '');
  check('bare /loop -> usage error', loopBare.type === 'error' && /usage: \/loop/.test(loopBare.message));
  const goalSmoke = resolveSlashAction('goal', 'show');
  check('goal smoke -> typed goal action', goalSmoke.type === 'goal' && goalSmoke.action.op === 'show');
  const goalBare = resolveSlashAction('goal', '');
  check('bare /goal -> show (panel, not explicit)', goalBare.type === 'goal' && goalBare.action.op === 'show');
  // /ps is a bare-only surface: the bare command dispatches process_list;
  // any argument tail is a LOCAL usage error (no RPC, draft preserved).
  const psBare = resolveSlashAction('ps', '');
  check('bare /ps -> typed ps action', psBare.type === 'ps');
  const psArgs = resolveSlashAction('ps', 'extra');
  check('/ps extra -> usage error', psArgs.type === 'error' && psArgs.message === 'usage: /ps');
  const psArgsTrimmed = resolveSlashAction('ps', '   extra   ');
  check('/ps whitespace-padded tail -> usage error', psArgsTrimmed.type === 'error' && psArgsTrimmed.message === 'usage: /ps');
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

// ---- /ps wire mapping + bounded TUI-parity process formatter ----
{
  check('ps wire -> process_list', JSON.stringify(psWire()) === JSON.stringify({ type: 'process_list' }));

  const running = {
    id: 'proc-1',
    ownerId: 'owner-1',
    label: 'deploy probe',
    state: 'running',
    pid: 4242,
    tty: false,
    startedAtMs: 1750000000000,
    outputStartCursor: 0,
    outputCursor: 128,
  };
  check('running row mirrors TUI line', formatProcessRow(running) ===
    'proc-1\tRunning\tdeploy probe\tcursor 0..128');
  check('row without label -> (unlabeled)', formatProcessRow({ ...running, label: null }).includes('\t(unlabeled)\t'));
  check('row without label field -> (unlabeled)', formatProcessRow({ ...running, label: undefined }).includes('\t(unlabeled)\t'));
  check('non-string label -> (unlabeled)', formatProcessRow({ ...running, label: 42 }).includes('\t(unlabeled)\t'));

  // State mapping: wire snake_case -> Rust Debug variant name (TUI parity).
  const states = {
    starting: 'Starting', running: 'Running', stopping: 'Stopping', exited: 'Exited',
    timed_out: 'TimedOut', expired: 'Expired', failed: 'Failed',
  };
  for (const [wire, label] of Object.entries(states)) {
    check(`state ${wire} -> ${label}`, formatProcessRow({ ...running, state: wire }).includes(`\t${label}\t`));
  }
  check('unknown state falls back to wire string', formatProcessRow({ ...running, state: 'suspending' }).includes('\tsuspending\t'));
  check('non-string state -> ?', formatProcessRow({ ...running, state: 7 }).includes('\t?\t'));
  check('missing id -> ?', formatProcessRow({ id: null, state: 'running' }).startsWith('?\tRunning\t'));

  // Sanitized labels: control characters (row/column injection) are stripped,
  // length is bounded.
  check('label newline stripped', formatProcessRow({ ...running, label: 'a\nb' }).includes('\tab\t'));
  check('label tab stripped', formatProcessRow({ ...running, label: 'a\tb' }) === 'proc-1\tRunning\tab\tcursor 0..128');
  check('label carriage return stripped', formatProcessRow({ ...running, label: 'a\rb' }).includes('\tab\t'));
  check('long label truncated with ellipsis', formatProcessRow({ ...running, label: 'x'.repeat(200) }).includes(`\t${'x'.repeat(64)}…\t`));
  check('label truncation respects bound', formatProcessRow({ ...running, label: 'y'.repeat(200) }).includes(`\t${'y'.repeat(64)}…\t`));
  check('malformed row never throws', typeof formatProcessRow(null) === 'string' && typeof formatProcessRow('junk') === 'string');
  check('cursor fields default to 0', formatProcessRow({ ...running, outputStartCursor: null, outputCursor: 'x' }).endsWith('cursor 0..0'));

  // Empty list -> TUI marker.
  check('empty list -> No supervised processes', formatProcessList([]) === 'No supervised processes');
  check('non-array list -> No supervised processes', formatProcessList(null) === 'No supervised processes');
  check('non-array object -> No supervised processes', formatProcessList({ processes: [] }) === 'No supervised processes');
  // List joins rows.
  const listText = formatProcessList([running, { ...running, id: 'proc-2', state: 'exited', label: null }]);
  check('list joins rows', listText.split('\n').length === 2, listText);
  check('list row shape', listText === 'proc-1\tRunning\tdeploy probe\tcursor 0..128\nproc-2\tExited\t(unlabeled)\tcursor 0..128');
  // Bounded: 20 processes render 16 rows + an overflow marker (backend cap).
  const many = Array.from({ length: 20 }, (_, i) => ({ ...running, id: `proc-${i}` }));
  const manyText = formatProcessList(many);
  check('list bounded to 16 rows', manyText.split('\n').length === 17, manyText);
  check('list overflow marker', manyText.endsWith('… and 4 more'));
}

console.log(`\nslashParse.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
