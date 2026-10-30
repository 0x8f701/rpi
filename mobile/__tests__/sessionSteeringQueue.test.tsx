import React from 'react';
import { Text, TextInput, TouchableOpacity, View } from 'react-native';
import renderer, { act } from 'react-test-renderer';

let mockFrame: (frame: Record<string, unknown>) => void;
let mockReconnect: () => void;
const defaultCommand = async (command: { type: string }) => ({
  success: true,
  data: command.type === 'session_list' ? { sessions: [
    { sessionId: 's2', runtimeIncarnation: 'inc-2', path: '/s2.jsonl', name: 'Beta', cwd: '/w', status: 'native', loaded: true },
  ] } : command.type === 'switch_session' ? { sessionId: 's2', runtimeIncarnation: 'inc-2' }
    : command.type === 'queue_list' ? { steering: [], followUp: [], total: 0 }
      : command.type === 'get_messages' ? { messages: [] } : {},
});
const mockClient = {
  state: 'connected',
  probe: { sessionId: 's1', runtimeIncarnation: 'inc-1' },
  command: jest.fn(defaultCommand),
  subscribe: jest.fn(),
  subscribeFrames: jest.fn((callback: typeof mockFrame) => { mockFrame = callback; return () => {}; }),
  subscribeReconnect: jest.fn((callback: () => void) => { mockReconnect = callback; return () => {}; }),
  hasOpenSocket: () => true,
  resumeFromBackground: jest.fn(),
};

jest.mock('../src/runtime', () => ({
  registry: { client: () => mockClient, snapshot: () => [] },
  mappingStore: {},
  lanDiscovery: {},
}));
jest.mock('react-native-webview', () => {
  const ReactModule = require('react');
  const { View: NativeView } = require('react-native');
  return { WebView: ReactModule.forwardRef((_props: unknown, ref: unknown) => {
    ReactModule.useImperativeHandle(ref, () => ({ injectJavaScript: () => {} }));
    return ReactModule.createElement(NativeView, { testID: 'picker' });
  }) };
});

import { SessionWorkspaceScreen } from '../src/screens/SessionWorkspaceScreen';
import { steeringPreviewsFromMessages } from '../src/steeringQueue';
import { DARK } from '../src/ui';

let tree: renderer.ReactTestRenderer;
const onError = jest.fn();
const screen = () => <SessionWorkspaceScreen palette={DARK} runnerId="r1" onError={onError} />;
const byId = (id: string) => tree.root.findAllByType(View).filter((node) => node.props.testID === id);
const strip = () => byId('activity-strip');
const queue = () => byId('steering-queue');
const row = () => byId('composer-status-row');
const action = (label: string) => tree.root.findAllByType(TouchableOpacity)
  .filter((node) => node.props.accessibilityLabel === label);

async function emit(frame: Record<string, unknown>) {
  await act(async () => mockFrame({ sessionId: 's1', runtimeIncarnation: 'inc-1', turnId: 't1', ...frame }));
}

beforeEach(async () => {
  mockClient.state = 'connected';
  mockClient.command.mockReset();
  mockClient.command.mockImplementation(defaultCommand);
  onError.mockClear();
  await act(async () => { tree = renderer.create(screen()); });
});
afterEach(async () => { await act(async () => tree.unmount()); });

it('keeps Working on activity-strip after the status row mounts', async () => {
  expect(row()).toHaveLength(0);
  await emit({ type: 'turn_started' });
  expect(strip()).toHaveLength(1);
  expect(strip()[0].props.accessibilityLabel).toBe('Working…');
  expect(strip()[0].findByType(Text).props.numberOfLines).toBe(1);
  expect(row()).toHaveLength(1);
  expect(queue()).toHaveLength(0);
});

it('paints steering previews from queue_update user messages', async () => {
  await emit({
    type: 'queue_update',
    steering: [
      { role: 'user', content: 'fix the test' },
      { role: 'assistant', content: 'ignored' },
      { role: 'user', content: [{ type: 'text', text: 'second\nline' }] },
    ],
  });
  expect(queue()).toHaveLength(1);
  expect(row()).toHaveLength(1);
  const labels = queue()[0].findAllByType(Text).map((node) => node.props.children);
  expect(labels).toContain('Steering');
  expect(labels).toContain('fix the test');
  expect(labels).toContain('second line');
});

