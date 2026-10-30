import React from 'react';
import renderer, { act } from 'react-test-renderer';
import { Text } from 'react-native';
import { encodeTmuxTerminalData, type ProcessKey, type ProcessSignal } from '../src/protocol';
import type { RunnerClient } from '../src/runnerClient';
import { ProcessesPanel } from '../src/screens/ProcessesPanel';
import { DARK } from '../src/ui';

type WireCommand = { type: string } & Record<string, unknown>;
type Call = { command: WireCommand; sessionId: string | undefined };

const SESSION = 's1';
const OTHER_SESSION = 's2';

const LIVE = {
  id: 'p1',
  ownerId: 'owner',
  label: 'sleep',
  state: 'running',
  pid: 42,
  tty: true,
  startedAtMs: 1,
  outputStartCursor: 0,
  outputCursor: 5,
};

const EXITED = {
  ...LIVE,
  state: 'exited',
  pid: null,
  exitCode: 0,
  exitedAtMs: 2,
};

function ok(command: string, data: unknown) {
  return { type: 'response', command, success: true, data };
}

function fail(command: string, error: string) {
  return { type: 'response', command, success: false, error };
}

function logsPage(text: string, cursor: number, eof: boolean, lostBytes = 0) {
  const dataBase64 = encodeTmuxTerminalData(text);
  return {
    requestedCursor: 0,
    startCursor: 0,
    cursor,
    eof,
    lost: lostBytes > 0,
    lostBytes,
    chunks: [{ startCursor: 0, cursor, stream: 'stdout', dataBase64 }],
  };
}

function mockClient(handler: (command: WireCommand, sessionId: string | undefined) => unknown): {
  client: RunnerClient;
  calls: Call[];
  emit: (frame: unknown) => void;
} {
  const calls: Call[] = [];
  let onFrame: ((frame: unknown) => void) | null = null;
  const client = {
    command: async (command: WireCommand, sessionId?: string) => {
      calls.push({ command, sessionId });
      return handler(command, sessionId);
    },
    subscribeFrames: (listener: (frame: unknown) => void) => {
      onFrame = listener;
      return () => {
        onFrame = null;
      };
    },
  } as unknown as RunnerClient;
  return {
    client,
    calls,
    emit: (frame: unknown) => {
      onFrame?.(frame);
    },
  };
}

function defaultHandler(command: WireCommand): unknown {
  switch (command.type) {
    case 'get_state':
      return ok('get_state', { sessionId: SESSION, runtimeIncarnation: 'inc-1', cwd: '/work' });
    case 'process_list':
      return ok('process_list', [LIVE]);
    case 'process_describe':
      return ok('process_describe', LIVE);
    case 'process_logs':
      return ok('process_logs', logsPage('hello', 5, false));
    case 'process_spawn':
      return ok('process_spawn', LIVE);
    case 'process_stop':
      return ok('process_stop', EXITED);
    case 'process_wait':
      return ok('process_wait', EXITED);
    case 'process_write':
    case 'process_keys':
    case 'process_resize':
    case 'process_signal':
      return ok(command.type, null);
    default:
      return fail(command.type, 'unexpected');
  }
}

async function withPanel(
  client: RunnerClient,
  onError: (message: string) => void,
  run: (tree: renderer.ReactTestRenderer) => Promise<void>,
  onClose: () => void = () => {},
): Promise<void> {
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(
      <ProcessesPanel
        palette={DARK}
        client={client}
        sessionId={SESSION}
        onClose={onClose}
        onError={onError}
      />,
    );
  });
  try {
    await run(tree);
  } finally {
    await act(async () => {
      tree.unmount();
    });
  }
}

function renderedText(tree: renderer.ReactTestRenderer): string {
  return tree.root
    .findAllByType(Text)
    .map((node) => React.Children.toArray(node.props.children).join(''))
    .join('\n');
}

function labeled(tree: renderer.ReactTestRenderer, label: string): renderer.ReactTestInstance {
  return tree.root.findByProps({ accessibilityLabel: label });
}

async function press(tree: renderer.ReactTestRenderer, label: string): Promise<void> {
  await act(async () => {
    labeled(tree, label).props.onPress();
  });
}

