#!/usr/bin/env node
// Focused regression for src/codeReview.ts — defensive wire normalization
// of the code_review_* snapshot payload (files/hunks/lines/threads) plus
// /code-review argument arity. Run through `npm run build`.
//
// Exit codes: 0 = every assertion held; 1 = a regression.
// Assertions exercise BEHAVIOR (what normalize returns), not source strings.
import {
  buildCodeReviewAbortPayload,
  clampCodeReviewThreadWidth,
  CODE_REVIEW_THREAD_WIDTH_DEFAULT,
  CODE_REVIEW_THREAD_WIDTH_MAX,
  CODE_REVIEW_THREAD_WIDTH_MIN,
  CODE_REVIEW_THREAD_WIDTH_STEP,
  CODE_REVIEW_THREAD_WIDTH_STORAGE_KEY,
  countFileThreads,
  countThreadComments,
  emptyCodeReviewSnapshot,
  FILE_STATUS_LETTERS,
  fileStatusLetter,
  findThreadForHunk,
  formatActiveRepliesLabel,
  hunkIdentityFor,
  hunkKey,
  hunkKeyFor,
  normalizeCodeReviewSnapshot,
  normalizeThreads,
  parseCodeReviewArgs,
  appendFileDiffPage,
  buildFileTree,
  hunkIsComplete,
  initLoadedDiff,
  isDiffPlaceholder,
  loadedHunkReady,
  normalizeFileDiffPage,
  planDiffWindow,
  readStoredCodeReviewThreadWidth,
  stepCodeReviewThreadWidth,
  threadIsStreaming,
  treeFileIndexAt,
  treeFilterRows,
  treeKeyboardAction,
  treeToggleCollapse,
  treeVisibleRows,
  writeStoredCodeReviewThreadWidth,
} from '../src/codeReview.ts';

const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

// ---- bad shapes never throw; error snapshot returned ----
{
  const nullSnap = normalizeCodeReviewSnapshot(null);
  check('null -> empty with error', !!nullSnap.error && nullSnap.files.length === 0);
  const strSnap = normalizeCodeReviewSnapshot('nope');
  check('string -> empty with error', !!strSnap.error);
  const arrSnap = normalizeCodeReviewSnapshot([]);
  check('array -> empty with error', !!arrSnap.error);
  check('empty helper defaults', emptyCodeReviewSnapshot().snapshotId === '');
  check('empty helper with error', emptyCodeReviewSnapshot('x').error === 'x');
}

// ---- happy path: full snapshot coerces and preserves ----
{
  const wire = {
    comparisonLabel: 'HEAD → working tree',
    snapshotId: 'snap-1',
    truncated: true,
    error: null,
    totalInsertions: 3,
    totalDeletions: 1,
    files: [
      {
        path: 'src/a.ts',
        status: 'modified',
        binary: false,
        insertions: 2,
        deletions: 1,
        truncated: false,
        hunks: [
          {
            header: '@@ -1,3 +1,4 @@',
            oldStart: 1,
            oldCount: 3,
            newStart: 1,
            newCount: 4,
            contentHash: 'h1',
            lines: [
              { kind: 'context', oldNo: 1, newNo: 1, text: ' keep' },
              { kind: 'deletion', oldNo: 2, text: '-old' },
              { kind: 'addition', newNo: 2, text: '+new' },
              { kind: 'meta', text: '\\ No newline' },
            ],
          },
        ],
      },
      {
        path: 'bin.dat',
        status: 'binary',
        binary: true,
        insertions: 0,
        deletions: 0,
        truncated: false,
        previousPath: 'old-bin.dat',
        message: 'binary change',
        hunks: [],
      },
    ],
    threads: [
      {
        identity: {
          snapshotId: 'snap-1',
          path: 'src/a.ts',
          oldStart: 1,
          oldCount: 3,
          newStart: 1,
          newCount: 4,
          contentHash: 'h1',
        },
        comments: [
          { role: 'user', text: 'looks off', partial: false },
          { role: 'assistant', text: 'checking', partial: false, model: 'gpt-test' },
        ],
        streamingText: 'partial…',
        error: null,
        stale: false,
        isStreaming: true,
        model: 'gpt-test',
      },
    ],
    isStreaming: true,
    activeCount: 1,
  };
  const snap = normalizeCodeReviewSnapshot(wire);
  check('label preserved', snap.comparisonLabel === 'HEAD → working tree');
  check('snapshotId preserved', snap.snapshotId === 'snap-1');
  check('truncated preserved', snap.truncated === true);
  check('error null preserved', snap.error === null);
  check('totals', snap.totalInsertions === 3 && snap.totalDeletions === 1);
  check('isStreaming', snap.isStreaming === true);
  check('activeCount', snap.activeCount === 1);
  check('files length', snap.files.length === 2);
  check('file path', snap.files[0].path === 'src/a.ts');
  check('file status', snap.files[0].status === 'modified');
  check('previousPath kept', snap.files[1].previousPath === 'old-bin.dat');
  check('message kept', snap.files[1].message === 'binary change');
  check('binary flag', snap.files[1].binary === true);
  check('hunk count', snap.files[0].hunks.length === 1);
  check('hunk hash', snap.files[0].hunks[0].contentHash === 'h1');
  check('line kinds', snap.files[0].hunks[0].lines.map((l) => l.kind).join(',') === 'context,deletion,addition,meta');
  check('line text', snap.files[0].hunks[0].lines[2].text === '+new');
  check('threads length', snap.threads.length === 1);
  check('thread comment', snap.threads[0].comments[0].text === 'looks off');
  check('comment model', snap.threads[0].comments[1].model === 'gpt-test');
  check('thread model', snap.threads[0].model === 'gpt-test');
  check('thread isStreaming', snap.threads[0].isStreaming === true);
  check('streamingText', snap.threads[0].streamingText === 'partial…');
  check('no activeHunk field', !('activeHunk' in snap));
}

