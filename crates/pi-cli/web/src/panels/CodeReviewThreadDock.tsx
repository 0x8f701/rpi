// Presentational per-hunk comment dock for the code-review workspace.
// Parent owns drafts, RPC submit, and selected identity/thread.
//
// Comment bodies (user, assistant, and streamed partial replies) render
// through the shared markdown pipeline (renderMarkdown + hydrateMermaid) —
// the same renderer the transcript uses. renderMarkdown escapes every HTML
// character that is not produced by its own transforms, so hostile comment
// HTML stays literal text and scripts never execute. Mermaid fences are
// hydrated asynchronously after each commit.

import { useEffect, useRef, type KeyboardEvent as ReactKeyboardEvent } from 'react';
import { hydrateMermaid, renderMarkdown } from '../markdown';
import { safeText } from '../redact';
import { type HunkIdentity, type ReviewThread } from '../codeReview';

export interface CodeReviewThreadDockProps {
  selectedIdentity: HunkIdentity | null;
  selectedThread: ReviewThread | undefined;
  selectedDraft: string;
  isNarrow: boolean;
  isHidden: boolean;
  isStreaming: boolean;
  busy: string | null;
  snapshotId: string;
  onBackToDiff: () => void;
  onRefresh: () => void;
  onDraftChange: (text: string) => void;
  onComposerKeyDown: (e: ReactKeyboardEvent<HTMLTextAreaElement>) => void;
  onSubmitComment: () => void;
}

/** Thread history + composer for the explicitly selected hunk. */
export function CodeReviewThreadDock({
  selectedIdentity,
  selectedThread,
  selectedDraft,
  isNarrow,
  isHidden,
  isStreaming,
  busy,
  snapshotId,
  onBackToDiff,
  onRefresh,
  onDraftChange,
  onComposerKeyDown,
  onSubmitComment,
}: CodeReviewThreadDockProps) {
  const threadRef = useRef<HTMLDivElement | null>(null);

  // Mermaid fences in comment bodies render asynchronously; hydrate the
  // hosts after every thread render. Safe to run repeatedly: hosts already
  // claimed via data-mermaid="done" are skipped.
  useEffect(() => {
    const node = threadRef.current;
    if (node) void hydrateMermaid(node);
  }, [selectedThread]);

  return (
    <div
      className={`code-review__comments${isHidden ? ' is-hidden' : ''}`}
      aria-label="Hunk comments"
    >
      <div className="code-review__comments-head">
        <span>Hunk review</span>
        {isNarrow && selectedIdentity && (
          <button type="button" className="code-review__back" onClick={onBackToDiff}>
            ← Back to diff
          </button>
        )}
      </div>
      {!selectedIdentity && (
        <div className="code-review__empty">
          Select a hunk to comment — hunks are never auto-selected.
        </div>
      )}
      {selectedIdentity && (
        <>
          <div className="code-review__hunk-id" title={safeText(selectedIdentity.contentHash)}>
            {safeText(selectedIdentity.path)} · @@ -{selectedIdentity.oldStart},
            {selectedIdentity.oldCount} +{selectedIdentity.newStart},
            {selectedIdentity.newCount}
          </div>
          <div className="code-review__thread" ref={threadRef} aria-live="polite">
            {selectedThread?.stale && (
              <div className="code-review__thread-stale">
                Thread may be stale after refresh.{' '}
                <button type="button" className="code-review__link" onClick={onRefresh}>
                  Refresh now
                </button>
              </div>
            )}
            {selectedThread?.error && (
              <div className="code-review__error">{safeText(selectedThread.error)}</div>
            )}
            {(selectedThread?.comments ?? []).map((comment, idx) => (
              <div
                key={`${selectedIdentity.contentHash}:c${idx}`}
                className={`code-review__comment code-review__comment--${comment.role}${
                  comment.partial ? ' is-partial' : ''
                }`}
              >
                <span className="code-review__comment-role">{comment.role}</span>
                <div
                  className="code-review__comment-text"
                  dangerouslySetInnerHTML={{ __html: renderMarkdown(comment.text) }}
                />
              </div>
            ))}
            {selectedThread?.streamingText ? (
              <div className="code-review__comment code-review__comment--assistant is-partial">
                <span className="code-review__comment-role">assistant</span>
                <div
                  className="code-review__comment-text"
                  dangerouslySetInnerHTML={{ __html: renderMarkdown(selectedThread.streamingText) }}
                />
              </div>
            ) : null}
            {!selectedThread && (
              <div className="code-review__empty">No comments on this hunk yet</div>
            )}
          </div>
          <div className="code-review__composer">
            {isStreaming && (
              <div className="code-review__streaming-note" role="status">
                A review reply is streaming — abort it before commenting.
              </div>
            )}
            <textarea
              className="code-review__comment-input"
              value={selectedDraft}
              onChange={(e) => onDraftChange(e.target.value)}
              onKeyDown={onComposerKeyDown}
              placeholder="Comment on this hunk…"
              rows={3}
              aria-label="Hunk comment"
              disabled={busy !== null || !snapshotId}
            />
            <div className="code-review__composer-actions">
              <button
                type="button"
                className="code-review__action"
                disabled={
                  busy !== null || !selectedDraft.trim() || !snapshotId || isStreaming
                }
                onClick={onSubmitComment}
                title="Submit comment (Ctrl+Enter)"
              >
                Comment
              </button>
              <span className="code-review__composer-hint">Ctrl+Enter to submit</span>
            </div>
          </div>
        </>
      )}
    </div>
  );
}
