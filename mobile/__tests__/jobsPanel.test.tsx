import React from 'react';
import renderer, { act } from 'react-test-renderer';
import { Text } from 'react-native';
import type { RunnerClient } from '../src/runnerClient';
import { JobsPanel } from '../src/screens/JobsPanel';
import { DARK } from '../src/ui';

type Command = { type: string } & Record<string, unknown>;
type Call = { command: Command; sessionId: string | undefined };
const SESSION = 'jobs-session';
const OTHER = 'other-session';
const LIVE = { id: 'j1', agentId: 'a1', agent: 'worker', parentId: 'parent', description: 'Investigate rendering', status: 'running', createdAt: 1 };
const QUEUED = { ...LIVE, id: 'j2', agentId: 'a2', description: 'Queued investigation', status: 'queued' };
const SECRET = 'sk-abcdefghijklmnopqrstuvwx';
const MESSAGE = { id: 'm1', from: 'a1', to: 'parent', body: 'Mailbox greeting', timestamp: 1 };
function ok(type: string, data: unknown) { return { type: 'response', command: type, success: true, data }; }
function fail(type: string, error: string) { return { type: 'response', command: type, success: false, error }; }
function list(extra: Record<string, unknown> = {}) { return ok('job_list', { enabled: true, jobs: [LIVE], messages: [MESSAGE], ...extra }); }
function output(text = 'Delivered answer', error?: string) {
  return ok('job_output', { job: { ...LIVE, result: { index: 0, id: LIVE.id, agent: LIVE.agent, status: 'completed', output: text, error, artifactRef: '', historyRef: '', artifactUri: '' } } });
}
function defaults(command: Command): unknown {
  switch (command.type) {
    case 'job_list': return list();
    case 'task_spawn': return ok(command.type, { spawns: [{ jobId: 'spawned', agentId: 'spawned-agent' }] });
    case 'job_cancel': return ok(command.type, {});
    case 'hub_send': return ok(command.type, { receipts: [{ to: command.to, outcome: 'delivered', messageId: 'sent' }] });
    case 'job_output': return output();
    case 'agent_history': return ok(command.type, { agentId: command.agentId, text: 'History greeting' });
    default: return fail(command.type, 'unexpected command');
  }
}
function mockClient(handler: (command: Command, sessionId?: string) => unknown = defaults) {
  const calls: Call[] = [];
  let listener: ((frame: unknown) => void) | null = null;
  const client = {
    command: async (command: Command, sessionId?: string) => { calls.push({ command, sessionId }); return handler(command, sessionId); },
    subscribeFrames: (next: (frame: unknown) => void) => { listener = next; return () => { listener = null; }; },
  } as unknown as RunnerClient;
  return { client, calls, emit: (frame: unknown) => listener?.(frame) };
}
function deferred() {
  let resolve!: (value: unknown) => void;
  const promise = new Promise<unknown>((done) => { resolve = done; });
  return { promise, resolve };
}
function panel(client: RunnerClient, onError: (message: string) => void, sessionId = SESSION, onClose = () => {}) {
  return <JobsPanel palette={DARK} client={client} sessionId={sessionId} onError={onError} onClose={onClose} />;
}
async function withPanel(client: RunnerClient, onError: (message: string) => void, run: (tree: renderer.ReactTestRenderer) => Promise<void>, onClose = () => {}) {
  let tree!: renderer.ReactTestRenderer;
  await act(async () => { tree = renderer.create(panel(client, onError, SESSION, onClose)); });
  try { await run(tree); } finally { await act(async () => { tree.unmount(); }); }
}
function labeled(tree: renderer.ReactTestRenderer, label: string) { return tree.root.findByProps({ accessibilityLabel: label }); }
function hasLabel(tree: renderer.ReactTestRenderer, label: string) { return tree.root.findAllByProps({ accessibilityLabel: label }).length > 0; }
function text(tree: renderer.ReactTestRenderer) {
  return tree.root.findAllByType(Text).flatMap((node) => typeof node.props.children === 'string' ? [node.props.children] : []).join('\n');
}
async function press(tree: renderer.ReactTestRenderer, label: string) { await act(async () => { labeled(tree, label).props.onPress(); }); }
async function type(tree: renderer.ReactTestRenderer, label: string, value: string) { await act(async () => { labeled(tree, label).props.onChangeText(value); }); }
function callsOf(calls: Call[], type: string) { return calls.filter((call) => call.command.type === type); }