// ---- defensive coercion: malformed nested entries dropped, scalars coerced ----
{
  const wire = {
    comparisonLabel: 42,
    snapshotId: null,
    truncated: 'yes',
    error: 7,
    totalInsertions: -5,
    totalDeletions: 'x',
    files: [
      null,
      'bad',
      { path: '', status: 'modified' }, // empty path dropped
      {
        path: 'ok.ts',
        status: 'not-a-status',
        binary: 'true',
        insertions: 1.9,
        deletions: -2,
        truncated: 1,
        hunks: [
          null,
          { header: 'h', oldStart: 1, oldCount: 1, newStart: 1, newCount: 1 }, // no contentHash
          {
            header: '@@',
            oldStart: 1,
            oldCount: 1,
            newStart: 1,
            newCount: 1,
            contentHash: 'hh',
            lines: [null, { kind: 'weird', text: 9 }, { kind: 'addition', text: 'ok' }],
          },
        ],
      },
    ],
    threads: null,
    isStreaming: 'no',
    activeCount: -3,
  };
  const snap = normalizeCodeReviewSnapshot(wire);
  check('non-string label -> empty', snap.comparisonLabel === '');
  check('non-string snapshotId -> empty', snap.snapshotId === '');
  check('non-bool truncated -> false', snap.truncated === false);
  check('non-string error -> null', snap.error === null);
  check('negative insertions -> 0', snap.totalInsertions === 0);
  check('non-number deletions -> 0', snap.totalDeletions === 0);
  check('only valid file kept', snap.files.length === 1 && snap.files[0].path === 'ok.ts');
  check('unknown status -> changed', snap.files[0].status === 'changed');
  check('non-bool binary -> false', snap.files[0].binary === false);
  check('float insertions floored', snap.files[0].insertions === 1);
  check('negative deletions -> 0', snap.files[0].deletions === 0);
  check('only hashed hunk kept', snap.files[0].hunks.length === 1);
  check('unknown line kind -> meta', snap.files[0].hunks[0].lines[0].kind === 'meta');
  check('non-string line text coerced', snap.files[0].hunks[0].lines[0].text === '');
  check('valid line kept', snap.files[0].hunks[0].lines[1].text === 'ok');
  check('null threads -> []', snap.threads.length === 0);
  check('non-bool isStreaming -> false', snap.isStreaming === false);
  check('negative activeCount -> 0', snap.activeCount === 0);
}

// ---- threads map form (defensive normalize) ----
{
  const mapThreads = normalizeThreads({
    t1: {
      identity: {
        snapshotId: 's',
        path: 'a.ts',
        oldStart: 1,
        oldCount: 1,
        newStart: 1,
        newCount: 1,
        contentHash: 'c',
      },
      comments: [{ role: 'assistant', text: 'hi', partial: true, model: 'm1' }],
      streamingText: '',
      error: 'boom',
      stale: true,
      isStreaming: false,
      model: 'm1',
    },
    bad: null,
    incomplete: { identity: { path: 'x' } },
  });
  check('map threads length', mapThreads.length === 1);
  check('map thread role', mapThreads[0].comments[0].role === 'assistant');
  check('map thread comment model', mapThreads[0].comments[0].model === 'm1');
  check('map thread model', mapThreads[0].model === 'm1');
  check('map thread error', mapThreads[0].error === 'boom');
  check('map thread stale', mapThreads[0].stale === true);
  check('map thread isStreaming false', mapThreads[0].isStreaming === false);

  const arrThreads = normalizeThreads([
    {
      identity: {
        snapshotId: 's',
        path: 'a.ts',
        oldStart: 1,
        oldCount: 1,
        newStart: 1,
        newCount: 1,
        contentHash: 'c',
      },
      comments: [],
      streamingText: '…',
      // omit isStreaming: derive from streamingText
      error: null,
      stale: false,
    },
  ]);
  check('array threads length', arrThreads.length === 1);
  check('derived isStreaming from text', arrThreads[0].isStreaming === true);
  check('non-collection threads -> []', normalizeThreads(undefined).length === 0);
}

// ---- hunkKey / findThreadForHunk ----
{
  const id = {
    snapshotId: 's',
    path: 'a.ts',
    oldStart: 1,
    oldCount: 2,
    newStart: 3,
    newCount: 4,
    contentHash: 'hash',
  };
  const threads = [
    {
      identity: { ...id },
      comments: [],
      streamingText: '',
      error: null,
      stale: false,
    },
  ];
  check('hunkKey stable', hunkKey(id) === ['a.ts', '1,2,3,4', 'hash'].join('\0'));
  check('findThread hits', !!findThreadForHunk(threads, id));
  check(
    'findThread misses on hash',
    !findThreadForHunk(threads, { ...id, contentHash: 'other' }),
  );
}

// ---- parseCodeReviewArgs arity ----
{
  check('empty args ok', parseCodeReviewArgs('').ok === true);
  check('whitespace args ok', parseCodeReviewArgs('   ').ok === true);
  const two = parseCodeReviewArgs('main feature');
  check('two revs ok', two.ok === true && two.ok && two.from === 'main' && two.to === 'feature');
  const one = parseCodeReviewArgs('main');
  check('one rev error', one.ok === false);
  const three = parseCodeReviewArgs('a b c');
  check('three revs error', three.ok === false);
  const spaced = parseCodeReviewArgs('  abc  def  ');
  check(
    'spaced two revs',
    spaced.ok === true && spaced.ok && spaced.from === 'abc' && spaced.to === 'def',
  );
}

// ---- hunkKeyFor / hunkIdentityFor / findThreadForHunk pairing ----
{
  const file = {
    path: 'src/a.ts',
    hunks: [
      {
        header: '@@ -1,3 +1,4 @@',
        oldStart: 1,
        oldCount: 3,
        newStart: 1,
        newCount: 4,
        contentHash: 'h1',
        lines: [],
      },
      {
        header: '@@ -9,2 +10,2 @@',
        oldStart: 9,
        oldCount: 2,
        newStart: 10,
        newCount: 2,
        contentHash: 'h2',
        lines: [],
      },
    ],
  };
  const hunk = file.hunks[0];
  check('hunkKeyFor matches hunkKey of identity', hunkKeyFor(file, hunk) === hunkKey(hunkIdentityFor('snap', file, hunk)));
  check('hunkKeyFor distinct per hunk', hunkKeyFor(file, file.hunks[0]) !== hunkKeyFor(file, file.hunks[1]));
  check('hunkIdentityFor carries snapshotId', hunkIdentityFor('snap-9', file, hunk).snapshotId === 'snap-9');
  check('hunkIdentityFor carries ranges', hunkIdentityFor('s', file, hunk).oldStart === 1 && hunkIdentityFor('s', file, hunk).newCount === 4);

  const id = hunkIdentityFor('snap', file, hunk);
  const threads = [
    {
      identity: id,
      comments: [{ role: 'user', text: 'hi', partial: false }],
      streamingText: '',
      error: null,
      stale: false,
    },
  ];
  check('hunkIdentityFor resolves its thread', !!findThreadForHunk(threads, id));
}

// ---- countThreadComments / countFileThreads badges ----
{
  const file = {
    path: 'src/a.ts',
    hunks: [
      { header: '@@', oldStart: 1, oldCount: 1, newStart: 1, newCount: 1, contentHash: 'h1', lines: [] },
      { header: '@@', oldStart: 5, oldCount: 1, newStart: 5, newCount: 1, contentHash: 'h2', lines: [] },
      { header: '@@', oldStart: 9, oldCount: 1, newStart: 9, newCount: 1, contentHash: 'h3', lines: [] },
    ],
  };
  check('no thread -> 0 comments', countThreadComments(undefined) === 0);
  const oneComment = {
    identity: hunkIdentityFor('s', file, file.hunks[0]),
    comments: [{ role: 'user', text: 'a', partial: false }],
    streamingText: '',
    error: null,
    stale: false,
  };
  check('thread count', countThreadComments(oneComment) === 1);
  const twoOnSecond = {
    ...oneComment,
    identity: hunkIdentityFor('s', file, file.hunks[1]),
    comments: [
      { role: 'user', text: 'a', partial: false },
      { role: 'assistant', text: 'b', partial: true },
    ],
  };
  const threads = [oneComment, twoOnSecond];
  check('file badge sums across hunks', countFileThreads(threads, file) === 3);
  check('file with no threads -> 0', countFileThreads([], file) === 0);
}

