import React from 'react';
import { Clipboard, Text } from 'react-native';
import renderer, { act } from 'react-test-renderer';

/** The saved rows the modal lists; Beta is the action target, and Gamma is a
 *  third INACTIVE row (archiving the active source session is forbidden). */
const CATALOG = {
  sessions: [
    { sessionId: 's1', path: '/p/a.jsonl', cwd: '/w', name: 'Alpha', runtimeIncarnation: 'inc1', status: 'native', loaded: true },
    { sessionId: 's2', path: '/p/b.jsonl', cwd: '/w', name: 'Beta', runtimeIncarnation: 'inc2', status: 'native', loaded: true },
    { sessionId: 's7', path: '/p/c.jsonl', cwd: '/w', name: 'Gamma', runtimeIncarnation: 'inc7', status: 'native', loaded: true },
  ],
};

/** Session ids a lifecycle cut can land on, with the row the runner resolves. */
const TARGETS: Record<string, { path: string; incarnation: string }> = {
  s3: { path: '/p/clone.jsonl', incarnation: 'inc3' },
  s4: { path: '/p/fork.jsonl', incarnation: 'inc4' },
  s5: { path: '/p/handoff.jsonl', incarnation: 'inc5' },
  s6: { path: '/p/new.jsonl', incarnation: 'inc6' },
};

const ROW_IDENTITY: Record<string, { sessionId: string; runtimeIncarnation: string }> = {
  '/p/a.jsonl': { sessionId: 's1', runtimeIncarnation: 'inc1' },
  '/p/b.jsonl': { sessionId: 's2', runtimeIncarnation: 'inc2' },
  '/p/c.jsonl': { sessionId: 's7', runtimeIncarnation: 'inc7' },
};

const FORK_MESSAGES: Record<string, string> = {
  e1: 'first question',
  e2: 'second question',
  e3: 'third question',
};

/** A same-owner user message frame; identity and text are the discriminator. */
const userLine = (sessionId: string, incarnation: string, text: string) => ({
  type: 'message_start',
  sessionId,
  runtimeIncarnation: incarnation,
  itemId: `u-${text}`,
  message: { role: 'user', content: [{ type: 'text', text }] },
});


/** Lifecycle success row: the identity, the state block, and the authoritative
 *  catalog row must all agree (`parseLifecycleTarget`). `editorText` rides only
 *  for a fork. */
function lifecycleBody(sessionId: string, editorText?: string) {
  const target = TARGETS[sessionId];
  return {
    sessionId,
    runtimeIncarnation: target.incarnation,
    state: { sessionId, sessionFile: target.path, cwd: '/w' },
    session: {
      sessionId,
      runtimeIncarnation: target.incarnation,
      path: target.path,
      cwd: '/w',
      status: 'native',
      loaded: true,
      source: 'rpi',
    },
    ...(editorText === undefined ? {} : { editorText }),
  };
}

function transcriptBody(sessionId: string) {
  return {
    messages: [{
      id: `m-${sessionId}`,
      message: { role: 'assistant', content: [{ type: 'text', text: `body of ${sessionId}` }] },
    }],
  };
}

const CONTEXT_TEXT = '# Pi session context\nFormat: pi-session-context/1\nTruncated: yes\n\n## Model\nprovider model\n';

const HANDOFF_ENVELOPE = {
  sessionId: 's5',
  runtimeIncarnation: 'inc5',
  catalogSessionId: 's5',
  catalogSource: 'rpi',
  catalogStatus: 'native',
  subscriptionRequired: true,
  lifecycleResponseVersion: 1,
  cursor: 0,
  notice: null,
};

const mockSubscribe = jest.fn();
let deleteBody: { success: boolean; data: unknown; error?: string };
let copyBody: { success: boolean; data: unknown; error?: string };
let handoffPending: Promise<{ success: boolean; data: unknown; error?: string }> | null;
let restorePending: Promise<{ success: boolean; data: unknown; error?: string }> | null;
let clonePending: Promise<{ success: boolean; data: unknown; error?: string }> | null;
let newSessionPending: Promise<{ success: boolean; data: unknown; error?: string }> | null;
let abortPending: Promise<{ success: boolean; data: unknown; error?: string }> | { success: boolean; data: unknown; error?: string } | null;
let forkOlderOverride: Promise<{ success: boolean; data: unknown; error?: string }> | { success: boolean; data: unknown; error?: string } | null;
let promptPending: Promise<{ success: boolean; data: unknown; error?: string }> | null;
let compactBody: Promise<{ success: boolean; data: unknown; error?: string }> | { success: boolean; data: unknown; error?: string } | null;
let messageOverrides: Record<string, Promise<{ success: boolean; data: unknown; error?: string }> | { success: boolean; data: unknown; error?: string }> = {};
let switchOverrides: Record<string, Promise<{ success: boolean; data: unknown; error?: string }> | { success: boolean; data: unknown; error?: string }> = {};
let renamePending: Promise<{ success: boolean; data: unknown; error?: string }> | { success: boolean; data: unknown; error?: string } | null = null;
let movePending: Promise<{ success: boolean; data: unknown; error?: string }> | { success: boolean; data: unknown; error?: string } | null = null;
let sessionListPending: Promise<{ success: boolean; data: unknown; error?: string }> | null = null;
let frameListener: ((frame: unknown) => void) | null = null;
let reconnectListener: (() => void) | null = null;

const mockCommand = jest.fn(async (command: { type: string; [key: string]: unknown }, sourceSessionId?: string) => {
  const ok = (data: unknown) => ({ type: 'response', command: command.type, success: true, data });
  switch (command.type) {
    case 'session_list': return sessionListPending ?? ok(CATALOG);
    case 'switch_session': {
      const path = String(command.sessionPath);
      if (path in switchOverrides) return switchOverrides[path];
      return ok(ROW_IDENTITY[path] ?? null);
    }
    case 'get_messages': return messageOverrides[sourceSessionId ?? ''] ?? ok(transcriptBody(sourceSessionId ?? ''));
    case 'get_session_stats': return ok({});
    case 'queue_list': return ok({ steering: [], followUp: [], total: 0 });
    case 'prompt': return promptPending ?? ok({});
    case 'compact': return compactBody ?? ok({});
    case 'clone': return clonePending ?? ok(lifecycleBody('s3'));
    case 'fork': return ok(lifecycleBody('s4', FORK_MESSAGES[String(command.entryId)]));
    case 'session_fork_latest': return ok(lifecycleBody('s4', 'latest prompt'));
    case 'get_fork_messages':
      return command.cursor === undefined
        ? ok({ messages: [{ entryId: 'e1', text: FORK_MESSAGES.e1 }, { entryId: 'e2', text: FORK_MESSAGES.e2 }], nextCursor: 'c2' })
        : forkOlderOverride ?? ok({ messages: [{ entryId: 'e1', text: FORK_MESSAGES.e1 }, { entryId: 'e3', text: FORK_MESSAGES.e3 }], nextCursor: null });
    case 'session_delete': return deleteBody;
    case 'session_restore': return restorePending ?? ok({});
    case 'session_copy_context': return copyBody;
    case 'session_handoff': return handoffPending ?? ok(HANDOFF_ENVELOPE);
    case 'new_session': return newSessionPending ?? ok(lifecycleBody('s6'));
    case 'abort': return abortPending ?? ok({});
    case 'set_session_name': return renamePending ?? ok({});
    case 'session_move': return movePending ?? ok({});
    default: return { type: 'response', command: command.type, success: false, data: null, error: `unhandled ${command.type}` };
  }
});

