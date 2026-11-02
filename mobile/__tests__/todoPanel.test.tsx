import React from 'react';
import renderer, { act } from 'react-test-renderer';
import { Text, TextInput, TouchableOpacity } from 'react-native';
import { TodoPanel } from '../src/screens/TodoPanel';
import { parseRunnerCommand, type RunnerCommand, type RpcResponse } from '../src/protocol';
import type { RunnerClient } from '../src/runnerClient';
import { DARK } from '../src/ui';

const SESSION = 'todo-parent-7';
const task = (id: string, content: string, status = 'pending', dependsOn: string[] = []) => ({
  id, content, status, dependsOn, ready: (status === 'pending' || status === 'in_progress') && dependsOn.length === 0,
  waitingOn: dependsOn.map((taskId) => ({ taskId, content: 'Design', status: 'in_progress' })),
});
const ok = (command: string, data: unknown): RpcResponse => ({ type: 'response', command, success: true, data });
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((complete) => { resolve = complete; });
  return { promise, resolve };
}
function harness(handler: (command: RunnerCommand, sessionId?: string) => RpcResponse | Promise<RpcResponse>) {
  const listeners = new Set<(frame: unknown) => void>();
  const command = jest.fn(handler);
  const client = { command, subscribeFrames: (listener: (frame: unknown) => void) => {
    listeners.add(listener);
    return () => { listeners.delete(listener); };
  } } as unknown as RunnerClient;
  return { client, command, emit: (frame: unknown) => listeners.forEach((listener) => listener(frame)) };
}
const button = (tree: renderer.ReactTestRenderer, label: string) => tree.root.findAllByType(TouchableOpacity).find((node) => node.props.accessibilityLabel === label)!;
const input = (tree: renderer.ReactTestRenderer, label: string) => tree.root.findAllByType(TextInput).find((node) => node.props.accessibilityLabel === label)!;
const text = (tree: renderer.ReactTestRenderer) => tree.root.findAllByType(Text).map((node) => node.props.children).flat().join(' ');
async function mount(client: RunnerClient, run: (tree: renderer.ReactTestRenderer, errors: jest.Mock) => Promise<void>) {
  let tree!: renderer.ReactTestRenderer;
  const errors = jest.fn();
  try {
    await act(async () => { tree = renderer.create(<TodoPanel palette={DARK} client={client} sessionId={SESSION} onClose={() => {}} onError={errors} />); });
    await run(tree, errors);
  } finally {
    if (tree) await act(async () => { tree.unmount(); });
  }
}

it('appends phase/items, completes by backend id and reopens the completed task', async () => {
  let phases = [{ name: 'Plan', tasks: [task('task-design-1', 'Design', 'in_progress')] }];
  const backend = harness((command, sessionId) => {
    expect(sessionId).toBe(SESSION);
    expect(parseRunnerCommand(command)).toEqual(command);
    if (command.type === 'todo_child_list') return ok(command.type, []);
    if (command.type === 'get_state') return ok(command.type, { sessionId: SESSION, todoPhases: phases });
    if (command.type !== 'todo_op') throw new Error('Unexpected command');
    if (command.op === 'append') {
      expect(command).toEqual({ type: 'todo_op', op: 'append', phase: 'Ship', items: ['Deploy'] });
      phases = [...phases, { name: 'Ship', tasks: [task('task-deploy-2', 'Deploy')] }];
    } else {
      expect(command.task).toBe('task-deploy-2');
      phases = phases.map((phase) => ({ ...phase, tasks: phase.tasks.map((entry) => entry.id === command.task ? task(entry.id, entry.content, command.op === 'done' ? 'completed' : 'in_progress', entry.dependsOn) : entry) }));
    }
    return ok(command.type, { phases, completedTasks: [], summary: 'Updated' });
  });
  await mount(backend.client, async (tree) => {
    await act(async () => { input(tree, 'Phase').props.onChangeText(' Ship '); input(tree, 'New task').props.onChangeText('Deploy'); });
    await act(async () => { button(tree, 'Add task').props.onPress(); });
    expect(input(tree, 'New task').props.value).toBe('');
    expect(text(tree)).toContain('Deploy');
    await act(async () => { button(tree, 'Done Deploy').props.onPress(); });
    expect(text(tree)).toContain('0 open · 0 active · 0 waiting · 1 done · 0 abandoned');
    await act(async () => { button(tree, 'Reopen Deploy').props.onPress(); });
    expect(text(tree)).toContain('1 open · 1 active · 0 waiting · 0 done · 0 abandoned');
    expect(button(tree, 'Done Deploy')).toBeDefined();
    expect(button(tree, 'Reopen Deploy')).toBeUndefined();
    expect(backend.command.mock.calls.filter(([command]) => command.type === 'todo_op').map(([command]) => command)).toEqual([
      { type: 'todo_op', op: 'append', phase: 'Ship', items: ['Deploy'] },
      { type: 'todo_op', op: 'done', task: 'task-deploy-2' },
      { type: 'todo_op', op: 'start', task: 'task-deploy-2' },
    ]);
  });
});

