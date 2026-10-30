// Presentational file-diff pane for the code-review workspace.
//
// The pane renders the effective hunks — the merged per-file hunks once
// loaded (authoritative), the snapshot hunks before that — in hunk cards
// (existing class/selector contract) sliced to the visible window: a hunk
// that straddles the window boundary renders only its in-window lines and
// hunks entirely beyond the window are omitted until the window grows. A
// hunk is comment-SELECTABLE only when it is complete AND fully merged
// (`loadedHunkReady`); page-split or byte-capped hunks render but their
// header is disabled until the complete per-file hunk arrives. There is no
// flat "continued" block: every rendered line belongs to a structured hunk.
//
import { memo, useEffect, useMemo, useRef, type ReactNode, type UIEvent } from 'react';
import { safeText } from '../redact';
import {
  type DiffGap,
  type DiffFile,
  type DiffHunk,
  type DiffLine,
  type FileDiffStatusView,
  type GapExpandFrom,
  type GapExpandState,
  type HunkReviewResult,
  type ReviewFinding,
  type ReviewFindingRef,
  type LoadedDiff,
  type LoadedHunk,
  type ReviewThread,
  CONTEXT_EXPAND_LINES,
  FILE_RENDER_SOFT_CAP,
  loadedDiffAuthoritativeForSnapshot,
  countThreadComments,
  diffGapId,
  emptyComparisonMessage,
  fileDiffStatusView,
  hunkKeyFor,
  findingRefMatches,
  findingRowIndexes,
  findingRowsCoverRange,
  findingsCategory,
  findingsForIdentity,
  findThreadForHunk,
  hunkIdentityFor,
  hunkIsComplete,
  isDiffPlaceholder,
  isNearDiffBottom,
  loadedHunkReady,
  remainingDiffGap,
  reviewCategoryTone,
  reviewDiffGaps,
  threadIsStreaming,
} from '../codeReview';
import { diffPathLanguage, highlightDiffLineFragments } from '../markdown';
import { MarkdownBody } from '../MarkdownBody';
import { fileExplorerPreviewKind, type FileExplorerPreviewKind } from '../fileExplorer';

function linePrefix(kind: string): string {
  switch (kind) {
    case 'addition':
      return '+';
    case 'deletion':
      return '-';
    case 'meta':
      return '\\';
    default:
      return ' ';
  }
}

/** Diff line bodies that are source text (not meta markers). */
function highlightableTexts(lines: Array<{ kind: string; text: string }>): Array<string | null> {
  return lines.map((line) =>
    line.kind === 'context' || line.kind === 'addition' || line.kind === 'deletion'
      ? line.text
      : null,
  );
}

/** Optional per-row finding mark: the row's highest covering category tone
 *  plus selected-finding emphasis. */
interface LineMark {
  tone: 'danger' | 'warn' | 'info' | null;
  selected: boolean;
}
function renderLine(
  key: string,
  line: { kind: string; oldNo?: number; newNo?: number; text: string },
  fragment: string | null,
  onClick?: () => void,
  mark?: LineMark | null,
  dataIndex?: number,
): ReactNode {
  return (
    <div
      key={key}
      className={`code-review__line code-review__line--${line.kind}${
        mark?.tone ? ` has-finding--${mark.tone}` : ''
      }${mark?.selected ? ' is-finding-selected' : ''}`}
      onClick={onClick}
      data-line-index={dataIndex}
    >
      <span className="code-review__line-no code-review__line-no--old">
        {line.oldNo != null ? line.oldNo : ''}
      </span>
      <span className="code-review__line-no code-review__line-no--new">
        {line.newNo != null ? line.newNo : ''}
      </span>
      <span className="code-review__line-prefix">{linePrefix(line.kind)}</span>
      {fragment !== null ? (
        <span
          className="code-review__line-text"
          dangerouslySetInnerHTML={{ __html: fragment }}
        />
      ) : (
        // React text node: auto-escaped; safeText keeps the redaction
        // contract identical to highlighted lines (credentials -> [REDACTED]).
        <span className="code-review__line-text">{safeText(line.text)}</span>
      )}
    </div>
  );
}

