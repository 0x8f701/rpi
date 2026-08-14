#!/usr/bin/env node
// Focused regression for the /loop + /goal Web composer surface in
// src/slashDispatch.ts — pure typed parsers (mirroring the TUI parsers in
// loop_commands.rs / goal_commands.rs), the RPC wire mapping (existing
// loop_*/goal_* wire shapes), and the defensive bounded visible-summary
// formatters. No DOM/browser/socket dependency. Run through `npm run build`.
//
// Exit codes: 0 = every assertion held; 1 = a regression.
//
// Assertions exercise BEHAVIOR (parse decisions, wire frames, rendered text),
// not source strings. The backend get_commands catalog stays authoritative;
// the TUI parsers are the semantic authority this surface mirrors.
import {
  formatGoalFromWire,
  formatGoalPins,
  formatGoalResult,
  formatGoalState,
  formatLoopList,
  formatLoopResult,
  formatLoopTaskRow,
  goalActivationPrefix,
  goalWire,
  isIntervalToken,
  loopIntervalToHuman,
  loopRequiresIdle,
  loopWire,
  parseGoalArgs,
  parseLoopArgs,
  unsupportedAliasMessage,
} from '../src/slashDispatch.ts';

const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}
function ok(action, name) {
  return action.ok && action.action.op === name;
}

/* ---------------- interval tokens (is_interval_token parity) ---------------- */
{
  check('bare seconds token', isIntervalToken('300') === true);
  check('single second token', isIntervalToken('3') === true);
  check('suffix tokens', ['3s', '5m', '2h', '1d'].every((t) => isIntervalToken(t) === true));
  check('empty is not a token', isIntervalToken('') === false);
  check('letters not a token', isIntervalToken('soon') === false);
  check('zero is a token (validated later)', isIntervalToken('0') === true);
  check('bare suffix not a token', isIntervalToken('m') === false);
  check('bad suffix not a token', isIntervalToken('5x') === false);
  check('mixed not a token', isIntervalToken('5m2') === false);
}

/* ---------------- parseLoopArgs: /loop decision table ---------------- */
{
  // Primary subcommand style + legacy bare create.
  check('bare /loop -> usage error', !parseLoopArgs('').ok && /usage: \/loop/.test(parseLoopArgs('').message));
  check('create 5m check deploy', (() => {
    const r = parseLoopArgs('create 5m check deploy');
    return ok(r, 'create') && r.action.interval === '5m' && r.action.prompt === 'check deploy';
  })());
  check('legacy 5m check deploy', (() => {
    const r = parseLoopArgs('5m check deploy');
    return ok(r, 'create') && r.action.interval === '5m' && r.action.prompt === 'check deploy';
  })());
  check('legacy 3s echo hello', (() => {
    const r = parseLoopArgs('3s echo hello');
    return ok(r, 'create') && r.action.interval === '3s' && r.action.prompt === 'echo hello';
  })());
  check('legacy 300 echo hello', (() => {
    const r = parseLoopArgs('300 echo hello');
    return ok(r, 'create') && r.action.interval === '300' && r.action.prompt === 'echo hello';
  })());
  // A leading interval wins over subcommand keywords: no ambiguity.
  check('5m list todos is a create', (() => {
    const r = parseLoopArgs('5m list todos');
    return ok(r, 'create') && r.action.prompt === 'list todos';
  })());
  // Subcommand surface.
  check('list', (() => {
    const r = parseLoopArgs('list');
    return ok(r, 'list');
  })());
  check('cancel abc123', (() => {
    const r = parseLoopArgs('cancel abc123');
    return ok(r, 'cancel') && r.action.taskId === 'abc123';
  })());
  check('delete abc123', (() => {
    const r = parseLoopArgs('delete abc123');
    return ok(r, 'delete') && r.action.taskId === 'abc123';
  })());
  check('update abc123 10m check again', (() => {
    const r = parseLoopArgs('update abc123 10m check again');
    return ok(r, 'update') && r.action.taskId === 'abc123' && r.action.interval === '10m' && r.action.prompt === 'check again';
  })());
  check('update abc123 5m (interval only)', (() => {
    const r = parseLoopArgs('update abc123 5m');
    return ok(r, 'update') && r.action.taskId === 'abc123' && r.action.interval === '5m' && r.action.prompt === undefined;
  })());
  check('update abc123 prompt only', (() => {
    const r = parseLoopArgs('update abc123 prompt only');
    return ok(r, 'update') && r.action.taskId === 'abc123' && r.action.interval === undefined && r.action.prompt === 'prompt only';
  })());
  check('update abc123 -> usage error', (() => {
    const r = parseLoopArgs('update abc123');
    return !r.ok && r.message === 'usage: /loop update <id> [interval] [prompt]';
  })());
  // Subcommand usage errors surface the canonical /loop syntax (TUI parity).
  check('list extra -> error', !parseLoopArgs('list extra').ok);
  check('cancel bare -> error', !parseLoopArgs('cancel').ok);
  check('cancel extra -> error', !parseLoopArgs('cancel abc123 extra').ok);
  check('delete bare -> error', !parseLoopArgs('delete').ok);
  check('delete extra -> error', !parseLoopArgs('delete abc123 extra').ok);
  check('create bare -> error', !parseLoopArgs('create').ok);
  check('create interval only -> error', !parseLoopArgs('create 5m').ok);
  check('legacy prompt only -> error (interval required)', !parseLoopArgs('check deploy').ok);
  check('whitespace-only -> usage error', !parseLoopArgs('   ').ok);
}

