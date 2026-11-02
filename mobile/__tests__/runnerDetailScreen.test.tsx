import React from 'react';
import { RefreshControl, TouchableOpacity } from 'react-native';
import renderer, { act } from 'react-test-renderer';
import { AnsiText } from '../src/AnsiText';
import { DARK } from '../src/ui';

jest.mock('../src/runtime', () => {
  const mockRemoveRunner = jest.fn(async () => {});
  const mockCommand = jest.fn(async (cmd: { type: string }) => {
    if (cmd.type === 'tmux_list_sessions') {
      return { success: true, data: { tmuxAvailable: true, sessions: [] } };
    }
    return { success: true, data: {} };
  });
  return {
    registry: {
      client: () => ({
        state: 'connected',
        command: mockCommand,
        probe: { sessionId: 's1', runtimeIncarnation: 'inc-1', cwd: '/work' },
      }),
    },
    mappingStore: { removeRunner: mockRemoveRunner },
    mockCommand,
  };
});

jest.mock('../src/screens/SessionWorkspaceScreen', () => ({
  SessionWorkspaceScreen: () => null,
}));

import { mappingStore } from '../src/runtime';
import { RunnerDetailScreen } from '../src/screens/RunnerDetailScreen';

const { mockCommand } = jest.requireMock('../src/runtime') as { mockCommand: jest.Mock };

async function flush(): Promise<void> {
  for (let tick = 0; tick < 8; tick += 1) {
    await act(async () => {
      await Promise.resolve();
    });
  }
}

function terminalButtons(tree: renderer.ReactTestRenderer, session: string) {
  return tree.root.findAllByType(TouchableOpacity)
    .filter((node) => node.props.accessibilityLabel === `Open terminal ${session}`);
}

const RUNNER = {
  id: 'r1',
  label: 'psy',
  origin: 'https://192.0.2.15:8765',
  authority: '192.0.2.15:8765',
  token: '',
  codename: 'psy',
  createdAt: 1,
  state: 'connected' as const,
  error: '',
  errorKind: null,
  probe: { sessionId: 's1', runtimeIncarnation: 'inc-1', cwd: '/work' },
};

it('deletes a runner only after a second Delete? press and Back disarms', async () => {
  const deleted = jest.fn();
  const back = jest.fn();
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(
      <RunnerDetailScreen
        palette={DARK}
        runnerId="r1"
        runners={[RUNNER]}
        onBack={back}
        onOpenTerminal={() => {}}
        onDeleted={deleted}
        onError={() => {}}
      />,
    );
  });
  await act(async () => { tree.root.findByProps({ accessibilityLabel: 'Delete runner' }).props.onPress(); });
  expect(mappingStore.removeRunner).not.toHaveBeenCalled();
  expect(deleted).not.toHaveBeenCalled();
  await act(async () => { tree.root.findByProps({ accessibilityLabel: 'Back' }).props.onPress(); });
  expect(back).not.toHaveBeenCalled();
  await act(async () => { tree.root.findByProps({ accessibilityLabel: 'Delete runner' }).props.onPress(); });
  await act(async () => { tree.root.findByProps({ accessibilityLabel: 'Delete?' }).props.onPress(); });
  expect(mappingStore.removeRunner).toHaveBeenCalled();
  expect(deleted).toHaveBeenCalledTimes(1);
  await act(async () => { tree.unmount(); });
});

it('surfaces a rejected runner delete and stays on the runner', async () => {
  (mappingStore.removeRunner as jest.Mock).mockImplementationOnce(async () => {
    throw new Error('runner persist failed');
  });
  const deleted = jest.fn();
  const errors: string[] = [];
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(
      <RunnerDetailScreen
        palette={DARK}
        runnerId="r1"
        runners={[RUNNER]}
        onBack={() => {}}
        onOpenTerminal={() => {}}
        onDeleted={deleted}
        onError={(message) => errors.push(message)}
      />,
    );
  });
  await act(async () => { tree.root.findByProps({ accessibilityLabel: 'Delete runner' }).props.onPress(); });
  await act(async () => { tree.root.findByProps({ accessibilityLabel: 'Delete?' }).props.onPress(); });
  expect(mappingStore.removeRunner).toHaveBeenCalled();
  expect(deleted).not.toHaveBeenCalled();
  expect(errors).toEqual(['runner persist failed']);
  expect(tree.root.findByProps({ accessibilityLabel: 'Delete?' })).toBeTruthy();
  await act(async () => { tree.unmount(); });
});

