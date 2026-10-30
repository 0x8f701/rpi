// Presentational changed-files rail for the code-review workspace.
// Parent owns the tree/collapse state, filter, selection, focus-roving
// state, and keyboard handlers. Rows arrive pre-computed by treeFilterRows
// (a filter keeps every matched file plus its ancestor directories, all
// forced expanded). Directory rows and file rows share the rail so the
// keyboard traversal spans the whole tree.

import { memo } from 'react';
import type { KeyboardEvent as ReactKeyboardEvent, MutableRefObject } from 'react';
import { safeText } from '../redact';
import {
  type AutoReviewAggregate,
  reviewCategoryTone,
  type DiffFile,
  type FileTree,
  type ReviewThread,
  type VisibleTreeRow,
  countFileThreads,
  emptyComparisonMessage,
  fileChangeStats,
  fileStatusLetter,
  diffTreePaddingLeft,
  untrackedNotShownNote,
} from '../codeReview';

/**
 * Row stats: authoritative +N/-M totals plus the file's ACTUAL body state.
 * The global preview cap is internal provenance — `preview`/`onDemand` are
 * pageable states and only a genuine backend byte cap surfaces a cap label,
 * never a generic "truncated".
 */
function fileStats(file: DiffFile): string {
  const stats = fileChangeStats(file);
  const parts: string[] = [];
  if (stats.insertions) parts.push(`+${stats.insertions}`);
  if (stats.deletions) parts.push(`-${stats.deletions}`);
  if (file.binary) parts.push('binary');
  else if (file.bodyState === 'preview') parts.push('preview');
  else if (file.bodyState === 'onDemand') parts.push('on demand');
  else if (file.bodyState === 'byteCapped') parts.push('size-capped');
  return parts.join(' ');
}

export interface CodeReviewFileListProps {
  files: DiffFile[];
  tree: FileTree;
  rows: VisibleTreeRow[];
  threads: ReviewThread[];
  /** Per-file review-finding aggregates (issue count + highest category)
   *  keyed by path, derived from the snapshot's reviewFindings. */
  autoByFile: Map<string, AutoReviewAggregate>;
  /** Per-directory review-finding aggregates keyed by directory path. */
  autoByDir: Map<string, AutoReviewAggregate>;
  selectedPath: string;
  fileQuery: string;
  busy: string | null;
  isHidden: boolean;
  untrackedNotShown?: number;
  rowButtonRefs: MutableRefObject<Array<HTMLButtonElement | null>>;
  onFileQueryChange: (query: string) => void;
  onSelectFile: (path: string, visibleIndex: number) => void;
  onToggleDir: (nodeIndex: number, visibleIndex: number) => void;
  onFileListKeyDown: (e: ReactKeyboardEvent<HTMLUListElement>) => void;
}