it('loads session jobs, paints their descriptions, and closes with Back', async () => {
  const { client, calls } = mockClient();
  const closed = jest.fn();
  await withPanel(client, () => {}, async (tree) => {
    expect(calls).toEqual([{ command: { type: 'job_list' }, sessionId: SESSION }]);
    expect(text(tree)).toContain(LIVE.description);
    await press(tree, 'Back');
    expect(closed).toHaveBeenCalledTimes(1);
  }, closed);
});

it('blocks spawning when orchestration is disabled or the task is empty', async () => {
  const disabled = mockClient(() => list({ enabled: false }));
  await withPanel(disabled.client, () => {}, async (tree) => {
    expect(text(tree)).toContain('Orchestration not enabled');
    await type(tree, 'Spawn task', 'work');
    expect(labeled(tree, 'Spawn').props.disabled).toBe(true);
    await press(tree, 'Spawn');
    expect(callsOf(disabled.calls, 'task_spawn')).toHaveLength(0);
  });
  const enabled = mockClient();
  await withPanel(enabled.client, () => {}, async (tree) => {
    await type(tree, 'Spawn task', '  ');
    expect(labeled(tree, 'Spawn').props.disabled).toBe(true);
    await press(tree, 'Spawn');
    expect(callsOf(enabled.calls, 'task_spawn')).toHaveLength(0);
  });
});

it('selects valid catalog chips, trims spawn drafts, omits an empty agent, and refreshes', async () => {
  const { client, calls } = mockClient((command) => command.type === 'job_list' ? list({ catalog: [null, 1, {}, { name: '' }, { name: 'scout', description: 'Research' }] }) : defaults(command));
  await withPanel(client, () => {}, async (tree) => {
    expect(hasLabel(tree, 'Catalog undefined')).toBe(false);
    expect(hasLabel(tree, 'Catalog ')).toBe(false);
    await press(tree, 'Catalog scout');
    expect(labeled(tree, 'Spawn agent').props.value).toBe('scout');
    await type(tree, 'Spawn task', '  investigate  ');
    await press(tree, 'Spawn');
    expect(callsOf(calls, 'task_spawn')).toEqual([{ sessionId: SESSION, command: { type: 'task_spawn', args: { task: 'investigate', agent: 'scout' } } }]);
    expect(labeled(tree, 'Spawn task').props.value).toBe('');
    expect(calls.map((call) => call.command.type)).toEqual(['job_list', 'task_spawn', 'job_list']);
    await type(tree, 'Spawn agent', '  ');
    await type(tree, 'Spawn task', 'next');
    await press(tree, 'Spawn');
    expect(callsOf(calls, 'task_spawn')[1]).toEqual({ sessionId: SESSION, command: { type: 'task_spawn', args: { task: 'next' } } });
    expect(calls.every((call) => call.sessionId === SESSION)).toBe(true);
  });
});

it.each([{ spawns: [] }, {}])('reports empty spawn results %j without discarding jobs or the draft', async (result) => {
  const errors: string[] = [];
  const { client, calls } = mockClient((command) => command.type === 'task_spawn' ? ok(command.type, result) : defaults(command));
  await withPanel(client, (message) => errors.push(message), async (tree) => {
    await type(tree, 'Spawn task', 'try again');
    await press(tree, 'Spawn');
    expect(errors).toEqual(['Spawn: task_spawn returned no jobs']);
    expect(text(tree)).toContain(LIVE.description);
    expect(labeled(tree, 'Spawn task').props.value).toBe('try again');
    expect(callsOf(calls, 'job_list')).toHaveLength(1);
  });
});

