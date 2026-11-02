// Explorer file conversations: Ask binding, prompt acceptance, Stop
// ownership, and teardown/lifetime guards on the native phone surface.
//
// Jest entry point: `cd mobile && npx jest __tests__/fileConversationPanel.test.tsx`.
//
// Every rendered case mounts through withPanel(), which unmounts in a finally:
// a panel left mounted by a failed expectation keeps polling — and its
// teardown closes — into the next test. The teardown-close cases mount
// manually because the close itself is the assertion.

import React from 'react';
import renderer, { act } from 'react-test-renderer';
import { Text } from 'react-native';
import type { RunnerClient } from '../src/runnerClient';
import { FileExplorerPanel } from '../src/screens/FileExplorerPanel';
import { isValidConversationId } from '../src/protocol';
import { normalizeBlame } from '../src/fileExplorer';
import { RunnerDisconnectedError } from '../src/transportError';
import { DARK } from '../src/ui';

type Command = { type: string } & Record<string, unknown>;
type Handler = (command: Command) => unknown;

const SESSION = 'ask-session';
const FILE_A = 'src/a.ts';
const FILE_B = 'src/b.ts';

const ok = (command: string, data: unknown) => ({ type: 'response', command, success: true, data });
const fail = (command: string, error: string, code?: string) => code
  ? { type: 'response', command, success: false, error, data: { code } }
  : { type: 'response', command, success: false, error };

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((complete) => { resolve = complete; });
  return { promise, resolve };
}

function entry(id: string, text: string, over: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    id,
    kind: 'user',
    role: 'user',
    text,
    toolName: null,
    toolArgs: null,
    output: null,
    thinking: null,
    isError: false,
    isPartial: false,
    ...over,
  };
}

/** conversationId → bound path of every file_conversation_open the shared
 * fixture answered, so pathless commands (prompt/abort/snapshot) echo the
 * SAME path their binding opened with — exactly what the engine does. The
 * registry is per test (cleared in mockClient). */
const pathByConversationId = new Map<string, string>();

/** Minimal valid snapshot echoing the request's conversationId and its
 *  binding's exact path. */
function snapshotFor(command: Command, over: Record<string, unknown> = {}): unknown {
  return {
    conversationId: command.conversationId,
    path: typeof command.path === 'string'
      ? command.path
      : (pathByConversationId.get(command.conversationId) ?? FILE_A),
    streaming: false,
    status: 'idle',
    entries: [],
    ...over,
  };
}

function defaultResponse(command: Command): unknown {
  switch (command.type) {
    case 'file_explorer_list':
      return ok(command.type, {
        entries: [
          { name: 'a.ts', path: FILE_A, kind: 'file' },
          { name: 'b.ts', path: FILE_B, kind: 'file' },
        ],
        truncated: false,
      });
    case 'file_explorer_read':
      return ok(command.type, { path: command.path, lines: ['first line', 'second line'], totalLines: 2, sizeBytes: 30, truncated: false });
    case 'file_explorer_changes':
      return ok(command.type, { files: [{ path: FILE_A, status: 'modified' }], isRepo: true, truncated: false });
    case 'file_conversation_open':
      pathByConversationId.set(command.conversationId as string, command.path as string);
      return ok(command.type, snapshotFor(command, { entries: [entry('open-1', 'opened row')] }));
    case 'file_conversation_snapshot':
      return ok(command.type, snapshotFor(command));
    case 'file_conversation_prompt':
      return ok(command.type, snapshotFor(command, { accepted: true, busy: false }));
    case 'file_conversation_close':
      return ok(command.type, { conversationId: command.conversationId, closed: true });
    default:
      throw new Error(`Unexpected ${command.type}`);
  }
}

function mockClient(handler: Handler) {
  pathByConversationId.clear();
  const command = jest.fn(async (request: Command, _sessionId?: string) => handler(request));
  const client = { command, subscribeFrames: jest.fn(() => () => {}) } as unknown as RunnerClient;
  return { client, command };
}

const labeled = (tree: renderer.ReactTestRenderer, label: string) => tree.root.findByProps({ accessibilityLabel: label });
const has = (tree: renderer.ReactTestRenderer, label: string) => tree.root.findAllByProps({ accessibilityLabel: label }).length > 0;
/** Concatenates a Text node's children recursively WITHOUT separators, so a
 *  mixed-children label ('Selected lines ', 2, '–', 2) still reads as one
 *  contiguous string a reader sees. */
function textOf(children: unknown): string {
  if (children == null || typeof children === 'boolean') return '';
  if (typeof children === 'string' || typeof children === 'number') return String(children);
  if (Array.isArray(children)) return children.map(textOf).join('');
  const element = children as { props?: { children?: unknown } };
  return element.props ? textOf(element.props.children) : '';
}
const text = (tree: renderer.ReactTestRenderer) => tree.root.findAllByType(Text).map((node) => textOf(node.props.children)).join('\n');

async function press(tree: renderer.ReactTestRenderer, label: string) {
  await act(async () => { labeled(tree, label).props.onPress(); });
}

async function typeInto(tree: renderer.ReactTestRenderer, label: string, value: string) {
  await act(async () => { labeled(tree, label).props.onChangeText(value); });
}

async function withPanel(client: RunnerClient, run: (tree: renderer.ReactTestRenderer, errors: jest.Mock) => Promise<void>) {
  let tree!: renderer.ReactTestRenderer;
  const errors = jest.fn();
  try {
    await act(async () => { tree = renderer.create(<FileExplorerPanel palette={DARK} client={client} sessionId={SESSION} onError={errors} onClose={() => {}} />); });
    await run(tree, errors);
  } finally {
    if (tree) await act(async () => { tree.unmount(); });
  }
}