const mockClient = {
  state: 'connected',
  probe: { sessionId: 's1', runtimeIncarnation: 'inc1', cwd: '/w' },
  command: mockCommand,
  subscribe: mockSubscribe,
  subscribeFrames: jest.fn((listener: (frame: unknown) => void) => {
    frameListener = listener;
    return () => {
      frameListener = null;
    };
  }),
  subscribeReconnect: jest.fn((listener: () => void) => {
    reconnectListener = listener;
    return () => {
      reconnectListener = null;
    };
  }),
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

jest.mock('../src/runtime', () => ({
  registry: {
    client: () => mockClient,
    snapshot: () => [mockRunner],
  },
  mappingStore: {},
  lanDiscovery: {},
}));

import { RunnerDisconnectedError } from '../src/transportError';
import { SessionWorkspaceScreen } from '../src/screens/SessionWorkspaceScreen';

let mockWebViewProps: { onMessage: (event: { nativeEvent: { data: string } }) => void } | null = null;

jest.mock('react-native-webview', () => {
  const ReactModule = require('react');
  const { View } = require('react-native');
  return {
    WebView: ReactModule.forwardRef((props: Record<string, unknown>, ref: unknown) => {
      ReactModule.useImperativeHandle(ref, () => ({ injectJavaScript: () => {} }));
      mockWebViewProps = props as unknown as { onMessage: (event: { nativeEvent: { data: string } }) => void };
      return ReactModule.createElement(View, { testID: 'picker' });
    }),
  };
});

const fetchMock = jest.fn();

import { DARK } from '../src/ui';

let setString: jest.SpyInstance;

function respond(status: number, body: unknown) {
  return { status, ok: status >= 200 && status < 300, json: async () => body };
}

async function openSessions() {
  const onError = jest.fn();
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(<SessionWorkspaceScreen palette={DARK} runnerId="r1" onError={onError} />);
  });
  const press = async (label: string) => {
    const control = tree.root.findByProps({ accessibilityLabel: label });
    await act(async () => {
      control.props.onPress();
    });
    await act(async () => {});
  };
  const post = async (payload: unknown) => {
    await act(async () => {
      mockWebViewProps?.onMessage({ nativeEvent: { data: JSON.stringify(payload) } });
    });
  };
  const deliver = async (frame: unknown) => {
    await act(async () => {
      frameListener?.(frame);
    });
  };
  await press('Panels');
  await press('sessions');
  return { tree, onError, press, post, deliver };
}

/** Commands sent, as `[type, payload, sourceSessionId]` rows. */
function sent(type: string): Array<[Record<string, unknown>, string | undefined]> {
  return mockCommand.mock.calls
    .filter((call) => call[0].type === type)
    .map((call) => [call[0], call[1]]);
}

function rendered(tree: renderer.ReactTestRenderer): string {
  return JSON.stringify(tree.toJSON());
}

function labels(tree: renderer.ReactTestRenderer): string[] {
  return tree.root
    .findAllByType(Text)
    .flatMap((node) => (Array.isArray(node.props.children) ? node.props.children : [node.props.children]))
    .filter((value): value is string => typeof value === 'string');
}

beforeEach(() => {
  mockCommand.mockClear();
  mockSubscribe.mockClear();
  deleteBody = { success: true, data: { archived: true, sessionId: 's2', undoToken: 'u-77' } };
  copyBody = { success: true, data: { format: 'pi-session-context/1', text: CONTEXT_TEXT, truncated: true } };
  handoffPending = null;
  restorePending = null;
  clonePending = null;
  newSessionPending = null;
  abortPending = null;
  forkOlderOverride = null;
  promptPending = null;
  compactBody = null;
  messageOverrides = {};
  switchOverrides = {};
  renamePending = null;
  movePending = null;
  sessionListPending = null;
  frameListener = null;
  fetchMock.mockReset();
  (global as { fetch: unknown }).fetch = fetchMock;
  setString = jest.spyOn(Clipboard, 'setString').mockImplementation(() => {});
});

afterEach(() => {
  setString.mockRestore();
  jest.useRealTimers();
});

it('clones a saved row into a fresh session with an empty composer', async () => {
  const { tree, onError, press } = await openSessions();

  // A draft on the source session must never leak into the clone target.
  const input = tree.root.findByProps({ placeholder: 'Message' });
  await act(async () => {
    input.props.onChangeText('source draft');
  });
  expect(tree.root.findByProps({ placeholder: 'Message' }).props.value).toBe('source draft');

  const listsBefore = sent('session_list').length;
  await press('Clone Beta');

  expect(sent('clone')).toEqual([[{ type: 'clone', sessionPath: '/p/b.jsonl' }, 's1']]);
  expect(onError).not.toHaveBeenCalled();
  // The clone target became the active view: subscribed and re-read, with the
  // catalog refreshed and the fresh composer empty.
  expect(mockSubscribe).toHaveBeenCalledWith({ sessionId: 's3', runtimeIncarnation: 'inc3' }, undefined);
  expect(sent('get_messages').at(-1)?.[1]).toBe('s3');
  expect(rendered(tree)).toContain('body of s3');
  expect(sent('session_list').length).toBeGreaterThan(listsBefore);
  expect(tree.root.findByProps({ placeholder: 'Message' }).props.value).toBe('');

  await act(async () => {
    tree.unmount();
  });
});

it('mints one session when Clone is double-tapped', async () => {
  const inFlight = Promise.withResolvers<{ success: boolean; data: unknown }>();
  clonePending = inFlight.promise;
  const { tree, onError } = await openSessions();

  const clone = tree.root.findByProps({ accessibilityLabel: 'Clone Beta' });
  await act(async () => {
    clone.props.onPress();
    clone.props.onPress();
  });
  expect(sent('clone')).toHaveLength(1);
  expect(tree.root.findByProps({ accessibilityLabel: 'Clone Beta' }).props.disabled).toBe(true);

  await act(async () => {
    inFlight.resolve({ success: true, data: lifecycleBody('s3') });
  });
  await act(async () => {});
  expect(rendered(tree)).toContain('body of s3');
  expect(onError).not.toHaveBeenCalled();

  await act(async () => {
    tree.unmount();
  });
});