it('Send all now and Remove fire queue_send_now and queue_cancel', async () => {
  await emit({
    type: 'queue_update',
    steering: [{ role: 'user', content: 'queued steer' }],
  });
  mockClient.command.mockClear();
  const sendNow = action('Send all now');
  expect(sendNow).toHaveLength(1);
  expect(sendNow[0].props.style.minWidth).toBe(44);
  expect(sendNow[0].props.style.minHeight).toBe(44);
  await act(async () => { await sendNow[0].props.onPress(); });
  expect(mockClient.command.mock.calls.map((call) => call[0])).toEqual([
    { type: 'queue_send_now' },
  ]);

  mockClient.command.mockClear();
  const remove = action('Remove steering message');
  expect(remove).toHaveLength(1);
  expect(remove[0].props.style.minWidth).toBe(44);
  expect(remove[0].props.style.minHeight).toBe(44);
  await act(async () => { await remove[0].props.onPress(); });
  expect(mockClient.command.mock.calls.map((call) => call[0])).toEqual([
    { type: 'queue_cancel', scope: 'steering', index: 0 },
  ]);
  expect(queue()).toHaveLength(0);
});

it('keeps the queue row when queue_cancel is refused', async () => {
  await emit({
    type: 'queue_update',
    steering: [{ role: 'user', content: 'keep me' }],
  });
  mockClient.command.mockImplementationOnce(async (command: { type: string }) => {
    if (command.type === 'queue_cancel') return { success: false, error: 'refused' };
    return { success: true, data: {} };
  });
  const remove = action('Remove steering message');
  expect(remove).toHaveLength(1);
  await act(async () => { await remove[0].props.onPress(); });
  expect(queue()).toHaveLength(1);
  expect(queue()[0].findAllByType(Text).map((node) => node.props.children)).toContain('keep me');
  expect(onError).toHaveBeenCalledWith('refused');
  mockClient.command.mockClear();
  onError.mockClear();
  mockClient.command.mockImplementationOnce(async (command: { type: string }) => {
    if (command.type === 'queue_cancel') return { success: false, error: 'refused again' };
    return { success: true, data: {} };
  });
  await act(async () => { await action('Remove steering message')[0].props.onPress(); });
  expect(mockClient.command.mock.calls.map((call) => call[0])).toEqual([
    { type: 'queue_cancel', scope: 'steering', index: 0 },
  ]);
  expect(queue()).toHaveLength(1);
  expect(queue()[0].findAllByType(Text).map((node) => node.props.children)).toContain('keep me');
  expect(onError).toHaveBeenCalledWith('refused again');
});

it('keeps the queue row when queue_send_now is refused', async () => {
  await emit({
    type: 'queue_update',
    steering: [{ role: 'user', content: 'still queued' }],
  });
  mockClient.command.mockImplementationOnce(async (command: { type: string }) => {
    if (command.type === 'queue_send_now') return { success: false, error: 'busy' };
    return { success: true, data: {} };
  });
  const sendNow = action('Send all now');
  await act(async () => { await sendNow[0].props.onPress(); });
  expect(queue()).toHaveLength(1);
  expect(queue()[0].findAllByType(Text).map((node) => node.props.children)).toContain('still queued');
  expect(onError).toHaveBeenCalledWith('busy');
});

it('keeps the queue row when queue_cancel throws', async () => {
  await emit({
    type: 'queue_update',
    steering: [{ role: 'user', content: 'keep after throw' }],
  });
  mockClient.command.mockImplementationOnce(async (command: { type: string }) => {
    if (command.type === 'queue_cancel') throw new Error('disconnected');
    return { success: true, data: {} };
  });
  const remove = action('Remove steering message');
  await act(async () => { await remove[0].props.onPress(); });
  expect(queue()).toHaveLength(1);
  expect(queue()[0].findAllByType(Text).map((node) => node.props.children)).toContain('keep after throw');
  expect(onError).toHaveBeenCalledWith('disconnected');
});

