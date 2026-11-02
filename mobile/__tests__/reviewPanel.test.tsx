// RN code-review surface: scope payloads, fail-closed snapshot projection,
// bounded diff paging, comment targeting, and Stop ownership.
//
// Jest entry point: `cd mobile && npx jest __tests__/reviewPanel.test.tsx`.
//
// Every rendered case mounts through withPanel(), which unmounts in a finally:
// a panel left mounted by a failed expectation keeps polling — and animating —
// into the next test and past the environment's teardown.

import React from 'react';
import renderer, { act } from 'react-test-renderer';
import { Text } from 'react-native';
import {
  CODE_REVIEW_SCOPE_USAGE,
  REVIEW_DIFF_MAX_UI_LINES,
  REVIEW_RENDER_SOFT_CAP,
  appendReviewDiffPage,
  emptyReviewSnapshot,
  initReviewLoadedDiff,
  orderReviewFindings,
  parseCodeReviewScope,
  parseReviewSnapshot,
  planReviewDiffWindow,
  reviewAggregateCategory,
  reviewCategoryLabel,
  reviewFindingNeighbor,
  reviewFindingRowRange,
  reviewFindingsForHunk,
  reviewHunkIsReady,
  reviewHunkKeyFor,
  reviewStageFeedback,
  type CodeReviewFileDiffHunkData,
  type CodeReviewFileDiffPageData,
  type DiffLine,
  type ReviewDiffHunk,
} from '../src/protocol';
import { ReviewPanel } from '../src/screens/ReviewPanel';
import type { RunnerClient } from '../src/runnerClient';
import { DARK } from '../src/ui';

type WireCommand = Record<string, unknown>;

function mockClient(
  handler: (command: WireCommand, sessionId?: string) => unknown,
): RunnerClient {
  return {
    command: async (command: WireCommand, sessionId?: string) => handler(command, sessionId),
    subscribeFrames: () => () => {},
  } as unknown as RunnerClient;
}

function ok(command: string, data: unknown): unknown {
  return { type: 'response', command, success: true, data };
}

function fail(command: string, error: string, code?: string): unknown {
  return code
    ? { type: 'response', command, success: false, error, data: { code } }
    : { type: 'response', command, success: false, error };
}

const HUNK_A: ReviewDiffHunk = {
  header: '@@ -1 +1 @@',
  oldStart: 1,
  oldCount: 1,
  newStart: 1,
  newCount: 1,
  contentHash: 'hash-a',
  lines: [
    { kind: 'deletion', text: '-old', oldNo: 1 },
    { kind: 'addition', text: '+new', newNo: 1 },
  ],
};

const HUNK_B: ReviewDiffHunk = {
  header: '@@ -5 +5 @@',
  oldStart: 5,
  oldCount: 1,
  newStart: 5,
  newCount: 1,
  contentHash: 'hash-b',
  lines: [
    { kind: 'deletion', text: '-gone', oldNo: 5 },
    { kind: 'addition', text: '+kept', newNo: 5 },
  ],
};

/** Snapshot file entry of the fixture wire payload. */
interface FixtureFile {
  path: string;
  status: string;
  bodyState: string;
  hunks: ReviewDiffHunk[];
}

const FILE_A: FixtureFile = { path: 'src/a.ts', status: 'modified', bodyState: 'complete', hunks: [HUNK_A] };
const FILE_B: FixtureFile = { path: 'src/b.ts', status: 'modified', bodyState: 'complete', hunks: [HUNK_B] };
/** A body the snapshot only parsed in part: its lines come from `file_diff`. */
const PAGED_FILE: FixtureFile = { path: 'src/big.ts', status: 'modified', bodyState: 'preview', hunks: [HUNK_B] };
/** Two hunks in one file; the first hunk interleaves an addition inside an
 *  old-side range so opposite-side rows between matching rows are visible. */
const HUNK_C1: ReviewDiffHunk = {
  header: '@@ -3,3 +3,3 @@',
  oldStart: 3,
  oldCount: 3,
  newStart: 3,
  newCount: 3,
  contentHash: 'hash-c1',
  lines: [
    { kind: 'context', text: ' ctx', oldNo: 3, newNo: 3 },
    { kind: 'deletion', text: '-old', oldNo: 4 },
    { kind: 'addition', text: '+new', newNo: 4 },
    { kind: 'context', text: ' ctx2', oldNo: 5, newNo: 5 },
  ],
};
const HUNK_C2: ReviewDiffHunk = {
  header: '@@ -10 +10 @@',
  oldStart: 10,
  oldCount: 1,
  newStart: 10,
  newCount: 1,
  contentHash: 'hash-c2',
  lines: [
    { kind: 'deletion', text: '-second hunk old', oldNo: 10 },
    { kind: 'addition', text: '+second hunk', newNo: 10 },
  ],
};
const FILE_C: FixtureFile = { path: 'src/c.ts', status: 'modified', bodyState: 'complete', hunks: [HUNK_C1, HUNK_C2] };
const SNAPSHOT_FILES: FixtureFile[] = [FILE_A, FILE_B, PAGED_FILE, FILE_C];

function snapshotPayload(extra: WireCommand = {}): WireCommand {
  return {
    comparisonLabel: 'HEAD → working tree',
    snapshotId: 'snap-1',
    reviewId: 'review-1',
    totalInsertions: 1,
    totalDeletions: 1,
    totalsComplete: true,
    files: [FILE_A, FILE_B],
    ...extra,
  };
}

/** The authoritative per-file page the backend serves for a snapshot file. */
function fileDiffPage(snapshotId: string, path: string): CodeReviewFileDiffPageData {
  const file = SNAPSHOT_FILES.find((entry) => entry.path === path);
  const source = file ? file.hunks : [];
  const hunks: CodeReviewFileDiffHunkData[] = source.map((hunk, index) => ({
    index,
    header: hunk.header,
    oldStart: hunk.oldStart,
    oldCount: hunk.oldCount,
    newStart: hunk.newStart,
    newCount: hunk.newCount,
    contentHash: hunk.contentHash,
    totalLines: hunk.lines.length,
    complete: true,
    lineStart: 0,
    lines: hunk.lines,
  }));
  return {
    snapshotId,
    path,
    previousPath: null,
    binary: false,
    status: 'modified',
    hunks,
    cursor: 0,
    nextCursor: null,
    hasMore: false,
    totalLines: hunks.reduce((count, hunk) => count + hunk.lines.length, 0),
    hunkCount: hunks.length,
    byteCapped: false,
  };
}

function identityPayload(path: string, contentHash: string): WireCommand {
  const start = path === FILE_B.path ? 5 : 1;
  return { path, oldStart: start, oldCount: 1, newStart: start, newCount: 1, contentHash };
}

async function withPanel(
  client: RunnerClient,
  onError: (message: string) => void,
  run: (tree: renderer.ReactTestRenderer) => Promise<void>,
  onClose: () => void = () => {},
  onRegisterBackHandler?: (handler: (() => boolean) | null) => void,
): Promise<void> {
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(
      <ReviewPanel
        palette={DARK}
        client={client}
        sessionId="s1"
        onClose={onClose}
        onError={onError}
        onRegisterBackHandler={onRegisterBackHandler}
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

/** Every rendered string leaf: what a reader of the panel can actually see. */
function renderedText(tree: renderer.ReactTestRenderer): string[] {
  return tree.root
    .findAllByType(Text)
    .flatMap((node) => (typeof node.props.children === 'string' ? [node.props.children] : []));
}

/** Host elements carrying a label: exactly one per rendered control/row. */
function labeledNodes(
  tree: renderer.ReactTestRenderer,
  label: string,
): renderer.ReactTestInstance[] {
  return tree.root.findAll((node) => (
    typeof node.type === 'string' && node.props.accessibilityLabel === label
  ));
}

async function press(tree: renderer.ReactTestRenderer, label: string): Promise<void> {
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: label }).props.onPress();
  });
}