function commandsOf(command: jest.Mock, type: string): Command[] {
  return command.mock.calls.filter((call) => call[0].type === type).map((call) => call[0]);
}

it('asks lazily, binds one v4 conversation to the exact preview path, and never re-opens or closes on navigation', async () => {
  const { client, command } = mockClient(defaultResponse);
  let openCommand!: Command;
  await withPanel(client, async (tree) => {
    expect(commandsOf(command, 'file_conversation_open')).toEqual([]);
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Ask about this file');
    const opens = commandsOf(command, 'file_conversation_open');
    expect(opens).toHaveLength(1);
    expect(isValidConversationId(opens[0].conversationId)).toBe(true);
    expect(opens[0]).toEqual({ type: 'file_conversation_open', conversationId: opens[0].conversationId, path: FILE_A });
    expect(command).toHaveBeenLastCalledWith(
      { type: 'file_conversation_open', conversationId: opens[0].conversationId, path: FILE_A },
      SESSION,
    );
    openCommand = opens[0];
    expect(text(tree)).toContain('opened row');

    // Ask → source → Ask: the SAME conversation, refreshed — never re-opened.
    await press(tree, 'Back');
    expect(has(tree, 'Ask about this file')).toBe(true);
    await press(tree, 'Ask about this file');
    expect(commandsOf(command, 'file_conversation_open')).toHaveLength(1);
    expect(commandsOf(command, 'file_conversation_snapshot')).toHaveLength(1);

    // Back through source to the tree closes nothing — only teardown does.
    await press(tree, 'Back');
    await press(tree, 'Back');
    expect(commandsOf(command, 'file_conversation_close')).toEqual([]);
  });
  // Panel teardown (the only lifetime boundary native has) closes the binding.
  expect(commandsOf(command, 'file_conversation_close')).toEqual([
    { type: 'file_conversation_close', conversationId: openCommand.conversationId },
  ]);
});

it('sends the exact tapped line range, consumes it on acceptance, and clears from the chip', async () => {
  const { client, command } = mockClient(defaultResponse);
  await withPanel(client, async (tree) => {
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Ask about this file');
    await press(tree, 'Line 2');
    expect(text(tree)).toContain('Selected lines 2–2');
    await press(tree, 'Line 1');
    expect(text(tree)).toContain('Selected lines 1–2');
    await press(tree, 'Clear selection');
    expect(has(tree, 'Selected lines')).toBe(false);
    await press(tree, 'Line 1');
    await press(tree, 'Line 2');
    expect(text(tree)).toContain('Selected lines 1–2');
    await typeInto(tree, 'File conversation composer', 'look here');
    await press(tree, 'Send');
    expect(commandsOf(command, 'file_conversation_prompt')).toEqual([
      {
        type: 'file_conversation_prompt',
        conversationId: commandsOf(command, 'file_conversation_open')[0].conversationId,
        message: 'look here',
        selection: { startLine: 1, endLine: 2 },
      },
    ]);
    // Acceptance consumes the selection: chip gone, next message unscoped.
    expect(has(tree, 'Selected lines')).toBe(false);
    await typeInto(tree, 'File conversation composer', 'and the rest');
    await press(tree, 'Send');
    const prompts = commandsOf(command, 'file_conversation_prompt');
    expect(prompts).toHaveLength(2);
    expect(prompts[1].selection).toBeUndefined();
  });
});

it('clears draft and consumed selection only on accepted true, and keeps both on accepted false', async () => {
  let prompts = 0;
  const { client, command } = mockClient((req) => {
    if (req.type === 'file_conversation_prompt') {
      prompts += 1;
      if (prompts === 1) return ok(req.type, snapshotFor(req, { accepted: false, busy: true }));
      return ok(req.type, snapshotFor(req, { accepted: true, busy: false, entries: [entry('reply-1', 'the answer')] }));
    }
    return defaultResponse(req);
  });
  await withPanel(client, async (tree, errors) => {
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Ask about this file');
    await typeInto(tree, 'File conversation composer', 'explain this');
    await press(tree, 'Line 2');
    await press(tree, 'Send');
    const first = commandsOf(command, 'file_conversation_prompt');
    expect(first).toHaveLength(1);
    expect(first[0].conversationId).toBe(commandsOf(command, 'file_conversation_open')[0].conversationId);
    expect(first[0].selection).toEqual({ startLine: 2, endLine: 2 });
    // accepted false is never a draft success: text, selection, no inline error.
    expect(labeled(tree, 'File conversation composer').props.value).toBe('explain this');
    expect(has(tree, 'Selected lines')).toBe(true);
    expect(errors).toHaveBeenCalledWith('The file conversation is busy — wait for the current reply');

    await press(tree, 'Send');
    const second = commandsOf(command, 'file_conversation_prompt');
    expect(second).toHaveLength(2);
    expect(second[1].selection).toEqual({ startLine: 2, endLine: 2 });
    expect(labeled(tree, 'File conversation composer').props.value).toBe('');
    expect(has(tree, 'Selected lines')).toBe(false);
    expect(text(tree)).toContain('the answer');
  });
});

it('stops the streaming reply on exactly the bound conversation id', async () => {
  const { client, command } = mockClient((req) => {
    if (req.type === 'file_conversation_open') {
      return ok(req.type, snapshotFor(req, { streaming: true, status: 'streaming', entries: [entry('partial-1', 'partial', { isPartial: true })] }));
    }
    if (req.type === 'file_conversation_abort') {
      return ok(req.type, snapshotFor(req, { streaming: false, status: 'idle' }));
    }
    return defaultResponse(req);
  });
  await withPanel(client, async (tree) => {
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Ask about this file');
    expect(has(tree, 'Stop')).toBe(true);
    await press(tree, 'Stop');
    const aborts = commandsOf(command, 'file_conversation_abort');
    expect(aborts).toHaveLength(1);
    expect(aborts[0]).toEqual({
      type: 'file_conversation_abort',
      conversationId: commandsOf(command, 'file_conversation_open')[0].conversationId,
    });
    expect(has(tree, 'Stop')).toBe(false);
    expect(has(tree, 'Send')).toBe(true);
  });
});

