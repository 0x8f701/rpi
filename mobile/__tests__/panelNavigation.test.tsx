import React from 'react';
import { BackHandler, Modal, Text, TouchableOpacity } from 'react-native';
import renderer, { act } from 'react-test-renderer';

/** Recorder factory for a panel screen. Hoisted as `mock*` so jest.mock
 *  factories can call it while intercepting the workspace's `./` imports. */
function mockPanelModule(name: string) {
  const ReactModule = require('react');
  const { Text: NativeText, TouchableOpacity, View } = require('react-native');
  const self = mockPanelModule as typeof mockPanelModule & { mounts: Record<string, number> };
  if (self.mounts === undefined) self.mounts = {};
  function Panel(props: {
    palette: unknown;
    client: unknown;
    sessionId: string;
    onClose: () => void;
    onError: (message: string) => void;
  }) {
    const mountId = ReactModule.useRef(0);
    if (mountId.current === 0) {
      const mounts = self.mounts;
      mounts[name] = (mounts[name] ?? 0) + 1;
      mountId.current = mounts[name];
    }
    return ReactModule.createElement(
      View,
      { testID: `panel:${name}`, mountId: mountId.current },
      ReactModule.createElement(NativeText, null, `${name}:${props.sessionId}`),
      ReactModule.createElement(TouchableOpacity, {
        accessibilityLabel: 'Back',
        onPress: props.onClose,
      }),
    );
  }
  return { [name]: Panel };
}

jest.mock('../src/screens/LoopPanel', () => mockPanelModule('LoopPanel'));
jest.mock('../src/screens/FileExplorerPanel', () => jest.requireActual('../src/screens/FileExplorerPanel'));
jest.mock('../src/screens/GoalPanel', () => mockPanelModule('GoalPanel'));
jest.mock('../src/screens/JobsPanel', () => mockPanelModule('JobsPanel'));
jest.mock('../src/screens/PersonasPanel', () => mockPanelModule('PersonasPanel'));
jest.mock('../src/screens/ProcessesPanel', () => jest.requireActual('../src/screens/ProcessesPanel'));
jest.mock('../src/screens/ReviewPanel', () => jest.requireActual('../src/screens/ReviewPanel'));
jest.mock('../src/screens/SessionSettingsPanel', () => mockPanelModule('SessionSettingsPanel'));
jest.mock('../src/screens/SettingsPanel', () => mockPanelModule('SettingsPanel'));
jest.mock('../src/screens/TodoPanel', () => mockPanelModule('TodoPanel'));
jest.mock('../src/screens/ActivityPanel', () => mockPanelModule('ActivityPanel'));
jest.mock('../src/screens/WorkflowPanel', () => mockPanelModule('WorkflowPanel'));

const CATALOG = {
  sessions: [
    { sessionId: 's1', path: '/p/a.jsonl', cwd: '/w', name: 'Alpha', runtimeIncarnation: 'inc1', status: 'native', loaded: true },
    { sessionId: 's2', path: '/p/b.jsonl', cwd: '/w', name: 'Beta', runtimeIncarnation: 'inc2', status: 'native', loaded: true },
  ],
};

const ROW_IDENTITY: Record<string, { sessionId: string; runtimeIncarnation: string }> = {
  '/p/a.jsonl': { sessionId: 's1', runtimeIncarnation: 'inc1' },
  '/p/b.jsonl': { sessionId: 's2', runtimeIncarnation: 'inc2' },
};

const mockCommand = jest.fn(async (command: { type: string; [key: string]: unknown }, sourceSessionId?: string) => {
  const ok = (data: unknown) => ({ type: 'response', command: command.type, success: true, data });
  switch (command.type) {
    case 'session_list': return ok(CATALOG);
    case 'switch_session': return ok(ROW_IDENTITY[String(command.sessionPath)] ?? null);
    case 'get_messages': return ok({
      messages: [{
        id: `m-${sourceSessionId ?? ''}`,
        message: { role: 'assistant', content: [{ type: 'text', text: `body of ${sourceSessionId ?? ''}` }] },
      }],
    });
    case 'get_session_stats': return ok({});
    case 'queue_list': return ok({ steering: [], followUp: [], total: 0 });
    default: return { type: 'response', command: command.type, success: false, data: null, error: `unhandled ${command.type}` };
  }
});

let mockActiveClient = {
  state: 'connected',
  probe: { sessionId: 's1', runtimeIncarnation: 'inc1', cwd: '/w' },
  command: mockCommand,
  subscribe: jest.fn(),
  subscribeFrames: jest.fn(() => () => {}),
  subscribeReconnect: jest.fn(() => () => {}),
  hasOpenSocket: () => true,
  resumeFromBackground: jest.fn(),
};