async function typeInto(
  tree: renderer.ReactTestRenderer,
  label: string,
  value: string,
): Promise<void> {
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: label }).props.onChangeText(value);
  });
}

it('sends the three /web scope payloads and rejects a malformed scope', () => {
  expect(parseCodeReviewScope('workingTree', '', '')).toEqual({
    ok: true,
    command: { type: 'code_review_open' },
  });
  expect(parseCodeReviewScope('base', ' main ', '')).toEqual({
    ok: true,
    command: { type: 'code_review_open', from: 'main', includeWorkingTree: true },
  });
  expect(parseCodeReviewScope('range', 'main', 'HEAD~2')).toEqual({
    ok: true,
    command: { type: 'code_review_open', from: 'main', to: 'HEAD~2' },
  });

  // Arity is the grammar: a missing revision, or a revision that is not one
  // token, is a malformed scope.
  expect(parseCodeReviewScope('base', '   ', '')).toEqual({ ok: false, error: CODE_REVIEW_SCOPE_USAGE });
  expect(parseCodeReviewScope('range', 'main', '')).toEqual({ ok: false, error: CODE_REVIEW_SCOPE_USAGE });
  expect(parseCodeReviewScope('range', 'main', 'a b').ok).toBe(false);
});

it('opens a review with the scope form after a not-open probe', async () => {
  const calls: WireCommand[] = [];
  let open = false;
  const client = mockClient((command) => {
    calls.push(command);
    if (command.type === 'code_review_open') {
      open = true;
      return ok('code_review_open', snapshotPayload());
    }
    if (command.type === 'code_review_snapshot' || command.type === 'code_review_refresh') {
      // Until the open lands the controller answers not-open; afterwards it
      // serves the snapshot the poll now reads — the mock must follow the
      // controller it simulates, or the poll would correctly clear the view.
      return open
        ? ok(String(command.type), snapshotPayload())
        : fail('code_review_snapshot', 'no open code review; call code_review_open first', 'code_review_not_open');
    }
    return fail(String(command.type), 'unexpected');
  });
  await withPanel(client, () => {}, async (tree) => {
    // No controller: the panel offers the scope instead of a blank workspace.
    await press(tree, 'Scope base');
    await typeInto(tree, 'Base revision', ' origin/main ');
    calls.length = 0;
    await press(tree, 'Open review');
    expect(calls[0]).toEqual({
      type: 'code_review_open',
      from: 'origin/main',
      includeWorkingTree: true,
    });
    // A successful open re-enters the open phase: the workspace renders, not
    // the scope form.
    expect(labeledNodes(tree, 'Review file src/a.ts')).toHaveLength(1);
  });
});

it('projects a well-formed snapshot and fails closed on a malformed one', async () => {
  expect(parseReviewSnapshot('not-an-object')).toBeNull();

  const parsed = parseReviewSnapshot(snapshotPayload({
    threads: [{
      identity: identityPayload(FILE_A.path, HUNK_A.contentHash),
      comments: [{ role: 'user', text: 'why?', thinking: 'checked **twice**' }],
      replyStatus: 'streaming',
      streamingText: 'checking…',
      streamingThinking: 'weighing options',
      isCancellable: true,
    }],
    activeCount: 1,
    internalBusy: true,
    reviewModelProvider: 'anthropic',
    reviewModelId: 'opus',
  }));
  expect(parsed?.files[0].path).toBe(FILE_A.path);
  expect(parsed?.files[0].hunks[0].contentHash).toBe(HUNK_A.contentHash);
  expect(parsed?.threads[0].comments[0]).toEqual({
    role: 'user',
    text: 'why?',
    partial: false,
    thinking: 'checked **twice**',
  });
  expect(parsed?.threads[0].isStreaming).toBe(true);
  expect(parsed?.threads[0].isCancellable).toBe(true);
  expect(parsed?.threads[0].streamingThinking).toBe('weighing options');
  expect(parsed?.internalBusy).toBe(true);
  expect(parsed?.reviewModelId).toBe('opus');

  // Unusable lists never fabricate rows and never throw.
  expect(parseReviewSnapshot({ files: 'nope', threads: 7 })).toEqual(emptyReviewSnapshot());

  const client = mockClient((command) => ok(String(command.type), 'garbage'));
  await withPanel(client, () => {}, async (tree) => {
    expect(labeledNodes(tree, 'Review file src/a.ts')).toHaveLength(0);
    expect(renderedText(tree)).toContain('Invalid code-review snapshot response');
  });
});

it('grows the diff window in soft-cap chunks and discards a stale page', () => {
  const parsed = parseReviewSnapshot(snapshotPayload({ files: [PAGED_FILE] }));
  const seeded = initReviewLoadedDiff('snap-1', parsed!.files[0]);

  // The snapshot seed is not the authority: its first window still needs the
  // authoritative page for this identity.
  expect(seeded.nextCursor).toBe(0);
  expect(seeded.hunksComplete).toBe(false);
  const firstPlan = planReviewDiffWindow(seeded, 0);
  expect(firstPlan.target).toBe(REVIEW_RENDER_SOFT_CAP);
  expect(firstPlan.canLoadMore).toBe(true);
  expect(firstPlan.needsFetch).toBe(true);
  expect(planReviewDiffWindow(seeded, 2 * REVIEW_RENDER_SOFT_CAP).target).toBe(3 * REVIEW_RENDER_SOFT_CAP);

  // A page from another snapshot (a review that moved on) is never merged, and
  // neither is one that arrives out of cursor order.
  const stale: CodeReviewFileDiffPageData = { ...fileDiffPage('snap-0', PAGED_FILE.path), snapshotId: 'snap-0' };
  expect(appendReviewDiffPage(seeded, stale)).toBe(seeded);
  const outOfOrder: CodeReviewFileDiffPageData = { ...fileDiffPage('snap-1', PAGED_FILE.path), cursor: 5 };
  expect(appendReviewDiffPage(seeded, outOfOrder)).toBe(seeded);

  const merged = appendReviewDiffPage(seeded, fileDiffPage('snap-1', PAGED_FILE.path));
  expect(merged).not.toBe(seeded);
  expect(merged.hunksComplete).toBe(true);
  // The seed lines are a prefix of the authoritative subset: no duplicates.
  expect(merged.lines.map((line) => line.text)).toEqual(['-gone', '+kept']);
  expect(reviewHunkIsReady(merged.hunks[0])).toBe(true);
  expect(reviewHunkKeyFor(merged, merged.hunks[0])).toBe(reviewHunkKeyFor(PAGED_FILE, HUNK_B));
  expect(planReviewDiffWindow(merged, REVIEW_DIFF_MAX_UI_LINES).canLoadMore).toBe(false);

  // A file larger than the UI hard cap plans up to the cap and no further.
  const huge = { ...merged, hasMore: true, hunksComplete: false, totalLines: 50_000 };
  const capped = planReviewDiffWindow(huge, REVIEW_DIFF_MAX_UI_LINES);
  expect(capped.target).toBe(REVIEW_DIFF_MAX_UI_LINES);
  expect(capped.hardCapped).toBe(true);
  expect(capped.canLoadMore).toBe(false);
});