it('preserves last-good list and refused add draft across failed and malformed refreshes', async () => {
  let refreshFailure: 'none' | 'refused' | 'malformed' | 'rejected' = 'none';
  const backend = harness((command) => {
    if (command.type === 'todo_child_list') return ok(command.type, []);
    if (command.type === 'todo_op') return { type: 'response', command: command.type, success: false, error: 'Append denied' };
    if (refreshFailure === 'refused') return { type: 'response', command: command.type, success: false, error: 'State denied' };
    if (refreshFailure === 'rejected') throw new Error('Disconnected');
    return ok(command.type, { todoPhases: refreshFailure === 'malformed' ? [{ name: 'Plan', tasks: [{ id: 'broken' }] }] : [{ name: 'Plan', tasks: [task('task-1', 'Keep me', 'in_progress')] }] });
  });
  await mount(backend.client, async (tree, errors) => {
    await act(async () => { input(tree, 'Phase').props.onChangeText('Plan'); input(tree, 'New task').props.onChangeText('Retry me'); });
    await act(async () => { button(tree, 'Add task').props.onPress(); });
    expect(input(tree, 'New task').props.value).toBe('Retry me');
    expect(errors).toHaveBeenLastCalledWith('Todo append: Append denied');
    for (const failure of ['refused', 'malformed', 'rejected'] as const) {
      refreshFailure = failure;
      await act(async () => { button(tree, 'Refresh Todo').props.onPress(); });
      expect(text(tree)).toContain('Keep me');
      expect(input(tree, 'New task').props.value).toBe('Retry me');
    }
    expect(errors.mock.calls.map(([message]) => message)).toEqual(['Todo append: Append denied', 'Refresh Todo: State denied', 'Refresh Todo: Invalid Todo task', 'Refresh Todo: Disconnected']);
  });
});

it('shows selected task readiness and edits dependency edges using IDs, not labels', async () => {
  let phases = [{ name: 'Plan', tasks: [task('task-design-1', 'Design', 'in_progress'), task('task-build-2', 'Build')] }];
  const backend = harness((command, sessionId) => {
    expect(sessionId).toBe(SESSION);
    expect(parseRunnerCommand(command)).toEqual(command);
    if (command.type === 'todo_child_list') return ok(command.type, []);
    if (command.type === 'todo_op') {
      expect(command.task).toBe('task-build-2');
      expect(command.dependsOn).toEqual(['task-design-1']);
      phases = [{ name: 'Plan', tasks: [task('task-design-1', 'Design', 'in_progress'), task('task-build-2', 'Build', 'pending', command.op === 'add_dependency' ? ['task-design-1'] : [])] }];
      return ok(command.type, { phases, completedTasks: [], summary: 'Updated' });
    }
    return ok(command.type, { todoPhases: phases });
  });
  await mount(backend.client, async (tree) => {
    await act(async () => { button(tree, 'Details Build').props.onPress(); });
    expect(text(tree)).toContain('ID: task-build-2 · Phase: Plan · Status: pending · Ready: yes');
    expect(button(tree, 'Link Build')).toBeUndefined();
    await act(async () => { button(tree, 'Link Design').props.onPress(); });
    expect(text(tree)).toContain('Waiting on: Design (in_progress)');
    expect(text(tree)).toContain('2 open · 1 active · 1 waiting · 0 done · 0 abandoned');
    await act(async () => { button(tree, 'Unlink task-design-1').props.onPress(); });
    expect(text(tree)).toContain('No dependencies.');
    expect(text(tree)).not.toContain('Waiting on:');
    expect(backend.command.mock.calls.filter(([command]) => command.type === 'todo_op').map(([command]) => command)).toEqual([
      { type: 'todo_op', op: 'add_dependency', task: 'task-build-2', dependsOn: ['task-design-1'] },
      { type: 'todo_op', op: 'remove_dependency', task: 'task-build-2', dependsOn: ['task-design-1'] },
    ]);
  });
});

