import React from 'react';
import renderer, { act } from 'react-test-renderer';
import { Text, TextInput, TouchableOpacity } from 'react-native';
import { LoopPanel } from '../src/screens/LoopPanel';
import { parseRunnerCommand, type RunnerCommand, type RpcResponse } from '../src/protocol';
import type { RunnerClient } from '../src/runnerClient';
import { DARK } from '../src/ui';

const SESSION = 'loop-session-1';
const OTHER = 'loop-session-2';

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((complete) => { resolve = complete; });
  return { promise, resolve };
}

function ok(command: string, data: unknown): RpcResponse {
  return { type: 'response', command, success: true, data };
}

function fail(command: string, error: string): RpcResponse {
  return { type: 'response', command, success: false, error };
}

function loopTask(over: Record<string, unknown> = {}) {
  return {
    id: 'loop-abc123',
    intervalSecs: 300,
    prompt: 'check deploy',
    durable: false,
    createdAt: '2026-09-17T12:00:00Z',
    lastFiredAt: null,
    expiresAt: '2026-09-18T12:00:00Z',
    runCount: 0,
    ...over,
  };
}

function harness(handler: (command: RunnerCommand, sessionId?: string) => RpcResponse | Promise<RpcResponse>) {
  const listeners = new Set<(frame: unknown) => void>();
  const command = jest.fn(handler);
  const client = {
    command,
    subscribeFrames: (listener: (frame: unknown) => void) => {
      listeners.add(listener);
      return () => { listeners.delete(listener); };
    },
  } as unknown as RunnerClient;
  return { client, command, emit: (frame: unknown) => listeners.forEach((listener) => listener(frame)) };
}

function button(tree: renderer.ReactTestRenderer, label: string) {
  return tree.root.findAllByType(TouchableOpacity).find((node) => node.props.accessibilityLabel === label);
}

function input(tree: renderer.ReactTestRenderer, label: string) {
  return tree.root.findAllByType(TextInput).find((node) => node.props.accessibilityLabel === label)!;
}

function text(tree: renderer.ReactTestRenderer): string {
  const out: string[] = [];
  const walk = (value: unknown) => {
    if (typeof value === 'string') out.push(value);
    else if (Array.isArray(value)) value.forEach(walk);
  };
  tree.root.findAllByType(Text).forEach((node) => walk(node.props.children));
  return out.join(' ');
}

function commandsOf(command: jest.Mock, type: string) {
  return command.mock.calls.filter(([sent]: [RunnerCommand]) => sent.type === type);
}

async function mount(
  client: RunnerClient,
  run: (tree: renderer.ReactTestRenderer, errors: jest.Mock) => Promise<void>,
  sessionId = SESSION,
) {
  let tree!: renderer.ReactTestRenderer;
  const errors = jest.fn();
  try {
    await act(async () => {
      tree = renderer.create(
        <LoopPanel palette={DARK} client={client} sessionId={sessionId} onClose={() => {}} onError={errors} />,
      );
    });
    await run(tree, errors);
  } finally {
    if (tree) await act(async () => { tree.unmount(); });
  }
}

it('lists session-scoped loops and never invents pause or resume', async () => {
  const listed = [loopTask(), loopTask({ id: 'loop-def456', intervalSecs: 3600, prompt: 'hourly ping', runCount: 2 })];
  const backend = harness((command, sessionId) => {
    expect(sessionId).toBe(SESSION);
    expect(parseRunnerCommand(command)).toEqual(command);
    expect(command.type).toBe('loop_list');
    return ok('loop_list', listed);
  });
  await mount(backend.client, async (tree) => {
    expect(text(tree)).toContain('Active loops (2)');
    expect(text(tree)).toContain('check deploy');
    expect(text(tree)).toContain('hourly ping');
    expect(text(tree)).toContain('5m · every 5 minutes · scheduled');
    expect(text(tree)).toContain('1h · every 1 hour · scheduled');
    expect(text(tree).toLowerCase()).not.toMatch(/pause|resume/);
    expect(button(tree, 'Pause')).toBeUndefined();
    expect(button(tree, 'Resume')).toBeUndefined();
    expect(commandsOf(backend.command, 'loop_list')).toHaveLength(1);
    expect(backend.command.mock.calls[0][1]).toBe(SESSION);
  });
});