it('comments on the selected hunk identity and surfaces a rejection once', async () => {
  const calls: WireCommand[] = [];
  const errors: string[] = [];
  const accepted = 'why is this **here**?';
  let live = snapshotPayload();
  let rejectComment = false;
  const client = mockClient((command) => {
    calls.push(command);
    if (command.type === 'code_review_snapshot') return ok('code_review_snapshot', live);
    if (command.type === 'code_review_file_diff') {
      return ok('code_review_file_diff', fileDiffPage('snap-1', String(command.path)));
    }
    if (command.type === 'code_review_comment') {
      if (rejectComment) return fail('code_review_comment', 'comment targets a stale snapshot');
      live = snapshotPayload({
        files: [FILE_A, FILE_B],
        threads: [{
          identity: identityPayload(FILE_A.path, HUNK_A.contentHash),
          comments: [{ role: 'assistant', text: accepted, thinking: 'checked **twice**' }],
          replyStatus: 'completed',
        }],
      });
      return ok('code_review_comment', live);
    }
    return fail(String(command.type), 'unexpected');
  });
  await withPanel(client, (message) => errors.push(message), async (tree) => {
    await press(tree, 'Review file src/a.ts');
    await press(tree, `Hunk ${HUNK_A.header}`);
    await typeInto(tree, 'Hunk comment', accepted);
    calls.length = 0;
    await press(tree, 'Comment');
    expect(calls[0]).toEqual({
      type: 'code_review_comment',
      snapshotId: 'snap-1',
      path: FILE_A.path,
      oldStart: 1,
      oldCount: 1,
      newStart: 1,
      newCount: 1,
      contentHash: HUNK_A.contentHash,
      comment: accepted,
    });

    // The accepted reply is the snapshot's, rendered as Markdown through the
    // shared body (bold inlines included) and with no role gutter.
    expect(labeledNodes(tree, 'Review comment 0')).toHaveLength(1);
    const bold = tree.root.findAllByType(Text).filter((node) => node.props.style?.fontWeight === '700');
    expect(bold.some((node) => node.props.children === 'here')).toBe(true);
    expect(renderedText(tree).some((line) => line.startsWith('assistant:'))).toBe(false);

    // Thinking renders the same way, behind its fold.
    await press(tree, 'Thinking');
    expect(renderedText(tree)).toContain('twice');

    rejectComment = true;
    errors.length = 0;
    calls.length = 0;
    await typeInto(tree, 'Hunk comment', 'rejected note');
    await press(tree, 'Comment');
    expect(calls.filter((call) => call.type === 'code_review_comment')).toHaveLength(1);
    expect(errors).toEqual(['comment targets a stale snapshot']);
    // No phantom row, and the draft survives to retry.
    expect(labeledNodes(tree, 'Review comment 1')).toHaveLength(0);
    expect(renderedText(tree)).not.toContain('rejected note');
    expect(tree.root.findByProps({ accessibilityLabel: 'Hunk comment' }).props.value).toBe('rejected note');
  });
});

it('stops the in-flight reply on its own hunk and never closes the review', async () => {
  const calls: WireCommand[] = [];
  let live = snapshotPayload({
    files: [FILE_A, FILE_B],
    threads: [
      {
        identity: identityPayload(FILE_A.path, HUNK_A.contentHash),
        comments: [],
        replyStatus: 'streaming',
        streamingText: 'reading the diff…',
        isCancellable: true,
      },
      {
        identity: identityPayload(FILE_B.path, HUNK_B.contentHash),
        comments: [],
        replyStatus: 'idle',
        isCancellable: false,
      },
    ],
    activeCount: 1,
  });
  const client = mockClient((command) => {
    calls.push(command);
    if (command.type === 'code_review_snapshot') return ok('code_review_snapshot', live);
    if (command.type === 'code_review_file_diff') {
      return ok('code_review_file_diff', fileDiffPage('snap-1', String(command.path)));
    }
    if (command.type === 'code_review_abort') {
      live = snapshotPayload({
        files: [FILE_A, FILE_B],
        threads: [{
          identity: identityPayload(FILE_A.path, HUNK_A.contentHash),
          comments: [],
          replyStatus: 'cancelled',
          isCancellable: false,
        }],
      });
      return ok('code_review_abort', live);
    }
    return fail(String(command.type), 'unexpected');
  });
  await withPanel(client, () => {}, async (tree) => {
    await press(tree, 'Review file src/a.ts');
    await press(tree, `Hunk ${HUNK_A.header}`);
    calls.length = 0;
    await press(tree, 'Stop reply');
    expect(calls[0]).toEqual({
      type: 'code_review_abort',
      snapshotId: 'snap-1',
      path: FILE_A.path,
      oldStart: 1,
      oldCount: 1,
      newStart: 1,
      newCount: 1,
      contentHash: HUNK_A.contentHash,
    });
    // Stopping a reply never closes the review, and the terminal answer
    // retires only that hunk's control.
    expect(calls.some((call) => call.type === 'code_review_close')).toBe(false);
    expect(labeledNodes(tree, 'Stop reply')).toHaveLength(0);

    // The other file's reply is not in flight: it has no stop control at all.
    await press(tree, 'Back');
    await press(tree, 'Review file src/b.ts');
    await press(tree, `Hunk ${HUNK_B.header}`);
    expect(labeledNodes(tree, 'Stop reply')).toHaveLength(0);
  });
});

const COMMIT = '4be64ed1aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa';
const PARENT = '9ab77c22bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb';
const MERGE = '22223333cccccccccccccccccccccccccccccccc';
const ROOT = '00000001dddddddddddddddddddddddddddddddd';
const COMMIT_DATE = '2026-10-25T14:03:00+02:00';

function commitRow(over: WireCommand = {}): WireCommand {
  return {
    commit: COMMIT,
    parentCommit: PARENT,
    parentCount: 1,
    subject: 'fix review binding',
    author: 'cj',
    authorDate: COMMIT_DATE,
    ...over,
  };
}

it('opens a selected commit from the bound session and keeps an empty subject', async () => {
  const calls: Array<{ command: WireCommand; sessionId?: string }> = [];
  const client = mockClient((command, sessionId) => {
    calls.push({ command, sessionId });
    if (command.type === 'code_review_snapshot') {
      return fail('code_review_snapshot', 'no open code review; call code_review_open first', 'code_review_not_open');
    }
    if (command.type === 'code_review_commits') {
      return ok('code_review_commits', {
        commits: [
          commitRow({ subject: '' }),
          commitRow({ commit: ROOT, parentCommit: null, parentCount: 0, subject: 'first commit' }),
          commitRow({ commit: MERGE, parentCount: 3, subject: 'combine branches' }),
        ],
        truncated: false,
      });
    }
    if (command.type === 'code_review_open') return fail('code_review_open', 'unknown revision');
    return fail(String(command.type), 'unexpected');
  });
  await withPanel(client, () => {}, async (tree) => {
    await press(tree, 'Scope commit');
    calls.length = 0;
    await press(tree, 'Recent commits');
    expect(calls.map((entry) => entry.command.type)).toEqual(['code_review_commits']);
    expect(calls[0]?.sessionId).toBe('s1');
    const visible = renderedText(tree);
    expect(visible).toContain('');
    expect(visible).toContain(`cj · ${COMMIT_DATE} · ${PARENT} · 1`);
    expect(visible).toContain(`cj · ${COMMIT_DATE} ·  · 0`);
    expect(visible).toContain(`cj · ${COMMIT_DATE} · ${PARENT} · 3`);
    expect(visible).toContain('first commit');
    expect(visible).toContain('combine branches');
    calls.length = 0;
    await press(tree, `Commit ${COMMIT}`);
    expect(calls[0]?.command).toEqual({ type: 'code_review_open', commit: COMMIT });
    expect(calls[0]?.sessionId).toBe('s1');
    expect(renderedText(tree)).toContain('unknown revision');
    expect(tree.root.findByProps({ accessibilityLabel: 'Commit ref' }).props.value).toBe(COMMIT);
    expect(labeledNodes(tree, `Commit ${COMMIT}`)).toHaveLength(0);
  });
});

