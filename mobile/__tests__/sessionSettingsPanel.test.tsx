import React from 'react';
import renderer, { act } from 'react-test-renderer';
import { Text } from 'react-native';
import type { RunnerClient } from '../src/runnerClient';
import { SessionSettingsPanel } from '../src/screens/SessionSettingsPanel';
import { DARK } from '../src/ui';

type Command = { type: string } & Record<string, unknown>;
const SESSION = 'settings-session';
const ok = (command: string, data: unknown) => ({ type: 'response', command, success: true, data });
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((complete, fail) => { resolve = complete; reject = fail; });
  return { promise, resolve, reject };
}
function response(command: Command): unknown {
  switch (command.type) {
    case 'get_available_models': return ok(command.type, { models: [{ id: 'model-a', provider: 'provider-a', name: 'Model Alpha' }, { id: 'model-b', provider: 'provider-b', name: '' }] });
    case 'get_available_thinking_levels': return ok(command.type, { levels: ['off', 'high'] });
    case 'set_model':
    case 'set_thinking_level':
    case 'set_steering_mode': return ok(command.type, null);
    case 'get_state': return ok(command.type, { steeringMode: 'all' });
    default: throw new Error(`Unexpected ${command.type}`);
  }
}
function mockClient(handler: (command: Command) => unknown = response) {
  const command = jest.fn(async (request: Command, _sessionId?: string) => handler(request));
  const client = { command, subscribeFrames: jest.fn(() => () => {}) } as unknown as RunnerClient;
  return { client, command };
}
const labeled = (tree: renderer.ReactTestRenderer, label: string) => tree.root.findByProps({ accessibilityLabel: label });
const text = (tree: renderer.ReactTestRenderer) => tree.root.findAllByType(Text).map((node) => node.props.children).flat().join('\n');
async function press(tree: renderer.ReactTestRenderer, label: string) {
  await act(async () => { labeled(tree, label).props.onPress(); });
}
async function withPanel(client: RunnerClient, run: (tree: renderer.ReactTestRenderer, errors: jest.Mock) => Promise<void>) {
  let tree!: renderer.ReactTestRenderer;
  const errors = jest.fn();
  try {
    await act(async () => { tree = renderer.create(<SessionSettingsPanel palette={DARK} client={client} sessionId={SESSION} onError={errors} onClose={() => {}} />); });
    await run(tree, errors);
  } finally {
    if (tree) await act(async () => { tree.unmount(); });
  }
}

async function mountPanel(client: RunnerClient, sessionId: string, errors: jest.Mock) {
  let tree!: renderer.ReactTestRenderer;
  await act(async () => { tree = renderer.create(<SessionSettingsPanel palette={DARK} client={client} sessionId={sessionId} onError={errors} onClose={() => {}} />); });
  return tree;
}

it('loads model and thinking catalogs sequentially in the panel session', async () => {
  const pending = deferred<unknown>();
  const { client, command } = mockClient((request) => request.type === 'get_available_models' ? pending.promise : response(request));
  await withPanel(client, async (tree, errors) => {
    expect(command.mock.calls).toEqual([[{ type: 'get_available_models' }, SESSION]]);
    await act(async () => { pending.resolve(response({ type: 'get_available_models' })); });
    expect(command.mock.calls).toEqual([
      [{ type: 'get_available_models' }, SESSION],
      [{ type: 'get_available_thinking_levels' }, SESSION],
      [{ type: 'get_state' }, SESSION],
    ]);
    expect(text(tree)).toContain('Model Alpha');
    expect(text(tree)).toContain('model-b');
    expect(text(tree)).toContain('high');
    expect(labeled(tree, 'Set model provider-a/model-a')).toBeDefined();
    expect(labeled(tree, 'Set thinking off')).toBeDefined();
    expect(errors).not.toHaveBeenCalled();
  });
});

it('sets model and thinking with the selected provider, model id and session', async () => {
  const { client, command } = mockClient();
  await withPanel(client, async (tree, errors) => {
    await press(tree, 'Set model provider-a/model-a');
    expect(command).toHaveBeenLastCalledWith({ type: 'set_model', provider: 'provider-a', modelId: 'model-a' }, SESSION);
    await press(tree, 'Set thinking high');
    expect(command).toHaveBeenLastCalledWith({ type: 'set_thinking_level', level: 'high' }, SESSION);
    expect(errors).not.toHaveBeenCalled();
  });
});