it('allows only one in-flight Spawn and releases the guard after completion', async () => {
  const pending = deferred();
  const { client, calls } = mockClient((command) => command.type === 'task_spawn' ? pending.promise : defaults(command));
  await withPanel(client, () => {}, async (tree) => {
    await type(tree, 'Spawn task', 'work');
    await press(tree, 'Spawn');
    await press(tree, 'Spawn');
    expect(callsOf(calls, 'task_spawn')).toHaveLength(1);
    expect(labeled(tree, 'Spawn').props.disabled).toBe(true);
    await act(async () => { pending.resolve(defaults({ type: 'task_spawn' })); });
    await type(tree, 'Spawn task', 'more work');
    await press(tree, 'Spawn');
    expect(callsOf(calls, 'task_spawn')).toHaveLength(2);
  });
});

it('filters all job statuses and stops live jobs with a session-scoped refresh', async () => {
  const settled = ['completed', 'failed', 'cancelled'].map((status) => ({ ...LIVE, id: status, description: `${status} task`, status }));
  const { client, calls } = mockClient((command) => command.type === 'job_list' ? list({ jobs: [LIVE, QUEUED, ...settled] }) : defaults(command));
  await withPanel(client, () => {}, async (tree) => {
    expect(text(tree)).toContain(LIVE.description);
    expect(text(tree)).toContain(QUEUED.description);
    for (const job of settled) expect(text(tree)).not.toContain(job.description);
    expect(hasLabel(tree, 'Stop j1')).toBe(true);
    expect(hasLabel(tree, 'Stop j2')).toBe(true);
    await press(tree, 'Completed');
    expect(text(tree)).not.toContain(LIVE.description);
    expect(text(tree)).not.toContain(QUEUED.description);
    for (const job of settled) {
      expect(text(tree)).toContain(job.description);
      expect(hasLabel(tree, `Stop ${job.id}`)).toBe(false);
    }
    await press(tree, 'Running');
    await press(tree, 'Stop j2');
    expect(callsOf(calls, 'job_cancel')).toEqual([{ sessionId: SESSION, command: { type: 'job_cancel', jobIds: ['j2'] } }]);
    expect(calls.map((call) => call.command.type)).toEqual(['job_list', 'job_cancel', 'job_list']);
    expect(calls.every((call) => call.sessionId === SESSION)).toBe(true);
  });
});

it('holds Stop across rows until the pending cancellation settles', async () => {
  const pending = deferred();
  const { client, calls } = mockClient((command) => command.type === 'job_list' ? list({ jobs: [LIVE, QUEUED] }) : command.type === 'job_cancel' ? pending.promise : defaults(command));
  await withPanel(client, () => {}, async (tree) => {
    await press(tree, 'Stop j1');
    await press(tree, 'Stop j2');
    expect(callsOf(calls, 'job_cancel')).toHaveLength(1);
    await act(async () => { pending.resolve(ok('job_cancel', {})); });
    await press(tree, 'Stop j2');
    expect(callsOf(calls, 'job_cancel')).toHaveLength(2);
  });
});

it('keeps a failed hub draft, sends to the agent ID, and clears only the successful draft', async () => {
  const errors: string[] = [];
  let rejected = true;
  const { client, calls } = mockClient((command) => {
    if (command.type === 'job_list') return list({ jobs: [LIVE, QUEUED] });
    if (command.type === 'hub_send' && rejected) return ok(command.type, { receipts: [{ to: command.to, outcome: 'failed', error: 'mailbox unavailable' }] });
    return defaults(command);
  });
  await withPanel(client, (message) => errors.push(message), async (tree) => {
    expect(labeled(tree, 'Send hub j1').props.disabled).toBe(true);
    await type(tree, 'Hub j1', '  hello worker  ');
    await type(tree, 'Hub j2', 'keep this');
    await press(tree, 'Send hub j1');
    expect(callsOf(calls, 'hub_send')[0]).toEqual({ sessionId: SESSION, command: { type: 'hub_send', to: 'a1', body: 'hello worker' } });
    expect(errors).toEqual(['Hub: mailbox unavailable']);
    expect(labeled(tree, 'Hub j1').props.value).toBe('  hello worker  ');
    rejected = false;
    await press(tree, 'Send hub j1');
    expect(labeled(tree, 'Hub j1').props.value).toBe('');
    expect(labeled(tree, 'Hub j2').props.value).toBe('keep this');
    expect(callsOf(calls, 'job_list')).toHaveLength(2);
    expect(calls.every((call) => call.sessionId === SESSION)).toBe(true);
  });
});