// ---- isDiffPlaceholder / placeholder LoadedDiff ----
{
  const placeholder = {
    path: 'big3.txt',
    status: 'modified',
    binary: false,
    insertions: 0,
    deletions: 0,
    truncated: true,
    hunks: [],
  };
  check('placeholder detected', isDiffPlaceholder(placeholder) === true);
  const partial = {
    ...placeholder,
    hunks: [{ header: '@@', oldStart: 1, oldCount: 1, newStart: 1, newCount: 1, contentHash: 'h', lines: [] }],
  };
  check('partial file is NOT a placeholder', isDiffPlaceholder(partial) === false);
  check('untruncated empty file is NOT a placeholder', isDiffPlaceholder({ ...placeholder, truncated: false }) === false);
  check('binary empty file is NOT a placeholder', isDiffPlaceholder({ ...placeholder, binary: true }) === false);

  const loaded = initLoadedDiff('snap-1', placeholder, true);
  check('placeholder loaded diff starts empty', loaded.lines.length === 0 && loaded.hunks.length === 0);
  check('placeholder eager fetch starts at cursor 0', loaded.nextCursor === 0);
  check('placeholder paging enabled', loaded.hasMoreBackend === true);
  check('placeholder not complete until a page arrives', loaded.hunksComplete === false);
  check('placeholder not byte-capped initially', loaded.byteCapped === false);
  const noPaging = initLoadedDiff('snap-1', placeholder, false);
  check('placeholder without global truncation does not page', noPaging.hasMoreBackend === false);
  check('binary files never fetch', initLoadedDiff('snap-1', { ...placeholder, binary: true }, true).hunksComplete === true);
}

// ---- hunk completeness checks (frontend mirror of the backend) ----
{
  const completeHunk = {
    header: '@@ -1,3 +1,3 @@',
    oldStart: 1,
    oldCount: 2,
    newStart: 1,
    newCount: 2,
    contentHash: 'h',
    lines: [
      { kind: 'context', oldNo: 1, newNo: 1, text: ' a' },
      { kind: 'deletion', oldNo: 2, text: '-b' },
      { kind: 'addition', newNo: 2, text: '+c' },
    ],
  };
  check('complete hunk passes content check', hunkIsComplete(completeHunk) === true);
  const cutHunk = {
    ...completeHunk,
    lines: completeHunk.lines.slice(0, 2),
  };
  check('cut hunk fails content check', hunkIsComplete(cutHunk) === false);
  const loadedHunk = {
    index: 0,
    ...completeHunk,
    totalLines: 3,
    complete: true,
    lines: completeHunk.lines,
  };
  check('fully merged hunk is ready', loadedHunkReady(loadedHunk) === true);
  check('page-split hunk not ready', loadedHunkReady({ ...loadedHunk, lines: completeHunk.lines.slice(0, 1) }) === false);
  check('byte-capped hunk never ready', loadedHunkReady({ ...loadedHunk, complete: false }) === false);
}

// ---- planDiffWindow for an empty placeholder (zero snapshot lines) ----
{
  const loaded = {
    snapshotId: 's',
    path: 'p.rs',
    hunks: [],
    lines: [],
    hunkCount: 0,
    nextCursor: 0,
    hasMoreBackend: true,
    totalLines: 0,
    backendTruncated: true,
    byteCapped: false,
    hunksComplete: false,
    loading: false,
    error: null,
  };
  const plan = planDiffWindow(loaded, 0);
  check('placeholder plan needs backend fetch', plan.needsFetch === true && plan.canLoadMore === true);
  check('placeholder plan targets the soft cap', plan.target === 4000);
  check('placeholder plan not hard-capped', plan.hardCapped === false && plan.totalAvailable === 20000);
}

// ---- status glyphs: TUI parity (A/D/M/R/C/B/?) ----
// Mirrors crates/pi-cli/src/code_review.rs::FileStatus::as_str, which the
// TUI file tree renders as one colored compact letter per file.
{
  const expected = {
    added: 'A',
    deleted: 'D',
    modified: 'M',
    renamed: 'R',
    copied: 'C',
    binary: 'B',
    changed: '?',
  };
  for (const [status, glyph] of Object.entries(expected)) {
    check(`letter ${status} -> ${glyph}`, fileStatusLetter(status) === glyph);
    check(`letter map ${status}`, FILE_STATUS_LETTERS[status] === glyph);
  }
  check('letter covers every FileStatus', Object.keys(FILE_STATUS_LETTERS).length === 7);
  check('letter fallback unknown', fileStatusLetter('unknown') === '?');
}