const mockRunner = {
  id: 'r1',
  label: 'runner',
  origin: 'http://127.0.0.1:8787',
  authority: '127.0.0.1:8787',
  token: 'tok',
  codename: null,
  createdAt: 0,
  state: 'connected',
  error: '',
  errorKind: null,
  probe: null,
  allocationHint: null,
};

const PROCESS_FIXTURE = {
  id: 'back-process', ownerId: 'owner', label: 'back-process-fixture', state: 'exited',
  pid: null, tty: false, startedAtMs: 1, exitedAtMs: 2, exitCode: 0,
  outputStartCursor: 0, outputCursor: 0,
};

jest.mock('../src/runtime', () => ({
  registry: {
    client: () => mockActiveClient,
    snapshot: () => [mockRunner],
    load: jest.fn(async () => {}),
    connectAll: jest.fn(),
    subscribe: (listener: (rows: typeof mockRunner[]) => void) => { listener([mockRunner]); return () => {}; },
  },
  mappingStore: {
    load: jest.fn(async () => {}),
    subscribe: () => () => {},
    projectRows: () => [{ id: 'back-project', label: 'BackFixture', cwd: '/tmp/back-fixture', createdAt: 0 }],
    mappingsForProject: () => [],
  },
  lanDiscovery: { subscribe: () => () => {}, bind: () => () => {} },
}));

jest.mock('../src/mappingSync', () => ({ runMappingSync: jest.fn(async () => {}), wireMappingSync: jest.fn() }));
jest.mock('../src/channelLog', () => ({ ...jest.requireActual('../src/channelLog'), loadPersistedChannelLogs: jest.fn(async () => {}) }));
jest.mock('react-native-webview', () => {
  const ReactModule = require('react');
  const { View } = require('react-native');
  return {
    WebView: ReactModule.forwardRef((props: Record<string, unknown>, ref: unknown) => {
      ReactModule.useImperativeHandle(ref, () => ({ injectJavaScript: () => {} }));
      return ReactModule.createElement(View, { testID: 'picker' });
    }),
  };
});

import { LoopPanel } from '../src/screens/LoopPanel';
import { FileExplorerPanel } from '../src/screens/FileExplorerPanel';
import { GoalPanel } from '../src/screens/GoalPanel';
import { JobsPanel } from '../src/screens/JobsPanel';
import { PersonasPanel } from '../src/screens/PersonasPanel';
import { ProcessesPanel } from '../src/screens/ProcessesPanel';
import { ReviewPanel } from '../src/screens/ReviewPanel';
import { SessionSettingsPanel } from '../src/screens/SessionSettingsPanel';
import { SessionWorkspaceScreen } from '../src/screens/SessionWorkspaceScreen';
import { SettingsPanel } from '../src/screens/SettingsPanel';
import { TodoPanel } from '../src/screens/TodoPanel';
import { ActivityPanel } from '../src/screens/ActivityPanel';
import { WorkflowPanel } from '../src/screens/WorkflowPanel';
import { DARK } from '../src/ui';
import App from '../App';
import { RunnersScreen } from '../src/screens/RunnersScreen';
import { RunnerDetailScreen } from '../src/screens/RunnerDetailScreen';
import { ProjectsScreen } from '../src/screens/ProjectsScreen';
import { ProjectChannelScreen } from '../src/screens/ProjectChannelScreen';

const CHOOSER_IDS = [
  'sessions',
  'goal',
  'todo',
  'activity',
  'jobs',
  'files',
  'review',
  'session-settings',
  'processes',
  'personas',
  'loop',
  'workflow',
  'settings',
] as const;

const SCREEN_ROWS: ReadonlyArray<{
  id: Exclude<(typeof CHOOSER_IDS)[number], 'sessions'>;
  Component: React.ComponentType<{
    palette: unknown;
    client: unknown;
    sessionId: string;
    onClose: () => void;
    onError: (message: string) => void;
  }>;
}> = [
  { id: 'goal', Component: GoalPanel },
  { id: 'todo', Component: TodoPanel },
  { id: 'activity', Component: ActivityPanel },
  { id: 'jobs', Component: JobsPanel },
  { id: 'files', Component: FileExplorerPanel },
  { id: 'review', Component: ReviewPanel },
  { id: 'session-settings', Component: SessionSettingsPanel },
  { id: 'processes', Component: ProcessesPanel },
  { id: 'personas', Component: PersonasPanel },
  { id: 'loop', Component: LoopPanel },
  { id: 'workflow', Component: WorkflowPanel },
  { id: 'settings', Component: SettingsPanel },
];

function labels(tree: renderer.ReactTestRenderer): string[] {
  return tree.root
    .findAllByType(Text)
    .flatMap((node) => (Array.isArray(node.props.children) ? node.props.children : [node.props.children]))
    .filter((value): value is string => typeof value === 'string');
}