it('keeps mutations single-flight synchronously and preserves an edited draft on completion', async () => {
  const action = deferred<RpcResponse>();
  let phases = [{ name: 'Plan', tasks: [task('task-1', 'Existing', 'in_progress')] }];
  const backend = harness((command) => {
    if (command.type === 'todo_child_list') return ok(command.type, []);
    return command.type === 'todo_op' ? action.promise : ok(command.type, { todoPhases: phases });
  });
  await mount(backend.client, async (tree) => {
    await act(async () => { input(tree, 'Phase').props.onChangeText('Plan'); input(tree, 'New task').props.onChangeText('First'); });
    const add = button(tree, 'Add task').props.onPress;
    const done = button(tree, 'Done Existing').props.onPress;
    await act(async () => { add(); done(); add(); });
    expect(backend.command.mock.calls.filter(([command]) => command.type === 'todo_op')).toHaveLength(1);
    expect(button(tree, 'Done Existing').props.disabled).toBe(true);
    await act(async () => {
      input(tree, 'New task').props.onChangeText('Next');
      phases = [{ name: 'Plan', tasks: [task('task-1', 'Existing', 'in_progress'), task('task-2', 'First')] }];
      action.resolve(ok('todo_op', { phases, completedTasks: [], summary: 'Updated' }));
    });
    expect(input(tree, 'New task').props.value).toBe('Next');
    expect(button(tree, 'Done Existing').props.disabled).toBe(false);
  });
});

it('ignores foreign/unscoped events and older refresh completions', async () => {
  const old = deferred<RpcResponse>();
  let reads = 0;
  const backend = harness((command) => {
    if (command.type === 'todo_child_list') return ok(command.type, []);
    reads += 1;
    if (reads === 2) return old.promise;
    return ok(command.type, { todoPhases: [{ name: 'Plan', tasks: [task('task-1', reads === 1 ? 'Initial' : 'Latest', 'in_progress')] }] });
  });
  await mount(backend.client, async (tree) => {
    await act(async () => { backend.emit({ type: 'todo_updated' }); backend.emit({ type: 'todo_updated', sessionId: 'other-parent' }); });
    expect(reads).toBe(1);
    await act(async () => { backend.emit({ type: 'todo_updated', sessionId: SESSION }); });
    await act(async () => { backend.emit({ type: 'todo_updated', sessionId: SESSION }); });
    expect(text(tree)).toContain('Latest');
    await act(async () => { old.resolve(ok('get_state', { todoPhases: [{ name: 'Plan', tasks: [task('task-1', 'Stale', 'in_progress')] }] })); });
    expect(text(tree)).toContain('Latest');
    expect(text(tree)).not.toContain('Stale');
  });
});

it('invalidates pending actions and refreshes on owner change without clearing the new owner draft', async () => {
  const action = deferred<RpcResponse>();
  const stale = deferred<RpcResponse>();
  let oldReads = 0;
  const backend = harness((command, sessionId) => {
    if (command.type === 'todo_child_list') return ok(command.type, []);
    if (command.type === 'todo_op') {
      expect(sessionId).toBe(SESSION);
      expect(command).toEqual({ type: 'todo_op', op: 'append', phase: 'Plan', items: ['Same draft'] });
      return action.promise;
    }
    if (sessionId === SESSION && ++oldReads === 2) return stale.promise;
    return ok(command.type, { todoPhases: [{ name: 'Plan', tasks: [task(`task-${sessionId}`, sessionId === SESSION ? 'Old owner' : 'New owner', 'in_progress')] }] });
  });
  await mount(backend.client, async (tree, errors) => {
    await act(async () => { input(tree, 'Phase').props.onChangeText('Plan'); input(tree, 'New task').props.onChangeText('Same draft'); });
    await act(async () => { button(tree, 'Refresh Todo').props.onPress(); button(tree, 'Add task').props.onPress(); });
    await act(async () => { tree.update(<TodoPanel palette={DARK} client={backend.client} sessionId="todo-parent-8" onClose={() => {}} onError={errors} />); });
    await act(async () => { input(tree, 'New task').props.onChangeText('Same draft'); });
    await act(async () => {
      action.resolve(ok('todo_op', { phases: [{ name: 'Plan', tasks: [task('old-task', 'Old owner', 'in_progress'), task('old-added-task', 'Same draft')] }], completedTasks: [], summary: 'Updated' }));
      stale.resolve(ok('get_state', { todoPhases: [{ name: 'Plan', tasks: [task('stale-task', 'Stale owner', 'in_progress')] }] }));
    });
    expect(text(tree)).toContain('New owner');
    expect(text(tree)).not.toContain('Stale owner');
    expect(input(tree, 'New task').props.value).toBe('Same draft');
    expect(errors).not.toHaveBeenCalled();
    expect(backend.command.mock.calls.filter(([command, sessionId]) => command.type === 'get_state' && sessionId === 'todo-parent-8')).toHaveLength(1);
    expect(oldReads).toBe(2);
  });
});