it('keeps the commit draft on an invalid ref and drops a stale list', async () => {
  const calls: WireCommand[] = [];
  let releaseStale: (() => void) | undefined;
  const stale = new Promise<unknown>((resolve) => {
    releaseStale = () => resolve(ok('code_review_commits', {
      commits: [commitRow({ commit: 'stalehashshouldnotopen0000000000000000', subject: 'stale subject' })],
      truncated: false,
    }));
  });
  let commits = 0;
  const client = mockClient((command) => {
    calls.push(command);
    if (command.type === 'code_review_snapshot') {
      return fail('code_review_snapshot', 'no open code review; call code_review_open first', 'code_review_not_open');
    }
    if (command.type === 'code_review_commits') {
      commits += 1;
      if (commits === 1) return stale;
      return ok('code_review_commits', { commits: [], truncated: false });
    }
    if (command.type === 'code_review_open') return fail('code_review_open', 'unknown revision');
    return fail(String(command.type), 'unexpected');
  });
  const closed: boolean[] = [];
  let back: (() => boolean) | null = null;
  await withPanel(client, () => {}, async (tree) => {
    await press(tree, 'Scope commit');
    await typeInto(tree, 'Commit ref', 'not a ref');
    calls.length = 0;
    await press(tree, 'Open review');
    expect(calls.some((call) => call.type === 'code_review_open')).toBe(false);
    expect(renderedText(tree)).toContain(CODE_REVIEW_SCOPE_USAGE);
    expect(tree.root.findByProps({ accessibilityLabel: 'Commit ref' }).props.value).toBe('not a ref');

    await typeInto(tree, 'Commit ref', 'missing-ref');
    calls.length = 0;
    await press(tree, 'Open review');
    expect(calls[0]).toEqual({ type: 'code_review_open', commit: 'missing-ref' });
    expect(renderedText(tree)).toContain('unknown revision');
    expect(tree.root.findByProps({ accessibilityLabel: 'Commit ref' }).props.value).toBe('missing-ref');
    expect(labeledNodes(tree, 'Review file src/a.ts')).toHaveLength(0);

    await press(tree, 'Recent commits');
    await act(async () => {
      expect(back?.()).toBe(true);
    });
    expect(labeledNodes(tree, `Commit ${COMMIT}`)).toHaveLength(0);
    await press(tree, 'Recent commits');
    expect(renderedText(tree)).toContain('No commits.');
    await act(async () => {
      releaseStale?.();
    });
    expect(renderedText(tree)).not.toContain('stale subject');
    expect(labeledNodes(tree, 'Commit stalehashshouldnotopen0000000000000000')).toHaveLength(0);

    expect(labeledNodes(tree, 'Recent commits')).toHaveLength(1);
    expect(closed).toEqual([]);
    await act(async () => {
      expect(back?.()).toBe(true);
    });
    expect(renderedText(tree)).not.toContain('No commits.');
    expect(closed).toEqual([]);
    await act(async () => {
      expect(back?.()).toBe(true);
    });
    expect(closed).toEqual([true]);
  }, () => closed.push(true), (handler) => {
    back = handler;
  });
});

it('suppresses only a typed not-open commits failure', async () => {
  const client = mockClient((command) => {
    if (command.type === 'code_review_snapshot') {
      return fail('code_review_snapshot', 'no open code review; call code_review_open first', 'code_review_not_open');
    }
    if (command.type === 'code_review_commits') {
      return fail('code_review_commits', 'no open code review; call code_review_open first', 'code_review_not_open');
    }
    return fail(String(command.type), 'unexpected');
  });
  await withPanel(client, () => {}, async (tree) => {
    await press(tree, 'Scope commit');
    await press(tree, 'Recent commits');
    expect(renderedText(tree)).not.toContain('no open code review; call code_review_open first');
    expect(labeledNodes(tree, 'Commit ref')).toHaveLength(1);
  });
});

