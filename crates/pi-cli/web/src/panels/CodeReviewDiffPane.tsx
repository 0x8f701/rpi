// Presentational file-diff pane for the code-review workspace.
// Parent owns selection, collapse set, snapshot-derived identity, and the
// per-file loaded diff + visible-line window (Load more / Load full).
//
// The pane renders the loaded line stream in hunk cards (existing class/
// selector contract) sliced to the visible window: a hunk that straddles the
// window boundary renders only its in-window lines, hunks entirely beyond
// the window are omitted until the window grows, and backend-loaded lines
// beyond the snapshot's hunks render as a flat "continued" block. Load
// more / Load full controls sit below the hunks while content remains.
//
// Line bodies are syntax-highlighted in bounded batches (one hljs call per
// hunk / continued run) when the selected path maps to a registered
// highlight.js language; fragments are balanced per line, textContent stays
// identical to the plain renderer, and hostile input stays literal (hljs
// escapes, exactly like renderFence). Meta lines and unknown/binary files
// render as plain text.

import { useMemo, type ReactNode } from 'react';
import { safeText } from '../redact';
import {
  FILE_DIFF_MAX_UI_LINES,
  type DiffFile,
  type HunkIdentity,
  type LoadedDiff,
  type ReviewThread,
  countThreadComments,
  findThreadForHunk,
  hunkIdentityFor,
  hunkKey,
  hunkKeyFor,
  isDiffPlaceholder,
} from '../codeReview';
import { diffPathLanguage, highlightDiffLineFragments } from '../markdown';

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

