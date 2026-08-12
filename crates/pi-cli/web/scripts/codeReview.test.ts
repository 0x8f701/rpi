#!/usr/bin/env node
// Focused regression for src/codeReview.ts — defensive wire normalization
// of the code_review_* snapshot payload (files/hunks/lines/threads) plus
// /code-review argument arity. Run through `npm run build`.
//
// Exit codes: 0 = every assertion held; 1 = a regression.
// Assertions exercise BEHAVIOR (what normalize returns), not source strings.
import {
  countFileThreads,
  countThreadComments,
  emptyCodeReviewSnapshot,
  FILE_STATUS_LETTERS,
  fileStatusLetter,
  findThreadForHunk,
  hunkIdentityFor,
  hunkKey,
  hunkKeyFor,
  normalizeCodeReviewSnapshot,
  normalizeThreads,
  parseCodeReviewArgs,
  appendFileDiffPage,
  buildFileTree,
  initLoadedDiff,
  isDiffPlaceholder,
  normalizeFileDiffPage,
  planDiffWindow,
  treeFileIndexAt,
  treeFilterRows,
  treeKeyboardAction,
  treeToggleCollapse,
  treeVisibleRows,
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
        comments: [{ role: 'user', text: 'looks off', partial: false }],
        streamingText: 'partial…',
        error: null,
        stale: false,
      },
    ],
    isStreaming: true,
    activeHunk: {
      snapshotId: 'snap-1',
      path: 'src/a.ts',
      oldStart: 1,
      oldCount: 3,
      newStart: 1,
      newCount: 4,
      contentHash: 'h1',
    },
  };
  const snap = normalizeCodeReviewSnapshot(wire);
  check('label preserved', snap.comparisonLabel === 'HEAD → working tree');
  check('snapshotId preserved', snap.snapshotId === 'snap-1');
  check('truncated preserved', snap.truncated === true);
  check('error null preserved', snap.error === null);
  check('totals', snap.totalInsertions === 3 && snap.totalDeletions === 1);
  check('isStreaming', snap.isStreaming === true);
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
  check('streamingText', snap.threads[0].streamingText === 'partial…');
  check('activeHunk path', snap.activeHunk && snap.activeHunk.path === 'src/a.ts');
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
    activeHunk: { path: '', contentHash: 'x' }, // incomplete identity → null
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
  check('bad activeHunk -> null', snap.activeHunk === null);
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
      comments: [{ role: 'assistant', text: 'hi', partial: true }],
      streamingText: '',
      error: 'boom',
      stale: true,
    },
    bad: null,
    incomplete: { identity: { path: 'x' } },
  });
  check('map threads length', mapThreads.length === 1);
  check('map thread role', mapThreads[0].comments[0].role === 'assistant');
  check('map thread error', mapThreads[0].error === 'boom');
  check('map thread stale', mapThreads[0].stale === true);

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
      streamingText: '',
      error: null,
      stale: false,
    },
  ]);
  check('array threads length', arrThreads.length === 1);
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

// ---- isDiffPlaceholder / placeholder LoadedDiff + byte cap ----
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
  check('placeholder loaded diff starts empty', loaded.lines.length === 0 && loaded.snapshotLineCount === 0);
  check('placeholder paging enabled at cursor 0', loaded.hasMoreBackend === true && loaded.nextCursor === 0);
  check('placeholder not byte-capped initially', loaded.byteCapped === false);
  const noPaging = initLoadedDiff('snap-1', placeholder, false);
  check('placeholder without global truncation does not page', noPaging.hasMoreBackend === false);

  const page = {
    snapshotId: 'snap-1',
    path: 'big3.txt',
    binary: false,
    status: 'modified',
    lines: [{ kind: 'addition', newNo: 1, text: '+a' }],
    cursor: 0,
    nextCursor: undefined,
    hasMore: false,
    totalLines: 1,
    truncated: true,
  };
  const merged = appendFileDiffPage(loaded, page);
  check('byte cap surfaced from page', merged.byteCapped === true);
  check('byte cap stops paging', merged.hasMoreBackend === false && merged.totalLines === 1);
  const normalPage = { ...page, truncated: false, lines: [{ kind: 'addition', newNo: 2, text: '+b' }], cursor: 1, nextCursor: 2, hasMore: true, totalLines: 2 };
  const loaded2 = appendFileDiffPage(loaded, { ...page, truncated: false, lines: [{ kind: 'addition', newNo: 1, text: '+a' }], nextCursor: 1, hasMore: true });
  check('byte cap absent on clean pages', appendFileDiffPage(loaded2, normalPage).byteCapped === false);
}

