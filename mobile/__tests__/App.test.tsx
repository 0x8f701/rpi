/**
 * @format
 *
 * Startup behavior: the local profile (a runner saved on this device, its
 * mappings, its channel logs) is what the screens are built from, so the app
 * publishes its controls as soon as that local state is loaded. The remote
 * boot sync is a round trip to a runner that may be offline — a profile whose
 * paired runner stopped listening must still reach its own Runners controls
 * instead of spinning forever.
 */

import 'react-native';
import React from 'react';
import { ActivityIndicator, BackHandler, Text, TouchableOpacity } from 'react-native';
import AsyncStorage from '@react-native-async-storage/async-storage';
import App from '../App';
import { runMappingSync, wireMappingSync } from '../src/mappingSync';
import { loadPersistedChannelLogs } from '../src/channelLog';
import { RUNNER_STORAGE_KEY, TOKEN_STORAGE_KEY } from '../src/registry';
import renderer, { act } from 'react-test-renderer';
import { RunnersScreen } from '../src/screens/RunnersScreen';
import { RunnerDetailScreen } from '../src/screens/RunnerDetailScreen';
import { ProjectsScreen, NewProjectScreen } from '../src/screens/ProjectsScreen';
import { ProjectChannelScreen } from '../src/screens/ProjectChannelScreen';
import { AddRunnerScreen } from '../src/screens/AddRunnerScreen';
import { SideChatScreen } from '../src/screens/SideChatScreen';
import { TmuxTerminalScreen } from '../src/screens/TmuxTerminalScreen';

jest.mock('../src/mappingSync', () => ({
  runMappingSync: jest.fn(),
  wireMappingSync: jest.fn(),
}));

jest.mock('../src/channelLog', () => ({
  ...jest.requireActual('../src/channelLog'),
  loadPersistedChannelLogs: jest.fn(),
}));

const runSync = runMappingSync as unknown as jest.Mock;
const wireSync = wireMappingSync as unknown as jest.Mock;
const hydrateLogs = loadPersistedChannelLogs as unknown as jest.Mock;

/** Stand-in for the React Native WebSocket global: the saved row still dials on
 *  startup, and a dial that never completes must not schedule reconnect work
 *  inside the test. */
class FakeSocket {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;

  readyState: number = FakeSocket.CONNECTING;
  onopen: (() => void) | null = null;
  onmessage: ((event: { data: string }) => void) | null = null;
  onclose: ((event: { code: number; reason: string }) => void) | null = null;
  onerror: (() => void) | null = null;

  constructor(readonly url: string, readonly protocols: string[] = []) {}

  send(): void {}

  close(): void {
    this.readyState = FakeSocket.CLOSED;
  }
}

const transportGlobal = globalThis as { WebSocket?: unknown };
const realWebSocket = transportGlobal.WebSocket;

beforeAll(() => {
  transportGlobal.WebSocket = FakeSocket;
});

afterAll(() => {
  transportGlobal.WebSocket = realWebSocket;
});

beforeEach(async () => {
  await AsyncStorage.clear();
  runSync.mockReset();
  wireSync.mockReset();
  hydrateLogs.mockReset();
  hydrateLogs.mockResolvedValue(undefined);
});

/** A profile whose paired runner is saved on this device but whose listener is
 *  gone: exactly the profile the boot sync hangs on. TEST-NET-1 (RFC 5737) is
 *  reserved for documentation, so no real runner address can appear here. */
async function savedRunner(id: string, authority: string, token: string): Promise<void> {
  await AsyncStorage.setItem(
    RUNNER_STORAGE_KEY,
    JSON.stringify({
      version: 1,
      runners: [{ id, label: 'psy', origin: `http://${authority}`, authority, codename: null, createdAt: 1 }],
    }),
  );
  await AsyncStorage.setItem(TOKEN_STORAGE_KEY, JSON.stringify({ version: 1, tokens: { [authority]: token } }));
}

function labels(tree: renderer.ReactTestRenderer): string[] {
  return tree.root
    .findAll((node) => typeof node.props?.accessibilityLabel === 'string')
    .map((node) => node.props.accessibilityLabel as string);
}

function texts(tree: renderer.ReactTestRenderer): string[] {
  return tree.root
    .findAllByType(Text)
    .map((node) => node.props.children)
    .filter((value): value is string => typeof value === 'string');
}

it('renders correctly', async () => {
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(<App />);
  });
  await act(async () => {
    tree.unmount();
  });
});

it('publishes the Runners controls while the remote boot sync is still pending', async () => {
  await savedRunner('runner-boot-pending', '192.0.2.15:8765', 'boot-token');
  let release!: () => void;
  runSync.mockImplementation(() => new Promise<void>((resolve) => {
    release = () => resolve();
  }));

  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(<App />);
  });
  await act(async () => {});

  // The boot sync really started and really is still in flight.
  expect(runSync).toHaveBeenCalledWith(expect.anything(), expect.anything(), 'boot');
  expect(release).toBeDefined();
  // Wiring is exactly one call, and it happens before the boot sync can push.
  expect(wireSync).toHaveBeenCalledTimes(1);
  expect(wireSync.mock.invocationCallOrder[0]).toBeLessThan(runSync.mock.invocationCallOrder[0]);

  // The saved local runner and every local control are on screen with no spinner.
  expect(texts(tree)).toContain('psy');
  expect(labels(tree)).toEqual(expect.arrayContaining(['Runners', 'Projects', 'Side chat', 'Add runner']));
  expect(tree.root.findAllByType(ActivityIndicator)).toHaveLength(0);

  // The sync completing does not take readiness back.
  await act(async () => {
    release();
  });
  expect(texts(tree)).toContain('psy');
  expect(labels(tree)).toEqual(expect.arrayContaining(['Add runner']));
  expect(tree.root.findAllByType(ActivityIndicator)).toHaveLength(0);

  await act(async () => {
    tree.unmount();
  });
});