// ---- collapsible path tree ----
{
  const files = [
    { path: 'src/a.ts', status: 'modified', binary: false, insertions: 2, deletions: 1, truncated: false, hunks: [] },
    { path: 'src/deep/b.rs', status: 'added', binary: false, insertions: 3, deletions: 0, truncated: false, hunks: [] },
    { path: 'top.rs', status: 'modified', binary: false, insertions: 1, deletions: 1, truncated: false, hunks: [] },
  ];
  const tree = buildFileTree(files);
  check('tree nodes built', tree.nodes.length === 5);
  const byPath = (p) => tree.nodes.find((n) => n.path === p);
  const src = byPath('src');
  const deep = byPath('src/deep');
  const a = byPath('src/a.ts');
  const b = byPath('src/deep/b.rs');
  const top = byPath('top.rs');
  const srcIdx = tree.nodes.indexOf(src);
  const deepIdx = tree.nodes.indexOf(deep);
  const aIdx = tree.nodes.indexOf(a);
  const bIdx = tree.nodes.indexOf(b);
  const topIdx = tree.nodes.indexOf(top);
  check('roots dirs-before-files', tree.roots.join(',') === `${srcIdx},${topIdx}`);
  check('src children dirs-before-files', src.children.join(',') === `${deepIdx},${aIdx}`);
  check('deep child file', deep.children.join(',') === String(bIdx));
  check('file depth', a.depth === 1 && b.depth === 2 && top.depth === 0);
  check('file fileIndex maps snapshot order', a.fileIndex === 0 && b.fileIndex === 1 && top.fileIndex === 2);
  check('dir aggregates insertions', src.insertions === 5 && src.deletions === 1);
  check('nested dir aggregates', deep.insertions === 3 && deep.deletions === 0);
  check('default expanded', treeVisibleRows(tree).length === 5);
  // Every tree level shows its BASENAME only; the hierarchy carries the
  // directories while the node's full repo-relative path stays on the node.
  check('file node name is basename', a.name === 'a.ts' && b.name === 'b.rs' && top.name === 'top.rs');
  check('dir node name is basename', src.name === 'src' && deep.name === 'deep');
  check('file node keeps full path', a.path === 'src/a.ts' && b.path === 'src/deep/b.rs');
  check('nested basename differs from full path', b.name !== b.path && top.name === top.path);
  check('nested file carries its status', b.status === 'added' && a.status === 'modified');

  // Collapse hides children; toggle re-expands.
  const collapsedOnce = treeToggleCollapse(tree, srcIdx);
  check('collapse hides subtree', treeVisibleRows(collapsedOnce).length === 2);
  check('collapsed root reported', collapsedOnce.collapsed.has(src.id) === true);
  const reExpanded = treeToggleCollapse(collapsedOnce, srcIdx);
  check('toggle re-expands', treeVisibleRows(reExpanded).length === 5);
  const collapsedDeep = treeToggleCollapse(tree, deepIdx);
  const deepRows = treeVisibleRows(collapsedDeep);
  check('setCollapsed hides nested subtree', deepRows.length === 4 && !deepRows.some((r) => r.nodeIndex === bIdx));
  check('collapse does not affect other branches', deepRows.some((r) => r.nodeIndex === topIdx));

  // Filter keeps matched files + ancestors, forced expanded.
  const filtered = treeFilterRows(tree, 'deep');
  const filteredPaths = filtered.map((r) => tree.nodes[r.nodeIndex].path);
  check('filter keeps file + ancestors', filteredPaths.join(',') === 'src,src/deep,src/deep/b.rs');
  check('filter forces expansion', filtered.filter((r) => r.isDir).every((r) => r.expanded === true));
  check('filter empty query -> full rows', treeFilterRows(tree, '').length === 5);
  check('filter whitespace -> full rows', treeFilterRows(tree, '   ').length === 5);
  check('filter no match -> empty', treeFilterRows(tree, 'zzz').length === 0);
  const caseFilter = treeFilterRows(tree, 'B.RS');
  check('filter case-insensitive', caseFilter.length === 3);

  // Keyboard navigation over visible rows.
  // rows (depth-first): src(dir), src/deep(dir), src/deep/b.rs(file),
  // src/a.ts(file), top.rs(file).
  const rows = treeVisibleRows(tree);
  const down = treeKeyboardAction(rows, tree, 0, 'ArrowDown');
  check('ArrowDown moves next', down.kind === 'move' && down.nextIndex === 1);
  const up = treeKeyboardAction(rows, tree, 2, 'ArrowUp');
  check('ArrowUp moves prev', up.kind === 'move' && up.nextIndex === 1);
  const home = treeKeyboardAction(rows, tree, 3, 'Home');
  check('Home jumps to first', home.kind === 'move' && home.nextIndex === 0);
  const end = treeKeyboardAction(rows, tree, 0, 'End');
  check('End jumps to last', end.kind === 'move' && end.nextIndex === 4);
  const enterDir = treeKeyboardAction(rows, tree, 0, 'Enter');
  check('Enter on dir toggles', enterDir.kind === 'toggle' && enterDir.nodeIndex === srcIdx);
  const spaceDir = treeKeyboardAction(rows, tree, 0, ' ');
  check('Space on dir toggles', spaceDir.kind === 'toggle');
  const enterFile = treeKeyboardAction(rows, tree, 2, 'Enter');
  check('Enter on file selects', enterFile.kind === 'select' && enterFile.nodeIndex === bIdx);
  const collapsedRows = treeVisibleRows(collapsedOnce);
  const rightOnCollapsed = treeKeyboardAction(collapsedRows, collapsedOnce, 0, 'ArrowRight');
  check('ArrowRight expands collapsed dir', rightOnCollapsed.kind === 'toggle' && rightOnCollapsed.nodeIndex === srcIdx);
  const rightOnExpanded = treeKeyboardAction(rows, tree, 0, 'ArrowRight');
  check('ArrowRight moves into expanded dir', rightOnExpanded.kind === 'move' && rightOnExpanded.nextIndex === 1);
  const leftOnExpanded = treeKeyboardAction(rows, tree, 0, 'ArrowLeft');
  check('ArrowLeft collapses expanded dir', leftOnExpanded.kind === 'toggle' && leftOnExpanded.nodeIndex === srcIdx);
  const leftOnFile = treeKeyboardAction(rows, tree, 2, 'ArrowLeft');
  check('ArrowLeft on file moves to parent', leftOnFile.kind === 'move' && leftOnFile.nextIndex === 1);
  const unknownKey = treeKeyboardAction(rows, tree, 0, 'x');
  check('unknown key -> none', unknownKey.kind === 'none');
  check('empty rows -> none', treeKeyboardAction([], tree, 0, 'ArrowDown').kind === 'none');

  // Row <-> file index mapping.
  check('file index at row', treeFileIndexAt(rows, tree, 3) === 0);
  check('file index at nested row', treeFileIndexAt(rows, tree, 2) === 1);
  check('dir row has no file index', treeFileIndexAt(rows, tree, 0) === -1);
  check('out-of-range row -> -1', treeFileIndexAt(rows, tree, 99) === -1);
}

// ---- normalizeFileDiffPage (hunk-window wire shape) ----
{
  const valid = normalizeFileDiffPage({
    snapshotId: 'snap-1',
    path: 'a.rs',
    binary: false,
    status: 'modified',
    hunks: [
      {
        index: 0,
        header: '@@ -1,2 +1,2 @@',
        oldStart: 1,
        oldCount: 1,
        newStart: 1,
        newCount: 1,
        contentHash: 'cafebabe',
        totalLines: 3,
        complete: true,
        lineStart: 0,
        lines: [
          { kind: 'addition', text: '+x', newNo: 1 },
          { kind: 'deletion', text: '-y', oldNo: 2 },
          { kind: 'context', text: ' z', oldNo: 3, newNo: 3 },
        ],
      },
    ],
    cursor: 0,
    nextCursor: 3,
    hasMore: true,
    totalLines: 200,
    hunkCount: 2,
    truncated: false,
  });
  check('page normalized', !!valid);
  check('page fields', valid.snapshotId === 'snap-1' && valid.path === 'a.rs' && valid.cursor === 0);
  check('page nextCursor', valid.nextCursor === 3);
  check('page hasMore', valid.hasMore === true && valid.totalLines === 200);
  check('page hunkCount', valid.hunkCount === 2);
  check('page hunk descriptor', valid.hunks.length === 1 && valid.hunks[0].index === 0 && valid.hunks[0].contentHash === 'cafebabe');
  check('page hunk complete identity', valid.hunks[0].oldStart === 1 && valid.hunks[0].oldCount === 1 && valid.hunks[0].newStart === 1 && valid.hunks[0].newCount === 1);
  check('page hunk window', valid.hunks[0].totalLines === 3 && valid.hunks[0].complete === true && valid.hunks[0].lineStart === 0);
  check('page hunk lines kinds', valid.hunks[0].lines.map((l) => l.kind).join(',') === 'addition,deletion,context');
  check('page line numbers', valid.hunks[0].lines[0].newNo === 1 && valid.hunks[0].lines[1].oldNo === 2);
  check('page no nextCursor when absent', normalizeFileDiffPage({ snapshotId: 's', path: 'a', hunks: [], cursor: 0, hasMore: false, totalLines: 0, hunkCount: 0, truncated: false }).nextCursor === undefined);
  check('page missing path -> null', normalizeFileDiffPage({ snapshotId: 's', hunks: [] }) === null);
  check('page missing snapshotId -> null', normalizeFileDiffPage({ path: 'a', hunks: [] }) === null);
  check('page null -> null', normalizeFileDiffPage(null) === null);
  check('page garbage -> null', normalizeFileDiffPage('nope') === null);
  const badStatus = normalizeFileDiffPage({ snapshotId: 's', path: 'a', hunks: [], cursor: 0, hasMore: false, totalLines: 0, hunkCount: 0, truncated: false, status: 'weird' });
  check('page unknown status -> changed', badStatus.status === 'changed');
  const badHunk = normalizeFileDiffPage({ snapshotId: 's', path: 'a', hunks: [{ index: 0, lines: [{ kind: 'bogus', text: 9 }, null] }, null], cursor: 0, hasMore: false, totalLines: 0, hunkCount: 1, truncated: false });
  check('page malformed hunks coerced/dropped', badHunk.hunks.length === 0);
}