/* ---------------- parseGoalArgs: /goal decision table ---------------- */
{
  check('bare /goal -> show (panel, no explicit)', (() => {
    const r = parseGoalArgs('');
    return ok(r, 'show') && r.action.explicit === undefined;
  })());
  check('show -> show (explicit RPC)', (() => {
    const r = parseGoalArgs('show');
    return ok(r, 'show') && r.action.explicit === true;
  })());
  check('get alias -> show (explicit)', (() => {
    const r = parseGoalArgs('get');
    return ok(r, 'show') && r.action.explicit === true;
  })());
  check('inspect alias -> show (explicit)', (() => {
    const r = parseGoalArgs('inspect');
    return ok(r, 'show') && r.action.explicit === true;
  })());
  check('whitespace bare -> panel show', (() => {
    const r = parseGoalArgs('   ');
    return ok(r, 'show') && r.action.explicit === undefined;
  })());
  check('show extra -> error', !parseGoalArgs('show extra').ok);
  check('inspect extra -> error', !parseGoalArgs('inspect extra').ok);
  // Lifecycle verbs take no arguments.
  for (const op of ['pause', 'resume', 'complete', 'drop']) {
    check(`${op} -> ${op}`, (() => {
      const r = parseGoalArgs(op);
      return ok(r, op);
    })());
    check(`${op} extra -> error`, !parseGoalArgs(`${op} extra`).ok);
  }
  check('pins -> pins', (() => {
    const r = parseGoalArgs('pins');
    return ok(r, 'pins');
  })());
  check('pins extra -> error', !parseGoalArgs('pins extra').ok);
  // Pin / unpin.
  check('pin keeps full text', (() => {
    const r = parseGoalArgs('pin keep the release checklist in scope');
    return ok(r, 'pin') && r.action.text === 'keep the release checklist in scope';
  })());
  check('pin bare -> error', !parseGoalArgs('pin').ok);
  check('pin whitespace -> error', !parseGoalArgs('pin   ').ok);
  check('unpin 0', (() => {
    const r = parseGoalArgs('unpin 0');
    return ok(r, 'unpin') && r.action.index === 0;
  })());
  check('unpin 7', (() => {
    const r = parseGoalArgs('unpin 7');
    return ok(r, 'unpin') && r.action.index === 7;
  })());
  check('unpin bare -> error', !parseGoalArgs('unpin').ok);
  check('unpin non-number -> error', !parseGoalArgs('unpin nope').ok);
  check('unpin extra -> error', !parseGoalArgs('unpin 0 extra').ok);
  // Create: --tokens flag + objective, and the bare create shorthand.
  check('create --tokens 42 ship safely', (() => {
    const r = parseGoalArgs('create --tokens 42 ship safely');
    return ok(r, 'create') && r.action.objective === 'ship safely' && r.action.tokenBudget === 42;
  })());
  check('set alias with --tokens', (() => {
    const r = parseGoalArgs('set --tokens 10 focus');
    return ok(r, 'create') && r.action.objective === 'focus' && r.action.tokenBudget === 10;
  })());
  check('create --tokens 0 -> error', !parseGoalArgs('create --tokens 0 no').ok);
  check('create --tokens abc -> error', !parseGoalArgs('create --tokens abc no').ok);
  check('create --tokens missing value -> error', !parseGoalArgs('create --tokens').ok);
  check('create --tokens 42 no objective -> error', !parseGoalArgs('create --tokens 42').ok);
  check('create no objective -> error', !parseGoalArgs('create').ok);
  check('bare objective shorthand', (() => {
    const r = parseGoalArgs('ship safely');
    return ok(r, 'create') && r.action.objective === 'ship safely' && r.action.tokenBudget === undefined;
  })());
  check('exact Chinese objective shorthand', (() => {
    const r = parseGoalArgs('制作zig版本的pi-coding-agent');
    return ok(r, 'create') && r.action.objective === '制作zig版本的pi-coding-agent';
  })());
  // --tokens only parses BEFORE the objective starts (TUI parity).
  check('--tokens after objective stays in objective', (() => {
    const r = parseGoalArgs('create ship safely --tokens 9');
    return ok(r, 'create') && r.action.objective === 'ship safely --tokens 9' && r.action.tokenBudget === undefined;
  })());
}

