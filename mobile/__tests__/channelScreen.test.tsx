import React from 'react';
import renderer, { act } from 'react-test-renderer';
import { appendChannelLog, channelLogsFor, replaceChannelLogs } from '../src/channelLog';
import { DARK } from '../src/ui';
import { channelTerminalReplyText, channelTurnItems } from '../src/transcriptItems';

const mockSubscribe = jest.fn();
let mockFrameListener: (frame: unknown) => void;
let mockReconnectListener: () => void = () => {};
let getMessagesData: { messages: unknown[] } = {
  messages: [{ id: 'u1', message: { role: 'user', content: [{ type: 'text', text: 'prior' }] } }],
};

beforeEach(() => {
  getMessagesData = {
    messages: [{ id: 'u1', message: { role: 'user', content: [{ type: 'text', text: 'prior' }] } }],
  };
  const { mappingStore } = require('../src/runtime');
  mappingStore.deleteProject.mockReset();
  mappingStore.deleteProject.mockResolvedValue(undefined);
});

const mockCommand = jest.fn(async (command: { type: string }) => {
  if (command.type === 'session_list') {
    return {
      type: 'response',
      command: 'session_list',
      success: true,
      data: {
        sessions: [{
          sessionId: 's1',
          path: '/sessions/s1.jsonl',
          cwd: '/work',
          name: 'app',
          loaded: true,
          runtimeIncarnation: 'inc-1',
          status: 'native',
        }],
      },
    };
  }
  if (command.type === 'get_messages') {
    return {
      type: 'response',
      command: 'get_messages',
      success: true,
      data: getMessagesData,
    };
  }
  return { type: 'response', command: command.type, success: true, data: {} };
});

jest.mock('../src/runtime', () => ({
  registry: {
    client: () => ({
      state: 'connected',
      command: mockCommand,
      subscribe: mockSubscribe,
      subscribeFrames: (listener: (frame: unknown) => void) => {
        mockFrameListener = listener;
        return () => {};
      },
      subscribeReconnect: (listener: () => void) => {
        mockReconnectListener = listener;
        return () => {};
      },
      probe: { sessionId: 's1', runtimeIncarnation: 'inc-1', cwd: '/work' },
    }),
  },
  mappingStore: {
    mappingsForProject: () => [{
      id: 'm1',
      projectId: 'p-log',
      runnerId: 'r1',
      sessionId: 's1',
      sessionPath: '/sessions/s1.jsonl',
      sessionName: 'app',
      createdAt: 1,
    }],
    subscribe: () => () => {},
    deleteProject: jest.fn(async () => {}),
  },
}));

import { ProjectChannelScreen } from '../src/screens/ProjectChannelScreen';

const screen = (key: string) => (
  <ProjectChannelScreen
    key={key}
    palette={DARK}
    projectId="p-log"
    projects={[{ id: 'p-log', label: 'app', cwd: '/work', createdAt: 1, updatedAt: 1 }]}
    runners={[{
      id: 'r1',
      label: 'psy',
      origin: 'https://192.0.2.15:8765',
      authority: '192.0.2.15:8765',
      token: '',
      codename: 'psy',
      createdAt: 1,
      state: 'connected',
      error: '',
      errorKind: null,
      probe: { sessionId: 's1', runtimeIncarnation: 'inc-1', cwd: '/work' },
    }]}
    onBack={() => {}}
    onDeleted={() => {}}
    onError={() => {}}
  />
);

it('keeps channel logs across unmount and subscribes mapped sessions', async () => {
  replaceChannelLogs('p-log', []);
  appendChannelLog('p-log', {
    id: 'stay-1',
    at: 1,
    kind: 'human',
    mappingId: null,
    author: 'You',
    text: 'persisted across back',
  });
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(screen('a'));
  });
  const texts = () => tree.root.findAllByType(require('react-native').Text).map((node) => node.props.children);
  expect(texts()).toContain('persisted across back');
  await act(async () => {
    mockFrameListener({ type: 'turn_terminal', sessionId: 's1', status: 'failed', message: 'model refused' });
  });
  expect(channelLogsFor('p-log').map((log) => log.text)).toEqual(['persisted across back']);
  expect(mockCommand.mock.calls.some((call) => call[0].type === 'session_list')).toBe(true);
  expect(mockCommand.mock.calls.some((call) => call[0].type === 'get_messages')).toBe(true);
  expect(mockSubscribe).toHaveBeenCalled();
  await act(async () => {
    tree.unmount();
  });
  await act(async () => {
    tree = renderer.create(screen('b'));
  });
  expect(texts()).toContain('persisted across back');
  await act(async () => {
    tree.unmount();
  });
});