it('sends only one overlapping queue_cancel and unlocks after settle', async () => {
  await emit({
    type: 'queue_update',
    steering: [
      { role: 'user', content: 'A' },
      { role: 'user', content: 'B' },
      { role: 'user', content: 'C' },
    ],
  });
  let finish!: (value: { success: boolean; data?: unknown; error?: string }) => void;
  const pending = new Promise<{ success: boolean; data?: unknown; error?: string }>((resolve) => {
    finish = resolve;
  });
  mockClient.command.mockImplementation(async (command: { type: string }) => {
    if (command.type === 'queue_cancel') return pending;
    return defaultCommand(command);
  });
  mockClient.command.mockClear();
  const remove = action('Remove steering message');
  expect(remove).toHaveLength(3);
  expect(remove[0].props.style.minWidth).toBe(44);
  expect(remove[0].props.style.minHeight).toBe(44);
  await act(async () => {
    remove[0].props.onPress();
    remove[1].props.onPress();
  });
  expect(mockClient.command.mock.calls.map((call) => call[0])).toEqual([
    { type: 'queue_cancel', scope: 'steering', index: 0 },
  ]);
  expect(action('Remove steering message')[0].props.disabled).toBe(true);
  expect(action('Send all now')[0].props.disabled).toBe(true);
  await act(async () => {
    finish({ success: true });
  });
  const labels = queue()[0].findAllByType(Text).map((node) => node.props.children);
  expect(labels).not.toContain('A');
  expect(labels).toContain('B');
  expect(labels).toContain('C');
  expect(action('Remove steering message')[0].props.disabled).toBe(false);
  mockClient.command.mockClear();
  mockClient.command.mockImplementation(defaultCommand);
  await act(async () => { await action('Remove steering message')[0].props.onPress(); });
  expect(mockClient.command.mock.calls.map((call) => call[0])).toEqual([
    { type: 'queue_cancel', scope: 'steering', index: 0 },
  ]);
});

it('does not drop a newer queue_update after a late cancel ACK', async () => {
  await emit({
    type: 'queue_update',
    steering: [{ role: 'user', content: 'old' }],
  });
  let finish!: (value: { success: boolean; data?: unknown; error?: string }) => void;
  const pending = new Promise<{ success: boolean; data?: unknown; error?: string }>((resolve) => {
    finish = resolve;
  });
  mockClient.command.mockImplementation(async (command: { type: string }) => {
    if (command.type === 'queue_cancel') return pending;
    return defaultCommand(command);
  });
  const remove = action('Remove steering message');
  let pressDone: Promise<void> | undefined;
  await act(async () => {
    pressDone = Promise.resolve(remove[0].props.onPress());
  });
  await emit({
    type: 'queue_update',
    steering: [
      { role: 'user', content: 'old' },
      { role: 'user', content: 'extra' },
    ],
  });
  await act(async () => {
    finish({ success: true });
    await pressDone;
  });
  expect(queue()).toHaveLength(1);
  const labels = queue()[0].findAllByType(Text).map((node) => node.props.children);
  expect(labels).toContain('old');
  expect(labels).toContain('extra');
  expect(onError).not.toHaveBeenCalled();
});

it('places Working left and Steering right on composer-status-row', async () => {
  await emit({ type: 'turn_started' });
  await emit({
    type: 'queue_update',
    steering: [{ role: 'user', content: 'nudge the agent' }],
  });
  expect(row()).toHaveLength(1);
  expect(strip()).toHaveLength(1);
  expect(queue()).toHaveLength(1);
  expect(strip()[0].props.accessibilityLabel).toBe('Working…');
  expect(row()[0].props.style.flexDirection).toBe('row');
  expect(row()[0].props.style.alignItems).toBe('flex-start');
  expect(strip()[0].props.style.maxWidth).toBe('42%');
});