it('forks the chosen message, paging older pages into the same picker', async () => {
  const { tree, onError, press } = await openSessions();

  await press('Fork from Beta');
  expect(sent('get_fork_messages')).toEqual([[{ type: 'get_fork_messages', sessionPath: '/p/b.jsonl' }, 's1']]);
  expect(labels(tree)).toContain('first question');
  expect(labels(tree)).toContain('second question');
  expect(sent('fork')).toEqual([]);

  // An older page merges into the list: the duplicate entry is not rendered
  // twice, the new one joins, and the cursor is spent.
  await press('Load older fork messages');
  expect(sent('get_fork_messages')).toEqual([
    [{ type: 'get_fork_messages', sessionPath: '/p/b.jsonl' }, 's1'],
    [{ type: 'get_fork_messages', sessionPath: '/p/b.jsonl', cursor: 'c2' }, 's1'],
  ]);
  expect(labels(tree).filter((label) => label === 'first question')).toHaveLength(1);
  expect(labels(tree)).toContain('third question');
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Load older fork messages' })).toHaveLength(0);

  // The CHOSEN message is what forks, and its prompt becomes the child draft.
  await press('Fork from e3');
  expect(sent('fork')).toEqual([[{ type: 'fork', entryId: 'e3', sessionPath: '/p/b.jsonl' }, 's1']]);
  expect(onError).not.toHaveBeenCalled();
  expect(rendered(tree)).toContain('body of s4');
  expect(tree.root.findByProps({ placeholder: 'Message' }).props.value).toBe('third question');

  await act(async () => {
    tree.unmount();
  });
});

it('keeps the loaded fork page and its cursor when an older page is malformed', async () => {
  const { tree, onError, press } = await openSessions();

  await press('Fork from Beta');
  expect(labels(tree)).toContain('second question');

  // A success that carries no usable page must not be read as "no more
  // candidates": the loaded list AND its cursor survive, so paging stays
  // retryable instead of silently ending.
  forkOlderOverride = { success: true, data: {} };
  await press('Load older fork messages');
  expect(onError).toHaveBeenCalledWith('Fork messages failed');
  expect(labels(tree)).toContain('first question');
  expect(labels(tree)).toContain('second question');
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Load older fork messages' }).length).toBeGreaterThan(0);

  // A truthful retry of the SAME cursor merges the older page.
  forkOlderOverride = null;
  await press('Load older fork messages');
  expect(sent('get_fork_messages')).toEqual([
    [{ type: 'get_fork_messages', sessionPath: '/p/b.jsonl' }, 's1'],
    [{ type: 'get_fork_messages', sessionPath: '/p/b.jsonl', cursor: 'c2' }, 's1'],
    [{ type: 'get_fork_messages', sessionPath: '/p/b.jsonl', cursor: 'c2' }, 's1'],
  ]);
  expect(labels(tree)).toContain('third question');
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Load older fork messages' })).toHaveLength(0);

  await act(async () => {
    tree.unmount();
  });
});

it('drops an older fork page that lands after the picker was reopened', async () => {
  const inFlight = Promise.withResolvers<{ success: boolean; data: unknown }>();
  forkOlderOverride = inFlight.promise;
  const { tree, onError, press } = await openSessions();

  await press('Fork from Beta');
  await press('Load older fork messages');
  expect(sent('get_fork_messages')).toHaveLength(2);

  // Cancel and reopen the SAME row: the reopened picker is a new request
  // generation, so the page still in flight belongs to the old one.
  await press('Cancel fork from message');
  forkOlderOverride = null;
  await press('Fork from Beta');
  expect(labels(tree)).toContain('second question');
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Load older fork messages' }).length).toBeGreaterThan(0);

  await act(async () => {
    inFlight.resolve({ success: true, data: { messages: [{ entryId: 'e9', text: 'stale candidate' }], nextCursor: null } });
  });
  await act(async () => {});
  expect(labels(tree)).not.toContain('stale candidate');
  expect(labels(tree)).toContain('second question');
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Load older fork messages' }).length).toBeGreaterThan(0);
  expect(onError).not.toHaveBeenCalled();

  await act(async () => {
    tree.unmount();
  });
});

it('cancels the fork picker back to the catalog without forking', async () => {
  const { tree, onError, press } = await openSessions();

  await press('Fork from Beta');
  expect(labels(tree)).toContain('first question');
  await press('Cancel fork from message');

  expect(tree.root.findAllByProps({ accessibilityLabel: 'Fork from e1' })).toHaveLength(0);
  expect(labels(tree)).not.toContain('first question');
  expect(sent('fork')).toEqual([]);
  // The catalog is back, so the row's own actions are offered again.
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Resume Beta' }).length).toBeGreaterThan(0);
  expect(onError).not.toHaveBeenCalled();

  await act(async () => {
    tree.unmount();
  });
});

it('archives a row only after the confirm tap and restores it through Undo', async () => {
  jest.useFakeTimers();
  const { tree, onError, press } = await openSessions();

  await press('Delete session Beta');
  expect(sent('session_delete')).toEqual([]);

  await press('Confirm delete Beta');
  expect(sent('session_delete')).toEqual([[{ type: 'session_delete', sessionPath: '/p/b.jsonl' }, 's1']]);
  expect(onError).not.toHaveBeenCalled();
  expect(labels(tree)).toContain('Session archived');
  expect(labels(tree)).toContain('Beta');

  // Undo restores exactly the archived session, once.
  await press('Undo archive Beta');
  expect(sent('session_restore')).toEqual([[{ type: 'session_restore', undoToken: 'u-77' }, 's1']]);
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Undo archive Beta' })).toHaveLength(0);

  await act(async () => {
    tree.unmount();
  });
});

it('spends the undo token once when Undo is double-tapped', async () => {
  jest.useFakeTimers();
  const inFlight = Promise.withResolvers<{ success: boolean; data: unknown }>();
  restorePending = inFlight.promise;
  const { tree, onError, press } = await openSessions();

  await press('Delete session Beta');
  await press('Confirm delete Beta');

  const undo = tree.root.findByProps({ accessibilityLabel: 'Undo archive Beta' });
  await act(async () => {
    undo.props.onPress();
    undo.props.onPress();
  });
  expect(sent('session_restore')).toHaveLength(1);
  expect(tree.root.findByProps({ accessibilityLabel: 'Undo archive Beta' }).props.disabled).toBe(true);

  await act(async () => {
    inFlight.resolve({ success: true, data: {} });
  });
  await act(async () => {});
  // One restore, no false failure after the real success.
  expect(onError).not.toHaveBeenCalled();
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Undo archive Beta' })).toHaveLength(0);

  await act(async () => {
    tree.unmount();
  });
});

