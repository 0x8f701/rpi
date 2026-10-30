#!/usr/bin/env node
// Focused regression for src/codeReview.ts — defensive wire normalization
// of the code_review_* snapshot payload (files/hunks/lines/threads) plus
// /code-review argument arity. Run through `npm run build`.
//
// Exit codes: 0 = every assertion held; 1 = a regression.
// Assertions exercise BEHAVIOR (what normalize returns), not source strings.
import {
  applyAutoReviewButtonEvent,
  applyHighlightOwnerEvent,
  applyReviewLifecycle,
  AUTO_REVIEW_SPINNER_GATE_MS,
  autoReviewBusy,
  autoReviewButtonView,
  highlightButtonView,
  highlightOwnerBusy,
  CODE_REVIEW_BATCH_POLL_MS,
  CODE_REVIEW_SNAPSHOT_POLL_MS,
  codeReviewBatchOwnerLive,
  snapshotPollDelayMs,
  buildCodeReviewAbortPayload,
  buildCodeReviewStopPayload,
  clampCodeReviewThreadWidth,
  CODE_REVIEW_THREAD_WIDTH_DEFAULT,
  CODE_REVIEW_THREAD_WIDTH_STORAGE_KEY,
  CODE_REVIEW_THREAD_WIDTH_MAX,
  CODE_REVIEW_THREAD_WIDTH_MIN,
  CODE_REVIEW_THREAD_WIDTH_STEP,
  emptyCodeReviewSnapshot,
  emptyComparisonMessage,
  emptyReviewLifecycle,
  initialHighlightOwnerState,
  initialReviewErrorReport,
  parseCodeReviewAutoReviewAck,
  parseCodeReviewHighlightAck,
  reviewErrorNotices,
  threadLatestUserIsAuto,
  FILE_STATUS_LETTERS,
  fileStatusLetter,
  diffTreePaddingLeft,
  fileChangeStats,
  fileListChangeStats,
  loadedDiffAuthoritativeForSnapshot,
  loadedDiffChangeStats,
  findThreadForHunk,
  formatActiveRepliesLabel,
  formatReviewCloseWarning,
  formatReviewRefreshWarning,
  hunkIdentityFor,
  initialAutoReviewButtonState,
  hunkKey,
  hunkKeyFor,
  normalizeCodeReviewSnapshot,
  normalizeFileDiffPage,
  normalizeThreads,
  overlayReviewLifecycle,
  parseCodeReviewArgs,
  codeReviewOpenCommand,
  codeReviewOpenSnapshotError,
  normalizeCodeReviewCommits,
  buildCodeReviewCommitsPayload,
  filterCommitRows,
  commitPickerEnterAction,
  shortCommitLabel,
  isValidGitHash,
  CODE_REVIEW_COMMITS_MAX_LIMIT,
  CODE_REVIEW_COMMIT_LABEL_HEX,
  classifyReviewProbe,
  isNoOpenCodeReviewError,
  reviewModelOptionValue,
  buildCodeReviewSetModelPayload,
  buildCodeReviewAutoReviewPayload,
  emptyLoadedDiffCache,
  nextFileDiffLoadState,
  shouldEagerFileDiffFetch,
  appendFileDiffPage,
  estimateLoadedDiffBytes,
  buildFileTree,
  initLoadedDiff,
  hunkIsComplete,
  isDiffPlaceholder,
  loadedHunkReady,
  CODE_REVIEW_MAX_COMMENT_BYTES,
  LOADED_DIFF_CACHE_MAX_BYTES,
  LOADED_DIFF_CACHE_MAX_ENTRIES,
  loadedDiffCachePut,
  loadedDiffCacheTouch,
  fileDiffStatusView,
  isNearDiffBottom,
  planDiffWindow,
  readStoredCodeReviewThreadWidth,
  reviewComposerLocked,
  reviewHasInternalBusy,
  reviewLifecycleActiveCount,
  reviewReplyIsLive,
  reviewReplyIsTerminal,
  stepCodeReviewThreadWidth,
  threadIsCancellable,
  threadIsStreaming,
  threadNeedsThinkingPlaceholder,
  countThreadComments,
  countFileThreads,
  treeFileIndexAt,
  treeFilterRows,
  treeKeyboardAction,
  treeToggleCollapse,
  treeVisibleRows,
  shouldRenderReviewError,
  untrackedNotShownNote,
  writeStoredCodeReviewThreadWidth,
  CONTEXT_EXPAND_LINES,
  reviewDiffGaps,
  newGapExpandState,
  remainingDiffGap,
  gapExpandWindow,
  recordGapExpand,
  mergeExpandedLines,
  normalizeFileContextPage,
  aggregateFindingsByDirectory,
  aggregateFindingsByFile,
  buildCodeReviewTldrPayload,
  parseCodeReviewTldrAck,
  tldrButtonView,
  tldrMarkdownForSnapshot,
  CODE_REVIEW_FILES_WIDTH_DEFAULT,
  CODE_REVIEW_FILES_WIDTH_MAX,
  CODE_REVIEW_FILES_WIDTH_MIN,
  CODE_REVIEW_FILES_WIDTH_STEP,
  CODE_REVIEW_FILES_WIDTH_STORAGE_KEY,
  CODE_REVIEW_ROOT_DIR_KEY,
  CODE_REVIEW_MAX_FINDING_SUMMARY_BYTES,
  clampCodeReviewFilesWidth,
  readStoredCodeReviewFilesWidth,
  stepCodeReviewFilesWidth,
  writeStoredCodeReviewFilesWidth,
} from '../src/codeReview.ts';
import {
  applyCodeReviewToastAnchor,
  clearCodeReviewToastAnchor,
  CODE_REVIEW_TOAST_GAP_PX,
  CODE_REVIEW_TOAST_MAX_HEIGHT_VAR,
  CODE_REVIEW_TOAST_TOP_VAR,
  codeReviewToastAnchor,
  syncCodeReviewToastAnchor,
} from '../src/codeReviewToastAnchor.ts';

const failures = [];

let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}
// ---- transport failures toast; review business failures stay inline ----
check('transport close is not rendered inline', !shouldRenderReviewError(new Error('connection closed (code 1006)')));
check('bounded review timeout remains inline', shouldRenderReviewError(new Error('code_review_open timed out after 600s (no response from server)')));
check('RPC business failure remains inline', shouldRenderReviewError(Object.assign(new Error('review failed'), { rpc: true })));

{
  const idle = initialAutoReviewButtonState();
  const idleView = autoReviewButtonView(idle, true);
  check('idle auto-review view has no progress', idleView.progress === null && idleView.phase === 'idle' && !autoReviewBusy(idle));
  const pending = applyAutoReviewButtonEvent(idle, { type: 'click', reviewId: 'rev-1' });
  const pendingHidden = autoReviewButtonView(pending, false);
  const pendingShown = autoReviewButtonView(pending, true);
  check(
    'pending click stays busy and screens only after the spinner gate',
    autoReviewBusy(pending)
      && pending.reviewId === 'rev-1'
      && pendingHidden.phase === 'triaging'
      && pendingHidden.progress === null
      && pendingShown.phase === 'triaging'
      && pendingShown.progress === 'Screening…',
  );
  // A stale idle snapshot during the click → ACK window cannot clear the
  // request owner: the button stays a live Stop with no close/duplicate gap.
  check(
    'stale idle snapshot does not clear a pending request owner',
    applyAutoReviewButtonEvent(pending, {
      type: 'snapshot',
      state: 'idle',
      counts: { submitted: 0, completed: 0, failed: 0 },
      batchId: 0,
      reviewId: 'rev-1',
    }) === pending,
  );
  const acked = applyAutoReviewButtonEvent(pending, { type: 'ack', batchId: 7 });
  check(
    'the accepted ACK keeps the owner busy and records the batch identity',
    autoReviewBusy(acked) && acked.pending && acked.acceptedBatchId === 7,
  );
  check(
    'an older-batch idle snapshot cannot clear the accepted owner',
    applyAutoReviewButtonEvent(acked, {
      type: 'snapshot',
      state: 'idle',
      counts: { submitted: 0, completed: 0, failed: 0 },
      batchId: 3,
      reviewId: 'rev-1',
    }) === acked,
  );
  const released = applyAutoReviewButtonEvent(acked, {
    type: 'snapshot',
    state: 'triaging',
    counts: { submitted: 0, completed: 0, failed: 0 },
    batchId: 7,
    reviewId: 'rev-1',
  });
  const triageView = autoReviewButtonView(released, true);
  check(
    'the first matching-or-newer snapshot releases the owner into screening',
    !released.pending && released.acceptedBatchId === null
      && autoReviewBusy(released)
      && released.wireState === 'triaging'
      && triageView.phase === 'triaging'
      && triageView.progress === 'Screening…',
  );
  const running = applyAutoReviewButtonEvent(released, {
    type: 'snapshot',
    state: 'running',
    counts: { submitted: 2, completed: 0, failed: 0 },
    batchId: 7,
    reviewId: 'rev-1',
  });
  const runningHidden = autoReviewButtonView(running, false);
  const runningShown = autoReviewButtonView(running, true);
  check(
    'running hides Review 0/2 until the spinner gate',
    autoReviewBusy(running) && runningHidden.phase === 'running' && runningHidden.progress === null,
  );
  check(
    'running after the spinner gate paints Review 0/2',
    runningShown.phase === 'running' && runningShown.progress === 'Review 0/2',
  );
  check(
    'a wire running state with explicit zero counts still reviews, it does not screen',
    autoReviewButtonView(applyAutoReviewButtonEvent(initialAutoReviewButtonState(), {
      type: 'snapshot',
      state: 'running',
      counts: { submitted: 0, completed: 0, failed: 0 },
      batchId: 1,
    }), true).progress === 'Review 0/0',
  );
  check(
    'absent running counts render dashes instead of invented zeros',
    autoReviewButtonView(applyAutoReviewButtonEvent(initialAutoReviewButtonState(), {
      type: 'snapshot',
      state: 'running',
      counts: { submitted: null, completed: null, failed: null },
      batchId: 1,
    }), true).progress === 'Review --/--',
  );
  check(
    'a rejected request clears only its own owner and keeps the last state',
    (() => {
      const rejected = applyAutoReviewButtonEvent(running, { type: 'reject' });
      return !rejected.pending && rejected.wireState === 'running'
        && rejected.acceptedBatchId === null
        && rejected.reviewId === null;
    })(),
  );
  const done = applyAutoReviewButtonEvent(running, {
    type: 'snapshot',
    state: 'done',
    counts: { submitted: 2, completed: 2, failed: 0 },
    batchId: 7,
    reviewId: 'rev-1',
  });
  const doneView = autoReviewButtonView(done, false);
  check(
    'done keeps Review 2/2 after the batch is no longer busy',
    !autoReviewBusy(done) && doneView.phase === 'done' && doneView.progress === 'Review 2/2'
      && autoReviewButtonView(done, true).progress === 'Review 2/2',
  );
  check(
    'an ACK after the owner was released is a no-op',
    applyAutoReviewButtonEvent(done, { type: 'ack', batchId: 7 }) === done,
  );
  // Pre-ACK adoption: a visible triaging/running state during the click window
  // is the accepted batch (only one batch may run), so the owner releases.
  check(
    'a pre-ACK visible triaging snapshot releases the owner',
    (() => {
      const adopted = applyAutoReviewButtonEvent(pending, {
        type: 'snapshot',
        state: 'triaging',
        counts: { submitted: 0, completed: 0, failed: 0 },
        batchId: 9,
        reviewId: 'rev-1',
      });
      return !adopted.pending && autoReviewBusy(adopted) && adopted.wireState === 'triaging';
    })(),
  );
  check(
    'a pending owner for a stale reviewId is released by the first snapshot of a new review',
    (() => {
      const restarted = applyAutoReviewButtonEvent(pending, {
        type: 'snapshot',
        state: 'idle',
        counts: { submitted: 0, completed: 0, failed: 0 },
        batchId: 0,
        reviewId: 'rev-2',
      });
      return !restarted.pending
        && restarted.reviewId === null
        && restarted.acceptedBatchId === null
        && !autoReviewBusy(restarted)
        && restarted.wireState === 'idle';
    })(),
  );
  check(
    'an accepted owner for a stale reviewId is released by a restart idle snapshot',
    (() => {
      const restarted = applyAutoReviewButtonEvent(acked, {
        type: 'snapshot',
        state: 'idle',
        counts: { submitted: 0, completed: 0, failed: 0 },
        batchId: 0,
        reviewId: 'rev-2',
      });
      return !restarted.pending
        && !autoReviewBusy(restarted)
        && restarted.wireState === 'idle';
    })(),
  );
}

// ---- strict ACK parses: current shapes accepted, malformed shapes rejected ----
{
  check(
    'auto ACK parses the current triaging shape',
    (() => {
      const ack = parseCodeReviewAutoReviewAck({ ok: true, state: 'triaging', batchId: 12 });
      return ack.ok && ack.state === 'triaging' && ack.batchId === 12;
    })(),
  );
  const badAutoAcks = [
    null,
    'nope',
    { ok: false, state: 'triaging', batchId: 1 },
    { ok: true, state: 'running', batchId: 1 },
    { ok: true, state: 'triaging' },
    { ok: true, state: 'triaging', batchId: '7' },
    { ok: true, state: 'triaging', batchId: -1 },
    { ok: true, state: 'triaging', batchId: 1.5 },
  ];
  for (const wire of badAutoAcks) {
    check(
      `malformed auto ACK is strictly rejected (${JSON.stringify(wire) ?? String(wire)})`,
      !parseCodeReviewAutoReviewAck(wire).ok,
    );
  }
  check(
    'highlight ACK parses the current running shape with the generation identity',
    (() => {
      const ack = parseCodeReviewHighlightAck({ ok: true, state: 'running', highlightGeneration: 4 });
      return ack.ok && ack.state === 'running' && ack.generation === 4;
    })(),
  );
  const badHighlightAcks = [
    null,
    7,
    { ok: true, state: 'triaging', highlightGeneration: 1 },
    { ok: true, state: 'running' },
    { ok: true, state: 'running', highlightGeneration: '3' },
  ];
  for (const wire of badHighlightAcks) {
    check(
      'malformed highlight ACK is strictly rejected',
      !parseCodeReviewHighlightAck(wire).ok,
      JSON.stringify(wire) ?? String(wire),
    );
  }
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
  check('empty helper untracked defaults to 0', emptyCodeReviewSnapshot().untrackedNotShown === 0);
  check('empty helper internalBusy defaults false', emptyCodeReviewSnapshot().internalBusy === false);
  check('empty helper isStreaming defaults false', emptyCodeReviewSnapshot().isStreaming === false);
  check('null snapshot internalBusy fails closed', nullSnap.internalBusy === false);
}

// ---- untracked-not-shown note: path-free empty-state copy ----
{
  check('note absent at 0', untrackedNotShownNote(0) === null);
  check('note absent for negative', untrackedNotShownNote(-3) === null);
  check(
    'singular note',
    untrackedNotShownNote(1) === '1 untracked file is not part of the review',
  );
  check(
    'plural note',
    untrackedNotShownNote(2) === '2 untracked files are not part of the review',
  );
  check(
    'empty comparison with no untracked',
    emptyComparisonMessage(0) === 'No changed files in this comparison.',
  );
  check(
    'empty comparison with untracked',
    emptyComparisonMessage(2) ===
      'No tracked changes — 2 untracked files are not part of the review',
  );
  const wire = normalizeCodeReviewSnapshot({
    comparisonLabel: 'HEAD → working tree',
    snapshotId: 'snap-u',
    files: [],
    untrackedNotShown: 3,
  });
  check('wire count preserved', wire.untrackedNotShown === 3);
  check(
    'wire empty comparison uses count',
    emptyComparisonMessage(wire.untrackedNotShown) ===
      'No tracked changes — 3 untracked files are not part of the review',
  );
  const missing = normalizeCodeReviewSnapshot({ snapshotId: 'snap-clean', files: [] });
  check('missing wire count -> 0', missing.untrackedNotShown === 0);
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
    totalsComplete: true,
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
  check('totalsComplete preserved', snap.totalsComplete === true);
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
  check('comment model', snap.threads[0].comments[1].model === 'gpt-test');
  check('thread model', snap.threads[0].model === 'gpt-test');
  check('thread isStreaming', snap.threads[0].isStreaming === true);
  check('streamingText', snap.threads[0].streamingText === 'partial…');
  check('no activeHunk field', !('activeHunk' in snap));
  check('omitted thread isCancellable fails closed', snap.threads[0].isCancellable === false);
  check('omitted snapshot internalBusy fails closed', snap.internalBusy === false);
  check('omitted review model identity is null', snap.reviewModelProvider === null && snap.reviewModelId === null && snap.reviewModelLabel === null);
  const identitySnap = normalizeCodeReviewSnapshot({
    ...wire,
    reviewModelProvider: 'user-steering',
    reviewModelId: 'mock',
    reviewModelLabel: 'Mock Model Nine',
  });
  check(
    'review model identity is provider+id, never inferred from the label',
    identitySnap.reviewModelProvider === 'user-steering'
      && identitySnap.reviewModelId === 'mock'
      && identitySnap.reviewModelLabel === 'Mock Model Nine',
  );
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
    totalsComplete: 'yes',
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
    internalBusy: 1,
  };
  const snap = normalizeCodeReviewSnapshot(wire);
  check('non-string label -> empty', snap.comparisonLabel === '');
  check('non-string snapshotId -> empty', snap.snapshotId === '');
  check('non-string error -> null', snap.error === null);
  check('negative insertions -> 0', snap.totalInsertions === 0);
  check('non-number deletions -> 0', snap.totalDeletions === 0);
  check('non-bool totalsComplete -> false', snap.totalsComplete === false);
  check('only valid file kept', snap.files.length === 1 && snap.files[0].path === 'ok.ts');
  check('unknown status -> changed', snap.files[0].status === 'changed');
  check('non-bool binary -> false', snap.files[0].binary === false);
  check('float insertions floored', snap.files[0].insertions === 1);
  check('negative deletions -> 0', snap.files[0].deletions === 0);
  // Fail-closed: missing bodyState on a non-binary file is page-needed, never
  // silently "complete" (an extra bounded page fetch is harmless).
  check('missing bodyState -> preview (fail-closed)', snap.files[0].bodyState === 'preview');
  check('only hashed hunk kept', snap.files[0].hunks.length === 1);
  check('unknown line kind -> meta', snap.files[0].hunks[0].lines[0].kind === 'meta');
  check('non-string line text coerced', snap.files[0].hunks[0].lines[0].text === '');
  check('valid line kept', snap.files[0].hunks[0].lines[1].text === 'ok');
  check('null threads -> []', snap.threads.length === 0);
  check('non-bool isStreaming -> false', snap.isStreaming === false);
  check('negative activeCount -> 0', snap.activeCount === 0);
  check('malformed internalBusy -> false', snap.internalBusy === false);
  check('malformed internalBusy not inferred from threads', reviewHasInternalBusy(snap) === false);
}