function labeled(tree: renderer.ReactTestRenderer, label: string) {
  return tree.root.findAllByType(TouchableOpacity).filter((node) => (
    node.props.accessibilityLabel === label && typeof node.props.onPress === 'function'
  ));
}

/** RN iOS Modal keeps children after `visible` flips false; only the currently
 *  shown overlay is an actionable control. */
function visibleLabeled(tree: renderer.ReactTestRenderer, label: string) {
  return tree.root
    .findAllByType(Modal)
    .filter((node) => node.props.visible === true)
    .flatMap((node) => node.findAllByType(TouchableOpacity).filter((child) => (
      child.props.accessibilityLabel === label && typeof child.props.onPress === 'function'
    )));
}

async function mountWorkspace(overlayToast?: React.ReactNode) {
  const onError = jest.fn();
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(
      <SessionWorkspaceScreen palette={DARK} runnerId="r1" onError={onError} overlayToast={overlayToast} />,
    );
  });
  const press = async (label: string) => {
    const overlay = visibleLabeled(tree, label);
    const control = overlay[0] ?? labeled(tree, label)[0];
    await act(async () => {
      control.props.onPress();
    });
    await act(async () => {});
  };
  return { tree, onError, press };
}

beforeEach(() => {
  mockCommand.mockReset();
  mockCommand.mockImplementation(async (command: { type: string; [key: string]: unknown }, sourceSessionId?: string) => {
    const ok = (data: unknown) => ({ type: 'response', command: command.type, success: true, data });
    switch (command.type) {
      case 'session_list': return ok(CATALOG);
      case 'switch_session': return ok(ROW_IDENTITY[String(command.sessionPath)] ?? null);
      case 'get_messages': return ok({
        messages: [{
          id: `m-${sourceSessionId ?? ''}`,
          message: { role: 'assistant', content: [{ type: 'text', text: `body of ${sourceSessionId ?? ''}` }] },
        }],
      });
      case 'get_session_stats': return ok({});
      case 'queue_list': return ok({ steering: [], followUp: [], total: 0 });
      case 'file_explorer_list': return ok({ entries: [{ name: 'preview.txt', path: 'preview.txt', kind: 'file' }] });
      case 'file_explorer_read': return ok({ path: 'preview.txt', lines: ['first preview line', 'second preview line'], totalLines: 2, sizeBytes: 40 });
      case 'get_state': return ok({ sessionId: 's1', runtimeIncarnation: 'inc1', cwd: '/w' });
      case 'process_list': return ok([PROCESS_FIXTURE]);
      case 'process_describe': return ok(PROCESS_FIXTURE);
      case 'process_logs': return ok({ requestedCursor: 0, startCursor: 0, cursor: 0, eof: true, lost: false, lostBytes: 0, chunks: [] });
      case 'get_available_models': return ok({ models: [] });
      case 'code_review_snapshot': return ok({
        comparisonLabel: 'HEAD → working tree', snapshotId: 'back-snapshot', reviewId: 'back-review',
        totalInsertions: 1, totalDeletions: 1, totalsComplete: true,
        files: [{ path: 'preview.txt', status: 'modified', bodyState: 'complete', hunks: [{
          header: '@@ -1 +1 @@', oldStart: 1, oldCount: 1, newStart: 1, newCount: 1,
          contentHash: 'back-hunk', lines: [{ kind: 'deletion', text: '-old-review-line' }, { kind: 'addition', text: '+new-review-line' }],
        }] }],
      });
      case 'code_review_file_diff': return ok({
        snapshotId: 'back-snapshot', path: 'preview.txt', previousPath: null, binary: false, status: 'modified',
        cursor: 0, nextCursor: null, hasMore: false, totalLines: 2, hunkCount: 1, byteCapped: false,
        hunks: [{
          index: 0, header: '@@ -1 +1 @@', oldStart: 1, oldCount: 1, newStart: 1, newCount: 1,
          contentHash: 'back-hunk', totalLines: 2, complete: true, lineStart: 0,
          lines: [{ kind: 'deletion', text: '-old-review-line' }, { kind: 'addition', text: '+new-review-line' }],
        }],
      });
      case 'code_review_close': return ok(null);
      default: return { type: 'response', command: command.type, success: false, data: null, error: `unhandled ${command.type}` };
    }
  });
  mockActiveClient = {
    state: 'connected',
    probe: { sessionId: 's1', runtimeIncarnation: 'inc1', cwd: '/w' },
    command: mockCommand,
    subscribe: jest.fn(),
    subscribeFrames: jest.fn(() => () => {}),
    subscribeReconnect: jest.fn(() => () => {}),
    hasOpenSocket: () => true,
    resumeFromBackground: jest.fn(),
  };
  (mockPanelModule as typeof mockPanelModule & { mounts?: Record<string, number> }).mounts = {};
});