async function typeInto(tree: renderer.ReactTestRenderer, label: string, value: string): Promise<void> {
  await act(async () => {
    labeled(tree, label).props.onChangeText(value);
  });
}

function commandsOf(calls: Call[], type: string): Call[] {
  return calls.filter((call) => call.command.type === type);
}

it('lists processes from a session-scoped process_list and Back closes', async () => {
  const closed: string[] = [];
  const { client, calls } = mockClient((command, sessionId) => defaultHandler(command));
  await withPanel(client, () => {}, async (tree) => {
    expect(renderedText(tree)).toContain('sleep');
    expect(renderedText(tree)).toContain('running');
    expect(commandsOf(calls, 'process_list').every((call) => call.sessionId === SESSION)).toBe(true);
    expect(commandsOf(calls, 'get_state').every((call) => call.sessionId === SESSION)).toBe(true);
    await press(tree, 'Back');
    expect(closed).toEqual(['closed']);
  }, () => {
    closed.push('closed');
  });
});

it('keeps last-good rows when a later process_list fails', async () => {
  const errors: string[] = [];
  let listMode: 'ok' | 'fail' | 'invalid' = 'ok';
  const { client } = mockClient((command) => {
    if (command.type === 'process_list' && listMode === 'fail') {
      return fail('process_list', 'runner busy');
    }
    if (command.type === 'process_list' && listMode === 'invalid') {
      return ok('process_list', { not: 'an-array' });
    }
    return defaultHandler(command);
  });
  await withPanel(client, (message) => errors.push(message), async (tree) => {
    expect(renderedText(tree)).toContain('sleep');
    listMode = 'fail';
    await press(tree, 'Refresh');
    expect(renderedText(tree)).toContain('sleep');
    expect(errors.some((message) => message.includes('runner busy'))).toBe(true);
    listMode = 'invalid';
    await press(tree, 'Refresh');
    expect(renderedText(tree)).toContain('sleep');
    expect(errors.some((message) => message.includes('Invalid process_list'))).toBe(true);
  });
});

it('spawns bash -c in the get_state cwd and refuses an empty cwd', async () => {
  const errors: string[] = [];
  const spawned: Call[] = [];
  const { client, calls } = mockClient((command, sessionId) => {
    if (command.type === 'process_spawn') spawned.push({ command, sessionId });
    return defaultHandler(command);
  });
  await withPanel(client, (message) => errors.push(message), async (tree) => {
    expect(labeled(tree, 'Spawn').props.disabled).toBe(true);
    await typeInto(tree, 'Command', '  printf hi  ');
    await typeInto(tree, 'Label', 'e2e');
    await press(tree, 'Spawn');
    expect(spawned).toEqual([{
      sessionId: SESSION,
      command: {
        type: 'process_spawn',
        spec: {
          argv: ['bash', '-c', 'printf hi'],
          cwd: '/work',
          tty: true,
          terminalSize: { rows: 24, cols: 80 },
          label: 'e2e',
        },
      },
    }]);
    expect(commandsOf(calls, 'process_spawn')[0].sessionId).toBe(SESSION);
  });

  const empty = mockClient((command) => {
    if (command.type === 'get_state') {
      return ok('get_state', { sessionId: SESSION, runtimeIncarnation: 'inc-1', cwd: '  ' });
    }
    return defaultHandler(command);
  });
  await withPanel(empty.client, () => {}, async (tree) => {
    await typeInto(tree, 'Command', 'printf hi');
    expect(labeled(tree, 'Spawn').props.disabled).toBe(true);
    expect(commandsOf(empty.calls, 'process_spawn')).toHaveLength(0);
  });

  const unowned = mockClient((command) => {
    if (command.type === 'get_state') {
      return ok('get_state', { runtimeIncarnation: 'inc-1', cwd: '/work' });
    }
    return defaultHandler(command);
  });
  await withPanel(unowned.client, () => {}, async (tree) => {
    await typeInto(tree, 'Command', 'printf hi');
    expect(labeled(tree, 'Spawn').props.disabled).toBe(true);
    expect(commandsOf(unowned.calls, 'process_spawn')).toHaveLength(0);
  });
});