export interface ReviewPreviewInfo {
  kind: FileExplorerPreviewKind;
  loading: boolean;
  error: string | null;
  text: string | null;
  truncated: boolean;
}

/** Files whose NEW version renders in the review Preview surface: the file
 *  explorer's own render decision (md/markdown/html/htm/svg); binary and
 *  unknown extensions fail closed to Source. */
export function reviewPreviewKind(
  file: Pick<DiffFile, 'path' | 'binary'>,
): FileExplorerPreviewKind | null {
  if (file.binary) return null;
  return fileExplorerPreviewKind(file.path);
}

export interface CodeReviewDiffPaneProps {
  selectedFile: DiffFile | null;
  loadedDiff: LoadedDiff | null;
  windowLimit: number;
  filesCount: number;
  snapshotId: string;
  threads: ReviewThread[];
  selectedHunkKey: string | null;
  collapsedHunks: Set<string>;
  isHidden: boolean;
  isLoadingMore: boolean;
  loadError: string | null;
  onNearBottom: () => void;
  onRetry: () => void;
  untrackedNotShown?: number;
  onSelectHunk: (key: string) => void;
  onNavigateHunk: (key: string, requiredWindow: number) => void;
  onToggleHunkCollapsed: (key: string) => void;
  gapStates: Record<string, GapExpandState>;
  gapLines: Record<string, DiffLine[]>;
  oldLineCount: number;
  newLineCount: number;
  expandingGapId: string | null;
  onExpandGap: (gap: DiffGap, from: GapExpandFrom, take: number) => void;
  /** Source|Preview surface for the selected file; Preview needs `preview`. */
  surface: 'source' | 'preview';
  onSurfaceChange: (surface: 'source' | 'preview') => void;
  /** Renderable preview state for the selected file; null = not renderable. */
  preview: ReviewPreviewInfo | null;
  /** Snapshot's validated per-hunk review findings; null = unreviewed.
   *  Rows inside each finding's inclusive side range carry the actual
   *  mark — there is no whole-hunk tint. */
  reviewResults: readonly HunkReviewResult[] | null;
  /** Global finding selection; the matching hunk's rows gain emphasis. */
  selectedFinding: ReviewFindingRef | null;
  /** Step to the previous issue. The parent owns the global order and the
   *  no-wrap endpoints (selection holds at the first/last issue). */
  onPreviousIssue: () => void;
  /** Step to the next issue; same parent-owned order/endpoints contract. */
  onNextIssue: () => void;
  /** Index of the selected finding in the global ordered list, or -1. */
  issueIndex: number;
  /** Length of the global ordered finding list. */
  issueCount: number;

}

/** True when a hunk may be selected/commented right now. */
function hunkSelectable(hunk: DiffHunk | LoadedHunk): boolean {
  if ('totalLines' in hunk && 'complete' in hunk) {
    return loadedHunkReady(hunk as LoadedHunk);
  }
  return hunkIsComplete(hunk);
}

