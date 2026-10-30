import React from 'react';
import renderer, { act } from 'react-test-renderer';
import { Text } from 'react-native';
import type { RunnerClient } from '../src/runnerClient';
import { FileExplorerPanel } from '../src/screens/FileExplorerPanel';
import { DARK } from '../src/ui';

type Command = { type: string } & Record<string, unknown>;
const SESSION = 'explorer-session';
const ROOT = { entries: [{ name: 'src', path: 'src', kind: 'dir' }, { name: 'readme.txt', path: 'readme.txt', kind: 'file' }] };
const ok = (command: string, data: unknown) => ({ type: 'response', command, success: true, data });
const fail = (command: string) => ({ type: 'response', command, success: false, error: `${command} refused` });
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((complete) => { resolve = complete; });
  return { promise, resolve };
}
function response(command: Command): unknown {
  switch (command.type) {
    case 'file_explorer_list': return ok(command.type, command.path ? { entries: [{ name: 'index.ts', path: 'src/index.ts', kind: 'file' }] } : ROOT);
    case 'file_explorer_read': return ok(command.type, { path: command.path, lines: ['first line'], totalLines: 1, sizeBytes: 20 });
    case 'file_explorer_changes': return ok(command.type, { files: [{ path: 'src/index.ts', status: 'modified' }], isRepo: true });
    case 'file_explorer_diff': return ok(command.type, { path: command.path, lines: ['+changed line'], totalLines: 1 });
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
async function withPanel(client: RunnerClient, run: (tree: renderer.ReactTestRenderer, errors: jest.Mock, close: jest.Mock) => Promise<void>) {
  let tree!: renderer.ReactTestRenderer;
  const errors = jest.fn();
  const close = jest.fn();
  try {
    await act(async () => { tree = renderer.create(<FileExplorerPanel palette={DARK} client={client} sessionId={SESSION} onError={errors} onClose={close} />); });
    await run(tree, errors, close);
  } finally {
    if (tree) await act(async () => { tree.unmount(); });
  }
}

it('lists root without a path and opens folders and files in the panel session', async () => {
  const { client, command } = mockClient();
  await withPanel(client, async (tree, errors, close) => {
    expect(command.mock.calls).toEqual([[{ type: 'file_explorer_list' }, SESSION]]);
    expect(text(tree)).toContain('readme.txt');
    expect(text(tree)).toContain('src');
    await press(tree, 'Open folder src');
    expect(command).toHaveBeenLastCalledWith({ type: 'file_explorer_list', path: 'src' }, SESSION);
    await press(tree, 'Open file src/index.ts');
    expect(command).toHaveBeenLastCalledWith({ type: 'file_explorer_read', path: 'src/index.ts', maxLines: 400 }, SESSION);
    expect(text(tree)).toContain('first line');
    await press(tree, 'Back');
    expect(labeled(tree, 'Open file src/index.ts')).toBeDefined();
    expect(close).not.toHaveBeenCalled();
    expect(errors).not.toHaveBeenCalled();
  });
});

it('appends paged lines and removes Load more at the final cursor', async () => {
  const { client, command } = mockClient((request) => request.type === 'file_explorer_read'
    ? ok(request.type, { path: request.path, lines: request.cursor ? ['second line'] : ['first line'], nextCursor: request.cursor ? undefined : 1, totalLines: 2, sizeBytes: 40 })
    : response(request));
  await withPanel(client, async (tree) => {
    await press(tree, 'Open file readme.txt');
    await press(tree, 'Load more');
    expect(command).toHaveBeenLastCalledWith({ type: 'file_explorer_read', path: 'readme.txt', cursor: 1, maxLines: 400 }, SESSION);
    const row = (line: number) => labeled(tree, `Line ${line}`);
    expect(row(1).findAllByType(Text).map((node) => String(node.props.children)).join(' ')).toContain('first line');
    expect(row(2).findAllByType(Text).map((node) => String(node.props.children)).join(' ')).toContain('second line');
    expect(tree.root.findAllByProps({ accessibilityLabel: 'Load more' })).toHaveLength(0);
  });
});

it('navigates diff, changes, parent directory and root Back without closing early', async () => {
  const { client, command } = mockClient();
  await withPanel(client, async (tree, errors, close) => {
    await press(tree, 'Open folder src');
    await press(tree, 'Changes');
    expect(command).toHaveBeenLastCalledWith({ type: 'file_explorer_changes' }, SESSION);
    expect(text(tree)).toContain('modified');
    await press(tree, 'Diff src/index.ts');
    expect(command).toHaveBeenLastCalledWith({ type: 'file_explorer_diff', path: 'src/index.ts', maxLines: 400 }, SESSION);
    expect(text(tree)).toContain('+changed line');
    await press(tree, 'Back');
    expect(labeled(tree, 'Diff src/index.ts')).toBeDefined();
    await press(tree, 'Back');
    expect(labeled(tree, 'Open file src/index.ts')).toBeDefined();
    await press(tree, 'Back');
    expect(command).toHaveBeenLastCalledWith({ type: 'file_explorer_list' }, SESSION);
    expect(labeled(tree, 'Open folder src')).toBeDefined();
    expect(close).not.toHaveBeenCalled();
    await press(tree, 'Back');
    expect(close).toHaveBeenCalledTimes(1);
    expect(errors).not.toHaveBeenCalled();
  });
});

it('clears listed rows on refused or malformed lists and allows Files to retry', async () => {
  let mode = 'ok';
  const { client } = mockClient((request) => request.type === 'file_explorer_list' && mode !== 'ok'
    ? mode === 'fail' ? fail(request.type) : ok(request.type, { invalid: true })
    : response(request));
  await withPanel(client, async (tree, errors) => {
    mode = 'fail';
    await press(tree, 'Open folder src');
    expect(text(tree)).toContain('--');
    expect(text(tree)).not.toContain('readme.txt');
    expect(errors).toHaveBeenCalledWith('file_explorer_list refused');
    mode = 'ok';
    await press(tree, 'Files');
    expect(labeled(tree, 'Open folder src')).toBeDefined();
    mode = 'invalid';
    await press(tree, 'Open folder src');
    expect(text(tree)).toContain('--');
    expect(text(tree)).not.toContain('readme.txt');
    expect(errors).toHaveBeenCalledTimes(1);
  });
});

it('clears previously loaded changes on a refused refresh', async () => {
  let refused = false;
  const { client } = mockClient((request) => refused && request.type === 'file_explorer_changes' ? fail(request.type) : response(request));
  await withPanel(client, async (tree, errors) => {
    await press(tree, 'Changes');
    expect(labeled(tree, 'Diff src/index.ts')).toBeDefined();
    refused = true;
    await press(tree, 'Changes');
    expect(text(tree)).toContain('--');
    expect(tree.root.findAllByProps({ accessibilityLabel: 'Diff src/index.ts' })).toHaveLength(0);
    expect(errors).toHaveBeenCalledWith('file_explorer_changes refused');
  });
});

it.each([
  ['file_explorer_read', 'Open file readme.txt', 'first line'],
  ['file_explorer_diff', 'Diff src/index.ts', '+changed line'],
])('clears a visible preview when a second pending %s fails', async (type, label, content) => {
  const pending = deferred<unknown>();
  let requests = 0;
  const { client } = mockClient((request) => request.type === type && ++requests === 2 ? pending.promise : response(request));
  await withPanel(client, async (tree, errors) => {
    if (type === 'file_explorer_diff') await press(tree, 'Changes');
    await act(async () => {
      const open = labeled(tree, label).props.onPress;
      open();
      open();
    });
    expect(text(tree)).toContain(content);
    await act(async () => { pending.resolve(fail(type)); });
    expect(text(tree)).not.toContain(content);
    expect(errors).toHaveBeenCalledWith(`${type} refused`);
    await press(tree, 'Back');
    expect(labeled(tree, label)).toBeDefined();
  });
});

it('hides oversized file contents behind the size warning', async () => {
  const { client } = mockClient((request) => request.type === 'file_explorer_read'
    ? ok(request.type, { path: request.path, lines: ['hidden oversized content'], totalLines: 1, sizeBytes: 256 * 1024 + 1 }) : response(request));
  await withPanel(client, async (tree) => {
    await press(tree, 'Open file readme.txt');
    expect(text(tree)).toContain('File is too large to display.');
    expect(text(tree)).not.toContain('hidden oversized content');
  });
});

it('redacts secret text in file and diff previews', async () => {
  const secret = 'synthetic-secret-value';
  const { client } = mockClient((request) => ['file_explorer_read', 'file_explorer_diff'].includes(request.type)
    ? ok(request.type, { path: request.path, lines: [`password=${secret}`, 'visible text'], totalLines: 2, sizeBytes: 80 }) : response(request));
  await withPanel(client, async (tree) => {
    await press(tree, 'Open file readme.txt');
    expect(text(tree)).toContain('[REDACTED]');
    expect(text(tree)).toContain('visible text');
    expect(text(tree)).not.toContain(secret);
    await press(tree, 'Changes');
    await press(tree, 'Diff src/index.ts');
    expect(text(tree)).toContain('[REDACTED]');
    expect(text(tree)).toContain('visible text');
    expect(text(tree)).not.toContain(secret);
  });
});

it('re-lists root with the new session after switching from a folder', async () => {
  const { client, command } = mockClient();
  await withPanel(client, async (tree, errors, close) => {
    await press(tree, 'Open folder src');
    await act(async () => { tree.update(<FileExplorerPanel palette={DARK} client={client} sessionId="other-session" onError={errors} onClose={close} />); });
    expect(command).toHaveBeenLastCalledWith({ type: 'file_explorer_list' }, 'other-session');
    expect(labeled(tree, 'Open folder src')).toBeDefined();
    await press(tree, 'Open file readme.txt');
    expect(command).toHaveBeenLastCalledWith({ type: 'file_explorer_read', path: 'readme.txt', maxLines: 400 }, 'other-session');
  });
});

it.each(['success', 'failure'])('ignores a late list %s after unmount', async (outcome) => {
  const pending = deferred<unknown>();
  const { client, command } = mockClient(() => pending.promise);
  const errors = jest.fn();
  let tree!: renderer.ReactTestRenderer;
  await act(async () => { tree = renderer.create(<FileExplorerPanel palette={DARK} client={client} sessionId={SESSION} onError={errors} onClose={() => {}} />); });
  await act(async () => { tree.unmount(); });
  await act(async () => { pending.resolve(outcome === 'success' ? ok('file_explorer_list', ROOT) : fail('file_explorer_list')); });
  expect(tree.toJSON()).toBeNull();
  expect(command.mock.calls).toEqual([[{ type: 'file_explorer_list' }, SESSION]]);
  expect(errors.mock.calls).toEqual([]);
});