it.each([
  ['body', output(`Answer ${SECRET}`), 'Answer [REDACTED]'],
  ['error', output('ignored body', `Failure ${SECRET}`), 'Failure [REDACTED]'],
  ['undelivered', ok('job_output', { job: LIVE }), '(no delivered output yet)'],
])('toggles %s output and hides it without another RPC', async (_name, response, expected) => {
  const { client, calls } = mockClient((command) => command.type === 'job_output' ? response : defaults(command));
  await withPanel(client, () => {}, async (tree) => {
    await press(tree, 'Output j1');
    expect(labeled(tree, 'Output body j1').props.children).toBe(expected);
    expect(text(tree)).not.toContain(SECRET);
    expect(callsOf(calls, 'job_output')).toEqual([{ sessionId: SESSION, command: { type: 'job_output', jobId: 'j1' } }]);
    await press(tree, 'Hide output j1');
    expect(hasLabel(tree, 'Output body j1')).toBe(false);
    expect(callsOf(calls, 'job_output')).toHaveLength(1);
  });
});

it('prevents overlapping output requests and reports invalid snapshots', async () => {
  const pending = deferred();
  const errors: string[] = [];
  let waiting = true;
  const { client, calls } = mockClient((command) => command.type === 'job_output' ? waiting ? pending.promise : ok(command.type, { job: { ...LIVE, id: 'wrong' } }) : defaults(command));
  await withPanel(client, (message) => errors.push(message), async (tree) => {
    await press(tree, 'Output j1');
    await press(tree, 'Output j1');
    expect(callsOf(calls, 'job_output')).toHaveLength(1);
    await act(async () => { pending.resolve(output()); });
    expect(labeled(tree, 'Output body j1').props.children).toBe('Delivered answer');
    await press(tree, 'Hide output j1');
    waiting = false;
    await press(tree, 'Output j1');
    expect(errors).toEqual(['Output: invalid job_output']);
    expect(hasLabel(tree, 'Output body j1')).toBe(false);
  });
});

it.each(['running', 'queued'])('loads and redacts %s history, polls every two seconds, and stops when hidden', async (status) => {
  jest.useFakeTimers();
  const { client, calls } = mockClient((command) => command.type === 'job_list' ? list({ jobs: [{ ...LIVE, status }] }) : command.type === 'agent_history' ? ok(command.type, { agentId: command.agentId, text: `History ${SECRET}` }) : defaults(command));
  try {
    await withPanel(client, () => {}, async (tree) => {
      await press(tree, 'Details j1');
      expect(labeled(tree, 'History j1').props.children).toBe('History [REDACTED]');
      expect(callsOf(calls, 'agent_history')).toEqual([{ sessionId: SESSION, command: { type: 'agent_history', agentId: 'a1', lines: 200 } }]);
      await act(async () => { jest.advanceTimersByTime(1999); });
      expect(callsOf(calls, 'agent_history')).toHaveLength(1);
      await act(async () => { jest.advanceTimersByTime(1); });
      expect(callsOf(calls, 'agent_history')).toHaveLength(2);
      expect(text(tree)).toContain('Hide details');
      await press(tree, 'Details j1');
      expect(hasLabel(tree, 'History j1')).toBe(false);
      await act(async () => { jest.advanceTimersByTime(4000); });
      expect(callsOf(calls, 'agent_history')).toHaveLength(2);
      expect(calls.every((call) => call.sessionId === SESSION)).toBe(true);
    });
  } finally { jest.useRealTimers(); }
});