it('opens a scrollable chooser from one Panels control and closes it with the quiet close glyph', async () => {
  const { tree, press } = await mountWorkspace();
  expect(labeled(tree, 'Panels')).toHaveLength(1);
  expect(labeled(tree, 'Send')).toHaveLength(1);
  expect(labeled(tree, 'Stop')).toHaveLength(0);
  for (const id of CHOOSER_IDS) {
    expect(visibleLabeled(tree, id)).toHaveLength(0);
  }

  await press('Panels');
  expect(visibleLabeled(tree, 'Close panels')).toHaveLength(1);
  expect(labels(tree)).toEqual(expect.arrayContaining([
    'Sessions',
    'Goal',
    'Todo',
    'Activity',
    'Jobs',
    'Files',
    'Review',
    'Session settings',
    'Processes',
    'Personas',
    'Loop',
    'Workflow',
    'Settings',
  ]));
  for (const id of CHOOSER_IDS) {
    expect(visibleLabeled(tree, id)).toHaveLength(1);
  }
  expect(labeled(tree, 'Panels')).toHaveLength(1);
  expect(labeled(tree, 'Send')).toHaveLength(1);

  await press('Close panels');
  expect(visibleLabeled(tree, 'Close panels')).toHaveLength(0);
  for (const id of CHOOSER_IDS) {
    expect(visibleLabeled(tree, id)).toHaveLength(0);
  }
  expect(labeled(tree, 'Panels')).toHaveLength(1);
  expect(JSON.stringify(tree.toJSON())).toContain('Message');
});

it('mounts every panel with the active session, the real client, and the workspace onError', async () => {
  const { tree, onError, press } = await mountWorkspace();
  for (const row of SCREEN_ROWS) {
    await press('Panels');
    await press(row.id);
    const node = tree.root.findByType(row.Component);
    expect(node.props.palette).toBe(DARK);
    expect(node.props.client).toBe(mockActiveClient);
    expect(node.props.sessionId).toBe('s1');
    expect(node.props.onError).toBe(onError);
    expect(labeled(tree, 'Panels')).toHaveLength(0);
    await press('Back');
    expect(tree.root.findAllByType(row.Component)).toHaveLength(0);
    expect(labeled(tree, 'Panels')).toHaveLength(1);

    expect(JSON.stringify(tree.toJSON())).toContain('Message');
  }
});

it('keeps Session settings and Settings as distinct screens', async () => {
  const { tree, press } = await mountWorkspace();
  await press('Panels');
  await press('session-settings');
  expect(tree.root.findAllByType(SessionSettingsPanel)).toHaveLength(1);
  expect(tree.root.findAllByType(SettingsPanel)).toHaveLength(0);
  await press('Back');
  await press('Panels');
  await press('settings');
  expect(tree.root.findAllByType(SettingsPanel)).toHaveLength(1);
  expect(tree.root.findAllByType(SessionSettingsPanel)).toHaveLength(0);
});

it('opens the sessions menu from the chooser without replacing the workspace', async () => {
  const { tree, press } = await mountWorkspace();
  await press('Panels');
  await press('sessions');
  expect(visibleLabeled(tree, 'Close panels')).toHaveLength(0);
  expect(visibleLabeled(tree, 'Resume Alpha')).toHaveLength(1);
  expect(visibleLabeled(tree, 'Resume Beta')).toHaveLength(1);
  expect(labeled(tree, 'Panels')).toHaveLength(1);
  expect(labeled(tree, 'Send')).toHaveLength(1);
  expect(JSON.stringify(tree.toJSON())).toContain('Message');
  for (const row of SCREEN_ROWS) {
    expect(tree.root.findAllByType(row.Component)).toHaveLength(0);
  }

  await press('Back');
  expect(visibleLabeled(tree, 'Resume Alpha')).toHaveLength(0);
  expect(labeled(tree, 'Panels')).toHaveLength(1);
});

it('hands a newly opened panel the session the workspace switched to', async () => {
  const { tree, onError, press } = await mountWorkspace();
  await press('Panels');
  await press('sessions');
  await press('Resume Beta');
  expect(JSON.stringify(tree.toJSON())).toContain('body of s2');

  await press('Panels');
  await press('goal');
  const node = tree.root.findByType(GoalPanel);
  expect(node.props.client).toBe(mockActiveClient);
  expect(node.props.sessionId).toBe('s2');
  expect(node.props.palette).toBe(DARK);
  expect(node.props.onError).toBe(onError);
});

