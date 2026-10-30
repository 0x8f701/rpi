#!/usr/bin/env node
// Focused regression for src/slashDispatch.ts + parseSupportedCommand —
// the submit-path decision table for Web-supported slash commands, including
// the sole public `/todo` root and its existing typed RPC mappings.
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
  todoWire,
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
  check('commit bare parses as a supported command', (() => {
    const p = parseSupportedCommand('/commit');
    return p && p.name === 'commit' && p.args === '';
  })());
  check('commit ref tail stays intact for dispatch', (() => {
    const p = parseSupportedCommand('/commit HEAD~1');
    return p && p.name === 'commit' && p.args === 'HEAD~1';
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
  check('todo bare parses as the sole root', (() => {
    const p = parseSupportedCommand('/todo');
    return p && p.name === 'todo' && p.args === '';
  })());
  check('todo verb tail stays intact for dispatch', (() => {
    const p = parseSupportedCommand('/todo add child --role writer work');
    return p && p.name === 'todo' && p.args === 'add child --role writer work';
  })());
  check('todo-child is unsupported and follows normal unknown behavior', parseSupportedCommand('/todo-child') === null);
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
    crBare.type === 'code-review' && !crBare.from && !crBare.to && !crBare.includeWorkingTree,
  );

  const crTwo = resolveSlashAction('code-review', 'abc def');
  check(
    'code-review two revs',
    crTwo.type === 'code-review' && crTwo.from === 'abc' && crTwo.to === 'def' && !crTwo.includeWorkingTree,
  );

  const crOne = resolveSlashAction('code-review', 'only');
  check(
    'code-review one rev -> base-to-working-tree',
    crOne.type === 'code-review' && crOne.from === 'only' && !crOne.to && crOne.includeWorkingTree === true,
  );

  const crThree = resolveSlashAction('code-review', 'a b c');
  check('code-review three revs -> error', crThree.type === 'error' && /usage: \/code-review/.test(crThree.message));

  const commitBare = resolveSlashAction('commit', '');
  check(
    'commit bare -> chooser action without a ref',
    commitBare.type === 'commit' && commitBare.ref === undefined,
  );
  const commitRef = resolveSlashAction('commit', 'abc123def');
  check(
    'commit single ref -> direct pinned scope',
    commitRef.type === 'commit' && commitRef.ref === 'abc123def',
  );
  const commitSpaced = resolveSlashAction('commit', '  HEAD~1  ');
  check('commit ref is a trimmed single token', commitSpaced.type === 'commit' && commitSpaced.ref === 'HEAD~1');
  const commitExtra = resolveSlashAction('commit', 'a b');
  check(
    'commit extra args -> TUI-parity usage error',
    commitExtra.type === 'error' && commitExtra.message === 'Usage: /commit [<ref>]',
  );

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
  const jobsBare = resolveSlashAction('jobs', '');
  check('bare /jobs -> typed jobs action', jobsBare.type === 'jobs');
  const jobsArgs = resolveSlashAction('jobs', 'extra');
  check('/jobs extra -> usage error', jobsArgs.type === 'error' && jobsArgs.message === 'usage: /jobs');
  const ircBare = resolveSlashAction('irc', '');
  check('bare /irc -> typed irc action', ircBare.type === 'irc');
  const ircArgs = resolveSlashAction('irc', 'extra');
  check('/irc extra -> usage error', ircArgs.type === 'error' && ircArgs.message === 'usage: /irc');
  const todoBare = resolveSlashAction('todo', '');
  check('bare /todo -> unified panel action', todoBare.type === 'todo' && todoBare.action.op === 'open');
  const todoList = resolveSlashAction('todo', 'list');
  check('/todo list -> unified panel action', todoList.type === 'todo' && todoList.action.op === 'open');
  const todoListArgs = resolveSlashAction('todo', 'list extra');
  check('/todo list rejects trailing arguments', todoListArgs.type === 'error' && todoListArgs.message === 'usage: /todo list');
  const todoListUpper = resolveSlashAction('todo', 'LIST');
  check('/todo verbs are case-insensitive', todoListUpper.type === 'todo' && todoListUpper.action.op === 'open');
  const todoNew = resolveSlashAction('todo', 'new deploy child');
  check('/todo new label -> child create action', todoNew.type === 'todo' && todoNew.action.op === 'new' && todoNew.action.label === 'deploy child');
  const todoNewBare = resolveSlashAction('todo', 'new');
  check('/todo new omits an empty label', todoNewBare.type === 'todo' && todoNewBare.action.op === 'new' && !('label' in todoNewBare.action));
  const todoSelectOne = resolveSlashAction('todo', 'select child-1');
  check('/todo select accepts exactly one identifier', todoSelectOne.type === 'todo' && todoSelectOne.action.op === 'select' && todoSelectOne.action.childSessionId === 'child-1');
  const todoSelectMissing = resolveSlashAction('todo', 'select');
  check('/todo select requires an identifier', todoSelectMissing.type === 'error' && todoSelectMissing.message === 'usage: /todo select <id>');
  const todoSelect = resolveSlashAction('todo', 'select child-1 extra');
  check('/todo select rejects trailing identifiers', todoSelect.type === 'error' && todoSelect.message === 'usage: /todo select <id>');
  const childId = '00000000-0000-4000-8000-000000000001';
  const todoSendSelected = resolveSlashAction('todo', 'send continue now');
  check('/todo send message -> selected child action', todoSendSelected.type === 'todo' && todoSendSelected.action.op === 'send' && !todoSendSelected.action.childSessionId && todoSendSelected.action.message === 'continue now');
  const todoSendId = resolveSlashAction('todo', `send ${childId} continue now`);
  check('/todo send id message -> explicit child action', todoSendId.type === 'todo' && todoSendId.action.op === 'send' && todoSendId.action.childSessionId === childId && todoSendId.action.message === 'continue now');
  const todoAdd = resolveSlashAction('todo', 'add child-1 --revision 7 --depends a,b --agent worker --role writer implement feature');
  check('/todo add parses existing options', todoAdd.type === 'todo' && todoAdd.action.op === 'add' && todoAdd.action.childSessionId === 'child-1' && todoAdd.action.expectedRevision === 7 && JSON.stringify(todoAdd.action.dependsOn) === JSON.stringify(['a', 'b']) && todoAdd.action.agent === 'worker' && todoAdd.action.role === 'writer' && todoAdd.action.content === 'implement feature');
  const todoAddRevision = resolveSlashAction('todo', 'add child-1 --revision nope work');
  check('/todo add rejects non-integer revisions', todoAddRevision.type === 'error' && todoAddRevision.message === 'revision must be an integer');
  for (const operation of ['status', 'pause', 'resume', 'stop', 'cancel']) {
    const bare = resolveSlashAction('todo', operation);
    check(`/todo ${operation} accepts selected child`, bare.type === 'todo' && bare.action.op === operation && !bare.action.childSessionId);
    const one = resolveSlashAction('todo', `${operation} child-1`);
    check(`/todo ${operation} accepts one identifier`, one.type === 'todo' && one.action.op === operation && one.action.childSessionId === 'child-1');
    const extra = resolveSlashAction('todo', `${operation} child-1 extra`);
    check(`/todo ${operation} rejects trailing identifiers`, extra.type === 'error' && extra.message === `usage: /todo ${operation} [id]`);
  }
  const todoArchiveOne = resolveSlashAction('todo', 'archive child-1');
  check('/todo archive accepts exactly one identifier', todoArchiveOne.type === 'todo' && todoArchiveOne.action.op === 'archive' && todoArchiveOne.action.childSessionId === 'child-1');
  const todoArchiveMissing = resolveSlashAction('todo', 'archive');
  check('/todo archive requires an identifier', todoArchiveMissing.type === 'error' && todoArchiveMissing.message === 'usage: /todo archive <id>');
  const todoArchiveArgs = resolveSlashAction('todo', 'archive child-1 extra');
  check('/todo archive rejects trailing identifiers', todoArchiveArgs.type === 'error' && todoArchiveArgs.message === 'usage: /todo archive <id>');
  const todoStart = resolveSlashAction('todo', 'START exact task text');
  check('/todo DAG verbs are case-insensitive', todoStart.type === 'todo' && todoStart.action.op === 'start' && todoStart.action.task === 'exact task text');
  for (const [operation, usage] of [['start', 'usage: /todo start <task>'], ['done', 'usage: /todo done <task>'], ['drop', 'usage: /todo drop <task>']]) {
    const missing = resolveSlashAction('todo', operation);
    check(`/todo ${operation} requires a task`, missing.type === 'error' && missing.message === usage);
  }
  const todoClear = resolveSlashAction('todo', 'clear');
  check('/todo clear -> DAG clear action', todoClear.type === 'todo' && todoClear.action.op === 'clear');
  const todoClearArgs = resolveSlashAction('todo', 'clear extra');
  check('/todo clear rejects trailing arguments', todoClearArgs.type === 'error' && todoClearArgs.message === 'usage: /todo clear');
  const todoMarkdown = resolveSlashAction('todo', '# Phase\n- [ ] work');
  check('/todo markdown rejects actionably instead of prompting', todoMarkdown.type === 'error' && todoMarkdown.message.includes('unavailable in Web'));
  const todoUnknown = resolveSlashAction('todo', 'nonesuch');
  check('/todo unknown -> usage error', todoUnknown.type === 'error' && /usage: \/todo/.test(todoUnknown.message));
  check('/todo new maps to frozen create RPC', JSON.stringify(todoWire({ op: 'new', label: 'deploy' }, 'parent')) === JSON.stringify({ label: 'deploy', type: 'todo_child_create', parentSessionId: 'parent' }));
  check('/todo bare new omits label on wire', JSON.stringify(todoWire({ op: 'new' }, 'parent')) === JSON.stringify({ type: 'todo_child_create', parentSessionId: 'parent' }));
  check('/todo select maps to frozen select RPC', JSON.stringify(todoWire({ op: 'select', childSessionId: 'child-1' }, 'parent')) === JSON.stringify({ childSessionId: 'child-1', type: 'todo_child_select', parentSessionId: 'parent' }));
  check('/todo send maps to frozen send RPC', JSON.stringify(todoWire({ op: 'send', childSessionId: childId, message: 'go' }, 'parent')) === JSON.stringify({ childSessionId: childId, message: 'go', type: 'todo_child_send', parentSessionId: 'parent' }));
  check('/todo selected-child send omits child id on wire', JSON.stringify(todoWire({ op: 'send', message: 'go' }, 'parent')) === JSON.stringify({ message: 'go', type: 'todo_child_send', parentSessionId: 'parent' }));
  check('/todo add maps every supported option', JSON.stringify(todoWire({ op: 'add', childSessionId: 'child-1', content: 'work', dependsOn: ['a'], agent: 'worker', role: 'writer', expectedRevision: 7 }, 'parent')) === JSON.stringify({ childSessionId: 'child-1', content: 'work', dependsOn: ['a'], agent: 'worker', role: 'writer', expectedRevision: 7, type: 'todo_child_add_task', parentSessionId: 'parent' }));
  for (const operation of ['status', 'pause', 'resume', 'stop', 'cancel']) {
    check(`/todo ${operation} maps to frozen lifecycle RPC`, JSON.stringify(todoWire({ op: operation, childSessionId: 'child-1' }, 'parent')) === JSON.stringify({ childSessionId: 'child-1', type: `todo_child_${operation}`, parentSessionId: 'parent' }));
  }
  check('/todo selected-child status omits child id on wire', JSON.stringify(todoWire({ op: 'status' }, 'parent')) === JSON.stringify({ type: 'todo_child_status', parentSessionId: 'parent' }));
  check('/todo archive maps to frozen archive RPC', JSON.stringify(todoWire({ op: 'archive', childSessionId: 'child-1' }, 'parent')) === JSON.stringify({ childSessionId: 'child-1', type: 'todo_child_archive', parentSessionId: 'parent' }));
  check('/todo remove maps to remove RPC', JSON.stringify(todoWire({ op: 'remove', childSessionId: 'child-1' }, 'parent')) === JSON.stringify({ childSessionId: 'child-1', type: 'todo_child_remove', parentSessionId: 'parent' }));
  const todoRemove = resolveSlashAction('todo', 'remove child-1');
  check('/todo remove parses exact child id', todoRemove.type === 'todo' && todoRemove.action.op === 'remove' && todoRemove.action.childSessionId === 'child-1');
  const todoRemoveMissing = resolveSlashAction('todo', 'remove');
  check('/todo remove requires an exact child id', todoRemoveMissing.type === 'error' && todoRemoveMissing.message === 'usage: /todo remove <id>');
  const todoRemoveExtra = resolveSlashAction('todo', 'remove child-1 extra');
  check('/todo remove rejects trailing arguments', todoRemoveExtra.type === 'error' && todoRemoveExtra.message === 'usage: /todo remove <id>');
  check('/todo start maps to todo_op', JSON.stringify(todoWire({ op: 'start', task: 'work' }, 'parent')) === JSON.stringify({ type: 'todo_op', op: 'start', task: 'work' }));
  check('/todo done maps to todo_op', JSON.stringify(todoWire({ op: 'done', task: 'work' }, 'parent')) === JSON.stringify({ type: 'todo_op', op: 'done', task: 'work' }));
  check('/todo drop maps to todo_op', JSON.stringify(todoWire({ op: 'drop', task: 'work' }, 'parent')) === JSON.stringify({ type: 'todo_op', op: 'drop', task: 'work' }));
  check('/todo clear maps to cascade removal', JSON.stringify(todoWire({ op: 'clear' }, 'parent')) === JSON.stringify({ type: 'todo_op', op: 'rm', cascade: true }));
  check('bare /todo has no RPC wire', todoWire({ op: 'open' }, 'parent') === null);
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
