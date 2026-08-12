// Presentational changed-files rail for the code-review workspace.
// Parent owns the tree/collapse state, filter, selection, focus-roving
// state, and keyboard handlers. Rows arrive pre-computed by treeFilterRows
// (a filter keeps every matched file plus its ancestor directories, all
// forced expanded). Directory rows and file rows share the rail so the
// keyboard traversal spans the whole tree.

import type { KeyboardEvent as ReactKeyboardEvent, MutableRefObject } from 'react';
import { safeText } from '../redact';
import {
  type DiffFile,
  type FileTree,
  type ReviewThread,
  type VisibleTreeRow,
  countFileThreads,
  fileStatusLetter,
} from '../codeReview';

function fileStats(file: DiffFile): string {
  const parts: string[] = [];
  if (file.insertions) parts.push(`+${file.insertions}`);
  if (file.deletions) parts.push(`-${file.deletions}`);
  if (file.binary) parts.push('binary');
  if (file.truncated) parts.push('truncated');
  return parts.join(' ');
}

export interface CodeReviewFileListProps {
  files: DiffFile[];
  tree: FileTree;
  rows: VisibleTreeRow[];
  threads: ReviewThread[];
  selectedPath: string;
  fileQuery: string;
  busy: string | null;
  isHidden: boolean;
  rowButtonRefs: MutableRefObject<Array<HTMLButtonElement | null>>;
  onFileQueryChange: (query: string) => void;
  onSelectFile: (path: string, visibleIndex: number) => void;
  onToggleDir: (nodeIndex: number, visibleIndex: number) => void;
  onFileListKeyDown: (e: ReactKeyboardEvent<HTMLUListElement>) => void;
}

/** Filter + collapsible path tree of changed files with thread badges. */
export function CodeReviewFileList({
  files,
  tree,
  rows,
  threads,
  selectedPath,
  fileQuery,
  busy,
  isHidden,
  rowButtonRefs,
  onFileQueryChange,
  onSelectFile,
  onToggleDir,
  onFileListKeyDown,
}: CodeReviewFileListProps) {
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
        <div className="code-review__empty">No changed files in this comparison.</div>
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
          const indent = { paddingLeft: `${8 + node.depth * 14}px` };
          if (node.kind === 'dir') {
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
                  className="code-review__tree-dir"
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
                </button>
              </li>
            );
          }
          const file = files[node.fileIndex ?? -1];
          if (!file) return null;
          const active = selectedPath === file.path;
          const threadCount = countFileThreads(threads, file);
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
                className={`code-review__file${active ? ' is-active' : ''}`}
                style={indent}
                onClick={() => onSelectFile(file.path, visibleIndex)}
                title={safeText(file.path)}
                data-file-path={file.path}
                aria-label={`${file.status}: ${file.path}`}
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
              </button>
            </li>
          );
        })}
      </ul>
    </aside>
  );
}