{
  const snap = normalizeCodeReviewSnapshot({
    threads: [
      {
        identity: {
          snapshotId: 's',
          path: 'ok.ts',
          oldStart: 1,
          oldCount: 1,
          newStart: 1,
          newCount: 1,
          contentHash: 'hh',
        },
        comments: [],
        isCancellable: 'yes',
      },
    ],
    internalBusy: 'yes',
  });
  check('malformed isCancellable -> false', snap.threads[0].isCancellable === false);
  check('string internalBusy -> false', snap.internalBusy === false);
}

// ---- thinking wire: streamingThinking + assistant-comment thinking ----
{
  const threadWire = (over: Record<string, unknown>) => ({
    identity: {
      snapshotId: 's',
      path: 'a.ts',
      oldStart: 1,
      oldCount: 1,
      newStart: 1,
      newCount: 1,
      contentHash: 'hh',
    },
    comments: [
      { role: 'user', text: 'why?', partial: false },
      { role: 'assistant', text: 'because', partial: false, thinking: 'reasoning trace' },
      { role: 'assistant', text: 'empty trace', partial: false, thinking: '' },
      { role: 'assistant', text: 'bad trace', partial: false, thinking: 7 },
    ],
    isStreaming: true,
    streamingText: '',
    streamingThinking: 'live reasoning',
    ...over,
  });
  const snap = normalizeCodeReviewSnapshot({ threads: [threadWire({})] });
  const thread = snap.threads[0];
  check('streamingThinking preserved while streaming', thread.streamingThinking === 'live reasoning');
  check('comment thinking preserved', thread.comments[1].thinking === 'reasoning trace');
  check('empty comment thinking omitted', !('thinking' in thread.comments[2]));
  check('non-string comment thinking omitted', !('thinking' in thread.comments[3]));
  check('user comment has no thinking', !('thinking' in thread.comments[0]));
  check('no placeholder while thinking streams', threadNeedsThinkingPlaceholder(thread) === false);

  const deadAir = normalizeCodeReviewSnapshot({
    threads: [threadWire({ streamingThinking: '' })],
  }).threads[0];
  check('placeholder fires on dead air', threadNeedsThinkingPlaceholder(deadAir) === true);
  const garbageThinking = normalizeCodeReviewSnapshot({
    threads: [threadWire({ streamingThinking: 7 })],
  }).threads[0];
  check('non-string streamingThinking coerced to empty', garbageThinking.streamingThinking === '');
  check('placeholder fires on coerced empty thinking', threadNeedsThinkingPlaceholder(garbageThinking) === true);
  check(
    'placeholder suppressed once text streams',
    threadNeedsThinkingPlaceholder(
      normalizeCodeReviewSnapshot({ threads: [threadWire({ streamingText: 'partial' })] }).threads[0],
    ) === false,
  );

  const settled = normalizeCodeReviewSnapshot({
    threads: [threadWire({ isStreaming: false, streamingText: 'residue', streamingThinking: 'residue' })],
  }).threads[0];
  check('terminal status clears streaming buffers', settled.streamingText === '' && settled.streamingThinking === '');
  check('placeholder never fires when idle', threadNeedsThinkingPlaceholder(settled) === false);
  check('placeholder fails closed without a thread', threadNeedsThinkingPlaceholder(undefined) === false);

  // Overlay projection: a live owner keeps the snapshot's thinking buffer,
  // a terminal owner clears it, and a lifecycle-only thread has none yet.
  const live = applyReviewLifecycle(emptyReviewLifecycle('rev-1', 's'), { type: 'snapshot', snapshot: snap });
  check(
    'overlay keeps thinking of the live owner',
    overlayReviewLifecycle(snap, live).threads[0].streamingThinking === 'live reasoning',
  );
  const terminated = applyReviewLifecycle(live, {
    type: 'terminal',
    key: hunkKey(thread.identity),
    generation: 0,
    status: 'completed',
  });
  const settledOverlay = overlayReviewLifecycle(snap, terminated);
  check(
    'terminal overlay clears projected thinking',
    settledOverlay.threads[0].streamingThinking === '' && settledOverlay.threads[0].isStreaming === false,
  );
  const detached = overlayReviewLifecycle(snap, emptyReviewLifecycle('rev-1', 'other-snap'));
  check(
    'unowned thread falls back to the wire thinking buffer',
    detached.threads[0].streamingThinking === 'live reasoning',
  );
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
      // omit replyStatus: fail closed to idle (no live inference)
      error: null,
      stale: false,
    },
  ]);
  check('array threads length', arrThreads.length === 1);
  check('missing replyStatus stays idle', arrThreads[0].isStreaming === false && arrThreads[0].replyStatus === 'idle');
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
  check(
    'one rev is base-to-working-tree',
    one.ok === true && one.ok && one.from === 'main' && one.to === undefined && one.includeWorkingTree === true,
  );
  const three = parseCodeReviewArgs('a b c');
  check('three revs error', three.ok === false);
  const spaced = parseCodeReviewArgs('  abc  def  ');
  check(
    'spaced two revs',
    spaced.ok === true && spaced.ok && spaced.from === 'abc' && spaced.to === 'def',
  );
}

const COMMIT_SHA40 = 'a'.repeat(40);
const COMMIT_SHA64 = 'b'.repeat(64);
const COMMIT_PARENT = 'c'.repeat(40);
const commitRowFixture = (over = {}) => ({
  commit: COMMIT_SHA40,
  parentCommit: null,
  parentCount: 0,
  subject: 'add commit picker',
  author: 'Pi Bot',
  authorDate: '2026-10-24T10:00:00+02:00',
  ...over,
});
{
  const twoRevs = codeReviewOpenCommand({ from: 'abc', to: 'def' });
  check(
    'open command for two revisions sends from+to',
    twoRevs.ok === true && twoRevs.ok && JSON.stringify(twoRevs.command) === JSON.stringify({ type: 'code_review_open', from: 'abc', to: 'def' }),
  );
  const oneRev = codeReviewOpenCommand({ from: 'origin/mainnet-beta', includeWorkingTree: true });
  check(
    'open command for one revision sends includeWorkingTree',
    oneRev.ok === true && oneRev.ok && JSON.stringify(oneRev.command) === JSON.stringify({ type: 'code_review_open', from: 'origin/mainnet-beta', includeWorkingTree: true }),
  );
  const pinnedCommit = codeReviewOpenCommand({ commit: COMMIT_SHA40 });
  check(
    'open command for a pinned commit sends only commit',
    pinnedCommit.ok === true && pinnedCommit.ok && JSON.stringify(pinnedCommit.command) === JSON.stringify({ type: 'code_review_open', commit: COMMIT_SHA40 }),
  );
  const pinnedSha256 = codeReviewOpenCommand({ commit: COMMIT_SHA64 });
  check(
    'open command accepts sha-256 object ids (not 40-only)',
    pinnedSha256.ok === true && pinnedSha256.ok && pinnedSha256.command.commit === COMMIT_SHA64,
  );
  const conflated = codeReviewOpenCommand({ commit: COMMIT_SHA40, from: 'main', to: 'HEAD', includeWorkingTree: true });
  check(
    'mixed operands are REJECTED, never silently narrowed to one scope',
    conflated.ok === false && !conflated.ok && conflated.error === 'code_review_open commit excludes from, to, and includeWorkingTree',
  );
  const commitWithFrom = codeReviewOpenCommand({ commit: COMMIT_SHA40, from: 'main' });
  const commitWithWorkingTree = codeReviewOpenCommand({ commit: COMMIT_SHA40, includeWorkingTree: true });
  check(
    'every revision-operand conflation rejects (from alone or includeWorkingTree too)',
    commitWithFrom.ok === false && commitWithWorkingTree.ok === false,
  );
  const emptyCommit = codeReviewOpenCommand({ commit: '' });
  check(
    'an explicitly empty commit rides the wire (backend fails clearly) instead of a silent working-tree open',
    emptyCommit.ok === true && emptyCommit.ok && emptyCommit.command.commit === '',
  );
  check(
    'open snapshot error is the git message, never the ref',
    codeReviewOpenSnapshotError({
      error: 'invalid source revision; expected a commit, branch, or tag',
      comparisonLabel: 'origin/mainnet-beta → working tree',
      reviewId: 'rev-1',
    }) === 'invalid source revision; expected a commit, branch, or tag',
  );
  check('open snapshot without error is null', codeReviewOpenSnapshotError({ reviewId: 'rev-ok', files: [] }) === null);
}

// ---- code_review_commits strict normalization + /commit picker decisions ----
{
  const good = normalizeCodeReviewCommits({
    commits: [commitRowFixture(), commitRowFixture({ commit: COMMIT_SHA64, parentCommit: COMMIT_PARENT, parentCount: 1 })],
    truncated: false,
  });
  check(
    'commits list parses strict rows in order',
    good.ok === true && good.ok && good.list.commits.length === 2 && good.list.commits[0].commit === COMMIT_SHA40 && good.list.commits[1].commit === COMMIT_SHA64,
  );
  check(
    'sha-256 object id and first-parent hash are accepted (not 40-only)',
    good.ok === true && good.ok && good.list.commits[1].parentCommit === COMMIT_PARENT && good.list.commits[1].parentCount === 1,
  );
  const merge = normalizeCodeReviewCommits({ commits: [commitRowFixture({ parentCommit: COMMIT_PARENT, parentCount: 2 })], truncated: true });
  check(
    'merge row keeps parent count 2 and the truncated flag',
    merge.ok === true && merge.ok && merge.list.commits[0].parentCount === 2 && merge.list.truncated === true,
  );
  const empty = normalizeCodeReviewCommits({ commits: [], truncated: false });
  check(
    'empty commits array is a genuine empty success, not a fallback',
    empty.ok === true && empty.ok && empty.list.commits.length === 0 && empty.list.truncated === false,
  );

  const unusableWires = [
    ['non-object payload', null],
    ['array payload', [commitRowFixture()]],
    ['missing commits array', { truncated: false }],
    ['commits not an array', { commits: 'x', truncated: false }],
    ['missing truncated flag', { commits: [] }],
    ['non-boolean truncated', { commits: [], truncated: 'no' }],
    ['all rows malformed', { commits: [{ commit: 'zz' }, 42, null], truncated: false }],
  ];
  for (const [name, wire] of unusableWires) {
    const result = normalizeCodeReviewCommits(wire);
    check(`unusable commits payload fails closed: ${name}`, result.ok === false && !result.ok && result.error.length > 0);
  }

  const malformedRows = [
    ['39-hex id rejected', { commit: COMMIT_SHA40.slice(1) }],
    ['41-hex id rejected', { commit: COMMIT_SHA40 + 'a' }],
    ['non-hex id rejected', { commit: 'g'.repeat(40) }],
    ['uppercase id rejected', { commit: COMMIT_SHA40.toUpperCase() }],
    ['ref-like parent rejected', { parentCommit: 'HEAD~1', parentCount: 1 }],
    ['41-hex parent rejected', { parentCommit: COMMIT_PARENT + 'f', parentCount: 1 }],
    ['parent without count rejected', { parentCommit: COMMIT_PARENT, parentCount: 0 }],
    ['count without parent rejected', { parentCommit: null, parentCount: 1 }],
    ['fractional count rejected', { parentCommit: null, parentCount: 0.5 }],
    ['negative count rejected', { parentCommit: null, parentCount: -1 }],
    ['string count rejected', { parentCommit: null, parentCount: '1' }],
    ['non-string subject rejected', { subject: 7 }],
    ['non-string author rejected', { author: null }],
    ['non-string authorDate rejected', { authorDate: 42 }],
  ];
  for (const [name, over] of malformedRows) {
    const result = normalizeCodeReviewCommits({ commits: [commitRowFixture(over)], truncated: false });
    check(
      `any malformed commit row fails the whole list: ${name}`,
      result.ok === false && !result.ok && result.list === undefined,
      JSON.stringify(result.ok ? result.list : result.error),
    );
  }
  const emptySubject = normalizeCodeReviewCommits({ commits: [commitRowFixture({ subject: '' })], truncated: false });
  check(
    'an empty subject stays valid (git allows it)',
    emptySubject.ok === true && emptySubject.ok && emptySubject.list.commits.length === 1 && emptySubject.list.commits[0].subject === '',
  );
  const mixed = normalizeCodeReviewCommits({
    commits: [commitRowFixture({ commit: 'nope' }), commitRowFixture({ commit: COMMIT_SHA64, parentCommit: COMMIT_PARENT, parentCount: 1 })],
    truncated: false,
  });
  check(
    'one malformed row fails the whole listing — no partial omission beside valid rows',
    mixed.ok === false && !mixed.ok && mixed.error.length > 0,
  );

  check(
    'commits payload omits limit by default',
    JSON.stringify(buildCodeReviewCommitsPayload()) === JSON.stringify({ type: 'code_review_commits' }),
  );
  check('commits payload clamps limit to the 100 cap', buildCodeReviewCommitsPayload(500).limit === CODE_REVIEW_COMMITS_MAX_LIMIT);
  check('commits payload floors fractional limits', buildCodeReviewCommitsPayload(2.9).limit === 2);
  check(
    'commits payload drops non-positive limits',
    !('limit' in buildCodeReviewCommitsPayload(0)) && !('limit' in buildCodeReviewCommitsPayload(-3)),
  );
  check('hash validator accepts 40 and 64 hex only', isValidGitHash(COMMIT_SHA40) && isValidGitHash(COMMIT_SHA64) && !isValidGitHash('HEAD') && !isValidGitHash('a'.repeat(39)) && !isValidGitHash(42));
  check('shortCommitLabel uses the backend 12-hex width', shortCommitLabel(COMMIT_SHA64) === COMMIT_SHA64.slice(0, CODE_REVIEW_COMMIT_LABEL_HEX) && CODE_REVIEW_COMMIT_LABEL_HEX === 12);

  const rows = [
    commitRowFixture(),
    commitRowFixture({ commit: COMMIT_SHA64, parentCommit: COMMIT_PARENT, parentCount: 1, subject: 'fix web regression', author: 'Ada', authorDate: '2026-10-23T09:30:00+02:00' }),
  ];
  check('commit filter empty query returns every row', filterCommitRows(rows, '  ').length === 2);
  const byHash = filterCommitRows(rows, COMMIT_SHA64.slice(0, 6));
  check('commit filter matches a hash prefix', byHash.length === 1 && byHash[0].commit === COMMIT_SHA64);
  check('commit filter matches subject case-insensitively', filterCommitRows(rows, 'PICKER').length === 1);
  const byAuthor = filterCommitRows(rows, 'ada');
  check('commit filter matches author', byAuthor.length === 1 && byAuthor[0].author === 'Ada');
  check('commit filter no match is empty (typed-ref path stays available)', filterCommitRows(rows, 'HEAD~1').length === 0);

  const enterRow = commitPickerEnterAction(rows, 1, 'fix web');
  check('commit picker enter picks the active row full hash', enterRow.kind === 'row' && enterRow.ref === COMMIT_SHA64);
  const enterTyped = commitPickerEnterAction([], -1, ' HEAD~1 ');
  check('commit picker enter with no matching row picks the typed ref trimmed', enterTyped.kind === 'typed' && enterTyped.ref === 'HEAD~1');
  const enterInvalid = commitPickerEnterAction([], -1, 'a b c');
  check('commit picker enter with a multi-token tail errors with the TUI usage line', enterInvalid.kind === 'invalid' && enterInvalid.message === 'Usage: /commit [<ref>]');
  const enterNone = commitPickerEnterAction([], -1, '   ');
  check('commit picker enter with nothing active and nothing typed is a no-op', enterNone.kind === 'none');
  const enterClamped = commitPickerEnterAction(rows, 9, '');
  check('an out-of-range caret falls through to the typed path', enterClamped.kind === 'none');
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
  const onDemandOnly = {
    path: 'lazy.txt',
    hunks: [],
  };
  const pageOnlyThread = {
    identity: {
      snapshotId: 's',
      path: 'lazy.txt',
      oldStart: 5,
      oldCount: 1,
      newStart: 5,
      newCount: 1,
      contentHash: 'h-page',
    },
    comments: [
      { role: 'user', text: 'a', partial: false },
      { role: 'assistant', text: 'b', partial: false },
    ],
    streamingText: '',
    error: null,
    stale: false,
  };
  check('onDemand loaded-only file badge counts threads by path', countFileThreads([pageOnlyThread], onDemandOnly) === 2);
}

