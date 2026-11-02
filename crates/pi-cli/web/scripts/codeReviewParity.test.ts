#!/usr/bin/env node
// Two-stage Auto Review + review UI parity regressions (pure layer only):
// snapshot wire defaults (triaging / reviewLanguageDefault / autoReviewError),
// the composer model-switch state machine, the Auto Review button machine
// with the 151ms spinner gate, reply-language resolution, findings-category
// aggregation, and the Source|Preview render-kind decision (the file
// explorer's own decision, reused). Later sections assert the EXACT-identity
// per-hunk review findings — side-range rows, cross-file order, no-wrap
// issue navigation — plus the components' findings props, the hunk
// composer's main-composer icon language, and the Explorer pane-shimmer CSS
// hooks. Run through `npm run build`.
//
// Exit codes: 0 = every assertion held; 1 = a regression.
// Assertions exercise BEHAVIOR (what the functions return), not source strings.
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createElement, act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import { imageAttachmentAccept, type ComposerAttachment } from '../src/attachments.ts';
import {
  AUTO_REVIEW_SPINNER_GATE_MS,
  aggregateFindingsByDirectory,
  aggregateFindingsByFile,
  applyAutoReviewButtonEvent,
  applyReviewModelEvent,
  autoReviewButtonView,
  autoReviewLanguagePayload,
  autoReviewBusy,
  boundedReviewStatus,
  buildCodeReviewAutoReviewPayload,
  buildCodeReviewSetModelPayload,
  emptyCodeReviewSnapshot,
  findingRowIndexes,
  findingsForIdentity,
  findingsCategory,
  hunkIdentityFor,
  initialAutoReviewButtonState,
  initialReviewModelState,
  invalidateReviewFindingRef,
  modelIdFromOptionValue,
  navigateReviewFinding,
  normalizeAvailableModels,
  normalizeCodeReviewSnapshot,
  normalizeThreads,
  orderReviewFindings,
  parseCodeReviewSetModelAck,
  REVIEW_LANGUAGE_OPTIONS,
  reviewCategoryTone,
  reviewFindingRefValid,
  reviewLanguageSelection,
  reviewModelLabel,
  reviewModelPending,
  reviewModelSelectValue,
} from '../src/codeReview.ts';
import type {
  AutoReviewAggregate,
  DiffFile,
  DiffLine,
  FileTree,
  HunkIdentity,
  HunkReviewResult,
  ReviewFinding,
  ReviewFindingRef,
  ReviewThread,
  VisibleTreeRow,
} from '../src/codeReview.ts';
import {
  CodeReviewDiffPane,
  reviewPreviewKind,
  type CodeReviewDiffPaneProps,
} from '../src/panels/CodeReviewDiffPane.tsx';
import { CodeReviewFileList } from '../src/panels/CodeReviewFileList.tsx';
import { CodeReviewThreadDock, type CodeReviewThreadDockProps } from '../src/panels/CodeReviewThreadDock.tsx';
import { fileExplorerPreviewKind, normalizeRead } from '../src/fileExplorer.ts';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: unknown, detail = ''): void {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

// ---- snapshot wire defaults: reviewLanguageDefault / autoReviewError / triaging ----
{
  const absent = normalizeCodeReviewSnapshot({});
  check('absent reviewLanguageDefault normalizes to null', absent.reviewLanguageDefault === null);
  check('absent autoReviewError normalizes to null', absent.autoReviewError === null);
  check('absent autoReviewState falls back to idle', absent.autoReviewState === 'idle');
  check(
    'empty snapshot carries the same defaults',
    emptyCodeReviewSnapshot().reviewLanguageDefault === null
      && emptyCodeReviewSnapshot().autoReviewError === null
      && emptyCodeReviewSnapshot().autoReviewState === 'idle',
  );
  const tagged = normalizeCodeReviewSnapshot({ reviewLanguageDefault: 'de' });
  check('string reviewLanguageDefault passes through', tagged.reviewLanguageDefault === 'de');
  check(
    'non-string reviewLanguageDefault nulls',
    normalizeCodeReviewSnapshot({ reviewLanguageDefault: 42 }).reviewLanguageDefault === null,
  );
  check(
    'triaging is a recognized batch state',
    normalizeCodeReviewSnapshot({ autoReviewState: 'triaging' }).autoReviewState === 'triaging',
  );
  check(
    'unknown batch states fall back to idle',
    normalizeCodeReviewSnapshot({ autoReviewState: 'paused' }).autoReviewState === 'idle',
  );
  check(
    'typed autoReviewError passes through',
    (() => {
      const error = normalizeCodeReviewSnapshot({
        autoReviewError: { message: 'triage failed', code: 'invalid-triage' },
      }).autoReviewError;
      return error !== null && error.message === 'triage failed' && error.code === 'invalid-triage';
    })(),
  );
  check(
    'malformed autoReviewError nulls whole (retired plain strings included)',
    normalizeCodeReviewSnapshot({ autoReviewError: 'triage failed' }).autoReviewError === null
      && normalizeCodeReviewSnapshot({ autoReviewError: 7 }).autoReviewError === null
      && normalizeCodeReviewSnapshot({ autoReviewError: { message: 'm' } }).autoReviewError === null,
  );
}

// ---- model switch state machine: select → command → ack / typed reject ----
{
  const initial = initialReviewModelState();
  check(
    'model machine starts unknown and clean',
    initial.appliedValue === '' && initial.appliedLabel === ''
      && initial.pendingValue === null && initial.status === null,
  );

  const picked = applyReviewModelEvent(initial, { type: 'pick', value: 'anthropic/claude-x' });
  check(
    'pick marks the switch in flight with the picked value',
    reviewModelPending(picked) && picked.pendingValue === 'anthropic/claude-x',
  );
  check(
    'pick clears a previous status',
    applyReviewModelEvent({ ...initial, status: 'old failure' }, { type: 'pick', value: 'prov/m' })
      .status === null,
  );
  check(
    'pick of the applied value is a no-op',
    applyReviewModelEvent({ ...initial, appliedValue: 'prov/m' }, { type: 'pick', value: 'prov/m' })
      .pendingValue === null,
  );
  check('empty pick is ignored', applyReviewModelEvent(initial, { type: 'pick', value: '' })
    === initial);

  const acked = applyReviewModelEvent(picked, { type: 'ack', label: 'Claude X' });
  check(
    'ack applies the picked value with the display label',
    acked.appliedValue === 'anthropic/claude-x' && acked.appliedLabel === 'Claude X',
  );
  check('ack clears pending and status', !reviewModelPending(acked) && acked.status === null);
  check('select value follows the applied model after ack', reviewModelSelectValue(acked)
    === 'anthropic/claude-x');

  const ackedNoLabel = applyReviewModelEvent(picked, { type: 'ack', label: null });
  check(
    'ack without a usable label still applies the value',
    ackedNoLabel.appliedValue === 'anthropic/claude-x' && ackedNoLabel.appliedLabel === '',
  );

  const rejectMessage = `unknown model "x" (catalog: 12 available)${' detail'.repeat(40)}`;
  const rejected = applyReviewModelEvent(picked, { type: 'reject', message: rejectMessage });
  check(
    'typed reject keeps the applied value so the select reverts',
    rejected.appliedValue === '' && rejected.pendingValue === null,
  );
  check(
    'typed reject surfaces a bounded inline status',
    rejected.status !== null && rejected.status.length <= 240 && rejected.status.endsWith('…'),
  );
  check(
    'label prefers the ack label over the raw value',
    reviewModelLabel({
      appliedValue: 'prov/m2',
      appliedLabel: 'Model Two',
      pendingValue: null,
      status: null,
    }) === 'Model Two',
  );

  const cancelled = applyReviewModelEvent(picked, { type: 'cancel' });
  check(
    'transport cancel reverts silently without a status',
    cancelled.pendingValue === null && cancelled.status === null
      && cancelled.appliedValue === '',
  );

  check(
    'set_model payload carries the qualified provider/id modelId',
    JSON.stringify(buildCodeReviewSetModelPayload('anthropic/grok-4'))
      === JSON.stringify({ type: 'code_review_set_model', modelId: 'anthropic/grok-4' }),
  );
  check(
    'set_model ack parses the display name',
    parseCodeReviewSetModelAck({ ok: true, model: 'Grok 4' }) === 'Grok 4',
  );
  check(
    'set_model ack rejects ok:false and garbage',
    parseCodeReviewSetModelAck({ ok: false, model: 'x' }) === null
      && parseCodeReviewSetModelAck('nope') === null,
  );
  check(
    'model id extraction splits at the FIRST slash only',
    modelIdFromOptionValue('anthropic/grok-4') === 'grok-4'
      && modelIdFromOptionValue('openrouter/vendor/model-x') === 'vendor/model-x',
  );
  const models = normalizeAvailableModels({
    models: [
      { id: 'a', name: 'A', provider: 'p1' },
      { id: '', name: 'drop', provider: 'p1' },
      { id: 'b', provider: 'p2' },
      'junk',
    ],
  });
  check(
    'model catalog rows drop malformed entries',
    models.length === 2 && models[0].name === 'A' && models[1].name === '',
  );
  check(
    'bounded status trims and caps rejects',
    boundedReviewStatus('  failed  ') === 'failed'
      && boundedReviewStatus('x'.repeat(300)).length === 240
      && boundedReviewStatus('x'.repeat(10)) === 'x'.repeat(10)
      && boundedReviewStatus('   ') === '',
  );
}

// ---- Auto Review button machine: idle → pending → triaging/running → done ----
{
  const initial = initialAutoReviewButtonState();
  const idleView = autoReviewButtonView(initial, false);
  check(
    'idle view has no progress',
    !idleView.busy && idleView.phase === 'idle' && idleView.progress === null,
  );
  check(
    'gate constant is the frozen delay (current tree: 151ms)',
    AUTO_REVIEW_SPINNER_GATE_MS === 151,
  );

  const clicked = applyAutoReviewButtonEvent(initial, { type: 'click' });
  const preGate = autoReviewButtonView(clicked, false);
  check(
    'click is busy and quiet before the gate',
    autoReviewBusy(clicked) && preGate.busy && preGate.phase === 'triaging' && preGate.progress === null,
  );
  const gated = autoReviewButtonView(clicked, true);
  check('post-gate pending screens', gated.busy && gated.phase === 'triaging' && gated.progress === 'Screening…');
  check(
    'double click is idempotent',
    applyAutoReviewButtonEvent(clicked, { type: 'click' }) === clicked,
  );

  const rejected = applyAutoReviewButtonEvent(clicked, { type: 'reject' });
  check(
    'a rejected request clears only its own owner and returns to idle',
    !autoReviewBusy(rejected) && autoReviewButtonView(rejected, true).phase === 'idle'
      && autoReviewButtonView(rejected, true).progress === null,
  );
  // Pre-ACK adoption: a visible triaging/running snapshot during the click
  // window is the accepted batch (only one batch may run), so the owner
  // releases while the wire state stays busy.
  const preAckRunning = applyAutoReviewButtonEvent(clicked, {
    type: 'snapshot',
    state: 'running',
    counts: { submitted: 2, completed: 0, failed: 0 },
    batchId: 1,
  });
  check(
    'a pre-ACK visible running snapshot releases the owner (the batch is live)',
    autoReviewBusy(preAckRunning) && !preAckRunning.pending && preAckRunning.wireState === 'running',
  );
  const running = applyAutoReviewButtonEvent(preAckRunning, {
    type: 'snapshot',
    state: 'running',
    counts: { submitted: 2, completed: 0, failed: 0 },
    batchId: 1,
  });
  const runningHidden = autoReviewButtonView(running, false);
  check(
    'running hides Review 0/2 until the spinner gate',
    autoReviewBusy(running) && runningHidden.phase === 'running' && runningHidden.progress === null,
  );
  const runningShown = autoReviewButtonView(running, true);
  check(
    'running after the spinner gate paints Review 0/2',
    runningShown.phase === 'running' && runningShown.progress === 'Review 0/2',
  );

  const triaging = applyAutoReviewButtonEvent(initialAutoReviewButtonState(), {
    type: 'snapshot',
    state: 'triaging',
    counts: { submitted: 0, completed: 0, failed: 0 },
    batchId: 2,
  });
  const triageView = autoReviewButtonView(triaging, true);
  check(
    'triaging owns the button and screens after the gate',
    autoReviewBusy(triaging) && triageView.phase === 'triaging' && triageView.progress === 'Screening…',
  );

  const done = applyAutoReviewButtonEvent(running, {
    type: 'snapshot',
    state: 'done',
    counts: { submitted: 2, completed: 2, failed: 0 },
    batchId: 1,
  });
  const doneView = autoReviewButtonView(done, false);
  check(
    'done releases the button and keeps Review 2/2',
    !autoReviewBusy(done)
      && doneView.phase === 'done'
      && doneView.progress === 'Review 2/2'
      && autoReviewButtonView(done, true).progress === 'Review 2/2',
  );
  check(
    'snapshot events with identical state keep the same object',
    applyAutoReviewButtonEvent(done, {
      type: 'snapshot',
      state: 'done',
      counts: { submitted: 2, completed: 2, failed: 0 },
      batchId: 1,
    }) === done,
  );

  const failedBatch = applyAutoReviewButtonEvent(done, {
    type: 'snapshot',
    state: 'failed',
    counts: { submitted: 5, completed: 2, failed: 3 },
    batchId: 1,
  });
  check(
    'failed batch is not busy (the typed error toast owns the failure)',
    !autoReviewBusy(failedBatch),
  );

  check(
    'auto review payload carries language only when explicit',
    JSON.stringify(buildCodeReviewAutoReviewPayload())
      === JSON.stringify({ type: 'code_review_auto_review' })
      && JSON.stringify(buildCodeReviewAutoReviewPayload(undefined))
        === JSON.stringify({ type: 'code_review_auto_review' })
      && buildCodeReviewAutoReviewPayload('fr').language === 'fr',
  );
}

// ---- reply language: explicit > settings default; fail-closed unknown tags ----
{
  check(
    'untouched + no default resolves to the Default sentinel',
    reviewLanguageSelection(null, null) === '',
  );
  check('settings default preselects its tag', reviewLanguageSelection('de', null) === 'de');
  check('an unknown settings default is not selectable', reviewLanguageSelection('pt-BR', null) === '');
  check('an explicit sentinel pick wins over the default', reviewLanguageSelection('de', '') === '');
  check('an explicit tag pick wins over the default', reviewLanguageSelection('de', 'fr') === 'fr');
  check(
    'language rides the wire only for a known explicit pick',
    autoReviewLanguagePayload(null) === undefined
      && autoReviewLanguagePayload('') === undefined
      && autoReviewLanguagePayload('ja') === 'ja'
      && autoReviewLanguagePayload('pt') === undefined,
  );
  check(
    'exactly the seven common languages ship',
    REVIEW_LANGUAGE_OPTIONS.length === 7
      && new Set(REVIEW_LANGUAGE_OPTIONS.map((option) => option.tag)).size === 7
      && REVIEW_LANGUAGE_OPTIONS.some((option) => option.tag === 'zh' && option.label === '中文'),
  );
}

// ---- review findings aggregation + tones (snapshot.reviewFindings) ----
{
  const identity = (path: string): HunkIdentity => ({
    snapshotId: 's1',
    path,
    oldStart: 1,
    oldCount: 5,
    newStart: 1,
    newCount: 5,
    contentHash: `h-${path}`,
  });
  const finding = (category: ReviewFinding['category']): ReviewFinding => ({
    category,
    side: 'new',
    startLine: 1,
    endLine: 1,
    summary: `${category} issue`,
    evidence: 'evidence',
  });
  const results: HunkReviewResult[] = [
    { identity: identity('src/a.rs'), findings: [finding('critical'), finding('naming'), finding('naming')] },
    { identity: identity('src/lib/b.rs'), findings: [finding('overdesign')] },
    { identity: identity('c.md'), findings: [] },
  ];

  const byFile = aggregateFindingsByFile(results);
  check('per-file aggregate sums findings', byFile.get('src/a.rs')?.issues === 3);
  check('per-file category is the group maximum', byFile.get('src/a.rs')?.category === 'critical');
  check('second file aggregates its own count', byFile.get('src/lib/b.rs')?.issues === 1 && byFile.get('src/lib/b.rs')?.category === 'overdesign');
  check('reviewed-clean hunks contribute nothing', !byFile.has('c.md'));
  check('absent collection is unreviewed (empty map)', aggregateFindingsByFile(null).size === 0);

  const byDir = aggregateFindingsByDirectory(results);
  check('directory aggregate covers the whole subtree', byDir.get('src')?.issues === 4 && byDir.get('src')?.category === 'critical');
  check('nested directories aggregate independently', byDir.get('src/lib')?.issues === 1 && byDir.get('src/lib')?.category === 'overdesign');
  check('a clean-only root file creates no root entry', !byDir.has('(root)'));

  check(
    'category tone follows the finding category (absent = none)',
    reviewCategoryTone('critical') === 'danger'
      && reviewCategoryTone('overdesign') === 'warn'
      && reviewCategoryTone('naming') === 'info'
      && reviewCategoryTone(null) === null
      && reviewCategoryTone(undefined) === null,
  );
  check(
    'findingsCategory picks the highest category and none for an empty hunk',
    findingsCategory([finding('naming'), finding('critical'), finding('overdesign')]) === 'critical'
      && findingsCategory([]) === null,
  );
}

// ---- Source|Preview render-kind decision reuses the file explorer ----
{
  const cases: Array<[string, string | null]> = [
    ['README.md', 'markdown'],
    ['notes.markdown', 'markdown'],
    ['page.html', 'html'],
    ['page.htm', 'html'],
    ['icon.svg', 'svg'],
    ['main.rs', null],
    ['a.txt', null],
    ['archive.tar.gz', null],
  ];
  for (const [path, expected] of cases) {
    const kind = reviewPreviewKind({ path, binary: false });
    check(`preview decision for ${path}`, kind === expected, `got ${kind}`);
    check(
      `review decision reuses the explorer decision for ${path}`,
      kind === fileExplorerPreviewKind(path),
    );
  }
  check(
    'binary files fail closed to Source',
    reviewPreviewKind({ path: 'icon.svg', binary: true }) === null
      && reviewPreviewKind({ path: 'README.md', binary: true }) === null,
  );
  const read = normalizeRead({
    path: 'icon.svg',
    lines: ['<svg/>'],
    totalLines: 1,
    sizeBytes: 6,
    truncated: false,
  });
  check(
    'the explorer read normalizer serves the preview fetch',
    read != null && read.path === 'icon.svg' && read.lines.length === 1 && !read.truncated,
  );
}

// ---- review findings: exact-identity matching, side-range rows, global
//      ordering, and no-wrap previous/next navigation ----
{
  const identity = (over: Partial<HunkIdentity> = {}): HunkIdentity => ({
    snapshotId: 's1',
    path: 'src/a.rs',
    oldStart: 1,
    oldCount: 5,
    newStart: 1,
    newCount: 5,
    contentHash: 'h-a',
    ...over,
  });
  const finding = (over: Partial<ReviewFinding> = {}): ReviewFinding => ({
    category: 'naming',
    side: 'new',
    startLine: 2,
    endLine: 2,
    summary: 'issue',
    evidence: 'evidence',
    ...over,
  });
  const lines: DiffLine[] = [
    { kind: 'context', oldNo: 1, newNo: 1, text: 'a' },
    { kind: 'deletion', oldNo: 2, text: 'b' },
    { kind: 'addition', newNo: 2, text: 'c' },
    { kind: 'context', oldNo: 3, newNo: 3, text: 'd' },
    { kind: 'meta', text: '\\ No newline at end of file' },
  ];

  // Side-specific row matching: a NEW finding marks only rows carrying that
  // new line number (additions + context), an OLD finding only deletions +
  // context; meta rows never match.
  check(
    'a new-side finding marks the addition row carrying that line number',
    findingRowIndexes(lines, { side: 'new', startLine: 2, endLine: 2 }).join(',') === '2',
  );
  check(
    'an old-side finding marks the deletion row carrying that line number',
    findingRowIndexes(lines, { side: 'old', startLine: 2, endLine: 2 }).join(',') === '1',
  );
  check(
    'a wide old-side range spans context and deletion, never additions or meta',
    findingRowIndexes(lines, { side: 'old', startLine: 1, endLine: 3 }).join(',') === '0,1,3',
  );
  check(
    'a wide new-side range spans context and addition, never deletions or meta',
    findingRowIndexes(lines, { side: 'new', startLine: 1, endLine: 3 }).join(',') === '0,2,3',
  );
  check(
    'line numbers outside the hunk match nothing',
    findingRowIndexes(lines, { side: 'new', startLine: 40, endLine: 41 }).length === 0,
  );

  // Multiple issues in the SAME hunk stay distinct findings of one result.
  const sameHunk: HunkReviewResult = {
    identity: identity(),
    findings: [
      finding({ category: 'naming', side: 'new', startLine: 2, endLine: 2, summary: 'naming' }),
      finding({ category: 'critical', side: 'new', startLine: 2, endLine: 3, summary: 'critical' }),
      finding({ category: 'overdesign', side: 'old', startLine: 2, endLine: 2, summary: 'overdesign' }),
    ],
  };
  check(
    'findings resolve only for the EXACT identity (all seven fields)',
    findingsForIdentity([sameHunk], identity()).length === 3
      && findingsForIdentity([sameHunk], identity({ contentHash: 'h-x' })).length === 0
      && findingsForIdentity([sameHunk], identity({ oldStart: 2 })).length === 0
      && findingsForIdentity([sameHunk], identity({ path: 'src/b.rs' })).length === 0
      && findingsForIdentity([sameHunk], identity({ snapshotId: 's2' })).length === 0
      && findingsForIdentity(null, identity()).length === 0,
  );

  // Global order: file, hunk, first matching row, last matching row, old
  // before new. Same hunk: the one-row naming issue sorts before the wider
  // critical range that starts on the same row.
  const ordered = orderReviewFindings([{ path: 'src/a.rs' }], [sameHunk], [
    { identity: identity(), lines },
  ]);
  check(
    'multiple issues in one hunk order by row span (old before new on ties)',
    ordered.map((entry) => entry.finding.summary).join(',') === 'overdesign,naming,critical',
  );
  check(
    'ordered entries carry the exact stored indexes',
    ordered[0].resultIndex === 0 && ordered[0].findingIndex === 2
      && ordered[1].findingIndex === 0
      && ordered[2].findingIndex === 1,
  );

  // Cross-file: file order dominates row numbers.
  const fileB: HunkReviewResult = {
    identity: identity({ path: 'src/lib/b.rs', contentHash: 'h-b' }),
    findings: [finding({ side: 'new', startLine: 1, endLine: 1, summary: 'b' })],
  };
  const cross = orderReviewFindings(
    [{ path: 'src/lib/b.rs' }, { path: 'src/a.rs' }],
    [sameHunk, fileB],
    [
      { identity: identity(), lines },
      { identity: identity({ path: 'src/lib/b.rs', contentHash: 'h-b' }), lines },
    ],
  );
  check(
    'file order dominates row numbers across files',
    cross[0].identity.path === 'src/lib/b.rs' && cross[cross.length - 1].identity.path === 'src/a.rs',
  );
  check(
    'findings whose path is not in the file list are rejected',
    orderReviewFindings([], [sameHunk], [{ identity: identity(), lines }]).length === 0
      && orderReviewFindings(
        [{ path: 'src/a.rs' }],
        [sameHunk, fileB],
        [
          { identity: identity(), lines },
          { identity: identity({ path: 'src/lib/b.rs', contentHash: 'h-b' }), lines },
        ],
      ).every((entry) => entry.identity.path === 'src/a.rs'),
  );

  // Unloaded bodies: a hunk without a loaded body sorts after loaded rows.
  const unloaded = orderReviewFindings(
    [{ path: 'src/a.rs' }, { path: 'src/lib/b.rs' }],
    [sameHunk, fileB],
    [{ identity: identity(), lines }],
  );
  check(
    'findings of unloaded hunks sort after loaded rows',
    unloaded[unloaded.length - 1].identity.path === 'src/lib/b.rs',
  );

  // Previous/next: no wrapping; nothing selected steps to an endpoint.
  const ref = (entry: (typeof ordered)[number]): ReviewFindingRef => ({
    identity: entry.identity,
    findingIndex: entry.findingIndex,
  });
  check(
    'next with nothing selected lands on the first finding, previous on the last',
    navigateReviewFinding(ordered, null, 'next') === ordered[0]
      && navigateReviewFinding(ordered, null, 'previous') === ordered[2],
  );
  check(
    'a middle selection steps both ways',
    navigateReviewFinding(ordered, ref(ordered[1]), 'next') === ordered[2]
      && navigateReviewFinding(ordered, ref(ordered[1]), 'previous') === ordered[0],
  );
  check(
    'endpoints hold (no wrap)',
    navigateReviewFinding(ordered, ref(ordered[0]), 'previous') === null
      && navigateReviewFinding(ordered, ref(ordered[2]), 'next') === null,
  );
  check('an empty collection never navigates', navigateReviewFinding([], null, 'next') === null);

  // Selection invalidation: a replaced hunk clears, unrelated hunks keep.
  const selected: ReviewFindingRef = { identity: identity(), findingIndex: 0 };
  check(
    'a valid ref survives an unrelated hunk appearing',
    invalidateReviewFindingRef([sameHunk], [sameHunk, fileB], selected) !== null,
  );
  check(
    'a replaced findings array clears the selection',
    invalidateReviewFindingRef(
      [sameHunk],
      [{ identity: identity(), findings: [finding({ summary: 'replaced' })] }],
      selected,
    ) === null,
  );
  check('an absent next collection clears the selection', invalidateReviewFindingRef([sameHunk], null, selected) === null);
  check(
    'only an in-range index is a valid ref',
    !reviewFindingRefValid([sameHunk], { identity: identity(), findingIndex: 3 })
      && reviewFindingRefValid([sameHunk], selected),
  );
}

// ---- review components render the findings props; the hunk composer keeps
//      the main-composer icons ----
{
  const here = dirname(fileURLToPath(import.meta.url));
  const css = readFileSync(join(here, '../src/styles.css'), 'utf8');
  const identity = (over: Partial<HunkIdentity> = {}): HunkIdentity => ({
    snapshotId: 's1',
    path: 'src/a.rs',
    oldStart: 1,
    oldCount: 5,
    newStart: 1,
    newCount: 5,
    contentHash: 'h-a',
    ...over,
  });
  const finding = (over: Partial<ReviewFinding> = {}): ReviewFinding => ({
    category: 'naming',
    side: 'new',
    startLine: 2,
    endLine: 2,
    summary: 'issue',
    evidence: 'evidence',
    ...over,
  });
  check(
    'model and Chinese language selects are centered',
    /\.code-review__model-select,\s*\n\.code-review__auto-lang \{[\s\S]*?text-align:\s*center/.test(css)
      && /\.code-review__model-select,\s*\n\.code-review__auto-lang \{[\s\S]*?text-align-last:\s*center/.test(css),
  );
  check(
    'composer actions wrap at 390px',
    /@media \(max-width: 390px\) \{[\s\S]*?\.code-review__composer-actions \{[\s\S]*?flex-wrap:\s*wrap/.test(css),
  );

  const thread = (over: Partial<ReviewThread> = {}): ReviewThread => ({
    identity: identity(),
    comments: [],
    streamingText: '',
    streamingThinking: '',
    error: null,
    stale: false,
    isStreaming: false,
    isCancellable: false,
    generation: 0,
    replyId: '',
    replyStatus: 'idle',
    toolCalls: [],
    ...over,
  });
  const imageChip: ComposerAttachment = {
    id: 'att-1',
    name: 'shot.png',
    size: 12,
    mimeType: 'image/png',
    kind: 'image',
    previewUrl: 'blob:review-shot',
  };
  const dockHtml = (over: Partial<CodeReviewThreadDockProps> = {}): string => renderToStaticMarkup(createElement(CodeReviewThreadDock, {
    selectedIdentity: identity(),
    selectedThread: undefined,
    selectedDraft: '',
    composerKey: 'h-a',
    composerRef: { current: null },
    isNarrow: false,
    isHidden: false,
    busy: null,
    sessionId: null,
    acceptedFollow: null,
    snapshotId: 's1',
    reviewResults: null,
    selectedFinding: null,
    onSelectFinding: () => {},
    onBackToDiff: () => {},
    onRefresh: () => {},
    onComposerInput: () => {},
    onDraftCommit: () => {},
    onComposerKeyDown: () => {},
    onSubmitComment: () => {},
    onAbort: () => {},
    attachments: [],
    attachmentAccept: imageAttachmentAccept(),
    isRecording: false,
    isTranscribing: false,
    onAttachFiles: () => {},
    onRemoveAttachment: () => {},
    onToggleRecording: () => {},
    ...over,
  }));
  /** React SSR splits adjacent text with comments. */
  const flat = (html: string): string => html.replace(/<!--.*?-->/g, '');
  /** Compare the visible text of a fragment, not the markup around it. */
  const visibleText = (html: string): string => flat(html)
    .replace(/<[^>]*>/g, ' ')
    .replace(/\s+/g, ' ')
    .trim();
  const issueNav = (html: string): { text: string; previousDisabled: boolean; nextDisabled: boolean } => {
    const nav = html.match(/<div[^>]*aria-label="Issue navigation"[^>]*>[\s\S]*?<\/div>/)?.[0] ?? '';
    const buttonDisabled = (label: string): boolean => {
      const button = nav.match(new RegExp(`<button\\b(?=[^>]*aria-label="${label}")[^>]*>`))?.[0] ?? '';
      return /\sdisabled(?:=|\s|>)/.test(button);
    };
    return {
      text: visibleText(nav),
      previousDisabled: buttonDisabled('Previous issue'),
      nextDisabled: buttonDisabled('Next issue'),
    };
  };

  const idle = dockHtml();
  const recording = dockHtml({ isRecording: true });
  const transcribing = dockHtml({ isTranscribing: true });
  const locked = dockHtml({ busy: 'comment' });
  const withChip = dockHtml({ attachments: [imageChip] });
  const cancellable = dockHtml({ selectedThread: thread({ isCancellable: true }) });
  const attachBtn = (html: string) => html.match(/<button[^>]*class="[^"]*code-review__composer-btn--attach[^"]*"[^>]*>[\s\S]*?<\/button>/)?.[0] ?? '';
  const micBtn = (html: string) => html.match(/<button[^>]*class="[^"]*code-review__composer-btn--mic[^"]*"[^>]*>[\s\S]*?<\/button>/)?.[0] ?? '';

  check(
    'hunk composer keeps the submit glyph and Stop language of the main composer',
    idle.includes('➤')
      && idle.includes('Comment')
      && cancellable.includes('code-review__action--stop')
      && cancellable.includes('Stop generating (Esc)')
      && cancellable.includes('■')
      && cancellable.includes('Stop'),
  );
  check(
    'the dock renders no per-hunk model select and no bare placeholder',
    !idle.includes('code-review__model-select') && !idle.includes('model…'),
  );
  check(
    'attach and mic affordances use the shared establish glyphs',
    attachBtn(idle).includes('📎') && micBtn(idle).includes('🎤'),
  );
  check(
    'no unsupported placeholder remains on attach or mic',
    !idle.includes('not supported')
      && !idle.includes('Attaching files')
      && !idle.includes('Voice-to-text in the hunk composer'),
  );
  check(
    'image picker is hidden, multiple, and uses the accept prop',
    /<input[^>]*type="file"[^>]*>/.test(idle)
      && /<input[^>]*type="file"[^>]*accept="image\/png,image\/jpeg,image\/gif,image\/webp"/.test(idle)
      && /<input[^>]*type="file"[^>]*multiple/.test(idle)
      && /<input[^>]*type="file"[^>]*hidden/.test(idle),
  );
  check(
    'attach and mic are enabled when the composer is available',
    !attachBtn(idle).includes('disabled') && !micBtn(idle).includes('disabled'),
  );
  check(
    'attach and mic lock with the composer',
    attachBtn(locked).includes('disabled') && micBtn(locked).includes('disabled'),
  );
  check(
    'composer order is attach then mic then Comment (model select lives in the panel head)',
    (() => {
      const attachAt = idle.indexOf('code-review__composer-btn--attach');
      const micAt = idle.indexOf('code-review__composer-btn--mic');
      const sendAt = idle.indexOf('code-review__action--send');
      return attachAt >= 0 && micAt > attachAt && sendAt > micAt;
    })(),
  );
  check(
    'cancellable Stop replaces Comment as the last primary action',
    (() => {
      const attachAt = cancellable.indexOf('code-review__composer-btn--attach');
      const micAt = cancellable.indexOf('code-review__composer-btn--mic');
      const stopAt = cancellable.indexOf('code-review__action--stop');
      return attachAt >= 0 && micAt > attachAt && stopAt > micAt
        && !cancellable.includes('code-review__action--send');
    })(),
  );
  check(
    'idle mic uses the shared hold-to-talk accessible state',
    micBtn(idle).includes('aria-label="Microphone"')
      && micBtn(idle).includes('title="Hold to talk (voice to text)"')
      && micBtn(idle).includes('aria-pressed="false"')
      && !/\brecording\b/.test(micBtn(idle).match(/class="[^"]*"/)?.[0] ?? '')
      && !/\btranscribing\b/.test(micBtn(idle).match(/class="[^"]*"/)?.[0] ?? ''),
  );
  check(
    'recording mic uses the shared recording accessible state',
    micBtn(recording).includes('aria-pressed="true"')
      && micBtn(recording).includes('title="Recording… release to transcribe"')
      && /\brecording\b/.test(micBtn(recording).match(/class="[^"]*"/)?.[0] ?? '')
      && !micBtn(recording).includes('disabled'),
  );
  check(
    'transcribing mic uses the shared transcribing accessible state',
    micBtn(transcribing).includes('title="Transcribing…"')
      && /\btranscribing\b/.test(micBtn(transcribing).match(/class="[^"]*"/)?.[0] ?? '')
      && micBtn(transcribing).includes('disabled')
      && !/\brecording\b/.test(micBtn(transcribing).match(/class="[^"]*"/)?.[0] ?? ''),
  );
  check(
    'attachment chips render image thumb, name, and remove',
    withChip.includes('code-review__composer-attachments')
      && withChip.includes('class="composer-attachment"')
      && withChip.includes('class="composer-attachment__thumb"')
      && withChip.includes('src="blob:review-shot"')
      && withChip.includes('class="composer-attachment__name"')
      && withChip.includes('shot.png')
      && withChip.includes('class="composer-attachment__remove"')
      && withChip.includes('aria-label="Remove shot.png"'),
  );
  check(
    'the hunk picker accept mirrors the shared image-only MIME set',
    imageAttachmentAccept() === 'image/png,image/jpeg,image/gif,image/webp',
  );
  check(
    'the hunk composer renders no model status node',
    !idle.includes('code-review__composer-status'),
  );

  // Finding buttons: derived from the selected hunk's findings, rendered
  // ABOVE the manual thread, with exact selectedIdentity/index marking.
  const dockResults: HunkReviewResult[] = [{
    identity: identity(),
    findings: [
      finding({ category: 'naming', summary: 'banned alias' }),
      finding({ category: 'critical', startLine: 2, endLine: 3, summary: 'unsafe unwrap' }),
    ],
  }];
  const dockIssues = dockHtml({
    reviewResults: dockResults,
    selectedFinding: { identity: identity(), findingIndex: 1 },
  });
  check(
    'the dock renders the hunk finding buttons above the manual thread',
    dockIssues.includes('code-review__finding-list')
      && dockIssues.indexOf('code-review__finding-list') < dockIssues.indexOf('class="code-review__thread"')
      && (dockIssues.match(/class="code-review__finding"/g) ?? []).length === 1
      && dockIssues.includes('class="code-review__finding is-selected"'),
  );
  check(
    'finding buttons label category and side range',
    flat(dockIssues).includes('naming · new L2–2') && flat(dockIssues).includes('critical · new L2–3'),
  );
  check(
    'only the exact selectedIdentity/index is marked selected',
    !dockHtml({
      reviewResults: dockResults,
      selectedFinding: { identity: identity({ contentHash: 'h-other' }), findingIndex: 1 },
    }).includes('class="code-review__finding is-selected"'),
  );
  check(
    'an unreviewed hunk renders no finding list',
    !dockHtml().includes('code-review__finding-list'),
  );

  // DiffPane: side-range row marks, selected-row emphasis, data-line-index
  // for parent scroll, and the issue navigation count.
  const diffLines: DiffLine[] = [
    { kind: 'context', oldNo: 1, newNo: 1, text: 'fn a() {' },
    { kind: 'deletion', oldNo: 2, text: 'let x = 1;' },
    { kind: 'addition', newNo: 2, text: 'let x = 2;' },
    { kind: 'context', oldNo: 3, newNo: 3, text: '}' },
  ];
  const diffFile: DiffFile = {
    path: 'src/a.rs',
    status: 'modified',
    binary: false,
    insertions: 1,
    deletions: 1,
    bodyState: 'complete',
    truncated: false,
    hunks: [{
      header: '@@ -1,3 +1,3 @@',
      oldStart: 1,
      oldCount: 3,
      newStart: 1,
      newCount: 3,
      contentHash: 'h-a',
      lines: diffLines,
    }],
  };
  const diffIdentity = hunkIdentityFor('s1', diffFile, diffFile.hunks[0]);
  const diffResults: HunkReviewResult[] = [{
    identity: diffIdentity,
    findings: [
      finding({ category: 'critical', startLine: 2, endLine: 2, summary: 'unsafe unwrap' }),
      finding({ category: 'naming', side: 'old', startLine: 2, endLine: 2, summary: 'bad name' }),
    ],
  }];
  const paneHtml = (over: Partial<CodeReviewDiffPaneProps> = {}): string => renderToStaticMarkup(createElement(CodeReviewDiffPane, {
    selectedFile: diffFile,
    loadedDiff: null,
    windowLimit: 100,
    filesCount: 1,
    snapshotId: 's1',
    threads: [],
    selectedHunkKey: null,
    collapsedHunks: new Set<string>(),
    isHidden: false,
    isLoadingMore: false,
    loadError: null,
    onNearBottom: () => {},
    onRetry: () => {},
    onSelectHunk: () => {},
    onNavigateHunk: () => {},
    onToggleHunkCollapsed: () => {},
    gapStates: {},
    gapLines: {},
    oldLineCount: 10,
    newLineCount: 10,
    expandingGapId: null,
    onExpandGap: () => {},
    surface: 'source',
    onSurfaceChange: () => {},
    preview: null,
    reviewResults: null,
    selectedFinding: null,
    onPreviousIssue: () => {},
    onNextIssue: () => {},
    issueIndex: -1,
    issueCount: 0,
    ...over,
  }));
  const plainPane = paneHtml();
  const markedPane = flat(paneHtml({ reviewResults: diffResults }));
  check('an unreviewed file renders no finding marks', !plainPane.includes('has-finding--'));
  check(
    'the new-side critical row wears the danger tone at its line index',
    /code-review__line--addition has-finding--danger"[^>]*data-line-index="2"/.test(paneHtml({ reviewResults: diffResults })),
  );
  check(
    'the old-side naming row marks the deletion row',
    /code-review__line--deletion has-finding--info"/.test(paneHtml({ reviewResults: diffResults })),
  );
  check(
    'no whole-hunk tint, banner, or aggregate verdict remains',
    !markedPane.includes('has-highlight')
      && !markedPane.includes('hunk-highlight')
      && !markedPane.includes('hunk-verdict'),
  );
  const openNav = issueNav(paneHtml({
    reviewResults: diffResults,
    issueIndex: -1,
    issueCount: 2,
  }));
  check(
    'issue navigation renders the parent-supplied position and count',
    openNav.text.includes('\u2191 0 / 2 \u2193'),
    `got ${JSON.stringify(openNav.text)}`,
  );
  const closedNav = issueNav(plainPane);
  check(
    'nothing selected enables both directions when issues exist; zero disables both',
    !openNav.previousDisabled
      && !openNav.nextDisabled
      && closedNav.previousDisabled
      && closedNav.nextDisabled
      && closedNav.text.includes('\u2191 0 / 0 \u2193'),
    `open=${JSON.stringify(openNav)} closed=${JSON.stringify(closedNav)}`,
  );
  const firstNav = issueNav(paneHtml({ reviewResults: diffResults, issueIndex: 0, issueCount: 2 }));
  const lastNav = issueNav(paneHtml({ reviewResults: diffResults, issueIndex: 1, issueCount: 2 }));
  check(
    'a selected endpoint disables only that direction',
    firstNav.previousDisabled
      && !firstNav.nextDisabled
      && firstNav.text.includes('\u2191 1 / 2 \u2193')
      && !lastNav.previousDisabled
      && lastNav.nextDisabled
      && lastNav.text.includes('\u2191 2 / 2 \u2193'),
    `first=${JSON.stringify(firstNav)} last=${JSON.stringify(lastNav)}`,
  );
  check(
    'the selected finding rows gain emphasis only for the exact ref',
    paneHtml({ reviewResults: diffResults, selectedFinding: { identity: diffIdentity, findingIndex: 0 } }).includes('is-finding-selected')
      && !paneHtml({ reviewResults: diffResults, selectedFinding: { identity: { ...diffIdentity, contentHash: 'h-other' }, findingIndex: 0 } }).includes('is-finding-selected'),
  );

  // FileList: issue badges derive per file and per directory from the
  // findings-derived aggregate maps, across files.
  const listFiles: DiffFile[] = [
    diffFile,
    {
      path: 'src/lib/b.rs',
      status: 'added',
      binary: false,
      insertions: 2,
      deletions: 0,
      bodyState: 'complete',
      truncated: false,
      hunks: [],
    },
  ];
  const listTree: FileTree = {
    nodes: [
      { id: 'd-src', name: 'src', path: 'src', kind: 'dir', depth: 0, children: [1, 2], insertions: 0, deletions: 0 },
      { id: 'd-lib', name: 'lib', path: 'src/lib', kind: 'dir', depth: 1, children: [3], insertions: 0, deletions: 0 },
      { id: 'f-a', name: 'a.rs', path: 'src/a.rs', kind: 'file', depth: 1, children: [], fileIndex: 0, status: 'modified', insertions: 1, deletions: 1 },
      { id: 'f-b', name: 'b.rs', path: 'src/lib/b.rs', kind: 'file', depth: 2, children: [], fileIndex: 1, status: 'added', insertions: 2, deletions: 0 },
    ],
    roots: [0],
    collapsed: new Set<string>(),
  };
  const listRows: VisibleTreeRow[] = [
    { nodeIndex: 0, depth: 0, expanded: true, isDir: true },
    { nodeIndex: 1, depth: 1, expanded: true, isDir: true },
    { nodeIndex: 2, depth: 1, expanded: false, isDir: false },
    { nodeIndex: 3, depth: 2, expanded: false, isDir: false },
  ];
  const listHtml = flat(renderToStaticMarkup(createElement(CodeReviewFileList, {
    files: listFiles,
    tree: listTree,
    rows: listRows,
    threads: [],
    autoByFile: new Map<string, AutoReviewAggregate>([
      ['src/a.rs', { issues: 3, category: 'critical' }],
      ['src/lib/b.rs', { issues: 1, category: 'overdesign' }],
    ]),
    autoByDir: new Map<string, AutoReviewAggregate>([
      ['src', { issues: 4, category: 'critical' }],
      ['src/lib', { issues: 1, category: 'overdesign' }],
    ]),
    selectedPath: 'src/a.rs',
    fileQuery: '',
    busy: null,
    isHidden: false,
    rowButtonRefs: { current: [] },
    onFileQueryChange: () => {},
    onSelectFile: () => {},
    onToggleDir: () => {},
    onFileListKeyDown: () => {},
  })));
  check(
    'file badges derive per-file count and category tone across files',
    listHtml.includes('code-review__file-row--danger')
      && listHtml.includes('code-review__file-row--warn')
      && listHtml.includes('modified: src/a.rs · 3 review issues')
      && listHtml.includes('added: src/lib/b.rs · 1 review issue'),
  );
  check('directory rows cascade the subtree badge', listHtml.includes('title="4 review issues"'));

  // Explorer pane shimmer hooks (markup owned by FileExplorerPanel.tsx):
  // 12px captionless square, absolute centered (no reflow), opacity pulse,
  // static under reduced motion.
  check(
    'explorer pane shimmer is a captionless 12px square, absolutely centered',
    /\.file-explorer__pane-shimmer\s*{[^}]*width:\s*12px[^}]*height:\s*12px[^}]*(animation:\s*file-explorer-pane-shimmer|animation:\s*1)/.test(css)
      && /\.file-explorer__pane-loading\s*{[^}]*position:\s*absolute/.test(css),
  );
  check(
    'explorer pane shimmer is opacity-only and static under reduced motion',
    /@keyframes file-explorer-pane-shimmer\s*{[^}]*opacity/.test(css)
      && /prefers-reduced-motion: reduce[^{]*{[^}]*\.file-explorer__pane-shimmer\s*{[^}]*animation:\s*none/.test(css),
  );
}

type MiniListener = (event: MiniEvent) => void;
type MiniEvent = {
  type: string;
  target: MiniNode;
  currentTarget: MiniNode | null;
  bubbles: boolean;
  cancelable: boolean;
  defaultPrevented: boolean;
  button: number;
  preventDefault(): void;
  stopPropagation(): void;
  stopImmediatePropagation(): void;
};
type MiniNode = {
  nodeType: number;
  nodeName: string;
  tagName?: string;
  namespaceURI: string | null;
  ownerDocument: MiniDocument;
  parentNode: MiniNode | null;
  childNodes: MiniNode[];
  firstChild: MiniNode | null;
  lastChild: MiniNode | null;
  nextSibling: MiniNode | null;
  previousSibling: MiniNode | null;
  attributes: Map<string, string>;
  style: Record<string, string>;
  listeners: Map<string, Set<MiniListener>>;
  text: string;
  value: string;
  selected?: boolean;
  files: FileList | null;
  innerHTML: string;
  textContent: string;
  className: string;
  id: string;
  click(): void;
  hasAttribute(name: string): boolean;
  querySelector(selector: string): MiniNode | null;
  querySelectorAll(selector: string): MiniNode[];
  appendChild(node: MiniNode): MiniNode;
  setAttribute(name: string, value: string): void;
  getAttribute(name: string): string | null;
  dispatchEvent(event: { type: string; target?: MiniNode }): boolean;
};
type MiniDocument = MiniNode & {
  nodeType: 9;
  body: MiniNode;
  defaultView: MiniWindow;
  createElement(tag: string): MiniNode;
  createElementNS(ns: string, tag: string): MiniNode;
  createTextNode(text: string): MiniNode;
  createComment(text: string): MiniNode;
  createDocumentFragment(): MiniNode;
  getElementById(id: string): MiniNode | null;
};
type MiniWindow = {
  document: MiniDocument;
  HTMLElement: new () => object;
  HTMLIFrameElement: new () => object;
  HTMLInputElement: new () => object;
  HTMLButtonElement: new () => object;
  HTMLTextAreaElement: new () => object;
  SVGElement: new () => object;
  Element: new () => object;
  Node: new () => object;
  Text: new () => object;
  Comment: new () => object;
  DocumentFragment: new () => object;
  setTimeout: (fn: () => void, ms?: number) => number;
  clearTimeout: (id: number) => void;
  addEventListener(): void;
  removeEventListener(): void;
};

function createMiniDom(): { window: MiniWindow; document: MiniDocument } {
  class FakeCtor {}
  const relink = (parent: MiniNode) => {
    parent.firstChild = parent.childNodes[0] ?? null;
    parent.lastChild = parent.childNodes[parent.childNodes.length - 1] ?? null;
    for (let i = 0; i < parent.childNodes.length; i += 1) {
      const child = parent.childNodes[i];
      child.parentNode = parent;
      child.previousSibling = parent.childNodes[i - 1] ?? null;
      child.nextSibling = parent.childNodes[i + 1] ?? null;
    }
  };
  const makeNode = (nodeType: number, nodeName: string, owner: MiniDocument | null): MiniNode => {
    const node = {
      nodeType,
      nodeName,
      tagName: nodeType === 1 ? nodeName : undefined,
      namespaceURI: nodeType === 1 ? 'http://www.w3.org/1999/xhtml' : null,
      ownerDocument: owner as MiniDocument,
      parentNode: null,
      childNodes: [] as MiniNode[],
      firstChild: null,
      lastChild: null,
      nextSibling: null,
      previousSibling: null,
      attributes: new Map<string, string>(),
      style: {},
      listeners: new Map(),
      text: '',
      innerHTML: '',
      value: '',
      selected: false,
      files: null as FileList | null,
      get nodeValue() {
        return nodeType === 3 || nodeType === 8 ? node.text : null;
      },
      set nodeValue(value: string | null) {
        if (nodeType === 3 || nodeType === 8) node.text = value ?? '';
      },
      get textContent() {
        if (nodeType === 3 || nodeType === 8) return node.text;
        return node.childNodes.map((child) => child.textContent).join('');
      },
      set textContent(value: string) {
        node.childNodes = [];
        node.firstChild = null;
        node.lastChild = null;
        if (nodeType === 3 || nodeType === 8) {
          node.text = value;
          return;
        }
        if (value !== '') node.appendChild(ownerDocument.createTextNode(value));
      },
      get className() {
        return node.getAttribute('class') ?? '';
      },
      set className(value: string) {
        node.setAttribute('class', value);
      },
      get id() {
        return node.getAttribute('id') ?? '';
      },
      set id(value: string) {
        node.setAttribute('id', value);
      },
      get type() {
        return node.getAttribute('type') ?? '';
      },
      set type(value: string) {
        node.setAttribute('type', value);
      },
      get accept() {
        return node.getAttribute('accept') ?? '';
      },
      set accept(value: string) {
        node.setAttribute('accept', value);
      },
      get multiple() {
        return node.hasAttribute('multiple');
      },
      set multiple(value: boolean) {
        if (value) node.setAttribute('multiple', '');
        else node.removeAttribute('multiple');
      },
      get hidden() {
        return node.hasAttribute('hidden');
      },
      set hidden(value: boolean) {
        if (value) node.setAttribute('hidden', '');
        else node.removeAttribute('hidden');
      },
      get disabled() {
        return node.hasAttribute('disabled');
      },
      set disabled(value: boolean) {
        if (value) node.setAttribute('disabled', '');
        else node.removeAttribute('disabled');
      },
      appendChild(child: MiniNode) {
        return node.insertBefore(child, null);
      },
      removeChild(child: MiniNode) {
        const index = node.childNodes.indexOf(child);
        if (index < 0) throw new Error('not a child');
        node.childNodes.splice(index, 1);
        child.parentNode = null;
        child.nextSibling = null;
        child.previousSibling = null;
        relink(node);
        return child;
      },
      insertBefore(child: MiniNode, before: MiniNode | null) {
        if (child.parentNode) child.parentNode.removeChild(child);
        if (child.nodeType === 11) {
          const fragmentChildren = [...child.childNodes];
          child.childNodes = [];
          relink(child);
          for (const nested of fragmentChildren) node.insertBefore(nested, before);
          return child;
        }
        const index = before === null ? node.childNodes.length : node.childNodes.indexOf(before);
        node.childNodes.splice(index < 0 ? node.childNodes.length : index, 0, child);
        relink(node);
        return child;
      },
      remove() {
        node.parentNode?.removeChild(node);
      },
      setAttribute(name: string, value: string) {
        node.attributes.set(name, String(value));
      },
      getAttribute(name: string) {
        return node.attributes.has(name) ? node.attributes.get(name)! : null;
      },
      removeAttribute(name: string) {
        node.attributes.delete(name);
      },
      hasAttribute(name: string) {
        return node.attributes.has(name);
      },
      setAttributeNS(_ns: string | null, name: string, value: string) {
        node.setAttribute(name, value);
      },
      getAttributeNS(_ns: string | null, name: string) {
        return node.getAttribute(name);
      },
      addEventListener(type: string, fn: MiniListener, options?: boolean | { capture?: boolean }) {
        const capture = options === true || (typeof options === 'object' && options.capture === true);
        const key = capture ? `${type}-capture` : type;
        const set = node.listeners.get(key) ?? new Set();
        set.add(fn);
        node.listeners.set(key, set);
      },
      removeEventListener(type: string, fn: MiniListener, options?: boolean | { capture?: boolean }) {
        const capture = options === true || (typeof options === 'object' && options.capture === true);
        const key = capture ? `${type}-capture` : type;
        node.listeners.get(key)?.delete(fn);
      },
      dispatchEvent(raw: { type: string; target?: MiniNode }) {
        const payload: MiniEvent = {
          type: raw.type,
          target: raw.target ?? node,
          currentTarget: null,
          bubbles: true,
          cancelable: true,
          defaultPrevented: false,
          button: 0,
          preventDefault() { payload.defaultPrevented = true; },
          stopPropagation() { stopped = true; },
          stopImmediatePropagation() { stopped = true; immediate = true; },
        };
        let stopped = false;
        let immediate = false;
        const path: MiniNode[] = [];
        let current: MiniNode | null = node;
        while (current !== null) {
          path.push(current);
          current = current.parentNode;
        }
        const fire = (entry: MiniNode, key: string) => {
          payload.currentTarget = entry;
          const set = entry.listeners.get(key);
          if (!set) return;
          for (const fn of [...set]) {
            fn(payload);
            if (immediate) return;
          }
        };
        for (let i = path.length - 1; i >= 0; i -= 1) {
          fire(path[i], `${raw.type}-capture`);
          if (stopped) return !payload.defaultPrevented;
        }
        fire(node, raw.type);
        if (stopped) return !payload.defaultPrevented;
        for (let i = 1; i < path.length; i += 1) {
          fire(path[i], raw.type);
          if (stopped) return !payload.defaultPrevented;
        }
        return !payload.defaultPrevented;
      },
      click() {
        node.dispatchEvent({ type: 'click', target: node });
      },
      focus() {},
      blur() {},
      contains(other: MiniNode | null) {
        if (other === node) return true;
        return node.childNodes.some((child) => child.contains(other));
      },
      querySelector(selector: string) {
        return node.querySelectorAll(selector)[0] ?? null;
      },
      querySelectorAll(selector: string) {
        const out: MiniNode[] = [];
        const visit = (current: MiniNode) => {
          if (current.nodeType === 1 && matches(current, selector)) out.push(current);
          for (const child of current.childNodes) visit(child);
        };
        for (const child of node.childNodes) visit(child);
        return out;
      },
      cloneNode(deep = false) {
        const clone = makeNode(node.nodeType, node.nodeName, ownerDocument);
        clone.namespaceURI = node.namespaceURI;
        clone.text = node.text;
        for (const [name, value] of node.attributes) clone.attributes.set(name, value);
        if (deep) {
          for (const child of node.childNodes) clone.appendChild(child.cloneNode(true) as MiniNode);
        }
        return clone;
      },
    } as MiniNode;
    if (nodeName === 'OPTION') {
      Object.defineProperty(node, 'value', {
        get() {
          return node.getAttribute('value') ?? '';
        },
        set(nextValue: string) {
          node.setAttribute('value', nextValue);
        },
      });
    }
    if (nodeName === 'SELECT') {
      Object.defineProperty(node, 'options', {
        get() {
          return node.querySelectorAll('option');
        },
      });
      Object.defineProperty(node, 'value', {
        get() {
          const selected = node.querySelectorAll('option').find((option) => option.selected);
          return selected ? (selected.getAttribute('value') ?? '') : '';
        },
        set(nextValue: string) {
          for (const option of node.querySelectorAll('option')) {
            option.selected = (option.getAttribute('value') ?? '') === nextValue;
          }
        },
      });
    }
    return node;
  };
  const matches = (node: MiniNode, selector: string): boolean => {
    if (selector.startsWith('.')) return node.className.split(/\s+/).includes(selector.slice(1));
    if (selector.startsWith('#')) return node.id === selector.slice(1);
    if (selector.startsWith('[') && selector.endsWith(']')) {
      const body = selector.slice(1, -1);
      const eq = body.indexOf('=');
      if (eq < 0) return node.hasAttribute(body);
      const name = body.slice(0, eq);
      const raw = body.slice(eq + 1).replace(/^["']|["']$/g, '');
      return node.getAttribute(name) === raw;
    }
    return node.tagName === selector.toUpperCase();
  };
  const document = makeNode(9, '#document', null) as MiniDocument;
  const ownerDocument = document;
  document.ownerDocument = document;
  const htmlEl = makeNode(1, 'HTML', document);
  const head = makeNode(1, 'HEAD', document);
  const body = makeNode(1, 'BODY', document);
  document.appendChild(htmlEl);
  htmlEl.appendChild(head);
  htmlEl.appendChild(body);
  document.documentElement = htmlEl;
  document.head = head;
  document.body = body;
  document.createElement = (tag: string) => makeNode(1, tag.toUpperCase(), document);
  document.createElementNS = (ns: string, tag: string) => {
    const created = makeNode(1, tag.toUpperCase(), document);
    created.namespaceURI = ns;
    return created;
  };
  document.createTextNode = (text: string) => {
    const created = makeNode(3, '#text', document);
    created.text = text;
    return created;
  };
  document.createComment = (text: string) => {
    const created = makeNode(8, '#comment', document);
    created.text = text;
    return created;
  };
  document.createDocumentFragment = () => makeNode(11, '#document-fragment', document);
  document.getElementById = (id: string) => document.querySelector(`#${id}`);
  const windowObj: MiniWindow = {
    document,
    HTMLElement: FakeCtor,
    HTMLIFrameElement: FakeCtor,
    HTMLInputElement: FakeCtor,
    HTMLButtonElement: FakeCtor,
    HTMLTextAreaElement: FakeCtor,
    SVGElement: FakeCtor,
    Element: FakeCtor,
    Node: FakeCtor,
    Text: FakeCtor,
    Comment: FakeCtor,
    DocumentFragment: FakeCtor,
    setTimeout: (fn: () => void) => globalThis.setTimeout(fn, 0) as unknown as number,
    clearTimeout: (id: number) => globalThis.clearTimeout(id),
    addEventListener() {},
    removeEventListener() {},
  };
  document.defaultView = windowObj;
  return { window: windowObj, document };
}

function installMiniDom() {
  const mini = createMiniDom();
  Object.defineProperty(globalThis, 'window', { configurable: true, writable: true, value: mini.window });
  Object.defineProperty(globalThis, 'document', { configurable: true, writable: true, value: mini.document });
  globalThis.HTMLElement = mini.window.HTMLElement as unknown as typeof HTMLElement;
  globalThis.SVGElement = mini.window.SVGElement as unknown as typeof SVGElement;
  globalThis.Element = mini.window.Element as unknown as typeof Element;
  globalThis.Node = mini.window.Node as unknown as typeof Node;
  globalThis.Text = mini.window.Text as unknown as typeof Text;
  Object.defineProperty(globalThis, 'ResizeObserver', {
    configurable: true,
    writable: true,
    value: class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  });
  (globalThis as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  return mini;
}

{
  const identity: HunkIdentity = {
    snapshotId: 's1',
    path: 'src/a.rs',
    oldStart: 1,
    oldCount: 5,
    newStart: 1,
    newCount: 5,
    contentHash: 'h-a',
  };
  const imageChip: ComposerAttachment = {
    id: 'att-1',
    name: 'shot.png',
    size: 12,
    mimeType: 'image/png',
    kind: 'image',
    previewUrl: 'blob:review-shot',
  };
  const previousWindow = (globalThis as { window?: unknown }).window;
  const previousDocument = (globalThis as { document?: unknown }).document;
  const mini = installMiniDom();
  const container = mini.document.createElement('div');
  mini.document.body.appendChild(container);
  const root: Root = createRoot(container as unknown as Element);
  const attached: FileList[] = [];
  const removed: string[] = [];
  const toggled: number[] = [];
  const liveProps: CodeReviewThreadDockProps = {
    selectedIdentity: identity,
    selectedThread: undefined,
    selectedDraft: '',
    composerKey: 'h-a',
    composerRef: { current: null as HTMLTextAreaElement | null },
    isNarrow: false,
    isHidden: false,
    busy: null,
    snapshotId: 's1',
    sessionId: null,
    acceptedFollow: null,
    reviewResults: null,
    selectedFinding: null,
    onSelectFinding: () => {},
    onBackToDiff: () => {},
    onRefresh: () => {},
    onComposerInput: () => {},
    onDraftCommit: () => {},
    onComposerKeyDown: () => {},
    onSubmitComment: () => {},
    onAbort: () => {},
    attachments: [imageChip],
    attachmentAccept: imageAttachmentAccept(),
    isRecording: false,
    isTranscribing: false,
    onAttachFiles: (files: FileList) => { attached.push(files); },
    onRemoveAttachment: (id: string) => { removed.push(id); },
    onToggleRecording: () => { toggled.push(1); },
  };
  try {
    await act(async () => {
      root.render(createElement(CodeReviewThreadDock, liveProps));
    });
    const picker = container.querySelector('[type="file"]');
    const attach = container.querySelector('.code-review__composer-btn--attach');
    const mic = container.querySelector('.code-review__composer-btn--mic');
    const remove = container.querySelector('.composer-attachment__remove');
    if (!picker || !attach || !mic || !remove) throw new Error('missing composer media controls');
    let pickerClicks = 0;
    const nativeClick = picker.click.bind(picker);
    picker.click = () => {
      pickerClicks += 1;
      nativeClick();
    };
    await act(async () => { attach.click(); });
    check('attach button opens the hidden image picker', pickerClicks === 1);

    const file = new File(['x'], 'shot.png', { type: 'image/png' });
    const files = {
      0: file,
      length: 1,
      item(index: number) { return index === 0 ? file : null; },
      *[Symbol.iterator]() { yield file; },
    } as unknown as FileList;
    picker.files = files;
    picker.value = 'shot.png';
    await act(async () => {
      picker.dispatchEvent({ type: 'change', target: picker });
    });
    check(
      'picker change delivers FileList then clears the input',
      attached.length === 1 && attached[0] === files && picker.value === '',
    );

    await act(async () => { remove.click(); });
    check('chip remove delivers the attachment id', removed.length === 1 && removed[0] === 'att-1');

    await act(async () => { mic.click(); });
    check('idle mic click delivers onToggleRecording', toggled.length === 1);

    const followThread = {
      identity,
      comments: Array.from({ length: 8 }, (_, index) => ({
        role: 'assistant' as const,
        text: `Review detail ${index + 1}`,
        thinking: '',
        model: 'mock',
        partial: false,
      })),
      streamingText: '',
      streamingThinking: '',
      error: null,
      stale: false,
      isStreaming: false,
      isCancellable: false,
      generation: 0,
      replyId: '',
      replyStatus: 'idle' as const,
      toolCalls: [],
      autoReview: null,
    };
    await act(async () => {
      root.render(createElement(CodeReviewThreadDock, {
        ...liveProps,
        selectedThread: followThread,
        isNarrow: true,
        isHidden: true,
      }));
    });
    const threadEl = container.querySelector('.code-review__thread') as { scrollTop: number; scrollHeight: number; clientHeight: number } | null;
    check('hidden dock still mounts the thread scroller', threadEl !== null);
    if (threadEl) {
      threadEl.scrollHeight = 400;
      threadEl.clientHeight = 80;
      threadEl.scrollTop = 0;
    }
    await act(async () => {
      root.render(createElement(CodeReviewThreadDock, {
        ...liveProps,
        selectedThread: followThread,
        isNarrow: true,
        isHidden: false,
      }));
    });
    check(
      'unhiding the thread dock force-pins to the tail',
      threadEl !== null && threadEl.scrollTop === 400,
      threadEl ? `scrollTop=${threadEl.scrollTop}` : 'missing',
    );
  } finally {
    await act(async () => { root.unmount(); });
    if (previousWindow === undefined) delete (globalThis as { window?: unknown }).window;
    else Object.defineProperty(globalThis, 'window', { configurable: true, writable: true, value: previousWindow });
    if (previousDocument === undefined) delete (globalThis as { document?: unknown }).document;
    else Object.defineProperty(globalThis, 'document', { configurable: true, writable: true, value: previousDocument });
  }
}

console.log(`codeReviewParity: ${ran} checks, ${failures.length} failures`);
for (const failure of failures) console.error(`  FAIL ${failure}`);
process.exit(failures.length > 0 ? 1 : 0);