it('never applies a late answer for another file\'s conversation to the open one', async () => {
  const pendingOpenA = deferred<unknown>();
  let aOpens = 0;
  const { client, command } = mockClient((req) => {
    if (req.type === 'file_conversation_open' && req.path === FILE_A) {
      aOpens += 1;
      return pendingOpenA.promise; // A's open answer stays in flight
    }
    if (req.type === 'file_conversation_open') {
      return ok(req.type, snapshotFor(req, { entries: [entry('b-row', 'fresh b row')] }));
    }
    return defaultResponse(req);
  });
  await withPanel(client, async (tree) => {
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Ask about this file');
    // Back out to the tree and ask about B while A's open answer is pending.
    await press(tree, 'Back');
    await press(tree, 'Back');
    await press(tree, 'Open file src/b.ts');
    await press(tree, 'Ask about this file');
    expect(text(tree)).toContain('fresh b row');
    const ids = commandsOf(command, 'file_conversation_open').map((row) => row.conversationId);
    expect(ids).toHaveLength(2);
    expect(new Set(ids).size).toBe(2);

    // A's open finally lands: a foreign conversation must not touch B's view.
    await act(async () => {
      pendingOpenA.resolve(ok('file_conversation_open', snapshotFor(
        { type: 'file_conversation_open', conversationId: ids[0] },
        { entries: [entry('a-row', 'stale a row')] },
      )));
    });
    expect(text(tree)).toContain('fresh b row');
    expect(text(tree)).not.toContain('stale a row');
    expect(aOpens).toBe(1);
  });
});

it('refuses a snapshot bound to another path and renders the unreadable state', async () => {
  const { client, command } = mockClient((req) => {
    if (req.type === 'file_conversation_open') {
      return ok(req.type, { ...snapshotFor(req, { entries: [entry('wrong', 'should not render')] }), path: 'elsewhere.ts' });
    }
    return defaultResponse(req);
  });
  await withPanel(client, async (tree, errors) => {
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Ask about this file');
    expect(errors).toHaveBeenCalledWith('Invalid file conversation response');
    expect(has(tree, 'File conversation unreadable')).toBe(true);
    expect(text(tree)).not.toContain('should not render');
  });
});

it('answers teardown close not_found as the documented silent no-op', async () => {
  const { client, command } = mockClient((req) => {
    if (req.type === 'file_conversation_close') return fail(req.type, 'close refused', 'file_conversation_not_found');
    return defaultResponse(req);
  });
  const errors = jest.fn();
  let tree!: renderer.ReactTestRenderer;
  await act(async () => { tree = renderer.create(<FileExplorerPanel palette={DARK} client={client} sessionId={SESSION} onError={errors} onClose={() => {}} />); });
  await press(tree, 'Open file src/a.ts');
  await press(tree, 'Ask about this file');
  const id = commandsOf(command, 'file_conversation_open')[0].conversationId;
  await act(async () => { tree.unmount(); });
  expect(tree.toJSON()).toBeNull();
  expect(commandsOf(command, 'file_conversation_close')).toEqual([
    { type: 'file_conversation_close', conversationId: id },
  ]);
  expect(errors.mock.calls).toEqual([]);
});

it('surfaces teardown close failures other than the typed no-op centrally', async () => {
  const { client, command } = mockClient((req) => {
    if (req.type === 'file_conversation_close') return fail(req.type, 'close refused', 'file_conversation_capacity');
    return defaultResponse(req);
  });
  const errors = jest.fn();
  let tree!: renderer.ReactTestRenderer;
  await act(async () => { tree = renderer.create(<FileExplorerPanel palette={DARK} client={client} sessionId={SESSION} onError={errors} onClose={() => {}} />); });
  await press(tree, 'Open file src/a.ts');
  await press(tree, 'Ask about this file');
  const id = commandsOf(command, 'file_conversation_open')[0].conversationId;
  await act(async () => { tree.unmount(); });
  expect(tree.toJSON()).toBeNull();
  expect(commandsOf(command, 'file_conversation_close')).toEqual([
    { type: 'file_conversation_close', conversationId: id },
  ]);
  expect(errors.mock.calls).toEqual([['close refused']]);
});

it('keeps each file\'s draft and selection across A → B → A', async () => {
  const { client, command } = mockClient(defaultResponse);
  await withPanel(client, async (tree) => {
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Ask about this file');
    await typeInto(tree, 'File conversation composer', 'about a');
    await press(tree, 'Line 2');
    await press(tree, 'Back');
    await press(tree, 'Back');
    await press(tree, 'Open file src/b.ts');
    await press(tree, 'Ask about this file');
    expect(labeled(tree, 'File conversation composer').props.value).toBe('');
    await typeInto(tree, 'File conversation composer', 'about b');
    await press(tree, 'Back');
    await press(tree, 'Back');
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Ask about this file');
    expect(labeled(tree, 'File conversation composer').props.value).toBe('about a');
    expect(text(tree)).toContain('Selected lines 2–2');
    await press(tree, 'Send');
    expect(commandsOf(command, 'file_conversation_prompt')).toEqual([
      {
        type: 'file_conversation_prompt',
        conversationId: commandsOf(command, 'file_conversation_open')[0].conversationId,
        message: 'about a',
        selection: { startLine: 2, endLine: 2 },
      },
    ]);
  });
});

