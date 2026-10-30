import React from 'react';
import renderer, { act } from 'react-test-renderer';
import type { RunnerClient } from '../src/runnerClient';
import { FileExplorerPanel } from '../src/screens/FileExplorerPanel';
import { JobsPanel } from '../src/screens/JobsPanel';
import { ReviewPanel } from '../src/screens/ReviewPanel';
import { DARK } from '../src/ui';

function mockClient(handler: (command: { type: string; path?: string }) => unknown): RunnerClient {
  return {
    command: async (command: { type: string; path?: string }) => handler(command),
    subscribeFrames: () => () => {},
  } as unknown as RunnerClient;
}

it('opens a listed file path through file_explorer_read', async () => {
  const calls: Array<{ type: string; path?: string }> = [];
  const client = mockClient((command) => {
    calls.push(command);
    if (command.type === 'file_explorer_list') {
      return {
        type: 'response',
        command: 'file_explorer_list',
        success: true,
        data: { entries: [{ name: 'app.ts', path: 'src/app.ts', kind: 'file' }], truncated: false },
      };
    }
    if (command.type === 'file_explorer_read') {
      return {
        type: 'response',
        command: 'file_explorer_read',
        success: true,
        data: { path: command.path, lines: ['export const n = 1;'], totalLines: 1, sizeBytes: 20, truncated: false },
      };
    }
    return { type: 'response', command: command.type, success: false, error: 'unexpected' };
  });
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(
      <FileExplorerPanel palette={DARK} client={client} sessionId="s1" onClose={() => {}} onError={() => {}} />,
    );
  });
  const row = tree.root.findByProps({ accessibilityLabel: 'Open file src/app.ts' });
  await act(async () => {
    row.props.onPress();
  });
  expect(calls.some((call) => call.type === 'file_explorer_read' && call.path === 'src/app.ts')).toBe(true);
  const preview = tree.root.findAllByType(require('react-native').Text).some((node) => node.props.children === 'export const n = 1;');
  expect(preview).toBe(true);
  await act(async () => {
    tree.unmount();
  });
});

it('renders review file paths from the snapshot', async () => {
  const client = mockClient((command) => {
    if (command.type !== 'code_review_snapshot') {
      return { type: 'response', command: command.type, success: false, error: 'unexpected' };
    }
    return {
      type: 'response',
      command: 'code_review_snapshot',
      success: true,
      data: {
        comparisonLabel: 'HEAD',
        files: [{
          path: 'src/a.ts',
          status: 'modified',
          insertions: 1,
          deletions: 0,
          hunks: [{ header: '@@', contentHash: 'h1', lines: [{ kind: 'addition', text: '+ok' }] }],
        }],
      },
    };
  });
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(
      <ReviewPanel palette={DARK} client={client} sessionId="s1" onClose={() => {}} onError={() => {}} />,
    );
  });
  expect(tree.root.findByProps({ accessibilityLabel: 'Review file src/a.ts' })).toBeTruthy();
  await act(async () => {
    tree.unmount();
  });
});

it('shows No live jobs when job_list is empty', async () => {
  const client = mockClient((command) => {
    if (command.type !== 'job_list') {
      return { type: 'response', command: command.type, success: false, error: 'unexpected' };
    }
    return { type: 'response', command: 'job_list', success: true, data: { enabled: true, jobs: [] } };
  });
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(
      <JobsPanel palette={DARK} client={client} sessionId="s1" onClose={() => {}} onError={() => {}} />,
    );
  });
  const texts = tree.root.findAllByType(require('react-native').Text).map((node) => node.props.children);
  expect(texts).toContain('No live jobs');
  await act(async () => {
    tree.unmount();
  });
});