it('creates a loop with the TUI immediate wire and clears the matching draft', async () => {
  let listed: unknown[] = [];
  const backend = harness((command, sessionId) => {
    expect(sessionId).toBe(SESSION);
    expect(parseRunnerCommand(command)).toEqual(command);
    if (command.type === 'loop_create') {
      expect(command).toEqual({
        type: 'loop_create',
        interval: '1m',
        prompt: 'check deploy',
        fireImmediately: true,
        durable: false,
      });
      listed = [loopTask({ id: 'loop-new', intervalSecs: 60, prompt: 'check deploy' })];
      return ok('loop_create', listed[0]);
    }
    return ok('loop_list', listed);
  });
  await mount(backend.client, async (tree) => {
    expect(text(tree)).toContain('No active loops.');
    await act(async () => { button(tree, 'Create interval 1m')!.props.onPress(); });
    await act(async () => { input(tree, 'Loop prompt').props.onChangeText('check deploy'); });
    await act(async () => { button(tree, 'Create loop')!.props.onPress(); });
    expect(input(tree, 'Loop prompt').props.value).toBe('');
    expect(text(tree)).toContain('check deploy');
    expect(text(tree)).toContain('1m · every 1 minute · scheduled');
    expect(commandsOf(backend.command, 'loop_create')).toHaveLength(1);
  });
});

it('updates only supported interval and prompt fields for the selected task', async () => {
  let listed = [loopTask()];
  const backend = harness((command, sessionId) => {
    expect(sessionId).toBe(SESSION);
    expect(parseRunnerCommand(command)).toEqual(command);
    if (command.type === 'loop_update') {
      expect(command).toEqual({
        type: 'loop_update',
        taskId: 'loop-abc123',
        interval: '2h',
        prompt: 'rpc updated',
      });
      listed = [loopTask({ intervalSecs: 7200, prompt: 'rpc updated', runCount: 1 })];
      return ok('loop_update', listed[0]);
    }
    return ok('loop_list', listed);
  });
  await mount(backend.client, async (tree) => {
    await act(async () => { button(tree, 'Select loop loop-abc123')!.props.onPress(); });
    await act(async () => { input(tree, 'Update interval').props.onChangeText('2h'); });
    await act(async () => { input(tree, 'Update prompt').props.onChangeText('rpc updated'); });
    await act(async () => { button(tree, 'Update loop')!.props.onPress(); });
    expect(text(tree)).toContain('rpc updated');
    expect(text(tree)).toContain('2h · every 2 hours · scheduled');
    expect(commandsOf(backend.command, 'loop_update')).toHaveLength(1);
  });
});

it('requires confirmation before cancel current run and delete schedule', async () => {
  let listed = [loopTask({ id: 'loop-live' }), loopTask({ id: 'loop-keep', prompt: 'stay' })];
  const backend = harness((command) => {
    expect(parseRunnerCommand(command)).toEqual(command);
    if (command.type === 'loop_cancel') {
      expect(command).toEqual({ type: 'loop_cancel', taskId: 'loop-live' });
      listed = listed.filter((row) => row.id !== 'loop-live');
      return ok('loop_cancel', true);
    }
    if (command.type === 'loop_delete') {
      expect(command).toEqual({ type: 'loop_delete', taskId: 'loop-keep' });
      listed = [];
      return ok('loop_delete', true);
    }
    return ok('loop_list', listed);
  });
  await mount(backend.client, async (tree) => {
    expect(commandsOf(backend.command, 'loop_cancel')).toHaveLength(0);
    await act(async () => { button(tree, 'Cancel current run loop-live')!.props.onPress(); });
    expect(commandsOf(backend.command, 'loop_cancel')).toHaveLength(0);
    expect(text(tree)).toContain('Cancel run?');
    await act(async () => { button(tree, 'Confirm cancel current run loop-live')!.props.onPress(); });
    expect(commandsOf(backend.command, 'loop_cancel')).toHaveLength(1);
    expect(text(tree)).not.toContain('check deploy');
    expect(text(tree)).toContain('stay');

    await act(async () => { button(tree, 'Delete schedule loop-keep')!.props.onPress(); });
    expect(commandsOf(backend.command, 'loop_delete')).toHaveLength(0);
    expect(text(tree)).toContain('Delete schedule?');
    await act(async () => { button(tree, 'Confirm delete schedule loop-keep')!.props.onPress(); });
    expect(commandsOf(backend.command, 'loop_delete')).toHaveLength(1);
    expect(text(tree)).toContain('No active loops.');
  });
});

