// Presentational per-hunk comment dock for the code-review workspace.
// Parent owns drafts, RPC submit/abort, and selected identity/thread.
//
// Comment bodies (user, assistant, and streamed partial replies) render
// through the shared markdown pipeline (renderMarkdown + hydrateMermaid) —
// the same renderer the transcript uses. renderMarkdown escapes every HTML
// character that is not produced by its own transforms, so hostile comment
// HTML stays literal text and scripts never execute. Mermaid fences are
// hydrated asynchronously after each commit.
//
// Composer/abort gating is scoped to the selected thread only: other hunks
// may stream concurrently without blocking submit on this one. The composer
// is uncontrolled (keyed by hunk identity): the parent reads live text
// through composerRef on submit and commits drafts on blur / hunk switch, so
// typing never re-renders the panel.

import {
  useEffect,
  useRef,
  type KeyboardEvent as ReactKeyboardEvent,
  type Ref,
} from 'react';
import { hydrateMermaid, renderMarkdown } from '../markdown';
import { safeText } from '../redact';
import { type HunkIdentity, type ReviewThread, threadIsStreaming } from '../codeReview';

export interface CodeReviewThreadDockProps {
  selectedIdentity: HunkIdentity | null;
  selectedThread: ReviewThread | undefined;
  /** Committed draft seeding the uncontrolled composer when the hunk mounts. */
  selectedDraft: string;
  /** Stable hunk identity the uncontrolled textarea is keyed by (switch remounts). */
  composerKey: string | null;
  /** Imperative surface: parent reads live text on submit and clears on success. */
  composerRef: Ref<HTMLTextAreaElement>;
  isNarrow: boolean;
  isHidden: boolean;
  /** True only while the selected thread itself is streaming. */
  isStreaming: boolean;
  /** Local RPC busy for the selected thread (comment/abort), not global. */
  busy: string | null;
  snapshotId: string;
  onBackToDiff: () => void;
  onRefresh: () => void;
  /** Schedule an rAF-coalesced auto-resize for the live composer element. */
  onComposerInput: (input: HTMLTextAreaElement) => void;
  /** Commit the live composer text into the parent's per-hunk draft map. */
  onDraftCommit: () => void;
  onComposerKeyDown: (e: ReactKeyboardEvent<HTMLTextAreaElement>) => void;
  onSubmitComment: () => void;
  onAbort: () => void;
}

/** Role + optional model label for a committed or streaming assistant reply. */
function commentRoleLabel(role: string, model: string | undefined): string {
  if (role === 'assistant' && model) return `assistant · ${model}`;
  return role;
}

/** Thread history + composer for the explicitly selected hunk. */
export function CodeReviewThreadDock({
  selectedIdentity,
  selectedThread,
  selectedDraft,
  composerKey,
  composerRef,
  isNarrow,
  isHidden,
  isStreaming,
  busy,
  snapshotId,
  onBackToDiff,
  onRefresh,
  onComposerInput,
  onDraftCommit,
  onComposerKeyDown,
  onSubmitComment,
  onAbort,
}: CodeReviewThreadDockProps) {
  const threadRef = useRef<HTMLDivElement | null>(null);
  const selectedStreaming = isStreaming || threadIsStreaming(selectedThread);
  const streamingModel = selectedThread?.model;
  // Only local comment/abort RPC busy or a missing snapshot locks the
  // composer; a streaming reply queues instead of blocking new comments.
  const composerLocked = busy !== null || !snapshotId;

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
                <span className="code-review__comment-role">
                  {safeText(commentRoleLabel(comment.role, comment.model))}
                </span>
                <div
                  className="code-review__comment-text"
                  dangerouslySetInnerHTML={{ __html: renderMarkdown(comment.text) }}
                />
              </div>
            ))}
            {selectedThread?.streamingText ? (
              <div className="code-review__comment code-review__comment--assistant is-partial">
                <span className="code-review__comment-role">
                  {safeText(commentRoleLabel('assistant', streamingModel))}
                </span>
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
            <textarea
              key={composerKey ?? 'no-hunk'}
              className="code-review__comment-input"
              ref={composerRef}
              defaultValue={selectedDraft}
              onInput={(e) => onComposerInput(e.currentTarget)}
              onBlur={onDraftCommit}
              onKeyDown={onComposerKeyDown}
              placeholder="Comment on this hunk…"
              rows={3}
              aria-label="Hunk comment"
              disabled={composerLocked}
            />
            <div className="code-review__composer-actions">
              <button
                type="button"
                className="code-review__action"
                disabled={composerLocked}
                onClick={onSubmitComment}
                title="Submit comment (Enter)"
              >
                Comment
              </button>
              {selectedStreaming && (
                <button
                  type="button"
                  className="code-review__action code-review__action--warn"
                  disabled={busy !== null}
                  onClick={onAbort}
                  title="Abort this hunk's in-flight review reply"
                >
                  Abort
                </button>
              )}
              <span className="code-review__composer-hint">
                Enter to submit · Shift+Enter for a newline
              </span>
            </div>
          </div>
        </>
      )}
    </div>
  );
}