const CHILD = 'child-1';
const childTask = {
  id: 'task-1', content: 'port the reader', dependsOn: [], status: 'running',
  agent: 'Worker', role: 'worker', createdAt: 5, summary: 'work in progress',
};
const childReceipt = (id = CHILD) => ({
  id: 'receipt-1', parentSessionId: SESSION, childSessionId: id, messageId: 'message-1',
  acceptedAt: 6, status: 'running', summary: null,
});
const childEntry = (status = 'running', id = CHILD, label = 'Ship child') => ({
  childSessionId: id, label, createdAt: 1, status, parentEntryId: 'entry-1', parentRevision: 2, kind: 'todo' as const,
});
const childStatus = (status = 'running', id = CHILD, revision = 4) => ({
  parentSessionId: SESSION, childSessionId: id, status, isRunning: status === 'running',
  transcriptEntries: 1, revision, label: 'Ship child', createdAt: 1, lastError: null,
  tasks: [childTask], receipts: [childReceipt(id)],
});
const childDetail = (status = 'running', id = CHILD, revision = 4) => ({
  parentSessionId: SESSION, childSessionId: id, todoId: id,
  parent: { sessionId: SESSION, entryId: 'entry-1', revision: 2 },
  workspace: { workspace: 'workspace-1', cwd: '/workspace' },
  status, isRunning: status === 'running', revision, label: 'Ship child', createdAt: 1, lastError: null,
  transcriptEntries: 1, tasks: [childTask], receipts: [childReceipt(id)],
  supervisor: { id: 'supervisor-1', displayName: 'Main', role: 'supervisor', status: 'idle', taskId: null },
  agents: [{ id: 'agent-1', displayName: 'Worker', role: 'worker', status: 'running', taskId: 'task-1' }],
  jobs: [{ jobId: 'job-1', displayName: 'Worker', role: 'worker', status: 'running', taskId: 'task-1', summary: 'work in progress' }],
  activity: [{ atMs: 7, actor: 'Worker', kind: 'task', text: 'started' }],
  irc: [{ messageId: 'message-2', from: 'Worker', to: 'Main', text: 'hello', atMs: 8, replyTo: null }],
  transcript: [{ id: 'entry-1', role: 'assistant', actor: 'Worker', text: 'working', atMs: 9, interrupted: false }],
});
const created = (id: string) => ({
  identity: { parentSessionId: SESSION, childSessionId: id },
  snapshot: { parentSessionId: SESSION, childSessionId: id },
});

function childHarness(
  entries: ReturnType<typeof childEntry>[],
  extras: (command: RunnerCommand, sessionId?: string) => RpcResponse | Promise<RpcResponse> | undefined = () => undefined,
) {
  let listed = entries;
  return harness((command, sessionId) => {
    const override = extras(command, sessionId);
    if (override !== undefined) return override;
    expect(sessionId).toBe(SESSION);
    expect(parseRunnerCommand(command)).toEqual(command);
    if (command.type === 'get_state') return ok(command.type, { sessionId: SESSION, todoPhases: [] });
    if (command.type === 'todo_child_list') {
      expect(command.parentSessionId).toBe(SESSION);
      return ok(command.type, listed);
    }
    if (command.type === 'todo_child_status') {
      expect(command.parentSessionId).toBe(SESSION);
      const id = listed[0]?.childSessionId ?? CHILD;
      return ok(command.type, childStatus(listed[0]?.status, id));
    }
    if (command.type === 'todo_child_detail') {
      expect(command).toEqual({ type: 'todo_child_detail', parentSessionId: SESSION, childSessionId: command.childSessionId });
      const row = listed.find((entry) => entry.childSessionId === command.childSessionId) ?? listed[0];
      return ok(command.type, childDetail(row?.status, command.childSessionId));
    }
    if (command.type === 'todo_child_select') return ok(command.type, {});
    throw new Error(`Unexpected command ${command.type}`);
  });
}