// ---- per-comment UTF-8 byte cap (mirrors backend MAX_COMMENT_BYTES) ----
{
  const encoder = new TextEncoder();
  const atCap = '\u{e9}'.repeat(CODE_REVIEW_MAX_COMMENT_BYTES / 2);
  const over = `${atCap}\u{e9}`;
  check('multibyte text at exactly the byte cap passes', encoder.encode(atCap).length === CODE_REVIEW_MAX_COMMENT_BYTES);
  check(
    'one more multibyte unit crosses the byte cap while staying under it in chars',
    encoder.encode(over).length === CODE_REVIEW_MAX_COMMENT_BYTES + 2
      && over.length === atCap.length + 1,
  );
  check('cap constant is byte-strict', CODE_REVIEW_MAX_COMMENT_BYTES === 8192);
}


// ---- isDiffPlaceholder / placeholder LoadedDiff ----
{
  const placeholder = {
    path: 'big3.txt',
    status: 'modified',
    binary: false,
    insertions: 0,
    deletions: 0,
    bodyState: 'onDemand',
    truncated: true,
    hunks: [],
  };
  check('placeholder detected', isDiffPlaceholder(placeholder) === true);
  const partial = {
    ...placeholder,
    bodyState: 'preview',
    hunks: [{ header: '@@', oldStart: 1, oldCount: 1, newStart: 1, newCount: 1, contentHash: 'h', lines: [] }],
  };
  check('preview-cut file is NOT a placeholder', isDiffPlaceholder(partial) === false);
  check('complete empty file is NOT a placeholder', isDiffPlaceholder({ ...placeholder, bodyState: 'complete' }) === false);
  check('binary empty file is NOT a placeholder', isDiffPlaceholder({ ...placeholder, binary: true, bodyState: 'binary' }) === false);

  const loaded = initLoadedDiff('snap-1', placeholder);
  check('placeholder loaded diff starts empty', loaded.lines.length === 0 && loaded.hunks.length === 0);
  check('placeholder eager fetch starts at cursor 0', loaded.nextCursor === 0);
  check('placeholder paging enabled', loaded.hasMoreBackend === true);
  check('placeholder not complete until a page arrives', loaded.hunksComplete === false);
  check('placeholder not byte-capped initially', loaded.byteCapped === false);
  const noPaging = initLoadedDiff('snap-1', { ...placeholder, bodyState: 'complete', truncated: false });
  check('complete file does not page', noPaging.hasMoreBackend === false);
  check('binary files never fetch', initLoadedDiff('snap-1', { ...placeholder, binary: true, bodyState: 'binary' }).hunksComplete === true);
}

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
  check('tree indent matches file-explorer/code-review rows', diffTreePaddingLeft(0) === '8px' && diffTreePaddingLeft(2) === '36px');
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

{
  const file = {
    path: 'small.txt',
    status: 'modified',
    binary: false,
    insertions: 6479,
    deletions: 151,
    truncated: false,
    hunks: [{
      header: '@@ -1,3 +1,3 @@',
      oldStart: 1,
      oldCount: 3,
      newStart: 1,
      newCount: 3,
      contentHash: 'h',
      lines: [
        { kind: 'context', text: 'keep' },
        { kind: 'deletion', text: 'old' },
        { kind: 'addition', text: 'new' },
        { kind: 'context', text: 'ctx' },
      ],
    }],
  };
  const stats = fileChangeStats(file);
  check('hunk stats ignore inflated catalog counts', stats.insertions === 1 && stats.deletions === 1);
  const node = buildFileTree([file]).nodes.find((entry) => entry.path === 'small.txt');
  check('tree file row follows hunk stats', node !== undefined && node.insertions === 1 && node.deletions === 1);
  const loaded = initLoadedDiff('snap-stats', file);
  loaded.hunksComplete = true;
  check('complete loaded diff stats match hunks', fileListChangeStats(file, loaded).insertions === 1 && fileListChangeStats(file, loaded).deletions === 1);
  check('loadedDiffChangeStats counts additions and deletions', loadedDiffChangeStats(loaded).insertions === 1);
  const catalog = {
    path: 'big.txt',
    status: 'modified',
    binary: false,
    insertions: 100,
    deletions: 0,
    bodyState: 'onDemand',
    truncated: false,
    hunks: [],
  };
  const seeded = initLoadedDiff('snap-1', catalog);
  const cappedPage = {
    snapshotId: 'snap-1',
    path: 'big.txt',
    binary: false,
    status: 'modified',
    hunks: [{
      index: 0,
      header: '@@ -1 +1 @@',
      oldStart: 1,
      oldCount: 0,
      newStart: 1,
      newCount: 1,
      contentHash: 'partial',
      totalLines: 1,
      complete: true,
      lineStart: 0,
      lines: [{ kind: 'addition', newNo: 1, text: '+partial' }],
    }],
    cursor: 0,
    hasMore: false,
    totalLines: 1,
    hunkCount: 1,
    byteCapped: true,
  };
  const capped = appendFileDiffPage(seeded, cappedPage);
  check('terminal byte-capped page is complete and capped', capped.hunksComplete === true && capped.byteCapped === true);
  check('byte-capped list stats keep catalog totals',
    fileListChangeStats(catalog, capped, 'snap-1').insertions === 100
      && fileListChangeStats(catalog, capped, 'snap-1').deletions === 0);
  check('byte-capped loaded is not authoritative', loadedDiffAuthoritativeForSnapshot(capped, 'snap-1') === false);
  const cappedTree = buildFileTree([{ ...catalog, ...fileListChangeStats(catalog, capped, 'snap-1') }]);
  const cappedNode = cappedTree.nodes.find((entry) => entry.path === 'big.txt');
  check('byte-capped tree row keeps catalog totals',
    cappedNode !== undefined && cappedNode.insertions === 100 && cappedNode.deletions === 0);
  const stale = { ...capped, snapshotId: 'old-snap', byteCapped: false, hunksComplete: true };
  check('stale loaded snapshot keeps catalog totals',
    fileListChangeStats(catalog, stale, 'snap-1').insertions === 100);
  check('stale loaded is not authoritative for the current snapshot',
    loadedDiffAuthoritativeForSnapshot(stale, 'snap-1') === false);
  check('snapshot byteCapped body keeps catalog counts',
    fileChangeStats({ ...catalog, bodyState: 'byteCapped', hunks: cappedPage.hunks }).insertions === 100);
  const emptyComplete = {
    ...seeded,
    hunksComplete: true,
    hasMoreBackend: false,
    byteCapped: false,
  };
  check('empty complete loaded keeps catalog totals',
    fileListChangeStats(catalog, emptyComplete, 'snap-1').insertions === 100
      && fileListChangeStats(catalog, emptyComplete, 'snap-1').deletions === 0);
  check('empty complete loaded is not authoritative',
    loadedDiffAuthoritativeForSnapshot(emptyComplete, 'snap-1') === false);
  const contextOnly = appendFileDiffPage(seeded, {
    snapshotId: 'snap-1',
    path: 'big.txt',
    binary: false,
    status: 'modified',
    hunks: [{
      index: 0,
      header: '@@ -1 +1 @@',
      oldStart: 1,
      oldCount: 1,
      newStart: 1,
      newCount: 1,
      contentHash: 'ctx',
      totalLines: 1,
      complete: true,
      lineStart: 0,
      lines: [{ kind: 'context', oldNo: 1, newNo: 1, text: ' keep' }],
    }],
    cursor: 0,
    hasMore: false,
    totalLines: 1,
    hunkCount: 1,
    byteCapped: false,
  });
  check('complete loaded with no +/- keeps catalog totals',
    contextOnly.hunksComplete === true
      && contextOnly.hunks.length === 1
      && fileListChangeStats(catalog, contextOnly, 'snap-1').insertions === 100);



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
    byteCapped: false,
  });
  check('page normalized', !!valid);
  check('page byteCapped normalized', normalizeFileDiffPage({ snapshotId: 's', path: 'a', hunks: [], cursor: 0, hasMore: false, totalLines: 0, hunkCount: 0, byteCapped: true }).byteCapped === true);
  check('page fields', valid.snapshotId === 'snap-1' && valid.path === 'a.rs' && valid.cursor === 0);
  check('page nextCursor', valid.nextCursor === 3);
  check('page hasMore', valid.hasMore === true && valid.totalLines === 200);
  check('page hunkCount', valid.hunkCount === 2);
  check('page hunk descriptor', valid.hunks.length === 1 && valid.hunks[0].index === 0 && valid.hunks[0].contentHash === 'cafebabe');
  check('page hunk complete identity', valid.hunks[0].oldStart === 1 && valid.hunks[0].oldCount === 1 && valid.hunks[0].newStart === 1 && valid.hunks[0].newCount === 1);
  check('page hunk window', valid.hunks[0].totalLines === 3 && valid.hunks[0].complete === true && valid.hunks[0].lineStart === 0);
  check('page hunk lines kinds', valid.hunks[0].lines.map((l) => l.kind).join(',') === 'addition,deletion,context');
  check('page line numbers', valid.hunks[0].lines[0].newNo === 1 && valid.hunks[0].lines[1].oldNo === 2);
  check('page no nextCursor when absent', normalizeFileDiffPage({ snapshotId: 's', path: 'a', hunks: [], cursor: 0, hasMore: false, totalLines: 0, hunkCount: 0, byteCapped: false }).nextCursor === undefined);
  check('page missing path -> null', normalizeFileDiffPage({ snapshotId: 's', hunks: [] }) === null);
  check('page missing snapshotId -> null', normalizeFileDiffPage({ path: 'a', hunks: [] }) === null);
  check('page null -> null', normalizeFileDiffPage(null) === null);
  check('page garbage -> null', normalizeFileDiffPage('nope') === null);
  const badStatus = normalizeFileDiffPage({ snapshotId: 's', path: 'a', hunks: [], cursor: 0, hasMore: false, totalLines: 0, hunkCount: 0, byteCapped: false, status: 'weird' });
  const badHunk = normalizeFileDiffPage({ snapshotId: 's', path: 'a', hunks: [{ index: 0, lines: [{ kind: 'bogus', text: 9 }, null] }, null], cursor: 0, hasMore: false, totalLines: 0, hunkCount: 1, byteCapped: false });
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
    bodyState: 'preview',
    truncated: true,
    hunks: [{ header: '@@ -1,2 +1,2 @@', oldStart: 1, oldCount: 2, newStart: 1, newCount: 2, contentHash: 'h1', lines }],
  };
  const loaded = initLoadedDiff('snap-1', file);
  check('loaded snapshot lines', loaded.lines.length === 3 && loaded.hunks.length === 1);
  check('loaded seed hunk structured', loaded.hunks[0].index === 0 && loaded.hunks[0].header === '@@ -1,2 +1,2 @@');
  check('loaded seed hunk identity', loaded.hunks[0].contentHash === 'h1' && loaded.hunks[0].oldCount === 2);
  check('loaded seed hunk complete', loaded.hunks[0].complete === true && loaded.hunks[0].totalLines === 3);
  check('loaded eager fetch starts at cursor 0', loaded.nextCursor === 0);
  check('loaded backend paging enabled', loaded.hasMoreBackend === true);
  check('loaded truncated state', loaded.backendTruncated === true && loaded.totalLines === 3);
  check('loaded not complete until stream consumed', loaded.hunksComplete === false);
  const smallFile = initLoadedDiff('snap-1', { ...file, bodyState: 'complete', truncated: false });
  check('loaded paging off for complete body', smallFile.hasMoreBackend === false);

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
    byteCapped: false,
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
    deletions: 0,
    bodyState: 'onDemand',
    truncated: true,
    hunks: [],
  };
  const loaded = initLoadedDiff('snap-1', placeholder);
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
    byteCapped: false,
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
    deletions: 1,
    bodyState: 'preview',
    truncated: true,
    hunks: [
      { header: '@@ -1,3 +1,3 @@', oldStart: 1, oldCount: 3, newStart: 1, newCount: 3, contentHash: 'partial-hash', lines: seedLines },
    ],
  };
  const loaded = initLoadedDiff('snap-1', file);
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
    byteCapped: false,
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
    bodyState: 'preview',
    truncated: true,
    hunks: [bigHunk],
  });
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
    byteCapped: false,
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
    byteCapped: false,
  });
  check('seed dedupes page-2 subset', p2.lines.length === 8200);
  // A placeholder file (no seed) with the same hunk split across pages:
  // page 1 covers 0..1000, page 2 continues 1000..2000 — pure appends.
  const empty = initLoadedDiff('snap-1', { ...bigHunk, path: 'huge.txt', hunks: [], bodyState: 'onDemand', truncated: true });
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
    byteCapped: false,
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
    byteCapped: false,
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
    byteCapped: true,
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
  const pbFull = planDiffWindow(loadedBackend, 20000);
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
}

{
  const manyLines = (n) => Array.from({ length: n }, (_, i) => ({ kind: 'addition', newNo: i + 1, text: `+l${i}` }));
  const base = (over) => ({
    snapshotId: 's',
    path: 'big.rs',
    hunks: [],
    lines: manyLines(4100),
    hunkCount: 1,
    nextCursor: 4100,
    hasMoreBackend: false,
    totalLines: 4100,
    backendTruncated: true,
    byteCapped: false,
    hunksComplete: true,
    loading: false,
    error: null,
    ...over,
  });

  check('terminal page clears visible truncated', fileDiffStatusView(base(), 4100).kind === 'none');
  check('mid-growth preview shows no status either', fileDiffStatusView(base(), 4000).kind === 'none');

  const capped = fileDiffStatusView(
    base({ byteCapped: true, lines: manyLines(8200), totalLines: 8200 }),
    8200,
  );
  check('byte cap shows the exact reason', capped.kind === 'byte-cap' && capped.text.includes('backend size limit'));
  check('byte cap states the loaded count', capped.text.includes('Loaded 8,200 lines'));
  check('byte cap states the total unknown precisely', capped.text.includes('total line count is unknown'));

  const uiKnown = fileDiffStatusView(base({ lines: manyLines(25000), totalLines: 25000 }), 20000);
  check('ui cap shows the exact reason', uiKnown.kind === 'ui-cap' && uiKnown.text.includes('UI display limit is 20,000 lines'));
  check('ui cap shows Loaded N of M with the true total', uiKnown.text.includes('Loaded 20,000 of 25,000 lines'));

  const uiUnknown = fileDiffStatusView(
    base({ lines: manyLines(20000), totalLines: 20000, hasMoreBackend: true, hunksComplete: false }),
    20000,
  );
  check('ui cap while loading states the total unknown', uiUnknown.kind === 'ui-cap' && uiUnknown.text.includes('total line count is unknown until loading completes'));
  check('ui cap while loading states the loaded count', uiUnknown.text.includes('Loaded 20,000 lines'));
}

{
  check('viewport at the bottom is near', isNearDiffBottom({ scrollTop: 900, clientHeight: 100, scrollHeight: 1000 }) === true);
  check('within the threshold is near', isNearDiffBottom({ scrollTop: 860, clientHeight: 100, scrollHeight: 1000 }) === true);
  check('scrolled away is NOT near', isNearDiffBottom({ scrollTop: 500, clientHeight: 100, scrollHeight: 1000 }) === false);
  check('empty viewport never triggers', isNearDiffBottom({ scrollTop: 0, clientHeight: 0, scrollHeight: 0 }) === false);
}

{
  let loadState = { loading: false, error: null };
  let stalled = false;
  let fetches = 0;
  const growWindow = () => {
    if (loadState.error != null) return; // business error latches; Retry owns retry
    if (stalled) return; // transport stall awaits a new epoch
    if (!shouldEagerFileDiffFetch(loadState, stalled)) return; // in-flight: single-flight join
    fetches += 1;
    loadState = { loading: true, error: null };
  };
  growWindow();
  check('first near-bottom event issues exactly one fetch', fetches === 1);
  growWindow();
  growWindow();
  growWindow();
  check('repeated near-bottom events while loading produce one fetch', fetches === 1);
  check('scrolled away produces no fetch', isNearDiffBottom({ scrollTop: 0, clientHeight: 100, scrollHeight: 5000 }) === false);
  loadState = nextFileDiffLoadState({ kind: 'success' }, loadState);
  growWindow();
  check('next loading cycle may fetch again', fetches === 2);
  loadState = nextFileDiffLoadState({ kind: 'business-error', message: 'git failed' }, loadState);
  growWindow();
  check('latched business error blocks scroll retries', fetches === 2 && loadState.error === 'git failed');
}