/* ---------------- wire mapping: loop + goal RPC shapes ---------------- */
{
  const createWire = loopWire({ op: 'create', interval: '5m', prompt: 'check deploy' });
  check('loop create wire', JSON.stringify(createWire) === JSON.stringify({
    type: 'loop_create', interval: '5m', prompt: 'check deploy', fireImmediately: true, durable: false,
  }), JSON.stringify(createWire));
  check('loop list wire', loopWire({ op: 'list' }).type === 'loop_list');
  check('loop update full wire', JSON.stringify(loopWire({ op: 'update', taskId: 'abc', interval: '10m', prompt: 'again' })) ===
    JSON.stringify({ type: 'loop_update', taskId: 'abc', interval: '10m', prompt: 'again' }));
  check('loop update prompt-only wire omits interval', JSON.stringify(loopWire({ op: 'update', taskId: 'abc', prompt: 'again' })) ===
    JSON.stringify({ type: 'loop_update', taskId: 'abc', prompt: 'again' }));
  check('loop delete wire', JSON.stringify(loopWire({ op: 'delete', taskId: 'abc' })) === JSON.stringify({ type: 'loop_delete', taskId: 'abc' }));
  check('loop cancel wire', JSON.stringify(loopWire({ op: 'cancel', taskId: 'abc' })) === JSON.stringify({ type: 'loop_cancel', taskId: 'abc' }));

  check('goal show wire -> goal_get', goalWire({ op: 'show' }).type === 'goal_get');
  check('goal pins wire -> goal_get', goalWire({ op: 'pins' }).type === 'goal_get');
  // Create/resume carry the TUI parity `activate: true` switch (the backend
  // then resolves with the activation outcome, like /goal create|resume).
  check('goal create wire activates', JSON.stringify(goalWire({ op: 'create', objective: 'ship', tokenBudget: 42 })) ===
    JSON.stringify({ type: 'goal_create', objective: 'ship', tokenBudget: 42, activate: true }));
  check('goal create wire omits budget, keeps activate', JSON.stringify(goalWire({ op: 'create', objective: 'ship' })) ===
    JSON.stringify({ type: 'goal_create', objective: 'ship', activate: true }));
  check('goal pause wire', goalWire({ op: 'pause' }).type === 'goal_pause');
  check('goal resume wire activates', JSON.stringify(goalWire({ op: 'resume' })) === JSON.stringify({ type: 'goal_resume', activate: true }));
  check('goal complete wire', goalWire({ op: 'complete' }).type === 'goal_complete');
  check('goal drop wire', goalWire({ op: 'drop' }).type === 'goal_drop');
  check('goal pin wire', JSON.stringify(goalWire({ op: 'pin', text: 'stay calm' })) === JSON.stringify({ type: 'goal_pin', text: 'stay calm' }));
  check('goal unpin wire', JSON.stringify(goalWire({ op: 'unpin', index: 0 })) === JSON.stringify({ type: 'goal_unpin', index: 0 }));

  // TUI parity guard: only create/update need an idle session.
  check('loop create requires idle', loopRequiresIdle({ op: 'create', interval: '5m', prompt: 'p' }) === true);
  check('loop update requires idle', loopRequiresIdle({ op: 'update', taskId: 'a', prompt: 'p' }) === true);
  check('loop list does not require idle', loopRequiresIdle({ op: 'list' }) === false);
  check('loop delete does not require idle', loopRequiresIdle({ op: 'delete', taskId: 'a' }) === false);
  check('loop cancel does not require idle', loopRequiresIdle({ op: 'cancel', taskId: 'a' }) === false);

  // TUI loop aliases are intercepted with actionable errors — never prompts.
  check('alias /loops -> list hint', unsupportedAliasMessage('/loops') === 'alias of /loop: use /loop list');
  check('alias /loop-update -> update hint', unsupportedAliasMessage('/loop-update abc 5m x') === 'alias of /loop: use /loop update <id> [interval] [prompt]');
  check('alias /loop-delete -> delete hint', unsupportedAliasMessage('/loop-delete abc') === 'alias of /loop: use /loop delete <id>');
  check('alias /loop-cancel -> cancel hint', unsupportedAliasMessage('/loop-cancel abc') === 'alias of /loop: use /loop cancel <id>');
  check('alias case-sensitive', unsupportedAliasMessage('/LOOPS') === null);
  check('alias detection ignores leading space', unsupportedAliasMessage('  /loops') === 'alias of /loop: use /loop list');
  check('plain text is not an alias', unsupportedAliasMessage('hello /loops') === null);
  check('bare slash is not an alias', unsupportedAliasMessage('/') === null);
  check('empty text is not an alias', unsupportedAliasMessage('') === null);
  check('supported command is not an alias', unsupportedAliasMessage('/loop list') === null);
  check('unsupported non-alias is not an alias', unsupportedAliasMessage('/workflow') === null);
}