it('Close conversation evicts on success or the typed no-op, retains and surfaces on failure', async () => {
  let closeMode: 'ok' | 'capacity' | 'notfound' = 'ok';
  const { client, command } = mockClient((req) => {
    if (req.type === 'file_conversation_close') {
      if (closeMode === 'capacity') return fail(req.type, 'close refused', 'file_conversation_capacity');
      if (closeMode === 'notfound') return fail(req.type, 'close refused', 'file_conversation_not_found');
      return ok(req.type, { conversationId: req.conversationId, closed: true });
    }
    return defaultResponse(req);
  });
  await withPanel(client, async (tree, errors) => {
    // Success frees the slot: the next Ask mints a NEW conversation.
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Ask about this file');
    const firstId = commandsOf(command, 'file_conversation_open')[0].conversationId;
    await press(tree, 'Close conversation');
    expect(command).toHaveBeenLastCalledWith({ type: 'file_conversation_close', conversationId: firstId }, SESSION);
    expect(has(tree, 'Ask about this file')).toBe(true);
    expect(has(tree, 'Close conversation')).toBe(false);
    await press(tree, 'Ask about this file');
    const opens = commandsOf(command, 'file_conversation_open');
    expect(opens).toHaveLength(2);
    expect(opens[1].conversationId).not.toBe(firstId);
    expect(errors).not.toHaveBeenCalled();

    // A failed close surfaces and KEEPS the record whole: draft included,
    // resumable without re-open.
    await typeInto(tree, 'File conversation composer', 'kept draft');
    closeMode = 'capacity';
    await press(tree, 'Close conversation');
    expect(errors).toHaveBeenCalledWith('close refused');
    await press(tree, 'Ask about this file');
    expect(commandsOf(command, 'file_conversation_open')).toHaveLength(2);
    expect(commandsOf(command, 'file_conversation_snapshot')).toHaveLength(1);
    expect(labeled(tree, 'File conversation composer').props.value).toBe('kept draft');

    // The typed not_found close is the silent no-op eviction.
    closeMode = 'notfound';
    await press(tree, 'Close conversation');
    expect(errors).toHaveBeenCalledTimes(1);
    await press(tree, 'Ask about this file');
    const finalOpens = commandsOf(command, 'file_conversation_open');
    expect(finalOpens).toHaveLength(3);
    expect(finalOpens[2].conversationId).not.toBe(opens[1].conversationId);
  });
});

it('a delayed prompt failure after leaving Ask neither toasts nor hijacks navigation', async () => {
  const pendingPrompt = deferred<unknown>();
  const { client, command } = mockClient((req) => {
    if (req.type === 'file_conversation_prompt') return pendingPrompt.promise;
    return defaultResponse(req);
  });
  await withPanel(client, async (tree, errors) => {
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Ask about this file');
    await typeInto(tree, 'File conversation composer', 'explain this');
    await press(tree, 'Send');
    await press(tree, 'Back'); // leave Ask while the prompt is in flight
    await press(tree, 'Changes');
    expect(has(tree, 'Diff src/a.ts')).toBe(true);
    await act(async () => { pendingPrompt.resolve(fail('file_conversation_prompt', 'boom')); });
    expect(has(tree, 'Diff src/a.ts')).toBe(true); // no yank back to the source pane
    expect(errors).not.toHaveBeenCalled(); // stale error invalidated off-screen
    // Bookkeeping stayed truthful: a transient failure retains the binding.
    await press(tree, 'Back');
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Ask about this file');
    expect(commandsOf(command, 'file_conversation_open')).toHaveLength(1); // resume, not re-open
    expect(labeled(tree, 'File conversation composer').props.value).toBe('explain this');
  });
});

it('a delayed gone answer after leaving Ask evicts silently and the next Ask opens fresh', async () => {
  const pendingPrompt = deferred<unknown>();
  const { client, command } = mockClient((req) => {
    if (req.type === 'file_conversation_prompt') return pendingPrompt.promise;
    return defaultResponse(req);
  });
  await withPanel(client, async (tree, errors) => {
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Ask about this file');
    const firstId = commandsOf(command, 'file_conversation_open')[0].conversationId;
    await typeInto(tree, 'File conversation composer', 'explain this');
    await press(tree, 'Send');
    await press(tree, 'Back');
    await press(tree, 'Back'); // to the tree
    await act(async () => { pendingPrompt.resolve(fail('file_conversation_prompt', 'gone', 'file_conversation_not_found')); });
    expect(errors).not.toHaveBeenCalled();
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Ask about this file');
    const opens = commandsOf(command, 'file_conversation_open');
    expect(opens).toHaveLength(2);
    expect(opens[1].conversationId).not.toBe(firstId); // evicted: fresh conversation
    expect(labeled(tree, 'File conversation composer').props.value).toBe('');
  });
});

it('a valid accepted ack updates the retained record even after leaving Ask', async () => {
  const pendingPrompt = deferred<unknown>();
  const { client, command } = mockClient((req) => {
    if (req.type === 'file_conversation_prompt') return pendingPrompt.promise;
    return defaultResponse(req);
  });
  await withPanel(client, async (tree, errors) => {
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Ask about this file');
    await typeInto(tree, 'File conversation composer', 'accepted later');
    await press(tree, 'Line 2');
    await press(tree, 'Send');
    await press(tree, 'Back'); // leave while the prompt is in flight
    await act(async () => {
      pendingPrompt.resolve(ok('file_conversation_prompt', snapshotFor(
        { type: 'file_conversation_prompt', conversationId: commandsOf(command, 'file_conversation_open')[0].conversationId },
        { accepted: true, busy: false },
      )));
    });
    expect(errors).not.toHaveBeenCalled();
    // The retained record took the ack off-screen: re-entering shows the
    // cleared composer and consumed selection, with no stale toast.
    await press(tree, 'Ask about this file');
    expect(labeled(tree, 'File conversation composer').props.value).toBe('');
    expect(has(tree, 'Selected lines')).toBe(false);
  });
});