{
  const manyLines = (n) => Array.from({ length: n }, (_, i) => ({ kind: 'addition', newNo: i + 1, text: `+l${i}` }));
  const loaded = {
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
  const stalePage = {
    snapshotId: 's',
    path: 'big.rs',
    binary: false,
    status: 'modified',
    hunks: [],
    cursor: 9999, // stale: the stream expects cursor 4000
    nextCursor: 5000,
    hasMore: false,
    totalLines: 5000,
    hunkCount: 1,
    byteCapped: false,
  };
  const stale = appendFileDiffPage(loaded, stalePage);
  check('stale page dropped without growth', stale === loaded && stale.lines.length === 4000);
  check('stale page leaves the window plan unchanged', planDiffWindow(stale, 4000).target === planDiffWindow(loaded, 4000).target);
  check('stale page keeps the stream incomplete', stale.hunksComplete === false);
}
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
  check('omitted isCancellable fails closed', multi.threads.every((t) => t.isCancellable === false));
  check('omitted internalBusy fails closed', multi.internalBusy === false);
  check('internalBusy not inferred from derived live count', reviewHasInternalBusy(multi) === false);

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
  check(
    'abort keys are exact identity fields',
    JSON.stringify(Object.keys(payload).sort()) === JSON.stringify([
      'contentHash',
      'newCount',
      'newStart',
      'oldCount',
      'oldStart',
      'path',
      'snapshotId',
      'type',
    ]),
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

// ---- three-state: live header / Abort / internalBusy close-refresh ----
{
  check('header hidden at 0', formatActiveRepliesLabel(0) === null);
  check('header shows 1 reply', formatActiveRepliesLabel(1) === '1 reply');
  check('close idle+no drafts -> null', formatReviewCloseWarning(false, 0) === null);
  check('close live-only does not warn', formatReviewCloseWarning(false, 0) === null);
  check(
    'close internalBusy warns',
    formatReviewCloseWarning(true, 0) === 'Close anyway? in-flight review work will be discarded.',
  );
  check(
    'close drafts only',
    formatReviewCloseWarning(false, 1) === 'Close anyway? 1 unsent draft will be discarded.',
  );
  check(
    'close internalBusy and drafts',
    formatReviewCloseWarning(true, 2) === 'Close anyway? in-flight review work and 2 unsent drafts will be discarded.',
  );
  check('refresh idle+no drafts -> null', formatReviewRefreshWarning(false, 0) === null);
  check(
    'refresh internalBusy warns',
    formatReviewRefreshWarning(true, 0) === 'Refresh anyway? Review work is still in flight.',
  );
  check(
    'refresh drafts keep stale copy even if busy',
    formatReviewRefreshWarning(true, 1) === 'Refresh anyway? Your 1 draft will be kept, but threads may go stale.',
  );
  check('composer unlocked when idle', reviewComposerLocked(null, 'snap') === false);
  check('composer locked by local abort RPC', reviewComposerLocked('abort', 'snap') === true);
  check('composer locked without snapshot', reviewComposerLocked(null, '') === true);
  check('internalBusy true does not lock composer', reviewComposerLocked(null, 'snap') === false);
  check('threadIsCancellable null', threadIsCancellable(null) === false);
  check('reviewHasInternalBusy null', reviewHasInternalBusy(null) === false);
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

// ---- file-diff fetch latch: sticky business failure, transport suppression ----
// Models the panel's per-path load state machine via the pure helpers it
// calls (nextFileDiffLoadState + shouldEagerFileDiffFetch), proving a
// business error latches inline with zero automatic repeat calls and
// transport failures never render inline nor spin.
{
  const idle = { loading: false, error: null };

  // Successful completion alone clears a latched business error.
  check(
    'success clears a latched business error',
    nextFileDiffLoadState({ kind: 'success' }, { loading: false, error: 'boom' }).error === null,
  );
  check(
    'success sets loading false',
    nextFileDiffLoadState({ kind: 'success' }, { loading: true, error: 'x' }).loading === false,
  );

  // A business failure latches the message inline (and overwrites a prior one
  // so a retry that fails again shows the newest reason).
  check(
    'business-error latches the message',
    nextFileDiffLoadState({ kind: 'business-error', message: 'git acquisition failed' }, { loading: true, error: null }).error === 'git acquisition failed',
  );
  check(
    'business-error sets loading false',
    nextFileDiffLoadState({ kind: 'business-error', message: 'x' }, { loading: true, error: null }).loading === false,
  );
  check(
    'business-error overwrites a prior latched error',
    nextFileDiffLoadState({ kind: 'business-error', message: 'second' }, { loading: false, error: 'first' }).error === 'second',
  );

  // A transport failure suppresses the inline error (App toast owns it).
  check(
    'transport-error suppresses inline error',
    nextFileDiffLoadState({ kind: 'transport-error' }, { loading: true, error: null }).error === null,
  );
  check(
    'transport-error sets loading false',
    nextFileDiffLoadState({ kind: 'transport-error' }, { loading: true, error: null }).loading === false,
  );

  // A stale (superseded) return leaves the latch to the newer owner.
  const prior = { loading: true, error: 'prior' };
  check(
    'stale leaves the latch unchanged',
    nextFileDiffLoadState({ kind: 'stale' }, prior) === prior,
  );

  // Eager gate: idle + not stalled may fetch.
  check('eager fires when idle and not stalled', shouldEagerFileDiffFetch(idle, false) === true);
  // ...never while loading.
  check('eager suppressed while loading', shouldEagerFileDiffFetch({ loading: true, error: null }, false) === false);
  // ...never while a business error is latched (no automatic repeat).
  check('eager suppressed while business error latched', shouldEagerFileDiffFetch({ loading: false, error: 'boom' }, false) === false);
  // ...never while transport-stalled (no spin).
  check('eager suppressed while transport stalled', shouldEagerFileDiffFetch(idle, true) === false);
  // ...again once the error is cleared and the stall lifted (after retry/epoch).
  check('eager re-enabled after clear + unstall', shouldEagerFileDiffFetch({ loading: false, error: null }, false) === true);
}

// ---- sticky business failure: one eager call, zero auto-repeat, one retry ----
// Drives the helper state machine the way the panel does: the eager effect
// calls fetchPagesUntil only while shouldEagerFileDiffFetch holds; a business
// failure latches the error so the eager effect cannot re-issue; an explicit
// Retry (Load more) issues exactly one further fetch.
{
  let state = { loading: false, error: null };
  let stalled = false;
  let eagerCalls = 0;
  let retryCalls = 0;

  // Eager effect fires once for an incomplete, idle, non-stalled file.
  if (shouldEagerFileDiffFetch(state, stalled)) {
    eagerCalls += 1;
    state = { loading: true, error: null };
  }
  // The fetch fails with a business error.
  state = nextFileDiffLoadState({ kind: 'business-error', message: 'git acquisition failed' }, state);

  // The eager effect re-runs on the state change but the latched error gates it.
  if (shouldEagerFileDiffFetch(state, stalled)) {
    eagerCalls += 1;
    state = { loading: true, error: null };
  }
  // A second re-run (e.g. a poll) still cannot re-issue.
  if (shouldEagerFileDiffFetch(state, stalled)) {
    eagerCalls += 1;
    state = { loading: true, error: null };
  }

  // Explicit Retry (Load more): exactly one call, clearing the latch first.
  if (state.error != null) {
    retryCalls += 1;
    state = { loading: true, error: null };
  }
  // The retry fails again — error re-latched, still no auto-repeat.
  state = nextFileDiffLoadState({ kind: 'business-error', message: 'git acquisition failed' }, state);
  if (shouldEagerFileDiffFetch(state, stalled)) {
    eagerCalls += 1;
    state = { loading: true, error: null };
  }

  check('sticky failure: exactly one eager call', eagerCalls === 1, `got ${eagerCalls}`);
  check('sticky failure: exactly one explicit retry', retryCalls === 1, `got ${retryCalls}`);
  check('sticky failure: error stays visible after retry failure', state.error === 'git acquisition failed');
}

// ---- transport suppression + new-epoch retry ----
{
  let state = { loading: false, error: null };
  let stalled = false;
  let eagerCalls = 0;
  let epochRetryCalls = 0;

  if (shouldEagerFileDiffFetch(state, stalled)) {
    eagerCalls += 1;
    state = { loading: true, error: null };
  }
  // Transport failure: no inline error, stall the eager loader.
  state = nextFileDiffLoadState({ kind: 'transport-error' }, state);
  stalled = true;
  const inlineErrorAfterTransport = state.error;

  // While stalled, neither the eager effect nor a poll re-issues.
  if (shouldEagerFileDiffFetch(state, stalled)) {
    eagerCalls += 1;
    state = { loading: true, error: null };
  }
  if (shouldEagerFileDiffFetch(state, stalled)) {
    eagerCalls += 1;
    state = { loading: true, error: null };
  }

  // A new connection epoch clears the stall and retries once.
  stalled = false;
  if (shouldEagerFileDiffFetch(state, stalled)) {
    epochRetryCalls += 1;
    state = { loading: true, error: null };
  }

  check('transport: no inline error', inlineErrorAfterTransport === null);
  check('transport: exactly one eager call before epoch', eagerCalls === 1, `got ${eagerCalls}`);
  check('transport: no spin while stalled', eagerCalls === 1);
  check('transport: new epoch retries exactly once', epochRetryCalls === 1, `got ${epochRetryCalls}`);
}

// ---- restart re-open decision (classifyReviewProbe) ----
// A same-process reconnect preserves a live controller; a real process
// restart whose probe reports no open review reopens once; a transport
// failure of the probe waits for the next epoch (no inline, no spin).
{
  const liveSnapshot = { snapshotId: 'snap-7', files: [], threads: [], truncated: false };
  check(
    'same-process live snapshot -> preserve',
    classifyReviewProbe(liveSnapshot, null).kind === 'preserve',
  );
  check(
    'restart with no open review (empty probe) -> reopen',
    classifyReviewProbe(emptyCodeReviewSnapshot(), null).kind === 'reopen',
  );
  check(
    'restart with null probe data -> reopen',
    classifyReviewProbe(null, null).kind === 'reopen',
  );
  // The backend signals no-open as a typed rpcCode; prose never matches.
  const noReviewProse = Object.assign(new Error('no open code review; call code_review_open first'), { rpc: true });
  check(
    'restart: prose no-open without rpcCode -> preserve',
    classifyReviewProbe(null, noReviewProse).kind === 'preserve',
  );
  const noReviewTyped = Object.assign(new Error('no open code review; call code_review_open first'), {
    rpc: true,
    rpcCode: 'code_review_not_open',
  });
  check(
    'restart: typed no-open rpcCode -> reopen',
    classifyReviewProbe(null, noReviewTyped).kind === 'reopen',
  );
  check('typed no-open matcher ignores prose', isNoOpenCodeReviewError(noReviewProse) === false);
  check('typed no-open matcher accepts rpcCode', isNoOpenCodeReviewError(noReviewTyped) === true);
  check(
    'identity option value is provider/id',
    reviewModelOptionValue('user-steering', 'mock') === 'user-steering/mock'
      && reviewModelOptionValue(null, 'mock') === ''
      && reviewModelOptionValue('user-steering', '') === '',
  );
  check(
    'set_model payload sends the qualified identity',
    buildCodeReviewSetModelPayload('prov/m-1').modelId === 'prov/m-1'
      && buildCodeReviewSetModelPayload('prov/m-1').type === 'code_review_set_model',
  );
  check(
    'auto_review payload omits modelId unless qualified identity is given',
    buildCodeReviewAutoReviewPayload('zh').modelId === undefined
      && buildCodeReviewAutoReviewPayload('zh', 'prov/m-1').modelId === 'prov/m-1',
  );
  // Transport failure of the probe never reopens inline nor spins.
  const transport = new Error('connection closed (code 1006)');
  check(
    'probe transport failure -> wait (no reopen, no inline)',
    classifyReviewProbe(null, transport).kind === 'wait',
  );
  check(
    'probe send-failed transport -> wait',
    classifyReviewProbe(null, new Error('send failed: websocket closed')).kind === 'wait',
  );
  const overload = Object.assign(new Error('server busy: too many concurrent commands'), { rpc: true });
  check(
    'probe slot overload -> preserve without reopen',
    classifyReviewProbe(null, overload).kind === 'preserve',
  );
  const timeout = new Error('code_review_snapshot timed out after 30s (no response from server)');
  check(
    'probe business timeout -> preserve without reopen',
    classifyReviewProbe(null, timeout).kind === 'preserve',
  );
  const genericBusiness = Object.assign(new Error('snapshot unavailable'), { rpc: true });
  check(
    'probe generic business error -> preserve without reopen',
    classifyReviewProbe(null, genericBusiness).kind === 'preserve',
  );
  // A reopen must carry the original one/two revision args: the panel calls
  // openReview() which reads openArgs, so the decision only needs to fire it.
  // Verify the two-revision arg shape round-trips through parseCodeReviewArgs
  // (the open path the panel uses).
  const twoRev = parseCodeReviewArgs('HEAD feature');
  check(
    'reopen path preserves two-revision args',
    twoRev.ok === true && twoRev.from === 'HEAD' && twoRev.to === 'feature',
  );
  const zeroRev = parseCodeReviewArgs('');
  check(
    'reopen path preserves zero-revision (working tree) args',
    zeroRev.ok === true && zeroRev.from === undefined && zeroRev.to === undefined && zeroRev.includeWorkingTree === undefined,
  );
  const oneRev = parseCodeReviewArgs('HEAD~1');
  check(
    'reopen path preserves one-revision base-to-working-tree args',
    oneRev.ok === true && oneRev.from === 'HEAD~1' && oneRev.to === undefined && oneRev.includeWorkingTree === true,
  );
}

// ---- review reply lifecycle: generation owner, terminal precedence ----
{
  const identity = {
    snapshotId: 'snap-life',
    path: 'src/a.ts',
    oldStart: 1,
    oldCount: 1,
    newStart: 1,
    newCount: 1,
    contentHash: 'h-life',
  };
  const key = hunkKey(identity);
  const comments = [
    { role: 'user', text: 'please review', partial: false },
    { role: 'assistant', text: 'done', partial: false },
  ];
  const liveThread = {
    identity,
    comments: [comments[0]],
    streamingText: 'partial…',
    error: null,
    stale: false,
    isStreaming: true,
    generation: 1,
    replyId: 'r1',
    replyStatus: 'streaming',
    toolCalls: [],
  };
  const terminalThread = {
    identity,
    comments,
    streamingText: 'stale leftover',
    error: null,
    stale: false,
    isStreaming: true,
    generation: 1,
    replyId: 'r1',
    replyStatus: 'streaming',
    toolCalls: [],
  };
  const failedThread = {
    identity,
    comments: [comments[0]],
    streamingText: 'almost',
    error: 'provider failed',
    stale: false,
    isStreaming: false,
    generation: 1,
    replyId: 'r1',
    replyStatus: 'failed',
    toolCalls: [],
  };
  const cancelledThread = {
    identity,
    comments: [
      comments[0],
      { role: 'assistant', text: 'aborted mid-reply', partial: true },
    ],
    streamingText: '',
    error: null,
    stale: false,
    isStreaming: false,
    generation: 1,
    replyId: 'r1',
    replyStatus: 'cancelled',
    toolCalls: [],
  };

  function snapFor(thread, extras = {}) {
    return normalizeCodeReviewSnapshot({
      snapshotId: 'snap-life',
      reviewId: 'review-a',
      generation: 1,
      revision: 2,
      files: [],
      threads: [thread],
      ...extras,
    });
  }

  let life = emptyReviewLifecycle();
  life = applyReviewLifecycle(life, { type: 'snapshot', snapshot: snapFor(liveThread) });
  check('snapshot starts one in-flight owner', reviewLifecycleActiveCount(life) === 1);
  const gen = life.threads[key].reply.generation;
  check('first live generation is 1', gen === 1);
  check('reply id from wire', life.threads[key].reply.replyId === 'r1');
  check('live status is streaming', life.threads[key].reply.status === 'streaming');
  const afterFirst = life;
  const delayedEmpty = applyReviewLifecycle(afterFirst, {
    type: 'snapshot',
    snapshot: normalizeCodeReviewSnapshot({
      snapshotId: 'snap-life',
      generation: 1,
      revision: afterFirst.revision,
      files: [],
      threads: [],
    }),
  });
  check('delayed empty same-revision snapshot is ignored', delayedEmpty === afterFirst);
  check('first comment activity retained', reviewLifecycleActiveCount(delayedEmpty) === 1);
  check('first comment Abort retained', delayedEmpty.threads[key].reply.status === 'streaming');

  life = applyReviewLifecycle(life, { type: 'reply-start', key });

  life = applyReviewLifecycle(life, { type: 'delta', key, generation: gen, replyId: 'r1', text: ' more' });
  check('current-generation delta appends', life.threads[key].reply.streamingText.endsWith(' more'));

  life = applyReviewLifecycle(life, {
    type: 'tool-start',
    key,
    generation: gen,
    replyId: 'r1',
    toolCallId: 'call-1',
    toolName: 'read',
  });
  check('tool start marks tool activity', life.threads[key].reply.status === 'tool');
  check('one in-flight tool', life.threads[key].reply.tools.length === 1);
  life = applyReviewLifecycle(life, {
    type: 'tool-start',
    key,
    generation: gen,
    replyId: 'r1',
    toolCallId: 'call-1',
    toolName: 'read',
  });
  check('duplicate tool start is idempotent', life.threads[key].reply.tools.length === 1);
  life = applyReviewLifecycle(life, {
    type: 'tool-end',
    key,
    generation: gen,
    replyId: 'r1',
    toolCallId: 'call-1',
  });
  check('tools completion returns to streaming', life.threads[key].reply.status === 'streaming');
  check('completed tool retained', life.threads[key].reply.tools[0].status === 'completed');
  life = applyReviewLifecycle(life, {
    type: 'tool-start',
    key,
    generation: gen,
    replyId: 'r1',
    toolCallId: 'call-1',
    toolName: 'read',
  });
  check('duplicate start after completed tool is a no-op', life.threads[key].reply.status === 'streaming');
  check('completed tool status unchanged', life.threads[key].reply.tools[0].status === 'completed');


  const beforeComplete = life;
  life = applyReviewLifecycle(life, { type: 'terminal', key, generation: gen, replyId: 'r1', status: 'completed' });
  check('completed is terminal', reviewReplyIsTerminal(life.threads[key].reply.status));
  check('completed not live', !reviewReplyIsLive(life.threads[key].reply.status));
  check('completed clears stream', life.threads[key].reply.streamingText === '');

  const completed = life;
  life = applyReviewLifecycle(life, { type: 'terminal', key, generation: gen, replyId: 'r1', status: 'failed' });
  check('duplicate terminal is idempotent', life === completed);
  life = applyReviewLifecycle(life, { type: 'terminal', key, generation: gen, replyId: 'r1', status: 'cancelled' });
  check('later terminal of same gen cannot override completed', life === completed);
  life = applyReviewLifecycle(life, { type: 'abort', key });
  check('Abort after terminal reply is a no-op', life === completed);
  life = applyReviewLifecycle(life, { type: 'delta', key, generation: gen, replyId: 'r1', text: 'late' });
  check('late same-generation delta cannot resurrect', life === completed);
  life = applyReviewLifecycle(life, { type: 'delta', key, generation: gen - 1, replyId: 'r1', text: 'older' });
  check('late old-generation delta cannot resurrect', life === completed);
  life = applyReviewLifecycle(life, {
    type: 'tool-start',
    key,
    generation: gen,
    replyId: 'r1',
    toolCallId: 'late-tool',
    toolName: 'bash',
  });
  check('late same-generation tool cannot resurrect', life === completed);

  const staleLivePoll = snapFor(terminalThread, { isStreaming: true, activeCount: 1 });
  life = applyReviewLifecycle(life, { type: 'snapshot', snapshot: staleLivePoll });
  check('stale live poll cannot resurrect terminal', !reviewReplyIsLive(life.threads[key].reply.status));
  const overlaid = overlayReviewLifecycle(staleLivePoll, life);
  check('overlay clears Abort after terminal', overlaid.threads[0].isStreaming === false);
  check('overlay clears leftover streamingText', overlaid.threads[0].streamingText === '');
  check('overlay activeCount is 0', overlaid.activeCount === 0 && overlaid.isStreaming === false);
  check('overlay reply count unchanged', overlaid.threads[0].comments.length === comments.length);
  check('threadIsStreaming ignores leftover text', threadIsStreaming(overlaid.threads[0]) === false);

  const delayedBusyPoll = snapFor({
    ...terminalThread,
    isCancellable: true,
  }, { isStreaming: true, activeCount: 1, internalBusy: true });
  const delayedBusyLife = applyReviewLifecycle(life, { type: 'snapshot', snapshot: delayedBusyPoll });
  check('delayed busy poll cannot resurrect terminal owner', !reviewReplyIsLive(delayedBusyLife.threads[key].reply.status));
  check(
    'delayed busy poll cannot raise Abort after terminal',
    delayedBusyLife.threads[key].isCancellable === false,
  );
  const delayedBusyOverlay = overlayReviewLifecycle(delayedBusyPoll, delayedBusyLife);
  check(
    'wire internalBusy stays authoritative while Abort/streaming cannot resurrect',
    delayedBusyOverlay.internalBusy === true
      && reviewHasInternalBusy(delayedBusyOverlay) === true
      && formatReviewCloseWarning(delayedBusyOverlay.internalBusy, 0) !== null
      && delayedBusyOverlay.threads[0].isStreaming === false
      && delayedBusyOverlay.threads[0].isCancellable === false
      && threadIsCancellable(delayedBusyOverlay.threads[0]) === false
      && delayedBusyOverlay.activeCount === 0,
  );

  const delayedTerminalBusyPoll = snapFor({
    ...terminalThread,
    isStreaming: false,
    streamingText: '',
    replyStatus: 'completed',
    isCancellable: true,
  }, { isStreaming: false, activeCount: 0, internalBusy: true });
  const delayedTerminalBusyLife = applyReviewLifecycle(life, { type: 'snapshot', snapshot: delayedTerminalBusyPoll });
  check(
    'delayed terminal poll cannot raise Abort after completed',
    delayedTerminalBusyLife.threads[key].reply.status === 'completed'
      && delayedTerminalBusyLife.threads[key].isCancellable === false,
  );
  const delayedTerminalBusyOverlay = overlayReviewLifecycle(delayedTerminalBusyPoll, delayedTerminalBusyLife);
  check(
    'delayed terminal snapshot keeps wire internalBusy while Abort stays cleared',
    delayedTerminalBusyOverlay.internalBusy === true
      && reviewHasInternalBusy(delayedTerminalBusyOverlay) === true
      && formatReviewCloseWarning(delayedTerminalBusyOverlay.internalBusy, 0) !== null
      && delayedTerminalBusyOverlay.threads[0].isCancellable === false
      && delayedTerminalBusyOverlay.threads[0].isStreaming === false
      && delayedTerminalBusyOverlay.threads[0].replyStatus === 'completed',
  );
  for (const status of ['failed', 'cancelled']) {
    let terminalBusyLife = applyReviewLifecycle(emptyReviewLifecycle(), {
      type: 'snapshot',
      snapshot: snapFor({ ...liveThread, isCancellable: false }),
    });
    terminalBusyLife = applyReviewLifecycle(terminalBusyLife, {
      type: 'terminal',
      key,
      generation: 1,
      replyId: 'r1',
      status,
    });
    const delayedStatusPoll = snapFor({
      ...liveThread,
      isStreaming: false,
      streamingText: '',
      replyStatus: status,
      isCancellable: true,
    }, { isStreaming: false, activeCount: 0, internalBusy: true });
    const delayedStatusLife = applyReviewLifecycle(terminalBusyLife, { type: 'snapshot', snapshot: delayedStatusPoll });
    const delayedStatusOverlay = overlayReviewLifecycle(delayedStatusPoll, delayedStatusLife);
    check(
      `delayed ${status} snapshot keeps wire internalBusy while Abort cannot resurrect`,
      delayedStatusLife.threads[key].reply.status === status
        && delayedStatusLife.threads[key].isCancellable === false
        && delayedStatusOverlay.internalBusy === true
        && delayedStatusOverlay.threads[0].isCancellable === false
        && delayedStatusOverlay.threads[0].isStreaming === false
        && delayedStatusOverlay.threads[0].replyStatus === status,
    );
  }

  const liveBusy = snapFor({ ...liveThread, isCancellable: true }, { isStreaming: true, activeCount: 1, internalBusy: true });
  const liveBusyLife = applyReviewLifecycle(emptyReviewLifecycle(), { type: 'snapshot', snapshot: liveBusy });
  const liveBusyOverlay = overlayReviewLifecycle(liveBusy, liveBusyLife);
  check(
    'overlay preserves conservative busy before terminal',
    reviewReplyIsLive(liveBusyLife.threads[key].reply.status)
      && liveBusyOverlay.internalBusy === true
      && reviewHasInternalBusy(liveBusyOverlay) === true
      && liveBusyOverlay.threads[0].isStreaming === true
      && liveBusyOverlay.threads[0].isCancellable === true
      && liveBusyOverlay.activeCount === 1
      && formatReviewCloseWarning(liveBusyOverlay.internalBusy, 0) !== null,
  );

  const delayedOrphanBusy = overlayReviewLifecycle(delayedBusyPoll, emptyReviewLifecycle());
  check(
    'overlay without a lifecycle owner keeps the wire flag and clears Abort',
    delayedOrphanBusy.internalBusy === true
      && delayedOrphanBusy.threads[0].isStreaming === false
      && delayedOrphanBusy.threads[0].isCancellable === false
      && delayedOrphanBusy.activeCount === 0,
  );

  const reconnectTerminal = snapFor({
    ...terminalThread,
    isStreaming: false,
    streamingText: '',
    replyStatus: 'completed',
  });
  life = applyReviewLifecycle(life, { type: 'snapshot', snapshot: reconnectTerminal });
  check('reconnect terminal snapshot stays terminal', life.threads[key].reply.status === 'completed');
  check('reconnect overlay stays idle', overlayReviewLifecycle(reconnectTerminal, life).activeCount === 0);

  const lifecycleOnlySnapshot = normalizeCodeReviewSnapshot({
    snapshotId: 'snap-life',
    reviewId: 'review-a',
    generation: 1,
    revision: 3,
    files: [],
    threads: [],
  });
  for (const [label, isCancellable] of [['cancellable', true], ['not cancellable', false]]) {
    let terminalLife = applyReviewLifecycle(emptyReviewLifecycle(), {
      type: 'snapshot',
      snapshot: snapFor({ ...liveThread, isCancellable }),
    });
    terminalLife = applyReviewLifecycle(terminalLife, {
      type: 'terminal',
      key,
      generation: 1,
      replyId: 'r1',
      status: 'completed',
    });
    const terminalOverlay = overlayReviewLifecycle(lifecycleOnlySnapshot, terminalLife);
    const lifecycleThread = terminalOverlay.threads[0];
    check(`lifecycle-created terminal thread clears ${label} ownership`, lifecycleThread.replyStatus === 'completed'
      && lifecycleThread.isStreaming === false
      && lifecycleThread.isCancellable === false
      && terminalOverlay.internalBusy === false);
  }

  let absentCancellableLife = applyReviewLifecycle(emptyReviewLifecycle(), {
    type: 'snapshot',
    snapshot: snapFor(liveThread),
  });
  absentCancellableLife = applyReviewLifecycle(absentCancellableLife, {
    type: 'terminal',
    key,
    generation: 1,
    replyId: 'r1',
    status: 'completed',
  });
  check(
    'lifecycle-created thread fails closed when cancellable is absent',
    overlayReviewLifecycle(lifecycleOnlySnapshot, absentCancellableLife).threads[0].isCancellable === false,
  );

  const nextReply = snapFor({
    identity,
    comments: [...comments, { role: 'user', text: 'again', partial: false }],
    streamingText: 'new…',
    error: null,
    stale: false,
    isStreaming: true,
    generation: 1,
    replyId: 'r2',
    replyStatus: 'streaming',
    toolCalls: [],
  });
  life = applyReviewLifecycle(life, { type: 'snapshot', snapshot: { ...nextReply, revision: 5 } });
  check('new replyId opens a new owner', life.threads[key].reply.replyId === 'r2');
  check('new reply is live', reviewReplyIsLive(life.threads[key].reply.status));
  life = applyReviewLifecycle(life, { type: 'delta', key, generation: gen, replyId: 'r1', text: 'old-gen' });
  check('late old-generation delta after new owner cannot resurrect', life.threads[key].reply.streamingText === 'new…');
  life = applyReviewLifecycle(life, { type: 'terminal', key, generation: 1, replyId: 'r2', status: 'completed' });
  const r2Comments = overlayReviewLifecycle({ ...nextReply, revision: 5 }, life).threads[0].comments.length;
  const delayedR1 = snapFor({
    ...terminalThread,
    replyStatus: 'completed',
    isStreaming: false,
    streamingText: '',
  }, { revision: 1 });
  const afterR2 = { ...life, revision: 6 };
  const delayed = applyReviewLifecycle(afterR2, { type: 'snapshot', snapshot: delayedR1 });
  check('delayed r1 snapshot cannot replace r2', delayed.threads[key].reply.replyId === 'r2');
  check('delayed r1 cannot shrink r2 comments', delayed.threads[key].comments.length === r2Comments);

  const pruned = applyReviewLifecycle(life, {
    type: 'snapshot',
    snapshot: normalizeCodeReviewSnapshot({ snapshotId: 'snap-life', generation: 1, revision: 7, files: [], threads: [] }),
  });
  check('snapshot prunes missing hunk keys', pruned.threads[key] === undefined);

  const cutover = applyReviewLifecycle(life, {
    type: 'snapshot',
    snapshot: snapFor(liveThread, { snapshotId: 'snap-new', generation: 2, revision: 8 }),
  });
  check('new snapshot/generation resets lifecycle', cutover.generation === 2);
  check('new generation is live independently', reviewReplyIsLive(cutover.threads[key].reply.status));

  const refreshZero = applyReviewLifecycle(life, {
    type: 'snapshot',
    snapshot: normalizeCodeReviewSnapshot({
      snapshotId: 'snap-refresh',
      generation: 3,
      revision: 0,
      files: [],
      threads: [],
    }),
  });
  check('refresh new id revision0 resets even empty', Object.keys(refreshZero.threads).length === 0);
  check('refresh new id does not overlay old activity', reviewLifecycleActiveCount(refreshZero) === 0);

  const queued = snapFor({
    identity,
    comments: [
      comments[0],
      { role: 'user', text: 'queued', partial: false },
      comments[1],
    ],
    streamingText: '',
    error: null,
    stale: false,
    isStreaming: false,
    generation: 1,
    replyId: 'r9',
    replyStatus: 'completed',
    toolCalls: [],
  }, { revision: 20 });
  let abortLife = applyReviewLifecycle(emptyReviewLifecycle(), { type: 'snapshot', snapshot: queued });
  const abortedShorter = snapFor({
    identity,
    comments: [comments[0], comments[1]],
    streamingText: '',
    error: null,
    stale: false,
    isStreaming: false,
    generation: 1,
    replyId: 'r9',
    replyStatus: 'cancelled',
    toolCalls: [],
  }, { revision: 21 });
  abortLife = applyReviewLifecycle(abortLife, { type: 'snapshot', snapshot: abortedShorter });
  check('newer abort revision replaces shorter comments', abortLife.threads[key].comments.length === 2);
  check('newer abort is cancelled', abortLife.threads[key].reply.status === 'cancelled');

  const completedR1 = applyReviewLifecycle(emptyReviewLifecycle(), {
    type: 'snapshot',
    snapshot: snapFor({
      ...terminalThread,
      isStreaming: false,
      streamingText: '',
      replyStatus: 'completed',
    }, { revision: 30 }),
  });
  const phantom = applyReviewLifecycle(completedR1, { type: 'reply-start', key });
  check('failed comment has no phantom stream until backend r2', phantom.threads[key].reply.status === 'completed');
  check('completed r1 stays r1 until backend assigns r2', phantom.threads[key].reply.replyId === 'r1');
  const phantomAbort = applyReviewLifecycle(completedR1, { type: 'abort', key });
  check('failed abort has no phantom stream', phantomAbort.threads[key].reply.status === 'completed');


  const restartEmpty = applyReviewLifecycle(life, {
    type: 'snapshot',
    snapshot: normalizeCodeReviewSnapshot({
      snapshotId: 'snap-life',
      reviewId: 'review-b',
      generation: 1,
      revision: 0,
      files: [],
      threads: [],
    }),
  });
  check('new reviewId empty restart drops old threads', Object.keys(restartEmpty.threads).length === 0);
  check('new reviewId empty restart is idle', reviewLifecycleActiveCount(restartEmpty) === 0);

  const restartLive = applyReviewLifecycle(life, {
    type: 'snapshot',
    snapshot: snapFor({
      ...liveThread,
      replyId: 'r1',
      replyStatus: 'streaming',
      isStreaming: true,
    }, { reviewId: 'review-b', generation: 1, revision: 0 }),
  });
  check('new reviewId live r1 is not suppressed', restartLive.threads[key].reply.status === 'streaming');
  check('new reviewId live r1 is active', reviewLifecycleActiveCount(restartLive) === 1);
  check('new reviewId does not inherit old comments', restartLive.threads[key].comments.length === liveThread.comments.length);


  let failedLife = emptyReviewLifecycle();
  failedLife = applyReviewLifecycle(failedLife, { type: 'snapshot', snapshot: snapFor(failedThread) });
  check('failed snapshot is terminal', failedLife.threads[key].reply.status === 'failed');
  failedLife = applyReviewLifecycle(failedLife, { type: 'abort', key });
  check('Abort after failed is a no-op', failedLife.threads[key].reply.status === 'failed');

  let cancelledLife = emptyReviewLifecycle();
  cancelledLife = applyReviewLifecycle(cancelledLife, { type: 'snapshot', snapshot: snapFor(cancelledThread) });
  check('cancelled snapshot is terminal', cancelledLife.threads[key].reply.status === 'cancelled');
  cancelledLife = applyReviewLifecycle(cancelledLife, {
    type: 'delta',
    key,
    generation: cancelledLife.threads[key].reply.generation,
    replyId: 'r1',
    text: 'nope',
  });
  check('cancelled rejects late delta', cancelledLife.threads[key].reply.streamingText === '');

  for (const status of ['completed', 'failed', 'cancelled']) {
    const linger = snapFor({
      identity,
      comments: [comments[0]],
      streamingText: 'leftover',
      error: status === 'failed' ? 'provider failed' : null,
      stale: false,
      isStreaming: false,
      isCancellable: true,
      generation: 1,
      replyId: 'r1',
      replyStatus: status,
      toolCalls: [],
    }, { isStreaming: false, activeCount: 0, internalBusy: true });
    const lingerLife = applyReviewLifecycle(emptyReviewLifecycle(), { type: 'snapshot', snapshot: linger });
    const lingerOverlay = overlayReviewLifecycle(linger, lingerLife);
    check(
      `${status} linger stays cancellable`,
      lingerOverlay.threads[0].replyStatus === status
        && lingerOverlay.threads[0].isStreaming === false
        && threadIsCancellable(lingerOverlay.threads[0]) === true
        && lingerOverlay.activeCount === 0
        && formatActiveRepliesLabel(lingerOverlay.activeCount) === null
        && reviewHasInternalBusy(lingerOverlay) === true
        && formatReviewCloseWarning(lingerOverlay.internalBusy, 0) !== null
        && reviewComposerLocked(null, lingerOverlay.snapshotId) === false,
    );
  }

  const settled = snapFor({
    identity,
    comments,
    streamingText: '',
    error: null,
    stale: false,
    isStreaming: false,
    isCancellable: false,
    generation: 1,
    replyId: 'r1',
    replyStatus: 'completed',
    toolCalls: [],
  }, { isStreaming: false, activeCount: 0, internalBusy: false });
  const settledLife = applyReviewLifecycle(emptyReviewLifecycle(), { type: 'snapshot', snapshot: settled });
  const settledOverlay = overlayReviewLifecycle(settled, settledLife);
  check(
    'settled snapshot clears Abort and internalBusy',
    settledOverlay.threads[0].isCancellable === false
      && threadIsCancellable(settledOverlay.threads[0]) === false
      && reviewHasInternalBusy(settledOverlay) === false
      && settledOverlay.activeCount === 0
      && formatActiveRepliesLabel(settledOverlay.activeCount) === null
      && formatReviewCloseWarning(settledOverlay.internalBusy, 0) === null
      && formatReviewRefreshWarning(settledOverlay.internalBusy, 0) === null,
  );


  let abortLive = emptyReviewLifecycle();
  abortLive = applyReviewLifecycle(abortLive, { type: 'snapshot', snapshot: snapFor(liveThread) });
  abortLive = applyReviewLifecycle(abortLive, { type: 'abort', key });
  check('abort of live reply is cancelled', abortLive.threads[key].reply.status === 'cancelled');
  check('abort of live reply is not active', reviewLifecycleActiveCount(abortLive) === 0);

  check('threadIsStreaming null still false', threadIsStreaming(null) === false);
  check('beforeComplete was live', reviewReplyIsLive(beforeComplete.threads[key].reply.status));
}


{
  const mkLoaded = (path, lineCount, textLen) => {
    const file = {
      path,
      status: 'modified',
      binary: false,
      insertions: lineCount,
      deletions: 0,
      bodyState: 'complete',
      truncated: false,
      hunks: [
        {
          header: '@@ -1 +1 @@',
          oldStart: 1,
          oldCount: 1,
          newStart: 1,
          newCount: 1,
          contentHash: `hash-${path}`,
          lines: Array.from({ length: lineCount }, (_, i) => ({
            kind: 'addition',
            newNo: i + 1,
            text: `+${'x'.repeat(Math.max(0, textLen - 1))}`,
          })),
        },
      ],
    };
    return initLoadedDiff('snap-1', file);
  };

  const est = mkLoaded('est.txt', 3, 7);
  const estBase = 256 + 2 * ('snap-1'.length + 'est.txt'.length);
  const estStructural = 128 + 3 * 80;
  check(
    'estimator charges the retained-object base plus 2x string length and structural costs',
    estimateLoadedDiffBytes(est)
      === estStructural
        + estBase
        + 2 * (est.hunks[0].header.length + est.hunks[0].contentHash.length + 3 * 7),
  );
  const rawUtf16 =
    est.hunks[0].header.length +
    est.hunks[0].contentHash.length +
    est.hunks[0].lines.reduce((sum, line) => sum + line.text.length, 0);
  check(
    'estimator is at least twice the raw UTF-16 length',
    estimateLoadedDiffBytes(est) >= 2 * rawUtf16,
  );
  const emptyEstimate = estimateLoadedDiffBytes(
    initLoadedDiff('snap-1', { path: 'e.txt', status: 'modified', binary: false, insertions: 0, deletions: 0, bodyState: 'onDemand', truncated: true, hunks: [] }),
  );
  check('a retained zero-hunk loaded diff still weighs a positive base', emptyEstimate > 0, String(emptyEstimate));
  check(
    'empty-diff weight charges the top-level strings conservatively',
    emptyEstimate >= 2 * ('snap-1'.length + 'e.txt'.length),
  );

  const uni = initLoadedDiff('snap-1', {
    path: 'uni.txt',
    status: 'modified',
    binary: false,
    insertions: 2,
    deletions: 0,
    bodyState: 'complete',
    truncated: false,
    hunks: [
      {
        header: '@@ -1 +1 @@',
        oldStart: 1,
        oldCount: 1,
        newStart: 1,
        newCount: 1,
        contentHash: 'hash-uni',
        lines: [
          { kind: 'addition', newNo: 1, text: 'λ'.repeat(10) },
          { kind: 'addition', newNo: 2, text: 'λ'.repeat(10) },
        ],
      },
    ],
  });
  check(
    'non-Latin text charges 2 bytes per UTF-16 unit plus structures',
    estimateLoadedDiffBytes(uni)
      === 256 + 2 * ('snap-1'.length + 'uni.txt'.length)
        + 128
        + 2 * (uni.hunks[0].header.length + uni.hunks[0].contentHash.length)
        + 2 * (80 + 2 * 10),
  );


  let c1 = emptyLoadedDiffCache();
  for (let i = 1; i <= LOADED_DIFF_CACHE_MAX_ENTRIES; i++) {
    c1 = loadedDiffCachePut(c1, mkLoaded(`f${i}.txt`, 1, 10), { currentPath: `f${i}.txt` }).cache;
  }
  check('entry cap retains everything under the cap', Object.keys(c1.entries).length === LOADED_DIFF_CACHE_MAX_ENTRIES);
  const c1r = loadedDiffCachePut(c1, mkLoaded('f9.txt', 1, 10), { currentPath: 'f9.txt' });
  check('entry cap evicts exactly the oldest entry', c1r.evicted.length === 1 && c1r.evicted[0] === 'f1.txt');
  check('entry cap keeps size bounded after eviction', Object.keys(c1r.cache.entries).length === LOADED_DIFF_CACHE_MAX_ENTRIES && c1r.cache.entries['f1.txt'] === undefined && c1r.cache.entries['f9.txt'] !== undefined);
  check('eviction follows recency order', c1r.cache.order[0] === 'f2.txt' && c1r.cache.order[c1r.cache.order.length - 1] === 'f9.txt');

  const t3 = loadedDiffCacheTouch(c1r.cache, 'f3.txt', { currentPath: 'f3.txt' });
  check('touch moves path to MRU without eviction', t3.evicted.length === 0 && t3.cache.order[t3.cache.order.length - 1] === 'f3.txt');
  const c1t = loadedDiffCachePut(t3.cache, mkLoaded('f10.txt', 1, 10), { currentPath: 'f10.txt' });
  check('touched entry survives, untouched oldest evicts', c1t.evicted.length === 1 && c1t.evicted[0] === 'f2.txt' && c1t.cache.entries['f3.txt'] !== undefined);

  let c2 = emptyLoadedDiffCache();
  for (let i = 1; i <= LOADED_DIFF_CACHE_MAX_ENTRIES; i++) {
    c2 = loadedDiffCachePut(c2, mkLoaded(`g${i}.txt`, 1, 10), { currentPath: `g${i}.txt` }).cache;
  }
  const c2r = loadedDiffCachePut(c2, mkLoaded('g9.txt', 1, 10), { currentPath: 'g1.txt' });
  check('current path is never evicted', c2r.cache.entries['g1.txt'] !== undefined && !c2r.evicted.includes('g1.txt'));
  check('protection shifts eviction to the next oldest', c2r.evicted.length === 1 && c2r.evicted[0] === 'g2.txt' && Object.keys(c2r.cache.entries).length === LOADED_DIFF_CACHE_MAX_ENTRIES);

  const MIB = 1024 * 1024;
  const bigLoaded = (path, mib) => mkLoaded(path, 64, Math.floor((mib * MIB) / 64) - 1);
  const heavyEntry = bigLoaded('w.txt', 3.5);
  const heavyWeight = estimateLoadedDiffBytes(heavyEntry);
  check(
    'heavy entry weight is the retained base plus 2x content plus structures',
    heavyWeight
      === 256 + 2 * ('snap-1'.length + 'w.txt'.length)
        + 150
        + 2 * heavyEntry.hunks[0].contentHash.length
        + 64 * (80 + 2 * (Math.floor((3.5 * MIB) / 64) - 1)),
  );
  let c3 = emptyLoadedDiffCache();
  let c3evicted = [];
  for (const name of ['b1', 'b2', 'b3', 'b4']) {
    const r = loadedDiffCachePut(c3, bigLoaded(name, 3.5), { currentPath: name });
    c3 = r.cache;
    c3evicted = c3evicted.concat(r.evicted);
  }
  const c3weight = c3.order.reduce((sum, p) => sum + estimateLoadedDiffBytes(c3.entries[p]), 0);
  check('four heavy entries fit under the byte cap', Object.keys(c3.entries).length === 4 && c3evicted.length === 0 && c3weight <= LOADED_DIFF_CACHE_MAX_BYTES);
  const c3r = loadedDiffCachePut(c3, bigLoaded('b5', 3.5), { currentPath: 'b5' });
  check('byte cap evicts the oldest heavy entry', c3r.evicted.length === 1 && c3r.evicted[0] === 'b1');
  const c3total = c3r.cache.order.reduce((sum, p) => sum + estimateLoadedDiffBytes(c3r.cache.entries[p]), 0);
  check('byte cap total is bounded', c3total <= LOADED_DIFF_CACHE_MAX_BYTES && Object.keys(c3r.cache.entries).length === 4 && c3r.cache.entries.b1 === undefined);

  const oversize = bigLoaded('huge.bin', 40);
  const o1 = loadedDiffCachePut(emptyLoadedDiffCache(), oversize, { currentPath: 'huge.bin' });
  check('oversize current retained while selected', o1.evicted.length === 0 && o1.cache.entries['huge.bin'] !== undefined && estimateLoadedDiffBytes(o1.cache.entries['huge.bin']) > LOADED_DIFF_CACHE_MAX_BYTES);
  const o2 = loadedDiffCachePut(o1.cache, mkLoaded('small.txt', 1, 10), { currentPath: 'small.txt' });
  check('oversize ex-current evicted after selection moved away', o2.evicted.includes('huge.bin') && o2.cache.entries['huge.bin'] === undefined && o2.cache.entries['small.txt'] !== undefined);

  let c4 = emptyLoadedDiffCache();
  for (let i = 1; i <= LOADED_DIFF_CACHE_MAX_ENTRIES; i++) {
    c4 = loadedDiffCachePut(c4, mkLoaded(`h${i}.txt`, 1, 10), { currentPath: `h${i}.txt` }).cache;
  }
  const c4r = loadedDiffCachePut(c4, mkLoaded('h9.txt', 1, 10), { currentPath: 'h9.txt', inFlightPaths: ['h1.txt'] });
  check('in-flight path pinned against eviction', c4r.cache.entries['h1.txt'] !== undefined && !c4r.evicted.includes('h1.txt'));
  check('in-flight pin shifts eviction to the next oldest', c4r.evicted.length === 1 && c4r.evicted[0] === 'h2.txt');

  let c5 = emptyLoadedDiffCache();
  const start = mkLoaded('a.txt', 2, 4);
  c5 = loadedDiffCachePut(c5, start, { currentPath: 'a.txt' }).cache;
  for (let i = 1; i <= LOADED_DIFF_CACHE_MAX_ENTRIES; i++) {
    c5 = loadedDiffCachePut(c5, mkLoaded(`z${i}.txt`, 1, 10), { currentPath: `z${i}.txt` }).cache;
  }
  check('browsing filled the cache and evicted a.txt', c5.entries['a.txt'] === undefined);
  const stalePage = {
    snapshotId: 'snap-1',
    path: 'a.txt',
    binary: false,
    hunks: [],
    cursor: 999,
    hasMore: false,
    totalLines: 2,
    hunkCount: 1,
    byteCapped: false,
  };
  check('stale page rejected by the merge guards', appendFileDiffPage(start, stalePage) === start);
  const c5t = loadedDiffCacheTouch(c5, 'a.txt', { currentPath: 'z8.txt' });
  check('touch of evicted path does not resurrect', c5t.cache === c5 && c5t.evicted.length === 0 && c5t.cache.entries['a.txt'] === undefined);
  const refile = {
    path: 'a.txt',
    status: 'modified',
    binary: false,
    insertions: 0,
    deletions: 0,
    bodyState: 'onDemand',
    truncated: true,
    hunks: [],
  };
  const reinit = initLoadedDiff('snap-1', refile);
  check('re-select reinitializes evicted entry for refetch', reinit.nextCursor === 0 && reinit.hasMoreBackend === true && reinit.lines.length === 0);
  const c5r = loadedDiffCachePut(c5, reinit, { currentPath: 'a.txt' });
  check('re-select re-adds the evicted entry', c5r.cache.entries['a.txt'] === reinit && !c5r.evicted.includes('a.txt'));
}

{
  const hunk = {
    header: '@@ -40,8 +40,8 @@',
    oldStart: 40,
    oldCount: 8,
    newStart: 40,
    newCount: 8,
    contentHash: 'abc',
    lines: [
      { kind: 'context', oldNo: 40, newNo: 40, text: 'keep' },
      { kind: 'addition', newNo: 41, text: 'new' },
    ],
  };
  const gapsUnknownEof = reviewDiffGaps([hunk], 0, 0);
  check('gap above hunk at line 40 without file length', gapsUnknownEof.length === 1 && gapsUnknownEof[0].newStart === 1 && gapsUnknownEof[0].newCount === 39);
  const gaps = reviewDiffGaps([hunk], 80, 80);
  check('gap above hunk remains when file length is known', gaps[0].newStart === 1 && gaps[0].newCount === 39);
  check('gap below last hunk uses file length', gaps[1].newStart === 48 && gaps[1].newCount === 33);
  let state = newGapExpandState(gaps[0]);
  const first = gapExpandWindow(remainingDiffGap(state), 'end', CONTEXT_EXPAND_LINES);
  check('first expand reads the 20 lines against the hunk', !!first && first.newStart === 20 && first.count === 20);
  state = recordGapExpand(state, 'end', first.count);
  const second = gapExpandWindow(remainingDiffGap(state), 'end', CONTEXT_EXPAND_LINES);
  check('second expand continues upward', !!second && second.newStart === 1 && second.count === 19);
  const file = {
    path: 'a.rs',
    status: 'modified',
    binary: false,
    insertions: 1,
    deletions: 0,
    hunks: [hunk],
  };
  const keyBefore = hunkKeyFor(file, hunk);
  const incoming = Array.from({ length: 20 }, (_, i) => ({
    kind: 'context',
    oldNo: 20 + i,
    newNo: 20 + i,
    text: `line-${20 + i}`,
  }));
  const merged = mergeExpandedLines([], incoming, 'end');
  check('expanded context stays outside the hunk', merged.length === 20 && hunk.lines.length === 2);
  check('expand does not change hunk identity', hunkKeyFor(file, hunk) === keyBefore);
  // Mixed-direction expansion renders in SOURCE order, not arrival order:
  // 'end' window (20-39) consumed first, then 'start' (1-10) must interleave
  // before it, never after.
  const later = incoming.map((line, i) => ({ ...line, newNo: 20 + i, oldNo: 20 + i }));
  const earlier = Array.from({ length: 10 }, (_, i) => ({
    kind: 'context',
    oldNo: 1 + i,
    newNo: 1 + i,
    text: `top-${1 + i}`,
  }));
  const mixed = mergeExpandedLines(later, earlier, 'start');
  check('mixed-direction merge stays in source order', mixed[0].newNo === 1 && mixed[9].newNo === 10 && mixed[10].newNo === 20 && mixed[mixed.length - 1].newNo === 39);
  const page = normalizeFileContextPage({
    snapshotId: 'snap',
    path: 'a.rs',
    oldLineCount: 80,
    newLineCount: 80,
    binary: false,
    lines: incoming,
  });
  check('context page normalizes snapshot-scoped lines', !!page && page.lines[0].newNo === 20 && page.lines[0].kind === 'context');
  check('malformed context page is rejected', normalizeFileContextPage({ path: 'a.rs' }) === null);
  // Chunked end-window expansion must tile the gap exactly once: the loop
  // planner clamps every window to the 20-line request bound BEFORE planning
  // (the server truncates count but keeps the requested start), so oversized
  // plans can never duplicate or omit windows later.
  {
    const gap = { oldStart: 1, oldCount: 39, newStart: 1, newCount: 39 };
    let chunkState = newGapExpandState(gap);
    const loaded: Array<{ kind: string; oldNo: number | null; newNo: number | null; text: string }> = [];
    for (let safety = 0; safety < 10; safety++) {
      const remaining = remainingDiffGap(chunkState);
      if (remaining.oldCount === 0 && remaining.newCount === 0) break;
      const span = Math.max(remaining.newCount, remaining.oldCount);
      const plan = gapExpandWindow(remaining, 'end', Math.min(CONTEXT_EXPAND_LINES, span));
      check('chunk plan never exceeds the 20-line request bound', !!plan && plan.count <= CONTEXT_EXPAND_LINES);
      for (let i = 0; i < (plan?.count ?? 0); i++) {
        loaded.push({ kind: 'context', oldNo: (plan?.oldStart ?? 0) + i, newNo: (plan?.newStart ?? 0) + i, text: `g-${(plan?.newStart ?? 0) + i}` });
      }
      chunkState = recordGapExpand(chunkState, 'end', plan?.count ?? 0);
    }
    const newNos = loaded.map((line) => line.newNo ?? 0);
    const tiling = new Set(newNos).size === 39 && Math.min(...newNos) === 1 && Math.max(...newNos) === 39 && newNos.length === 39;
    check('chunked end expansion covers 1..39 exactly once', tiling);
  }
  const between = reviewDiffGaps(
    [
      { header: '@@ -10,3 +10,3 @@', oldStart: 10, oldCount: 3, newStart: 10, newCount: 3, contentHash: 'a', lines: [] },
      { header: '@@ -40,3 +40,3 @@', oldStart: 40, oldCount: 3, newStart: 40, newCount: 3, contentHash: 'b', lines: [] },
    ],
    0,
    0,
  );
  check('between-hunk gap without file length', between.length === 2 && between[1].newStart === 13 && between[1].newCount === 27);
  const deletedGaps = reviewDiffGaps(
    [{ header: '@@ -40,8 +0,0 @@', oldStart: 40, oldCount: 8, newStart: 0, newCount: 0, contentHash: 'd', lines: [] }],
    80,
    0,
  );
  check(
    'deleted-file gaps stay on the old side',
    deletedGaps.length === 2 && deletedGaps[0].oldCount === 39 && deletedGaps[0].newCount === 0 && deletedGaps[1].oldStart === 48 && deletedGaps[1].newCount === 0,
  );
  const addedGaps = reviewDiffGaps(
    [{ header: '@@ -0,0 +20,8 @@', oldStart: 0, oldCount: 0, newStart: 20, newCount: 8, contentHash: 'c', lines: [] }],
    0,
    50,
  );
  check(
    'added-file gaps stay on the new side',
    addedGaps.length === 2 && addedGaps[0].oldCount === 0 && addedGaps[0].newStart === 1 && addedGaps[0].newCount === 19 && addedGaps[1].newStart === 28 && addedGaps[1].newCount === 23,
  );
  let clamped = newGapExpandState(gaps[0]);
  clamped = recordGapExpand(clamped, 'end', 100);
  check('record clamps to the remaining gap span', clamped.fromEnd === 39 && remainingDiffGap(clamped).newCount === 0);
  const remainingWindow = gapExpandWindow(remainingDiffGap(newGapExpandState(gaps[0])), 'end', 39);
  check('remaining-button window fills the gap', !!remainingWindow && remainingWindow.count === 39 && remainingWindow.newStart === 1);
  const staleState = newGapExpandState(gaps[0]);
  const firstPlan = gapExpandWindow(remainingDiffGap(staleState), 'end', CONTEXT_EXPAND_LINES);
  const duplicatePlan = gapExpandWindow(remainingDiffGap(staleState), 'end', CONTEXT_EXPAND_LINES);
  check('parallel plans from a stale store repeat the same window', !!firstPlan && !!duplicatePlan && firstPlan.newStart === duplicatePlan.newStart && firstPlan.count === duplicatePlan.count);
  const serialState = recordGapExpand(staleState, 'end', firstPlan ? firstPlan.count : 0);
  const serialPlan = gapExpandWindow(remainingDiffGap(serialState), 'end', CONTEXT_EXPAND_LINES);
  check('serialized second plan continues upward', !!serialPlan && serialPlan.newStart === 1 && serialPlan.count === 19);
}


// ---------------------------------------------------------------------------
// Wave 2 — files-column width helpers (mirror of the thread-width contract).
// ---------------------------------------------------------------------------

check('files width storage key is the one boring key', CODE_REVIEW_FILES_WIDTH_STORAGE_KEY === 'rpi-code-review-files-width');
check('files width bounds/step/default match the frozen contract', CODE_REVIEW_FILES_WIDTH_MIN === 180
  && CODE_REVIEW_FILES_WIDTH_MAX === 480
  && CODE_REVIEW_FILES_WIDTH_DEFAULT === 224
  && CODE_REVIEW_FILES_WIDTH_STEP === 24);

check('files clamp keeps in-range values', clampCodeReviewFilesWidth(300) === 300);
check('files clamp raises below the floor', clampCodeReviewFilesWidth(10) === 180);
check('files clamp caps above the ceiling', clampCodeReviewFilesWidth(100000) === 480);
check('files clamp rounds fractional drag widths', clampCodeReviewFilesWidth(300.6) === 301);
check('files clamp maps NaN to the default', clampCodeReviewFilesWidth(Number.NaN) === 224);
check('files clamp never returns a non-integer', Number.isInteger(clampCodeReviewFilesWidth(300.5)));

{
  const store = new Map<string, string>();
  const storage = {
    getItem: (key) => (store.has(key) ? store.get(key) : null),
    setItem: (key, value) => { store.set(key, String(value)); },
  };
  check('files width empty store reads as the default', readStoredCodeReviewFilesWidth(storage) === 224);
  check('files width null storage reads as the default', readStoredCodeReviewFilesWidth(null) === 224);
  writeStoredCodeReviewFilesWidth(storage, 400);
  check('files width round-trips the clamped number', store.get(CODE_REVIEW_FILES_WIDTH_STORAGE_KEY) === '400' && readStoredCodeReviewFilesWidth(storage) === 400);
  writeStoredCodeReviewFilesWidth(storage, 100000);
  check('files width write clamps above the ceiling', store.get(CODE_REVIEW_FILES_WIDTH_STORAGE_KEY) === '480');
  writeStoredCodeReviewFilesWidth(storage, 5);
  check('files width write clamps below the floor', store.get(CODE_REVIEW_FILES_WIDTH_STORAGE_KEY) === '180');
  store.set(CODE_REVIEW_FILES_WIDTH_STORAGE_KEY, 'garbage');
  check('files width read maps corrupt payloads to the default', readStoredCodeReviewFilesWidth(storage) === 224);
  store.clear();
  const blocked = {
    getItem: () => { throw new Error('private mode'); },
    setItem: () => { throw new Error('private mode'); },
  };
  check('files width blocked storage reads as the default', readStoredCodeReviewFilesWidth(blocked) === 224);
  let blockedWriteThrew = false;
  try {
    writeStoredCodeReviewFilesWidth(blocked, 400);
  } catch {
    blockedWriteThrew = true;
  }
  check('files width blocked storage on write does not throw', !blockedWriteThrew);
}

check('files step grows by one step', stepCodeReviewFilesWidth(280, 1) === 304);
check('files step shrinks by one step', stepCodeReviewFilesWidth(280, -1) === 256);
check('files step clamps at the floor', stepCodeReviewFilesWidth(190, -1) === 180);
check('files step clamps at the ceiling', stepCodeReviewFilesWidth(470, 1) === 480);

// ---------------------------------------------------------------------------
// Auto comment flags and batch wire normalization.
// ---------------------------------------------------------------------------

const autoThreadWire = (path, over = {}) => ({
  identity: { snapshotId: 'snap-1', path, oldStart: 1, oldCount: 1, newStart: 2, newCount: 1, contentHash: 'h' },
  comments: [{ role: 'user', text: 'auto', partial: false, ...(over.autoComment === true ? { auto: true } : {}) }],
  isStreaming: false,
});


{
  const snap = normalizeCodeReviewSnapshot({
    threads: [
      autoThreadWire('a.ts', { autoComment: true }),
      autoThreadWire('b.ts'),
    ],
    comments: undefined,
  });
  const autoComment = snap.threads[0].comments[0];
  const manualComment = snap.threads[1].comments[0];
  check('auto flag survives on auto user comments', autoComment.auto === true);
  check('auto flag absent on manual comments', manualComment.auto === undefined);
  check('comment auto flag absent when wire omits it', normalizeCodeReviewSnapshot({
    threads: [autoThreadWire('a.ts')],
  }).threads[0].comments[0].auto === undefined);
}

{
  // Snapshot-level defaults: idle state, absent counts, wire tolerance.
  check('empty snapshot defaults auto state to idle', emptyCodeReviewSnapshot().autoReviewState === 'idle');
  check('empty snapshot leaves auto counts absent', emptyCodeReviewSnapshot().autoReviewCounts.submitted === null
    && emptyCodeReviewSnapshot().autoReviewCounts.completed === null
    && emptyCodeReviewSnapshot().autoReviewCounts.failed === null);
  const defaults = normalizeCodeReviewSnapshot({});
  check('missing wire auto state defaults to idle', defaults.autoReviewState === 'idle');
  check('missing wire auto counts stay absent', defaults.autoReviewCounts.submitted === null && defaults.autoReviewCounts.completed === null && defaults.autoReviewCounts.failed === null);
  check('explicit zero counts stay distinct from absence', normalizeCodeReviewSnapshot({
    autoReviewCounts: { submitted: 0, completed: 0, failed: 0 },
  }).autoReviewCounts.submitted === 0);
  const invalid = normalizeCodeReviewSnapshot({ autoReviewState: 'paused', autoReviewCounts: { submitted: -2, completed: 'x', failed: 0 } });
  check('unknown wire auto state falls back to idle', invalid.autoReviewState === 'idle');
  check('malformed wire auto counts stay absent', invalid.autoReviewCounts.submitted === null && invalid.autoReviewCounts.completed === null && invalid.autoReviewCounts.failed === null);
  const running = normalizeCodeReviewSnapshot({
    autoReviewState: 'running',
    autoReviewCounts: { submitted: 5, completed: 2, failed: 1 },
    threads: [autoThreadWire('src/a.rs')],
  });
  check('running state and counts normalize through', running.autoReviewState === 'running'
    && running.autoReviewCounts.submitted === 5
    && running.autoReviewCounts.completed === 2
    && running.autoReviewCounts.failed === 1);

  // The lifecycle overlay preserves batch state/counts and manual identities.
  const life = applyReviewLifecycle(emptyReviewLifecycle(), { type: 'snapshot', snapshot: running });
  const overlaid = overlayReviewLifecycle(running, life);
  check('overlay preserves autoReviewState', overlaid.autoReviewState === 'running');
  check('overlay preserves autoReviewCounts', overlaid.autoReviewCounts.submitted === 5 && overlaid.autoReviewCounts.completed === 2);
  check('overlay preserves manual thread identity', overlaid.threads[0].identity.path === 'src/a.rs');
}

// Precise findings reject the entire snapshot on any malformed result.
{
  const identity = { snapshotId: 'snap-h', path: 'src/a.ts', oldStart: 1, oldCount: 2, newStart: 1, newCount: 2, contentHash: 'hash-a' };
  const finding = { category: 'critical', side: 'new', startLine: 1, endLine: 2, summary: 'bounds check', evidence: 'unchecked index' };
  const result = { identity, findings: [finding] };
  const normalize = (reviewFindings) => normalizeCodeReviewSnapshot({ reviewFindings });
  const snap = normalize([result]);
  check('exact seven-field identity survives', JSON.stringify(snap.reviewFindings?.[0].identity) === JSON.stringify(identity));
  check('precise side range and evidence survive', JSON.stringify(snap.reviewFindings?.[0].findings[0]) === JSON.stringify(finding));
  check('missing findings remain unreviewed', normalizeCodeReviewSnapshot({}).reviewFindings === null);
  check('reviewed clean remains distinct', normalize([{ identity, findings: [] }]).reviewFindings?.[0].findings.length === 0);
  for (const malformed of [null, {}, { ...finding, category: 'clean' }, { ...finding, side: 'both' }, { ...finding, startLine: 0 }, { ...finding, endLine: 3 }, { ...finding, endLine: 0 }, { ...finding, extra: true }, { ...finding, evidence: null }, { ...finding, summary: 'x'.repeat(CODE_REVIEW_MAX_FINDING_SUMMARY_BYTES + 1) }]) {
    check('malformed finding rejects the whole snapshot', normalize([{ identity, findings: [finding, malformed] }]).error === 'Invalid review findings');
  }
  for (const malformed of ['nope', {}, [null], [result, result], [{ ...result, extra: true }], [{ identity: { ...identity, snapshotId: '' }, findings: [finding] }], [{ identity: { ...identity, newCount: 0 }, findings: [finding] }], [{ identity, findings: [finding, finding] }]]) {
    check('malformed result rejects rather than truncates', normalize(malformed).error === 'Invalid review findings');
  }
  check('summary at UTF-8 cap survives', normalize([{ identity, findings: [{ ...finding, summary: 'é'.repeat(CODE_REVIEW_MAX_FINDING_SUMMARY_BYTES / 2) }] }]).error === null);
  check('summary over UTF-8 cap rejects', normalize([{ identity, findings: [{ ...finding, summary: 'é'.repeat(CODE_REVIEW_MAX_FINDING_SUMMARY_BYTES / 2 + 1) }] }]).error === 'Invalid review findings');
  const twenty = Array.from({ length: 20 }, (_, i) => ({ ...finding, summary: `issue ${i}` }));
  check('twenty findings per hunk survive', normalize([{ identity, findings: twenty }]).reviewFindings?.[0].findings.length === 20);
  check('twenty-one findings reject', normalize([{ identity, findings: [...twenty, { ...finding, summary: '21' }] }]).error === 'Invalid review findings');
  const byFile = aggregateFindingsByFile(snap.reviewFindings);
  const byDir = aggregateFindingsByDirectory(snap.reviewFindings);
  check('finding count and severity aggregate by file', byFile.get('src/a.ts')?.issues === 1 && byFile.get('src/a.ts')?.category === 'critical');
  check('finding count cascades to directory', byDir.get('src')?.issues === 1);
  check('reviewed clean contributes no issue badge', aggregateFindingsByFile([{ identity, findings: [] }]).size === 0);
  check('overlay preserves precise findings', overlayReviewLifecycle(snap, emptyReviewLifecycle()).reviewFindings === snap.reviewFindings);
}

{
  check('TLDR payload carries explicit language', JSON.stringify(buildCodeReviewTldrPayload('zh')) === JSON.stringify({ type: 'code_review_tldr', language: 'zh' }));
  check('TLDR Stop is typed', buildCodeReviewStopPayload('tldr').operation === 'tldr');
  const ack = parseCodeReviewTldrAck({ ok: true, state: 'running', tldrGeneration: 7 });
  check('TLDR ACK retains generation', ack.ok && ack.generation === 7);
  for (const generation of [undefined, null, -1, 1.5, '7']) {
    check('TLDR malformed ACK generation rejects', !parseCodeReviewTldrAck({ ok: true, state: 'running', tldrGeneration: generation }).ok);
  }
  const wire = { snapshotId: 'snap-t', tldrState: 'done', tldrGeneration: 7, tldrSnapshotId: 'snap-t', tldrError: null, tldrMarkdown: '# Summary' };
  const snap = normalizeCodeReviewSnapshot(wire);
  check('TLDR matching snapshot exposes markdown', tldrMarkdownForSnapshot(snap) === '# Summary');
  check('TLDR stale snapshot hides markdown', tldrMarkdownForSnapshot({ ...snap, snapshotId: 'next' }) === '');
  for (const field of [{ tldrState: 'paused' }, { tldrGeneration: -1 }, { tldrSnapshotId: 9 }, { tldrError: false }, { tldrMarkdown: null }]) {
    check('malformed TLDR field rejects snapshot', normalizeCodeReviewSnapshot({ ...wire, ...field }).error === 'Invalid TLDR fields');
  }
  check('TLDR pending gate is quiet', tldrButtonView(true, 'idle', false).progress === null);
  check('TLDR running gate identifies summary', tldrButtonView(false, 'running', true).progress === 'Summarizing…');
  check('TLDR failed state permits retry', !tldrButtonView(false, 'failed', true).busy);
}

{
  check('stop payload type is code_review_stop', buildCodeReviewStopPayload('autoReview').type === 'code_review_stop');
  check('stop autoReview operation', buildCodeReviewStopPayload('autoReview').operation === 'autoReview');
  check('stop highlight operation', buildCodeReviewStopPayload('highlight').operation === 'highlight');
  check(
    'stop payload is exactly type + operation (session stamped by caller)',
    JSON.stringify(Object.keys(buildCodeReviewStopPayload('highlight')).sort()) === JSON.stringify(['operation', 'type']),
  );
  check('stop payload has no sessionId (caller stamps)', !Object.prototype.hasOwnProperty.call(buildCodeReviewStopPayload('autoReview'), 'sessionId'));
}

check('spinner gate is the frozen 151ms', AUTO_REVIEW_SPINNER_GATE_MS === 151);

// ---------------------------------------------------------------------------
// Final-audit blockers: authoritative internalBusy, typed operation owners,
// identity-deduped async error toasts, and the accepted-comment follow event.
// ---------------------------------------------------------------------------

{
  // P0-1: a wire internalBusy=true batch owner (Auto Review triage / Highlight)
  // must survive overlayReviewLifecycle even with zero live/cancellable
  // threads — the overlay may ADD busy, never zero it.
  const triageSnap = normalizeCodeReviewSnapshot({
    snapshotId: 'snap-busy',
    reviewId: 'rev-1',
    generation: 1,
    revision: 4,
    files: [],
    threads: [],
    isStreaming: false,
    activeCount: 0,
    internalBusy: true,
    autoReviewState: 'triaging',
    autoReviewBatchId: 7,
    highlightState: 'idle',
    highlightGeneration: 0,
  });
  const triageOverlay = overlayReviewLifecycle(triageSnap, emptyReviewLifecycle());
  check(
    'triage batch owner survives the overlay (P0 internalBusy)',
    triageOverlay.internalBusy === true
      && formatReviewCloseWarning(triageOverlay.internalBusy, 0) !== null
      && formatReviewRefreshWarning(triageOverlay.internalBusy, 0) !== null,
  );
  // Thread ownership may ADD busy on top of a false wire flag: a snapshot
  // whose thread is live projects busy through the reconciled lifecycle
  // owner even though the wire flag was false.
  const streamSnap = normalizeCodeReviewSnapshot({
    snapshotId: 'snap-busy',
    reviewId: 'rev-1',
    generation: 1,
    revision: 4,
    files: [],
    internalBusy: false,
    isStreaming: true,
    activeCount: 1,
    threads: [
      {
        identity: {
          snapshotId: 'snap-busy',
          path: 'src/a.ts',
          oldStart: 1,
          oldCount: 1,
          newStart: 2,
          newCount: 1,
          contentHash: 'h',
        },
        comments: [],
        isStreaming: true,
        streamingText: 'live',
        isCancellable: true,
        replyStatus: 'streaming',
      },
    ],
  });
  const streamLife = applyReviewLifecycle(emptyReviewLifecycle(), {
    type: 'snapshot',
    snapshot: streamSnap,
  });
  const streamedOverlay = overlayReviewLifecycle(streamSnap, streamLife);
  check(
    'a live lifecycle thread adds busy over a false wire flag',
    streamedOverlay.internalBusy === true
      && streamedOverlay.isStreaming === true,
  );
}

{
  const identity = {
    snapshotId: 's-live',
    path: 'src/a.ts',
    oldStart: 1,
    oldCount: 1,
    newStart: 2,
    newCount: 1,
    contentHash: 'h-live',
  };
  const longSnap = normalizeCodeReviewSnapshot({
    reviewId: 'rev-live',
    snapshotId: 's-live',
    generation: 1,
    revision: 2,
    threads: [{
      identity,
      comments: [],
      isStreaming: true,
      streamingText: 'hello world',
      streamingThinking: 'think longer',
      isCancellable: true,
      replyStatus: 'streaming',
      generation: 1,
      replyId: 'r1',
    }],
  });
  let life = applyReviewLifecycle(emptyReviewLifecycle(), { type: 'snapshot', snapshot: longSnap });
  life = applyReviewLifecycle(life, {
    type: 'delta',
    key: hunkKey(identity),
    generation: 1,
    replyId: 'r1',
    text: ' more',
  });
  const shortSnap = normalizeCodeReviewSnapshot({
    reviewId: 'rev-live',
    snapshotId: 's-live',
    generation: 1,
    revision: 3,
    threads: [{
      identity,
      comments: [],
      isStreaming: true,
      streamingText: 'hello',
      streamingThinking: 'think',
      isCancellable: true,
      replyStatus: 'streaming',
      generation: 1,
      replyId: 'r1',
    }],
  });
  const overlaid = overlayReviewLifecycle(shortSnap, life);
  check(
    'overlayReviewLifecycle paints live.reply.streamingText when the owner is live',
    overlaid.threads[0].streamingText === 'hello world more',
    overlaid.threads[0].streamingText,
  );
  check(
    'overlay paints live.reply.streamingThinking over a shorter snapshot',
    overlaid.threads[0].streamingThinking === 'think longer',
    overlaid.threads[0].streamingThinking,
  );
  const merged = applyReviewLifecycle(life, { type: 'snapshot', snapshot: shortSnap });
  const mergedReply = merged.threads[hunkKey(identity)].reply;
  check(
    'reconcileSnapshotThread keeps a longer last-good prefix only when incomingHasStream',
    mergedReply.streamingText === 'hello world more' && mergedReply.streamingThinking === 'think longer',
    `${mergedReply.streamingText}|${mergedReply.streamingThinking}`,
  );
  const emptySnap = normalizeCodeReviewSnapshot({
    reviewId: 'rev-live',
    snapshotId: 's-live',
    generation: 1,
    revision: 4,
    threads: [{
      identity,
      comments: [],
      isStreaming: true,
      streamingText: '',
      streamingThinking: '',
      isCancellable: true,
      replyStatus: 'streaming',
      generation: 1,
      replyId: 'r1',
    }],
  });
  const deadAirLife = applyReviewLifecycle(life, { type: 'snapshot', snapshot: emptySnap });
  const deadAirOverlay = overlayReviewLifecycle(emptySnap, deadAirLife);
  const deadAirReply = deadAirLife.threads[hunkKey(identity)].reply;
  check(
    'reconcileSnapshotThread empty live snapshot is dead-air (does not keep the previous prefix)',
    deadAirReply.streamingText === ''
      && deadAirReply.streamingThinking === ''
      && deadAirOverlay.threads[0].streamingText === ''
      && deadAirOverlay.threads[0].streamingThinking === '',
    `${deadAirReply.streamingText}|${deadAirReply.streamingThinking}|${deadAirOverlay.threads[0].streamingText}|${deadAirOverlay.threads[0].streamingThinking}`,
  );
}

{
  // Strict wire parsing of the new identity + typed-error fields.
  const snap = normalizeCodeReviewSnapshot({
    autoReviewState: 'failed',
    autoReviewError: { message: 'Auto review stopped', code: 'cancelled' },
    autoReviewBatchId: 9,
    highlightState: 'failed',
    highlightError: 'Highlight returned an invalid summary',
    highlightGeneration: 5,
  });
  check(
    'identity fields and typed auto error normalize through',
    snap.autoReviewBatchId === 9
      && snap.highlightGeneration === 5
      && snap.autoReviewError !== null
      && snap.autoReviewError.code === 'cancelled',
  );
  check(
    'malformed typed auto error nulls whole',
    normalizeCodeReviewSnapshot({ autoReviewError: 'plain string' }).autoReviewError === null
      && normalizeCodeReviewSnapshot({ autoReviewError: { message: 'm' } }).autoReviewError === null
      && normalizeCodeReviewSnapshot({ autoReviewError: { code: 'cancelled' } }).autoReviewError === null
      && normalizeCodeReviewSnapshot({ autoReviewError: 7 }).autoReviewError === null,
  );
  check(
    'missing identity fields default to 0',
    emptyCodeReviewSnapshot().autoReviewBatchId === 0
      && emptyCodeReviewSnapshot().highlightGeneration === 0
      && normalizeCodeReviewSnapshot({}).autoReviewBatchId === 0
      && normalizeCodeReviewSnapshot({}).highlightGeneration === 0,
  );
  const identity = { snapshotId: 'snap-1', path: 'src/a.ts', oldStart: 1, oldCount: 1, newStart: 2, newCount: 1, contentHash: 'h' };
  const validFinding = { category: 'critical', side: 'new', startLine: 2, endLine: 2, summary: 'bounds check', evidence: 'unchecked index' };
  for (const token of ['constructor', 'toString', '__proto__']) {
    for (const field of ['category', 'side']) {
      const rejected = normalizeCodeReviewSnapshot({
        reviewFindings: [{ identity, findings: [validFinding, { ...validFinding, [field]: token }] }],
      });
      check(`prototype-key ${field} ${token} rejects the whole findings snapshot`,
        rejected.error === 'Invalid review findings' && rejected.reviewFindings === null);
    }
  }
}

{
  // Highlight request owner: click → ACK gap → matching generation.
  const idle = initialHighlightOwnerState();
  check('idle highlight owner is not busy', !highlightOwnerBusy(idle));
  check(
    'idle highlight view has no identifying status',
    highlightButtonView(false, 'idle', true).progress === null,
  );
  const clicked = applyHighlightOwnerEvent(idle, { type: 'click', reviewId: 'rev-1' });
  check(
    'highlight click owns the button',
    highlightOwnerBusy(clicked) && clicked.reviewId === 'rev-1',
  );
  check(
    'pending highlight stays quiet before the spinner gate and then identifies',
    highlightButtonView(true, 'idle', false).progress === null
      && highlightButtonView(true, 'idle', true).progress === 'Identifying…',
  );
  const acked = applyHighlightOwnerEvent(clicked, { type: 'ack', generation: 3 });
  check(
    'highlight ACK keeps the owner busy with the accepted generation',
    highlightOwnerBusy(acked) && acked.acceptedGeneration === 3,
  );
  check(
    'a stale generation snapshot cannot clear the highlight owner',
    applyHighlightOwnerEvent(acked, {
      type: 'snapshot',
      generation: 2,
      state: 'idle',
      reviewId: 'rev-1',
    }) === acked,
  );
  for (const state of ['done', 'failed'] as const) {
    check('stale terminal generation cannot release pending owner', applyHighlightOwnerEvent(acked, {
      type: 'snapshot', generation: 2, state, reviewId: 'rev-1',
    }) === acked);
  }
  const released = applyHighlightOwnerEvent(acked, {
    type: 'snapshot',
    generation: 3,
    state: 'running',
    reviewId: 'rev-1',
  });
  check(
    'the first matching-or-newer generation snapshot releases the highlight owner',
    !highlightOwnerBusy(released) && released.acceptedGeneration === null,
  );
  check(
    'released running wire state still identifies after the gate',
    highlightButtonView(false, 'running', false).progress === null
      && highlightButtonView(false, 'running', true).progress === 'Identifying…',
  );
  check(
    'done and failed highlight wire states clear the identifying status',
    highlightButtonView(false, 'done', true).progress === null
      && highlightButtonView(false, 'failed', true).progress === null,
  );
  check(
    'a rejected highlight clears only its own owner',
    (() => {
      const again = applyHighlightOwnerEvent(released, { type: 'click', reviewId: 'rev-1' });
      const rejected = applyHighlightOwnerEvent(again, { type: 'reject' });
      return !highlightOwnerBusy(rejected) && rejected.reviewId === null;
    })(),
  );
  check(
    'a pending highlight owner for a stale reviewId is released by the first snapshot of a new review',
    (() => {
      const restarted = applyHighlightOwnerEvent(clicked, {
        type: 'snapshot',
        generation: 0,
        state: 'idle',
        reviewId: 'rev-2',
      });
      return !highlightOwnerBusy(restarted)
        && restarted.pending === false
        && restarted.reviewId === null
        && restarted.acceptedGeneration === null;
    })(),
  );
  check(
    'an accepted highlight owner for a stale reviewId is released by a restart idle snapshot',
    (() => {
      const restarted = applyHighlightOwnerEvent(acked, {
        type: 'snapshot',
        generation: 0,
        state: 'idle',
        reviewId: 'rev-2',
      });
      return !highlightOwnerBusy(restarted) && restarted.reviewId === null;
    })(),
  );
}

{
  // Async error toasts: exactly once per identity; cancelled is the only
  // quiet terminal category.
  let report = initialReviewErrorReport();
  const failedBatch = normalizeCodeReviewSnapshot({
    snapshotId: 's1',
    reviewId: 'rev-1',
    generation: 1,
    revision: 2,
    autoReviewState: 'failed',
    autoReviewError: { message: 'triage returned invalid JSON', code: 'invalid-triage' },
    autoReviewBatchId: 4,
    highlightError: 'provider down',
    highlightGeneration: 6,
  });
  let notices = reviewErrorNotices(failedBatch, report).notices;
  check(
    'both business failures surface with context once',
    notices.length === 2
      && notices[0].message === 'Auto review: triage returned invalid JSON'
      && notices[0].cancelled === false
      && notices[1].message === 'Highlight: provider down'
      && notices[1].cancelled === false,
  );
  report = reviewErrorNotices(failedBatch, initialReviewErrorReport()).reported;
  check(
    'repeated polls of the same identity are quiet',
    reviewErrorNotices(failedBatch, report).notices.length === 0,
  );
  // A newer batch id re-arms the toast even with identical copy.
  const newerBatch = normalizeCodeReviewSnapshot({
    ...failedBatch,
    autoReviewBatchId: 5,
  });
  check(
    'a new batch identity re-arms its error toast',
    reviewErrorNotices(newerBatch, report).notices.length === 1,
  );
  check(
    'a new review identity re-arms auto and highlight error toasts',
    reviewErrorNotices(
      normalizeCodeReviewSnapshot({ ...failedBatch, reviewId: 'rev-2' }),
      report,
    ).notices.length === 2,
  );
  // User stop: the typed cancelled category is the ONLY quiet terminal.
  const cancelledBatch = normalizeCodeReviewSnapshot({
    snapshotId: 's1',
    autoReviewState: 'failed',
    autoReviewError: { message: 'Auto review stopped', code: 'cancelled' },
    autoReviewBatchId: 7,
  });
  const cancelled = reviewErrorNotices(cancelledBatch, initialReviewErrorReport());
  check(
    'the typed user-stop category is quiet',
    cancelled.notices.length === 1
      && cancelled.notices[0].cancelled === true,
  );
  // snapshot.error is keyed per review identity (not repeated per poll).
  const errorSnap = normalizeCodeReviewSnapshot({
    reviewId: 'rev-2',
    generation: 1,
    revision: 3,
    error: 'git capture failed',
  });
  const first = reviewErrorNotices(errorSnap, initialReviewErrorReport());
  check(
    'snapshot.error toasts once with context',
    first.notices.length === 1
      && first.notices[0].message === 'Code review: git capture failed'
      && first.notices[0].cancelled === false,
  );
  check(
    'the same snapshot error identity never re-toasts',
    reviewErrorNotices(errorSnap, first.reported).notices.length === 0,
  );
  const bumped = reviewErrorNotices(
    normalizeCodeReviewSnapshot({ ...errorSnap, revision: 4 }),
    first.reported,
  );
  check(
    'a new revision re-arms the snapshot error toast',
    bumped.notices.length === 1,
  );
}
{
  const identity = {
    snapshotId: 's1',
    path: 'src/a.rs',
    oldStart: 1,
    oldCount: 1,
    newStart: 2,
    newCount: 1,
    contentHash: 'h',
  };
  const rejected = normalizeCodeReviewSnapshot({
    reviewId: 'rev-1',
    autoReviewBatchId: 4,
    autoReviewState: 'failed',
    autoReviewError: { message: 'Auto review hunk submission was rejected', code: 'launch-rejected' },
    threads: [{
      identity,
      comments: [{ role: 'user', text: 'Auto review', partial: false, auto: true }],
      error: 'src/a.rs identity is not in the request scope',
      replyStatus: 'failed',
    }, {
      identity: { ...identity, path: 'src/b.rs', contentHash: 'hb' },
      comments: [{ role: 'user', text: 'Auto review', partial: false, auto: true }],
      error: 'src/b.rs identity is not in the request scope',
      replyStatus: 'failed',
    }],
  });
  const notices = reviewErrorNotices(rejected, initialReviewErrorReport()).notices;
  check(
    'launch-rejected emits one toast with the exact auto thread cause',
    notices.length === 1
      && notices[0].kind === 'autoReview'
      && notices[0].message === 'Auto review: src/a.rs: src/a.rs identity is not in the request scope (+1 more)'
      && notices[0].cancelled === false,
    notices.map((notice) => notice.message).join(' | '),
  );
  const followed = normalizeCodeReviewSnapshot({
    reviewId: 'rev-1',
    threads: [{
      identity,
      comments: [
        { role: 'user', text: 'Auto review', partial: false, auto: true },
        { role: 'user', text: 'manual follow-up', partial: false },
      ],
      error: 'manual follow-up failed',
      replyStatus: 'failed',
    }],
  });
  check('a later manual user card is not an auto thread', !threadLatestUserIsAuto(followed.threads[0]));
  check(
    'a manual follow-up error stays out of the auto toast',
    reviewErrorNotices(followed, initialReviewErrorReport()).notices.length === 0,
  );
}

{
  check('idle snapshot poll stays 1500ms', snapshotPollDelayMs(false) === CODE_REVIEW_SNAPSHOT_POLL_MS && CODE_REVIEW_SNAPSHOT_POLL_MS === 1500);
  check('live Auto Review / Highlight poll is 200ms', snapshotPollDelayMs(true) === CODE_REVIEW_BATCH_POLL_MS && CODE_REVIEW_BATCH_POLL_MS === 200);
  check(
    'pending highlight owner is live',
    codeReviewBatchOwnerLive({
      autoReviewPending: false,
      highlightPending: true,
      tldrPending: false,
      tldrState: 'idle',
      autoReviewState: 'idle',
      highlightState: 'idle',
    }) === true,
  );
  check(
    'idle owners use the idle poll',
    codeReviewBatchOwnerLive({
      autoReviewPending: false,
      highlightPending: false,
      tldrPending: false,
      tldrState: 'idle',
      autoReviewState: 'idle',
      highlightState: 'idle',
    }) === false,
  );
  check(
    'running highlight keeps the live poll after the owner releases',
    codeReviewBatchOwnerLive({
      autoReviewPending: false,
      highlightPending: false,
      tldrPending: false,
      tldrState: 'idle',
      autoReviewState: 'idle',
      highlightState: 'running',
    }) === true,
  );
}

{
  const grown = codeReviewToastAnchor({
    headBottom: 200,
    tabBarBottom: null,
    summaryHeadBottom: null,
    composerTop: 422,
    composerHeight: 300,
    viewportHeight: 844,
  });
  const toastHeight = 40;
  const centeredTop = 844 / 2 - toastHeight / 2;
  check(
    'a 50%-centered toast would overlap a grown mobile composer',
    centeredTop + toastHeight > 422,
  );
  check(
    'grown-composer toast sits below the review head',
    grown.top === 200 + CODE_REVIEW_TOAST_GAP_PX,
  );
  check(
    'grown-composer toast stays above the hunk composer',
    grown.top + Math.min(toastHeight, grown.maxHeight) <= 422,
    `top=${grown.top} maxHeight=${grown.maxHeight}`,
  );
  const hiddenComposer = codeReviewToastAnchor({
    headBottom: 180,
    tabBarBottom: null,
    summaryHeadBottom: null,
    composerTop: 0,
    composerHeight: 0,
    viewportHeight: 844,
  });
  check(
    'display-none composer uses the viewport floor',
    hiddenComposer.maxHeight === 844 - CODE_REVIEW_TOAST_GAP_PX - (180 + CODE_REVIEW_TOAST_GAP_PX),
  );
  const mobile = codeReviewToastAnchor({
    headBottom: 249,
    tabBarBottom: 302,
    summaryHeadBottom: null,
    composerTop: 422,
    composerHeight: 300,
    viewportHeight: 844,
  });
  check(
    'mobile toast clears the tab bar that the header-only anchor covered',
    mobile.top === 302 + CODE_REVIEW_TOAST_GAP_PX,
  );
  check(
    'mobile tab protection preserves the composer boundary',
    mobile.top + mobile.maxHeight === 422 - CODE_REVIEW_TOAST_GAP_PX,
  );
  for (const [headBottom, tabBarBottom, summaryHeadBottom] of [[63, null, 127], [249, 302, 399]] as const) {
    const summary = codeReviewToastAnchor({
      headBottom,
      tabBarBottom,
      summaryHeadBottom,
      composerTop: 600,
      composerHeight: 200,
      viewportHeight: 844,
    });
    check(
      'toast clears summary Stop and Close on desktop and mobile',
      summary.top === summaryHeadBottom + CODE_REVIEW_TOAST_GAP_PX,
    );
    check(
      'summary header protection preserves the composer boundary',
      summary.top + summary.maxHeight === 600 - CODE_REVIEW_TOAST_GAP_PX,
    );
  }
  const props: Record<string, string> = {};
  const style = {
    setProperty(name: string, value: string) { props[name] = value; },
    removeProperty(name: string) { delete props[name]; },
  } as CSSStyleDeclaration;
  applyCodeReviewToastAnchor(style, grown);
  check(
    'toast anchor writes CSS variables',
    props[CODE_REVIEW_TOAST_TOP_VAR] === `${grown.top}px`
      && props[CODE_REVIEW_TOAST_MAX_HEIGHT_VAR] === `${grown.maxHeight}px`,
  );
  clearCodeReviewToastAnchor(style);
  check(
    'toast anchor clears CSS variables',
    props[CODE_REVIEW_TOAST_TOP_VAR] === undefined
      && props[CODE_REVIEW_TOAST_MAX_HEIGHT_VAR] === undefined,
  );
  applyCodeReviewToastAnchor(style, grown);
  syncCodeReviewToastAnchor({ querySelector() { return null; } } as unknown as HTMLElement, style, 844);
  check(
    'missing review head clears leftover toast anchor variables',
    props[CODE_REVIEW_TOAST_TOP_VAR] === undefined
      && props[CODE_REVIEW_TOAST_MAX_HEIGHT_VAR] === undefined,
  );
}

console.log(`\ncodeReview.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