/* ---------------- loop formatters: bounded, defensive ---------------- */
{
  check('interval 3 -> every 3 seconds', loopIntervalToHuman(3) === 'every 3 seconds');
  check('interval 60 -> every 1 minute', loopIntervalToHuman(60) === 'every 1 minute');
  check('interval 120 -> every 2 minutes', loopIntervalToHuman(120) === 'every 2 minutes');
  check('interval 3600 -> every 1 hour', loopIntervalToHuman(3600) === 'every 1 hour');
  check('interval 86400 -> every 1 day', loopIntervalToHuman(86400) === 'every 1 day');
  check('interval 172800 -> every 2 days', loopIntervalToHuman(172800) === 'every 2 days');
  check('interval missing -> every ?', loopIntervalToHuman(undefined) === 'every ?');

  const task = {
    id: 'abc123',
    intervalSecs: 300,
    prompt: 'check deploy',
    createdAt: '2026-01-01T00:00:00.000Z',
    lastFiredAt: null,
    expiresAt: '2026-01-08T00:00:00.000Z',
    runCount: 0,
  };
  check('task row mirrors TUI list line', formatLoopTaskRow(task) ===
    'abc123  every 5 minutes  next 2026-01-01T00:05:00.000Z  check deploy');
  check('task row uses lastFiredAt as base', formatLoopTaskRow({ ...task, lastFiredAt: '2026-01-01T00:10:00.000Z' }) ===
    'abc123  every 5 minutes  next 2026-01-01T00:15:00.000Z  check deploy');
  check('task row drops next when base missing', (() => {
    const row = formatLoopTaskRow({ id: 'x', intervalSecs: 60, prompt: 'p' });
    return row === 'x  every 1 minute  p';
  })());
  check('malformed task never throws', typeof formatLoopTaskRow(null) === 'string' && typeof formatLoopTaskRow('junk') === 'string');
  check('empty list -> no active loops', formatLoopList([]) === 'no active loops');
  check('non-array list -> no active loops', formatLoopList(null) === 'no active loops');
  check('list joins rows', formatLoopList([task, { ...task, id: 'def456' }]).split('\n').length === 2);
  // Bounded: 25 tasks render 20 rows + an overflow marker.
  const many = Array.from({ length: 25 }, (_, i) => ({ ...task, id: `task-${i}` }));
  const manyText = formatLoopList(many);
  check('list bounded to 20 rows', manyText.split('\n').length === 21, manyText);
  check('list overflow marker', manyText.endsWith('… and 5 more'));
  // Bounded: long prompts are truncated.
  const longPrompt = 'x'.repeat(200);
  check('task row truncates prompt', formatLoopTaskRow({ ...task, prompt: longPrompt }).endsWith('…'));
  check('task row keeps short prompts whole', formatLoopTaskRow({ ...task, prompt: 'hi' }).endsWith('hi'));
}