it('lists children, creates a labeled child, and selects it by id', async () => {
  let listed = [childEntry('idle')];
  const backend = childHarness(listed, (command) => {
    if (command.type === 'todo_child_create') {
      expect(command).toEqual({ type: 'todo_child_create', label: 'Next child' });
      listed = [childEntry('idle'), childEntry('created', 'child-2', 'Next child')];
      return ok(command.type, created('child-2'));
    }
    if (command.type === 'todo_child_list') return ok(command.type, listed);
    if (command.type === 'todo_child_select') {
      expect(command).toEqual({ type: 'todo_child_select', parentSessionId: SESSION, childSessionId: 'child-2' });
      return ok(command.type, {});
    }
    if (command.type === 'todo_child_detail' && command.childSessionId === 'child-2') {
      return ok(command.type, childDetail('created', 'child-2'));
    }
    return undefined;
  });
  await mount(backend.client, async (tree) => {
    expect(text(tree)).toContain('Ship child');
    expect(text(tree)).toContain('1 child');
    expect(text(tree)).toContain('port the reader');
    await act(async () => { input(tree, 'Child label').props.onChangeText(' Next child '); });
    await act(async () => { button(tree, 'Create child').props.onPress(); });
    expect(input(tree, 'Child label').props.value).toBe('');
    expect(text(tree)).toContain('Next child');
    expect(text(tree)).toContain('2 children');
    expect(backend.command.mock.calls.filter(([command]) => command.type === 'todo_child_create' || command.type === 'todo_child_select').map(([command]) => command)).toEqual([
      { type: 'todo_child_create', label: 'Next child' },
      { type: 'todo_child_select', parentSessionId: SESSION, childSessionId: 'child-2' },
    ]);
  });
});

it('sends a parent message, adds a revisioned task, and delivers a hub receipt', async () => {
  const backend = childHarness([childEntry('running')], (command) => {
    if (command.type === 'todo_child_send') {
      expect(command).toEqual({ type: 'todo_child_send', parentSessionId: SESSION, childSessionId: CHILD, message: 'keep going' });
      return ok(command.type, {});
    }
    if (command.type === 'todo_child_add_task') {
      expect(command).toEqual({
        type: 'todo_child_add_task', parentSessionId: SESSION, childSessionId: CHILD,
        content: 'follow up', dependsOn: ['task-1'], agent: 'Worker', role: 'writer', expectedRevision: 4,
      });
      return ok(command.type, {});
    }
    if (command.type === 'todo_child_hub_send') {
      expect(command).toEqual({
        type: 'todo_child_hub_send', parentSessionId: SESSION, childSessionId: CHILD,
        to: 'Worker', body: 'ping', replyTo: 'message-2',
      });
      return ok(command.type, [{ to: 'Worker', outcome: 'ok', messageId: 'hub-1', error: null }]);
    }
    return undefined;
  });
  await mount(backend.client, async (tree) => {
    await act(async () => { input(tree, 'Child message').props.onChangeText(' keep going '); });
    await act(async () => { button(tree, 'Send child').props.onPress(); });
    expect(input(tree, 'Child message').props.value).toBe('');
    await act(async () => {
      input(tree, 'Child task').props.onChangeText('follow up');
      input(tree, 'Child depends').props.onChangeText('task-1');
      input(tree, 'Child agent').props.onChangeText('Worker');
      input(tree, 'Child role').props.onChangeText('writer');
    });
    await act(async () => { button(tree, 'Add child task').props.onPress(); });
    expect(input(tree, 'Child task').props.value).toBe('');
    await act(async () => { button(tree, 'Hub recipient Worker').props.onPress(); });
    await act(async () => {
      input(tree, 'Hub message').props.onChangeText('ping');
      input(tree, 'Hub reply').props.onChangeText('message-2');
    });
    await act(async () => { button(tree, 'Send hub').props.onPress(); });
    expect(input(tree, 'Hub message').props.value).toBe('');
    expect(text(tree)).toContain('Worker · ok · hub-1');
  });
});