it('deletes a project only after a second Delete? press and Back disarms', async () => {
  const { mappingStore } = require('../src/runtime');
  const deleted = jest.fn();
  const back = jest.fn();
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(
      <ProjectChannelScreen
        palette={DARK}
        projectId="p-log"
        projects={[{ id: 'p-log', label: 'app', cwd: '/work', createdAt: 1, updatedAt: 1 }]}
        runners={[{
          id: 'r1', label: 'psy', origin: 'https://192.0.2.15:8765', authority: '192.0.2.15:8765',
          token: '', codename: 'psy', createdAt: 1, state: 'connected', error: '', errorKind: null,
          probe: { sessionId: 's1', runtimeIncarnation: 'inc-1', cwd: '/work' },
        }]}
        onBack={back}
        onDeleted={deleted}
        onError={() => {}}
      />,
    );
  });
  await act(async () => { tree.root.findByProps({ accessibilityLabel: 'Delete project' }).props.onPress(); });
  expect(mappingStore.deleteProject).not.toHaveBeenCalled();
  expect(deleted).not.toHaveBeenCalled();
  await act(async () => { tree.root.findByProps({ accessibilityLabel: 'Back' }).props.onPress(); });
  expect(back).not.toHaveBeenCalled();
  await act(async () => { tree.root.findByProps({ accessibilityLabel: 'Delete project' }).props.onPress(); });
  await act(async () => { tree.root.findByProps({ accessibilityLabel: 'Delete?' }).props.onPress(); });
  expect(mappingStore.deleteProject).toHaveBeenCalledWith(expect.anything(), 'p-log');
  expect(deleted).toHaveBeenCalledTimes(1);
  await act(async () => { tree.unmount(); });
});

it('surfaces a rejected project delete and stays on the channel', async () => {
  const { mappingStore } = require('../src/runtime');
  mappingStore.deleteProject.mockImplementationOnce(async () => {
    throw new Error('persist failed');
  });
  const deleted = jest.fn();
  const errors: string[] = [];
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(
      <ProjectChannelScreen
        palette={DARK}
        projectId="p-log"
        projects={[{ id: 'p-log', label: 'app', cwd: '/work', createdAt: 1, updatedAt: 1 }]}
        runners={[{
          id: 'r1', label: 'psy', origin: 'https://192.0.2.15:8765', authority: '192.0.2.15:8765',
          token: '', codename: 'psy', createdAt: 1, state: 'connected', error: '', errorKind: null,
          probe: { sessionId: 's1', runtimeIncarnation: 'inc-1', cwd: '/work' },
        }]}
        onBack={() => {}}
        onDeleted={deleted}
        onError={(message) => errors.push(message)}
      />,
    );
  });
  await act(async () => { tree.root.findByProps({ accessibilityLabel: 'Delete project' }).props.onPress(); });
  await act(async () => { tree.root.findByProps({ accessibilityLabel: 'Delete?' }).props.onPress(); });
  expect(mappingStore.deleteProject).toHaveBeenCalledWith(expect.anything(), 'p-log');
  expect(deleted).not.toHaveBeenCalled();
  expect(errors).toEqual(['persist failed']);
  expect(tree.root.findByProps({ accessibilityLabel: 'Delete?' })).toBeTruthy();
  await act(async () => { tree.unmount(); });
});


it('persists a failed terminal reply from a streaming runner', async () => {
  replaceChannelLogs('p-log', []);
  let tree!: renderer.ReactTestRenderer;
  await act(async () => { tree = renderer.create(screen('failed')); });
  await act(async () => {
    mockFrameListener({ type: 'message_start', sessionId: 's1', message: { role: 'assistant' } });
  });
  await act(async () => {
    mockFrameListener({ type: 'turn_terminal', sessionId: 's1', status: 'failed', message: 'model refused' });
  });
  const texts = tree.root.findAllByType(require('react-native').Text).map((node) => node.props.children);
  expect(texts).toContain('model refused');
  expect(texts).toContain('@psy');
  expect(channelLogsFor('p-log')).toEqual([
    expect.objectContaining({ kind: 'runner', text: 'model refused' }),
  ]);
  await act(async () => { tree.unmount(); });
});