it('keeps finding categories strict and derives the hunk aggregate', () => {
  const identity = { ...identityPayload(FILE_A.path, HUNK_A.contentHash), snapshotId: 'snap-1' };
  const namingFinding = {
    category: 'naming',
    side: 'new',
    startLine: 1,
    endLine: 1,
    summary: 'Naming: banned verb',
    evidence: 'The helper is named read but writes.',
  };
  const naming = parseReviewSnapshot(snapshotPayload({
    autoReviewState: 'done',
    highlightState: 'done',
    threads: [{
      identity: identityPayload(FILE_A.path, HUNK_A.contentHash),
      comments: [{ role: 'assistant', text: 'rename only' }],
      replyStatus: 'completed',
    }],
    reviewFindings: [{ identity, findings: [namingFinding] }],
  }));
  expect(naming?.reviewFindings).toHaveLength(1);
  expect(naming?.reviewFindings[0].findings).toEqual([namingFinding]);
  expect(reviewFindingsForHunk(naming!.reviewFindings, naming!.reviewFindings[0].identity)).toEqual([namingFinding]);
  expect(reviewAggregateCategory([namingFinding])).toBe('naming');
  expect(reviewAggregateCategory([])).toBe('clean');
  expect(reviewAggregateCategory([
    namingFinding,
    { ...namingFinding, category: 'overdesign' },
    { ...namingFinding, category: 'critical' },
  ])).toBe('critical');
  expect(reviewCategoryLabel('naming')).toBe('Naming');
  expect(reviewCategoryLabel('overdesign')).toBe('Overdesign');
  expect(reviewCategoryLabel('critical')).toBe('Critical');
  expect(reviewCategoryLabel('clean')).toBe('Clean');
  expect(reviewStageFeedback(naming)).toBe('Auto Review done');
  expect(reviewStageFeedback({ autoReviewState: 'triaging', highlightState: 'idle' })).toBe('Auto Review triaging');
  expect(reviewStageFeedback({ autoReviewState: 'idle', highlightState: 'running' })).toBe('Highlight running');

  for (const finding of [
    { ...namingFinding, category: 'clean' },
    { ...namingFinding, severity: 'warning' },
    { ...namingFinding, category: 'constructor' },
    { ...namingFinding, side: 'added' },
    { ...namingFinding, startLine: 0 },
    { ...namingFinding, startLine: 3, endLine: 2 },
    { ...namingFinding, endLine: 4294967296 },
    { ...namingFinding, summary: '   ' },
    { ...namingFinding, summary: 'a'.repeat(513) },
    { ...namingFinding, evidence: '' },
    { ...namingFinding, evidence: 'a'.repeat(8193) },
    { ...namingFinding, extra: 1 },
  ]) {
    expect(parseReviewSnapshot(snapshotPayload({
      reviewFindings: [{ identity, findings: [finding] }],
    }))).toBeNull();
  }
  // Duplicate hunk identity and duplicate finding both fail closed.
  expect(parseReviewSnapshot(snapshotPayload({
    reviewFindings: [{ identity, findings: [namingFinding] }, { identity, findings: [] }],
  }))).toBeNull();
  expect(parseReviewSnapshot(snapshotPayload({
    reviewFindings: [{ identity, findings: [namingFinding, namingFinding] }],
  }))).toBeNull();
  expect(parseReviewSnapshot(snapshotPayload({
    reviewFindings: [{ identity, findings: [namingFinding], extra: true }],
  }))).toBeNull();
  // A result must name a hunk of THIS snapshot with all seven identity
  // fields: a stale snapshot id or a foreign/unmatched hunk rejects the
  // whole payload instead of attaching to the current hunk.
  expect(parseReviewSnapshot(snapshotPayload({
    reviewFindings: [{ identity: { ...identity, snapshotId: 'snap-0' }, findings: [namingFinding] }],
  }))).toBeNull();
  expect(parseReviewSnapshot(snapshotPayload({
    reviewFindings: [{ identity: { ...identity, path: 'src/unknown.ts' }, findings: [namingFinding] }],
  }))).toBeNull();
  expect(parseReviewSnapshot(snapshotPayload({
    reviewFindings: [{ identity: { ...identity, contentHash: 'stale-hash' }, findings: [namingFinding] }],
  }))).toBeNull();
  // A finding whose range a complete hunk does not fully cover on its side
  // rejects the payload at the snapshot boundary; full coverage passes.
  const fileCIdentity = {
    snapshotId: 'snap-1',
    path: FILE_C.path,
    oldStart: HUNK_C1.oldStart,
    oldCount: HUNK_C1.oldCount,
    newStart: HUNK_C1.newStart,
    newCount: HUNK_C1.newCount,
    contentHash: HUNK_C1.contentHash,
  };
  expect(parseReviewSnapshot(snapshotPayload({
    files: [FILE_C],
    reviewFindings: [{ identity: fileCIdentity, findings: [{ ...namingFinding, side: 'old', startLine: 3, endLine: 6 }] }],
  }))).toBeNull();
  expect(parseReviewSnapshot(snapshotPayload({
    files: [FILE_C],
    reviewFindings: [{ identity: fileCIdentity, findings: [{ ...namingFinding, side: 'old', startLine: 3, endLine: 5 }] }],
  }))?.reviewFindings[0].findings).toHaveLength(1);

  // The budget is per hunk (one model response): 20 DISTINCT findings in the
  // same fully-covered range stay, the 21st unique summary rejects, and a
  // second hunk carries its own budget — the collection is never capped at 20.
  const twenty = Array.from({ length: 20 }, (_, index) => ({
    ...namingFinding,
    summary: `Naming: finding ${index + 1}`,
  }));
  expect(parseReviewSnapshot(snapshotPayload({
    reviewFindings: [{ identity, findings: twenty }],
  }))?.reviewFindings[0].findings).toHaveLength(20);
  expect(parseReviewSnapshot(snapshotPayload({
    reviewFindings: [{ identity, findings: [...twenty, { ...namingFinding, summary: 'Naming: finding 21' }] }],
  }))).toBeNull();
  const both = parseReviewSnapshot(snapshotPayload({
    reviewFindings: [
      { identity, findings: twenty },
      // HUNK_B's new-side row sits at line 5: its finding targets the range
      // that hunk actually covers, not FILE_A's 1..1.
      { identity: { ...identityPayload(FILE_B.path, HUNK_B.contentHash), snapshotId: 'snap-1' }, findings: [{ ...namingFinding, startLine: 5, endLine: 5 }] },
    ],
  }));
  expect(both?.reviewFindings[0].findings).toHaveLength(20);
  expect(both?.reviewFindings[1].findings).toHaveLength(1);
});

it('renders structured findings without calling anything critical', async () => {
  const live = snapshotPayload({
    autoReviewState: 'running',
    threads: [{
      identity: identityPayload(FILE_A.path, HUNK_A.contentHash),
      comments: [{ role: 'assistant', text: 'policy only' }],
      replyStatus: 'completed',
    }],
    reviewFindings: [{
      identity: { ...identityPayload(FILE_A.path, HUNK_A.contentHash), snapshotId: 'snap-1' },
      findings: [
        { category: 'naming', side: 'new', startLine: 1, endLine: 1, summary: 'banned verb', evidence: 'rename it' },
        { category: 'overdesign', side: 'old', startLine: 1, endLine: 1, summary: 'unused layer', evidence: 'drop it' },
      ],
    }],
  });
  const client = mockClient((command) => {
    if (command.type === 'code_review_snapshot') return ok('code_review_snapshot', live);
    if (command.type === 'code_review_file_diff') {
      return ok('code_review_file_diff', fileDiffPage('snap-1', String(command.path)));
    }
    return fail(String(command.type), 'unexpected');
  });
  await withPanel(client, () => {}, async (tree) => {
    expect(renderedText(tree)).toContain('Auto Review running');
    await press(tree, 'Review file src/a.ts');
    await press(tree, `Hunk ${HUNK_A.header}`);
    // The detail lists both structured findings and the derived aggregate;
    // overdesign outranks naming and nothing is called critical.
    expect(renderedText(tree)).toContain('Naming · lines 1–1 · new');
    expect(renderedText(tree)).toContain('Overdesign · lines 1–1 · old');
    expect(renderedText(tree)).toContain('Overdesign · 2');
    expect(renderedText(tree).some((line) => line.startsWith('Critical'))).toBe(false);
    // The manual comment survived next to the structured findings.
    expect(renderedText(tree)).toContain('policy only');
  });
});

it('matches findings against side-specific numbered rows only', () => {
  const lines: DiffLine[] = [
    { kind: 'context', text: ' ctx', oldNo: 3, newNo: 3 },
    { kind: 'deletion', text: '-old', oldNo: 4 },
    { kind: 'addition', text: '+new', newNo: 4 },
    { kind: 'context', text: ' ctx2', oldNo: 5, newNo: 5 },
  ];
  expect(reviewFindingRowRange(lines, { side: 'old', startLine: 4, endLine: 4 })).toEqual({ first: 1, last: 1 });
  expect(reviewFindingRowRange(lines, { side: 'new', startLine: 4, endLine: 4 })).toEqual({ first: 2, last: 2 });
  // The same number on the wrong side claims no row.
  expect(reviewFindingRowRange(lines, { side: 'old', startLine: 2, endLine: 2 })).toBeNull();
  // Context rows carry both coordinates; a range may span kinds.
  expect(reviewFindingRowRange(lines, { side: 'new', startLine: 3, endLine: 5 })).toEqual({ first: 0, last: 3 });
  expect(reviewFindingRowRange(lines, { side: 'old', startLine: 9, endLine: 12 })).toBeNull();
  // Full coverage is required: a range one of whose coordinates has no
  // same-side row (here old 6) is not represented exactly.
  expect(reviewFindingRowRange(lines, { side: 'old', startLine: 3, endLine: 6 })).toBeNull();
  // Paged rows still pending are never guessed: with only the first two rows
  // loaded, new 5 is missing and the 3–5 range stays unresolved.
  expect(reviewFindingRowRange(lines.slice(0, 2), { side: 'new', startLine: 3, endLine: 5 })).toBeNull();
});