it('pending work on one file never blocks another composer', async () => {
  const pendingA = deferred<unknown>();
  const { client, command } = mockClient((req) => {
    if (req.type === 'file_conversation_prompt' && req.message === 'about a') return pendingA.promise;
    return defaultResponse(req);
  });
  await withPanel(client, async (tree, errors) => {
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Ask about this file');
    await press(tree, 'Line 2');
    await typeInto(tree, 'File conversation composer', 'about a');
    await press(tree, 'Send'); // A's prompt stays in flight
    await press(tree, 'Back');
    await press(tree, 'Back');
    await press(tree, 'Open file src/b.ts');
    await press(tree, 'Ask about this file');
    // B is NOT stuck by A's pending prompt: its composer works immediately.
    expect(labeled(tree, 'File conversation composer').props.value).toBe('');
    await typeInto(tree, 'File conversation composer', 'about b');
    await press(tree, 'Send');
    // FULL ordered history: each prompt names ITS OWN conversation, payload
    // and selection — ownership is proven, never hidden by clearing history.
    const ids = commandsOf(command, 'file_conversation_open').map((row) => row.conversationId);
    expect(ids).toHaveLength(2);
    expect(new Set(ids).size).toBe(2);
    expect(commandsOf(command, 'file_conversation_prompt')).toEqual([
      {
        type: 'file_conversation_prompt',
        conversationId: ids[0],
        message: 'about a',
        selection: { startLine: 2, endLine: 2 },
      },
      {
        type: 'file_conversation_prompt',
        conversationId: ids[1],
        message: 'about b',
      },
    ]);

    // A's ack lands while ONLY B is presented: the retained A record takes
    // it off-screen — no toast, and its draft/selection clear there.
    await act(async () => {
      pendingA.resolve(ok('file_conversation_prompt', snapshotFor(
        { type: 'file_conversation_prompt', conversationId: ids[0] },
        { accepted: true, busy: false },
      )));
    });
    expect(errors).not.toHaveBeenCalled();
    await press(tree, 'Back');
    await press(tree, 'Back');
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Ask about this file');
    expect(labeled(tree, 'File conversation composer').props.value).toBe('');
    expect(has(tree, 'Selected lines')).toBe(false);
    await typeInto(tree, 'File conversation composer', 'follow-up on a');
    expect(labeled(tree, 'Send').props.disabled).toBe(false);
  });
});

function blameLine(currentLine: number, over: Record<string, unknown> = {}) {
  return {
    commit: 'abcdef1234567890',
    author: 'Ada',
    time: 1712345678,
    originalLine: currentLine,
    currentLine,
    text: `line ${currentLine}`,
    ...over,
  };
}

it('rejects blame payloads that are not the exact requested path and ordered range', () => {
  const request = { path: FILE_A, startLine: 1, endLine: 2 };
  const good = { path: FILE_A, lines: [blameLine(1), blameLine(2, { commit: null, author: 'Ada' })] };
  expect(normalizeBlame(good, request)?.lines.map((line) => line.commit)).toEqual(['abcdef1234567890', null]);
  expect(normalizeBlame({ ...good, path: FILE_B }, request)).toBeNull();
  expect(normalizeBlame({ path: FILE_A, lines: [blameLine(2), blameLine(1)] }, request)).toBeNull();
  expect(normalizeBlame({ path: FILE_A, lines: [blameLine(1)] }, request)).toBeNull();
  expect(normalizeBlame({ path: FILE_A, lines: [blameLine(1), blameLine(2), blameLine(3)] }, request)).toBeNull();
  expect(normalizeBlame({ path: FILE_A, lines: [blameLine(1, { commit: '0000000' })] }, { path: FILE_A, startLine: 1, endLine: 1 })?.lines[0].commit).toBeNull();
});

it('blames the exact source selection and labels a null commit Uncommitted', async () => {
  const { client, command } = mockClient((req) => {
    if (req.type === 'file_explorer_blame') {
      return ok(req.type, {
        path: req.path,
        lines: [blameLine(req.startLine as number), blameLine(req.endLine as number, { commit: null })],
      });
    }
    return defaultResponse(req);
  });
  await withPanel(client, async (tree, errors) => {
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Line 1');
    await press(tree, 'Line 2');
    expect(text(tree)).toContain('Selected lines 1–2');
    await press(tree, 'Blame selected lines');
    expect(has(tree, 'Blame loading')).toBe(false);
    expect(commandsOf(command, 'file_explorer_blame')).toEqual([
      { type: 'file_explorer_blame', path: FILE_A, startLine: 1, endLine: 2 },
    ]);
    expect(text(tree)).toContain('Uncommitted');
    expect(text(tree)).toContain('Ada');
    expect(errors).not.toHaveBeenCalled();
  });
});

it('force-opens Ask on the selected lines without clearing the saved draft', async () => {
  const { client, command } = mockClient(defaultResponse);
  await withPanel(client, async (tree) => {
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Ask about this file');
    await typeInto(tree, 'File conversation composer', 'kept draft');
    await press(tree, 'Back');
    await press(tree, 'Line 2');
    await press(tree, 'Ask about selection');
    expect(labeled(tree, 'File conversation composer').props.value).toBe('kept draft');
    expect(text(tree)).toContain('Selected lines 2–2');
    expect(commandsOf(command, 'file_conversation_open')).toHaveLength(1);
  });
});