it('surfaces a rejected remote boot sync without hiding the local controls', async () => {
  await savedRunner('runner-boot-rejected', '192.0.2.16:8765', 'boot-token');
  runSync.mockRejectedValue(new Error('chat_mapping_get failed: 500'));

  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(<App />);
  });
  await act(async () => {});

  expect(texts(tree)).toContain('chat_mapping_get failed: 500');
  expect(texts(tree)).toContain('psy');
  expect(labels(tree)).toEqual(expect.arrayContaining(['Runners', 'Projects', 'Side chat', 'Add runner']));
  expect(tree.root.findAllByType(ActivityIndicator)).toHaveLength(0);

  await act(async () => {
    tree.unmount();
  });
});

it('handles hardware back through the current route and releases it at home and unmount', async () => {
  await savedRunner('runner-back', '192.0.2.17:8765', 'back-token');
  runSync.mockResolvedValue(undefined);
  type BackListener = Parameters<typeof BackHandler.addEventListener>[1];
  const listeners = new Set<BackListener>();
  const subscribe = jest.spyOn(BackHandler, 'addEventListener').mockImplementation((event, listener) => {
    expect(event).toBe('hardwareBackPress');
    listeners.add(listener);
    return { remove: () => { listeners.delete(listener); } };
  });
  const hardwareBack = (): boolean => {
    for (const listener of Array.from(listeners).reverse()) {
      if (listener()) return true;
    }
    return false;
  };
  let tree!: renderer.ReactTestRenderer;
  const press = (label: string) => {
    tree.root.findAllByType(TouchableOpacity)
      .find((node) => node.props.accessibilityLabel === label)!.props.onPress();
  };
  try {
    await act(async () => { tree = renderer.create(<App />); });
    expect(hardwareBack()).toBe(false);
    act(() => { tree.root.findByType(RunnersScreen).props.onOpenRunner('runner-back'); });
    act(() => { press('Session'); });
    expect(tree.root.findByType(RunnerDetailScreen).props.runnerId).toBe('runner-back');
    act(() => { expect(hardwareBack()).toBe(true); });
    expect(tree.root.findAllByType(RunnerDetailScreen)).toHaveLength(0);
    expect(tree.root.findByType(RunnersScreen).props.runners[0].id).toBe('runner-back');
    expect(hardwareBack()).toBe(false);

    act(() => { press('Add runner'); });
    expect(tree.root.findAllByType(AddRunnerScreen)).toHaveLength(1);
    act(() => { expect(hardwareBack()).toBe(true); });
    expect(tree.root.findAllByType(RunnersScreen)).toHaveLength(1);

    act(() => { tree.root.findByType(RunnersScreen).props.onOpenRunner('runner-back'); });
    act(() => { press('Open side chat'); });
    expect(tree.root.findAllByType(SideChatScreen)).toHaveLength(1);
    act(() => { expect(hardwareBack()).toBe(true); });
    expect(tree.root.findByType(RunnerDetailScreen).props.runnerId).toBe('runner-back');
    act(() => { tree.root.findByType(RunnerDetailScreen).props.onOpenTerminal('test-session'); });
    expect(tree.root.findByType(TmuxTerminalScreen).props.session).toBe('test-session');
    act(() => { expect(hardwareBack()).toBe(true); });
    expect(tree.root.findByType(RunnerDetailScreen).props.runnerId).toBe('runner-back');
    act(() => { expect(hardwareBack()).toBe(true); });
    expect(tree.root.findAllByType(RunnersScreen)).toHaveLength(1);

    act(() => { press('Projects'); });
    expect(hardwareBack()).toBe(false);
    act(() => { tree.root.findByType(ProjectsScreen).props.onNewProject(); });
    expect(tree.root.findAllByType(NewProjectScreen)).toHaveLength(1);
    act(() => { expect(hardwareBack()).toBe(true); });
    expect(tree.root.findAllByType(ProjectsScreen)).toHaveLength(1);
    act(() => { tree.root.findByType(ProjectsScreen).props.onOpenProject('removed-project'); });
    expect(tree.root.findByType(ProjectChannelScreen).props.projectId).toBe('removed-project');
    act(() => { expect(hardwareBack()).toBe(true); });
    expect(tree.root.findAllByType(ProjectsScreen)).toHaveLength(1);
    expect(hardwareBack()).toBe(false);
    act(() => { press('Side chat'); });
    expect(hardwareBack()).toBe(false);
    expect(listeners.size).toBe(1);
  } finally {
    if (tree) act(() => { tree.unmount(); });
    subscribe.mockRestore();
  }
  expect(listeners.size).toBe(0);
  expect(hardwareBack()).toBe(false);
});