/** Selected-file header + per-hunk diff lines with explicit selection. */
export const CodeReviewDiffPane = memo(function CodeReviewDiffPane({
  selectedFile,
  loadedDiff,
  windowLimit,
  filesCount,
  snapshotId,
  threads,
  selectedHunkKey,
  collapsedHunks,
  isHidden,
  isLoadingMore,
  loadError,
  onNearBottom,
  onRetry,
  untrackedNotShown = 0,
  onSelectHunk,
  onNavigateHunk,
  onToggleHunkCollapsed,
  gapStates,
  gapLines,
  oldLineCount,
  newLineCount,
  expandingGapId,
  onExpandGap,
  surface,
  onSurfaceChange,
  preview,
  reviewResults,
  selectedFinding,
  onPreviousIssue,
  onNextIssue,
  issueIndex,
  issueCount,
}: CodeReviewDiffPaneProps) {
  // Effective hunk source: the merged per-file hunks once loaded (they
  // replace the snapshot's structure — catalog stats/identity stay with the
  // snapshot file entry), the snapshot hunks before that.
  const effectiveHunks = useMemo(() => {
    if (loadedDiff && loadedDiff.hunks.length > 0) return loadedDiff.hunks;
    return selectedFile ? selectedFile.hunks : [];
  }, [loadedDiff, selectedFile]);

  // Hunk line ranges over the effective hunks; the visible window slices
  // each hunk to its in-window lines.
  const hunksWithRanges = useMemo(() => {
    let offset = 0;
    return effectiveHunks.map((hunk) => {
      const range = { start: offset, end: offset + hunk.lines.length };
      offset = range.end;
      return { hunk, range };
    });
  }, [effectiveHunks]);
  const navigationHunks = useMemo(() => {
    if (!selectedFile) return [];
    return hunksWithRanges
      .filter(({ hunk, range }) => hunkSelectable(hunk) && range.start < FILE_RENDER_SOFT_CAP)
      .map((entry) => ({ ...entry, key: hunkKeyFor(selectedFile, entry.hunk) }));
  }, [hunksWithRanges, selectedFile]);
  const navigationIndex = navigationHunks.findIndex(({ key }) => key === selectedHunkKey);
  const hunkNodesRef = useRef(new Map<string, HTMLDivElement>());
  const diffPaneRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const pane = diffPaneRef.current;
    if (!selectedFile || !pane) return;
    pane.scrollTop = 0;
    pane.scrollLeft = 0;
  }, [selectedFile?.path]);
  useEffect(() => {
    if (isHidden || !selectedHunkKey) return;
    hunkNodesRef.current
      .get(selectedHunkKey)
      ?.scrollIntoView?.({ block: 'center', inline: 'nearest' });
  }, [isHidden, selectedHunkKey, windowLimit]);
  // Issue navigation must reveal the selected finding's first marked row,
  // not the hunk card: same-hunk next/previous keeps selectedHunkKey and
  // only changes selectedFinding. The row is absent until the parent's
  // file-global window growth commits; a collapsed hunk stays hidden until
  // the parent expands it. Either change reruns this effect.
  useEffect(() => {
    if (isHidden || !selectedFinding) return;
    const pane = diffPaneRef.current;
    if (!pane) return;
    const row = pane.querySelector<HTMLElement>('.code-review__line.is-finding-selected');
    if (!row || row.closest('.code-review__hunk-lines')?.classList.contains('is-collapsed')) return;
    row.scrollIntoView?.({ block: 'center', inline: 'nearest' });
  }, [isHidden, selectedFinding, windowLimit, collapsedHunks]);
  const navigateHunk = (step: -1 | 1) => {
    const nextIndex = navigationIndex < 0
      ? (step === 1 ? 0 : -1)
      : navigationIndex + step;
    const target = navigationHunks[nextIndex];
    if (!target) return;
    onNavigateHunk(
      target.key,
      Math.min(FILE_RENDER_SOFT_CAP, Math.max(target.range.end, 1)),
    );
  };

  // Registered highlight.js language inferred from the selected path; null
  // means plain rendering (unknown/plain-text/binary files).
  const diffLanguage = useMemo(
    () => (selectedFile && !selectedFile.binary ? diffPathLanguage(selectedFile.path) : null),
    [selectedFile],
  );

  // One balanced HTML fragment per visible line of each (uncollapsed) hunk,
  // batched as a single hljs call per hunk. Aligned 1:1 with the sliced lines.
  const hunkFragments: Record<string, Array<string | null>> = useMemo(() => {
    const out: Record<string, Array<string | null>> = {};
    const file = selectedFile;
    if (!file || !diffLanguage) return out;
    for (const { hunk, range } of hunksWithRanges) {
      const key = hunkKeyFor(file, hunk);
      if (collapsedHunks.has(key)) continue;
      const visibleCount = Math.max(0, Math.min(range.end, windowLimit) - range.start);
      if (visibleCount <= 0) continue;
      out[key] = highlightDiffLineFragments(
        diffLanguage,
        highlightableTexts(hunk.lines.slice(0, visibleCount)),
      );
    }
    return out;
  }, [hunksWithRanges, windowLimit, collapsedHunks, diffLanguage, selectedFile]);

  // Gap bands come from the SNAPSHOT's hunk list (the complete authority).
  const gapSourceHunks = useMemo(() => {
    if (!selectedFile || selectedFile.binary) return [];
    // An authoritative per-file load owns the hunk set it serves: gaps must
    // come from THOSE hunks, not the possibly-preview snapshot entry.
    if (loadedDiff && loadedDiffAuthoritativeForSnapshot(loadedDiff, snapshotId)) {
      return loadedDiff.hunks;
    }
    const snapshotComplete =
      selectedFile.bodyState === 'complete' && !selectedFile.truncated;
    return snapshotComplete ? selectedFile.hunks : [];
  }, [selectedFile, loadedDiff, snapshotId]);

  const bands = useMemo(() => {
    // An unauthorized gap source yields NO gaps: reviewDiffGaps([], known
    // counts) would otherwise invent a whole-file band over unloaded hunks.
    const gaps =
      gapSourceHunks.length === 0
        ? []
        : reviewDiffGaps(gapSourceHunks, oldLineCount, newLineCount);
    type Band =
      | { kind: 'gap'; gap: (typeof gaps)[number] }
      | { kind: 'hunk'; hunk: (typeof hunksWithRanges)[number]['hunk']; range: { start: number; end: number } };
    const out: Band[] = [];
    let gapIndex = 0;
    const gapBeforeHunk = (
      gap: (typeof gaps)[number],
      hunk: { oldStart: number; newStart: number },
    ): boolean => {
      if (gap.newCount > 0 && hunk.newStart > 0) {
        return gap.newStart + gap.newCount <= hunk.newStart;
      }
      if (gap.oldCount > 0 && hunk.oldStart > 0) {
        return gap.oldStart + gap.oldCount <= hunk.oldStart;
      }
      return false;
    };
    for (const row of hunksWithRanges) {
      while (gapIndex < gaps.length && gapBeforeHunk(gaps[gapIndex], row.hunk)) {
        out.push({ kind: 'gap', gap: gaps[gapIndex] });
        gapIndex += 1;
      }
      out.push({ kind: 'hunk', hunk: row.hunk, range: row.range });
    }
    while (gapIndex < gaps.length) {
      out.push({ kind: 'gap', gap: gaps[gapIndex] });
      gapIndex += 1;
    }
    return out;
  }, [gapSourceHunks, hunksWithRanges, oldLineCount, newLineCount]);

  const gapFragments: Record<string, Array<string | null>> = useMemo(() => {
    const out: Record<string, Array<string | null>> = {};
    if (!diffLanguage) return out;
    for (const band of bands) {
      if (band.kind !== 'gap') continue;
      const id = diffGapId(band.gap);
      const lines = gapLines[id];
      if (!lines || lines.length === 0) continue;
      out[id] = highlightDiffLineFragments(diffLanguage, highlightableTexts(lines));
    }
    return out;
  }, [bands, diffLanguage, gapLines]);

  const isPlaceholder = selectedFile ? isDiffPlaceholder(selectedFile) : false;
  const hunksEmpty = effectiveHunks.length === 0;
  // Diff data is still being fetched when the backend hunk set is not yet
  // complete and no error/byte cap stopped it.
  const loadingDiff =
    !!loadedDiff &&
    !loadedDiff.hunksComplete &&
    !loadedDiff.byteCapped &&
    !loadError;
  const status: FileDiffStatusView = loadedDiff
    ? fileDiffStatusView(loadedDiff, windowLimit)
    : { kind: 'none', text: null };
  const handleScroll = (event: UIEvent<HTMLDivElement>) => {
    if (isNearDiffBottom(event.currentTarget)) onNearBottom();
  };

  // One per-file render budget shared by hunk rows and every expanded gap
  // band, mirroring the backend MAX_FILE_RENDER_LINES. Gap lines render up
  // to this aggregate budget, in band order; overflow stays behind the gap
  // bar instead of growing the pane without bound.
  const gapRenderBudget = useMemo(() => {
    let hunkVisible = 0;
    for (const { range } of hunksWithRanges) {
      hunkVisible += Math.max(0, Math.min(range.end, windowLimit) - range.start);
    }
    return Math.max(0, FILE_RENDER_SOFT_CAP - hunkVisible);
  }, [hunksWithRanges, windowLimit]);
  const gapVisibleCounts = useMemo(() => {
    const out: Record<string, number> = {};
    let budget = gapRenderBudget;
    for (const band of bands) {
      if (band.kind !== 'gap') continue;
      const id = diffGapId(band.gap);
      const lines = gapLines[id]?.length ?? 0;
      const visible = Math.min(lines, budget);
      out[id] = visible;
      budget -= visible;
    }
    return out;
  }, [bands, gapLines, gapRenderBudget]);

  // Nothing selected (index < 0) can step either way: next is the first
  // issue and previous is the last. A real selection holds at the ends.
  const canPreviousIssue = issueCount > 0 && (issueIndex < 0 || issueIndex > 0);
  const canNextIssue = issueCount > 0 && (issueIndex < 0 || issueIndex < issueCount - 1);

  // Per-row finding marks over the visible hunk lines, keyed hunk key →
  // visible row index. Every row inside a finding's inclusive side range
  // wears the highest covering category's tone; rows of the globally
  // selected finding add emphasis. Rows beyond the visible window are
  // skipped — they do not exist in the DOM yet.
  const findingMarks = useMemo(() => {
    const out: Record<string, Array<LineMark | null>> = {};
    if (!selectedFile || !reviewResults) return out;
    for (const { hunk, range } of hunksWithRanges) {
      const identity = hunkIdentityFor(snapshotId, selectedFile, hunk);
      const findings = findingsForIdentity(reviewResults, identity);
      if (findings.length === 0) continue;
      const visibleCount = Math.max(0, Math.min(range.end, windowLimit) - range.start);
      if (visibleCount <= 0) continue;
      const rows: Array<{ covered: ReviewFinding[]; selected: boolean } | null> =
        new Array(visibleCount).fill(null);
      findings.forEach((finding, findingIndex) => {
        const rowsForFinding = findingRowIndexes(hunk.lines, finding);
        if (!findingRowsCoverRange(hunk.lines, finding, rowsForFinding)) return;
        const selected = findingRefMatches(selectedFinding, {
          resultIndex: -1,
          findingIndex,
          identity,
          finding,
          firstRow: -1,
          lastRow: -1,
          fileRow: -1,
        });
        for (const row of rowsForFinding) {
          if (row >= visibleCount) break;
          const mark = rows[row] ?? (rows[row] = { covered: [], selected: false });
          mark.covered.push(finding);
          if (selected) mark.selected = true;
        }
      });
      out[hunkKeyFor(selectedFile, hunk)] = rows.map((mark) =>
        mark ? { tone: reviewCategoryTone(findingsCategory(mark.covered)), selected: mark.selected } : null,
      );
    }
    return out;
  }, [hunksWithRanges, selectedFile, snapshotId, reviewResults, selectedFinding, windowLimit]);

  const openGaps = bands.flatMap((band) => {
    if (band.kind !== 'gap') return [];
    const id = diffGapId(band.gap);
    const state = gapStates[id] ?? { gap: band.gap, fromStart: 0, fromEnd: 0 };
    const remaining = remainingDiffGap(state);
    if (remaining.oldCount === 0 && remaining.newCount === 0) return [];
    return [{ gap: band.gap, remaining }];
  });
  const canExpandFile =
    !selectedFile?.binary && !hunksEmpty && !isPlaceholder && openGaps.length > 0;
  const expandRemainingFile = () => {
    for (const { remaining, gap } of openGaps) {
      const take = Math.max(remaining.newCount, remaining.oldCount);
      onExpandGap(gap, 'end', take);
    }
  };
  return (
    <div
      className={`code-review__diff${isHidden ? ' is-hidden' : ''}`}
      ref={diffPaneRef}
      aria-label="File diff"
      onScroll={handleScroll}
    >
      {!selectedFile && (
        <div className="code-review__empty">
          {filesCount > 0
            ? 'Select a file to view its diff'
            : emptyComparisonMessage(untrackedNotShown)}
        </div>
      )}
      {selectedFile && (
        <>
          <div className="code-review__diff-head">
            <span className="code-review__diff-path">
              {selectedFile.previousPath
                ? `${safeText(selectedFile.previousPath)} → ${safeText(selectedFile.path)}`
                : safeText(selectedFile.path)}
            </span>
            <span className="code-review__diff-meta">
              {selectedFile.status || 'changed'}
            </span>
            <div className="code-review__hunk-nav" role="group" aria-label="Hunk navigation">
              <button
                type="button"
                className="code-review__hunk-nav-btn"
                aria-label="Previous hunk"
                title="Previous hunk"
                disabled={navigationIndex <= 0}
                onClick={() => navigateHunk(-1)}
              >
                ↑
              </button>
              <span className="code-review__hunk-nav-position" aria-live="polite">
                {Math.max(0, navigationIndex + 1)} / {navigationHunks.length}
              </span>
              <button
                type="button"
                className="code-review__hunk-nav-btn"
                aria-label="Next hunk"
                title="Next hunk"
                disabled={
                  navigationHunks.length === 0
                  || navigationIndex === navigationHunks.length - 1
                }
                onClick={() => navigateHunk(1)}
              >
                ↓
              </button>
            </div>
            <div className="code-review__hunk-nav" role="group" aria-label="Issue navigation">
              <button
                type="button"
                className="code-review__hunk-nav-btn"
                aria-label="Previous issue"
                title="Previous issue"
                disabled={!canPreviousIssue}
                onClick={onPreviousIssue}
              >
                ↑
              </button>
              <span className="code-review__hunk-nav-position" aria-live="polite">
                {issueIndex < 0 ? 0 : issueIndex + 1} / {issueCount}
              </span>
              <button
                type="button"
                className="code-review__hunk-nav-btn"
                aria-label="Next issue"
                title="Next issue"
                disabled={!canNextIssue}
                onClick={onNextIssue}
              >
                ↓
              </button>
            </div>
            {canExpandFile && (
              <button
                type="button"
                className="code-review__gap-btn"
                onClick={expandRemainingFile}
                disabled={expandingGapId !== null}
              >
                Expand file
              </button>
            )}
            {preview && (
              <div className="code-review__surface" role="radiogroup" aria-label="File surface">
                <button
                  type="button"
                  className={`code-review__surface-btn${surface === 'source' ? ' is-active' : ''}`}
                  aria-pressed={surface === 'source'}
                  onClick={() => onSurfaceChange('source')}
                >
                  Source
                </button>
                <button
                  type="button"
                  className={`code-review__surface-btn${surface === 'preview' ? ' is-active' : ''}`}
                  aria-pressed={surface === 'preview'}
                  disabled={preview.error != null}
                  title={
                    preview.error != null
                      ? safeText(preview.error)
                      : 'Rendered NEW file content'
                  }
                  onClick={() => onSurfaceChange('preview')}
                >
                  Preview
                </button>
              </div>
            )}
          </div>
          {preview && surface === 'preview' && (
            preview.error != null ? (
              <div className="code-review__preview-error" role="alert">
                {safeText(preview.error)}
              </div>
            ) : preview.loading ? (
              <div className="code-review__preview-loading" role="status" aria-label="Loading preview">
                <span className="code-review__spinner" aria-hidden="true" />
              </div>
            ) : preview.text != null ? (
              <div className="code-review__preview" data-preview-kind={preview.kind}>
                {preview.truncated && (
                  <div className="code-review__preview-note">Preview truncated at the read limit.</div>
                )}
                {preview.kind === 'markdown' && (
                  <div className="code-review__preview-markdown">
                    <MarkdownBody className="code-review__preview-markdown-body" text={preview.text} />
                  </div>
                )}
                {preview.kind === 'html' && (
                  <div className="code-review__preview-html">
                    <iframe
                      className="code-review__preview-frame"
                      title="HTML preview"
                      sandbox=""
                      referrerPolicy="no-referrer"
                      srcDoc={preview.text}
                    />
                  </div>
                )}
                {preview.kind === 'svg' && (
                  <div className="code-review__preview-svg">
                    <img
                      className="code-review__preview-svg-img"
                      alt=""
                      src={`data:image/svg+xml;charset=utf-8,${encodeURIComponent(preview.text)}`}
                    />
                  </div>
                )}
              </div>
            ) : null
          )}
          {!(preview && surface === 'preview') && selectedFile.binary && (
            <div className="code-review__empty">Binary file — no text diff</div>
          )}
          {!selectedFile.binary && hunksEmpty && loadingDiff && (
            <div className="code-review__empty code-review__empty--loading" role="status" aria-label="Loading diff">
              <span className="code-review__spinner" aria-hidden="true" />
            </div>
          )}
          {!selectedFile.binary &&
            hunksEmpty &&
            !loadingDiff &&
            isPlaceholder && (
              <div className="code-review__empty">
                This file's diff was not included in the snapshot — it loads on demand.
              </div>
            )}
          {!selectedFile.binary &&
            hunksEmpty &&
            !loadingDiff &&
            !isPlaceholder && <div className="code-review__empty">No hunks in this file</div>}
          {!(preview && surface === 'preview' && preview.text != null) && (
          <div className="code-review__hunks">
            {bands.map((band) => {
              if (band.kind === 'gap') {
                const id = diffGapId(band.gap);
                const state = gapStates[id] ?? { gap: band.gap, fromStart: 0, fromEnd: 0 };
                const remaining = remainingDiffGap(state);
                const lines = gapLines[id] ?? [];
                const fragments = gapFragments[id] ?? null;
                const remainingCount = Math.max(remaining.newCount, remaining.oldCount);
                const busy = expandingGapId === id;
                return (
                  <div key={id} className="code-review__gap">
                    {lines.length > 0 && (
                      <pre className="code-review__hunk-lines">
                        {lines.slice(0, gapVisibleCounts[id] ?? 0).map((line, idx) =>
                          renderLine(
                            `${id}:${idx}`,
                            line,
                            fragments ? (fragments[idx] ?? null) : null,
                          ),
                        )}
                      </pre>
                    )}
                    {remainingCount > 0 && (
                      <div className="code-review__gap-bar">
                        <button
                          type="button"
                          className="code-review__gap-btn"
                          aria-label="Expand toward hunk"
                          disabled={busy}
                          onClick={() =>
                            onExpandGap(
                              band.gap,
                              'end',
                              Math.min(CONTEXT_EXPAND_LINES, remainingCount),
                            )
                          }
                        >
                          ▼
                        </button>
                        <button
                          type="button"
                          className="code-review__gap-btn code-review__gap-btn--grow"
                          aria-label="Expand remaining lines in this gap"
                          disabled={busy}
                          onClick={() =>
                            onExpandGap(
                              band.gap,
                              'end',
                              remainingCount,
                            )
                          }
                        >
                          {busy ? 'Expanding…' : `${remainingCount} lines`}
                        </button>
                        <button
                          type="button"
                          className="code-review__gap-btn"
                          aria-label="Expand toward file top"
                          disabled={busy}
                          onClick={() =>
                            onExpandGap(
                              band.gap,
                              'start',
                              Math.min(CONTEXT_EXPAND_LINES, remainingCount),
                            )
                          }
                        >
                          ▲
                        </button>
                      </div>
                    )}
                  </div>
                );
              }
              const { hunk, range } = band;
              if (!selectedFile) return null;
              const key = hunkKeyFor(selectedFile, hunk);
              const identity = hunkIdentityFor(snapshotId, selectedFile, hunk);
              const thread = findThreadForHunk(threads, identity);
              const threadCount = countThreadComments(thread);
              const isSelected = key === selectedHunkKey;
              const isStreaming = threadIsStreaming(thread);
              const collapsed = collapsedHunks.has(key);
              const visibleCount = Math.max(
                0,
                Math.min(range.end, windowLimit) - range.start,
              );
              if (visibleCount <= 0) return null;
              const selectable = hunkSelectable(hunk);
              const visibleLines = hunk.lines.slice(0, visibleCount);
              const fragments = hunkFragments[key] ?? null;
              return (
                <div
                  ref={(node) => {
                    if (node) hunkNodesRef.current.set(key, node);
                    else hunkNodesRef.current.delete(key);
                  }}
                  key={key}
                  className={`code-review__hunk${isSelected ? ' is-selected' : ''}`}
                >
                  <div className="code-review__hunk-header-row">
                    <button
                      type="button"
                      className="code-review__hunk-header"
                      onClick={() => onSelectHunk(key)}
                      disabled={!selectable}
                      aria-pressed={isSelected}
                      title={
                        selectable
                          ? 'Select this hunk to comment'
                          : 'This hunk is not fully loaded yet — it cannot be commented'
                      }
                    >
                      {safeText(
                        hunk.header ||
                          `@@ -${hunk.oldStart},${hunk.oldCount} +${hunk.newStart},${hunk.newCount} @@`,
                      )}
                    </button>
                    {threadCount > 0 && (
                      <span
                        className="code-review__hunk-badge"
                        title={`${threadCount} comment${threadCount === 1 ? '' : 's'}`}
                      >
                        {threadCount}
                      </span>
                    )}
                    {isStreaming && (
                      <span
                        className="code-review__streaming-dot"
                        title={thread?.streamingText ? 'Reply streaming' : 'Thinking…'}
                        aria-label={thread?.streamingText ? 'Reply streaming' : 'Thinking…'}
                      />
                    )}
                    {isSelected && (
                      <span className="code-review__hunk-selected-hint">comment</span>
                    )}
                    <button
                      type="button"
                      className="code-review__hunk-toggle"
                      onClick={() => onToggleHunkCollapsed(key)}
                      aria-expanded={!collapsed}
                      aria-label={collapsed ? 'Expand hunk' : 'Collapse hunk'}
                    >
                      {collapsed ? '▸' : '▾'}
                    </button>
                  </div>
                  <pre
                    className={`code-review__hunk-lines${collapsed ? ' is-collapsed' : ''}`}
                  >
                    {visibleLines.map((line, idx) =>
                      renderLine(
                        `${key}:${idx}`,
                        line,
                        fragments ? (fragments[idx] ?? null) : null,
                        selectable ? () => onSelectHunk(key) : undefined,
                        findingMarks[key]?.[idx] ?? undefined,
                        idx,
                      ),
                    )}
                  </pre>
                </div>
              );
            })}
          </div>
          )}
          {(status.kind !== 'none' || loadError) && (
            <div className="code-review__window" role="status">
              {status.text && (
                <div className="code-review__window-meta code-review__page-status">
                  {status.text}
                </div>
              )}
              {loadError && (
                <div className="code-review__window-error" role="alert">
                  {safeText(loadError)}{' '}
                  <button
                    type="button"
                    className="code-review__link"
                    disabled={isLoadingMore}
                    onClick={onRetry}
                  >
                    Retry
                  </button>
                </div>
              )}
            </div>
          )}
        </>
      )}
    </div>
  );
});