it('does not open session-scoped panels before a runtime identity exists', async () => {
  let resolveList!: (value: unknown) => void;
  const pending = new Promise((resolve) => {
    resolveList = resolve;
  });
  mockCommand.mockImplementation(async (command) => {
    if (command.type === 'session_list') return pending;
    if (command.type === 'queue_list') return { type: 'response', command: command.type, success: true, data: { steering: [], followUp: [], total: 0 } };
    return { type: 'response', command: command.type, success: false, data: null, error: `unhandled ${command.type}` };
  });
  const onError = jest.fn();
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(<SessionWorkspaceScreen palette={DARK} runnerId="r1" onError={onError} />);
  });
  const closed = labeled(tree, 'Close panels');
  expect(closed).toHaveLength(0);
  await act(async () => {
    labeled(tree, 'Panels')[0].props.onPress();
  });
  expect(labeled(tree, 'Close panels')).toHaveLength(0);
  expect(tree.root.findAllByType(GoalPanel)).toHaveLength(0);
  expect(mockCommand.mock.calls.every((call) => call[0].type !== 'goal_get')).toBe(true);
  await act(async () => {
    resolveList({
      type: 'response',
      command: 'session_list',
      success: true,
      data: CATALOG,
    });
  });
  await act(async () => {
    tree.unmount();
  });
});


it('rebinds reconnect to the get_state probe when the catalog no longer contains the active session', async () => {
  const { tree, onError, press } = await mountWorkspace();
  expect(JSON.stringify(tree.toJSON())).toContain('body of s1');
  const reconnect = mockActiveClient.subscribeReconnect.mock.calls[0][0] as () => void;
  mockActiveClient.probe = { sessionId: 's2', runtimeIncarnation: 'inc2', cwd: '/w' };
  mockCommand.mockImplementation(async (command: { type: string; [key: string]: unknown }, sourceSessionId?: string) => {
    const ok = (data: unknown) => ({ type: 'response', command: command.type, success: true, data });
    if (command.type === 'session_list') return ok({ sessions: [CATALOG.sessions[1]] });
    if (command.type === 'get_messages') {
      return ok({
        messages: [{
          id: `m-${sourceSessionId ?? ''}`,
          message: { role: 'assistant', content: [{ type: 'text', text: `body of ${sourceSessionId ?? ''}` }] },
        }],
      });
    }
    if (command.type === 'get_session_stats') return ok({});
    if (command.type === 'queue_list') return ok({ steering: [], followUp: [], total: 0 });
    return { type: 'response', command: command.type, success: false, data: null, error: `unhandled ${command.type}` };
  });
  await act(async () => {
    reconnect();
  });
  await act(async () => {});
  expect(JSON.stringify(tree.toJSON())).toContain('body of s2');
  expect(JSON.stringify(tree.toJSON())).not.toContain('body of s1');
  const scopedAfterRebind = mockCommand.mock.calls.filter((call) => call[1] === 's1').length;
  await press('Panels');
  await press('goal');
  expect(tree.root.findByType(GoalPanel).props.sessionId).toBe('s2');
  expect(mockCommand.mock.calls.filter((call) => call[1] === 's1').length).toBe(scopedAfterRebind);
  expect(onError).not.toHaveBeenCalled();
});

it('renders the app error toast inside the open panel modal', async () => {
  const { tree, press } = await mountWorkspace(<Text>toast-marker</Text>);
  await press('Panels');
  await press('sessions');
  const shown = tree.root.findAllByType(Modal).filter((node) => node.props.visible === true);
  expect(shown.length).toBeGreaterThan(0);
  expect(shown.every((node) => node.findAllByType(Text).some((child) => child.props.children === 'toast-marker'))).toBe(true);
});

it('remounts an open panel when the owning runtime identity changes', async () => {
  const { tree, onError, press } = await mountWorkspace();
  await press('Panels');
  await press('goal');
  expect(tree.root.findByProps({ testID: 'panel:GoalPanel' }).props.mountId).toBe(1);
  expect(tree.root.findByType(GoalPanel).props.sessionId).toBe('s1');

  mockActiveClient = {
    ...mockActiveClient,
    probe: { sessionId: 's2', runtimeIncarnation: 'inc2', cwd: '/w' },
  };
  await act(async () => {
    tree.update(<SessionWorkspaceScreen palette={DARK} runnerId="r1" onError={onError} />);
  });
  await act(async () => {});

  const node = tree.root.findByType(GoalPanel);
  expect(tree.root.findByProps({ testID: 'panel:GoalPanel' }).props.mountId).toBe(2);
  expect(node.props.sessionId).toBe('s2');
  expect(node.props.client).toBe(mockActiveClient);
  expect(node.props.palette).toBe(DARK);
  expect(node.props.onError).toBe(onError);
});