/* ---------------- loop result formatting (success + actionable errors) ---------------- */
{
  const task = {
    id: 'abc123',
    intervalSecs: 300,
    prompt: 'check deploy',
    createdAt: '2026-01-01T00:00:00.000Z',
    lastFiredAt: null,
    expiresAt: '2026-01-08T00:00:00.000Z',
  };
  const created = formatLoopResult({ op: 'create', interval: '5m', prompt: 'check deploy' }, task);
  check('create bubble', created.ok && created.text === 'scheduled abc123 · every 5 minutes · expires 2026-01-08T00:00:00.000Z', created.text);
  const updated = formatLoopResult({ op: 'update', taskId: 'abc123', interval: '10m', prompt: 'again' }, { ...task, intervalSecs: 600 });
  check('update bubble', updated.ok && updated.text === 'updated loop abc123 · every 10 minutes · next 2026-01-01T00:10:00.000Z · check deploy', updated.text);
  const listed = formatLoopResult({ op: 'list' }, [task]);
  check('list bubble', listed.ok && listed.text.startsWith('abc123  every 5 minutes'));
  check('delete true -> deleted', (() => {
    const r = formatLoopResult({ op: 'delete', taskId: 'abc123' }, true);
    return r.ok && r.text === 'deleted loop abc123';
  })());
  check('cancel true -> cancelled', (() => {
    const r = formatLoopResult({ op: 'cancel', taskId: 'abc123' }, true);
    return r.ok && r.text === 'cancelled loop abc123';
  })());
  check('delete false -> actionable error', (() => {
    const r = formatLoopResult({ op: 'delete', taskId: 'abc123' }, false);
    return !r.ok && r.message === 'no active loop with id abc123';
  })());
  check('cancel false -> actionable error', (() => {
    const r = formatLoopResult({ op: 'cancel', taskId: 'abc123' }, false);
    return !r.ok && r.message === 'no active loop with id abc123';
  })());
}

/* ---------------- goal formatters: state + pins, bounded ---------------- */
{
  const goal = {
    id: 'goal-1',
    objective: 'ship safely',
    tokenBudget: 42,
    pins: [],
    lifecycle: 'active',
    createdAt: '2026-01-01T00:00:00.000Z',
    updatedAt: '2026-01-01T00:00:00.000Z',
    usage: { tokensUsed: 0, activeTimeSeconds: 0 },
  };
  check('active goal state line', formatGoalFromWire(goal) === 'active · 0/42 tokens · ship safely');
  check('no-budget goal state line', formatGoalFromWire({ ...goal, tokenBudget: null }) === 'active · 0 tokens used · ship safely');
  check('paused manual state line', formatGoalFromWire({ ...goal, lifecycle: 'paused', pauseReason: 'manual' }) ===
    'paused (manually paused) · 0/42 tokens · ship safely');
  check('paused budget-exhausted state line', formatGoalFromWire({ ...goal, lifecycle: 'paused', pauseReason: 'budget_exhausted' }) ===
    'paused (budget exhausted; cannot resume) · 0/42 tokens · ship safely');
  check('paused resume-safety state line', formatGoalFromWire({ ...goal, lifecycle: 'paused', pauseReason: 'resume_safety' }) ===
    'paused (session resumed; run /goal resume) · 0/42 tokens · ship safely');
  check('completed state line', formatGoalFromWire({ ...goal, lifecycle: 'completed' }) === 'completed · 0/42 tokens · ship safely');
  check('usage counted', formatGoalFromWire({ ...goal, usage: { tokensUsed: 12, activeTimeSeconds: 5 } }) === 'active · 12/42 tokens · ship safely');
  check('malformed goal never throws', typeof formatGoalFromWire(null) === 'string' && typeof formatGoalFromWire('junk') === 'string');

  check('goal_get no current -> no goal', formatGoalState({ revision: 0 }) === 'no goal');
  check('goal_get null current -> no goal', formatGoalState({ current: null, revision: 0 }) === 'no goal');
  check('goal_get non-object -> no goal', formatGoalState(null) === 'no goal');
  check('goal_get with current -> state line', formatGoalState({ current: goal, revision: 1 }) === 'active · 0/42 tokens · ship safely');

  check('pins listing', formatGoalPins({ current: { ...goal, pins: ['follow the checklist', 'reference the omp style'] } }) ===
    '1. follow the checklist\n2. reference the omp style');
  check('pins empty -> no pins', formatGoalPins({ current: { ...goal, pins: [] } }) === 'no pins');
  check('pins no goal -> no goal', formatGoalPins({ revision: 0 }) === 'no goal');
  check('pins non-array -> no pins', formatGoalPins({ current: { ...goal } }) === 'no pins');
  // Bounded: more pins than the backend cap render an overflow marker.
  const overflowPins = Array.from({ length: 10 }, (_, i) => `pin ${i}`);
  const pinsText = formatGoalPins({ current: { ...goal, pins: overflowPins } });
  check('pins bounded to 8 rows', pinsText.split('\n').length === 9, pinsText);
  check('pins overflow marker', pinsText.endsWith('… and 2 more'));
  check('pin text truncated defensively', formatGoalPins({ current: { ...goal, pins: ['y'.repeat(300)] } }).endsWith('…'));
}