it('seeds Queue mode from get_state and does not no-op the first tap', async () => {
  const { client, command } = mockClient((request) => request.type === 'get_state'
    ? ok(request.type, { steeringMode: 'one-at-a-time' })
    : response(request));
  await withPanel(client, async (tree, errors) => {
    expect(labeled(tree, 'Queue mode').findByType(Text).props.children).toBe('one-at-a-time');
    await press(tree, 'Queue mode');
    expect(command).toHaveBeenLastCalledWith({ type: 'set_steering_mode', mode: 'all' }, SESSION);
    expect(labeled(tree, 'Queue mode').findByType(Text).props.children).toBe('all');
    expect(errors).not.toHaveBeenCalled();
  });
});

it('leaves Queue unknown and issues no toggle when get_state is refused', async () => {
  const { client, command } = mockClient((request) => request.type === 'get_state'
    ? { type: 'response', command: request.type, success: false, error: 'state refused' }
    : response(request));
  await withPanel(client, async (tree, errors) => {
    expect(labeled(tree, 'Queue mode').findByType(Text).props.children).toBe('--');
    expect(labeled(tree, 'Queue mode').props.disabled).toBe(true);
    await press(tree, 'Queue mode');
    expect(command.mock.calls.filter(([request]) => request.type === 'set_steering_mode')).toEqual([]);
    expect(errors.mock.calls).toEqual([['get_state failed: state refused']]);
  });
});

it('leaves Queue unknown and reports the failure when get_state rejects', async () => {
  const pending = deferred<unknown>();
  const { client, command } = mockClient((request) => request.type === 'get_state' ? pending.promise : response(request));
  await withPanel(client, async (tree, errors) => {
    expect(labeled(tree, 'Queue mode').findByType(Text).props.children).toBe('--');
    expect(labeled(tree, 'Queue mode').props.disabled).toBe(true);
    await act(async () => { pending.reject(new Error('state transport failed')); });
    expect(labeled(tree, 'Queue mode').findByType(Text).props.children).toBe('--');
    expect(labeled(tree, 'Queue mode').props.disabled).toBe(true);
    await press(tree, 'Queue mode');
    expect(command.mock.calls.filter(([request]) => request.type === 'set_steering_mode')).toEqual([]);
    expect(errors.mock.calls).toEqual([['get_state failed: state transport failed']]);
  });
});

it('leaves Queue unknown when get_state reports a mode the panel cannot act on', async () => {
  const { client, command } = mockClient((request) => request.type === 'get_state'
    ? ok(request.type, { steeringMode: 'sometimes' })
    : response(request));
  await withPanel(client, async (tree, errors) => {
    expect(labeled(tree, 'Queue mode').findByType(Text).props.children).toBe('--');
    expect(labeled(tree, 'Queue mode').props.disabled).toBe(true);
    await press(tree, 'Queue mode');
    expect(command.mock.calls.filter(([request]) => request.type === 'set_steering_mode')).toEqual([]);
    expect(errors.mock.calls).toEqual([['get_state failed: malformed steering mode']]);
  });
});

it('blocks Queue taps until get_state settles', async () => {
  const pending = deferred<unknown>();
  const { client, command } = mockClient((request) => request.type === 'get_state' ? pending.promise : response(request));
  await withPanel(client, async (tree) => {
    expect(labeled(tree, 'Queue mode').props.disabled).toBe(true);
    await press(tree, 'Queue mode');
    expect(command.mock.calls.map(([request]) => request.type)).toEqual([
      'get_available_models',
      'get_available_thinking_levels',
      'get_state',
    ]);
    await act(async () => { pending.resolve(ok('get_state', { steeringMode: 'one-at-a-time' })); });
    expect(labeled(tree, 'Queue mode').props.disabled).toBe(false);
    expect(labeled(tree, 'Queue mode').findByType(Text).props.children).toBe('one-at-a-time');
    await press(tree, 'Queue mode');
    expect(command).toHaveBeenLastCalledWith({ type: 'set_steering_mode', mode: 'all' }, SESSION);
  });
});