it('closes workspace panels before the runner route and removes the floating action from panels', async () => {
  const listeners = new Set<Parameters<typeof BackHandler.addEventListener>[1]>();
  const subscribe = jest.spyOn(BackHandler, 'addEventListener').mockImplementation((_event, listener) => {
    listeners.add(listener);
    return { remove: () => { listeners.delete(listener); } };
  });
  const hardwareBack = () => {
    for (const listener of Array.from(listeners).reverse()) if (listener()) return true;
    return false;
  };
  let tree!: renderer.ReactTestRenderer;
  const press = async (label: string) => {
    await act(async () => { (visibleLabeled(tree, label)[0] ?? labeled(tree, label)[0]).props.onPress(); });
  };
  try {
    await act(async () => { tree = renderer.create(<App />); });
    await act(async () => { tree.root.findByType(RunnersScreen).props.onOpenRunner('r1'); });
    await press('Session');
    expect(labeled(tree, 'Open side chat')).toHaveLength(1);
    await press('Panels');
    const chooser = tree.root.findAllByType(Modal).find((node) => node.props.visible);
    await act(async () => { chooser!.props.onRequestClose(); });
    expect(visibleLabeled(tree, 'session-settings')).toHaveLength(0);
    expect(tree.root.findAllByType(RunnerDetailScreen)).toHaveLength(1);
    for (const id of ['session-settings', 'settings']) {
      await press('Panels');
      await press(id);
      expect(labeled(tree, 'Open side chat')).toHaveLength(0);
      expect(labeled(tree, 'Panels')).toHaveLength(0);
      act(() => { expect(hardwareBack()).toBe(true); });
      expect(tree.root.findAllByType(RunnerDetailScreen)).toHaveLength(1);
      expect(labeled(tree, 'Panels')).toHaveLength(1);
      expect(labeled(tree, 'Send')).toHaveLength(1);
      expect(labeled(tree, 'Open side chat')).toHaveLength(1);
    }
    act(() => { expect(hardwareBack()).toBe(true); });
    expect(tree.root.findAllByType(RunnerDetailScreen)).toHaveLength(0);
    expect(tree.root.findAllByType(RunnersScreen)).toHaveLength(1);
    expect(hardwareBack()).toBe(false);
    await act(async () => { tree.root.findByType(RunnersScreen).props.onOpenRunner('r1'); });
    await press('Session');
    await press('Panels');
    await press('session-settings');
    await press('Terminals');
    expect(labeled(tree, 'Open side chat')).toHaveLength(1);
    act(() => { expect(hardwareBack()).toBe(true); });
    expect(tree.root.findAllByType(RunnersScreen)).toHaveLength(1);
  } finally {
    if (tree) act(() => { tree.unmount(); });
    subscribe.mockRestore();
  }
  expect(listeners.size).toBe(0);
});

it('returns hardware Back from a real file preview to Files before Session and Runners', async () => {
  const listeners = new Set<Parameters<typeof BackHandler.addEventListener>[1]>();
  const subscribe = jest.spyOn(BackHandler, 'addEventListener').mockImplementation((_event, listener) => {
    listeners.add(listener);
    return { remove: () => { listeners.delete(listener); } };
  });
  const hardwareBack = () => {
    for (const listener of Array.from(listeners).reverse()) if (listener()) return true;
    return false;
  };
  let tree!: renderer.ReactTestRenderer;
  const press = async (label: string) => {
    await act(async () => { (visibleLabeled(tree, label)[0] ?? labeled(tree, label)[0]).props.onPress(); });
  };
  try {
    await act(async () => { tree = renderer.create(<App />); });
    await act(async () => { tree.root.findByType(RunnersScreen).props.onOpenRunner('r1'); });
    await press('Session');
    await press('Panels');
    await press('files');
    await press('Open file preview.txt');
    expect(labels(tree).join('\n')).toContain('first preview line\nsecond preview line');
    expect(labeled(tree, 'Open file preview.txt')).toHaveLength(0);
    const previewBack = tree.root.findByType(FileExplorerPanel).findAllByType(TouchableOpacity)
      .find((node) => node.props.accessibilityLabel === 'Back');
    await act(async () => { previewBack!.props.onPress(); });
    expect(labeled(tree, 'Open file preview.txt')).toHaveLength(1);
    await press('Open file preview.txt');
    act(() => { expect(hardwareBack()).toBe(true); });
    expect(tree.root.findAllByType(FileExplorerPanel)).toHaveLength(1);
    expect(labeled(tree, 'Open file preview.txt')).toHaveLength(1);
    expect(labels(tree).join('\n')).not.toContain('first preview line');
    expect(labeled(tree, 'Open side chat')).toHaveLength(0);
    expect(labeled(tree, 'Panels')).toHaveLength(0);
    act(() => { expect(hardwareBack()).toBe(true); });
    expect(tree.root.findAllByType(FileExplorerPanel)).toHaveLength(0);
    expect(labeled(tree, 'Panels')).toHaveLength(1);
    expect(labeled(tree, 'Send')).toHaveLength(1);
    expect(labeled(tree, 'Open side chat')).toHaveLength(1);
    act(() => { expect(hardwareBack()).toBe(true); });
    expect(tree.root.findAllByType(RunnerDetailScreen)).toHaveLength(0);
    expect(tree.root.findAllByType(RunnersScreen)).toHaveLength(1);
  } finally {
    if (tree) act(() => { tree.unmount(); });
    subscribe.mockRestore();
  }
  expect(listeners.size).toBe(0);
});