// ---- initLoadedDiff / appendFileDiffPage (structured hunk merge) ----
{
  const lines = [
    { kind: 'deletion', oldNo: 1, text: '-a' },
    { kind: 'addition', newNo: 1, text: '+b' },
    { kind: 'context', oldNo: 2, newNo: 2, text: ' c' },
  ];
  const file = {
    path: 'big.rs',
    status: 'modified',
    binary: false,
    insertions: 1,
    deletions: 1,
    truncated: true,
    hunks: [{ header: '@@ -1,2 +1,2 @@', oldStart: 1, oldCount: 2, newStart: 1, newCount: 2, contentHash: 'h1', lines }],
  };
  const loaded = initLoadedDiff('snap-1', file, true);
  check('loaded snapshot lines', loaded.lines.length === 3 && loaded.hunks.length === 1);
  check('loaded seed hunk structured', loaded.hunks[0].index === 0 && loaded.hunks[0].header === '@@ -1,2 +1,2 @@');
  check('loaded seed hunk identity', loaded.hunks[0].contentHash === 'h1' && loaded.hunks[0].oldCount === 2);
  check('loaded seed hunk complete', loaded.hunks[0].complete === true && loaded.hunks[0].totalLines === 3);
  check('loaded eager fetch starts at cursor 0', loaded.nextCursor === 0);
  check('loaded backend paging enabled', loaded.hasMoreBackend === true);
  check('loaded truncated state', loaded.backendTruncated === true && loaded.totalLines === 3);
  check('loaded not complete until stream consumed', loaded.hunksComplete === false);
  const noPaging = initLoadedDiff('snap-1', file, false);
  check('loaded paging off when snapshot not truncated', noPaging.hasMoreBackend === false);
  const smallFile = initLoadedDiff('snap-1', { ...file, truncated: false }, true);
  check('loaded paging off when file not truncated', smallFile.hasMoreBackend === false);

  const descriptor = (over) => ({
    index: 0,
    header: '@@ -1,2 +1,2 @@',
    oldStart: 1,
    oldCount: 2,
    newStart: 1,
    newCount: 2,
    contentHash: 'h1',
    totalLines: 5,
    complete: true,
    lineStart: 0,
    lines: [
      { kind: 'deletion', oldNo: 1, text: '-a' },
      { kind: 'addition', newNo: 1, text: '+b' },
      { kind: 'context', oldNo: 2, newNo: 2, text: ' c' },
      { kind: 'addition', newNo: 3, text: '+d' },
      { kind: 'addition', newNo: 4, text: '+e' },
    ],
    ...over,
  });
  // First page starts at cursor 0 (the eager fetch is authoritative from the
  // start): the subset overlaps the seed byte-identically and dedupes.
  const page1 = {
    snapshotId: 'snap-1',
    path: 'big.rs',
    binary: false,
    status: 'modified',
    hunks: [descriptor({ lineStart: 0, lines: descriptor().lines.slice(0, 3) })],
    cursor: 0,
    nextCursor: 3,
    hasMore: true,
    totalLines: 5,
    hunkCount: 1,
    truncated: false,
  };
  const merged1 = appendFileDiffPage(loaded, page1);
  check('seed dedupes the overlapping first page', merged1.lines.length === 3 && merged1.lines[2].text === ' c');
  check('merged hunk upgraded to backend identity', merged1.hunks[0].totalLines === 5 && merged1.hunks[0].complete === true);
  check('partial merge not ready yet', loadedHunkReady(merged1.hunks[0]) === false);
  check('page cursor advances', merged1.nextCursor === 3);
  check('page total updated', merged1.totalLines === 5);
  check('page hasMore carried', merged1.hasMoreBackend === true);
  check('page not complete mid-stream', merged1.hunksComplete === false);
  check('page clears error/loading', merged1.loading === false && merged1.error === null);
  // Second page continues at hunk-local 3 and completes the stream.
  const page2 = {
    ...page1,
    hunks: [descriptor({ lineStart: 3, lines: descriptor().lines.slice(3) })],
    cursor: 3,
    nextCursor: undefined,
    hasMore: false,
  };
  const merged = appendFileDiffPage(merged1, page2);
  check('page continuation appended in order', merged.lines.length === 5 && merged.lines[3].text === '+d' && merged.lines[4].text === '+e');
  check('merged hunk becomes ready', loadedHunkReady(merged.hunks[0]) === true);
  check('final page completes the stream', merged.hasMoreBackend === false && merged.hunksComplete === true && merged.nextCursor === 5);
  const dupPage = appendFileDiffPage(merged, page2);
  check('duplicate cursor page dropped', dupPage === merged && dupPage.lines.length === 5);
  const staleSnap = appendFileDiffPage(loaded, { ...page1, snapshotId: 'other-snap' });
  check('stale snapshot page dropped', staleSnap === loaded);
  const wrongPath = appendFileDiffPage(loaded, { ...page1, path: 'other.rs' });
  check('wrong path page dropped', wrongPath === loaded);
  const wrongCursor = appendFileDiffPage(loaded, { ...page1, cursor: 99 });
  check('wrong cursor page dropped', wrongCursor === loaded);
}

