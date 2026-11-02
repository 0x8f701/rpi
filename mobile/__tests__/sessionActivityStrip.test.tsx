import React from 'react';
import { Text, View } from 'react-native';
import renderer, { act } from 'react-test-renderer';

let mockFrame: (frame: Record<string, unknown>) => void;
const mockClient = {
  state: 'connected',
  probe: { sessionId: 's1', runtimeIncarnation: 'inc-1' },
  command: jest.fn(async (command: { type: string }) => ({
    success: true,
    data: command.type === 'session_list' ? { sessions: [
      { sessionId: 's2', runtimeIncarnation: 'inc-2', path: '/s2.jsonl', name: 'Beta', cwd: '/w', status: 'native', loaded: true },
    ] } : command.type === 'switch_session' ? { sessionId: 's2', runtimeIncarnation: 'inc-2' }
      : command.type === 'queue_list' ? { steering: [], followUp: [], total: 0 }
        : command.type === 'get_messages' ? { messages: [] } : {},
  })),
  subscribe: jest.fn(),
  subscribeFrames: jest.fn((callback: typeof mockFrame) => { mockFrame = callback; return () => {}; }),
  subscribeReconnect: jest.fn(() => () => {}),
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
import { DARK } from '../src/ui';

let tree: renderer.ReactTestRenderer;
const onError = jest.fn();
const screen = () => <SessionWorkspaceScreen palette={DARK} runnerId="r1" onError={onError} />;
const strip = () => tree.root.findAllByType(View).filter((node) => node.props.testID === 'activity-strip');
function expectStrip(text: string | null) {
  expect(strip()).toHaveLength(text === null ? 0 : 1);
  if (text !== null) {
    expect(strip()[0].props.accessibilityLabel).toBe(text);
    const label = strip()[0].findByType(Text);
    expect(label.props.children).toBe(text);
    expect(label.props.numberOfLines).toBe(1);
  }
}
async function emit(frame: Record<string, unknown>) {
  await act(async () => mockFrame({ sessionId: 's1', turnId: 't1', ...frame }));
}
beforeEach(async () => {
  mockClient.state = 'connected';
  onError.mockClear();
  await act(async () => { tree = renderer.create(screen()); });
});
afterEach(async () => { await act(async () => tree.unmount()); });

it('tracks turn, thinking, text, and matching tool identities above the composer', async () => {
  expectStrip(null);
  await emit({ type: 'turn_started' });
  await emit({ type: 'message_start', message: { role: 'assistant' } });
  expectStrip('Working…');
  await emit({ type: 'message_update', assistantMessageEvent: { type: 'thinking_delta', delta: 'reason' } });
  expectStrip('Thinking…');
  await emit({ type: 'message_update', assistantMessageEvent: { type: 'text_delta', delta: 'answer' } });
  expectStrip('Working…');
  await emit({ type: 'tool_execution_start', toolCallId: 'tool1', toolName: 'bash' });
  expectStrip('Running bash…');
  await emit({ type: 'tool_execution_end', toolCallId: 'other' });
  expectStrip('Running bash…');
  await emit({ type: 'tool_execution_end', toolCallId: 'tool1' });
  expectStrip('Working…');
  await emit({ type: 'turn_terminal' });
  expectStrip(null);
});

it('accepts session-scoped child and rename events without runtime incarnation', async () => {
  await emit({ type: 'turn_started' });
  await emit({ type: 'job_updated', job: { id: 'j1', agentId: 'a1', agent: 'reviewer', parentId: 'Main', status: 'running' } });
  expectStrip('Waiting for reviewer…');
  await emit({ type: 'agent_updated', agent: { id: 'a1', displayName: 'Code reviewer' } });
  expectStrip('Waiting for Code reviewer…');
  await emit({ type: 'job_updated', job: { id: 'j1', status: 'completed' } });
  expectStrip('Working…');
});

it('ignores foreign and sessionless events, including snapshots', async () => {
  await emit({ type: 'turn_started', sessionId: 'foreign' });
  await emit({ type: 'turn_started', sessionId: '' });
  expectStrip(null);
  await emit({ type: 'turn_started' });
  await emit({ type: 'tool_execution_start', sessionId: 'foreign', toolCallId: 'x', toolName: 'wrong' });
  await emit({ type: 'session_resync', sessionId: 'foreign', activeTurnId: null });
  expectStrip('Working…');
  await emit({ type: 'turn_terminal' });
  await emit({ type: 'tool_execution_start', toolCallId: 'late', toolName: 'bash' });
  expectStrip(null);
});

it('reconciles snapshot and resync lifecycle without reviving terminal turns', async () => {
  await emit({ type: 'subscription_snapshot', activeTurnId: 't1' });
  expectStrip('Working…');
  await emit({ type: 'turn_terminal' });
  await emit({ type: 'session_resync', activeTurnId: 't1' });
  expectStrip(null);
  await emit({ type: 'session_resync', activeTurnId: 't2' });
  expectStrip('Working…');
  await emit({ type: 'subscription_snapshot', activeTurnId: '' });
  expectStrip(null);
});

it('clears the previous strip when a catalog session is resumed', async () => {
  await emit({ type: 'turn_started' });
  expectStrip('Working…');
  await act(async () => tree.root.findByProps({ accessibilityLabel: 'Panels' }).props.onPress());
  await act(async () => tree.root.findByProps({ accessibilityLabel: 'sessions' }).props.onPress());
  await act(async () => tree.root.findByProps({ accessibilityLabel: 'Resume Beta' }).props.onPress());
  expectStrip(null);
  await emit({ type: 'turn_started', turnId: 'old' });
  expectStrip(null);
  await emit({ type: 'turn_started', sessionId: 's2', turnId: 'new' });
  expectStrip('Working…');
});

it('reads reconnecting connection state on render only while a turn is active', async () => {
  await emit({ type: 'turn_started' });
  mockClient.state = 'reconnecting';
  await act(async () => tree.update(screen()));
  expectStrip('Reconnecting…');
  await emit({ type: 'turn_terminal' });
  expectStrip(null);
});