it('gates lifecycle actions and archives the selected child', async () => {
  let listed = [childEntry('running'), childEntry('stopped', 'child-2', 'Parked')];
  const backend = childHarness(listed, (command) => {
    if (command.type === 'todo_child_list') return ok(command.type, listed);
    if (command.type === 'todo_child_pause') {
      expect(command).toEqual({ type: 'todo_child_pause', parentSessionId: SESSION, childSessionId: CHILD });
      listed = [childEntry('paused'), listed[1]];
      return ok(command.type, childStatus('paused'));
    }
    if (command.type === 'todo_child_resume') {
      listed = [childEntry('running'), listed[1]];
      return ok(command.type, childStatus('running', CHILD, 5));
    }
    if (command.type === 'todo_child_stop') {
      listed = [childEntry('stopped'), listed[1]];
      return ok(command.type, childStatus('stopped', CHILD, 6));
    }
    if (command.type === 'todo_child_cancel') {
      expect(command).toEqual({ type: 'todo_child_cancel', parentSessionId: SESSION, childSessionId: CHILD });
      return ok(command.type, childStatus('running', CHILD, 5));
    }
    if (command.type === 'todo_child_archive') {
      expect(command).toEqual({ type: 'todo_child_archive', parentSessionId: SESSION, childSessionId: CHILD });
      listed = [listed[1]];
      return ok(command.type, {});
    }
    return undefined;
  });
  await mount(backend.client, async (tree) => {
    expect(button(tree, 'Pause child').props.disabled).toBe(false);
    expect(button(tree, 'Resume child').props.disabled).toBe(true);
    expect(button(tree, 'Archive child').props.disabled).toBe(true);
    await act(async () => { button(tree, 'Pause child').props.onPress(); });
    expect(button(tree, 'Resume child').props.disabled).toBe(false);
    await act(async () => { button(tree, 'Resume child').props.onPress(); });
    await act(async () => { button(tree, 'Cancel child').props.onPress(); });
    await act(async () => { button(tree, 'Stop child').props.onPress(); });
    expect(button(tree, 'Send child').props.disabled).toBe(true);
    expect(button(tree, 'Archive child').props.disabled).toBe(false);
    await act(async () => { button(tree, 'Archive child').props.onPress(); });
    expect(text(tree)).not.toContain('Ship child');
    expect(text(tree)).toContain('Parked');
    expect(backend.command.mock.calls.filter(([command]) => [
      'todo_child_pause', 'todo_child_resume', 'todo_child_cancel', 'todo_child_stop', 'todo_child_archive',
    ].includes(command.type)).map(([command]) => command.type)).toEqual([
      'todo_child_pause', 'todo_child_resume', 'todo_child_cancel', 'todo_child_stop', 'todo_child_archive',
    ]);
  });
});

it('keeps last-good children across refused and malformed lists', async () => {
  let listFailure: 'none' | 'refused' | 'malformed' = 'none';
  const backend = childHarness([childEntry('running')], (command) => {
    if (command.type !== 'todo_child_list') return undefined;
    if (listFailure === 'refused') return { type: 'response', command: command.type, success: false, error: 'List denied' };
    if (listFailure === 'malformed') return ok(command.type, [{ childSessionId: 'broken' }]);
    return ok(command.type, [childEntry('running')]);
  });
  await mount(backend.client, async (tree, errors) => {
    expect(text(tree)).toContain('Ship child');
    for (const failure of ['refused', 'malformed'] as const) {
      listFailure = failure;
      await act(async () => { backend.emit({ type: 'todo_child_routed', sessionId: SESSION }); });
      expect(text(tree)).toContain('Ship child');
    }
    expect(errors.mock.calls.map(([message]) => message)).toEqual([
      'Refresh child list: List denied',
      'Refresh child list: invalid todo_child_list response',
    ]);
  });
});