it('orders the global finding list by file, hunk, row, side, and category', () => {
  // FILE_B sorts before FILE_A in the snapshot even though 'a.ts' < 'b.ts'.
  const snap = parseReviewSnapshot(snapshotPayload({
    files: [FILE_B, FILE_A],
    reviewFindings: [
      {
        identity: { ...identityPayload(FILE_A.path, HUNK_A.contentHash), snapshotId: 'snap-1' },
        findings: [
          { category: 'naming', side: 'new', startLine: 1, endLine: 1, summary: 'a-new', evidence: 'e' },
          { category: 'critical', side: 'old', startLine: 1, endLine: 1, summary: 'a-old', evidence: 'e' },
        ],
      },
      {
        identity: { ...identityPayload(FILE_B.path, HUNK_B.contentHash), snapshotId: 'snap-1' },
        findings: [
          { category: 'naming', side: 'new', startLine: 5, endLine: 5, summary: 'b-new', evidence: 'e' },
        ],
      },
    ],
  }))!;
  const entries = orderReviewFindings(snap);
  expect(entries.map((entry) => entry.finding.summary)).toEqual(['b-new', 'a-old', 'a-new']);
  expect(entries.map((entry) => entry.path)).toEqual(['src/b.ts', 'src/a.ts', 'src/a.ts']);
  // References are typed indices, not summary text.
  expect(entries[2].ref).toEqual({
    identity: { ...identityPayload(FILE_A.path, HUNK_A.contentHash), snapshotId: 'snap-1' },
    findingIndex: 0,
  });
  // With no selection Next selects the first entry; Previous the last; and a
  // selected entry at a boundary has no neighbor (no wrapping).
  expect(reviewFindingNeighbor(entries, null, 'next')?.finding.summary).toBe('b-new');
  expect(reviewFindingNeighbor(entries, null, 'previous')?.finding.summary).toBe('a-new');
  expect(reviewFindingNeighbor(entries, entries[2].ref, 'next')).toBeNull();
  expect(reviewFindingNeighbor(entries, entries[0].ref, 'previous')).toBeNull();
  expect(reviewFindingNeighbor(entries, entries[0].ref, 'next')?.finding.summary).toBe('a-old');
});

it('marks exact rows, navigates previous/next across files, and keeps the draft', async () => {
  const live = snapshotPayload({
    files: [FILE_A, FILE_B],
    reviewFindings: [
      {
        identity: { ...identityPayload(FILE_A.path, HUNK_A.contentHash), snapshotId: 'snap-1' },
        findings: [
          { category: 'critical', side: 'old', startLine: 1, endLine: 1, summary: 'first', evidence: 'old row' },
        ],
      },
      {
        identity: { ...identityPayload(FILE_B.path, HUNK_B.contentHash), snapshotId: 'snap-1' },
        findings: [
          { category: 'naming', side: 'new', startLine: 5, endLine: 5, summary: 'second', evidence: 'new row' },
        ],
      },
    ],
  });
  const client = mockClient((command) => {
    if (command.type === 'code_review_snapshot') return ok('code_review_snapshot', live);
    if (command.type === 'code_review_file_diff') {
      return ok('code_review_file_diff', fileDiffPage('snap-1', String(command.path)));
    }
    return fail(String(command.type), 'unexpected');
  });
  await withPanel(client, () => {}, async (tree) => {
    await press(tree, 'Review file src/a.ts');
    // With no selection, Next selects the first finding: the FILE_A hunk
    // opens and its detail shows the typed reference's finding.
    await press(tree, 'Next finding');
    expect(renderedText(tree)).toContain('Finding 1 of 2');
    expect(renderedText(tree)).toContain('Critical · lines 1–1 · old');
    expect(renderedText(tree)).toContain('Critical · 1');
    await typeInto(tree, 'Hunk comment', 'keep me');
    // Next crosses into FILE_B; the manual draft is preserved.
    await press(tree, 'Next finding');
    expect(renderedText(tree)).toContain('Naming · lines 5–5 · new');
    expect(renderedText(tree)).toContain('Naming · 1');
    expect(tree.root.findByProps({ accessibilityLabel: 'Hunk comment' }).props.value).toBe('keep me');
    // The last finding disables Next (no wrapping); Previous walks back.
    expect(tree.root.findByProps({ accessibilityLabel: 'Next finding' }).props.disabled).toBe(true);
    await press(tree, 'Previous finding');
    expect(renderedText(tree)).toContain('Critical · lines 1–1 · old');
    expect(tree.root.findByProps({ accessibilityLabel: 'Previous finding' }).props.disabled).toBe(true);
  });
});

it('marks only rows carrying a coordinate on the finding side', async () => {
  const live = snapshotPayload({
    files: [FILE_C],
    reviewFindings: [{
      identity: { snapshotId: 'snap-1', path: FILE_C.path, oldStart: HUNK_C1.oldStart, oldCount: HUNK_C1.oldCount, newStart: HUNK_C1.newStart, newCount: HUNK_C1.newCount, contentHash: HUNK_C1.contentHash },
      findings: [
        // Old-side range 3–5 spans the inserted row: the addition has no old
        // coordinate and must stay unmarked while its neighbors are marked.
        { category: 'critical', side: 'old', startLine: 3, endLine: 5, summary: 'old range', evidence: 'old rows' },
      ],
    }],
  });
  const client = mockClient((command) => {
    if (command.type === 'code_review_snapshot') return ok('code_review_snapshot', live);
    if (command.type === 'code_review_file_diff') {
      return ok('code_review_file_diff', fileDiffPage('snap-1', String(command.path)));
    }
    return fail(String(command.type), 'unexpected');
  });
  await withPanel(client, () => {}, async (tree) => {
    await press(tree, 'Review file src/c.ts');
    const markWidth = (needle: string): number[] => tree.root
      .findAllByType(Text)
      .filter((node) => node.props.children === needle)
      .map((node) => node.props.style[1].borderLeftWidth);
    expect(markWidth(' ctx')).toEqual([3]);
    expect(markWidth('-old')).toEqual([3]);
    // The inserted row between the matching rows is NOT marked.
    expect(markWidth('+new')).toEqual([0]);
    expect(markWidth(' ctx2')).toEqual([3]);
    // An unaffected row of the same file (the second hunk) stays unmarked.
    expect(markWidth('+second hunk')).toEqual([0]);
  });
});