it('surfaces a rejected runner delete from the Session surface', async () => {
  (mappingStore.removeRunner as jest.Mock).mockImplementationOnce(async () => {
    throw new Error('session-surface persist failed');
  });
  const deleted = jest.fn();
  const errors: string[] = [];
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(
      <RunnerDetailScreen
        palette={DARK}
        runnerId="r1"
        runners={[RUNNER]}
        onBack={() => {}}
        onOpenTerminal={() => {}}
        onDeleted={deleted}
        onError={(message) => errors.push(message)}
      />,
    );
  });
  await act(async () => { tree.root.findByProps({ accessibilityLabel: 'Session' }).props.onPress(); });
  await act(async () => { tree.root.findByProps({ accessibilityLabel: 'Delete runner' }).props.onPress(); });
  await act(async () => { tree.root.findByProps({ accessibilityLabel: 'Delete?' }).props.onPress(); });
  expect(mappingStore.removeRunner).toHaveBeenCalled();
  expect(deleted).not.toHaveBeenCalled();
  expect(errors).toEqual(['session-surface persist failed']);
  await act(async () => { tree.unmount(); });
});

it('keeps the newer tmux session list when an older refresh settles last', async () => {
  mockCommand.mockClear();
  let resolveFirst!: (value: unknown) => void;
  let listCalls = 0;
  mockCommand.mockImplementation(async (cmd: { type: string; session?: string }) => {
    if (cmd.type === 'tmux_list_sessions') {
      listCalls += 1;
      if (listCalls === 1) {
        return await new Promise((resolve) => { resolveFirst = resolve; });
      }
      return { success: true, data: { tmuxAvailable: true, sessions: [{ name: 'new', windows: 1, attached: false }] } };
    }
    if (cmd.type === 'tmux_pane_snapshot') {
      return {
        success: true,
        data: { session: cmd.session, pane: '%0', width: 80, height: 24, content: `pane-${cmd.session}` },
      };
    }
    return { success: true, data: {} };
  });
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(
      <RunnerDetailScreen
        palette={DARK}
        runnerId="r1"
        runners={[RUNNER]}
        onBack={() => {}}
        onOpenTerminal={() => {}}
        onDeleted={() => {}}
        onError={() => {}}
      />,
    );
  });
  await act(async () => {
    tree.root.findByType(RefreshControl).props.onRefresh();
  });
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
  await act(async () => {
    resolveFirst({ success: true, data: { tmuxAvailable: true, sessions: [{ name: 'stale', windows: 1, attached: false }] } });
  });
  const texts = tree.root.findAllByType(require('react-native').Text)
    .flatMap((node) => {
      const children = node.props.children;
      if (typeof children === 'string') return [children];
      if (Array.isArray(children)) return children.filter((child: unknown) => typeof child === 'string') as string[];
      return [];
    });
  const rendered = texts.join(' ');
  expect(rendered).toContain('new');
  expect(rendered).not.toContain('stale');
  const calls = mockCommand.mock.calls as Array<[{ type: string; session?: string }]>;
  const listIndexes = calls.flatMap((call, index) => call[0].type === 'tmux_list_sessions' ? [index] : []);
  const paneCalls = calls.filter((call) => call[0].type === 'tmux_pane_snapshot');
  expect(listIndexes.length).toBeGreaterThanOrEqual(2);
  expect(paneCalls.length).toBeGreaterThan(0);
  expect(paneCalls.every((call) => call[0].session === 'new')).toBe(true);
  const firstPaneIndex = calls.findIndex((call) => call[0].type === 'tmux_pane_snapshot');
  expect(firstPaneIndex).toBeGreaterThan(listIndexes[1]);
  await act(async () => { tree.unmount(); });
});