it('parses queue_update previews from user messages only', () => {
  expect(steeringPreviewsFromMessages([
    { role: 'user', content: 'first' },
    { role: 'assistant', content: 'nope' },
    { role: 'user', content: [{ type: 'text', text: 'second\nline' }] },
  ])).toEqual(['first', 'second\nline']);
  expect(steeringPreviewsFromMessages(null)).toEqual([]);
});

it('does not resurrect consumed steering after a delayed steer ACK', async () => {
  let finish!: (value: { success: boolean }) => void;
  const pending = new Promise<{ success: boolean }>((resolve) => { finish = resolve; });
  mockClient.command.mockImplementation(async (command) => command.type === 'steer' ? pending : defaultCommand(command));
  await emit({ type: 'turn_started' });
  await act(async () => { tree.root.findByType(TextInput).props.onChangeText('consumed'); });
  await act(async () => { action('Steer')[0].props.onPress(); });
  expect(mockClient.command.mock.calls.map((call) => call[0])).toContainEqual({ type: 'steer', message: 'consumed' });
  await emit({ type: 'queue_update', steering: [{ role: 'user', content: 'consumed' }] });
  expect(action('Remove steering message')).toHaveLength(1);
  await emit({ type: 'queue_update', steering: [] });
  await act(async () => { finish({ success: true }); });
  expect(queue()).toHaveLength(0);
  expect(action('Send all now')).toHaveLength(0);
});

it('hydrates an existing queue on opening and restores its controls', async () => {
  await act(async () => { tree.unmount(); });
  mockClient.command.mockImplementation(async (command) => command.type === 'queue_list'
    ? { success: true, data: { steering: ['already queued'], followUp: [], total: 1 } }
    : defaultCommand(command));
  await act(async () => { tree = renderer.create(screen()); });
  expect(queue()[0].findAllByType(Text).map((node) => node.props.children)).toContain('already queued');
  expect(action('Send all now')[0].props.disabled).toBe(false);
  expect(action('Remove steering message')[0].props.disabled).toBe(false);
});

it('clears a missed drain on reconnect', async () => {
  await emit({ type: 'queue_update', steering: [{ role: 'user', content: 'drained offline' }] });
  expect(action('Remove steering message')).toHaveLength(1);
  await act(async () => { mockReconnect(); });
  expect(queue()).toHaveLength(0);
  expect(action('Send all now')).toHaveLength(0);
});

it('keeps a new queue event ahead of delayed hydration', async () => {
  let finish!: (value: { success: boolean; data: unknown }) => void;
  const pending = new Promise<{ success: boolean; data: unknown }>((resolve) => { finish = resolve; });
  mockClient.command.mockImplementation(async (command) => command.type === 'queue_list' ? pending : defaultCommand(command));
  mockClient.command.mockClear();
  await act(async () => { mockReconnect(); });
  expect(mockClient.command.mock.calls.map((call) => call[0])).toContainEqual({ type: 'queue_list' });
  await emit({ type: 'queue_update', steering: [{ role: 'user', content: 'new event' }] });
  await act(async () => { finish({ success: true, data: { steering: ['old query'], followUp: [], total: 1 } }); });
  const labels = queue()[0].findAllByType(Text).map((node) => node.props.children);
  expect(labels).toContain('new event');
  expect(labels).not.toContain('old query');
});