it('navigates between two hunks of one file without touching the file', async () => {
  const live = snapshotPayload({
    files: [FILE_C],
    reviewFindings: [
      {
        identity: { snapshotId: 'snap-1', path: FILE_C.path, oldStart: HUNK_C1.oldStart, oldCount: HUNK_C1.oldCount, newStart: HUNK_C1.newStart, newCount: HUNK_C1.newCount, contentHash: HUNK_C1.contentHash },
        findings: [
          { category: 'overdesign', side: 'old', startLine: 3, endLine: 5, summary: 'first hunk', evidence: 'a' },
        ],
      },
      {
        identity: { snapshotId: 'snap-1', path: FILE_C.path, oldStart: HUNK_C2.oldStart, oldCount: HUNK_C2.oldCount, newStart: HUNK_C2.newStart, newCount: HUNK_C2.newCount, contentHash: HUNK_C2.contentHash },
        findings: [
          { category: 'naming', side: 'new', startLine: 10, endLine: 10, summary: 'second hunk', evidence: 'b' },
        ],
      },
    ],
  });
  const client = mockClient((command) => {
    if (command.type === 'code_review_snapshot') return ok('code_review_snapshot', live);
    if (command.type === 'code_review_file_diff') {
      return ok('code_review_file_diff', fileDiffPage('snap-1', String(command.path)));
    }
    return fail(String(command.type), 'unexpected');
  });
  await withPanel(client, () => {}, async (tree) => {
    await press(tree, 'Review file src/c.ts');
    await press(tree, 'Next finding');
    expect(renderedText(tree)).toContain('Overdesign · lines 3–5 · old');
    await press(tree, 'Next finding');
    // The file stays open; the second hunk's detail is now shown.
    expect(renderedText(tree)).toContain('Naming · lines 10–10 · new');
    expect(renderedText(tree)).not.toContain('Overdesign · lines 3–5 · old');
    expect(renderedText(tree)).toContain('Finding 2 of 2');
    expect(tree.root.findByProps({ accessibilityLabel: 'Next finding' }).props.disabled).toBe(true);
    // Back to the first hunk: same file, detail follows the reference.
    await press(tree, 'Previous finding');
    expect(renderedText(tree)).toContain('Overdesign · lines 3–5 · old');
    expect(tree.root.findByProps({ accessibilityLabel: 'Previous finding' }).props.disabled).toBe(true);
  });
});

const TLDR_MARKDOWN = '## Overview\nOverview of the pinned diff.\n\n## Details\nDetail row src/a.ts.';

it('starts the TLDR, keeps the ack alive over stale idle polls, and renders one shared body', async () => {
  const calls: WireCommand[] = [];
  const errors: string[] = [];
  let live = snapshotPayload({ files: [FILE_A, FILE_B] });
  const client = mockClient((command) => {
    calls.push(command);
    if (command.type === 'code_review_snapshot' || command.type === 'code_review_refresh') {
      return ok(String(command.type), live);
    }
    if (command.type === 'code_review_tldr') {
      return ok('code_review_tldr', { ok: true, state: 'running', tldrGeneration: 2 });
    }
    return fail(String(command.type), 'unexpected');
  });
  await withPanel(client, (message) => errors.push(message), async (tree) => {
    expect(labeledNodes(tree, 'Code review summary')).toHaveLength(0);
    await press(tree, 'TL;DR');
    expect(calls.some((call) => call.type === 'code_review_tldr')).toBe(true);
    // The ack owns running before any snapshot confirms it.
    expect(labeledNodes(tree, 'Code review summary')).toHaveLength(1);
    expect(tree.root.findByProps({ accessibilityLabel: 'TLDR state' }).props.children).toBe('Generating…');
    expect(labeledNodes(tree, 'Stop summary')).toHaveLength(1);

    // A newer acked run also supersedes an older owned result on the SAME
    // snapshot: the stale done body never renders while the run is pending.
    live = snapshotPayload({
      files: [FILE_A, FILE_B],
      tldrState: 'done',
      tldrGeneration: 1,
      tldrSnapshotId: 'snap-1',
      tldrMarkdown: TLDR_MARKDOWN,
    });
    await press(tree, 'Refresh review');
    expect(tree.root.findByProps({ accessibilityLabel: 'TLDR state' }).props.children).toBe('Generating…');
    expect(renderedText(tree)).not.toContain('Overview of the pinned diff.');

    // A poll older than the acked generation never erases the running ack.
    live = snapshotPayload({ files: [FILE_A, FILE_B], tldrState: 'idle', tldrGeneration: 1 });
    await press(tree, 'Refresh review');
    expect(tree.root.findByProps({ accessibilityLabel: 'TLDR state' }).props.children).toBe('Generating…');

    // The controller confirms the run against THIS snapshot id.
    live = snapshotPayload({
      files: [FILE_A, FILE_B],
      tldrState: 'running',
      tldrGeneration: 2,
      tldrSnapshotId: 'snap-1',
    });
    await press(tree, 'Refresh review');
    expect(tree.root.findByProps({ accessibilityLabel: 'TLDR state' }).props.children).toBe('Generating…');
    expect(renderedText(tree)).not.toContain('Overview of the pinned diff.');

    // Done: the owned summary opens expanded. Collapse and expand render the
    // SAME document (overview and details together, never split by headings).
    live = snapshotPayload({
      files: [FILE_A, FILE_B],
      tldrState: 'done',
      tldrGeneration: 2,
      tldrSnapshotId: 'snap-1',
      tldrMarkdown: TLDR_MARKDOWN,
    });
    await press(tree, 'Refresh review');
    expect(tree.root.findByProps({ accessibilityLabel: 'TLDR state' }).props.children).toBe('Ready');
    expect(renderedText(tree)).toContain('Overview of the pinned diff.');
    expect(renderedText(tree)).toContain('Detail row src/a.ts.');
    expect(labeledNodes(tree, 'Stop summary')).toHaveLength(0);
    expect(labeledNodes(tree, 'Collapse details')).toHaveLength(1);
    expect(labeledNodes(tree, 'Expand details')).toHaveLength(0);
    // A later OLDER same-owner poll cannot regress the accepted generation:
    // the newer TLDR fields stay adopted after the pending ack cleared.
    live = snapshotPayload({
      files: [FILE_A, FILE_B],
      tldrState: 'done',
      tldrGeneration: 1,
      tldrSnapshotId: 'snap-1',
      tldrMarkdown: 'Stale older body.',
    });
    await press(tree, 'Refresh review');
    expect(tree.root.findByProps({ accessibilityLabel: 'TLDR state' }).props.children).toBe('Ready');
    expect(renderedText(tree)).toContain('Overview of the pinned diff.');
    expect(renderedText(tree)).not.toContain('Stale older body.');
    expect(labeledNodes(tree, 'Collapse details')).toHaveLength(1);
    await press(tree, 'Collapse details');
    expect(labeledNodes(tree, 'Expand details')).toHaveLength(1);
    expect(renderedText(tree)).toContain('Overview of the pinned diff.');
    expect(renderedText(tree)).toContain('Detail row src/a.ts.');
    await press(tree, 'Expand details');
    expect(labeledNodes(tree, 'Collapse details')).toHaveLength(1);
    expect(renderedText(tree)).toContain('Overview of the pinned diff.');
    expect(renderedText(tree)).toContain('Detail row src/a.ts.');
    await press(tree, 'Collapse details');

    // Close unmounts the pane; it never closes the review. Reopening a done
    // summary starts expanded again and keeps the accepted body.
    await press(tree, 'Close summary');
    expect(labeledNodes(tree, 'Code review summary')).toHaveLength(0);
    expect(calls.some((call) => call.type === 'code_review_close')).toBe(false);
    await press(tree, 'TL;DR');
    expect(labeledNodes(tree, 'Code review summary')).toHaveLength(1);
    expect(labeledNodes(tree, 'Collapse details')).toHaveLength(1);
    expect(labeledNodes(tree, 'Expand details')).toHaveLength(0);
    expect(tree.root.findByProps({ accessibilityLabel: 'TLDR state' }).props.children).toBe('Ready');
    expect(renderedText(tree)).toContain('Overview of the pinned diff.');
    expect(renderedText(tree)).toContain('Detail row src/a.ts.');
    expect(calls.filter((call) => call.type === 'code_review_tldr')).toHaveLength(1);
    expect(calls.some((call) => call.type === 'code_review_close')).toBe(false);
  });
  expect(errors).toEqual([]);
});