it('does not commit advisory events or an empty successful terminal', async () => {
  replaceChannelLogs('p-log', []);
  let tree!: renderer.ReactTestRenderer;
  await act(async () => { tree = renderer.create(screen('succeeded')); });
  await act(async () => {
    mockFrameListener({ type: 'message_start', sessionId: 's1', message: { role: 'assistant' } });
  });
  await act(async () => { mockFrameListener({ type: 'turn_end', sessionId: 's1' }); });
  expect(channelLogsFor('p-log')).toEqual([]);
  await act(async () => {
    mockFrameListener({ type: 'run_failed', sessionId: 's1', message: 'ignored advisory' });
  });
  expect(channelLogsFor('p-log')).toEqual([]);
  await act(async () => {
    mockFrameListener({ type: 'turn_terminal', sessionId: 's1', status: 'succeeded' });
  });
  expect(channelLogsFor('p-log')).toEqual([]);
  await act(async () => { tree.unmount(); });
});


it('selects assistant text before terminal errors and normalizes failure messages', () => {
  expect(channelTerminalReplyText({ status: 'failed', message: 'error' }, [
    { kind: 'assistant', id: 'a1', status: 'final', blocks: [{ type: 'text', text: 'answer' }] },
  ])).toBe('answer');
  expect(channelTerminalReplyText({ status: 'failed', message: '  model refused  ' }, [])).toBe('model refused');
  expect(channelTerminalReplyText({ status: 'aborted', message: 'cancelled' }, [])).toBe('');
  expect(channelTerminalReplyText({ status: 'failed', message: '  ' }, [])).toBe('The runner reported that the turn failed.');
  expect(channelTerminalReplyText({}, [])).toBe('The runner reported that the turn failed.');
  expect(channelTerminalReplyText({ status: 'unknown', message: 'unexpected failure' }, [])).toBe('unexpected failure');
  const mixed = [
    { kind: 'assistant' as const, id: 'old', status: 'final' as const, blocks: [{ type: 'text', text: 'pong' }] },
    { kind: 'assistant' as const, id: 'new', status: 'final' as const, blocks: [] },
  ];
  expect(channelTurnItems(mixed, new Set(['old'])).map((item) => item.id)).toEqual(['new']);
  expect(channelTerminalReplyText({ status: 'failed', message: '402 insufficient-balance' }, mixed, new Set(['old']))).toBe('402 insufficient-balance');
  expect(channelTerminalReplyText({ status: 'succeeded' }, [
    { kind: 'assistant', id: 'old', status: 'final', blocks: [{ type: 'text', text: 'pong' }] },
    { kind: 'assistant', id: 'new', status: 'final', blocks: [{ type: 'text', text: 'hello' }] },
  ], new Set(['old']))).toBe('hello');
});

it('still commits a failed terminal after message_end', async () => {
  replaceChannelLogs('p-log', []);
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(
      <ProjectChannelScreen
        palette={DARK}
        projectId="p-log"
        projects={[{ id: 'p-log', label: 'app', cwd: '/work', createdAt: 1, updatedAt: 1 }]}
        runners={[{
          id: 'r1', label: 'psy', origin: 'https://192.0.2.15:8765', authority: '192.0.2.15:8765',
          token: '', codename: 'psy', createdAt: 1, state: 'connected', error: '', errorKind: null,
          probe: { sessionId: 's1', runtimeIncarnation: 'inc-1', cwd: '/work' },
        }]}
        onBack={() => {}}
        onDeleted={() => {}}
        onError={() => {}}
      />,
    );
  });
  await act(async () => {
    mockFrameListener({ type: 'message_start', sessionId: 's1', message: { role: 'assistant' } });
  });
  await act(async () => {
    mockFrameListener({ type: 'message_end', sessionId: 's1' });
  });
  await act(async () => {
    mockFrameListener({ type: 'run_failed', sessionId: 's1', message: 'ignored advisory' });
  });
  await act(async () => {
    mockFrameListener({ type: 'turn_terminal', sessionId: 's1', status: 'failed', message: 'model refused after end' });
  });
  const texts = tree.root.findAllByType(require('react-native').Text).map((node) => node.props.children);
  expect(texts).toContain('model refused after end');
  expect(texts).not.toContain('ignored advisory');
  await act(async () => { tree.unmount(); });
});

