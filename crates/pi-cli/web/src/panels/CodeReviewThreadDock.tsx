// Presentational per-hunk comment dock for the code-review workspace.
// Parent owns drafts, RPC submit/abort, and selected identity/thread.
//
// Comment bodies (user, assistant, and streamed partial replies) render
// through the shared MarkdownBody — the same renderer + hydration the
// transcript uses. renderMarkdown escapes every HTML character that is not
// produced by its own transforms, so hostile comment HTML stays literal text
// and scripts never execute. MarkdownBody hydrates each body's Mermaid hosts
// after that body's own commit and re-pins the dock on every image decode, so
// a comment's diagram (or a late-loading image) can neither stay unrendered
// nor leave the viewport behind. Thinking folds use MarkdownBody after newline normalize.
//
// Every comment card carries the owning session id, which is what makes the
// workspace-path chips inside a body dispatchable: the click dispatcher
// resolves a chip against its closest `[data-session-id]` owner and stays
// inert without one.
//
// Composer/Stop gating is scoped to the selected thread only: other hunks
// may stream concurrently without blocking submit on this one. Stop follows
// isCancellable (owned/queued work), not visible live status. The composer
// is uncontrolled (keyed by hunk identity): the parent reads live text
// through composerRef on submit and commits drafts on blur / hunk switch, so
// typing never re-renders the panel. internalBusy never locks the composer.
// Accepted comments arrive as a full-hunk-keyed follow event: the dock
// follows that hunk only while pinned. Hunk switch force-pins; rejection
// and a switched hunk never retarget the event. User scroll up suspends;
// returning to the bottom resumes.

import {
  useEffect,
  useMemo,
  useRef,
  type ChangeEvent,
  type KeyboardEvent as ReactKeyboardEvent,
  type Ref,
} from 'react';
import { type ComposerAttachment } from '../attachments';
import { MarkdownBody } from '../MarkdownBody';
import { thinkingSummaryHtml } from '../markdown';
import { safeText } from '../redact';
import { normalizeThinkingNewlines } from '../transcript';
import { useScrollPin } from '../scrollPin';
import {
  type CodeReviewFollowEvent,
  type HunkIdentity,
  type HunkReviewResult,
  type ReviewFindingRef,
  findingsForIdentity,
  findingRefMatches,
  reviewComposerLocked,
  threadIsCancellable,
  threadNeedsThinkingPlaceholder,
  type ReviewThread,
} from '../codeReview';

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
  /** Local RPC busy for the selected thread (comment/abort), not global. */
  busy: string | null;
  /** Owning session: stamped on every comment card so the path chips inside a
   *  comment body resolve against an owner (the click dispatcher requires a
   *  non-empty `data-session-id` ancestor). */
  sessionId: string | null;
  snapshotId: string;
  /** Snapshot's validated per-hunk review findings; the dock derives the
   *  selected hunk's issue list from it (null = unreviewed). */
  reviewResults: readonly HunkReviewResult[] | null;
  /** Selected finding; its identity must equal selectedIdentity (the parent
   *  clears the ref on hunk switch). Drives is-selected on the issue list. */
  selectedFinding: ReviewFindingRef | null;
  /** Select one of the current hunk's findings by its array index. */
  onSelectFinding: (findingIndex: number) => void;
  onBackToDiff: () => void;
  onRefresh: () => void;
  /** Schedule an rAF-coalesced auto-resize for the live composer element. */
  onComposerInput: (input: HTMLTextAreaElement) => void;
  /** Commit the live composer text into the parent's per-hunk draft map. */
  onDraftCommit: () => void;
  onComposerKeyDown: (e: ReactKeyboardEvent<HTMLTextAreaElement>) => void;
  onSubmitComment: () => void;
  /**
   * Accepted-comment follow event (full hunk key + monotonic seq) from the
   * parent: the dock follows ONLY while pinned when the event's key matches
   * its current hunk; a rejection never emits one and a switched hunk never
   * retargets it.
   */
  acceptedFollow: CodeReviewFollowEvent | null;
  onAbort: () => void;
  attachments: readonly ComposerAttachment[];
  attachmentAccept: string;
  isRecording: boolean;
  isTranscribing: boolean;
  onAttachFiles: (files: FileList) => void;
  onRemoveAttachment: (id: string) => void;
  onToggleRecording: () => void;
}

/** Role + optional model label for a committed or streaming assistant reply. */
function commentRoleLabel(role: string, model: string | undefined): string {
  if (role === 'assistant' && model) return `assistant · ${model}`;
  return role;
}