it('disarms runner deletion with hardware Back before leaving Runner Detail', async () => {
  const listeners = new Set<Parameters<typeof BackHandler.addEventListener>[1]>();
  const subscribe = jest.spyOn(BackHandler, 'addEventListener').mockImplementation((_event, listener) => {
    listeners.add(listener);
    return { remove: () => { listeners.delete(listener); } };
  });
  const hardwareBack = () => {
    for (const listener of Array.from(listeners).reverse()) if (listener()) return true;
    return false;
  };
  let tree!: renderer.ReactTestRenderer;
  const press = async (label: string) => {
    await act(async () => { labeled(tree, label)[0].props.onPress(); });
  };
  try {
    await act(async () => { tree = renderer.create(<App />); });
    await act(async () => { tree.root.findByType(RunnersScreen).props.onOpenRunner('r1'); });
    await press('Delete runner');
    expect(labeled(tree, 'Delete?')).toHaveLength(1);
    await press('Back');
    expect(tree.root.findAllByType(RunnerDetailScreen)).toHaveLength(1);
    expect(labeled(tree, 'Delete runner')).toHaveLength(1);
    expect(labeled(tree, 'Delete?')).toHaveLength(0);
    await press('Delete runner');
    expect(labeled(tree, 'Delete?')).toHaveLength(1);
    act(() => { expect(hardwareBack()).toBe(true); });
    expect(tree.root.findAllByType(RunnerDetailScreen)).toHaveLength(1);
    expect(labeled(tree, 'Delete runner')).toHaveLength(1);
    expect(labeled(tree, 'Delete?')).toHaveLength(0);
    act(() => { expect(hardwareBack()).toBe(true); });
    expect(tree.root.findAllByType(RunnerDetailScreen)).toHaveLength(0);
    expect(tree.root.findAllByType(RunnersScreen)).toHaveLength(1);
    expect(hardwareBack()).toBe(false);
  } finally {
    if (tree) act(() => { tree.unmount(); });
    subscribe.mockRestore();
  }
  expect(listeners.size).toBe(0);
});

it('disarms project deletion with hardware Back before returning to Projects', async () => {
  const listeners = new Set<Parameters<typeof BackHandler.addEventListener>[1]>();
  const subscribe = jest.spyOn(BackHandler, 'addEventListener').mockImplementation((_event, listener) => {
    listeners.add(listener);
    return { remove: () => { listeners.delete(listener); } };
  });
  const hardwareBack = () => {
    for (const listener of Array.from(listeners).reverse()) if (listener()) return true;
    return false;
  };
  let tree!: renderer.ReactTestRenderer;
  const press = async (label: string) => {
    await act(async () => { labeled(tree, label)[0].props.onPress(); });
  };
  try {
    await act(async () => { tree = renderer.create(<App />); });
    await press('Projects');
    await act(async () => { tree.root.findByType(ProjectsScreen).props.onOpenProject('back-project'); });
    await press('Delete project');
    expect(labeled(tree, 'Delete?')).toHaveLength(1);
    await press('Back');
    expect(tree.root.findAllByType(ProjectChannelScreen)).toHaveLength(1);
    expect(labeled(tree, 'Delete project')).toHaveLength(1);
    expect(labeled(tree, 'Delete?')).toHaveLength(0);
    await press('Delete project');
    expect(labeled(tree, 'Delete?')).toHaveLength(1);
    act(() => { expect(hardwareBack()).toBe(true); });
    expect(tree.root.findAllByType(ProjectChannelScreen)).toHaveLength(1);
    expect(labeled(tree, 'Delete project')).toHaveLength(1);
    expect(labeled(tree, 'Delete?')).toHaveLength(0);
    act(() => { expect(hardwareBack()).toBe(true); });
    expect(tree.root.findAllByType(ProjectChannelScreen)).toHaveLength(0);
    expect(tree.root.findAllByType(ProjectsScreen)).toHaveLength(1);
    expect(hardwareBack()).toBe(false);
  } finally {
    if (tree) act(() => { tree.unmount(); });
    subscribe.mockRestore();
  }
  expect(listeners.size).toBe(0);
});

