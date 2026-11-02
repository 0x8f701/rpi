#!/usr/bin/env node
// Chat goal panel regression for src/chat/ChatGoalPanel.tsx:
// lifecycle gating, goal_* wire shapes, strict goal_get parsing
// (parseChatGoalState), bounded journal/objective helpers, and the local
// single-owner contract: the panel issues its own bare goal_get +
// goal_journal and takes no controlled state/refreshGoal props.
// Run through `npm run build`.
//
// Exit codes: 0 = every assertion held; 1 = a regression.
import { createElement } from 'react';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import {
  ChatGoalPanel,
  goalActionsFor,
  goalCreateCommand,
  goalCommandFor,
  chatGoalGetCommand,
  chatGoalJournalCommand,
  boundJournal,
  truncateText,
  goalFrameRefreshesJournal,
  completeGoalMutation,
  fetchChatGoalState,
  fetchChatGoalJournal,
  fetchCurrentChatGoalJournal,
  MAX_JOURNAL_ROWS,
  MAX_OBJECTIVE_CHARS,
  MAX_JOURNAL_OBJECTIVE_CHARS,
  type GoalAction,
} from '../src/chat/ChatGoalPanel.tsx';
// parseChatGoalState moved to its single home in src/chat/team.ts.
import { parseChatGoalState } from '../src/chat/team.ts';
import type { GoalEventWire, GoalStateWire } from '../src/panels/GoalPanel.tsx';
import type { RunnerCommand } from '../src/chat/protocol.ts';

const failures: string[] = [];
let assertions = 0;