it('omits spawn when the label is over 64 bytes', async () => {
  const errors: string[] = [];
  const { client, calls } = mockClient((command) => defaultHandler(command));
  await withPanel(client, (message) => errors.push(message), async (tree) => {
    await typeInto(tree, 'Command', 'sleep 1');
    await typeInto(tree, 'Label', 'x'.repeat(65));
    await press(tree, 'Spawn');
    expect(commandsOf(calls, 'process_spawn')).toHaveLength(0);
    expect(errors.some((message) => message.includes('64 bytes'))).toBe(true);
    expect(renderedText(tree)).toContain('sleep');
  });
});

it('keeps last-good list when process_spawn fails', async () => {
  const errors: string[] = [];
  const { client } = mockClient((command) => {
    if (command.type === 'process_spawn') return fail('process_spawn', 'argv refused');
    return defaultHandler(command);
  });
  await withPanel(client, (message) => errors.push(message), async (tree) => {
    await typeInto(tree, 'Command', 'sleep 1');
    await press(tree, 'Spawn');
    expect(renderedText(tree)).toContain('sleep');
    expect(errors.some((message) => message.includes('argv refused'))).toBe(true);
  });
});

it('single-flights Spawn so a second press sends no RPC', async () => {
  let release!: () => void;
  const blocked = new Promise<void>((resolve) => {
    release = resolve;
  });
  const { client, calls } = mockClient((command) => {
    if (command.type === 'process_spawn') {
      return blocked.then(() => defaultHandler(command));
    }
    return defaultHandler(command);
  });
  await withPanel(client, () => {}, async (tree) => {
    await typeInto(tree, 'Command', 'sleep 9');
    await act(async () => {
      labeled(tree, 'Spawn').props.onPress();
      labeled(tree, 'Spawn').props.onPress();
    });
    expect(commandsOf(calls, 'process_spawn')).toHaveLength(1);
    await act(async () => {
      release();
    });
  });
});

it('selects a row, loads describe plus cursor-0 logs, then Load logs advances the cursor', async () => {
  const { client, calls } = mockClient((command) => {
    if (command.type === 'process_logs') {
      const cursor = typeof command.cursor === 'number' ? command.cursor : 0;
      if (cursor === 0) return ok('process_logs', logsPage('hello', 5, false));
      return ok('process_logs', logsPage(' world', 11, true));
    }
    return defaultHandler(command);
  });
  await withPanel(client, () => {}, async (tree) => {
    await press(tree, 'Process sleep');
    expect(commandsOf(calls, 'process_describe')[0]).toEqual({
      sessionId: SESSION,
      command: { type: 'process_describe', processId: 'p1' },
    });
    expect(commandsOf(calls, 'process_logs')[0].command).toEqual({
      type: 'process_logs',
      processId: 'p1',
      cursor: 0,
      limitBytes: 16384,
    });
    expect(renderedText(tree)).toContain('hello');
    await press(tree, 'Load logs');
    expect(commandsOf(calls, 'process_logs')[1].command.cursor).toBe(5);
    expect(renderedText(tree)).toContain('hello world');
    expect(tree.root.findAll((node) => node.props.accessibilityLabel === 'Load logs')).toHaveLength(0);
  });
});

it('keeps last-good logs when a later page is unusable', async () => {
  const errors: string[] = [];
  let poison = false;
  const { client } = mockClient((command) => {
    if (command.type === 'process_logs' && poison) return ok('process_logs', { cursor: 'nope' });
    return defaultHandler(command);
  });
  await withPanel(client, (message) => errors.push(message), async (tree) => {
    await press(tree, 'Process sleep');
    expect(renderedText(tree)).toContain('hello');
    poison = true;
    await press(tree, 'Load logs');
    expect(renderedText(tree)).toContain('hello');
    expect(errors.some((message) => message.includes('Invalid process_logs'))).toBe(true);
  });
});