// ---- contract: globally truncated placeholder yields complete loaded hunk identities ----
{
  // A placeholder file the 2 MiB snapshot never carried: zero snapshot hunks.
  const placeholder = {
    path: 'zz-later.txt',
    status: 'modified',
    binary: false,
    insertions: 0,
    deletions: 0,
    truncated: true,
    hunks: [],
  };
  const loaded = initLoadedDiff('snap-1', placeholder, true);
  check('placeholder seeds no hunks', loaded.hunks.length === 0 && loaded.lines.length === 0);

  // The first backend page carries the complete hunk descriptor + lines.
  const page = {
    snapshotId: 'snap-1',
    path: 'zz-later.txt',
    binary: false,
    status: 'modified',
    hunks: [
      {
        index: 0,
        header: '@@ -1 +1 @@',
        oldStart: 1,
        oldCount: 1,
        newStart: 1,
        newCount: 1,
        contentHash: 'abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789',
        totalLines: 2,
        complete: true,
        lineStart: 0,
        lines: [
          { kind: 'deletion', oldNo: 1, text: '-later base' },
          { kind: 'addition', newNo: 1, text: '+later changed' },
        ],
      },
    ],
    cursor: 0,
    nextCursor: undefined,
    hasMore: false,
    totalLines: 2,
    hunkCount: 1,
    truncated: false,
  };
  const merged = appendFileDiffPage(loaded, page);
  check('placeholder hunk merged', merged.hunks.length === 1 && merged.lines.length === 2);
  check('placeholder hunk is a real hunk with header/ranges/hash', merged.hunks[0].header === '@@ -1 +1 @@' && merged.hunks[0].oldStart === 1 && merged.hunks[0].contentHash === page.hunks[0].contentHash);
  check('placeholder hunk complete and ready', merged.hunks[0].complete === true && loadedHunkReady(merged.hunks[0]) === true);
  check('placeholder stream complete', merged.hunksComplete === true && merged.hasMoreBackend === false);
}

// ---- contract: partial last snapshot hunk is replaced by the per-file hunk ----
{
  // A file partially cut by the 2 MiB global patch: the snapshot carries its
  // last hunk with a WRONG identity (partial lines -> different content hash).
  const seedLines = [
    { kind: 'context', oldNo: 1, newNo: 1, text: ' a' },
    { kind: 'deletion', oldNo: 2, text: '-b' },
  ];
  const file = {
    path: 'cut.rs',
    status: 'modified',
    binary: false,
    insertions: 0,
    deletions: 1,
    truncated: true,
    hunks: [
      { header: '@@ -1,3 +1,3 @@', oldStart: 1, oldCount: 3, newStart: 1, newCount: 3, contentHash: 'partial-hash', lines: seedLines },
    ],
  };
  const loaded = initLoadedDiff('snap-1', file, true);
  check('partial seed hunk detected incomplete', loaded.hunks[0].complete === false);
  check('partial seed hunk not selectable', loadedHunkReady(loaded.hunks[0]) === false);

  // The per-file page carries the COMPLETE hunk (different hash, all lines).
  const page = {
    snapshotId: 'snap-1',
    path: 'cut.rs',
    binary: false,
    status: 'modified',
    hunks: [
      {
        index: 0,
        header: '@@ -1,3 +1,3 @@',
        oldStart: 1,
        oldCount: 3,
        newStart: 1,
        newCount: 3,
        contentHash: 'complete-hash',
        totalLines: 5,
        complete: true,
        lineStart: 0,
        lines: [
          { kind: 'context', oldNo: 1, newNo: 1, text: ' a' },
          { kind: 'deletion', oldNo: 2, text: '-b' },
          { kind: 'deletion', oldNo: 3, text: '-b2' },
          { kind: 'addition', newNo: 2, text: '+c' },
          { kind: 'addition', newNo: 3, text: '+c2' },
        ],
      },
    ],
    cursor: 0,
    nextCursor: undefined,
    hasMore: false,
    totalLines: 5,
    hunkCount: 1,
    truncated: false,
  };
  const merged = appendFileDiffPage(loaded, page);
  check('partial hunk replaced by per-file hunk', merged.hunks[0].contentHash === 'complete-hash' && merged.hunks[0].lines.length === 5);
  check('replacement hunk complete and ready', merged.hunks[0].complete === true && loadedHunkReady(merged.hunks[0]) === true);
  check('replacement produced no duplicate lines', merged.lines.length === 5);
}

// ---- contract: page-split hunk merges with no duplication ----
{
  // One 8200-line hunk delivered in cursor-0 pages while the snapshot seed
  // already carried the full body: the seed dedupes byte-identically.
  const makeLines = (n, offset = 0) =>
    Array.from({ length: n }, (_, i) => ({ kind: 'addition', newNo: offset + i + 1, text: `+l${offset + i}` }));
  const bigHunk = {
    header: '@@ -0,0 +1,8200 @@',
    oldStart: 0,
    oldCount: 0,
    newStart: 1,
    newCount: 8200,
    contentHash: 'big-hash',
    lines: makeLines(8200),
  };
  const loaded = initLoadedDiff('snap-1', {
    path: 'big.txt',
    status: 'modified',
    binary: false,
    insertions: 8200,
    deletions: 0,
    truncated: true,
    hunks: [bigHunk],
  }, true);
  check('seeded big hunk ready immediately', loadedHunkReady(loaded.hunks[0]) === true);
  // Page 1 (cursor 0) covers lines 0..1000 — fully inside the seed.
  const p1 = appendFileDiffPage(loaded, {
    snapshotId: 'snap-1',
    path: 'big.txt',
    binary: false,
    status: 'modified',
    hunks: [{ index: 0, header: bigHunk.header, oldStart: 0, oldCount: 0, newStart: 1, newCount: 8200, contentHash: 'big-hash', totalLines: 8200, complete: true, lineStart: 0, lines: makeLines(1000) }],
    cursor: 0,
    nextCursor: 1000,
    hasMore: true,
    totalLines: 8200,
    hunkCount: 1,
    truncated: false,
  });
  check('seed dedupes page-1 subset', p1.lines.length === 8200 && p1.lines[999].text === '+l999');
  // Page 2 covers 1000..2000 — also inside the seed.
  const p2 = appendFileDiffPage(p1, {
    snapshotId: 'snap-1',
    path: 'big.txt',
    binary: false,
    status: 'modified',
    hunks: [{ index: 0, header: bigHunk.header, oldStart: 0, oldCount: 0, newStart: 1, newCount: 8200, contentHash: 'big-hash', totalLines: 8200, complete: true, lineStart: 1000, lines: makeLines(1000, 1000) }],
    cursor: 1000,
    nextCursor: 2000,
    hasMore: true,
    totalLines: 8200,
    hunkCount: 1,
    truncated: false,
  });
  check('seed dedupes page-2 subset', p2.lines.length === 8200);
  // A placeholder file (no seed) with the same hunk split across pages:
  // page 1 covers 0..1000, page 2 continues 1000..2000 — pure appends.
  const empty = initLoadedDiff('snap-1', { ...bigHunk, path: 'huge.txt', hunks: [], truncated: true }, true);
  const hp1 = appendFileDiffPage(empty, {
    snapshotId: 'snap-1',
    path: 'huge.txt',
    binary: false,
    status: 'modified',
    hunks: [{ index: 0, header: bigHunk.header, oldStart: 0, oldCount: 0, newStart: 1, newCount: 8200, contentHash: 'big-hash', totalLines: 8200, complete: true, lineStart: 0, lines: makeLines(1000) }],
    cursor: 0,
    nextCursor: 1000,
    hasMore: true,
    totalLines: 8200,
    hunkCount: 1,
    truncated: false,
  });
  check('split hunk page 1 appends', hp1.lines.length === 1000 && loadedHunkReady(hp1.hunks[0]) === false);
  const hp2 = appendFileDiffPage(hp1, {
    snapshotId: 'snap-1',
    path: 'huge.txt',
    binary: false,
    status: 'modified',
    hunks: [{ index: 0, header: bigHunk.header, oldStart: 0, oldCount: 0, newStart: 1, newCount: 8200, contentHash: 'big-hash', totalLines: 8200, complete: true, lineStart: 1000, lines: makeLines(1000, 1000) }],
    cursor: 1000,
    nextCursor: 2000,
    hasMore: true,
    totalLines: 8200,
    hunkCount: 1,
    truncated: false,
  });
  check('split hunk page 2 continues in order', hp2.lines.length === 2000 && hp2.lines[1999].text === '+l1999');
  check('split hunk not ready until all lines arrive', loadedHunkReady(hp2.hunks[0]) === false);
  // Byte-capped incomplete hunk: descriptor complete=false, never ready.
  const capped = appendFileDiffPage(hp2, {
    snapshotId: 'snap-1',
    path: 'huge.txt',
    binary: false,
    status: 'modified',
    hunks: [{ index: 0, header: bigHunk.header, oldStart: 0, oldCount: 0, newStart: 1, newCount: 8200, contentHash: 'big-hash', totalLines: 2500, complete: false, lineStart: 2000, lines: makeLines(500, 2000) }],
    cursor: 2000,
    nextCursor: undefined,
    hasMore: false,
    totalLines: 2500,
    hunkCount: 1,
    truncated: true,
  });
  check('byte-capped hunk surfaces the cap', capped.byteCapped === true && capped.hunksComplete === true);
  check('byte-capped hunk stays unselectable', loadedHunkReady(capped.hunks[0]) === false);
}