function renderLine(
  key: string,
  line: { kind: string; oldNo?: number; newNo?: number; text: string },
  fragment: string | null,
  onClick?: () => void,
): ReactNode {
  return (
    <div
      key={key}
      className={`code-review__line code-review__line--${line.kind}`}
      onClick={onClick}
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

export interface CodeReviewDiffPaneProps {
  selectedFile: DiffFile | null;
  loadedDiff: LoadedDiff | null;
  windowLimit: number;
  filesCount: number;
  snapshotId: string;
  threads: ReviewThread[];
  activeHunk: HunkIdentity | null;
  selectedHunkKey: string | null;
  collapsedHunks: Set<string>;
  isHidden: boolean;
  canLoadMore: boolean;
  hardCapped: boolean;
  moreCount: number;
  isLoadingMore: boolean;
  loadError: string | null;
  onSelectHunk: (key: string) => void;
  onToggleHunkCollapsed: (key: string) => void;
  onLoadMore: () => void;
  onLoadFull: () => void;
}

/** Selected-file header + per-hunk diff lines with explicit selection. */
export function CodeReviewDiffPane({
  selectedFile,
  loadedDiff,
  windowLimit,
  filesCount,
  snapshotId,
  threads,
  activeHunk,
  selectedHunkKey,
  collapsedHunks,
  isHidden,
  canLoadMore,
  hardCapped,
  moreCount,
  isLoadingMore,
  loadError,
  onSelectHunk,
  onToggleHunkCollapsed,
  onLoadMore,
  onLoadFull,
}: CodeReviewDiffPaneProps) {
  // Hunk line ranges over the snapshot portion of the loaded line stream;
  // the visible window slices each hunk to its in-window lines.
  const hunksWithRanges = useMemo(() => {
    if (!selectedFile) return [];
    let offset = 0;
    return selectedFile.hunks.map((hunk) => {
      const range = { start: offset, end: offset + hunk.lines.length };
      offset = range.end;
      return { hunk, range };
    });
  }, [selectedFile]);

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

  // Backend-loaded lines beyond the snapshot's hunks, within the window.
  const extraVisible = useMemo(() => {
    if (!loadedDiff) return 0;
    return Math.max(
      0,
      Math.min(loadedDiff.lines.length, windowLimit) - loadedDiff.snapshotLineCount,
    );
  }, [loadedDiff, windowLimit]);

  // One batch highlight for the visible continued lines (aligned 1:1).
  const extraFragments = useMemo(() => {
    if (!loadedDiff || extraVisible <= 0 || !diffLanguage) return null;
    const slice = loadedDiff.lines.slice(
      loadedDiff.snapshotLineCount,
      loadedDiff.snapshotLineCount + extraVisible,
    );
    return highlightDiffLineFragments(diffLanguage, highlightableTexts(slice));
  }, [loadedDiff, extraVisible, diffLanguage]);

  const isPlaceholder = selectedFile ? isDiffPlaceholder(selectedFile) : false;
  const placeholderEmpty = isPlaceholder && (loadedDiff?.lines.length ?? 0) === 0;
  const byteCapped = loadedDiff?.byteCapped ?? false;
  const loadedLineCount = loadedDiff?.lines.length ?? 0;

  return (
    <div
      className={`code-review__diff${isHidden ? ' is-hidden' : ''}`}
      aria-label="File diff"
    >
      {!selectedFile && (
        <div className="code-review__empty">
          {filesCount > 0
            ? 'Select a file to view its diff'
            : 'No changed files in this comparison.'}
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
              {selectedFile.truncated ? ' · truncated' : ''}
              {selectedFile.message ? ` · ${safeText(selectedFile.message)}` : ''}
            </span>
          </div>
          {selectedFile.truncated && !byteCapped && (
            <div className="code-review__banner code-review__banner--truncated" role="status">
              Large file — the diff loads in bounded pages.
            </div>
          )}
          {byteCapped && (
            <div className="code-review__banner code-review__banner--truncated" role="status">
              File body exceeds the backend size limit — later lines are not available.
            </div>
          )}
          {selectedFile.binary && (
            <div className="code-review__empty">Binary file — no text diff</div>
          )}
          {!selectedFile.binary && selectedFile.hunks.length === 0 && placeholderEmpty && (
            <div className="code-review__empty code-review__empty--loading">
              {isLoadingMore
                ? 'Loading diff…'
                : "This file's diff was not included in the snapshot — it loads on demand."}
            </div>
          )}
          {!selectedFile.binary &&
            selectedFile.hunks.length === 0 &&
            !placeholderEmpty &&
            loadedLineCount === 0 && <div className="code-review__empty">No hunks in this file</div>}
          <div className="code-review__hunks">
            {hunksWithRanges.map(({ hunk, range }) => {
              const key = hunkKeyFor(selectedFile, hunk);
              const identity = hunkIdentityFor(snapshotId, selectedFile, hunk);
              const thread = findThreadForHunk(threads, identity);
              const threadCount = countThreadComments(thread);
              const isSelected = key === selectedHunkKey;
              const isStreaming = Boolean(
                thread?.streamingText || (activeHunk && hunkKey(activeHunk) === key),
              );
              const collapsed = collapsedHunks.has(key);
              const visibleCount = Math.max(
                0,
                Math.min(range.end, windowLimit) - range.start,
              );
              if (visibleCount <= 0) return null;
              const visibleLines = hunk.lines.slice(0, visibleCount);
              const fragments = hunkFragments[key] ?? null;
              return (
                <div
                  key={key}
                  className={`code-review__hunk${isSelected ? ' is-selected' : ''}`}
                >
                  <div className="code-review__hunk-header-row">
                    <button
                      type="button"
                      className="code-review__hunk-header"
                      onClick={() => onSelectHunk(key)}
                      aria-pressed={isSelected}
                      title="Select this hunk to comment"
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
                        title="Reply streaming"
                        aria-label="Reply streaming"
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
                        () => onSelectHunk(key),
                      ),
                    )}
                  </pre>
                </div>
              );
            })}
            {extraVisible > 0 && loadedDiff && (
              <div className="code-review__hunk code-review__hunk--continued">
                <div className="code-review__hunk-header-row">
                  <span className="code-review__hunk-header code-review__hunk-header--muted">
                    {loadedDiff.snapshotLineCount === 0
                      ? 'loaded diff lines'
                      : '… continued lines from the loaded page'}
                  </span>
                </div>
                <pre className="code-review__hunk-lines">
                  {loadedDiff.lines
                    .slice(
                      loadedDiff.snapshotLineCount,
                      loadedDiff.snapshotLineCount + extraVisible,
                    )
                    .map((line, idx) =>
                      renderLine(
                        `continued:${idx}`,
                        line,
                        extraFragments ? (extraFragments[idx] ?? null) : null,
                      ),
                    )}
                </pre>
              </div>
            )}
          </div>
          {(canLoadMore || hardCapped || byteCapped) && (
            <div className="code-review__window" role="status">
              <div className="code-review__window-meta code-review__page-status">
                {hardCapped
                  ? `Only the first ${FILE_DIFF_MAX_UI_LINES.toLocaleString()} lines are shown (UI hard cap).`
                  : byteCapped && !canLoadMore
                    ? 'The file body exceeds the backend size limit — later lines are not available.'
                    : moreCount > 0
                      ? `${moreCount.toLocaleString()} more line${moreCount === 1 ? '' : 's'} below.`
                      : 'More lines load on demand.'}
              </div>
              <div className="code-review__window-actions">
                {canLoadMore && (
                  <button
                    type="button"
                    className="code-review__load-more"
                    disabled={isLoadingMore}
                    onClick={onLoadMore}
                  >
                    {isLoadingMore ? 'Loading…' : 'Load more'}
                  </button>
                )}
                {canLoadMore && (
                  <button
                    type="button"
                    className="code-review__load-full"
                    disabled={isLoadingMore}
                    onClick={onLoadFull}
                  >
                    Load full
                  </button>
                )}
              </div>
              {loadError && (
                <div className="code-review__window-error" role="alert">
                  {safeText(loadError)}{' '}
                  <button
                    type="button"
                    className="code-review__link"
                    disabled={isLoadingMore}
                    onClick={onLoadMore}
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
}