it('keeps the undo receipt when a restore fails, so Undo stays retryable', async () => {
  jest.useFakeTimers();
  const inFlight = Promise.withResolvers<{ success: boolean; data: unknown; error?: string }>();
  restorePending = inFlight.promise;
  const { tree, onError, press } = await openSessions();

  await press('Delete session Beta');
  await press('Confirm delete Beta');
  expect(labels(tree)).toContain('Session archived');

  await press('Undo archive Beta');
  expect(sent('session_restore')).toEqual([[{ type: 'session_restore', undoToken: 'u-77' }, 's1']]);
  // The receipt is not retired while the request is in flight.
  expect(labels(tree)).toContain('Session archived');

  await act(async () => {
    inFlight.resolve({ success: false, data: null, error: 'the runner is busy' });
  });
  await act(async () => {});
  expect(onError).toHaveBeenCalledWith('the runner is busy');
  // The session is still archived and the token is the only way back, so the
  // receipt survives the failure.
  expect(labels(tree)).toContain('Session archived');
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Undo archive Beta' }).length).toBeGreaterThan(0);

  // Retry re-sends the SAME token and retires the receipt on success.
  restorePending = null;
  await press('Undo archive Beta');
  expect(sent('session_restore')).toEqual([
    [{ type: 'session_restore', undoToken: 'u-77' }, 's1'],
    [{ type: 'session_restore', undoToken: 'u-77' }, 's1'],
  ]);
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Undo archive Beta' })).toHaveLength(0);

  await act(async () => {
    tree.unmount();
  });
});

it('never lets a settling restore erase a newer undo receipt', async () => {
  jest.useFakeTimers();
  const inFlight = Promise.withResolvers<{ success: boolean; data: unknown; error?: string }>();
  restorePending = inFlight.promise;
  const { tree, onError, press } = await openSessions();

  await press('Delete session Beta');
  await press('Confirm delete Beta');
  await press('Undo archive Beta');
  expect(sent('session_restore')).toEqual([[{ type: 'session_restore', undoToken: 'u-77' }, 's1']]);

  // A second archive of another INACTIVE row lands while the first restore is
  // still in flight (the runner refuses to archive the active source session).
  deleteBody = { success: true, data: { archived: true, sessionId: 's7', undoToken: 'u-99' } };
  await press('Delete session Gamma');
  await press('Confirm delete Gamma');
  expect(labels(tree)).toContain('Gamma');
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Undo archive Gamma' }).length).toBeGreaterThan(0);

  await act(async () => {
    inFlight.resolve({ success: true, data: {} });
  });
  await act(async () => {});
  expect(onError).not.toHaveBeenCalled();
  // The older success retired only its own receipt: Gamma's token and window
  // are untouched.
  expect(labels(tree)).toContain('Session archived');
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Undo archive Gamma' }).length).toBeGreaterThan(0);

  await press('Undo archive Gamma');
  expect(sent('session_restore')).toEqual([
    [{ type: 'session_restore', undoToken: 'u-77' }, 's1'],
    [{ type: 'session_restore', undoToken: 'u-99' }, 's1'],
  ]);
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Undo archive Gamma' })).toHaveLength(0);

  await act(async () => {
    tree.unmount();
  });
});

it('surfaces a drifted archive response instead of offering Undo, and expires the window', async () => {
  jest.useFakeTimers();
  const { tree, onError, press } = await openSessions();

  // A success that names another session is not a deletion of this row.
  deleteBody = { success: true, data: { archived: true, sessionId: 's9', undoToken: 'u-77' } };
  await press('Delete session Beta');
  await press('Confirm delete Beta');
  expect(onError).toHaveBeenCalledWith('session delete response is invalid');
  expect(labels(tree)).not.toContain('Session archived');
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Undo archive Beta' })).toHaveLength(0);

  // A valid archive offers Undo for the bounded window only.
  deleteBody = { success: true, data: { archived: true, sessionId: 's2', undoToken: 'u-77' } };
  await press('Delete session Beta');
  await press('Confirm delete Beta');
  expect(labels(tree)).toContain('Session archived');
  await act(async () => {
    jest.advanceTimersByTime(5000);
  });
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Undo archive Beta' })).toHaveLength(0);
  expect(sent('session_restore')).toEqual([]);

  await act(async () => {
    tree.unmount();
  });
});

it('copies the session context artifact and refuses a drifted one', async () => {
  const { tree, onError, press } = await openSessions();

  await press('Copy context Beta');
  expect(sent('session_copy_context')).toEqual([[{ type: 'session_copy_context', sessionPath: '/p/b.jsonl' }, 's1']]);
  expect(setString).toHaveBeenCalledWith(CONTEXT_TEXT);
  expect(labels(tree)).toContain('Copied context (truncated)');

  // A truncated header contradicts the artifact format: nothing reaches the
  // clipboard and the failure is surfaced.
  copyBody = { success: true, data: { format: 'pi-session-context/1', text: '# Pi session context\n', truncated: true } };
  await press('Copy context Beta');
  expect(onError).toHaveBeenCalledWith('session copy context response is invalid');
  expect(setString).toHaveBeenCalledTimes(1);

  await act(async () => {
    tree.unmount();
  });
});

it('hands the active session off once and subscribes the fresh session', async () => {
  const inFlight = Promise.withResolvers<{ success: boolean; data: unknown }>();
  handoffPending = inFlight.promise;
  const { tree, onError, press } = await openSessions();

  const listsBefore = sent('session_list').length;
  await press('Hand off to a new session');
  expect(sent('session_handoff')).toEqual([
    [{ type: 'session_handoff', responseMode: 'subscribe-handoff-v1' }, 's1'],
  ]);

  // While the handoff is in flight the control is disabled, and a second tap
  // never mints another session.
  expect(tree.root.findByProps({ accessibilityLabel: 'Hand off to a new session' }).props.disabled).toBe(true);
  await press('Hand off to a new session');
  expect(sent('session_handoff')).toHaveLength(1);

  await act(async () => {
    inFlight.resolve({ success: true, data: HANDOFF_ENVELOPE });
  });
  await act(async () => {});
  expect(mockSubscribe).toHaveBeenCalledWith({ sessionId: 's5', runtimeIncarnation: 'inc5' }, 0);
  expect(sent('get_messages').at(-1)?.[1]).toBe('s5');
  expect(rendered(tree)).toContain('body of s5');
  expect(sent('session_list').length).toBeGreaterThan(listsBefore);
  expect(onError).not.toHaveBeenCalled();

  await act(async () => {
    tree.unmount();
  });
});