/** Filter + collapsible path tree of changed files with thread badges. */
export const CodeReviewFileList = memo(function CodeReviewFileList({
  files,
  tree,
  rows,
  threads,
  autoByFile,
  autoByDir,
  selectedPath,
  fileQuery,
  busy,
  isHidden,
  untrackedNotShown = 0,
  rowButtonRefs,
  onFileQueryChange,
  onSelectFile,
  onToggleDir,
  onFileListKeyDown,
}: CodeReviewFileListProps) {
  const untrackedNote = untrackedNotShownNote(untrackedNotShown);
  return (
    <aside
      className={`code-review__files${isHidden ? ' is-hidden' : ''}`}
      aria-label="Changed files"
    >
      <input
        type="search"
        className="code-review__file-filter"
        value={fileQuery}
        onChange={(e) => onFileQueryChange(e.target.value)}
        placeholder="Filter files…"
        aria-label="Filter changed files"
      />
      {files.length === 0 && !busy && (
        <div className="code-review__empty">{emptyComparisonMessage(untrackedNotShown)}</div>
      )}
      {files.length > 0 && untrackedNote && (
        <div className="code-review__empty">{untrackedNote}</div>
      )}
      {files.length > 0 && rows.length === 0 && (
        <div className="code-review__empty">No files match your search.</div>
      )}
      <ul
        className="code-review__file-list"
        role="tree"
        aria-label="Files"
        tabIndex={rows.length > 0 ? 0 : -1}
        onKeyDown={onFileListKeyDown}
      >
        {rows.map((row, visibleIndex) => {
          const node = tree.nodes[row.nodeIndex];
          if (!node) return null;
          const indent = { paddingLeft: diffTreePaddingLeft(node.depth) };
          if (node.kind === 'dir') {
            const dirAuto = autoByDir.get(node.path);
            const dirTone = reviewCategoryTone(dirAuto?.category);
            return (
              <li
                key={node.id}
                role="treeitem"
                className="code-review__tree-row"
                data-tree-kind="dir"
                data-tree-id={node.id}
                aria-expanded={row.expanded}
                aria-level={node.depth + 1}
              >
                <button
                  type="button"
                  ref={(el) => {
                    rowButtonRefs.current[visibleIndex] = el;
                  }}
                  tabIndex={-1}
                  className={`code-review__tree-dir${dirTone ? ` code-review__file-row--${dirTone} code-review__file-row--dir` : ''}`}
                  style={indent}
                  onClick={() => onToggleDir(row.nodeIndex, visibleIndex)}
                  title={safeText(node.path)}
                  data-dir-path={node.path}
                >
                  <span className="code-review__tree-twisty" aria-hidden="true">
                    {row.expanded ? '▾' : '▸'}
                  </span>
                  <span className="code-review__file-path">{safeText(node.name)}</span>
                  <span className="code-review__file-stats">
                    {node.insertions > 0 ? `+${node.insertions}` : ''}
                    {node.deletions > 0 ? ` -${node.deletions}` : ''}
                  </span>
                  {dirAuto != null && dirAuto.issues > 0 && (
                    <span
                      className="code-review__file-badge code-review__file-issues"
                      title={`${dirAuto.issues} review issue${dirAuto.issues === 1 ? '' : 's'}`}
                    >
                      {dirAuto.issues}
                    </span>
                  )}
                </button>
              </li>
            );
          }
          const file = files[node.fileIndex ?? -1];
          if (!file) return null;
          const active = selectedPath === file.path;
          const threadCount = countFileThreads(threads, file);
          const fileAuto = autoByFile.get(file.path);
          const fileTone = reviewCategoryTone(fileAuto?.category);
          return (
            <li
              key={file.path}
              role="treeitem"
              aria-selected={active}
              aria-level={node.depth + 1}
            >
              <button
                type="button"
                ref={(el) => {
                  rowButtonRefs.current[visibleIndex] = el;
                }}
                tabIndex={-1}
                className={`code-review__file${active ? ' is-active' : ''}${fileTone ? ` code-review__file-row--${fileTone}` : ''}`}
                style={indent}
                onClick={() => onSelectFile(file.path, visibleIndex)}
                title={safeText(file.path)}
                data-file-path={file.path}
                aria-label={
                  fileAuto != null && fileAuto.issues > 0
                    ? `${file.status}: ${file.path} · ${fileAuto.issues} review issue${fileAuto.issues === 1 ? '' : 's'}`
                    : `${file.status}: ${file.path}`
                }
              >
                <span
                  className={`code-review__file-status code-review__file-status--${file.status}`}
                  aria-hidden="true"
                >
                  {fileStatusLetter(file.status)}
                </span>
                <span className="code-review__file-path">{safeText(node.name)}</span>
                <span className="code-review__file-stats">{fileStats(file)}</span>
                {threadCount > 0 && (
                  <span
                    className="code-review__file-badge"
                    title={`${threadCount} comment${threadCount === 1 ? '' : 's'}`}
                  >
                    {threadCount}
                  </span>
                )}
                {fileAuto != null && fileAuto.issues > 0 && (
                  <span
                    className="code-review__file-badge code-review__file-issues"
                    title={`${fileAuto.issues} review issue${fileAuto.issues === 1 ? '' : 's'}`}
                  >
                    {fileAuto.issues}
                  </span>
                )}
              </button>
            </li>
          );
        })}
      </ul>
    </aside>
  );
});