it('deletes a tmux session only after the second Delete? press', async () => {
  mockCommand.mockClear();
  let deleted = false;
  mockCommand.mockImplementation(async (cmd: { type: string; session?: string }) => {
    if (cmd.type === 'tmux_list_sessions') {
      return {
        success: true,
        data: { tmuxAvailable: true, sessions: deleted ? [] : [{ name: 'work', windows: 1, attached: false }] },
      };
    }
    if (cmd.type === 'tmux_pane_snapshot') {
      return { success: true, data: { session: cmd.session, pane: '%0', width: 80, height: 24, content: `preview-${cmd.session}` } };
    }
    if (cmd.type === 'tmux_session_delete') {
      deleted = true;
      return { success: true, data: {} };
    }
    return { success: true, data: {} };
  });
  const errors: string[] = [];
  const openTerminal = jest.fn();
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(
      <RunnerDetailScreen
        palette={DARK}
        runnerId="r1"
        runners={[RUNNER]}
        onBack={() => {}}
        onOpenTerminal={openTerminal}
        onDeleted={() => {}}
        onError={(message) => errors.push(message)}
      />,
    );
  });
  await flush();
  expect(terminalButtons(tree, 'work')).toHaveLength(1);
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: 'Delete session work' }).props.onPress({ stopPropagation() {} });
  });
  expect(mockCommand.mock.calls.some((call: [{ type: string }]) => call[0].type === 'tmux_session_delete')).toBe(false);
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: 'Confirm delete work' }).props.onPress({ stopPropagation() {} });
  });
  await flush();
  const deleteCalls = (mockCommand.mock.calls as Array<[{ type: string; session?: string }]>).filter((call) => call[0].type === 'tmux_session_delete');
  expect(deleteCalls).toHaveLength(1);
  expect(deleteCalls[0][0].session).toBe('work');
  expect(errors).toEqual([]);
  expect(openTerminal).not.toHaveBeenCalled();
  expect(terminalButtons(tree, 'work')).toHaveLength(0);
  const texts = tree.root.findAllByType(require('react-native').Text)
    .flatMap((node) => {
      const children = node.props.children;
      if (typeof children === 'string') return [children];
      if (Array.isArray(children)) return children.filter((child: unknown) => typeof child === 'string') as string[];
      return [];
    });
  expect(texts).toContain('No tmux sessions');
  expect(texts.join(' ')).not.toContain('work');
  await act(async () => { tree.unmount(); });
});

it('reports the runner reason when tmux session delete is refused', async () => {
  mockCommand.mockImplementation(async (cmd: { type: string }) => {
    if (cmd.type === 'tmux_list_sessions') {
      return { success: true, data: { tmuxAvailable: true, sessions: [{ name: 'work', windows: 1, attached: false }] } };
    }
    if (cmd.type === 'tmux_session_delete') return { success: false, error: 'session is attached' };
    return { success: true, data: {} };
  });
  const errors: string[] = [];
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(
      <RunnerDetailScreen
        palette={DARK}
        runnerId="r1"
        runners={[RUNNER]}
        onBack={() => {}}
        onOpenTerminal={() => {}}
        onDeleted={() => {}}
        onError={(message) => errors.push(message)}
      />,
    );
  });
  await act(async () => { await Promise.resolve(); });
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: 'Delete session work' }).props.onPress({ stopPropagation() {} });
  });
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: 'Confirm delete work' }).props.onPress({ stopPropagation() {} });
  });
  expect(errors).toEqual(['session is attached']);
  await act(async () => { tree.unmount(); });
});