// ---- planDiffWindow (soft cap + UI hard cap + backend stream) ----
{
  const manyLines = (n) => Array.from({ length: n }, (_, i) => ({ kind: 'addition', newNo: i + 1, text: `+l${i}` }));
  const loaded8200 = {
    snapshotId: 's',
    path: 'big.rs',
    hunks: [],
    lines: manyLines(8200),
    hunkCount: 1,
    nextCursor: 8200,
    hasMoreBackend: false,
    totalLines: 8200,
    backendTruncated: true,
    byteCapped: false,
    hunksComplete: true,
    loading: false,
    error: null,
  };
  const p1 = planDiffWindow(loaded8200, 4000);
  check('plan grows by one chunk', p1.target === 8000 && p1.canLoadMore === true);
  check('plan local window no fetch', p1.needsFetch === false && p1.hardCapped === false);
  const p2 = planDiffWindow(loaded8200, 8000);
  check('plan second chunk to end', p2.target === 8200 && p2.canLoadMore === true);
  const p3 = planDiffWindow(loaded8200, 8200);
  check('plan no more at end', p3.canLoadMore === false);
  const pEnd = planDiffWindow(loaded8200, 4000, 4000, true);
  check('plan toEnd jumps to local end', pEnd.target === 8200);
  // Initial window: a fully loaded 8200-line diff still renders only the
  // first 4000 lines (DOM bound unchanged), with Load more growing it.
  check('initial window never exceeds the soft cap', p1.localAvailable === 8200 && planDiffWindow(loaded8200, 0).target === 4000);

  const loadedBackend = {
    snapshotId: 's',
    path: 'big.rs',
    hunks: [],
    lines: manyLines(4000),
    hunkCount: 1,
    nextCursor: 4000,
    hasMoreBackend: true,
    totalLines: 4000,
    backendTruncated: true,
    byteCapped: false,
    hunksComplete: false,
    loading: false,
    error: null,
  };
  const pb = planDiffWindow(loadedBackend, 4000);
  check('plan backend needs fetch', pb.needsFetch === true && pb.totalAvailable === 20000 && pb.target === 8000);
  const pbEnd = planDiffWindow(loadedBackend, 4000, 4000, true);
  check('plan backend toEnd caps at UI limit', pbEnd.target === 20000 && pbEnd.needsFetch === true);
  const pbFull = planDiffWindow(loadedBackend, 20000, 4000, true);
  check('plan at UI cap stops', pbFull.canLoadMore === false && pbFull.hardCapped === true);

  const loadedHuge = {
    snapshotId: 's',
    path: 'huge.rs',
    hunks: [],
    lines: manyLines(25000),
    hunkCount: 1,
    nextCursor: 25000,
    hasMoreBackend: false,
    totalLines: 25000,
    backendTruncated: true,
    byteCapped: false,
    hunksComplete: true,
    loading: false,
    error: null,
  };
  const ph = planDiffWindow(loadedHuge, 4000);
  check('plan huge file hard-capped', ph.hardCapped === true && ph.localAvailable === 20000);
  check('plan huge file window still grows locally', ph.target === 8000 && ph.canLoadMore === true);
  const phEnd = planDiffWindow(loadedHuge, 20000, 4000, true);
  check('plan huge file toEnd capped at 20000', phEnd.target === 20000 && phEnd.canLoadMore === false);
}