it('does not let a prior assistant answer mask a later failed terminal', async () => {
  getMessagesData = {
    messages: [
      { id: 'u1', message: { role: 'user', content: [{ type: 'text', text: 'prior' }] } },
      { id: 'a-old', message: { role: 'assistant', content: [{ type: 'text', text: 'stale pong' }] } },
    ],
  };
  replaceChannelLogs('p-log', []);
  let tree!: renderer.ReactTestRenderer;
  await act(async () => { tree = renderer.create(screen('stale-fail')); });
  await act(async () => {
    mockFrameListener({ type: 'message_start', sessionId: 's1', message: { role: 'assistant' } });
  });
  await act(async () => {
    mockFrameListener({ type: 'message_end', sessionId: 's1' });
  });
  await act(async () => {
    mockFrameListener({ type: 'turn_terminal', sessionId: 's1', status: 'failed', message: '402 insufficient-balance' });
  });
  const texts = tree.root.findAllByType(require('react-native').Text).map((node) => node.props.children);
  expect(texts).toContain('402 insufficient-balance');
  expect(channelLogsFor('p-log')).toEqual([
    expect.objectContaining({ kind: 'runner', text: '402 insufficient-balance' }),
  ]);
  expect(channelLogsFor('p-log').map((log) => log.text)).not.toContain('stale pong');
  await act(async () => { tree.unmount(); });
});

it('commits only the current-turn assistant after a prior answer', async () => {
  getMessagesData = {
    messages: [
      { id: 'u1', message: { role: 'user', content: [{ type: 'text', text: 'prior' }] } },
      { id: 'a-old', message: { role: 'assistant', content: [{ type: 'text', text: 'stale pong' }] } },
    ],
  };
  replaceChannelLogs('p-log', []);
  let tree!: renderer.ReactTestRenderer;
  await act(async () => { tree = renderer.create(screen('stale-ok')); });
  await act(async () => {
    mockFrameListener({ type: 'message_start', sessionId: 's1', itemId: 'a-new', message: { role: 'assistant' } });
  });
  await act(async () => {
    mockFrameListener({
      type: 'message_update',
      sessionId: 's1',
      itemId: 'a-new',
      assistantMessageEvent: { type: 'text_delta', delta: 'hello' },
    });
  });
  await act(async () => {
    mockFrameListener({ type: 'turn_terminal', sessionId: 's1', status: 'succeeded' });
  });
  expect(channelLogsFor('p-log')).toEqual([
    expect.objectContaining({ kind: 'runner', text: 'hello' }),
  ]);
  await act(async () => { tree.unmount(); });
});

it('awaits abort and surfaces throw or refusal without clearing streaming', async () => {
  const errors: string[] = [];
  replaceChannelLogs('p-log', []);
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(
      <ProjectChannelScreen
        palette={DARK}
        projectId="p-log"
        projects={[{ id: 'p-log', label: 'app', cwd: '/work', createdAt: 1, updatedAt: 1 }]}
        runners={[{
          id: 'r1', label: 'psy', origin: 'https://192.0.2.15:8765', authority: '192.0.2.15:8765',
          token: '', codename: 'psy', createdAt: 1, state: 'connected', error: '', errorKind: null,
          probe: { sessionId: 's1', runtimeIncarnation: 'inc-1', cwd: '/work' },
        }]}
        onBack={() => {}}
        onDeleted={() => {}}
        onError={(message) => errors.push(message)}
      />,
    );
  });
  await act(async () => {
    mockFrameListener({ type: 'message_start', sessionId: 's1', message: { role: 'assistant' } });
  });
  mockCommand.mockImplementationOnce(async (command: { type: string }) => {
    if (command.type === 'abort') return { type: 'response', command: 'abort', success: false, error: 'busy' };
    return { type: 'response', command: command.type, success: true, data: {} };
  });
  await act(async () => {
    await tree.root.findByProps({ accessibilityLabel: 'Stop' }).props.onPress();
  });
  expect(mockCommand).toHaveBeenCalledWith({ type: 'abort' }, 's1');
  expect(errors).toEqual(['busy']);
  expect(tree.root.findByProps({ accessibilityLabel: 'Stop' })).toBeTruthy();
  mockCommand.mockImplementationOnce(async (command: { type: string }) => {
    if (command.type === 'abort') throw new Error('disconnected');
    return { type: 'response', command: command.type, success: true, data: {} };
  });
  await act(async () => {
    await tree.root.findByProps({ accessibilityLabel: 'Stop' }).props.onPress();
  });
  expect(errors).toEqual(['busy', 'disconnected']);
  expect(tree.root.findByProps({ accessibilityLabel: 'Stop' })).toBeTruthy();
  await act(async () => {
    mockFrameListener({ type: 'turn_terminal', sessionId: 's1', status: 'aborted', message: 'cancelled' });
  });
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Stop' })).toHaveLength(0);
  await act(async () => { tree.unmount(); });
});