// ---- planDiffWindow for an empty placeholder (zero snapshot lines) ----
{
  const loaded = {
    snapshotId: 's',
    path: 'p.rs',
    lines: [],
    snapshotLineCount: 0,
    nextCursor: 0,
    hasMoreBackend: true,
    totalLines: 0,
    backendTruncated: true,
    byteCapped: false,
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

// ---- normalizeFileDiffPage ----
{
  const valid = normalizeFileDiffPage({
    snapshotId: 'snap-1',
    path: 'a.rs',
    binary: false,
    status: 'modified',
    lines: [
      { kind: 'addition', text: '+x', newNo: 1 },
      { kind: 'deletion', text: '-y', oldNo: 2 },
      { kind: 'context', text: ' z', oldNo: 3, newNo: 3 },
    ],
    cursor: 0,
    nextCursor: 3,
    hasMore: true,
    totalLines: 200,
    truncated: false,
  });
  check('page normalized', !!valid);
  check('page fields', valid.snapshotId === 'snap-1' && valid.path === 'a.rs' && valid.cursor === 0);
  check('page nextCursor', valid.nextCursor === 3);
  check('page hasMore', valid.hasMore === true && valid.totalLines === 200);
  check('page lines kinds', valid.lines.map((l) => l.kind).join(',') === 'addition,deletion,context');
  check('page line numbers', valid.lines[0].newNo === 1 && valid.lines[1].oldNo === 2);
  check('page no nextCursor when absent', normalizeFileDiffPage({ snapshotId: 's', path: 'a', lines: [], cursor: 0, hasMore: false, totalLines: 0, truncated: false }).nextCursor === undefined);
  check('page missing path -> null', normalizeFileDiffPage({ snapshotId: 's', lines: [] }) === null);
  check('page missing snapshotId -> null', normalizeFileDiffPage({ path: 'a', lines: [] }) === null);
  check('page null -> null', normalizeFileDiffPage(null) === null);
  check('page garbage -> null', normalizeFileDiffPage('nope') === null);
  const badStatus = normalizeFileDiffPage({ snapshotId: 's', path: 'a', lines: [], cursor: 0, hasMore: false, totalLines: 0, truncated: false, status: 'weird' });
  check('page unknown status -> changed', badStatus.status === 'changed');
  const badLine = normalizeFileDiffPage({ snapshotId: 's', path: 'a', lines: [{ kind: 'bogus', text: 9 }, null], cursor: 0, hasMore: false, totalLines: 0, truncated: false });
  check('page malformed lines coerced/dropped', badLine.lines.length === 1 && badLine.lines[0].kind === 'meta' && badLine.lines[0].text === '');
}

// ---- initLoadedDiff / appendFileDiffPage ----
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
  check('loaded snapshot lines', loaded.lines.length === 3 && loaded.snapshotLineCount === 3);
  check('loaded cursor starts at snapshot lines', loaded.nextCursor === 3);
  check('loaded backend paging enabled', loaded.hasMoreBackend === true);
  check('loaded truncated state', loaded.backendTruncated === true && loaded.totalLines === 3);
  const noPaging = initLoadedDiff('snap-1', file, false);
  check('loaded paging off when snapshot not truncated', noPaging.hasMoreBackend === false);
  const smallFile = initLoadedDiff('snap-1', { ...file, truncated: false }, true);
  check('loaded paging off when file not truncated', smallFile.hasMoreBackend === false);

  const page = {
    snapshotId: 'snap-1',
    path: 'big.rs',
    binary: false,
    status: 'modified',
    lines: [{ kind: 'addition', newNo: 3, text: '+d' }, { kind: 'addition', newNo: 4, text: '+e' }],
    cursor: 3,
    nextCursor: 5,
    hasMore: true,
    totalLines: 5,
    truncated: false,
  };
  const merged = appendFileDiffPage(loaded, page);
  check('page appended in order', merged.lines.length === 5 && merged.lines[3].text === '+d' && merged.lines[4].text === '+e');
  check('page cursor advances', merged.nextCursor === 5);
  check('page total updated', merged.totalLines === 5);
  check('page hasMore carried', merged.hasMoreBackend === true);
  check('page clears error/loading', merged.loading === false && merged.error === null);
  const dupPage = appendFileDiffPage(merged, page);
  check('duplicate cursor page dropped', dupPage === merged && dupPage.lines.length === 5);
  const staleSnap = appendFileDiffPage(loaded, { ...page, snapshotId: 'other-snap' });
  check('stale snapshot page dropped', staleSnap === loaded);
  const wrongPath = appendFileDiffPage(loaded, { ...page, path: 'other.rs' });
  check('wrong path page dropped', wrongPath === loaded);
  const wrongCursor = appendFileDiffPage(loaded, { ...page, cursor: 0 });
  check('wrong cursor page dropped', wrongCursor === loaded);
  const finalPage = appendFileDiffPage(merged, { ...page, cursor: 5, nextCursor: undefined, hasMore: false, lines: [{ kind: 'context', oldNo: 5, newNo: 5, text: ' f' }] });
  check('final page stops paging', finalPage.lines.length === 6 && finalPage.hasMoreBackend === false && finalPage.nextCursor === 6);
}

// ---- planDiffWindow (soft cap + UI hard cap + backend fetch) ----
{
  const manyLines = (n) => Array.from({ length: n }, (_, i) => ({ kind: 'addition', newNo: i + 1, text: `+l${i}` }));
  const loaded8200 = {
    snapshotId: 's',
    path: 'big.rs',
    lines: manyLines(8200),
    snapshotLineCount: 8200,
    nextCursor: 8200,
    hasMoreBackend: false,
    totalLines: 8200,
    backendTruncated: true,
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

  const loadedBackend = {
    snapshotId: 's',
    path: 'big.rs',
    lines: manyLines(4000),
    snapshotLineCount: 4000,
    nextCursor: 4000,
    hasMoreBackend: true,
    totalLines: 4000,
    backendTruncated: true,
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
    lines: manyLines(25000),
    snapshotLineCount: 25000,
    nextCursor: 25000,
    hasMoreBackend: false,
    totalLines: 25000,
    backendTruncated: true,
    loading: false,
    error: null,
  };
  const ph = planDiffWindow(loadedHuge, 4000);
  check('plan huge file hard-capped', ph.hardCapped === true && ph.localAvailable === 20000);
  check('plan huge file window still grows locally', ph.target === 8000 && ph.canLoadMore === true);
  const phEnd = planDiffWindow(loadedHuge, 20000, 4000, true);
  check('plan huge file toEnd capped at 20000', phEnd.target === 20000 && phEnd.canLoadMore === false);
}

console.log(`\ncodeReview.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