/* ---------------- goal result formatting ---------------- */
{
  const goal = {
    id: 'goal-1',
    objective: 'ship safely',
    tokenBudget: 42,
    pins: ['stay calm'],
    lifecycle: 'active',
    createdAt: '2026-01-01T00:00:00.000Z',
    updatedAt: '2026-01-01T00:00:00.000Z',
    usage: { tokensUsed: 0, activeTimeSeconds: 0 },
  };
  check('show result from goal_get state', (() => {
    const r = formatGoalResult({ op: 'show' }, { current: goal, revision: 3 });
    return r.ok && r.text === 'active · 0/42 tokens · ship safely';
  })());
  check('pins result from goal_get state', (() => {
    const r = formatGoalResult({ op: 'pins' }, { current: goal, revision: 3 });
    return r.ok && r.text === '1. stay calm';
  })());
  check('create result from Goal wire', (() => {
    const r = formatGoalResult({ op: 'create', objective: 'ship safely', tokenBudget: 42 }, goal);
    return r.ok && r.text === 'active · 0/42 tokens · ship safely';
  })());
  // activate:true create/resume resolve with the activation outcome; the
  // summary path chains goal_get and passes the GoalState as stateData.
  check('activation prefix mapping', goalActivationPrefix('started') === 'Goal work started'
    && goalActivationPrefix('queued') === 'Goal work queued'
    && goalActivationPrefix('already_active') === 'Goal work already active'
    && goalActivationPrefix(undefined) === ''
    && goalActivationPrefix('bogus') === '');
  check('create started outcome + state', (() => {
    const r = formatGoalResult({ op: 'create', objective: 'ship safely', tokenBudget: 42 }, 'started', { current: goal, revision: 1 });
    return r.ok && r.text === 'Goal work started · active · 0/42 tokens · ship safely';
  })());
  check('create queued outcome + state', (() => {
    const r = formatGoalResult({ op: 'create', objective: 'ship safely', tokenBudget: 42 }, 'queued', { current: goal, revision: 1 });
    return r.ok && r.text === 'Goal work queued · active · 0/42 tokens · ship safely';
  })());
  check('resume already-active outcome + state', (() => {
    const r = formatGoalResult({ op: 'resume' }, 'already_active', { current: goal, revision: 2 });
    return r.ok && r.text === 'Goal work already active · active · 0/42 tokens · ship safely';
  })());
  check('create mutation shape ignores missing state', (() => {
    const r = formatGoalResult({ op: 'create', objective: 'ship safely' }, goal);
    return r.ok && r.text === 'active · 0/42 tokens · ship safely';
  })());
  check('pin mutation result from Goal wire', (() => {
    const r = formatGoalResult({ op: 'pin', text: 'stay calm' }, goal);
    return r.ok && r.text === 'active · 0/42 tokens · ship safely';
  })());
  check('pause result from Goal wire', (() => {
    const r = formatGoalResult({ op: 'pause' }, { ...goal, lifecycle: 'paused', pauseReason: 'manual' });
    return r.ok && r.text === 'paused (manually paused) · 0/42 tokens · ship safely';
  })());
  check('drop result -> dropped state line', (() => {
    const r = formatGoalResult({ op: 'drop' }, { id: 'g', objective: 'ship safely', lifecycle: 'dropped', usage: { tokensUsed: 0, activeTimeSeconds: 0 } });
    return r.ok && r.text === 'dropped · 0 tokens used · ship safely';
  })());
}

console.log(`\nloopGoal.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