it('shows the selected details history past the legacy 800-char slice with a redacted end marker and an unclamped title', async () => {
  const history = `${'x'.repeat(900)} ${SECRET} HISTORY-END`;
  const description = `${'d'.repeat(400)} DESCRIPTION-TAIL`;
  const { client, calls } = mockClient((command) => command.type === 'job_list'
    ? list({ jobs: [{ ...LIVE, description }] })
    : command.type === 'agent_history' ? ok(command.type, { agentId: command.agentId, text: history }) : defaults(command));
  await withPanel(client, () => {}, async (tree) => {
    await press(tree, 'Details j1');
    expect(callsOf(calls, 'agent_history')).toEqual([{ sessionId: SESSION, command: { type: 'agent_history', agentId: 'a1', lines: 200 } }]);
    const historyText = labeled(tree, 'History j1').props.children as string;
    expect(historyText.length).toBeGreaterThan(800);
    expect(historyText.endsWith('HISTORY-END')).toBe(true);
    expect(historyText).toContain('[REDACTED]');
    expect(historyText).not.toContain(SECRET);
    const title = tree.root.findAllByType(Text).find((node) => node.props.children === description);
    expect(title?.props.numberOfLines).toBeUndefined();
    expect(title?.props.selectable).toBe(true);
  });
});

it('rejects history for a different agent and does not poll completed jobs', async () => {
  jest.useFakeTimers();
  const errors: string[] = [];
  const { client, calls } = mockClient((command) => command.type === 'job_list' ? list({ jobs: [{ ...LIVE, status: 'completed' }] }) : command.type === 'agent_history' ? ok(command.type, { agentId: 'wrong', text: 'Wrong history' }) : defaults(command));
  try {
    await withPanel(client, (message) => errors.push(message), async (tree) => {
      await press(tree, 'Completed');
      await press(tree, 'Details j1');
      expect(errors).toEqual(['History: invalid agent_history']);
      expect(text(tree)).not.toContain('Wrong history');
      await act(async () => { jest.advanceTimersByTime(6000); });
      expect(callsOf(calls, 'agent_history')).toHaveLength(1);
    });
  } finally { jest.useRealTimers(); }
});

it('shows mailbox messages, redacts secrets, and deduplicates only matching-session deliveries', async () => {
  const { client, emit } = mockClient();
  await withPanel(client, () => {}, async (tree) => {
    await press(tree, 'IRC page');
    expect(text(tree)).toContain(MESSAGE.body);
    await act(async () => {
      emit({ type: 'message_delivered', sessionId: OTHER, message: { ...MESSAGE, id: 'foreign', body: 'Foreign mailbox' } });
      emit({ type: 'message_delivered', sessionId: SESSION, message: { ...MESSAGE, id: 'invalid', timestamp: 'bad', body: 'Invalid mailbox' } });
      const frame = { type: 'message_delivered', sessionId: SESSION, message: { ...MESSAGE, id: 'm2', body: `Delivered ${SECRET}` } };
      emit(frame);
      emit(frame);
    });
    expect(text(tree)).not.toContain('Foreign mailbox');
    expect(text(tree)).not.toContain('Invalid mailbox');
    expect(text(tree)).not.toContain(SECRET);
    expect(text(tree).split('Delivered [REDACTED]')).toHaveLength(2);
    await press(tree, 'Jobs page');
    expect(text(tree)).toContain(LIVE.description);
  });
});

it('upserts matching-session jobs and ignores foreign updates', async () => {
  const { client, emit } = mockClient();
  await withPanel(client, () => {}, async (tree) => {
    await act(async () => {
      emit({ type: 'job_updated', sessionId: OTHER, job: { ...LIVE, description: 'Foreign replacement' } });
      emit({ type: 'job_updated', sessionId: SESSION, job: QUEUED });
    });
    expect(text(tree)).toContain(LIVE.description);
    expect(text(tree)).not.toContain('Foreign replacement');
    expect(text(tree)).toContain(QUEUED.description);
    await act(async () => { emit({ type: 'job_updated', sessionId: SESSION, job: { ...LIVE, description: 'Finished investigation', status: 'completed' } }); });
    expect(text(tree)).not.toContain(LIVE.description);
    await press(tree, 'Completed');
    expect(text(tree)).toContain('Finished investigation');
    expect(hasLabel(tree, 'Stop j1')).toBe(false);
  });
});