it('shows Stop from turn_started without message_start', async () => {
  replaceChannelLogs('p-log', []);
  let tree!: renderer.ReactTestRenderer;
  await act(async () => { tree = renderer.create(screen('turn-started-stop')); });
  await act(async () => {
    mockFrameListener({ type: 'turn_started', sessionId: 's1', runtimeIncarnation: 'inc-1', turnId: 'turn-1' });
  });
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Stop' }).length).toBeGreaterThan(0);
  await act(async () => {
    mockFrameListener({ type: 'turn_terminal', sessionId: 's1', status: 'succeeded' });
  });
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Stop' })).toHaveLength(0);
  await act(async () => { tree.unmount(); });
});

it('commits a failed terminal after snapshot activeTurnId without message_start', async () => {
  replaceChannelLogs('p-log', []);
  let tree!: renderer.ReactTestRenderer;
  await act(async () => { tree = renderer.create(screen('snapshot-live')); });
  await act(async () => {
    mockFrameListener({ type: 'subscription_snapshot', sessionId: 's1', activeTurnId: 'turn-9' });
  });
  await act(async () => {
    mockFrameListener({ type: 'turn_terminal', sessionId: 's1', status: 'failed', message: 'model refused' });
  });
  expect(channelLogsFor('p-log')).toEqual([
    expect.objectContaining({ kind: 'runner', text: 'model refused' }),
  ]);
  const texts = tree.root.findAllByType(require('react-native').Text).map((node) => node.props.children);
  expect(texts).toContain('model refused');
  await act(async () => { tree.unmount(); });
});

it('does not log historical assistant text after turn_started then failed terminal', async () => {
  replaceChannelLogs('p-log', []);
  getMessagesData = {
    messages: [
      { id: 'u-old', message: { role: 'user', content: [{ type: 'text', text: 'old ping' }] } },
      { id: 'a-old', message: { role: 'assistant', content: [{ type: 'text', text: 'stale pong' }] } },
    ],
  };
  let tree!: renderer.ReactTestRenderer;
  await act(async () => { tree = renderer.create(screen('turn-started-history')); });
  await act(async () => {
    mockFrameListener({ type: 'turn_started', sessionId: 's1', runtimeIncarnation: 'inc-1', turnId: 'turn-9' });
  });
  await act(async () => {
    mockFrameListener({ type: 'turn_terminal', sessionId: 's1', status: 'failed', message: 'model refused' });
  });
  expect(channelLogsFor('p-log')).toEqual([
    expect.objectContaining({ kind: 'runner', text: 'model refused' }),
  ]);
  const texts = tree.root.findAllByType(require('react-native').Text).map((node) => node.props.children);
  expect(texts).toContain('stale pong');
  expect(texts.filter((row) => row === 'stale pong')).toHaveLength(1);
  expect(texts).toContain('model refused');
  await act(async () => { tree.unmount(); });
});

it('does not log historical assistant text as the failed snapshot-resume reply', async () => {
  replaceChannelLogs('p-log', []);
  getMessagesData = {
    messages: [
      { id: 'u-old', message: { role: 'user', content: [{ type: 'text', text: 'old ping' }] } },
      { id: 'a-old', message: { role: 'assistant', content: [{ type: 'text', text: 'stale pong' }] } },
    ],
  };
  let tree!: renderer.ReactTestRenderer;
  await act(async () => { tree = renderer.create(screen('snapshot-history')); });
  await act(async () => {
    mockFrameListener({ type: 'subscription_snapshot', sessionId: 's1', activeTurnId: 'turn-9' });
  });
  await act(async () => {
    mockFrameListener({ type: 'turn_terminal', sessionId: 's1', status: 'failed', message: 'model refused' });
  });
  expect(channelLogsFor('p-log')).toEqual([
    expect.objectContaining({ kind: 'runner', text: 'model refused' }),
  ]);
  const texts = tree.root.findAllByType(require('react-native').Text).map((node) => node.props.children);
  expect(texts).toContain('stale pong');
  expect(texts.filter((row) => row === 'stale pong')).toHaveLength(1);
  expect(texts).toContain('model refused');
  await act(async () => { tree.unmount(); });
});