it('applies transcript frames only from the exact runtime on display', async () => {
  const { tree, deliver, press } = await openSessions();
  expect(rendered(tree)).toContain('body of s1');

  // A frame for the CURRENT runtime starts a turn: the composer shows Stop.
  await deliver(userLine('s1', 'inc1', 'SOURCE RUNNING'));
  expect(rendered(tree)).toContain('SOURCE RUNNING');
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Stop' }).length).toBeGreaterThan(0);

  // The target session becomes the active view: a distinct cut leaves the
  // child idle, so the stale Stop control is gone.
  await press('Clone Beta');
  expect(rendered(tree)).toContain('body of s3');
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Stop' })).toHaveLength(0);
  expect(tree.root.findByProps({ accessibilityLabel: 'Send' })).toBeTruthy();

  // The runner keeps the previous subscription alive: a late frame for the
  // SOURCE runtime, another incarnation of the same session id, or a frame
  // that names no runtime at all must never reach this transcript.
  await deliver(userLine('s1', 'inc1', 'STALE SOURCE LINE'));
  await deliver(userLine('s3', 'inc9', 'STALE INCARNATION LINE'));
  await deliver({ type: 'message_start', itemId: 'u-none', message: { role: 'user', content: [{ type: 'text', text: 'UNOWNED LINE' }] } });
  expect(rendered(tree)).not.toContain('STALE SOURCE LINE');
  expect(rendered(tree)).not.toContain('STALE INCARNATION LINE');
  expect(rendered(tree)).not.toContain('UNOWNED LINE');

  // A connection-level frame never reaches the reducer either.
  await deliver({ type: 'session_resync_required', sessionId: 's3', runtimeIncarnation: 'inc3', sequence: 9 });
  expect(rendered(tree)).toContain('body of s3');

  // The exact owner is applied, in order.
  await deliver(userLine('s3', 'inc3', 'LIVE OWNER LINE'));
  expect(rendered(tree)).toContain('LIVE OWNER LINE');

  // The authoritative snapshot derives the child's streaming state: a running
  // turn flips the composer to Stop, and an idle one back to Send.
  await deliver({ type: 'subscription_snapshot', sessionId: 's3', runtimeIncarnation: 'inc3', activeTurnId: 'turn-9' });
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Stop' }).length).toBeGreaterThan(0);
  await deliver({ type: 'subscription_snapshot', sessionId: 's3', runtimeIncarnation: 'inc3', activeTurnId: null });
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Stop' })).toHaveLength(0);

  await act(async () => {
    tree.unmount();
  });
});

it('never lets a slower earlier switch_session adopt after a newer Resume', async () => {
  const slow = Promise.withResolvers<{ success: boolean; data: unknown }>();
  switchOverrides['/p/b.jsonl'] = slow.promise;
  const { tree, onError, press } = await openSessions();

  await press('Resume Beta');
  await press('Resume Gamma');
  expect(rendered(tree)).toContain('body of s7');
  expect(sent('switch_session')).toEqual([
    [{ type: 'switch_session', sessionPath: '/p/b.jsonl' }, 's1'],
    [{ type: 'switch_session', sessionPath: '/p/c.jsonl' }, 's1'],
  ]);

  await act(async () => {
    slow.resolve({ success: true, data: ROW_IDENTITY['/p/b.jsonl'] });
  });
  await act(async () => {});
  expect(rendered(tree)).toContain('body of s7');
  expect(rendered(tree)).not.toContain('body of s2');
  expect(onError).not.toHaveBeenCalled();

  await act(async () => {
    tree.unmount();
  });
});

it('never lets a slower earlier clone adopt after a newer Resume', async () => {
  const slow = Promise.withResolvers<{ success: boolean; data: unknown }>();
  clonePending = slow.promise;
  const { tree, onError, press } = await openSessions();

  await press('Clone Beta');
  await press('Resume Gamma');
  expect(rendered(tree)).toContain('body of s7');

  await act(async () => {
    slow.resolve({ success: true, data: lifecycleBody('s3') });
  });
  await act(async () => {});
  expect(rendered(tree)).toContain('body of s7');
  expect(rendered(tree)).not.toContain('body of s3');
  expect(onError).not.toHaveBeenCalled();

  await act(async () => {
    tree.unmount();
  });
});

it('does not let a same-owner reconnect suppress a pending Resume', async () => {
  const slow = Promise.withResolvers<{ success: boolean; data: unknown }>();
  switchOverrides['/p/b.jsonl'] = slow.promise;
  const { tree, onError, press } = await openSessions();

  await press('Resume Beta');
  await act(async () => {
    reconnectListener?.();
  });
  await act(async () => {});
  await act(async () => {
    slow.resolve({ success: true, data: ROW_IDENTITY['/p/b.jsonl'] });
  });
  await act(async () => {});
  expect(rendered(tree)).toContain('body of s2');
  expect(rendered(tree)).not.toContain('body of s1');
  expect(onError).not.toHaveBeenCalled();

  await act(async () => {
    tree.unmount();
  });
});

it('does not let a late reconnect catalog load overwrite Resume', async () => {
  const { tree, onError, press } = await openSessions();
  const slowList = Promise.withResolvers<{ success: boolean; data: unknown; error?: string }>();
  sessionListPending = slowList.promise;
  await act(async () => {
    reconnectListener?.();
  });
  await press('Resume Beta');
  expect(rendered(tree)).toContain('body of s2');
  expect(rendered(tree)).not.toContain('body of s1');
  await act(async () => {
    slowList.resolve({ success: true, data: CATALOG });
  });
  await act(async () => {});
  expect(rendered(tree)).toContain('body of s2');
  expect(rendered(tree)).not.toContain('body of s1');
  expect(onError).not.toHaveBeenCalled();

  await act(async () => {
    tree.unmount();
  });
});

it('surfaces a thrown switch_session and keeps the current session', async () => {
  switchOverrides['/p/b.jsonl'] = {
    then(_ok: unknown, fail?: (reason: unknown) => unknown) {
      return Promise.resolve(fail?.(new Error('switch transport failed')));
    },
  } as Promise<{ success: boolean; data: unknown; error?: string }>;
  const { tree, onError, press } = await openSessions();

  await press('Resume Beta');
  expect(onError).toHaveBeenCalledWith('switch transport failed');
  expect(rendered(tree)).toContain('body of s1');
  expect(rendered(tree)).not.toContain('body of s2');

  await act(async () => {
    tree.unmount();
  });
});

it('never lets a late session read overwrite the session the user moved to', async () => {
  const slow = Promise.withResolvers<{ success: boolean; data: unknown }>();
  messageOverrides = { s2: slow.promise };
  const { tree, press } = await openSessions();

  await press('Resume Beta');
  await press('Resume Gamma');
  expect(rendered(tree)).toContain('body of s7');

  // The older read settles last: it belongs to a session the user left.
  await act(async () => {
    slow.resolve({ success: true, data: transcriptBody('s2') });
  });
  await act(async () => {});
  expect(rendered(tree)).toContain('body of s7');
  expect(rendered(tree)).not.toContain('body of s2');

  await act(async () => {
    tree.unmount();
  });
});

it('propagates reconnect read failures that are not the typed disconnection', async () => {
  const { tree, onError } = await openSessions();
  expect(rendered(tree)).toContain('body of s1');

  // A reconnect reload that times out is NOT the typed disconnection: it must
  // surface instead of being swallowed as "reconnecting".
  messageOverrides = { s1: Promise.reject(new Error('get_messages timed out after 5000 ms')) };
  await act(async () => {
    reconnectListener?.();
  });
  await act(async () => {});
  expect(onError).toHaveBeenCalledWith('get_messages timed out after 5000 ms');

  // The typed disconnection is the only quiet case.
  onError.mockClear();
  messageOverrides = { s1: Promise.reject(new RunnerDisconnectedError()) };
  await act(async () => {
    reconnectListener?.();
  });
  await act(async () => {});
  expect(onError).not.toHaveBeenCalled();

  await act(async () => {
    tree.unmount();
  });
});