it('never shows a done or failed result from an older snapshot', async () => {
  const errors: string[] = [];
  let live = snapshotPayload({ files: [FILE_A, FILE_B] });
  const client = mockClient((command) => {
    if (command.type === 'code_review_snapshot' || command.type === 'code_review_refresh') {
      return ok(String(command.type), live);
    }
    if (command.type === 'code_review_tldr') {
      return ok('code_review_tldr', { ok: true, state: 'running', tldrGeneration: 2 });
    }
    return fail(String(command.type), 'unexpected');
  });
  await withPanel(client, (message) => errors.push(message), async (tree) => {
    await press(tree, 'TL;DR');
    expect(tree.root.findByProps({ accessibilityLabel: 'TLDR state' }).props.children).toBe('Generating…');

    // The snapshot changed under the run: the old result is not this
    // snapshot's, so neither its summary nor its failure may render.
    live = snapshotPayload({
      files: [FILE_A, FILE_B],
      tldrState: 'done',
      tldrGeneration: 1,
      tldrSnapshotId: 'snap-0',
      tldrMarkdown: TLDR_MARKDOWN,
    });
    await press(tree, 'Refresh review');
    // The acked run is still live: the panel shows it, not the foreign result.
    expect(tree.root.findByProps({ accessibilityLabel: 'TLDR state' }).props.children).toBe('Generating…');
    expect(renderedText(tree)).not.toContain('Overview of the pinned diff.');
    live = snapshotPayload({
      files: [FILE_A, FILE_B],
      tldrState: 'failed',
      tldrGeneration: 1,
      tldrSnapshotId: 'snap-0',
      tldrError: 'leaked failure text',
    });
    await press(tree, 'Refresh review');
    expect(tree.root.findByProps({ accessibilityLabel: 'TLDR state' }).props.children).toBe('Generating…');
    expect(renderedText(tree)).not.toContain('leaked failure text');

    // Once the run belongs to this snapshot the summary renders: the snapshot
    // reached the acked generation, so the pending ack is superseded.
    live = snapshotPayload({
      files: [FILE_A, FILE_B],
      tldrState: 'done',
      tldrGeneration: 2,
      tldrSnapshotId: 'snap-1',
      tldrMarkdown: TLDR_MARKDOWN,
    });
    await press(tree, 'Refresh review');
    expect(tree.root.findByProps({ accessibilityLabel: 'TLDR state' }).props.children).toBe('Ready');
    expect(renderedText(tree)).toContain('Overview of the pinned diff.');
  });
  expect(errors).toEqual([]);
});

it('stops the running summary through the typed tldr operation and never closes the review', async () => {
  const calls: WireCommand[] = [];
  const errors: string[] = [];
  let live = snapshotPayload({
    files: [FILE_A, FILE_B],
    tldrState: 'running',
    tldrGeneration: 1,
    tldrSnapshotId: 'snap-1',
  });
  const client = mockClient((command) => {
    calls.push(command);
    if (command.type === 'code_review_snapshot' || command.type === 'code_review_refresh') {
      return ok(String(command.type), live);
    }
    if (command.type === 'code_review_stop') {
      live = snapshotPayload({ files: [FILE_A, FILE_B], tldrState: 'idle', tldrGeneration: 1 });
      return ok('code_review_stop', { ok: true, operation: 'tldr' });
    }
    return fail(String(command.type), 'unexpected');
  });
  await withPanel(client, (message) => errors.push(message), async (tree) => {
    await press(tree, 'TL;DR');
    // An already-running snapshot opens the pane without starting a second run.
    expect(calls.some((call) => call.type === 'code_review_tldr')).toBe(false);
    expect(tree.root.findByProps({ accessibilityLabel: 'TLDR state' }).props.children).toBe('Generating…');
    expect(labeledNodes(tree, 'Stop summary')).toHaveLength(1);
    calls.length = 0;
    await press(tree, 'Stop summary');
    expect(calls[0]).toEqual({ type: 'code_review_stop', operation: 'tldr' });
    expect(calls.some((call) => call.type === 'code_review_close')).toBe(false);
    await press(tree, 'Refresh review');
    expect(tree.root.findByProps({ accessibilityLabel: 'TLDR state' }).props.children).toBe('Idle');
    expect(labeledNodes(tree, 'Stop summary')).toHaveLength(0);
    // The pane stays open and idle: the action restarts from here.
    expect(labeledNodes(tree, 'Code review summary')).toHaveLength(1);
  });
  expect(errors).toEqual([]);
});

it('reports a malformed tldr ack centrally and never fakes a run', async () => {
  const errors: string[] = [];
  let live = snapshotPayload({ files: [FILE_A, FILE_B] });
  let ack: unknown = { state: 'running' };
  const client = mockClient((command) => {
    if (command.type === 'code_review_snapshot' || command.type === 'code_review_refresh') {
      return ok(String(command.type), live);
    }
    if (command.type === 'code_review_tldr') return ok('code_review_tldr', ack);
    return fail(String(command.type), 'unexpected');
  });
  await withPanel(client, (message) => errors.push(message), async (tree) => {
    await press(tree, 'TL;DR');
    expect(errors).toEqual(['Invalid code_review_tldr ack response']);
    expect(labeledNodes(tree, 'Code review summary')).toHaveLength(0);

    // A well-formed retry starts the run.
    ack = { ok: true, state: 'running', tldrGeneration: 1 };
    await press(tree, 'TL;DR');
    expect(tree.root.findByProps({ accessibilityLabel: 'TLDR state' }).props.children).toBe('Generating…');
  });
});

it('retries a failed summary from the TL;DR action instead of stranding it', async () => {
  const calls: WireCommand[] = [];
  const errors: string[] = [];
  let live = snapshotPayload({ files: [FILE_A, FILE_B] });
  let ack: unknown = { ok: true, state: 'running', tldrGeneration: 1 };
  const client = mockClient((command) => {
    calls.push(command);
    if (command.type === 'code_review_snapshot' || command.type === 'code_review_refresh') {
      return ok(String(command.type), live);
    }
    if (command.type === 'code_review_tldr') return ok('code_review_tldr', ack);
    return fail(String(command.type), 'unexpected');
  });
  await withPanel(client, (message) => errors.push(message), async (tree) => {
    await press(tree, 'TL;DR');
    live = snapshotPayload({
      files: [FILE_A, FILE_B],
      tldrState: 'failed',
      tldrGeneration: 1,
      tldrSnapshotId: 'snap-1',
      tldrError: 'summary backend failed',
    });
    await press(tree, 'Refresh review');
    // The owned failure renders with its safe text and no Stop control.
    expect(tree.root.findByProps({ accessibilityLabel: 'TLDR state' }).props.children).toBe('Failed');
    expect(renderedText(tree)).toContain('summary backend failed');
    expect(labeledNodes(tree, 'Stop summary')).toHaveLength(0);
    calls.length = 0;
    ack = { ok: true, state: 'running', tldrGeneration: 2 };
    await press(tree, 'TL;DR');
    // The same action retries: a new generation is acked and running shows.
    expect(calls.some((call) => call.type === 'code_review_tldr')).toBe(true);
    expect(tree.root.findByProps({ accessibilityLabel: 'TLDR state' }).props.children).toBe('Generating…');
  });
  expect(errors).toEqual([]);
});
