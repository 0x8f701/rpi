// Selection toolbar. Mounted outside the source pre so a press cannot clear the range.

import { safeText } from '../redact';
import type { FileConversationLineRange, FileExplorerBlameResult } from '../fileExplorer';
import type { CSSProperties } from 'react';

const FILE_SELECTION_MAX_DATE_SECONDS = 8.64e12;

export function fileSelectionRangeLabel(range: FileConversationLineRange): string {
  return range.startLine === range.endLine
    ? `Line ${range.startLine}`
    : `Lines ${range.startLine}–${range.endLine}`;
}

export function fileSelectionCommitLabel(commit: string | null): { label: string; title: string } {
  if (commit == null) return { label: 'Uncommitted', title: 'Uncommitted' };
  const short = commit.length > 9 ? commit.slice(0, 9) : commit;
  return { label: short, title: commit };
}

export function fileSelectionBlameDate(time: number): string | null {
  if (!Number.isInteger(time) || time < 0 || time > FILE_SELECTION_MAX_DATE_SECONDS) return null;
  const date = new Date(time * 1000);
  if (Number.isNaN(date.getTime())) return null;
  const year = date.getFullYear();
  const month = String(date.getMonth() + 1).padStart(2, '0');
  const day = String(date.getDate()).padStart(2, '0');
  return `${year}-${month}-${day}`;
}

export function fileSelectionBlameMatches(
  result: FileExplorerBlameResult | null,
  expected: { path: string; startLine: number; endLine: number } | null,
): result is FileExplorerBlameResult {
  if (result == null || expected == null) return false;
  if (result.path !== expected.path) return false;
  const count = expected.endLine - expected.startLine + 1;
  if (result.lines.length !== count) return false;
  return result.lines.every((line, index) => line.currentLine === expected.startLine + index);
}

export function FileSelectionActions({
  range,
  pending,
  blame,
  blameRef,
  blameStyle,
  onAsk,
  onBlame,
  onClose,
}: {
  range: FileConversationLineRange;
  pending: boolean;
  blame: FileExplorerBlameResult | null;
  /** Callback ref for the blame popup; the anchor measures it once rendered. */
  blameRef?: (node: HTMLOListElement | null) => void;
  /** Measured anchor box for the popup (bar-relative top/left plus clamped
   *  max sizes); undefined keeps the CSS pre-measure fallback. */
  blameStyle?: CSSProperties;
  onAsk: () => void;
  onBlame: () => void;
  onClose: () => void;
}) {
  return (
    <div
      className="file-selection-actions"
      role="toolbar"
      aria-label="Selected lines"
      data-testid="file-selection-actions"
      aria-busy={pending}
      onMouseDown={(event) => event.preventDefault()}
    >
      <span className="file-selection-actions__range">{fileSelectionRangeLabel(range)}</span>
      <button
        type="button"
        className="file-selection-actions__btn"
        data-testid="file-selection-ask"
        aria-label="Ask"
        title="Ask about the selected lines"
        disabled={pending}
        onClick={onAsk}
      >
        <span className="file-selection-actions__glyph" aria-hidden="true">✎</span>
        Ask
      </button>
      <button
        type="button"
        className="file-selection-actions__btn"
        data-testid="file-selection-blame"
        aria-label="Blame"
        title="Show git blame for the selected lines"
        disabled={pending}
        onClick={onBlame}
      >
        {pending ? (
          <span className="file-explorer__pane-shimmer" aria-hidden="true" />
        ) : (
          <span className="file-selection-actions__glyph" aria-hidden="true">☰</span>
        )}
        Blame
      </button>
      <button
        type="button"
        className="file-selection-actions__close"
        data-testid="file-selection-close"
        aria-label="Close selection"
        title="Close selection"
        onClick={onClose}
      >
        {'\u00D7'}
      </button>
      {blame && (
        <ol ref={blameRef} style={blameStyle} className="file-selection-actions__blame" data-testid="file-selection-blame-list">
          {blame.lines.map((line) => {
            const commit = fileSelectionCommitLabel(line.commit);
            return (
              <li
                key={line.currentLine}
                className="file-selection-actions__blame-line"
                data-line={line.currentLine}
              >
                <span className="file-selection-actions__commit" title={commit.title}>{safeText(commit.label)}</span>
                <span className="file-selection-actions__author">{safeText(line.author)}</span>
                <span className="file-selection-actions__date">{fileSelectionBlameDate(line.time)}</span>
                <span className="file-selection-actions__text">{safeText(line.text)}</span>
              </li>
            );
          })}
        </ol>
      )}
    </div>
  );
}