it('preserves draft and last-good list when create or refresh is refused', async () => {
  let refreshFailure: 'none' | 'refused' | 'malformed' | 'rejected' = 'none';
  const good = [loopTask({ prompt: 'Keep me' })];
  const backend = harness((command) => {
    if (command.type === 'loop_create') return fail('loop_create', 'Create denied');
    if (refreshFailure === 'refused') return fail('loop_list', 'List denied');
    if (refreshFailure === 'rejected') throw new Error('Disconnected');
    if (refreshFailure === 'malformed') return ok('loop_list', [{ id: 'broken' }]);
    return ok('loop_list', good);
  });
  await mount(backend.client, async (tree, errors) => {
    await act(async () => { input(tree, 'Loop prompt').props.onChangeText('Retry me'); });
    await act(async () => { button(tree, 'Create loop')!.props.onPress(); });
    expect(input(tree, 'Loop prompt').props.value).toBe('Retry me');
    expect(text(tree)).toContain('Keep me');
    expect(errors).toHaveBeenLastCalledWith('Create loop: Create denied');
    for (const failure of ['refused', 'malformed', 'rejected'] as const) {
      refreshFailure = failure;
      await act(async () => { button(tree, 'Refresh loops')!.props.onPress(); });
      expect(text(tree)).toContain('Keep me');
      expect(input(tree, 'Loop prompt').props.value).toBe('Retry me');
    }
    expect(errors.mock.calls.map(([message]) => message)).toEqual([
      'Create loop: Create denied',
      'Refresh loops: List denied',
      'Refresh loops: Invalid loop task',
      'Refresh loops: Disconnected',
    ]);
  });
});

it('single-flights create so a second press sends no RPC', async () => {
  const action = deferred<RpcResponse>();
  const listed = [loopTask()];
  const backend = harness((command) => command.type === 'loop_create' ? action.promise : ok('loop_list', listed));
  await mount(backend.client, async (tree) => {
    await act(async () => { input(tree, 'Loop prompt').props.onChangeText('once'); });
    const create = button(tree, 'Create loop')!.props.onPress;
    await act(async () => { create(); create(); button(tree, 'Cancel current run loop-abc123')!.props.onPress(); });
    expect(commandsOf(backend.command, 'loop_create')).toHaveLength(1);
    expect(commandsOf(backend.command, 'loop_cancel')).toHaveLength(0);
    expect(button(tree, 'Create loop')!.props.disabled).toBe(true);
    await act(async () => {
      action.resolve(ok('loop_create', loopTask({ id: 'loop-once', prompt: 'once', intervalSecs: 300 })));
    });
    expect(commandsOf(backend.command, 'loop_create')).toHaveLength(1);
  });
});

it('invalidates a late list after owner switch and keeps the new owner draft', async () => {
  const stale = deferred<RpcResponse>();
  const lateCreate = deferred<RpcResponse>();
  let oldReads = 0;
  const backend = harness((command, sessionId) => {
    if (command.type === 'loop_create') {
      expect(sessionId).toBe(SESSION);
      return lateCreate.promise;
    }
    if (sessionId === SESSION && ++oldReads === 2) return stale.promise;
    return ok('loop_list', [loopTask({
      id: `loop-${sessionId}`,
      prompt: sessionId === SESSION ? 'Old owner' : 'New owner',
    })]);
  });
  await mount(backend.client, async (tree, errors) => {
    expect(text(tree)).toContain('Old owner');
    await act(async () => { input(tree, 'Loop prompt').props.onChangeText('Same draft'); });
    await act(async () => {
      button(tree, 'Refresh loops')!.props.onPress();
      button(tree, 'Create loop')!.props.onPress();
    });
    await act(async () => {
      tree.update(
        <LoopPanel palette={DARK} client={backend.client} sessionId={OTHER} onClose={() => {}} onError={errors} />,
      );
    });
    await act(async () => { input(tree, 'Loop prompt').props.onChangeText('Same draft'); });
    await act(async () => {
      stale.resolve(ok('loop_list', [loopTask({ id: 'loop-stale', prompt: 'Stale owner' })]));
      lateCreate.resolve(ok('loop_create', loopTask({ id: 'loop-late', prompt: 'Late create' })));
    });
    expect(text(tree)).toContain('New owner');
    expect(text(tree)).not.toContain('Stale owner');
    expect(text(tree)).not.toContain('Late create');
    expect(input(tree, 'Loop prompt').props.value).toBe('Same draft');
    expect(errors).not.toHaveBeenCalled();
    expect(backend.command.mock.calls.filter(([, sessionId]) => sessionId === OTHER)).toHaveLength(1);
  });
});