it('keeps the last-good transcript only for the same session, never across a cut', async () => {
  messageOverrides = { s2: { success: false, data: null, error: 'the runner is busy' } };
  const { tree, onError, press } = await openSessions();
  expect(rendered(tree)).toContain('body of s1');

  // A distinct target whose read is refused must not wear the previous
  // session's transcript.
  await press('Resume Beta');
  expect(onError).toHaveBeenCalledWith('the runner is busy');
  expect(rendered(tree)).not.toContain('body of s1');
  expect(rendered(tree)).toContain('No messages.');

  // The SAME session re-read keeps what is on screen when it is refused.
  messageOverrides = {};
  await press('Panels');
  await press('sessions');
  await press('Resume Beta');
  expect(rendered(tree)).toContain('body of s2');
  messageOverrides = { s2: { success: false, data: null, error: 'still busy' } };
  await press('Panels');
  await press('sessions');
  await press('Resume Beta');
  expect(onError).toHaveBeenCalledWith('still busy');
  expect(rendered(tree)).toContain('body of s2');

  await act(async () => {
    tree.unmount();
  });
});

it('never lets a slower read overwrite live frames that already landed', async () => {
  const slow = Promise.withResolvers<{ success: boolean; data: unknown }>();
  const { tree, deliver, press } = await openSessions();

  // The same session is re-read, and the read hangs.
  messageOverrides = { s1: slow.promise };
  await press('Resume Alpha');

  // A live event for the current runtime lands first.
  await deliver({
    type: 'message_start',
    sessionId: 's1',
    runtimeIncarnation: 'inc1',
    itemId: 'u-live',
    message: { role: 'user', content: [{ type: 'text', text: 'LIVE BEFORE READ' }] },
  });
  expect(rendered(tree)).toContain('LIVE BEFORE READ');

  // The read settles afterwards with the older snapshot: live authority wins.
  await act(async () => {
    slow.resolve({ success: true, data: transcriptBody('s1') });
  });
  await act(async () => {});
  expect(rendered(tree)).toContain('LIVE BEFORE READ');

  await act(async () => {
    tree.unmount();
  });
});

it('sends one prompt when Send is double-tapped before the ACK lands', async () => {
  const ack = Promise.withResolvers<{ success: boolean; data: unknown }>();
  promptPending = ack.promise;
  const { tree, onError } = await openSessions();

  await act(async () => {
    tree.root.findByProps({ placeholder: 'Message' }).props.onChangeText('send me once');
  });
  const send = tree.root.findByProps({ accessibilityLabel: 'Send' });
  await act(async () => {
    send.props.onPress();
    send.props.onPress();
  });
  await act(async () => {});
  expect(sent('prompt')).toHaveLength(1);

  await act(async () => {
    ack.resolve({ success: true, data: {} });
  });
  await act(async () => {});
  expect(sent('prompt')).toHaveLength(1);
  expect(onError).not.toHaveBeenCalled();

  await act(async () => {
    tree.unmount();
  });
});

it('keeps the attached chip when the runner refuses the message', async () => {
  promptPending = Promise.resolve({ success: false, data: null, error: 'the runner is busy' });
  const { tree, onError, post } = await openSessions();

  const code = Buffer.from('const answer = 42;', 'utf8');
  await post({ type: 'begin', name: 'notes.ts', mimeType: 'text/typescript', size: code.length, encoding: 'text', total: 1 });
  await post({ type: 'chunk', data: code.toString('utf8') });
  await post({ type: 'end' });
  expect(labels(tree)).toContain('notes.ts');

  await act(async () => {
    tree.root.findByProps({ placeholder: 'Message' }).props.onChangeText('please review');
  });
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: 'Send' }).props.onPress();
  });
  await act(async () => {});

  // The chip was genuinely part of the submission, and the refusal keeps it:
  // nothing was accepted, so nothing is cleared.
  const [sentCommand] = sent('prompt').at(-1) ?? [{ message: '' }];
  expect(sentCommand.message).toContain('const answer = 42;');
  expect(sentCommand.message).toContain('please review');
  expect(onError).toHaveBeenCalledWith('the runner is busy');
  expect(labels(tree)).toContain('notes.ts');
  expect(tree.root.findByProps({ placeholder: 'Message' }).props.value).toBe('please review');

  await act(async () => {
    tree.unmount();
  });
});

it('keeps a refused slash command in the composer too', async () => {
  compactBody = { success: false, data: null, error: 'compaction is already running' };
  const { tree, onError } = await openSessions();

  await act(async () => {
    tree.root.findByProps({ placeholder: 'Message' }).props.onChangeText('/compact');
  });
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: 'Send' }).props.onPress();
  });
  await act(async () => {});

  expect(onError).toHaveBeenCalledWith('compaction is already running');
  expect(tree.root.findByProps({ placeholder: 'Message' }).props.value).toBe('/compact');

  await act(async () => {
    tree.unmount();
  });
});

it('clears only the submission the composer still owns', async () => {
  const ack = Promise.withResolvers<{ success: boolean; data: unknown }>();
  promptPending = ack.promise;
  const { tree, press } = await openSessions();

  await act(async () => {
    tree.root.findByProps({ placeholder: 'Message' }).props.onChangeText('source message');
  });
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: 'Send' }).props.onPress();
  });

  // A lifecycle cut hands the composer to a child while the source ACK is in
  // flight, and the child's own draft is typed.
  await press('Clone Beta');
  await act(async () => {
    tree.root.findByProps({ placeholder: 'Message' }).props.onChangeText('child draft');
  });

  await act(async () => {
    ack.resolve({ success: true, data: {} });
  });
  await act(async () => {});
  // The source session's acceptance must not wipe the child's composer.
  expect(tree.root.findByProps({ placeholder: 'Message' }).props.value).toBe('child draft');

  await act(async () => {
    tree.unmount();
  });
});