it('sends stdin through process_write with the session id', async () => {
  const { client, calls } = mockClient((command) => defaultHandler(command));
  await withPanel(client, () => {}, async (tree) => {
    await press(tree, 'Process sleep');
    expect(labeled(tree, 'Send input').props.disabled).toBe(true);
    await typeInto(tree, 'Process input', 'echo 1');
    await press(tree, 'Send input');
    expect(commandsOf(calls, 'process_write')[0]).toEqual({
      sessionId: SESSION,
      command: {
        type: 'process_write',
        processId: 'p1',
        dataBase64: encodeTmuxTerminalData('echo 1'),
      },
    });
  });
});

it('sends every ProcessKey through process_keys', async () => {
  const keys: ProcessKey[] = [
    'ENTER', 'TAB', 'ESCAPE', 'CTRL_C', 'CTRL_D', 'UP', 'DOWN', 'LEFT', 'RIGHT',
  ];
  const { client, calls } = mockClient((command) => defaultHandler(command));
  await withPanel(client, () => {}, async (tree) => {
    await press(tree, 'Process sleep');
    for (const key of keys) {
      await press(tree, `Key ${key}`);
    }
    const sent = commandsOf(calls, 'process_keys').map((call) => call.command.keys);
    expect(sent).toEqual(keys.map((key) => [key]));
    expect(commandsOf(calls, 'process_keys').every((call) => call.sessionId === SESSION)).toBe(true);
  });
});

it('resizes with integer cols and rows', async () => {
  const errors: string[] = [];
  const { client, calls } = mockClient((command) => defaultHandler(command));
  await withPanel(client, (message) => errors.push(message), async (tree) => {
    await press(tree, 'Process sleep');
    await typeInto(tree, 'Columns', '120');
    await typeInto(tree, 'Rows', '40');
    await press(tree, 'Resize');
    expect(commandsOf(calls, 'process_resize')[0]).toEqual({
      sessionId: SESSION,
      command: { type: 'process_resize', processId: 'p1', cols: 120, rows: 40 },
    });
    await typeInto(tree, 'Columns', '0');
    await press(tree, 'Resize');
    expect(commandsOf(calls, 'process_resize')).toHaveLength(1);
    expect(errors.some((message) => message.includes('Resize'))).toBe(true);
  });
});

it('stops, signals, and waits on the selected process', async () => {
  const signals: ProcessSignal[] = ['SIGINT', 'SIGTERM', 'SIGHUP', 'SIGQUIT', 'SIGKILL'];
  const { client, calls } = mockClient((command) => defaultHandler(command));
  await withPanel(client, () => {}, async (tree) => {
    await press(tree, 'Process sleep');
    for (const signal of signals) {
      await press(tree, `Signal ${signal}`);
    }
    expect(commandsOf(calls, 'process_signal').map((call) => call.command.signal)).toEqual(signals);
    await press(tree, 'Stop');
    expect(commandsOf(calls, 'process_stop')[0]).toEqual({
      sessionId: SESSION,
      command: { type: 'process_stop', processId: 'p1' },
    });
    await typeInto(tree, 'Wait timeout', '2500');
    await press(tree, 'Wait');
    expect(commandsOf(calls, 'process_wait')[0]).toEqual({
      sessionId: SESSION,
      command: { type: 'process_wait', processId: 'p1', timeoutMs: 2500 },
    });
    expect(commandsOf(calls, 'process_stop')[0].sessionId).toBe(SESSION);
    expect(commandsOf(calls, 'process_signal').every((call) => call.sessionId === SESSION)).toBe(true);
    expect(commandsOf(calls, 'process_wait')[0].sessionId).toBe(SESSION);
  });
});