it('drops a late blame result after the path changes and does not toast it', async () => {
  const pending = deferred<unknown>();
  const { client, command } = mockClient((req) => {
    if (req.type === 'file_explorer_blame') return pending.promise;
    return defaultResponse(req);
  });
  await withPanel(client, async (tree, errors) => {
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Line 1');
    await press(tree, 'Blame selected lines');
    await press(tree, 'Back');
    await press(tree, 'Open file src/b.ts');
    await act(async () => {
      pending.resolve(fail('file_explorer_blame', 'stale blame'));
    });
    expect(errors).not.toHaveBeenCalled();
    expect(has(tree, 'Blame attribution')).toBe(false);
    expect(commandsOf(command, 'file_explorer_blame')).toHaveLength(1);
  });
});

it('drops a late blame result when Ask selection changes and keeps the draft', async () => {
  const pending = deferred<unknown>();
  const { client } = mockClient((req) => {
    if (req.type === 'file_explorer_blame') return pending.promise;
    return defaultResponse(req);
  });
  await withPanel(client, async (tree, errors) => {
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Line 1');
    await press(tree, 'Ask about selection');
    await typeInto(tree, 'File conversation composer', 'kept while blaming');
    await press(tree, 'Blame selected lines');
    await press(tree, 'Line 2');
    expect(text(tree)).toContain('Selected lines 1–2');
    await act(async () => {
      pending.resolve(ok('file_explorer_blame', {
        path: FILE_A,
        lines: [blameLine(1, { author: 'stale-author' })],
      }));
    });
    expect(errors).not.toHaveBeenCalled();
    expect(has(tree, 'Blame attribution')).toBe(false);
    expect(text(tree)).not.toContain('stale-author');
    expect(labeled(tree, 'File conversation composer').props.value).toBe('kept while blaming');
    await press(tree, 'Back');
    expect(text(tree)).toContain('Selected lines 1–2');
    expect(has(tree, 'Blame attribution')).toBe(false);
  });
});

it('rejects a blame time Date cannot format', () => {
  const request = { path: FILE_A, startLine: 1, endLine: 1 };
  expect(normalizeBlame({ path: FILE_A, lines: [blameLine(1, { time: 8.64e12 + 1 })] }, request)).toBeNull();
  expect(normalizeBlame({ path: FILE_A, lines: [blameLine(1, { time: Number.MAX_SAFE_INTEGER + 1 })] }, request)).toBeNull();
  expect(normalizeBlame({ path: FILE_A, lines: [blameLine(1, { time: 8.64e12 })] }, request)?.lines[0].time).toBe(8.64e12);
});

it('redacts secrets in both blame failure toasts', async () => {
  const secret = 'sk-syntheticsecrettoken';
  let calls = 0;
  const { client } = mockClient((req) => {
    if (req.type !== 'file_explorer_blame') return defaultResponse(req);
    calls += 1;
    if (calls === 1) return fail('file_explorer_blame', `blame failed ${secret}`);
    return ok(req.type, { path: FILE_B, lines: [blameLine(1)] });
  });
  await withPanel(client, async (tree, errors) => {
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Line 1');
    await press(tree, 'Blame selected lines');
    expect(errors).toHaveBeenCalledWith('blame failed [REDACTED]');
    expect(errors.mock.calls.flat().join('\n')).not.toContain(secret);
    await press(tree, 'Blame selected lines');
    expect(errors).toHaveBeenLastCalledWith('Invalid file blame');
    expect(has(tree, 'Blame attribution')).toBe(false);
  });
});



it('a background accepted prompt clears only that file and leaves the displayed blame', async () => {
  const pendingA = deferred<unknown>();
  const { client, command } = mockClient((req) => {
    if (req.type === 'file_conversation_prompt' && req.message === 'about a') return pendingA.promise;
    if (req.type === 'file_explorer_blame') {
      return ok(req.type, { path: req.path, lines: [blameLine(req.startLine as number, { author: 'foreground' })] });
    }
    return defaultResponse(req);
  });
  await withPanel(client, async (tree, errors) => {
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Line 1');
    await press(tree, 'Ask about selection');
    await typeInto(tree, 'File conversation composer', 'about a');
    await press(tree, 'Send');
    await press(tree, 'Back');
    await press(tree, 'Back');
    await press(tree, 'Open file src/b.ts');
    await press(tree, 'Line 2');
    await press(tree, 'Blame selected lines');
    expect(text(tree)).toContain('foreground');
    await act(async () => {
      pendingA.resolve(ok('file_conversation_prompt', snapshotFor(
        { type: 'file_conversation_prompt', conversationId: commandsOf(command, 'file_conversation_open')[0].conversationId },
        { accepted: true, busy: false },
      )));
    });
    expect(errors).not.toHaveBeenCalled();
    expect(text(tree)).toContain('Selected lines 2–2');
    expect(text(tree)).toContain('foreground');
    await press(tree, 'Back');
    await press(tree, 'Open file src/a.ts');
    expect(has(tree, 'Selected lines')).toBe(false);
    expect(has(tree, 'Blame attribution')).toBe(false);
    await press(tree, 'Ask about this file');
    expect(labeled(tree, 'File conversation composer').props.value).toBe('');
    expect(has(tree, 'Selected lines')).toBe(false);
  });
});