it('returns hardware Back from process detail to the process list before Session', async () => {
  const listeners = new Set<Parameters<typeof BackHandler.addEventListener>[1]>();
  const subscribe = jest.spyOn(BackHandler, 'addEventListener').mockImplementation((_event, listener) => {
    listeners.add(listener);
    return { remove: () => { listeners.delete(listener); } };
  });
  const hardwareBack = () => {
    for (const listener of Array.from(listeners).reverse()) if (listener()) return true;
    return false;
  };
  let tree!: renderer.ReactTestRenderer;
  const press = async (label: string) => {
    await act(async () => { (visibleLabeled(tree, label)[0] ?? labeled(tree, label)[0]).props.onPress(); });
  };
  try {
    await act(async () => { tree = renderer.create(<App />); });
    await act(async () => { tree.root.findByType(RunnersScreen).props.onOpenRunner('r1'); });
    await press('Session');
    await press('Panels');
    await press('processes');
    await press('Process back-process-fixture');
    expect(labeled(tree, 'Process back-process-fixture')).toHaveLength(0);
    const detailBack = tree.root.findByType(ProcessesPanel).findAllByType(TouchableOpacity)
      .find((node) => node.props.accessibilityLabel === 'Back');
    await act(async () => { detailBack!.props.onPress(); });
    expect(labeled(tree, 'Process back-process-fixture')).toHaveLength(1);
    await press('Process back-process-fixture');
    act(() => { expect(hardwareBack()).toBe(true); });
    expect(tree.root.findAllByType(ProcessesPanel)).toHaveLength(1);
    expect(labeled(tree, 'Process back-process-fixture')).toHaveLength(1);
    expect(labeled(tree, 'Open side chat')).toHaveLength(0);
    act(() => { expect(hardwareBack()).toBe(true); });
    expect(tree.root.findAllByType(ProcessesPanel)).toHaveLength(0);
    expect(labeled(tree, 'Panels')).toHaveLength(1);
    expect(labeled(tree, 'Send')).toHaveLength(1);
    expect(labeled(tree, 'Open side chat')).toHaveLength(1);
  } finally {
    if (tree) act(() => { tree.unmount(); });
    subscribe.mockRestore();
  }
  expect(listeners.size).toBe(0);
});

it('returns hardware Back from a review file to the review list before Session', async () => {
  const listeners = new Set<Parameters<typeof BackHandler.addEventListener>[1]>();
  const subscribe = jest.spyOn(BackHandler, 'addEventListener').mockImplementation((_event, listener) => {
    listeners.add(listener);
    return { remove: () => { listeners.delete(listener); } };
  });
  const hardwareBack = () => {
    for (const listener of Array.from(listeners).reverse()) if (listener()) return true;
    return false;
  };
  let tree!: renderer.ReactTestRenderer;
  const press = async (label: string) => {
    await act(async () => { (visibleLabeled(tree, label)[0] ?? labeled(tree, label)[0]).props.onPress(); });
  };
  try {
    await act(async () => { tree = renderer.create(<App />); });
    await act(async () => { tree.root.findByType(RunnersScreen).props.onOpenRunner('r1'); });
    await press('Session');
    await press('Panels');
    await press('review');
    await press('Review file preview.txt');
    expect(labels(tree).join('\n')).toContain('new-review-line');
    expect(labeled(tree, 'Review file preview.txt')).toHaveLength(0);
    const fileBack = tree.root.findByType(ReviewPanel).findAllByType(TouchableOpacity)
      .find((node) => node.props.accessibilityLabel === 'Back');
    await act(async () => { fileBack!.props.onPress(); });
    expect(labeled(tree, 'Review file preview.txt')).toHaveLength(1);
    await press('Review file preview.txt');
    act(() => { expect(hardwareBack()).toBe(true); });
    expect(tree.root.findAllByType(ReviewPanel)).toHaveLength(1);
    expect(labeled(tree, 'Review file preview.txt')).toHaveLength(1);
    expect(labeled(tree, 'Open side chat')).toHaveLength(0);
    act(() => { expect(hardwareBack()).toBe(true); });
    expect(tree.root.findAllByType(ReviewPanel)).toHaveLength(0);
    expect(labeled(tree, 'Panels')).toHaveLength(1);
    expect(labeled(tree, 'Send')).toHaveLength(1);
    expect(labeled(tree, 'Open side chat')).toHaveLength(1);
  } finally {
    if (tree) act(() => { tree.unmount(); });
    subscribe.mockRestore();
  }
  expect(listeners.size).toBe(0);
});