it('ignores a late process_list after unmount and a foreign-session process_exited', async () => {
  const errors: string[] = [];
  let resolveList!: (value: unknown) => void;
  let listCalls = 0;
  const pending = new Promise((resolve) => {
    resolveList = resolve;
  });
  const { client, calls, emit } = mockClient((command) => {
    if (command.type === 'process_list') {
      listCalls += 1;
      if (listCalls === 1) return pending;
      return defaultHandler(command);
    }
    return defaultHandler(command);
  });
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(
      <ProcessesPanel
        palette={DARK}
        client={client}
        sessionId={SESSION}
        onClose={() => {}}
        onError={(message) => errors.push(message)}
      />,
    );
  });
  await act(async () => {
    tree.unmount();
  });
  await act(async () => {
    resolveList(ok('process_list', [{ id: 'stale', state: 'running' }]));
  });
  expect(errors).toEqual([]);

  const live = mockClient((command) => defaultHandler(command));
  await withPanel(live.client, (message) => errors.push(message), async (tree) => {
    const before = commandsOf(live.calls, 'process_list').length;
    await act(async () => {
      live.emit({ type: 'process_exited', sessionId: OTHER_SESSION, process: EXITED });
    });
    expect(commandsOf(live.calls, 'process_list')).toHaveLength(before);
    await act(async () => {
      live.emit({ type: 'process_exited', process: EXITED });
    });
    expect(commandsOf(live.calls, 'process_list')).toHaveLength(before);
    await act(async () => {
      live.emit({ type: 'process_exited', sessionId: SESSION, process: EXITED });
    });
    expect(commandsOf(live.calls, 'process_list').length).toBeGreaterThan(before);
  });
});

it('appends process_output for the open process and Back from detail does not close', async () => {
  const closed: string[] = [];
  const { client, emit } = mockClient((command) => defaultHandler(command));
  await withPanel(client, () => {}, async (tree) => {
    await press(tree, 'Process sleep');
    expect(renderedText(tree)).toContain('hello');
    await act(async () => {
      emit({
        type: 'process_output',
        id: 'p1',
        dataBase64: encodeTmuxTerminalData(' leaked'),
        cursor: 12,
      });
    });
    expect(renderedText(tree)).not.toContain('leaked');
    await act(async () => {
      emit({
        type: 'process_output',
        sessionId: SESSION,
        id: 'p1',
        dataBase64: encodeTmuxTerminalData(' more'),
        cursor: 10,
      });
    });
    expect(renderedText(tree)).toContain('hello more');
    await press(tree, 'Back');
    expect(closed).toEqual([]);
    expect(renderedText(tree)).toContain('sleep');
    await press(tree, 'Back');
    expect(closed).toEqual(['closed']);
  }, () => {
    closed.push('closed');
  });
});

it('renders process log pages without the TextDecoder global available on Hermes', async () => {
  const descriptor = Object.getOwnPropertyDescriptor(globalThis, 'TextDecoder');
  Object.defineProperty(globalThis, 'TextDecoder', { configurable: true, writable: true, value: undefined });
  const errors: string[] = [];
  const { client } = mockClient((command) => command.type === 'process_logs'
    ? ok('process_logs', logsPage('native-process-ok café 中文 𝄞', 35, true))
    : defaultHandler(command));
  try {
    await withPanel(client, (message) => errors.push(message), async (tree) => {
      await press(tree, 'Process sleep');
      expect(renderedText(tree)).toContain('native-process-ok café 中文 𝄞');
      expect(errors).toEqual([]);
    });
  } finally {
    if (descriptor) Object.defineProperty(globalThis, 'TextDecoder', descriptor);
    else Reflect.deleteProperty(globalThis, 'TextDecoder');
  }
});

it('renders live process output without the TextDecoder global available on Hermes', async () => {
  const descriptor = Object.getOwnPropertyDescriptor(globalThis, 'TextDecoder');
  Object.defineProperty(globalThis, 'TextDecoder', { configurable: true, writable: true, value: undefined });
  const errors: string[] = [];
  const { client, emit } = mockClient((command) => command.type === 'process_logs'
    ? ok('process_logs', { ...logsPage('', 0, false), chunks: [] })
    : defaultHandler(command));
  try {
    await withPanel(client, (message) => errors.push(message), async (tree) => {
      await press(tree, 'Process sleep');
      await act(async () => {
        emit({ type: 'process_output', sessionId: SESSION, id: 'p1',
          dataBase64: encodeTmuxTerminalData('native-live-ok café 中文 𝄞'), cursor: 32 });
      });
      expect(renderedText(tree)).toContain('native-live-ok café 中文 𝄞');
      expect(errors).toEqual([]);
    });
  } finally {
    if (descriptor) Object.defineProperty(globalThis, 'TextDecoder', descriptor);
    else Reflect.deleteProperty(globalThis, 'TextDecoder');
  }
});