// ---- concurrent thread state / model / activeCount normalization ----
{
  const multi = normalizeCodeReviewSnapshot({
    comparisonLabel: 'x',
    snapshotId: 's2',
    files: [],
    threads: [
      {
        identity: {
          snapshotId: 's2',
          path: 'a.ts',
          oldStart: 1,
          oldCount: 1,
          newStart: 1,
          newCount: 1,
          contentHash: 'a',
        },
        comments: [{ role: 'assistant', text: 'done', partial: false, model: 'alpha' }],
        streamingText: '',
        isStreaming: false,
        model: 'alpha',
        error: null,
        stale: false,
      },
      {
        identity: {
          snapshotId: 's2',
          path: 'b.ts',
          oldStart: 2,
          oldCount: 1,
          newStart: 2,
          newCount: 1,
          contentHash: 'b',
        },
        comments: [],
        streamingText: 'working…',
        isStreaming: true,
        model: 'beta',
        error: null,
        stale: false,
      },
      {
        identity: {
          snapshotId: 's2',
          path: 'c.ts',
          oldStart: 3,
          oldCount: 1,
          newStart: 3,
          newCount: 1,
          contentHash: 'c',
        },
        comments: [],
        streamingText: '',
        isStreaming: true,
        error: null,
        stale: false,
      },
    ],
    // omit isStreaming/activeCount → derive from threads
  });
  check('derived activeCount from threads', multi.activeCount === 2);
  check('derived aggregate isStreaming', multi.isStreaming === true);
  check('thread a not streaming', multi.threads[0].isStreaming === false);
  check('thread b streaming', multi.threads[1].isStreaming === true);
  check('thread b model', multi.threads[1].model === 'beta');
  check('comment model preserved', multi.threads[0].comments[0].model === 'alpha');
  check('empty model omitted', multi.threads[2].model === undefined);
  check(
    'threadIsStreaming helper',
    threadIsStreaming(multi.threads[1]) === true && threadIsStreaming(multi.threads[0]) === false,
  );
  check('threadIsStreaming null', threadIsStreaming(null) === false);

  const wireCount = normalizeCodeReviewSnapshot({
    threads: [
      {
        identity: {
          snapshotId: 's',
          path: 'a.ts',
          oldStart: 1,
          oldCount: 1,
          newStart: 1,
          newCount: 1,
          contentHash: 'a',
        },
        comments: [],
        streamingText: '',
        isStreaming: true,
      },
    ],
    isStreaming: true,
    activeCount: 5, // wire wins over derived (1)
  });
  check('wire activeCount preferred', wireCount.activeCount === 5);
  check('wire isStreaming preferred', wireCount.isStreaming === true);

  // empty-string model is dropped (optionalString)
  const emptyModel = normalizeThreads([
    {
      identity: {
        snapshotId: 's',
        path: 'a.ts',
        oldStart: 1,
        oldCount: 1,
        newStart: 1,
        newCount: 1,
        contentHash: 'a',
      },
      comments: [{ role: 'assistant', text: 'x', partial: false, model: '' }],
      streamingText: '',
      isStreaming: false,
      model: '',
    },
  ]);
  check('empty comment model omitted', emptyModel[0].comments[0].model === undefined);
  check('empty thread model omitted', emptyModel[0].model === undefined);
}

// ---- buildCodeReviewAbortPayload full identity ----
{
  const payload = buildCodeReviewAbortPayload({
    snapshotId: 'snap-9',
    path: 'src/x.ts',
    oldStart: 10,
    oldCount: 2,
    newStart: 12,
    newCount: 3,
    contentHash: 'hash-x',
  });
  check('abort type', payload.type === 'code_review_abort');
  check('abort snapshotId', payload.snapshotId === 'snap-9');
  check('abort path', payload.path === 'src/x.ts');
  check('abort oldStart', payload.oldStart === 10);
  check('abort oldCount', payload.oldCount === 2);
  check('abort newStart', payload.newStart === 12);
  check('abort newCount', payload.newCount === 3);
  check('abort contentHash', payload.contentHash === 'hash-x');
  check(
    'abort has no sessionId (caller stamps)',
    !Object.prototype.hasOwnProperty.call(payload, 'sessionId'),
  );
}

// ---- formatActiveRepliesLabel ----
{
  check('0 replies -> null', formatActiveRepliesLabel(0) === null);
  check('negative -> null', formatActiveRepliesLabel(-2) === null);
  check('1 reply singular', formatActiveRepliesLabel(1) === '1 reply');
  check('2 replies plural', formatActiveRepliesLabel(2) === '2 replies');
  check('float floors', formatActiveRepliesLabel(3.9) === '3 replies');
  check('NaN -> null', formatActiveRepliesLabel(Number.NaN) === null);
}

// ---- thread width bounds / persistence helpers ----
{
  check('default width', CODE_REVIEW_THREAD_WIDTH_DEFAULT === 280);
  check('min width', CODE_REVIEW_THREAD_WIDTH_MIN === 240);
  check('max width', CODE_REVIEW_THREAD_WIDTH_MAX === 480);
  check('storage key', CODE_REVIEW_THREAD_WIDTH_STORAGE_KEY === 'rpi-code-review-thread-width');
  check('clamp mid', clampCodeReviewThreadWidth(300) === 300);
  check('clamp below min', clampCodeReviewThreadWidth(100) === CODE_REVIEW_THREAD_WIDTH_MIN);
  check('clamp above max', clampCodeReviewThreadWidth(999) === CODE_REVIEW_THREAD_WIDTH_MAX);
  check('clamp NaN -> default', clampCodeReviewThreadWidth(Number.NaN) === CODE_REVIEW_THREAD_WIDTH_DEFAULT);
  check('clamp rounds', clampCodeReviewThreadWidth(300.6) === 301);
  check(
    'step shrink (-1)',
    stepCodeReviewThreadWidth(280, -1) === 280 - CODE_REVIEW_THREAD_WIDTH_STEP,
  );
  check(
    'step grow clamps at max',
    stepCodeReviewThreadWidth(CODE_REVIEW_THREAD_WIDTH_MAX, 1) === CODE_REVIEW_THREAD_WIDTH_MAX,
  );
  check(
    'step shrink clamps at min',
    stepCodeReviewThreadWidth(CODE_REVIEW_THREAD_WIDTH_MIN, -1) === CODE_REVIEW_THREAD_WIDTH_MIN,
  );
  check(
    'step grow (+1)',
    stepCodeReviewThreadWidth(280, 1) === 280 + CODE_REVIEW_THREAD_WIDTH_STEP,
  );

  const mem = new Map();
  const storage = {
    getItem(key) {
      return mem.has(key) ? mem.get(key) : null;
    },
    setItem(key, value) {
      mem.set(key, value);
    },
  };
  check('read missing -> default', readStoredCodeReviewThreadWidth(storage) === CODE_REVIEW_THREAD_WIDTH_DEFAULT);
  check('read null storage -> default', readStoredCodeReviewThreadWidth(null) === CODE_REVIEW_THREAD_WIDTH_DEFAULT);
  const written = writeStoredCodeReviewThreadWidth(storage, 350);
  check('write returns clamped', written === 350);
  check('write persists', mem.get(CODE_REVIEW_THREAD_WIDTH_STORAGE_KEY) === '350');
  check('read persisted', readStoredCodeReviewThreadWidth(storage) === 350);
  writeStoredCodeReviewThreadWidth(storage, 50);
  check('write clamps min', mem.get(CODE_REVIEW_THREAD_WIDTH_STORAGE_KEY) === String(CODE_REVIEW_THREAD_WIDTH_MIN));
  mem.set(CODE_REVIEW_THREAD_WIDTH_STORAGE_KEY, 'not-a-number');
  check(
    'read garbage -> default',
    readStoredCodeReviewThreadWidth(storage) === CODE_REVIEW_THREAD_WIDTH_DEFAULT,
  );

  const throwing = {
    getItem() {
      throw new Error('blocked');
    },
    setItem() {
      throw new Error('blocked');
    },
  };
  check(
    'read throws -> default',
    readStoredCodeReviewThreadWidth(throwing) === CODE_REVIEW_THREAD_WIDTH_DEFAULT,
  );
  check(
    'write throws still returns clamp',
    writeStoredCodeReviewThreadWidth(throwing, 400) === 400,
  );
}

console.log(`\ncodeReview.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