it('retires only what the ACK accepted, not edits or chips added meanwhile', async () => {
  const ack = Promise.withResolvers<{ success: boolean; data: unknown }>();
  promptPending = ack.promise;
  const { tree, post } = await openSessions();

  // A chip is in the queue when the message is sent.
  const code = Buffer.from('const answer = 42;', 'utf8');
  await post({ type: 'begin', name: 'notes.ts', mimeType: 'text/typescript', size: code.length, encoding: 'text', total: 1 });
  await post({ type: 'chunk', data: code.toString('utf8') });
  await post({ type: 'end' });
  expect(labels(tree)).toContain('notes.ts');

  await act(async () => {
    tree.root.findByProps({ placeholder: 'Message' }).props.onChangeText('first message');
  });
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: 'Send' }).props.onPress();
  });

  // While the ACK is in flight the user types more and attaches another chip.
  await act(async () => {
    tree.root.findByProps({ placeholder: 'Message' }).props.onChangeText('first message plus more');
  });
  const extra = Buffer.from('extra text', 'utf8');
  await post({ type: 'begin', name: 'extra.txt', mimeType: 'text/plain', size: extra.length, encoding: 'text', total: 1 });
  await post({ type: 'chunk', data: extra.toString('utf8') });
  await post({ type: 'end' });
  expect(labels(tree)).toContain('extra.txt');

  await act(async () => {
    ack.resolve({ success: true, data: {} });
  });
  await act(async () => {});
  // The sent chip is retired; the later text and the later chip are the user's.
  expect(labels(tree)).not.toContain('notes.ts');
  expect(labels(tree)).toContain('extra.txt');
  expect(tree.root.findByProps({ placeholder: 'Message' }).props.value).toBe('first message plus more');

  await act(async () => {
    tree.unmount();
  });
});

it('does not clear text typed while a slash ACK is pending', async () => {
  const ack = Promise.withResolvers<{ success: boolean; data: unknown }>();
  compactBody = ack.promise;
  const { tree } = await openSessions();

  await act(async () => {
    tree.root.findByProps({ placeholder: 'Message' }).props.onChangeText('/compact');
  });
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: 'Send' }).props.onPress();
  });
  await act(async () => {
    tree.root.findByProps({ placeholder: 'Message' }).props.onChangeText('/compact again');
  });

  await act(async () => {
    ack.resolve({ success: true, data: {} });
  });
  await act(async () => {});
  expect(tree.root.findByProps({ placeholder: 'Message' }).props.value).toBe('/compact again');

  await act(async () => {
    tree.unmount();
  });
});

it('starts the handoff child empty and cannot resurrect source attachments', async () => {
  const upload = Promise.withResolvers<unknown>();
  fetchMock.mockReturnValueOnce(upload.promise);
  handoffPending = Promise.resolve({ success: true, data: HANDOFF_ENVELOPE });
  const { tree, onError, press, post } = await openSessions();

  // The source composer holds a draft and a PDF whose upload is in flight.
  await act(async () => {
    tree.root.findByProps({ placeholder: 'Message' }).props.onChangeText('source draft');
  });
  const pdfBytes = Buffer.from('%PDF-1.7 scanned body', 'utf8');
  await post({ type: 'begin', name: 'report.pdf', mimeType: 'application/pdf', size: pdfBytes.length, encoding: 'bytes', total: 1 });
  await post({ type: 'chunk', data: pdfBytes.toString('base64') });
  await post({ type: 'end' });
  expect(labels(tree)).toContain('report.pdf');

  await press('Hand off to a new session');
  await act(async () => {});
  expect(rendered(tree)).toContain('body of s5');
  // The child is a NEW session: it inherits neither the draft nor the chip.
  expect(tree.root.findByProps({ placeholder: 'Message' }).props.value).toBe('');
  expect(labels(tree)).not.toContain('report.pdf');

  // The upload that was already in flight settles into the child's composer:
  // it must not bring the source attachment back.
  await act(async () => {
    upload.resolve(respond(200, { text: 'Extracted paper body', instruction: 'PDF attachment: report.pdf — extracted text layer.' }));
  });
  await act(async () => {});
  expect(labels(tree)).not.toContain('report.pdf');
  expect(onError).not.toHaveBeenCalled();

  await act(async () => {
    tree.unmount();
  });
});

it('shows Stop from a same-owner turn_started without waiting for message_start', async () => {
  const { tree, deliver } = await openSessions();

  await deliver({ type: 'turn_started', sessionId: 's1', runtimeIncarnation: 'inc1', turnId: 'turn-1' });
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Stop' }).length).toBeGreaterThan(0);
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Steer' }).length).toBeGreaterThan(0);
  const strip = tree.root.findAll((node) => node.props.testID === 'activity-strip');
  expect(strip[0]?.props.accessibilityLabel).toBe('Working…');

  await act(async () => {
    tree.unmount();
  });
});

it('disables Move while the current session is streaming', async () => {
  const { tree, deliver } = await openSessions();

  await act(async () => {
    tree.root.findByProps({ placeholder: 'destination directory' }).props.onChangeText('/tmp/moved');
  });
  expect(tree.root.findByProps({ accessibilityLabel: 'Move' }).props.disabled).toBe(false);

  await deliver({ type: 'turn_started', sessionId: 's1', runtimeIncarnation: 'inc1', turnId: 'turn-1' });
  expect(tree.root.findByProps({ accessibilityLabel: 'Move' }).props.disabled).toBe(true);

  await deliver({ type: 'turn_terminal', sessionId: 's1', runtimeIncarnation: 'inc1', turnId: 'turn-1', status: 'succeeded' });
  expect(tree.root.findByProps({ accessibilityLabel: 'Move' }).props.disabled).toBe(false);

  await act(async () => {
    tree.unmount();
  });
});

it('keeps Stop through an inner message_end and clears only on turn_terminal', async () => {
  const { tree, deliver } = await openSessions();

  // A same-owner start flips the composer to Stop, with Steer reachable beside it.
  await deliver(userLine('s1', 'inc1', 'RUNNING TURN'));
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Stop' }).length).toBeGreaterThan(0);
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Steer' }).length).toBeGreaterThan(0);

  // Inner message_end is not turn completion: Stop and Steer stay up.
  await deliver({ type: 'message_end', sessionId: 's1', runtimeIncarnation: 'inc1', itemId: 'u-RUNNING TURN' });
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Stop' }).length).toBeGreaterThan(0);
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Steer' }).length).toBeGreaterThan(0);

  // turn_terminal is the only clearer: Stop/Steer leave and Send returns.
  await deliver({ type: 'turn_terminal', sessionId: 's1', runtimeIncarnation: 'inc1' });
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Stop' })).toHaveLength(0);
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Steer' })).toHaveLength(0);
  expect(tree.root.findByProps({ accessibilityLabel: 'Send' })).toBeTruthy();

  await act(async () => {
    tree.unmount();
  });
});

it('rejects a relative New cwd before any new_session', async () => {
  const { tree, onError } = await openSessions();

  await act(async () => {
    tree.root.findByProps({ placeholder: 'cwd for new session' }).props.onChangeText('tmp/new');
  });
  const neu = tree.root.findByProps({ accessibilityLabel: 'New' });
  expect(neu.props.disabled).toBe(false);
  await act(async () => {
    neu.props.onPress();
  });
  expect(onError).toHaveBeenCalledWith('new session cwd must be an absolute directory path');
  expect(tree.root.findByProps({ placeholder: 'cwd for new session' }).props.value).toBe('tmp/new');
  expect(sent('new_session')).toEqual([]);
  await act(async () => {
    tree.unmount();
  });
});