it('retries a lost open ack with the same id and teardown still closes it', async () => {
  let opens = 0;
  const { client, command } = mockClient((req) => {
    if (req.type === 'file_conversation_open') {
      opens += 1;
      if (opens === 1) return Promise.reject(new Error('file_conversation_open timed out after 30s (no response from server)'));
      return defaultResponse(req);
    }
    return defaultResponse(req);
  });
  const errors = jest.fn();
  let tree: renderer.ReactTestRenderer | undefined;
  try {
    await act(async () => { tree = renderer.create(<FileExplorerPanel palette={DARK} client={client} sessionId={SESSION} onError={errors} onClose={() => {}} />); });
    await press(tree!, 'Open file src/a.ts');
    await press(tree!, 'Ask about this file');
    const first = commandsOf(command, 'file_conversation_open');
    expect(first).toHaveLength(1);
    expect(errors).toHaveBeenCalledWith(expect.stringContaining('timed out'));
    await press(tree!, 'Back');
    await press(tree!, 'Ask about this file');
    const retried = commandsOf(command, 'file_conversation_open');
    expect(retried).toHaveLength(2);
    expect(retried[1].conversationId).toBe(first[0].conversationId);
    expect(retried[1].path).toBe(FILE_A);
  } finally {
    if (tree) await act(async () => { tree!.unmount(); });
  }
  expect(commandsOf(command, 'file_conversation_close')).toEqual([
    { type: 'file_conversation_close', conversationId: commandsOf(command, 'file_conversation_open')[0].conversationId },
  ]);
});

it('retries the same id after a disconnect and retires only a typed refusal', async () => {
  let opens = 0;
  const { client, command } = mockClient((req) => {
    if (req.type === 'file_conversation_open') {
      opens += 1;
      if (opens === 1) return Promise.reject(new RunnerDisconnectedError());
      return fail(req.type, 'the engine is full right now', 'file_conversation_capacity');
    }
    return defaultResponse(req);
  });
  await withPanel(client, async (tree, errors) => {
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Ask about this file');
    const first = commandsOf(command, 'file_conversation_open')[0];
    expect(errors).not.toHaveBeenCalled();
    await press(tree, 'Back');
    await press(tree, 'Ask about this file');
    const second = commandsOf(command, 'file_conversation_open');
    expect(second).toHaveLength(2);
    expect(second[1].conversationId).toBe(first.conversationId);
    expect(errors).toHaveBeenCalledWith('the engine is full right now');
    expect(has(tree, 'Ask about this file')).toBe(true);
    await press(tree, 'Ask about this file');
    const third = commandsOf(command, 'file_conversation_open');
    expect(third).toHaveLength(3);
    expect(third[2].conversationId).not.toBe(first.conversationId);
  });
  expect(commandsOf(command, 'file_conversation_close').map((row) => row.conversationId)).not.toContain(
    commandsOf(command, 'file_conversation_open')[0].conversationId,
  );
});

it('a deferred accepted prompt keeps the draft and selection typed while it was in flight', async () => {
  const pendingPrompt = deferred<unknown>();
  const { client, command } = mockClient((req) => {
    if (req.type === 'file_conversation_prompt') return pendingPrompt.promise;
    return defaultResponse(req);
  });
  await withPanel(client, async (tree) => {
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Ask about this file');
    await press(tree, 'Line 1');
    await typeInto(tree, 'File conversation composer', 'first question');
    await press(tree, 'Send');
    expect(commandsOf(command, 'file_conversation_prompt')).toEqual([{
      type: 'file_conversation_prompt',
      conversationId: commandsOf(command, 'file_conversation_open')[0].conversationId,
      message: 'first question',
      selection: { startLine: 1, endLine: 1 },
    }]);
    // The composer stays editable: this input belongs to no accepted submission.
    await typeInto(tree, 'File conversation composer', 'newer draft');
    await press(tree, 'Line 2');
    expect(text(tree)).toContain('Selected lines 1\u20132');
    await act(async () => {
      pendingPrompt.resolve(ok('file_conversation_prompt', snapshotFor(
        { type: 'file_conversation_prompt', conversationId: commandsOf(command, 'file_conversation_open')[0].conversationId },
        { accepted: true, busy: false },
      )));
    });
    expect(labeled(tree, 'File conversation composer').props.value).toBe('newer draft');
    expect(text(tree)).toContain('Selected lines 1\u20132');
    await press(tree, 'Send');
    const prompts = commandsOf(command, 'file_conversation_prompt');
    expect(prompts).toHaveLength(2);
    expect(prompts[1]).toEqual({
      type: 'file_conversation_prompt',
      conversationId: prompts[0].conversationId,
      message: 'newer draft',
      selection: { startLine: 1, endLine: 2 },
    });
  });
});

it('a deferred accepted prompt keeps a selection chosen on the source pane before the ack', async () => {
  const pendingPrompt = deferred<unknown>();
  const { client, command } = mockClient((req) => {
    if (req.type === 'file_conversation_prompt') return pendingPrompt.promise;
    return defaultResponse(req);
  });
  await withPanel(client, async (tree) => {
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Ask about this file');
    await press(tree, 'Line 1');
    await typeInto(tree, 'File conversation composer', 'first question');
    await press(tree, 'Send');
    await press(tree, 'Back');
    await press(tree, 'Line 2'); // extends the retained line 1, so the range is 1-2
    await press(tree, 'Ask about selection');
    expect(text(tree)).toContain('Selected lines 1\u20132');
    await act(async () => {
      pendingPrompt.resolve(ok('file_conversation_prompt', snapshotFor(
        { type: 'file_conversation_prompt', conversationId: commandsOf(command, 'file_conversation_open')[0].conversationId },
        { accepted: true, busy: false },
      )));
    });
    // The source replacement bumped the submitted revision, so neither the
    // unsent draft nor the new range belongs to the accepted prompt.
    expect(labeled(tree, 'File conversation composer').props.value).toBe('first question');
    expect(text(tree)).toContain('Selected lines 1\u20132');
    await press(tree, 'Send');
    const prompts = commandsOf(command, 'file_conversation_prompt');
    expect(prompts).toHaveLength(2);
    expect(prompts[1].message).toBe('first question');
    expect(prompts[1].selection).toEqual({ startLine: 1, endLine: 2 });
  });
});