function ThinkingBlock({ thinking, onLayoutChange }: { thinking: string; onLayoutChange: () => void }) {
  const normalized = normalizeThinkingNewlines(thinking);
  return (
    <details className="thinking" open>
      <summary
        className="thinking__summary"
        dangerouslySetInnerHTML={{ __html: thinkingSummaryHtml() }}
      />
      <MarkdownBody
        className="thinking__body"
        text={normalized}
        onLayoutChange={onLayoutChange}
      />
    </details>
  );
}

/** Thread history + composer for the explicitly selected hunk. */
export function CodeReviewThreadDock({
  selectedIdentity,
  selectedThread,
  selectedDraft,
  composerKey,
  composerRef,
  acceptedFollow,
  isNarrow,
  isHidden,
  busy,
  sessionId,
  snapshotId,
  reviewResults,
  selectedFinding,
  onSelectFinding,
  onBackToDiff,
  onRefresh,
  onComposerInput,
  onDraftCommit,
  onComposerKeyDown,
  onSubmitComment,
  onAbort,
  attachments,
  attachmentAccept,
  isRecording,
  isTranscribing,
  onAttachFiles,
  onRemoveAttachment,
  onToggleRecording,
}: CodeReviewThreadDockProps) {
  const {
    transcriptRef: threadRef,
    transcriptContentRef: threadContentRef,
    onTranscriptScroll: onThreadScroll,
    pinIfPinned,
    forcePin,
  } = useScrollPin(selectedIdentity !== null && !isHidden);
  const appliedFollowRef = useRef<CodeReviewFollowEvent | null>(null);
  const threadKeyRef = useRef(composerKey);
  const attachInputRef = useRef<HTMLInputElement | null>(null);
  const selectedCancellable = threadIsCancellable(selectedThread);
  const streamingModel = selectedThread?.model;
  const composerLocked = reviewComposerLocked(busy, snapshotId);
  const hiddenRef = useRef(isHidden);
  useEffect(() => {
    if (!threadRef.current) return;
    const becameVisible = hiddenRef.current && !isHidden;
    hiddenRef.current = isHidden;
    if (isHidden) return;
    if (becameVisible || threadKeyRef.current !== composerKey) {
      threadKeyRef.current = composerKey;
      forcePin();
      appliedFollowRef.current = acceptedFollow;
      return;
    }
    if (
      acceptedFollow
      && appliedFollowRef.current !== acceptedFollow
      && acceptedFollow.key === composerKey
    ) {
      appliedFollowRef.current = acceptedFollow;
      pinIfPinned();
    } else {
      pinIfPinned();
    }
  }, [acceptedFollow, composerKey, forcePin, pinIfPinned, selectedThread, isHidden]);

  /** Owning session id stamped as the markdown path owner: on the thread (for
   *  markdown outside a comment card — the streamed thinking block) and on
   *  every comment card. A body inside a card resolves that closer,
   *  comment-scoped owner, so its basename completion reads one comment's
   *  text instead of the whole thread. No `data-workspace-cwd` is stamped:
   *  the review wire never carries the repository root (the snapshot builder
   *  omits it), so an ABSOLUTE path reference in review prose stays inert —
   *  the same behavior as a transcript click with no authoritative cwd. */
  const ownerSessionId = sessionId ?? '';


  // The selected hunk's review-finding buttons, in stored order; rendered
  // above the manual thread. Null when the hunk is unreviewed or clean.
  const findingButtons = useMemo(() => {
    if (!selectedIdentity || !reviewResults) return null;
    const identity = selectedIdentity;
    const findings = findingsForIdentity(reviewResults, identity);
    if (findings.length === 0) return null;
    return findings.map((finding, index) => {
      const selected = findingRefMatches(selectedFinding, {
        resultIndex: -1,
        findingIndex: index,
        identity,
        finding,
        firstRow: -1,
        lastRow: -1,
        fileRow: -1,
      });
      return (
        <button
          key={index}
          type="button"
          className={`code-review__finding${selected ? ' is-selected' : ''}`}
          aria-pressed={selected}
          onClick={() => onSelectFinding(index)}
        >
          <span className="code-review__finding-meta" style={{ display: 'block' }}>
            {finding.category} · {finding.side} L{finding.startLine}–{finding.endLine}
          </span>
          <span style={{ display: 'block' }}>{safeText(finding.summary)}</span>
          {finding.evidence !== '' && (
            <span className="code-review__finding-evidence" style={{ display: 'block' }}>
              {safeText(finding.evidence)}
            </span>
          )}
        </button>
      );
    });
  }, [reviewResults, selectedIdentity, selectedFinding, onSelectFinding]);

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
          {findingButtons && (
            <div className="code-review__finding-list" role="group" aria-label="Review issues">
              {findingButtons}
            </div>
          )}
          <div
            className="code-review__thread"
            ref={threadRef}
            aria-live="polite"
            onScroll={onThreadScroll}
          >
            <div
              className="code-review__thread-content"
              ref={threadContentRef}
              data-session-id={ownerSessionId}
            >
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
                  data-session-id={ownerSessionId}
                >
                  <span className="code-review__comment-role">
                    {safeText(commentRoleLabel(comment.role, comment.model))}
                  </span>
                  {comment.role === 'assistant' && comment.thinking ? (
                    <ThinkingBlock thinking={comment.thinking} onLayoutChange={pinIfPinned} />
                  ) : null}
                  <MarkdownBody
                    className="code-review__comment-text"
                    text={comment.text}
                    onLayoutChange={pinIfPinned}
                  />
                </div>
              ))}
              {selectedThread?.streamingThinking ? (
                <ThinkingBlock thinking={selectedThread.streamingThinking} onLayoutChange={pinIfPinned} />
              ) : null}
              {selectedThread?.streamingText ? (
                <div
                  className="code-review__comment code-review__comment--assistant is-partial"
                  data-session-id={ownerSessionId}
                >
                  <span className="code-review__comment-role">
                    {safeText(commentRoleLabel('assistant', streamingModel))}
                  </span>
                  <MarkdownBody
                    className="code-review__comment-text"
                    text={selectedThread.streamingText}
                    onLayoutChange={pinIfPinned}
                  />
                </div>
              ) : null}
              {threadNeedsThinkingPlaceholder(selectedThread) ? (
                <div
                  className="code-review__comment code-review__comment--assistant is-partial"
                  data-session-id={ownerSessionId}
                >
                  <span className="code-review__comment-role">
                    {safeText(commentRoleLabel('assistant', streamingModel))}
                  </span>
                  <span className="code-review__thinking-shimmer">Thinking…</span>
                </div>
              ) : null}
              {!selectedThread && (
                <div className="code-review__empty">No comments on this hunk yet</div>
              )}
            </div>
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
            {attachments.length > 0 && (
              <div className="code-review__composer-attachments" aria-label="Attached files">
                {attachments.map((attachment) => (
                  <span
                    key={attachment.id}
                    className="composer-attachment"
                    title={attachment.name}
                  >
                    {attachment.previewUrl ? (
                      <img className="composer-attachment__thumb" src={attachment.previewUrl} alt="" />
                    ) : null}
                    <span className="composer-attachment__meta">
                      <span className="composer-attachment__name">{safeText(attachment.name)}</span>
                    </span>
                    <button
                      type="button"
                      className="composer-attachment__remove"
                      title="Remove attachment"
                      aria-label={`Remove ${attachment.name}`}
                      onClick={() => onRemoveAttachment(attachment.id)}
                    >
                      ✕
                    </button>
                  </span>
                ))}
              </div>
            )}
            <input
              ref={attachInputRef}
              type="file"
              accept={attachmentAccept}
              multiple
              hidden
              onChange={(e: ChangeEvent<HTMLInputElement>) => {
                const files = e.target.files;
                if (files) onAttachFiles(files);
                e.target.value = '';
              }}
            />
            <div className="code-review__composer-actions">
              <button
                type="button"
                className="code-review__composer-btn code-review__composer-btn--attach"
                disabled={composerLocked}
                onClick={() => attachInputRef.current?.click()}
                title="Attach images"
                aria-label="Attach files"
              >
                <span aria-hidden="true">📎</span>
              </button>
              <button
                type="button"
                className={`code-review__composer-btn code-review__composer-btn--mic${isRecording ? ' recording' : isTranscribing ? ' transcribing' : ''}`}
                disabled={composerLocked || isTranscribing}
                aria-label="Microphone"
                aria-pressed={isRecording}
                title={
                  isRecording
                    ? 'Recording… release to transcribe'
                    : isTranscribing
                      ? 'Transcribing…'
                      : 'Hold to talk (voice to text)'
                }
                onClick={onToggleRecording}
              >
                <span aria-hidden="true">🎤</span>
              </button>
              {selectedCancellable ? (
                <button
                  type="button"
                  className="code-review__action code-review__action--stop"
                  disabled={busy !== null}
                  onClick={onAbort}
                  title="Stop generating (Esc)"
                  aria-label="Stop generating"
                >
                  <span aria-hidden="true">■</span>
                  Stop
                </button>
              ) : (
                <button
                  type="button"
                  className="code-review__action code-review__action--send"
                  disabled={composerLocked}
                  onClick={onSubmitComment}
                  title="Submit comment (Enter)"
                >
                  <span aria-hidden="true">➤</span>
                  Comment
                </button>
              )}
              <span className="code-review__composer-hint">
                {selectedCancellable
                  ? 'Enter to submit · Shift+Enter for a newline · Esc to stop'
                  : 'Enter to submit · Shift+Enter for a newline'}
              </span>
            </div>
          </div>
        </>
      )}
    </div>
  );
}
