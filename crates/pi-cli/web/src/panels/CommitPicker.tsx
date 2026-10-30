// Recent-commit chooser for `/commit` — a controlled popover that reuses the
// command-picker dialog geometry (`.command-picker` wrapper + `__popover`
// opening upward from the composer) and its option/hint styling. The HOST
// owns all data flow: it fetches `code_review_commits`, normalizes the wire
// via normalizeCodeReviewCommits, and passes rows/loading down; this
// component only searches, keyboard-navigates, and reports the decision:
//   - Enter picks the active row's full hash, or — when nothing matches —
//     the typed text as a single-token ref ("HEAD~1"); a multi-token tail
//     reports `Usage: /commit [<ref>]` via onError instead of a pick.
//   - Escape (and outside mousedown) closes WITHOUT issuing any selection.
//   - A selection fires onSelect(ref) exactly once; closing after a pick is
//     the host's job (it owns `open`), so Escape is the only onClose source
//     inside this component.
import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import {
  commitPickerEnterAction,
  filterCommitRows,
  shortCommitLabel,
  type CodeReviewCommitRow,
} from '../codeReview';

export interface CommitPickerProps {
  /** Controlled visibility (host state). False renders nothing. */
  open?: boolean;
  /** Normalized recent-commit rows, newest first (already backend-capped). */
  rows: CodeReviewCommitRow[];
  /** True while the host's code_review_commits fetch is in flight. */
  loading?: boolean;
  /** A picked row hash OR a typed single-token ref; the host opens the review. */
  onSelect: (ref: string) => void;
  /** Escape / outside click — closes without issuing any selection. */
  onClose: () => void;
  /** A typed multi-token tail can never be one ref; host toasts the message. */
  onError: (message: string) => void;
}

export function CommitPicker({
  open = false,
  rows,
  loading = false,
  onSelect,
  onClose,
  onError,
}: CommitPickerProps) {
  const [query, setQuery] = useState('');
  const [activeIndex, setActiveIndex] = useState(0);
  const searchRef = useRef<HTMLInputElement>(null);
  const dialogRef = useRef<HTMLDivElement>(null);
  const activeRowRef = useRef<HTMLLIElement | null>(null);
  const listId = 'commit-picker-list';

  // (Re)entering resets the search and focuses it, like the command picker.
  useEffect(() => {
    if (open) {
      setQuery('');
      setActiveIndex(0);
      searchRef.current?.focus();
    }
  }, [open]);

  // Outside-mousedown dismisses; a press inside never does (mousedown, not
  // click, so a row's own click selection always lands first — the same
  // contract as the command picker's outside-click dismiss).
  useEffect(() => {
    if (!open) return;
    function onDown(e: MouseEvent) {
      if (dialogRef.current && !dialogRef.current.contains(e.target as Node)) onClose();
    }
    document.addEventListener('mousedown', onDown);
    return () => document.removeEventListener('mousedown', onDown);
  }, [open, onClose]);

  const filtered = useMemo(() => filterCommitRows(rows, query), [rows, query]);
  const safeActive = filtered.length === 0 ? -1 : Math.min(activeIndex, filtered.length - 1);

  // Keyboard/filter movement keeps the active option visible: scroll it into
  // view by the nearest edge so the popover never jumps wholesale. The
  // optional call keeps non-browser DOMs (tests) safe.
  useEffect(() => {
    activeRowRef.current?.scrollIntoView?.({ block: 'nearest' });
  }, [safeActive, filtered]);

  if (!open) return null;

  function onKeyDown(e: KeyboardEvent<HTMLInputElement>) {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      if (filtered.length) setActiveIndex((i) => Math.min(i + 1, filtered.length - 1));
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      if (filtered.length) setActiveIndex((i) => Math.max(i - 1, 0));
    } else if (e.key === 'Enter') {
      e.preventDefault();
      // While the listing is in flight nothing selectable is on screen —
      // Enter must never pick a hidden row (or a ref) against the loading UI.
      if (loading) return;
      const action = commitPickerEnterAction(filtered, safeActive, query);
      if (action.kind === 'row' || action.kind === 'typed') {
        onSelect(action.ref);
      } else if (action.kind === 'invalid') {
        onError(action.message);
      }
      // 'none': nothing active and nothing typed — stay open.
    } else if (e.key === 'Escape') {
      e.preventDefault();
      onClose();
    }
  }

  return (
    <div className="command-picker" ref={dialogRef}>
      <div className="command-picker__popover" role="dialog" aria-label="Recent commits">
        <input
          ref={searchRef}
          className="command-picker__search"
          type="text"
          placeholder="Search commits or type a ref…"
          value={query}
          aria-label="Search commits"
          aria-controls={listId}
          aria-autocomplete="list"
          autoComplete="off"
          onChange={(e) => {
            setQuery(e.target.value);
            setActiveIndex(0);
          }}
          onKeyDown={onKeyDown}
        />
        <ul id={listId} className="command-picker__list" role="listbox" aria-label="Recent commits">
          {loading && <li className="command-picker__hint" role="status">Loading…</li>}
          {!loading && filtered.length === 0 && (
            query.trim() === '' ? (
              <li className="command-picker__hint" role="status">No recent commits</li>
            ) : (
              <li className="command-picker__hint">
                No commits match — press Enter to review the typed ref
              </li>
            )
          )}
          {!loading && filtered.map((row, i) => (
            <li
              key={row.commit}
              role="option"
              data-commit={row.commit}
              ref={i === safeActive ? activeRowRef : undefined}
              className={`command-picker__option${i === safeActive ? ' is-active' : ''}`}
              aria-selected={i === safeActive}
              onMouseDown={(e) => { e.preventDefault(); }}
              onClick={() => onSelect(row.commit)}
              onMouseEnter={() => setActiveIndex(i)}
            >
              <span className="command-picker__name">{shortCommitLabel(row.commit)}</span>
              <span className="command-picker__arg">{row.author} · {row.authorDate}</span>
              <span className="command-picker__desc">{row.subject}</span>
            </li>
          ))}
        </ul>
      </div>
    </div>
  );
}