it('retains last-good jobs on failed refreshes and malformed mailbox data', async () => {
  const errors: string[] = [];
  let response: unknown = list();
  const { client } = mockClient(() => response);
  await withPanel(client, (message) => errors.push(message), async (tree) => {
    response = fail('job_list', 'runner busy');
    await press(tree, 'Refresh');
    expect(errors).toEqual(['Jobs: runner busy']);
    expect(text(tree)).toContain(LIVE.description);
    response = list({ jobs: [QUEUED], messages: null });
    await press(tree, 'Refresh');
    expect(errors).toEqual(['Jobs: runner busy', 'Jobs: invalid job_list messages']);
    expect(text(tree)).toContain(LIVE.description);
    expect(text(tree)).not.toContain(QUEUED.description);
  });
});

it('leaves jobs unloaded if the initial mailbox is invalid', async () => {
  const errors: string[] = [];
  const { client } = mockClient(() => list({ messages: null }));
  await withPanel(client, (message) => errors.push(message), async (tree) => {
    expect(errors).toEqual(['Jobs: invalid job_list messages']);
    expect(text(tree)).not.toContain(LIVE.description);
    expect(text(tree)).toContain('--');
  });
});

it('resets jobs, mailbox, output, history, and drafts when switching sessions', async () => {
  const next = deferred();
  const errors: string[] = [];
  const onError = (message: string) => errors.push(message);
  const { client, calls } = mockClient((command, sessionId) => command.type === 'job_list' && sessionId === OTHER ? next.promise : defaults(command));
  await withPanel(client, onError, async (tree) => {
    await press(tree, 'Output j1');
    await press(tree, 'Details j1');
    await type(tree, 'Spawn task', 'old task');
    await type(tree, 'Hub j1', 'old draft');
    await act(async () => { tree.update(panel(client, onError, OTHER)); });
    expect(text(tree)).not.toContain(LIVE.description);
    expect(hasLabel(tree, 'Output body j1')).toBe(false);
    expect(hasLabel(tree, 'History j1')).toBe(false);
    expect(labeled(tree, 'Spawn task').props.value).toBe('');
    await press(tree, 'IRC page');
    expect(text(tree)).not.toContain(MESSAGE.body);
    expect(text(tree)).toContain('No hub mailbox messages.');
    expect(callsOf(calls, 'job_list').map((call) => call.sessionId)).toEqual([SESSION, OTHER]);
    await act(async () => { next.resolve(list({ messages: [] })); });
    await press(tree, 'Jobs page');
    expect(labeled(tree, 'Hub j1').props.value).toBe('');
    expect(hasLabel(tree, 'Output body j1')).toBe(false);
    expect(hasLabel(tree, 'History j1')).toBe(false);
    expect(errors).toEqual([]);
  });
});

it('does not paint a stale job_list from the previous session', async () => {
  const pending = deferred();
  const onError = () => {};
  const { client } = mockClient((_command, sessionId) => sessionId === SESSION ? pending.promise : list({ jobs: [QUEUED], messages: [] }));
  await withPanel(client, onError, async (tree) => {
    await act(async () => { tree.update(panel(client, onError, OTHER)); });
    await act(async () => { pending.resolve(list()); });
    expect(text(tree)).not.toContain(LIVE.description);
  });
});

it('does not report a late job_list failure after unmount', async () => {
  const pending = deferred();
  const errors: string[] = [];
  const { client } = mockClient(() => pending.promise);
  let tree!: renderer.ReactTestRenderer;
  await act(async () => { tree = renderer.create(panel(client, (message) => errors.push(message))); });
  await act(async () => { tree.unmount(); });
  await act(async () => { pending.resolve(fail('job_list', 'late failure')); });
  expect(errors).toEqual([]);
});