it('keeps the current session Queue mode when a superseded load settles late', async () => {
  const stale = deferred<unknown>();
  let staleGet = true;
  const { client, command } = mockClient((request) => {
    if (request.type === 'get_state' && staleGet) return stale.promise;
    return response(request);
  });
  await withPanel(client, async (tree, errors) => {
    expect(labeled(tree, 'Queue mode').props.disabled).toBe(true);
    staleGet = false;
    await act(async () => { tree.update(<SessionSettingsPanel palette={DARK} client={client} sessionId="next-session" onError={errors} onClose={() => {}} />); });
    expect(labeled(tree, 'Queue mode').findByType(Text).props.children).toBe('all');
    await act(async () => { stale.resolve(ok('get_state', { steeringMode: 'one-at-a-time' })); });
    expect(labeled(tree, 'Queue mode').findByType(Text).props.children).toBe('all');
    expect(labeled(tree, 'Queue mode').props.disabled).toBe(false);
    await press(tree, 'Queue mode');
    expect(command).toHaveBeenLastCalledWith({ type: 'set_steering_mode', mode: 'one-at-a-time' }, 'next-session');
    expect(errors).not.toHaveBeenCalled();
  });
});

it('retires a load unmounted with its catalog request still pending', async () => {
  const pending = deferred<unknown>();
  const { client, command } = mockClient((request) => request.type === 'get_available_thinking_levels' ? pending.promise : response(request));
  const errors = jest.fn();
  const tree = await mountPanel(client, SESSION, errors);
  expect(labeled(tree, 'Queue mode').props.disabled).toBe(true);
  await act(async () => { tree.unmount(); });
  const issued = command.mock.calls.length;
  await act(async () => { pending.resolve(response({ type: 'get_available_thinking_levels' })); });
  expect(command.mock.calls).toHaveLength(issued);
  expect(command.mock.calls.map(([request]) => request.type)).toEqual(['get_available_models', 'get_available_thinking_levels']);
  expect(errors).not.toHaveBeenCalled();
});

it('does not publish a retired get_state refusal after the panel is remounted', async () => {
  const pending = deferred<unknown>();
  let staleGet = true;
  const { client } = mockClient((request) => {
    if (request.type === 'get_state' && staleGet) return pending.promise;
    return response(request);
  });
  const errors = jest.fn();
  const first = await mountPanel(client, SESSION, errors);
  staleGet = false;
  await act(async () => { first.unmount(); });
  const second = await mountPanel(client, 'next-session', errors);
  expect(labeled(second, 'Queue mode').findByType(Text).props.children).toBe('all');
  await act(async () => { pending.resolve({ type: 'response', command: 'get_state', success: false, error: 'late refusal' }); });
  expect(errors).not.toHaveBeenCalled();
  expect(labeled(second, 'Queue mode').findByType(Text).props.children).toBe('all');
  expect(labeled(second, 'Queue mode').props.disabled).toBe(false);
  await act(async () => { second.unmount(); });
});

it('does not report a retired get_state rejection after unmount', async () => {
  const pending = deferred<unknown>();
  const { client, command } = mockClient((request) => request.type === 'get_state' ? pending.promise : response(request));
  const errors = jest.fn();
  const tree = await mountPanel(client, SESSION, errors);
  await act(async () => { tree.unmount(); });
  const issued = command.mock.calls.length;
  await act(async () => { pending.reject(new Error('late transport failure')); });
  expect(errors).not.toHaveBeenCalled();
  expect(command.mock.calls).toHaveLength(issued);
});

it('changes queue mode only on success and toggles back to all', async () => {
  const pending = deferred<unknown>();
  let updates = 0;
  const { client, command } = mockClient((request) => request.type === 'set_steering_mode' && ++updates === 1 ? pending.promise : response(request));
  await withPanel(client, async (tree) => {
    expect(labeled(tree, 'Queue mode').findByType(Text).props.children).toBe('all');
    await press(tree, 'Queue mode');
    expect(command).toHaveBeenLastCalledWith({ type: 'set_steering_mode', mode: 'one-at-a-time' }, SESSION);
    expect(labeled(tree, 'Queue mode').findByType(Text).props.children).toBe('all');
    await act(async () => { pending.resolve(ok('set_steering_mode', null)); });
    expect(labeled(tree, 'Queue mode').findByType(Text).props.children).toBe('one-at-a-time');
    await press(tree, 'Queue mode');
    expect(command).toHaveBeenLastCalledWith({ type: 'set_steering_mode', mode: 'all' }, SESSION);
    expect(labeled(tree, 'Queue mode').findByType(Text).props.children).toBe('all');
  });
});