it('ignores hydration and queue events from a previous session owner', async () => {
  let finish!: (value: { success: boolean; data: unknown }) => void;
  const pending = new Promise<{ success: boolean; data: unknown }>((resolve) => { finish = resolve; });
  mockClient.command.mockImplementation(async (command, sessionId?: string) => command.type === 'queue_list' && sessionId === 's1'
    ? pending : defaultCommand(command));
  mockClient.command.mockClear();
  await act(async () => { mockReconnect(); });
  expect(mockClient.command.mock.calls.map((call) => call[0])).toContainEqual({ type: 'queue_list' });
  await act(async () => { action('Panels')[0].props.onPress(); });
  await act(async () => { action('sessions')[0].props.onPress(); });
  await act(async () => { action('Resume Beta')[0].props.onPress(); });
  await emit({ type: 'queue_update', sessionId: 's2', runtimeIncarnation: 'inc-2', steering: [{ role: 'user', content: 'Beta queue' }] });
  await act(async () => { finish({ success: true, data: { steering: ['old owner'], followUp: [], total: 1 } }); });
  await emit({ type: 'queue_update', steering: [{ role: 'user', content: 'foreign event' }] });
  await emit({ type: 'queue_update', sessionId: 's2', runtimeIncarnation: 'retired-inc', steering: [] });
  const labels = queue()[0].findAllByType(Text).map((node) => node.props.children);
  expect(labels).toContain('Beta queue');
  expect(labels).not.toContain('old owner');
  expect(labels).not.toContain('foreign event');
});

it.each([
  ['refusal', async () => ({ success: false, error: 'refused queue read' })],
  ['malformed response', async () => ({ success: true, data: { steering: [{}] } })],
  ['transport failure', async () => { throw new Error('queue read timed out'); }],
])('surfaces queue hydration %s while preserving last-good rows', async (_label, respond) => {
  await emit({ type: 'queue_update', steering: [{ role: 'user', content: 'last good' }] });
  mockClient.command.mockImplementation(async (command) => command.type === 'queue_list' ? respond() : defaultCommand(command));
  await act(async () => { mockReconnect(); });
  expect(onError).toHaveBeenCalledWith(expect.stringContaining('queue_list'));
  expect(queue()[0].findAllByType(Text).map((node) => node.props.children)).toContain('last good');
});

it.each(['switch', 'unmount'] as const)('retires refused and rejected hydration after %s', async (retire) => {
  let refuse!: (value: { success: boolean; error: string }) => void;
  let reject!: (error: Error) => void;
  const pendingRefusal = new Promise<{ success: boolean; error: string }>((resolve) => { refuse = resolve; });
  const pendingRejection = new Promise<never>((_resolve, fail) => { reject = fail; });
  let reads = 0;
  mockClient.command.mockImplementation(async (command, sessionId?: string) => {
    if (command.type === 'queue_list' && sessionId === 's1') return ++reads === 1 ? pendingRefusal : pendingRejection;
    return defaultCommand(command);
  });
  await act(async () => { mockReconnect(); });
  await act(async () => { mockReconnect(); });
  expect(reads).toBe(2);
  if (retire === 'switch') {
    await act(async () => { action('Panels')[0].props.onPress(); });
    await act(async () => { action('sessions')[0].props.onPress(); });
    await act(async () => { action('Resume Beta')[0].props.onPress(); });
    await emit({ type: 'queue_update', sessionId: 's2', runtimeIncarnation: 'inc-2', steering: [{ role: 'user', content: 'current authority' }] });
  } else {
    await act(async () => { tree.unmount(); });
  }
  onError.mockClear();
  await act(async () => {
    refuse({ success: false, error: 'retired refusal' });
    reject(new Error('retired timeout'));
  });
  expect(onError).not.toHaveBeenCalled();
  if (retire === 'switch') {
    expect(queue()[0].findAllByType(Text).map((node) => node.props.children)).toContain('current authority');
  }
});

it('surfaces a current hydration timeout even after a newer queue event', async () => {
  let reject!: (error: Error) => void;
  const pending = new Promise<never>((_resolve, fail) => { reject = fail; });
  mockClient.command.mockImplementation(async (command) => command.type === 'queue_list' ? pending : defaultCommand(command));
  mockClient.command.mockClear();
  await act(async () => { mockReconnect(); });
  expect(mockClient.command.mock.calls.map((call) => call[0])).toContainEqual({ type: 'queue_list' });
  await emit({ type: 'queue_update', steering: [{ role: 'user', content: 'new authority' }] });
  await act(async () => { reject(new Error('current timeout')); });
  expect(onError).toHaveBeenCalledWith(expect.stringContaining('current timeout'));
  expect(queue()[0].findAllByType(Text).map((node) => node.props.children)).toContain('new authority');
});