it('reconnect attachMapping rejection surfaces onError', async () => {
  const errors: string[] = [];
  replaceChannelLogs('p-log', []);
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(
      <ProjectChannelScreen
        palette={DARK}
        projectId="p-log"
        projects={[{ id: 'p-log', label: 'app', cwd: '/work', createdAt: 1, updatedAt: 1 }]}
        runners={[{
          id: 'r1', label: 'psy', origin: 'https://192.0.2.15:8765', authority: '192.0.2.15:8765',
          token: '', codename: 'psy', createdAt: 1, state: 'connected', error: '', errorKind: null,
          probe: { sessionId: 's1', runtimeIncarnation: 'inc-1', cwd: '/work' },
        }]}
        onBack={() => {}}
        onDeleted={() => {}}
        onError={(message) => errors.push(message)}
      />,
    );
  });
  mockCommand.mockImplementationOnce(async () => {
    throw new Error('attach failed');
  });
  await act(async () => {
    mockReconnectListener();
  });
  expect(errors).toEqual(['attach failed']);
  await act(async () => { tree.unmount(); });
});

it('rejects Send with zero recipients without accepting the message', async () => {
  const { mappingStore } = require('../src/runtime');
  const mappings = jest.spyOn(mappingStore, 'mappingsForProject').mockReturnValue([]);
  const errors: string[] = [];
  const message = 'native-keyboard-check';
  replaceChannelLogs('p-log', []);
  let tree: renderer.ReactTestRenderer | undefined;
  try {
    await act(async () => {
      tree = renderer.create(React.cloneElement(screen('zero-recipients'), {
        onError: (error: string) => errors.push(error),
      }));
    });
    const root = tree!.root;
    await act(async () => {
      root.findByType(require('react-native').TextInput).props.onChangeText(message);
    });
    await act(async () => {
      await root.findByProps({ accessibilityLabel: 'Send' }).props.onPress();
    });
    expect({
      draft: root.findByType(require('react-native').TextInput).props.value,
      logs: channelLogsFor('p-log'),
      renderedMessage: root.findAllByType(require('react-native').Text)
        .some((node) => node.props.children === message),
      errors,
    }).toEqual({
      draft: message,
      logs: [],
      renderedMessage: false,
      errors: ['Add a runner to join this channel.'],
    });
  } finally {
    await act(async () => { tree?.unmount(); });
    mappings.mockRestore();
  }
});

it('overlapping Send calls prompt once', async () => {
  replaceChannelLogs('p-log', []);
  const previous = mockCommand.getMockImplementation();
  let settle!: (value: { type: string; command: string; success: boolean; data: object }) => void;
  const pending = new Promise<{ type: string; command: string; success: boolean; data: object }>((resolve) => {
    settle = resolve;
  });
  mockCommand.mockImplementation(async (command: { type: string }) => {
    if (command.type === 'prompt') {
      return pending;
    }
    if (command.type === 'session_list') {
      return {
        type: 'response',
        command: 'session_list',
        success: true,
        data: {
          sessions: [{
            sessionId: 's1',
            path: '/sessions/s1.jsonl',
            cwd: '/work',
            name: 'app',
            loaded: true,
            runtimeIncarnation: 'inc-1',
            status: 'native',
          }],
        },
      };
    }
    if (command.type === 'get_messages') {
      return {
        type: 'response',
        command: 'get_messages',
        success: true,
        data: getMessagesData,
      };
    }
    return { type: 'response', command: command.type, success: true, data: {} };
  });
  let tree!: renderer.ReactTestRenderer;
  try {
    await act(async () => {
      tree = renderer.create(screen('overlap'));
    });
    await act(async () => {
      tree.root.findByType(require('react-native').TextInput).props.onChangeText('keep this draft');
    });
    const send = tree.root.findByProps({ accessibilityLabel: 'Send' });
    await act(async () => {
      send.props.onPress();
      send.props.onPress();
    });
    expect(mockCommand.mock.calls.filter((call) => call[0].type === 'prompt')).toHaveLength(1);
    expect(channelLogsFor('p-log').filter((log) => log.kind === 'human').map((log) => log.text)).toEqual(['keep this draft']);
    await act(async () => {
      settle({ type: 'response', command: 'prompt', success: true, data: {} });
    });
    await act(async () => { tree.unmount(); });
  } finally {
    if (previous) mockCommand.mockImplementation(previous);
  }
});