it('ignores foreign child frames and a stale list after a newer one', async () => {
  const late = deferred<RpcResponse>();
  let lists = 0;
  const backend = childHarness([childEntry('running')], (command) => {
    if (command.type !== 'todo_child_list') return undefined;
    lists += 1;
    if (lists === 2) return late.promise;
    return ok(command.type, [childEntry(lists === 1 ? 'running' : 'paused', CHILD, lists === 1 ? 'Ship child' : 'Latest child')]);
  });
  await mount(backend.client, async (tree) => {
    expect(text(tree)).toContain('Ship child');
    const before = lists;
    await act(async () => {
      backend.emit({ type: 'todo_child_routed' });
      backend.emit({ type: 'todo_child_routed', sessionId: 'other-parent' });
      backend.emit({ type: 'todo_updated', sessionId: 'other-parent' });
    });
    expect(lists).toBe(before);
    await act(async () => { backend.emit({ type: 'todo_child_routed', sessionId: SESSION }); });
    await act(async () => { backend.emit({ type: 'todo_updated', sessionId: SESSION }); });
    expect(text(tree)).toContain('Latest child');
    await act(async () => { late.resolve(ok('todo_child_list', [childEntry('failed', CHILD, 'Stale child')])); });
    expect(text(tree)).toContain('Latest child');
    expect(text(tree)).not.toContain('Stale child');
  });
});

it('keeps create single-flight and drops a late create after session change', async () => {
  const action = deferred<RpcResponse>();
  const backend = childHarness([], (command, sessionId) => {
    if (command.type === 'todo_child_create') {
      expect(sessionId).toBe(SESSION);
      return action.promise;
    }
    if (command.type === 'todo_child_list') return ok(command.type, []);
    if (command.type === 'get_state') return ok(command.type, { sessionId: sessionId ?? SESSION, todoPhases: [] });
    return undefined;
  });
  await mount(backend.client, async (tree, errors) => {
    await act(async () => { input(tree, 'Child label').props.onChangeText('Held'); });
    const create = button(tree, 'Create child').props.onPress;
    await act(async () => { create(); create(); });
    expect(backend.command.mock.calls.filter(([command]) => command.type === 'todo_child_create')).toHaveLength(1);
    expect(button(tree, 'Create child').props.disabled).toBe(true);
    await act(async () => { tree.update(<TodoPanel palette={DARK} client={backend.client} sessionId="todo-parent-8" onClose={() => {}} onError={errors} />); });
    await act(async () => { action.resolve(ok('todo_child_create', created(CHILD))); });
    expect(text(tree)).toContain('No Todo children.');
    expect(text(tree)).not.toContain('Ship child');
    expect(errors).not.toHaveBeenCalled();
  });
});

it('reports a malformed create and keeps the draft without selecting a child', async () => {
  const backend = childHarness([], (command) => {
    if (command.type === 'todo_child_create') {
      return ok(command.type, { identity: { parentSessionId: SESSION, childSessionId: CHILD }, snapshot: { parentSessionId: SESSION, childSessionId: 'other' } });
    }
    return undefined;
  });
  await mount(backend.client, async (tree, errors) => {
    await act(async () => { input(tree, 'Child label').props.onChangeText('Keep me'); });
    await act(async () => { button(tree, 'Create child').props.onPress(); });
    expect(input(tree, 'Child label').props.value).toBe('Keep me');
    expect(button(tree, 'Select child Keep me')).toBeUndefined();
    expect(errors).toHaveBeenLastCalledWith('Create child: invalid todo_child_create response');
  });
});

it('keeps a hub draft when a receipt fails', async () => {
  const backend = childHarness([childEntry('running')], (command) => {
    if (command.type === 'todo_child_hub_send') {
      return ok(command.type, [{ to: 'Worker', outcome: 'failed', messageId: null, error: 'offline' }]);
    }
    return undefined;
  });
  await mount(backend.client, async (tree, errors) => {
    await act(async () => {
      input(tree, 'Hub recipient').props.onChangeText('Worker');
      input(tree, 'Hub message').props.onChangeText('stay');
    });
    await act(async () => { button(tree, 'Send hub').props.onPress(); });
    expect(input(tree, 'Hub message').props.value).toBe('stay');
    expect(text(tree)).toContain('Worker · failed · offline');
    expect(errors).toHaveBeenLastCalledWith('Hub child: offline');
  });
});