it('mints one session when New is double-tapped', async () => {
  const inFlight = Promise.withResolvers<{ success: boolean; data: unknown }>();
  newSessionPending = inFlight.promise;
  const { tree, onError } = await openSessions();

  await act(async () => {
    tree.root.findByProps({ placeholder: 'cwd for new session' }).props.onChangeText('/tmp/new');
  });
  const neu = tree.root.findByProps({ accessibilityLabel: 'New' });
  await act(async () => {
    neu.props.onPress();
    neu.props.onPress();
  });
  expect(sent('new_session')).toEqual([
    [{ type: 'new_session', cwd: '/tmp/new' }, 's1'],
  ]);
  expect(tree.root.findByProps({ accessibilityLabel: 'New' }).props.disabled).toBe(true);

  await act(async () => {
    inFlight.resolve({ success: true, data: lifecycleBody('s6') });
  });
  await act(async () => {});
  expect(sent('new_session')).toHaveLength(1);
  expect(rendered(tree)).toContain('body of s6');
  expect(onError).not.toHaveBeenCalled();

  await act(async () => {
    tree.unmount();
  });
});

it('surfaces a thrown abort without clearing Stop', async () => {
  const { tree, onError, deliver } = await openSessions();

  await deliver(userLine('s1', 'inc1', 'RUNNING TURN'));
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Stop' }).length).toBeGreaterThan(0);

  abortPending = Promise.reject(new Error('abort transport failed'));
  const stop = tree.root.findAllByProps({ accessibilityLabel: 'Stop' })[0];
  await act(async () => {
    stop.props.onPress();
  });
  await act(async () => {});
  expect(sent('abort')).toEqual([[{ type: 'abort' }, 's1']]);
  expect(onError).toHaveBeenCalledWith('abort transport failed');
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Stop' }).length).toBeGreaterThan(0);
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Steer' }).length).toBeGreaterThan(0);

  await act(async () => {
    tree.unmount();
  });
});

it('keeps the rename draft when the runner refuses', async () => {
  renamePending = { success: false, data: null, error: 'rename refused' };
  const { tree, onError } = await openSessions();

  await act(async () => {
    tree.root.findByProps({ placeholder: 'name' }).props.onChangeText('beta-renamed');
  });
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: 'Rename' }).props.onPress();
  });
  expect(onError).toHaveBeenCalledWith('rename refused');
  expect(tree.root.findByProps({ placeholder: 'name' }).props.value).toBe('beta-renamed');
  expect(sent('set_session_name')).toEqual([
    [{ type: 'set_session_name', name: 'beta-renamed', sessionPath: '/p/a.jsonl' }, 's1'],
  ]);

  await act(async () => {
    tree.unmount();
  });
});

it('surfaces a thrown rename and keeps the draft', async () => {
  renamePending = {
    then(_ok: unknown, fail?: (reason: unknown) => unknown) {
      return Promise.resolve(fail?.(new Error('rename transport failed')));
    },
  } as Promise<{ success: boolean; data: unknown; error?: string }>;
  const { tree, onError } = await openSessions();

  await act(async () => {
    tree.root.findByProps({ placeholder: 'name' }).props.onChangeText('beta-renamed');
  });
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: 'Rename' }).props.onPress();
  });
  expect(onError).toHaveBeenCalledWith('rename transport failed');
  expect(tree.root.findByProps({ placeholder: 'name' }).props.value).toBe('beta-renamed');
  expect(sent('set_session_name')).toEqual([
    [{ type: 'set_session_name', name: 'beta-renamed', sessionPath: '/p/a.jsonl' }, 's1'],
  ]);

  await act(async () => {
    tree.unmount();
  });
});

it('rejects a relative Move destination before any session_move', async () => {
  const { tree, onError } = await openSessions();

  await act(async () => {
    tree.root.findByProps({ placeholder: 'destination directory' }).props.onChangeText('tmp/moved');
  });
  const move = tree.root.findByProps({ accessibilityLabel: 'Move' });
  expect(move.props.disabled).toBe(false);
  await act(async () => {
    move.props.onPress();
  });
  expect(onError).toHaveBeenCalledWith('destination directory must be an absolute path');
  expect(tree.root.findByProps({ placeholder: 'destination directory' }).props.value).toBe('tmp/moved');
  expect(sent('session_move')).toEqual([]);

  await act(async () => {
    tree.unmount();
  });
});

it('keeps the move draft when the runner refuses', async () => {
  movePending = { success: false, data: null, error: 'Move failed' };
  const { tree, onError } = await openSessions();

  await act(async () => {
    tree.root.findByProps({ placeholder: 'destination directory' }).props.onChangeText('/tmp/moved');
  });
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: 'Move' }).props.onPress();
  });
  expect(onError).toHaveBeenCalledWith('Move failed');
  expect(tree.root.findByProps({ placeholder: 'destination directory' }).props.value).toBe('/tmp/moved');
  expect(sent('session_move')).toEqual([
    [{ type: 'session_move', sessionPath: '/p/a.jsonl', targetCwd: '/tmp/moved' }, 's1'],
  ]);

  await act(async () => {
    tree.unmount();
  });
});

it('surfaces a thrown move and keeps the draft', async () => {
  movePending = {
    then(_ok: unknown, fail?: (reason: unknown) => unknown) {
      return Promise.resolve(fail?.(new Error('move transport failed')));
    },
  } as Promise<{ success: boolean; data: unknown; error?: string }>;
  const { tree, onError } = await openSessions();

  await act(async () => {
    tree.root.findByProps({ placeholder: 'destination directory' }).props.onChangeText('/tmp/moved');
  });
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: 'Move' }).props.onPress();
  });
  expect(onError).toHaveBeenCalledWith('move transport failed');
  expect(tree.root.findByProps({ placeholder: 'destination directory' }).props.value).toBe('/tmp/moved');
  expect(sent('session_move')).toEqual([
    [{ type: 'session_move', sessionPath: '/p/a.jsonl', targetCwd: '/tmp/moved' }, 's1'],
  ]);

  await act(async () => {
    tree.unmount();
  });
});

it('clears New cwd after create even if the child read fails', async () => {
  const inFlight = Promise.withResolvers<{ success: boolean; data: unknown }>();
  newSessionPending = inFlight.promise;
  messageOverrides.s6 = {
    then(_ok: unknown, fail?: (reason: unknown) => unknown) {
      return Promise.resolve(fail?.(new Error('child messages failed')));
    },
  } as Promise<{ success: boolean; data: unknown; error?: string }>;
  const { tree, onError } = await openSessions();

  await act(async () => {
    tree.root.findByProps({ placeholder: 'cwd for new session' }).props.onChangeText('/tmp/new');
  });
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: 'New' }).props.onPress();
  });
  await act(async () => {
    inFlight.resolve({ success: true, data: lifecycleBody('s6') });
  });
  await act(async () => {});
  expect(sent('new_session')).toHaveLength(1);
  expect(tree.root.findByProps({ placeholder: 'cwd for new session' }).props.value).toBe('');
  expect(onError).toHaveBeenCalledWith('child messages failed');

  await act(async () => {
    tree.unmount();
  });
});