it('keeps an ambiguously created id when a retry read fails, so teardown still closes it', async () => {
  let opens = 0;
  const { client, command } = mockClient((req) => {
    if (req.type === 'file_conversation_open') {
      opens += 1;
      // The open may already exist: a lost ack then a typed read failure
      // (read_strict before the existing binding is returned) is not proof
      // the server never created this id.
      if (opens === 1) return Promise.reject(new Error('file_conversation_open timed out after 30s (no response from server)'));
      return fail(req.type, 'file disappeared', 'file_unavailable');
    }
    return defaultResponse(req);
  });
  const errors = jest.fn();
  let tree: renderer.ReactTestRenderer | undefined;
  let id = '';
  try {
    await act(async () => { tree = renderer.create(<FileExplorerPanel palette={DARK} client={client} sessionId={SESSION} onError={errors} onClose={() => {}} />); });
    await press(tree!, 'Open file src/a.ts');
    await press(tree!, 'Ask about this file');
    id = commandsOf(command, 'file_conversation_open')[0].conversationId as string;
    expect(errors).toHaveBeenCalledWith(expect.stringContaining('timed out'));
    await press(tree!, 'Back');
    await press(tree!, 'Ask about this file');
    const retried = commandsOf(command, 'file_conversation_open');
    expect(retried).toHaveLength(2);
    expect(retried[1].conversationId).toBe(id);
    expect(errors).toHaveBeenCalledWith('file disappeared');
    expect(has(tree!, 'Ask about this file')).toBe(true);
    // Still the same owned id: another Ask retries it rather than minting.
    await press(tree!, 'Ask about this file');
    const third = commandsOf(command, 'file_conversation_open');
    expect(third).toHaveLength(3);
    expect(third[2].conversationId).toBe(id);
  } finally {
    if (tree) await act(async () => { tree!.unmount(); });
  }
  expect(commandsOf(command, 'file_conversation_close')).toEqual([
    { type: 'file_conversation_close', conversationId: id },
  ]);
});

it('ignores a close envelope that is not this id closed, and a late close cannot evict its replacement', async () => {
  const pendingClose = deferred<unknown>();
  let closes = 0;
  const { client, command } = mockClient((req) => {
    if (req.type === 'file_conversation_close') {
      closes += 1;
      if (closes === 1) return pendingClose.promise;
      // The replacement's close succeeds, but the envelope names the retired
      // id. Deleting by path would drop the live binding.
      const retiredId = commandsOf(command, 'file_conversation_open')[0]?.conversationId;
      return ok(req.type, { conversationId: retiredId, closed: true });
    }
    if (req.type === 'file_conversation_snapshot') {
      return fail(req.type, 'conversation is gone', 'file_conversation_not_found');
    }
    return defaultResponse(req);
  });
  await withPanel(client, async (tree, errors) => {
    await press(tree, 'Open file src/a.ts');
    await press(tree, 'Ask about this file');
    const firstId = commandsOf(command, 'file_conversation_open')[0].conversationId;
    await press(tree, 'Close conversation');
    expect(commandsOf(command, 'file_conversation_close')).toEqual([
      { type: 'file_conversation_close', conversationId: firstId },
    ]);
    expect(has(tree, 'Ask about this file')).toBe(true);

    // While that close is unanswered the record is not resumable.
    await press(tree, 'Ask about this file');
    expect(commandsOf(command, 'file_conversation_open')).toHaveLength(1);
    expect(has(tree, 'Close conversation')).toBe(false);

    // Success that does not parse as { this conversationId, closed: true }
    // is a surfaced failure and keeps the record.
    await act(async () => {
      pendingClose.resolve(ok('file_conversation_close', {
        conversationId: '00000000-0000-4000-8000-000000000000',
        closed: true,
      }));
    });
    expect(errors).toHaveBeenCalledWith('Close failed');
    await press(tree, 'Ask about this file');
    expect(commandsOf(command, 'file_conversation_open')).toHaveLength(1);
    expect(commandsOf(command, 'file_conversation_snapshot')).toHaveLength(1);
    expect(errors).toHaveBeenCalledWith('conversation is gone');

    // That typed not_found is the authoritative retirement. The next Ask mints
    // a replacement. Closing it answers success for the retired id only:
    // identity-checked deletion must keep the replacement, so the following
    // Ask resumes it instead of minting a third conversation.
    await press(tree, 'Ask about this file');
    const opens = commandsOf(command, 'file_conversation_open');
    expect(opens).toHaveLength(2);
    const replacementId = opens[1].conversationId;
    expect(replacementId).not.toBe(firstId);
    await press(tree, 'Close conversation');
    expect(commandsOf(command, 'file_conversation_close')).toEqual([
      { type: 'file_conversation_close', conversationId: firstId },
      { type: 'file_conversation_close', conversationId: replacementId },
    ]);
    expect(errors.mock.calls).toEqual([
      ['Close failed'],
      ['conversation is gone'],
      ['Close failed'],
    ]);
    await press(tree, 'Ask about this file');
    const after = commandsOf(command, 'file_conversation_open');
    expect(after).toHaveLength(2);
    expect(after[1].conversationId).toBe(replacementId);
    expect(commandsOf(command, 'file_conversation_snapshot').length).toBeGreaterThan(1);
  });
});