it('surfaces a rejected tmux session delete and recovers for retry', async () => {
  mockCommand.mockClear();
  let failures = 1;
  mockCommand.mockImplementation(async (cmd: { type: string; session?: string }) => {
    if (cmd.type === 'tmux_list_sessions') {
      return { success: true, data: { tmuxAvailable: true, sessions: [{ name: 'work', windows: 1, attached: false }] } };
    }
    if (cmd.type === 'tmux_pane_snapshot') {
      return { success: true, data: { session: cmd.session, pane: '%0', width: 80, height: 24, content: `preview-${cmd.session}` } };
    }
    if (cmd.type === 'tmux_session_delete') {
      if (failures > 0) {
        failures -= 1;
        throw new Error('delete transport failure');
      }
      return { success: true, data: {} };
    }
    return { success: true, data: {} };
  });
  const errors: string[] = [];
  const openTerminal = jest.fn();
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(
      <RunnerDetailScreen
        palette={DARK}
        runnerId="r1"
        runners={[RUNNER]}
        onBack={() => {}}
        onOpenTerminal={openTerminal}
        onDeleted={() => {}}
        onError={(message) => errors.push(message)}
      />,
    );
  });
  await flush();
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: 'Delete session work' }).props.onPress({ stopPropagation() {} });
  });
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: 'Confirm delete work' }).props.onPress({ stopPropagation() {} });
  });
  await flush();
  expect(errors).toEqual(['delete transport failure']);
  expect(openTerminal).not.toHaveBeenCalled();
  expect(terminalButtons(tree, 'work')).toHaveLength(1);
  expect(tree.root.findAllByType(AnsiText).map((node) => node.props.text)).toEqual(['preview-work']);
  // Confirmation released: the label is back to the first-press arm state.
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: 'Delete session work' }).props.onPress({ stopPropagation() {} });
  });
  // Busy released: the retry confirm reaches the command instead of being swallowed.
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: 'Confirm delete work' }).props.onPress({ stopPropagation() {} });
  });
  await flush();
  const deleteCalls = (mockCommand.mock.calls as Array<[{ type: string; session?: string }]>).filter((call) => call[0].type === 'tmux_session_delete');
  expect(deleteCalls).toHaveLength(2);
  expect(deleteCalls.every((call) => call[0].session === 'work')).toBe(true);
  expect(errors).toEqual(['delete transport failure']);
  expect(terminalButtons(tree, 'work')).toHaveLength(1);
  await act(async () => { tree.unmount(); });
});

it('keeps the session name when create is refused', async () => {
  mockCommand.mockImplementation(async (cmd: { type: string }) => {
    if (cmd.type === 'tmux_list_sessions') {
      return { success: true, data: { tmuxAvailable: true, sessions: [] } };
    }
    if (cmd.type === 'tmux_session_create') return { success: false, error: 'name exists' };
    return { success: true, data: {} };
  });
  const errors: string[] = [];
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(
      <RunnerDetailScreen
        palette={DARK}
        runnerId="r1"
        runners={[RUNNER]}
        onBack={() => {}}
        onOpenTerminal={() => {}}
        onDeleted={() => {}}
        onError={(message) => errors.push(message)}
      />,
    );
  });
  await act(async () => { await Promise.resolve(); });
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: 'Create session' }).props.onPress();
  });
  await act(async () => {
    tree.root.findByProps({ placeholder: 'session name' }).props.onChangeText('work');
  });
  const create = tree.root.findAll((node) => node.props?.onPress && node.props.children?.props?.children === 'Create')[0]
    ?? tree.root.findAllByType(require('react-native').TouchableOpacity).find((node) => {
      const label = node.findAllByType(require('react-native').Text).map((text) => text.props.children).join('');
      return label === 'Create';
    });
  expect(create).toBeTruthy();
  await act(async () => { create!.props.onPress(); });
  expect(errors).toEqual(['name exists']);
  expect(tree.root.findByProps({ placeholder: 'session name' }).props.value).toBe('work');
  await act(async () => { tree.unmount(); });
});