function check(name: string, condition: boolean, detail = ''): void {
  assertions += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

function sameActions(actual: GoalAction[], expected: GoalAction[]): boolean {
  return actual.length === expected.length && expected.every((action, index) => actual[index] === action);
}

// ---- status → button gating matrix ----
{
  check('active exposes pause/complete/drop', sameActions(goalActionsFor('active'), ['pause', 'complete', 'drop']));
  check('paused exposes resume/complete/drop', sameActions(goalActionsFor('paused'), ['resume', 'complete', 'drop']));
  check('completed exposes no lifecycle actions', sameActions(goalActionsFor('completed'), []));
  check('dropped exposes no lifecycle actions', sameActions(goalActionsFor('dropped'), []));
  check('no goal exposes no lifecycle actions', sameActions(goalActionsFor(null), []));
}

// ---- command wire shapes per contract (slashDispatch.ts:647-658) ----
{
  const create = goalCreateCommand('ship the team console');
  check('goal_create type', create.type === 'goal_create');
  check('goal_create objective', create.objective === 'ship the team console');
  check('goal_create omits tokenBudget when absent', !('tokenBudget' in create));
  const withBudget = goalCreateCommand('ship', 5000);
  check('goal_create carries tokenBudget', withBudget.tokenBudget === 5000);
  check('goal_create is mutation-only (no activate)', !('activate' in create) && !('activate' in withBudget));

  check('goal_pause shape', JSON.stringify(goalCommandFor('pause')) === '{"type":"goal_pause"}');
  check('goal_resume shape', JSON.stringify(goalCommandFor('resume')) === '{"type":"goal_resume"}');
  check('goal_complete shape', JSON.stringify(goalCommandFor('complete')) === '{"type":"goal_complete"}');
  check('goal_drop shape', JSON.stringify(goalCommandFor('drop')) === '{"type":"goal_drop"}');

  check('goal_get shape', JSON.stringify(chatGoalGetCommand()) === '{"type":"goal_get"}');
  check('goal_journal shape', JSON.stringify(chatGoalJournalCommand()) === '{"type":"goal_journal"}');
}

// ---- bounded journal rendering ----
{
  const entry = (revision: number): GoalEventWire => ({
    revision,
    timestamp: '2026-08-29T00:00:00Z',
    kind: { type: 'created' },
    goal: {
      id: `g${revision}`,
      objective: `objective ${revision}`,
      pins: [],
      lifecycle: 'active',
      createdAt: '2026-08-29T00:00:00Z',
      updatedAt: '2026-08-29T00:00:00Z',
      usage: { tokensUsed: 0, activeTimeSeconds: 0 },
    },
  });

  const under = boundJournal([entry(1), entry(2)], MAX_JOURNAL_ROWS);
  check('journal under bound passes through', under.length === 2 && under[0].revision === 1 && under[1].revision === 2);

  const many = Array.from({ length: MAX_JOURNAL_ROWS + 25 }, (_, index) => entry(index + 1));
  const bounded = boundJournal(many, MAX_JOURNAL_ROWS);
  check('journal over bound is capped', bounded.length === MAX_JOURNAL_ROWS);
  check('journal keeps the LATEST entries (tail)', bounded[0].revision === 26 && bounded[bounded.length - 1].revision === 75);

  check('empty journal stays empty', boundJournal([], MAX_JOURNAL_ROWS).length === 0);
}

// ---- long-text truncation ----
{
  const short = 'short objective';
  check('short text passes through', truncateText(short, MAX_OBJECTIVE_CHARS) === short);
  const long = 'x'.repeat(MAX_OBJECTIVE_CHARS + 40);
  const truncated = truncateText(long, MAX_OBJECTIVE_CHARS);
  check('long text is truncated to bound', truncated.length === MAX_OBJECTIVE_CHARS);
  check('truncated text ends with ellipsis', truncated.endsWith('\u2026'));
  check('journal objective bound is tighter than detail bound', MAX_JOURNAL_OBJECTIVE_CHARS < MAX_OBJECTIVE_CHARS);
}

// ---- strict goal_get parser (panel-local snapshot validation) ----
{
  const validState: GoalStateWire = {
    revision: 4,
    current: {
      id: 'goal-ship',
      objective: 'ship the team console',
      pins: [],
      lifecycle: 'active',
      createdAt: '2026-08-29T00:00:00Z',
      updatedAt: '2026-08-29T00:00:00Z',
      usage: { tokensUsed: 12, activeTimeSeconds: 3 },
    },
  };
  const parsed = parseChatGoalState(validState);
  check(
    'parser accepts a valid goal_get snapshot',
    parsed !== null && parsed.revision === 4 && parsed.current?.objective === 'ship the team console',
    JSON.stringify(parsed),
  );
  const cleared = parseChatGoalState({ revision: 5, current: null });
  check('parser allows current null (no active goal)', cleared !== null && cleared.revision === 5 && cleared.current === null);
  check('parser rejects null payloads', parseChatGoalState(null) === null);
  check('parser rejects primitive payloads', parseChatGoalState(42) === null);
  check('parser rejects array payloads', parseChatGoalState([validState]) === null);
  check('parser rejects non-numeric revision', parseChatGoalState({ revision: '4', current: validState.current }) === null);
  check('parser rejects missing revision', parseChatGoalState({ current: validState.current }) === null);
  const omitted = parseChatGoalState({ revision: 4 });
  check(
    'parser accepts omitted current as null (legitimate short wire)',
    omitted !== null && omitted.revision === 4 && omitted.current === null,
    JSON.stringify(omitted),
  );
  check('parser rejects non-object current', parseChatGoalState({ revision: 4, current: 'goal-ship' }) === null);
  check('parser rejects array current', parseChatGoalState({ revision: 4, current: [validState.current] }) === null);
}

// ---- local goal_get/journal ownership; no initial goal ----
{
  const commands: RunnerCommand[] = [];
  const html = renderToStaticMarkup(createElement(ChatGoalPanel, {
    sendCommand: async (command) => {
      commands.push(command);
      return [];
    },
    events: () => () => {},
  }));
  check(
    'local panel starts with no goal and offers creation',
    html.includes('data-lifecycle="none"') && html.includes('Create goal'),
    html,
  );
  check(
    'empty panel labels the field Goal with a distinct placeholder',
    html.includes('goal-create__objective">Goal')
      && html.includes('placeholder="What should this agent accomplish?"')
      && !html.includes('Objective')
      && !html.includes('Objective — why future turns may continue'),
    html,
  );
  check(
    'empty panel token budget label is not duplicated as the placeholder',
    html.includes('Token budget')
      && html.includes('placeholder="optional"')
      && !html.includes('Token budget (optional)')
      && !html.includes('placeholder="Token budget"'),
    html,
  );
  check(
    'empty panel omits Journal entirely',
    !html.includes('Journal') && !html.includes('goal-journal'),
    html,
  );

  const goalGet = chatGoalGetCommand();
  const goalJournal = chatGoalJournalCommand();
  check(
    'panel-owned goal_get is bare (sessionId stamped by runners.ts)',
    JSON.stringify(goalGet) === '{"type":"goal_get"}' && !('sessionId' in goalGet),
  );
  check(
    'panel-owned goal_journal is bare (sessionId stamped by runners.ts)',
    JSON.stringify(goalJournal) === '{"type":"goal_journal"}' && !('sessionId' in goalJournal),
  );
}

{
  check('goal_updated refreshes selected journal', goalFrameRefreshesJournal({ type: 'goal_updated' }));
  check('goal_updated refreshes even pre-bootstrap', goalFrameRefreshesJournal({ type: 'goal_updated' }, false));
  check('goal_usage_charged refreshes goal+journal like goal_updated', goalFrameRefreshesJournal({ type: 'goal_usage_charged' }));
  check('goal_usage_charged refreshes even pre-bootstrap', goalFrameRefreshesJournal({ type: 'goal_usage_charged' }, false));
  check('unowned frame does not refresh journal', !goalFrameRefreshesJournal({ type: 'job_updated' }));
}

// ---- authoritative subscription cuts refresh; bootstrap cuts do not ----
{
  check('authoritative session_resync refreshes goal+journal', goalFrameRefreshesJournal({ type: 'session_resync', authoritative: true }));
  check('authoritative subscription_snapshot refreshes goal+journal', goalFrameRefreshesJournal({ type: 'subscription_snapshot', authoritative: true, messages: [] }));
  check('non-authoritative reconnect snapshot does not refresh', !goalFrameRefreshesJournal({ type: 'subscription_snapshot', authoritative: false, messages: [] }));
  check('unstamped snapshot does not refresh', !goalFrameRefreshesJournal({ type: 'subscription_snapshot', messages: [] }));
  check('bootstrap-time cut leaves refresh to the mount fetch', goalFrameRefreshesJournal({ type: 'session_resync', authoritative: true }, false) === false
    && goalFrameRefreshesJournal({ type: 'subscription_snapshot', authoritative: true, messages: [] }, false) === false);
}

{
  const commands: RunnerCommand[] = [];
  const order: string[] = [];
  let refreshCalls = 0;
  let journalCalls = 0;
  await completeGoalMutation(
    async (command) => {
      commands.push(command);
      order.push(command.type);
      return {};
    },
    goalCommandFor('pause'),
    () => {
      refreshCalls += 1;
      order.push('refreshGoal');
    },
    () => {
      journalCalls += 1;
      order.push('refreshJournal');
    },
  );
  check('mutation sends existing command first', commands.length === 1 && commands[0].type === 'goal_pause');
  check(
    'mutation requests one goal-only canonical refresh',
    refreshCalls === 1 && commands.every((command) => command.type !== 'job_list' && command.type !== 'goal_get'),
  );
  check('mutation refreshes selected journal', journalCalls === 1);
  check(
    'mutation order is sendCommand then refreshGoal then journal',
    JSON.stringify(order) === JSON.stringify(['goal_pause', 'refreshGoal', 'refreshJournal']),
  );

  let failedRefreshCalls = 0;
  let failedJournalCalls = 0;
  let failed = false;
  try {
    await completeGoalMutation(
      async () => { throw new Error('rejected'); },
      goalCommandFor('drop'),
      () => { failedRefreshCalls += 1; },
      () => { failedJournalCalls += 1; },
    );
  } catch {
    failed = true;
  }
  check('failed mutation does not hide the error', failed);
  check('failed mutation does not refresh canonical goal', failedRefreshCalls === 0);
  check('failed mutation does not refresh journal', failedJournalCalls === 0);
}

{
  const payload = { revision: 4, current: null };
  const fetched = await fetchChatGoalState(async () => payload);
  check('fetchChatGoalState returns a parsed snapshot', fetched.revision === 4 && fetched.current === null);
  let getFailed = false;
  try {
    await fetchChatGoalState(async () => { throw new Error('goal_get failed'); });
  } catch (error) {
    getFailed = error instanceof Error && error.message === 'goal_get failed';
  }
  check('fetchChatGoalState propagates sendCommand errors', getFailed);
  let invalidGet = false;
  try {
    await fetchChatGoalState(async () => ({ not: 'a goal' }));
  } catch (error) {
    invalidGet = error instanceof Error && error.message === 'invalid goal_get response';
  }
  check('fetchChatGoalState rejects invalid payloads', invalidGet);
  const journal = await fetchChatGoalJournal(async () => []);
  check('fetchChatGoalJournal returns journal rows', Array.isArray(journal) && journal.length === 0);
  let journalFailed = false;
  try {
    await fetchChatGoalJournal(async () => { throw new Error('goal_journal failed'); });
  } catch (error) {
    journalFailed = error instanceof Error && error.message === 'goal_journal failed';
  }
  check('fetchChatGoalJournal propagates sendCommand errors', journalFailed);
  let invalidJournal = false;
  try {
    await fetchChatGoalJournal(async () => ({ entries: [] }));
  } catch (error) {
    invalidJournal = error instanceof Error && error.message === 'invalid goal_journal response';
  }
  check('fetchChatGoalJournal rejects non-array payloads', invalidJournal);
  const stale = await fetchCurrentChatGoalJournal(
    async () => { throw new Error('stale journal'); },
    1,
    () => 2,
  );
  check('superseded journal failure is non-fatal', stale.kind === 'superseded');
  let currentFailed = false;
  try {
    await fetchCurrentChatGoalJournal(
      async () => { throw new Error('current journal'); },
      2,
      () => 2,
    );
  } catch (error) {
    currentFailed = error instanceof Error && error.message === 'current journal';
  }
  check('current journal failure still propagates', currentFailed);
  const currentOk = await fetchCurrentChatGoalJournal(
    async () => [],
    3,
    () => 3,
  );
  check('current journal fetch returns rows', currentOk.kind === 'current' && currentOk.entries.length === 0);
}

{
  const here = dirname(fileURLToPath(import.meta.url));
  const webGoal = readFileSync(join(here, '../src/panels/GoalPanel.tsx'), 'utf8');
  const chatGoal = readFileSync(join(here, '../src/chat/ChatGoalPanel.tsx'), 'utf8');
  const webCreateAt = webGoal.indexOf('const onCreate = (e: FormEvent) => {');
  const webCreate = webCreateAt >= 0 ? webGoal.slice(webCreateAt, webCreateAt + 900) : '';
  const chatCreateAt = chatGoal.indexOf('const onCreate = (e: FormEvent) => {');
  const chatCreate = chatCreateAt >= 0 ? chatGoal.slice(chatCreateAt, chatCreateAt + 900) : '';
  check('web Goal run claims busyRef before send', webGoal.includes('if (busyRef.current) return false;') && webGoal.includes('busyRef.current = true;'));
  check('web Goal create keeps draft until accepted ACK', webCreate.includes('if (!done) return;') && webCreate.includes('current.trim() === text') && !webCreate.includes("setObjective('');"));
  check('web Goal pin keeps draft until accepted ACK', webGoal.includes("void run({ type: 'goal_pin', text }).then((done) =>") && webGoal.includes('setPinText((current) => current.trim() === text'));
  check('chat Goal run claims busyRef before send', chatGoal.includes('if (busyRef.current) return false;') && chatGoal.includes('busyRef.current = true;'));
  check('chat Goal create keeps draft until accepted ACK', chatCreate.includes('if (!done) return;') && chatCreate.includes('current.trim() === text') && !chatCreate.includes("setObjective('');"));
}

console.log(`\nchatGoalPanel.test: ${assertions} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