it('keeps all and reports a refused queue-mode update', async () => {
  const { client, command } = mockClient((request) => request.type === 'set_steering_mode'
    ? { type: 'response', command: request.type, success: false, error: 'Queue refused' } : response(request));
  await withPanel(client, async (tree, errors) => {
    await press(tree, 'Queue mode');
    expect(command).toHaveBeenLastCalledWith({ type: 'set_steering_mode', mode: 'one-at-a-time' }, SESSION);
    expect(labeled(tree, 'Queue mode').findByType(Text).props.children).toBe('all');
    expect(errors).toHaveBeenCalledWith('Queue refused');
    expect(labeled(tree, 'Queue mode').props.disabled).toBe(false);
  });
});

it('clears both catalogs, forgets the Queue mode, and reports errors when reloading fails', async () => {
  let refused = false;
  const { client, command } = mockClient((request) => refused
    ? { type: 'response', command: request.type, success: false, error: `${request.type} refused` } : response(request));
  await withPanel(client, async (tree, errors) => {
    expect(text(tree)).toContain('Model Alpha');
    expect(text(tree)).toContain('high');
    refused = true;
    await act(async () => { tree.update(<SessionSettingsPanel palette={DARK} client={client} sessionId="other-session" onError={errors} onClose={() => {}} />); });
    expect(command.mock.calls.slice(-3)).toEqual([
      [{ type: 'get_available_models' }, 'other-session'],
      [{ type: 'get_available_thinking_levels' }, 'other-session'],
      [{ type: 'get_state' }, 'other-session'],
    ]);
    expect(tree.root.findAllByType(Text).filter((node) => node.props.children === '--')).toHaveLength(3);
    expect(text(tree)).not.toContain('Model Alpha');
    expect(text(tree)).not.toContain('high');
    expect(labeled(tree, 'Queue mode').props.disabled).toBe(true);
    expect(errors.mock.calls).toEqual([
      ['get_available_models refused'],
      ['get_available_thinking_levels refused'],
      ['get_state failed: get_state refused'],
    ]);
  });
});

it('shows empty copy for empty model and level arrays', async () => {
  const { client } = mockClient((request) => {
    if (request.type === 'get_available_models') return ok(request.type, { models: [] });
    if (request.type === 'get_available_thinking_levels') return ok(request.type, { levels: [] });
    if (request.type === 'get_state') return ok(request.type, { steeringMode: 'all' });
    return response(request);
  });
  await withPanel(client, async (tree, errors) => {
    expect(text(tree)).toContain('No models.');
    expect(text(tree)).toContain('No levels.');
    expect(text(tree)).not.toContain('--');
    expect(errors).not.toHaveBeenCalled();
  });
});

it('disables other actions and blocks thinking while a model change is in flight', async () => {
  const pending = deferred<unknown>();
  const { client, command } = mockClient((request) => request.type === 'set_model' ? pending.promise : response(request));
  await withPanel(client, async (tree) => {
    await press(tree, 'Set model provider-a/model-a');
    for (const label of ['Set model provider-a/model-a', 'Set model provider-b/model-b', 'Set thinking high', 'Queue mode']) {
      expect(labeled(tree, label).props.disabled).toBe(true);
    }
    await press(tree, 'Set thinking high');
    await press(tree, 'Queue mode');
    expect(command.mock.calls.map(([request]) => request.type)).toEqual(['get_available_models', 'get_available_thinking_levels', 'get_state', 'set_model']);
    await act(async () => { pending.resolve(ok('set_model', null)); });
    expect(labeled(tree, 'Set thinking high').props.disabled).toBe(false);
    expect(labeled(tree, 'Queue mode').props.disabled).toBe(false);
    await press(tree, 'Set thinking high');
    expect(command).toHaveBeenLastCalledWith({ type: 'set_thinking_level', level: 'high' }, SESSION);
  });
});
