// Code-review fullscreen workspace — file list + selected file diff + per-hunk
// comment threads with streamed/polled replies. RPC surface:
//   code_review_open | snapshot | refresh | comment | abort | close | file_diff | file_context
//
// CRITICAL multi-session invariant: every code_review_* command stamps the
// owning `sessionId` prop explicitly. On A→B switch, sessionIdRef already
// points to B; an unstamped unmount close would close B's controller. The
// mount site keys the panel by session and passes the owning sid as a prop.
//
// Wire shapes are defensively normalized in ../codeReview.ts. EVERY
// model/diff string is rendered via React text (automatic escaping) after
// redactSecrets() so secrets never paint raw. No innerHTML for diff content.
//
// Review semantics: hunks are NEVER auto-selected — a comment target exists
// only after the user explicitly selects a hunk (header, line, or keyboard).
// Switching files clears the hunk selection. File/hunk selection is preserved
// across polls; an invalid selection renders an empty state instead of
// silently jumping to another file/hunk.
//
// Presentational leaves (DOM class/selector contract preserved):
//   CodeReviewFileList | CodeReviewDiffPane | CodeReviewThreadDock
//   CodeReviewConfirmDialog

import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type PointerEvent as ReactPointerEvent,
} from 'react';
import { createAutoResizeController, type AutoResizeController } from '../autoResize';
import { safeText } from '../redact';
import {
  AUTO_REVIEW_SPINNER_GATE_MS,
  applyAutoReviewButtonEvent,
  autoReviewBusy,
  autoReviewButtonView,
  highlightButtonView,
  autoReviewLanguagePayload,
  buildCodeReviewAutoReviewPayload,
  buildCodeReviewHighlightPayload,
  buildCodeReviewTldrPayload,
  buildCodeReviewSetModelPayload,
  buildCodeReviewStopPayload,
  initialAutoReviewButtonState,
  initialHighlightOwnerState,
  initialReviewModelState,
  applyHighlightOwnerEvent,
  highlightOwnerBusy,
  codeReviewBatchOwnerLive,
  snapshotPollDelayMs,
  applyReviewModelEvent,
  reviewModelOptionValue,
  parseCodeReviewAutoReviewAck,
  parseCodeReviewHighlightAck,
  parseCodeReviewTldrAck,
  parseCodeReviewSetModelAck,
  reviewLanguageSelection,
  reviewModelLabel,
  reviewModelPending,
  reviewErrorNotices,
  initialReviewErrorReport,
  reviewModelSelectValue,
  normalizeAvailableModels,
  REVIEW_LANGUAGE_OPTIONS,
  type AutoReviewButtonState,
  isNoOpenCodeReviewError,
  type CodeReviewStopOperation,
  type CodeReviewFollowEvent,
  type HighlightOwnerState,
  type ReviewErrorReport,
  type ReviewModelState,
  tldrButtonView,
  tldrMarkdownForSnapshot,
} from '../codeReview';
import {
  clearCodeReviewToastAnchor,
  syncCodeReviewToastAnchor,
} from '../codeReviewToastAnchor';
import {
  FILE_DIFF_MAX_UI_LINES,
  FILE_PAGE_REQUEST_LINES,
  CODE_REVIEW_THREAD_WIDTH_MAX,
  CODE_REVIEW_THREAD_WIDTH_MIN,
  type CodeReviewSnapshot,
  type DiffFile,
  type DiffHunk,
  type DiffLine,
  type FileDiffFetchOutcome,
  type FileTree,
  type GapExpandFrom,
  type GapExpandState,
  type DiffGap,
  type HunkIdentity,
  type LoadedDiffCache,
  type LoadedDiffCacheGuards,
  type LoadedDiffCacheResult,
  type LoadedDiff,
  type ReviewThread,
  appendFileDiffPage,
  applyReviewLifecycle,
  buildCodeReviewAbortPayload,
  buildFileTree,
  classifyReviewProbe,
  codeReviewOpenCommand,
  emptyCodeReviewSnapshot,
  emptyReviewLifecycle,
  emptyLoadedDiffCache,
  CODE_REVIEW_MAX_COMMENT_BYTES,
  fileListChangeStats,
  loadedDiffAuthoritativeForSnapshot,
  loadedDiffCachePut,
  loadedDiffCacheTouch,
  findThreadForHunk,
  formatActiveRepliesLabel,
  formatReviewCloseWarning,
  formatReviewRefreshWarning,
  hunkIdentityFor,
  hunkKey,
  hunkKeyFor,

  shouldRenderReviewError,
  initLoadedDiff,
  nextFileDiffLoadState,
  normalizeCodeReviewSnapshot,
  normalizeFileDiffPage,
  diffGapId,
  gapExpandWindow,
  mergeExpandedLines,
  newGapExpandState,
  normalizeFileContextPage,
  remainingDiffGap,
  recordGapExpand,
  CONTEXT_EXPAND_LINES,
  FILE_RENDER_SOFT_CAP,
  overlayReviewLifecycle,
  reviewHasInternalBusy,
  planDiffWindow,
  readStoredCodeReviewThreadWidth,
  shouldEagerFileDiffFetch,
  stepCodeReviewThreadWidth,
  threadIsCancellable,
  treeFileIndexAt,
  treeFilterRows,
  treeKeyboardAction,
  treeToggleCollapse,
  writeStoredCodeReviewThreadWidth,
  aggregateFindingsByDirectory,
  aggregateFindingsByFile,
  reviewFindingsForSnapshot,
  findingRefMatches,
  type OrderedReviewFinding,
  invalidateReviewFindingRef,
  threadLatestUserIsAuto,
  navigateReviewFinding,
  orderReviewFindings,
  type ReviewFindingDirection,
  type ReviewFindingRef,

  CODE_REVIEW_FILES_WIDTH_MAX,
  CODE_REVIEW_FILES_WIDTH_MIN,
  readStoredCodeReviewFilesWidth,
  stepCodeReviewFilesWidth,
  writeStoredCodeReviewFilesWidth,
 } from '../codeReview';
import { normalizeRead, FILE_EXPLORER_HARD_MAX_LINES } from '../fileExplorer';
import { groupModelsByProvider, type ModelProviderGroup } from '../modelSelect';
import { CodeReviewConfirmDialog } from './CodeReviewConfirmDialog';
import { CodeReviewDiffPane } from './CodeReviewDiffPane';
import { CodeReviewFileList } from './CodeReviewFileList';
import { CodeReviewThreadDock } from './CodeReviewThreadDock';
import { CodeReviewTldrPane } from './CodeReviewTldrPane';
import { reviewPreviewKind, type ReviewPreviewInfo } from './CodeReviewDiffPane';
import {
  admitBuiltAttachments,
  aggregateWire,
  attachmentsToImageBlocks,
  classifyImageAttachments,
  formatSkipSummary,
  imageAttachmentAccept,
  mergeIntakeResults,
  placeholderFor,
  readAccepted,
  readAttachmentsInOrder,
  removeSentAttachments,
  utf8ByteLength,
  type AttachSkip,
  type ComposerAttachment,
  type ReadResult,
} from '../attachments';
import { isToastOwnedRpcError } from '../pending';
import { nextId } from '../transcript';
import { useSttRecorder } from '../useSttRecorder';

export interface CodeReviewOpenArgs {
  /** `/commit <ref>` scope: review the pinned commit — parent→commit (a root
   *  commit diffs from the empty tree; a merge from its first parent), pinned
   *  to the resolved hash. Mutually exclusive with from/to/includeWorkingTree. */
  commit?: string;
  from?: string;
  to?: string;
  includeWorkingTree?: true;
  alreadyOpen?: true;
}

interface CodeReviewPanelProps {
  /**
   * sendCommand bound to the App websocket (resolves with RPC data). Carries
   * the App's per-call options: the owner-targeted unmount close must be allowed
   * while a switch is pending (a warm display activation can unmount this panel
   * inside that window, and an ungated send would be refused before it reaches
   * the wire, leaving the review open on the session it owns).
   */
  sendCommand: (
    command: Record<string, unknown>,
    bubbleId?: string,
    opts?: { auxiliary?: boolean; allowDuringSwitch?: boolean },
  ) => Promise<unknown>;
  /**
   * Owning session id captured at open time. Stamped on EVERY code_review_*
   * command (including unmount close) so A→B cleanup cannot target B.
   */
  sessionId: string | null;
  /** Scope captured when `/code-review [...]` or `/commit <ref>` opened the panel. */
  openArgs: CodeReviewOpenArgs;
  /**
   * Monotonically increasing authoritative connection epoch from App. A new
   * value signals a ready reconnect: the panel reconciles once (probe the
   * existing controller; preserve a live same-process one; reopen only when
   * the authoritative RPC reports no review is open). Transport failures of
   * the probe never render inline nor spin — the next epoch retries.
   */
  connectionEpoch: number;
  /** Runtime settings subset the panel needs (STT availability gate). */
  liveSettings: { sttConfigured?: boolean } | null;
  /** The App's single toast surface for every user-action/validation failure. */
  onToast: (message: string, error: boolean) => void;
  onClose: () => void;
}

const NARROW_QUERY = '(max-width: 900px)';

const SPINNER_SLOTS = ['autoReview', 'highlight', 'tldr', 'refresh'] as const;
type SpinnerSlot = (typeof SPINNER_SLOTS)[number];
type SpinnerGate = Record<SpinnerSlot, boolean>;
type MobileTab = 'files' | 'diff' | 'thread';
type ConfirmAction = { kind: 'close' | 'refresh'; draftCount: number };

/**
 * Fullscreen code-review workspace. Owns open/poll/refresh/comment/abort/close
 * against the backend controller; App only mounts it and passes openArgs +
 * the owning sessionId. Session-keyed remount tears state down cleanly.
 * Presentation is delegated to adjacent leaves; this file keeps orchestration.
 */
export function CodeReviewPanel({
  sendCommand,
  sessionId,
  openArgs,
  connectionEpoch,
  liveSettings,
  onToast,
  onClose,
}: CodeReviewPanelProps) {
  const [snapshot, setSnapshot] = useState<CodeReviewSnapshot>(() => emptyCodeReviewSnapshot());
  const [selectedPath, setSelectedPath] = useState<string>('');
  /** Explicit hunk selection only; null = nothing selected (never auto-picks). */
  const [selectedHunkKey, setSelectedHunkKey] = useState<string | null>(null);
  /** Typed finding selection. Cleared when that hunk's findings are replaced. */
  const [selectedFinding, setSelectedFinding] = useState<ReviewFindingRef | null>(null);
  const selectedFindingRef = useRef<ReviewFindingRef | null>(null);
  const reviewFindingsRef = useRef<CodeReviewSnapshot['reviewFindings']>(null);

  /** Per-hunk comment drafts keyed by hunkKey; survive hunk switches. */
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState<'open' | 'refresh' | 'comment' | 'abort' | null>('open');
  const mutatingRef = useRef(false);
  /** Per-hunk image attachments keyed by hunk key; a hunk switch never retargets them. */
  const [hunkAttachments, setHunkAttachments] = useState<Record<string, ComposerAttachment[]>>({});
  const [fileQuery, setFileQuery] = useState('');
  const [isNarrow, setIsNarrow] = useState(() => window.matchMedia(NARROW_QUERY).matches);
  /** Files-rail collapse (the whole tree column, not dir rows). Narrow
   * viewports open collapsed by default so the diff owns the first paint;
   * desktop joins open. The tree NEVER unmounts — collapse is the
   * code-review--tree-collapsed modifier on the section (styles.css owns
   * the track), so expanding restores the rail width by dropping the class. */
  const [treeCollapsed, setTreeCollapsed] = useState(() => window.matchMedia(NARROW_QUERY).matches);
  const [mobileTab, setMobileTab] = useState<MobileTab>('files');
  const [focusMode, setFocusMode] = useState(false);
  const [confirmAction, setConfirmAction] = useState<ConfirmAction | null>(null);
  const [collapsedHunks, setCollapsedHunks] = useState<Set<string>>(() => new Set());
  const [gapStore, setGapStore] = useState<
    Record<
      string,
      {
        snapshotId: string;
        oldLineCount: number;
        newLineCount: number;
        states: Record<string, GapExpandState>;
        lines: Record<string, DiffLine[]>;
      }
    >
  >({});
  const [expandingGapId, setExpandingGapId] = useState<string | null>(null);
  const [hasUserSelectedFile, setHasUserSelectedFile] = useState(false);
  const [fileFocusIndex, setFileFocusIndex] = useState(-1);
  const [diffCache, setDiffCache] = useState<LoadedDiffCache>(emptyLoadedDiffCache);
  const loadedDiffs = diffCache.entries;
  /** Collapsible path tree over snapshot.files (collapse set preserved across polls). */
  const [fileTree, setFileTree] = useState<FileTree>(() => buildFileTree([]));
  /** Per-path visible-line window; snapshotId guards against stale windows. */
  const [diffViews, setDiffViews] = useState<Record<string, { snapshotId: string; window: number }>>({});
  /** Per-path load-more busy/error state (one in-flight load per file). */
  const [loadStates, setLoadStates] = useState<Record<string, { loading: boolean; error: string | null }>>({});
  /** Desktop thread-column width (px); drives --code-review-thread-width. */
  const [threadWidth, setThreadWidth] = useState(() =>
    readStoredCodeReviewThreadWidth(
      typeof window !== 'undefined' ? window.localStorage : null,
    ),
  );
  /** Desktop files-column width (px); drives --code-review-files-width. */
  const [filesWidth, setFilesWidth] = useState(() =>
    readStoredCodeReviewFilesWidth(
      typeof window !== 'undefined' ? window.localStorage : null,
    ),
  );
  /** Auto Review request owner: the snapshot batch state is authoritative for
   *  progress; `pending` spans click → ACK → the first snapshot carrying the
   *  accepted batch id, so the gap stays a live Stop. */
  const [autoReviewButton, setAutoReviewButton] = useState<AutoReviewButtonState>(
    initialAutoReviewButtonState,
  );
  const [spinnerDue, setSpinnerDue] = useState<SpinnerGate>(() => ({
    autoReview: false,
    highlight: false,
    tldr: false,
    refresh: false,
  }));
  /** Explicit language pick (concrete tag); null until the user touches the select. */
  const [autoLanguagePicked, setAutoLanguagePicked] = useState<string | null>(null);
  /** Highlight request owner: pending spans click → ACK → the first snapshot
   *  carrying the accepted generation; the snapshot state stays authoritative
   *  for running/done/failed display once the owner is released. */
  const [highlightOwner, setHighlightOwner] = useState<HighlightOwnerState>(
    initialHighlightOwnerState,
  );
  const [tldrOwner, setTldrOwner] = useState<HighlightOwnerState>(initialHighlightOwnerState);
  const [tldrOpen, setTldrOpen] = useState(false);
  /** Accepted-comment follow event for the dock's scroll pin: the full hunk
   *  key of the accepted submit + a monotonic seq. */
  const [acceptedFollow, setAcceptedFollow] = useState<CodeReviewFollowEvent | null>(null);
  const followSeqRef = useRef(0);
  /** Review model switch state machine + the session's model catalog rows. */
  const [reviewModel, setReviewModel] = useState<ReviewModelState>(initialReviewModelState);
  const [models, setModels] = useState<Array<{ id: string; name: string; provider: string }>>([]);
  /** Source|Preview surface for the selected file (per session, never stored). */
  const [previewSurface, setPreviewSurface] = useState(false);
  const [previewStates, setPreviewStates] = useState<
    Record<string, { loading: boolean; error: string | null; text: string | null; truncated: boolean }>
  >({});
  /** Snapshot id the preview cache was built against; content may change on refresh. */
  const previewSnapshotIdRef = useRef('');
  /** Per-path preview read seq: bumping discards in-flight file_explorer_read calls. */
  const previewSeqRef = useRef<Record<string, number>>({});
  const aliveRef = useRef(true);
  // Capture the owning session at mount so unmount close still targets A even
  // if the parent re-rendered with a newer sessionIdRef before cleanup ran.
  const owningSessionIdRef = useRef(sessionId);
  const panelRef = useRef<HTMLElement | null>(null);
  const rowButtonRefs = useRef<Array<HTMLButtonElement | null>>([]);
  const previousFocusRef = useRef<HTMLElement | null>(null);
  const settledOnceRef = useRef(false);
  const snapshotRequestSeqRef = useRef(0);
  const batchOwnerLiveRef = useRef(false);
  /** Request seq per path: bumping discards in-flight code_review_file_diff calls. */
  const loadSeqRef = useRef<Record<string, number>>({});
  /** Per-path selection epoch: bump on leave/re-enter and on snapshot
   *  re-init; eviction bumps ONLY the evicted path's epoch, so an unrelated
   *  selected path's in-flight fetch keeps its generation. */
  const selectionEpochRef = useRef<Record<string, number>>({});
  const fetchPromisesRef = useRef<Record<string, { seq: number; epoch: number; promise: Promise<LoadedDiff | null> }>>({});
  /** Per-path transport-stall flags: a code_review_file_diff transport failure
   *  suppresses the inline error and stalls the eager loader (no spin) until a
   *  new connection epoch clears the flag and retries. */
  const fetchStalledRef = useRef<Record<string, boolean>>({});
  const diffCacheRef = useRef<LoadedDiffCache>(emptyLoadedDiffCache());
  const lastEpochRef = useRef(connectionEpoch);
  const openReviewRef = useRef<(forceOpen?: boolean) => void>(() => {});
  const reopenInFlightRef = useRef(false);
  /** Generation-scoped reply owner; snapshot polls cannot resurrect a terminal reply. */
  const lifecycleRef = useRef(emptyReviewLifecycle());
  /** Last-reported async-error identity per field (exactly one toast per
   *  snapshot/batch/highlight identity). */
  const errorReportRef = useRef<ReviewErrorReport>(initialReviewErrorReport());
  /** Projection staleness floor: the newest (reviewId, revision) applied to
   *  panel state; an older-revision snapshot response is dropped. */
  const lastAppliedRevisionRef = useRef<{ reviewId: string | null; revision: number }>({
    reviewId: null,
    revision: 0,
  });
  /** Last-reported background (poll/context/probe) failure key; a key is
   *  cleared by the next successful snapshot so a recurrence re-toasts. */
  const backgroundErrorRef = useRef<string | null>(null);
  const selectedPathRef = useRef('');
  const selectedHunkKeyRef = useRef<string | null>(null);
  const sttDraftHunkKeyRef = useRef<string | null>(null);
  const hunkAttachmentsRef = useRef<Record<string, ComposerAttachment[]>>({});
  /** Drag start snapshot for the desktop thread-column resizer. */
  const resizeDragRef = useRef<{ startX: number; startWidth: number } | null>(null);
  /** Drag start snapshot for the desktop files-column resizer. */
  const filesResizeDragRef = useRef<{ startX: number; startWidth: number } | null>(null);
  const gapStoreRef = useRef(gapStore);
  const spinnerTimersRef = useRef<Partial<Record<SpinnerSlot, number>>>({});
  const expandChainRef = useRef(Promise.resolve());
  const lineCountSeqRef = useRef<Record<string, number>>({});
  const diffCacheGuards = useCallback((): LoadedDiffCacheGuards => ({
    currentPath: selectedPathRef.current,
    inFlightPaths: Object.keys(fetchPromisesRef.current),
  }), []);
  const handleEvictedDiffs = useCallback((paths: string[]) => {
    if (paths.length === 0) return;
    const evicted = new Set(paths);
    for (const path of paths) {
      loadSeqRef.current[path] = (loadSeqRef.current[path] ?? 0) + 1;
      selectionEpochRef.current[path] = (selectionEpochRef.current[path] ?? 0) + 1;
      delete fetchPromisesRef.current[path];
      delete fetchStalledRef.current[path];
    }
    const dropEvicted = <T,>(prev: Record<string, T>): Record<string, T> => {
      const keys = Object.keys(prev);
      if (keys.every((key) => !evicted.has(key))) return prev;
      const next: Record<string, T> = {};
      for (const key of keys) if (!evicted.has(key)) next[key] = prev[key];
      return next;
    };
    setDiffViews(dropEvicted);
    setLoadStates(dropEvicted);
  }, []);

  const applyDiffCacheResult = useCallback((result: LoadedDiffCacheResult) => {
    diffCacheRef.current = result.cache;
    setDiffCache(result.cache);
    handleEvictedDiffs(result.evicted);
  }, [handleEvictedDiffs]);
  /** Live composer textarea (uncontrolled); drafts commit on blur/hunk switch. */

  const composerRef = useRef<HTMLTextAreaElement | null>(null);
  /** rAF-coalesced auto-grow/collapse controller for the composer (see ../autoResize). */
  const composerAutoResizeRef = useRef<AutoResizeController<HTMLTextAreaElement> | null>(null);

  /** Stamp every code_review_* payload with the owning sessionId. */
  const reviewCommand = useCallback(
    (command: Record<string, unknown>): Record<string, unknown> => {
      const sid = owningSessionIdRef.current;
      if (sid) return { ...command, sessionId: sid };
      return command;
    },
    [],
  );

  /**
   * One App toast per identical background failure: transport-family
   * rejections stay with the shared outage toast, App-owned toasts never
   * duplicate, and the caller's stale-identity guards run first. The latch
   * key is `${label}\0${key}\0${message}` so a changed cause re-toasts.
   */
  const reportBackgroundFailure = useCallback(
    (key: string, label: string, err: unknown) => {
      if (!shouldRenderReviewError(err) || isToastOwnedRpcError(err)) return;
      const message = err instanceof Error ? err.message : String(err);
      const latchKey = `${label}\0${key}\0${message}`;
      if (backgroundErrorRef.current === latchKey) return;
      backgroundErrorRef.current = latchKey;
      onToast(`${label}: ${message}`, true);
    },
    [onToast],
  );

  // Model switch: one compact select in the composer row over the SAME
  // catalog the main composer loads (get_available_models). The reducer keeps
  // the applied value until an ack lands, so a typed reject (unknown model)
  // reverts the select; transport failures cancel silently (shared toast
  // owns them) and the composer stays usable.
  const loadModels = useCallback(() => {
    const sid = owningSessionIdRef.current;
    if (!sid) return;
    // The catalog fetch is part of the same owner-targeted bootstrap and has no
    // settle retry of its own: allow it during the panel's restore window.
    sendCommand({ type: 'get_available_models', sessionId: sid }, undefined, { allowDuringSwitch: true })
      .then((data) => {
        if (!aliveRef.current) return;
        setModels(normalizeAvailableModels(data));
      })
      .catch((err: unknown) => {
        // Transport failures stay with the shared outage toast (degraded
        // catalog, select keeps its placeholder — same as the main
        // composer); a business catalog failure toasts with context.
        reportBackgroundFailure('model-catalog', 'Review model catalog failed', err);
      });
  }, [reportBackgroundFailure, sendCommand]);

  const onReviewModelChange = useCallback(
    (value: string) => {
      setReviewModel((prev) => applyReviewModelEvent(prev, { type: 'pick', value }));
      if (!value) return;
      sendCommand(reviewCommand(buildCodeReviewSetModelPayload(value)))
        .then((data) => {
          if (!aliveRef.current) return;
          setReviewModel((prev) => {
            if (prev.pendingValue !== value) return prev;
            return applyReviewModelEvent(prev, { type: 'ack', label: parseCodeReviewSetModelAck(data) });
          });
        })
        .catch((err: unknown) => {
          if (!aliveRef.current) return;
          const message = err instanceof Error ? err.message : String(err);
          setReviewModel((prev) => {
            if (prev.pendingValue !== value) return prev;
            return applyReviewModelEvent(prev, { type: 'reject', message });
          });
          if (shouldRenderReviewError(err) && !isToastOwnedRpcError(err)) {
            onToast(message, true);
          }
        });
    },
    [onToast, reviewCommand, sendCommand],
  );

  const modelGroups: ModelProviderGroup[] = useMemo(() => groupModelsByProvider(models), [models]);
  const snapshotModelValue = reviewModelOptionValue(
    snapshot.reviewModelProvider,
    snapshot.reviewModelId,
  );
  const catalogHasSnapshotModel = snapshotModelValue !== ''
    && modelGroups.some((group) => group.models.some((option) => option.value === snapshotModelValue));
  /** The single review-model select (panel head) shows the applied value,
   *  falling back to the snapshot's fork model only while the catalog lacks
   *  it; '' = unknown and renders the placeholder path. */
  const reviewModelValue = reviewModelSelectValue(reviewModel)
    || (catalogHasSnapshotModel ? snapshotModelValue : '');

  const togglePreviewSurface = useCallback((surface: 'source' | 'preview') => {
    setPreviewSurface(surface === 'preview');
  }, []);
  /**
   * Normalize + project one code_review_* response into panel state. The
   * batch/highlight identity fields feed the typed request owners (stale
   * idle snapshots cannot clear an accepted owner), and the async error
   * fields own exactly one App toast per snapshot/batch/highlight identity.
   */
  const applyData = useCallback((data: unknown) => {
    const next = normalizeCodeReviewSnapshot(data);
    if (
      next.error === 'Invalid review findings'
      || next.error === 'Invalid code-review snapshot response'
      || next.error === 'Invalid TLDR fields'
    ) {
      onToast(`Code review: ${next.error}`, true);
      return;
    }
    // Out-of-order staleness gate: a snapshot response with an older
    // controller revision (a delayed poll that raced a start/stop) must
    // never regress the projected state — it cannot clear a live Stop or an
    // async error surface. A reviewId change (restart/reopen) resets the
    // floor.
    if (
      lastAppliedRevisionRef.current.reviewId === next.reviewId
      && next.revision < lastAppliedRevisionRef.current.revision
    ) {
      return;
    }
    lastAppliedRevisionRef.current = { reviewId: next.reviewId, revision: next.revision };
    setAutoReviewButton((prev) =>
      applyAutoReviewButtonEvent(prev, {
        type: 'snapshot',
        state: next.autoReviewState,
        counts: next.autoReviewCounts,
        batchId: next.autoReviewBatchId,
        reviewId: next.reviewId,
      }),
    );
    setHighlightOwner((prev) =>
      applyHighlightOwnerEvent(prev, {
        type: 'snapshot',
        generation: next.highlightGeneration,
        state: next.highlightState,
        reviewId: next.reviewId,
      }),
    );
    setTldrOwner((prev) =>
      applyHighlightOwnerEvent(prev, {
        type: 'snapshot',
        generation: next.tldrGeneration,
        state: next.tldrState,
        reviewId: next.reviewId,
      }),
    );
    const report = reviewErrorNotices(next, errorReportRef.current);
    if (report.notices.length > 0) {
      errorReportRef.current = report.reported;
      backgroundErrorRef.current = null;
      for (const notice of report.notices) {
        if (!notice.cancelled) onToast(notice.message, true);
      }
    }
    if (previewSnapshotIdRef.current !== next.snapshotId) {
      previewSnapshotIdRef.current = next.snapshotId;
      previewSeqRef.current = {};
      setPreviewStates({});
      setPreviewSurface(false);
    }
    const prevLifecycle = lifecycleRef.current;
    const nextLifecycle = applyReviewLifecycle(prevLifecycle, {
      type: 'snapshot',
      snapshot: next,
    });
    const overlaid = overlayReviewLifecycle(next, nextLifecycle);
    if (nextLifecycle !== prevLifecycle) lifecycleRef.current = nextLifecycle;
    const nextFinding = overlaid.snapshotId !== snapshot.snapshotId
      ? null
      : invalidateReviewFindingRef(
          reviewFindingsRef.current,
          overlaid.reviewFindings,
          selectedFindingRef.current,
        );
    reviewFindingsRef.current = overlaid.reviewFindings;
    selectedFindingRef.current = nextFinding;
    setSelectedFinding(nextFinding);
    setSnapshot(overlaid);

    setGapStore((prev) => {
      const keys = Object.keys(prev);
      if (keys.every((path) => prev[path].snapshotId === overlaid.snapshotId)) return prev;
      gapStoreRef.current = {};
      return {};
    });
    return overlaid;
  }, [onToast, snapshot.snapshotId]);


  const fetchSnapshot = useCallback(() => {
    const requestSeq = snapshotRequestSeqRef.current;
    // Same owner-targeted during-switch allowance as the bootstrap: a poll that
    // lands inside the panel's own restore window must not be refused (it would
    // latch a background failure and never refresh the restored panel).
    return sendCommand(reviewCommand({ type: 'code_review_snapshot' }), undefined, { allowDuringSwitch: true })
      .then((data) => {
        if (!aliveRef.current || requestSeq !== snapshotRequestSeqRef.current) return;
        applyData(data);
        backgroundErrorRef.current = null;
      })
      .catch((err: unknown) => {
        if (!aliveRef.current || requestSeq !== snapshotRequestSeqRef.current) return;
        if (isNoOpenCodeReviewError(err)) {
          if (!lastAppliedRevisionRef.current.reviewId) return;
          openReviewRef.current(true);
          return;
        }
        reportBackgroundFailure('snapshot-poll', 'Code review snapshot failed', err);
      });
  }, [applyData, reportBackgroundFailure, reviewCommand, sendCommand]);


  const closeReviewById = useCallback((reviewId: string) => {
    const closeCmd: Record<string, unknown> = { type: 'code_review_close', reviewId };
    const sid = owningSessionIdRef.current;
    if (sid) closeCmd.sessionId = sid;
    // Owner-targeted cleanup of THIS panel's review. A warm display activation
    // can unmount the panel while its switch is still pending (App clears
    // activePanel on the display transition), and the pending-switch gate
    // rejects an ungated send — which would leave the review open on the
    // session it owns. The explicit owner sessionId keeps the during-switch
    // allowance on that session, exactly like the App's probe cleanup.
    void sendCommand(closeCmd, undefined, { allowDuringSwitch: true }).catch((err: unknown) => {
      if (isNoOpenCodeReviewError(err)) return;
      if (!isToastOwnedRpcError(err)) {
        onToast(err instanceof Error ? err.message : String(err), true);
      }
    });
  }, [onToast, sendCommand]);

  const openReview = useCallback((forceOpen = false) => {
    if (forceOpen) {
      if (reopenInFlightRef.current) return;
      reopenInFlightRef.current = true;
    }
    if (mutatingRef.current) {
      if (forceOpen) reopenInFlightRef.current = false;
      return;
    }
    // Operand validation happens BEFORE the busy state: a rejected mixed
    // scope (commit conflated with from/to/includeWorkingTree) toasts once
    // and leaves the panel idle — no busy flag, no lifecycle reset, no wire
    // traffic. The explicit result API never throws synchronously.
    const validatedOpen = codeReviewOpenCommand(openArgs);
    if (!validatedOpen.ok) {
      if (forceOpen) reopenInFlightRef.current = false;
      // The useState('open') initial placeholder must not stick on a rejected
      // scope: clear it so the panel settles idle with the toast, never a
      // permanent entry loader.
      setBusy(null);
      onToast(validatedOpen.error, true);
      return;
    }
    const snapshotAlreadyOpen = !forceOpen && openArgs.alreadyOpen;
    mutatingRef.current = true;
    const requestSeq = ++snapshotRequestSeqRef.current;
    const keepLastGood = lastAppliedRevisionRef.current.reviewId != null;
    if (!keepLastGood) {
      setBusy('open');
      lifecycleRef.current = emptyReviewLifecycle();
    }
    const command = snapshotAlreadyOpen
      ? { type: 'code_review_snapshot' }
      : validatedOpen.command;
    // Owner-targeted bootstrap of THIS panel's review. A restored view slot
    // mounts the panel inside a still-pending switch (a warm display activation
    // restores it before the authoritative cut lands), and the pending gate
    // would refuse an ungated send — leaving a restored panel that never opens
    // its own review (the poll only reopens when a last-good review exists). The
    // explicit owner sessionId keeps the during-switch allowance on the session
    // this panel owns, exactly like the unmount close below.
    const applyOpenResult = (data: unknown) => {
      if (requestSeq !== snapshotRequestSeqRef.current) return;
      if (!aliveRef.current) {
        const reviewId = normalizeCodeReviewSnapshot(data).reviewId;
        if (reviewId) closeReviewById(reviewId);
        return;
      }
      applyData(data);
    };
    const reportOpenFailure = (err: unknown) => {
      if (!aliveRef.current || requestSeq !== snapshotRequestSeqRef.current) return;
      if (!keepLastGood) lifecycleRef.current = emptyReviewLifecycle();
      if (!isToastOwnedRpcError(err)) {
        onToast(err instanceof Error ? err.message : String(err), true);
      }
    };
    sendCommand(reviewCommand(command), undefined, { allowDuringSwitch: true })
      .then(applyOpenResult)
      .catch((err: unknown) => {
        if (!aliveRef.current || requestSeq !== snapshotRequestSeqRef.current) return;
        if (isNoOpenCodeReviewError(err) && snapshotAlreadyOpen) {
          const reopened = codeReviewOpenCommand(openArgs);
          if (!reopened.ok) {
            reportOpenFailure(new Error(reopened.error));
            return;
          }
          return sendCommand(
            reviewCommand(reopened.command),
            undefined,
            { allowDuringSwitch: true },
          )
            .then(applyOpenResult)
            .catch(reportOpenFailure);
        }
        if (isNoOpenCodeReviewError(err)) return;
        reportOpenFailure(err);
      })
      .finally(() => {
        mutatingRef.current = false;
        if (forceOpen) reopenInFlightRef.current = false;
        if (aliveRef.current && requestSeq === snapshotRequestSeqRef.current) setBusy(null);
      });
  }, [applyData, closeReviewById, onToast, openArgs.commit, openArgs.from, openArgs.to, openArgs.includeWorkingTree, openArgs.alreadyOpen, reviewCommand, sendCommand]);
  openReviewRef.current = openReview;

  useEffect(() => {
    aliveRef.current = true;
    previousFocusRef.current =
      document.activeElement instanceof HTMLElement ? document.activeElement : null;
    openReview();
    loadModels();
    return () => {
      aliveRef.current = false;
      for (const slot of SPINNER_SLOTS) {
        const timer = spinnerTimersRef.current[slot];
        if (timer != null) window.clearTimeout(timer);
      }
      spinnerTimersRef.current = {};
      const reviewId = lastAppliedRevisionRef.current.reviewId;
      if (reviewId) closeReviewById(reviewId);
      const previous = previousFocusRef.current;
      if (previous && previous.isConnected) {
        previous.focus({ preventScroll: true });
      }
    };
    // Mount/unmount only: openReview/loadModels/onToast identity must not
    // close a live controller.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Poll while mounted. Snapshot polls are PANEL-owned (App never toasts
  // them): a successful snapshot resets the failure latch; a stale request
  // (superseded by a newer request seq) and exact transport failures are
  // suppressed — transport outages stay with the shared App toast and the
  // last-good snapshot is kept; every business failure toasts with context
  // once per identical failure stretch. Polls pause while an open or refresh
  // is already busy so polls cannot accumulate against an in-flight snapshot
  // mutation (those bump the request seq and own the next cutover). A typed
  // no-open poll with last-good reopens once and never toasts ReviewNotOpen;
  // a reopen already in flight is ignored so poll cannot loop.
  // Idle cadence is CODE_REVIEW_SNAPSHOT_POLL_MS; while Auto Review / Highlight
  // owns the run, ACK fetches immediately and the loop uses BATCH_POLL_MS.
  const batchOwnerLive = codeReviewBatchOwnerLive({
    autoReviewPending: autoReviewButton.pending,
    highlightPending: highlightOwner.pending,
    tldrPending: tldrOwner.pending,
    autoReviewState: snapshot.autoReviewState,
    highlightState: snapshot.highlightState,
    tldrState: snapshot.tldrState,
  });
  batchOwnerLiveRef.current = batchOwnerLive;
  useEffect(() => {
    if (busy === 'open' || busy === 'refresh') return;
    // Invalid scope (codeReviewOpenCommand reject) polls nothing — fail-closed.
    if (!codeReviewOpenCommand(openArgs).ok) return;
    let cancelled = false;
    let timer: number | null = null;
    const tick = () => {
      if (cancelled) return;
      void fetchSnapshot();
      timer = window.setTimeout(tick, snapshotPollDelayMs(batchOwnerLiveRef.current));
    };
    tick();
    return () => {
      cancelled = true;
      if (timer != null) window.clearTimeout(timer);
    };
  }, [applyData, reportBackgroundFailure, busy, fetchSnapshot, batchOwnerLive, openArgs]);


  // Re-fetch the model catalog after an authoritative reconnect (a restart
  // may serve a different catalog; a transport failure stays silent here —
  // the select remains on the placeholder and the epoch retries).
  useEffect(() => {
    if (lastEpochRef.current === connectionEpoch) return;
    loadModels();
  }, [connectionEpoch, loadModels]);
  useEffect(() => {
    const mq = window.matchMedia(NARROW_QUERY);
    const onChange = (event: MediaQueryListEvent) => setIsNarrow(event.matches);
    mq.addEventListener('change', onChange);
    return () => mq.removeEventListener('change', onChange);
  }, []);
  useEffect(() => {
    const panel = panelRef.current;
    if (!panel) return;
    const rootStyle = document.documentElement.style;
    const apply = () => syncCodeReviewToastAnchor(panel, rootStyle, window.innerHeight);
    const observer = new ResizeObserver(apply);
    const head = panel.querySelector('.code-review__head');
    const tabBar = panel.querySelector('.code-review__tab-bar');
    const summaryHead = panel.querySelector('.code-review-tldr__head');
    const composer = panel.querySelector('.code-review__composer');
    const commentInput = panel.querySelector('.code-review__comment-input');
    if (head) observer.observe(head);
    if (tabBar) observer.observe(tabBar);
    if (summaryHead) observer.observe(summaryHead);
    if (composer) observer.observe(composer);
    if (commentInput) observer.observe(commentInput);
    window.addEventListener('resize', apply);
    apply();
    return () => {
      observer.disconnect();
      window.removeEventListener('resize', apply);
      clearCodeReviewToastAnchor(rootStyle);
    };
  }, [isNarrow, mobileTab, selectedHunkKey, tldrOpen]);

  // Filtered tree rows (a filter keeps matched files + ancestor dirs, all
  // forced expanded; empty query = the full visible tree).
  const rows = useMemo(() => treeFilterRows(fileTree, fileQuery), [fileTree, fileQuery]);

  const selectedFile: DiffFile | null = useMemo(
    () => snapshot.files.find((file) => file.path === selectedPath) ?? null,
    [snapshot.files, selectedPath],
  );

  // Per-file loaded diff + visible window for the selected file. The loaded
  // diff persists across polls (same snapshot id); a refresh (new id) resets
  // both, and in-flight page requests for the path are invalidated.
  const loadedDiff: LoadedDiff | null = useMemo(
    () => (selectedFile ? (loadedDiffs[selectedFile.path] ?? null) : null),
    [loadedDiffs, selectedFile],
  );

  // No fallback to hunks[0]: an invalid or empty selection stays empty so the
  // user must explicitly pick the hunk they want to comment on. The effective
  // hunk source is the merged per-file hunks once loaded (they carry the
  // exact backend identities the comment RPC resolves), the snapshot hunks
  // before that.
  const selectedHunk: DiffHunk | null = useMemo(() => {
    if (!selectedFile || !selectedHunkKey) return null;
    const source =
      loadedDiff && loadedDiff.hunks.length > 0 ? loadedDiff.hunks : selectedFile.hunks;
    return (
      source.find((hunk) => hunkKeyFor(selectedFile, hunk) === selectedHunkKey) ?? null
    );
  }, [loadedDiff, selectedFile, selectedHunkKey]);

  const selectedIdentity: HunkIdentity | null = useMemo(() => {
    if (!selectedFile || !selectedHunk) return null;
    return hunkIdentityFor(snapshot.snapshotId, selectedFile, selectedHunk);
  }, [selectedFile, selectedHunk, snapshot.snapshotId]);

  const selectedThread: ReviewThread | undefined = useMemo(() => {
    if (!selectedIdentity) return undefined;
    return findThreadForHunk(snapshot.threads, selectedIdentity);
  }, [selectedIdentity, snapshot.threads]);

  const selectedDraft = selectedHunkKey ? (drafts[selectedHunkKey] ?? '') : '';
  const selectedAttachments = selectedHunkKey ? (hunkAttachments[selectedHunkKey] ?? []) : [];
  // Visible live header/dot follows activeCount. Abort follows isCancellable.
  // Close/refresh warnings follow internalBusy (owned/queued work), never live
  // status. Composer stays editable while internalBusy is true.
  const hasInternalBusy = reviewHasInternalBusy(snapshot);
  const selectedIsCancellable = threadIsCancellable(selectedThread);
  const activeRepliesLabel = formatActiveRepliesLabel(snapshot.activeCount);

  const selectedView = selectedPath ? diffViews[selectedPath] : undefined;
  const windowLimit =
    selectedView && selectedView.snapshotId === snapshot.snapshotId
      ? selectedView.window
      : 0;
  const loadState = selectedPath
    ? (loadStates[selectedPath] ?? { loading: false, error: null })
    : { loading: false, error: null };

  const filesForList = useMemo(() => {
    return snapshot.files.map((file) => {
      const loaded = loadedDiffs[file.path];
      const stats = fileListChangeStats(file, loaded, snapshot.snapshotId);
      if (loadedDiffAuthoritativeForSnapshot(loaded, snapshot.snapshotId)) {
        return { ...file, insertions: stats.insertions, deletions: stats.deletions, hunks: [] };
      }
      if (stats.insertions === file.insertions && stats.deletions === file.deletions) return file;
      return { ...file, insertions: stats.insertions, deletions: stats.deletions };
    });
  }, [loadedDiffs, snapshot.files, snapshot.snapshotId]);

  // Rebuild the tree when the snapshot's file set changes, preserving the
  // user's collapse state for directories that still exist.
  useEffect(() => {
    setFileTree((prev) => {
      const next = buildFileTree(filesForList);
      if (prev.collapsed.size === 0) return next;
      const valid = new Set(next.nodes.map((n) => n.id));
      const keep = new Set<string>();
      for (const id of prev.collapsed) {
        if (valid.has(id)) keep.add(id);
      }
      next.collapsed = keep;
      return next;
    });
  }, [filesForList]);

  // Keep the selected-path/hunk refs in sync for async stale guards.
  useEffect(() => {
    selectedPathRef.current = selectedPath;
  }, [selectedPath]);
  useEffect(() => {
    selectedHunkKeyRef.current = selectedHunkKey;
  }, [selectedHunkKey]);
  const admittedFindings = useMemo(
    () => reviewFindingsForSnapshot(snapshot.snapshotId, snapshot.files, snapshot.reviewFindings),
    [snapshot.files, snapshot.reviewFindings, snapshot.snapshotId],
  );
  const autoByFile = useMemo(
    () => aggregateFindingsByFile(admittedFindings),
    [admittedFindings],
  );
  const autoByDir = useMemo(
    () => aggregateFindingsByDirectory(admittedFindings),
    [admittedFindings],
  );


  // Initialize (or re-initialize on refresh) the selected file's loaded diff
  // and its visible window. A stale loaded diff is replaced atomically with
  // the fresh snapshot's lines; the window restarts at the soft cap.
  useEffect(() => {
    if (!selectedPath) return;
    const file = snapshot.files.find((f) => f.path === selectedPath);
    if (!file) return;
    const current = diffCacheRef.current.entries[selectedPath];
    if (current && current.snapshotId === snapshot.snapshotId && current.path === selectedPath) {
      applyDiffCacheResult(
        loadedDiffCacheTouch(diffCacheRef.current, selectedPath, diffCacheGuards()),
      );
      return;
    }
    const next = initLoadedDiff(snapshot.snapshotId, file);
    applyDiffCacheResult(loadedDiffCachePut(diffCacheRef.current, next, diffCacheGuards()));
    setDiffViews((views) => ({
      ...views,
      [selectedPath]: {
        snapshotId: snapshot.snapshotId,
        window: Math.min(FILE_RENDER_SOFT_CAP, next.lines.length),
      },
    }));
    setLoadStates((states) => ({ ...states, [selectedPath]: { loading: false, error: null } }));
    // A fresh snapshot (refresh) resets any transport stall for this path so
    // the eager loader may fetch the new snapshot's pages.
    delete fetchStalledRef.current[selectedPath];
    loadSeqRef.current[selectedPath] = (loadSeqRef.current[selectedPath] ?? 0) + 1;
    selectionEpochRef.current[selectedPath] = (selectionEpochRef.current[selectedPath] ?? 0) + 1;
  }, [
    applyDiffCacheResult,
    diffCacheGuards,
    selectedPath,
    snapshot.files,
    snapshot.snapshotId,
  ]);

  useEffect(() => {
    const path = selectedPath;
    const snapshotId = snapshot.snapshotId;
    const file = snapshot.files.find((entry) => entry.path === path);
    if (!path || !snapshotId || !file || file.binary) return;
    const existing = gapStoreRef.current[path];
    if (existing && existing.snapshotId === snapshotId) return;
    const seq = (lineCountSeqRef.current[path] ?? 0) + 1;
    lineCountSeqRef.current[path] = seq;
    void (async () => {
      try {
        const data = await sendCommand(
          reviewCommand({
            type: 'code_review_file_context',
            snapshotId,
            path,
            oldStart: 0,
            newStart: 0,
            count: 0,
          }),
        );
        if (!aliveRef.current || lineCountSeqRef.current[path] !== seq) return;
        const page = normalizeFileContextPage(data);
        if (!page || page.snapshotId !== snapshotId || page.path !== path) return;
        setGapStore(() => {
          const current = gapStoreRef.current[path];
          const next = current && current.snapshotId === snapshotId
            ? {
                ...gapStoreRef.current,
                [path]: {
                  ...current,
                  oldLineCount: page.oldLineCount,
                  newLineCount: page.newLineCount,
                },
              }
            : {
                ...gapStoreRef.current,
                [path]: {
                  snapshotId,
                  oldLineCount: page.oldLineCount,
                  newLineCount: page.newLineCount,
                  states: {},
                  lines: {},
                },
              };
          gapStoreRef.current = next;
          return next;
        });
      } catch (err: unknown) {
        // Suppressed only for transport failures (shared outage toast) — the
        // last-good gap store is kept; every business failure toasts with
        // context once per path+snapshot identity.
        if (!aliveRef.current || lineCountSeqRef.current[path] !== seq) return;
        reportBackgroundFailure(`context-probe\0${path}\0${snapshotId}`, 'Code review context probe failed', err);
      }
    })();
  }, [reportBackgroundFailure, reviewCommand, selectedPath, sendCommand, snapshot.files, snapshot.snapshotId]);

  // Desktop: reveal the first file's diff once the open settles and the user
  // has not made their own choice. Mobile stays on the Files tab with no
  // selection; hunks are never auto-selected anywhere.
  useEffect(() => {
    if (busy !== null || isNarrow || hasUserSelectedFile) return;
    if (snapshot.files.length === 0) return;
    setSelectedPath((prev) =>
      prev && snapshot.files.some((file) => file.path === prev) ? prev : snapshot.files[0].path,
    );
  }, [busy, hasUserSelectedFile, isNarrow, snapshot.files]);

  // Move initial focus into the workspace once the first snapshot settles.
  useEffect(() => {
    if (busy !== null || settledOnceRef.current) return;
    settledOnceRef.current = true;
    if (snapshot.files.length === 0) {
      panelRef.current?.querySelector<HTMLElement>('button.code-review__close')?.focus();
      return;
    }
    const index = rows.findIndex((r) => fileTree.nodes[r.nodeIndex]?.path === selectedFile?.path);
    const button = rowButtonRefs.current[index >= 0 ? index : 0];
    button?.focus();
  }, [busy, fileTree, rows, selectedFile, snapshot.files.length]);


  // Keep the roving tree-row focus index in range of the visible rows.
  useEffect(() => {
    const index = rows.findIndex((r) => fileTree.nodes[r.nodeIndex]?.path === selectedPath);
    setFileFocusIndex(index >= 0 ? index : 0);
  }, [fileTree, rows, selectedPath]);

  // Move focus into the inline confirm dialog when it opens.
  useEffect(() => {
    if (!confirmAction) return;
    const primary = panelRef.current?.querySelector<HTMLElement>(
      '.code-review__confirm-actions button',
    );
    primary?.focus();
  }, [confirmAction]);

  const refresh = useCallback(() => {
    if (mutatingRef.current) return;
    mutatingRef.current = true;
    const requestSeq = ++snapshotRequestSeqRef.current;
    setBusy('refresh');
    sendCommand(reviewCommand({ type: 'code_review_refresh' }))
      .then((data) => {
        if (!aliveRef.current || requestSeq !== snapshotRequestSeqRef.current) return;
        applyData(data);
      })
      .catch((err: unknown) => {
        if (!aliveRef.current || requestSeq !== snapshotRequestSeqRef.current) return;
        if (!isToastOwnedRpcError(err)) {
          onToast(err instanceof Error ? err.message : String(err), true);
        }
      })
      .finally(() => {
        mutatingRef.current = false;
        if (aliveRef.current && requestSeq === snapshotRequestSeqRef.current) setBusy(null);
      });
  }, [applyData, onToast, reviewCommand, sendCommand]);

  const abort = useCallback(() => {
    if (!selectedIdentity || !selectedIsCancellable) return;
    // Capture identity at click time so a later selection change cannot
    // apply a stale abort response against a different hunk. Payload is the
    // exact HunkIdentity (path/ranges/hash/snapshotId); session is stamped
    // by reviewCommand.
    const identity = selectedIdentity;
    if (mutatingRef.current) return;
    mutatingRef.current = true;
    const requestSeq = ++snapshotRequestSeqRef.current;
    setBusy('abort');
    sendCommand(reviewCommand(buildCodeReviewAbortPayload(identity)))
      .then((data) => {
        if (!aliveRef.current || requestSeq !== snapshotRequestSeqRef.current) return;
        applyData(data);
      })
      .catch((err: unknown) => {
        if (!aliveRef.current || requestSeq !== snapshotRequestSeqRef.current) return;
        if (!isToastOwnedRpcError(err)) {
          onToast(err instanceof Error ? err.message : String(err), true);
        }
      })
      .finally(() => {
        mutatingRef.current = false;
        if (aliveRef.current && requestSeq === snapshotRequestSeqRef.current) setBusy(null);
      });
  }, [applyData, onToast, reviewCommand, selectedIdentity, selectedIsCancellable, sendCommand]);

  // Auto Review: submits the code_review_auto_review batch (the backend walks
  // the open snapshot's triaged hunks through the existing per-hunk comment
  // pipeline). The accepted ACK is strictly `{ok:true,state:'triaging',
  // batchId}` — the batchId becomes this action's request owner: the button
  // stays busy/Stop (pending) until the first snapshot carrying an
  // autoReviewBatchId >= the accepted id arrives, so Esc and a second click
  // during the gap issue typed code_review_stop and never close or
  // duplicate-start. Stale identity snapshots cannot clear the owner. EVERY
  // reject is non-destructive and clears only this owner: typed rejects, a
  // malformed ACK, and transport failures leave the workspace untouched and
  // the button immediately actionable again; transport failures stay
  // App-owned (shared outage toast), business rejects toast here. The
  // explicit non-default language pick rides `language`.
  const runAutoReview = useCallback(() => {
    if (autoReviewBusy(autoReviewButton)) return;
    setAutoReviewButton((prev) =>
      applyAutoReviewButtonEvent(prev, { type: 'click', reviewId: snapshot.reviewId || null }),
    );
    sendCommand(
      reviewCommand(buildCodeReviewAutoReviewPayload(autoReviewLanguagePayload(autoLanguagePicked))),
    )
      .then((data) => {
        if (!aliveRef.current) return;
        const ack = parseCodeReviewAutoReviewAck(data);
        if (!ack.ok) {
          setAutoReviewButton((prev) => applyAutoReviewButtonEvent(prev, { type: 'reject' }));
          onToast(ack.error, true);
          return;
        }
        setAutoReviewButton((prev) =>
          applyAutoReviewButtonEvent(prev, { type: 'ack', batchId: ack.batchId }),
        );
        void fetchSnapshot();
      })
      .catch((err: unknown) => {
        if (!aliveRef.current) return;
        setAutoReviewButton((prev) => applyAutoReviewButtonEvent(prev, { type: 'reject' }));
        if (shouldRenderReviewError(err) && !isToastOwnedRpcError(err)) {
          onToast(err instanceof Error ? err.message : String(err), true);
        }
      });
  }, [autoLanguagePicked, autoReviewButton, fetchSnapshot, onToast, reviewCommand, sendCommand, snapshot.reviewId]);

  /**
   * Run one Highlight call. The accepted ACK is strictly
   * `{ok:true,state:'running',highlightGeneration}`; the generation becomes
   * the request owner exactly like the Auto Review batchId. The snapshot's
   * highlightState stays authoritative for running/done/failed display once
   * the owner is released.
   */
  const runHighlight = useCallback(() => {
    if (highlightOwnerBusy(highlightOwner)) return;
    setHighlightOwner((prev) =>
      applyHighlightOwnerEvent(prev, { type: 'click', reviewId: snapshot.reviewId || null }),
    );
    sendCommand(reviewCommand(buildCodeReviewHighlightPayload()))
      .then((data) => {
        if (!aliveRef.current) return;
        const ack = parseCodeReviewHighlightAck(data);
        if (!ack.ok) {
          setHighlightOwner((prev) => applyHighlightOwnerEvent(prev, { type: 'reject' }));
          onToast(`Highlight start failed: ${ack.error}`, true);
          return;
        }
        setHighlightOwner((prev) =>
          applyHighlightOwnerEvent(prev, { type: 'ack', generation: ack.generation }),
        );
        void fetchSnapshot();
      })
      .catch((err: unknown) => {
        if (!aliveRef.current) return;
        setHighlightOwner((prev) => applyHighlightOwnerEvent(prev, { type: 'reject' }));
        if (shouldRenderReviewError(err) && !isToastOwnedRpcError(err)) {
          onToast(`Highlight start failed: ${err instanceof Error ? err.message : String(err)}`, true);
        }
      });
  }, [fetchSnapshot, highlightOwner, onToast, reviewCommand, sendCommand, snapshot.reviewId]);
  const runTldr = useCallback(() => {
    if (highlightOwnerBusy(tldrOwner) || snapshot.tldrState === 'running') return;
    setTldrOpen(true);
    setTldrOwner((prev) =>
      applyHighlightOwnerEvent(prev, { type: 'click', reviewId: snapshot.reviewId || null }),
    );
    sendCommand(reviewCommand(buildCodeReviewTldrPayload(autoReviewLanguagePayload(autoLanguagePicked))))
      .then((data) => {
        if (!aliveRef.current) return;
        const ack = parseCodeReviewTldrAck(data);
        if (!ack.ok) {
          setTldrOwner((prev) => applyHighlightOwnerEvent(prev, { type: 'reject' }));
          onToast(ack.error, true);
          return;
        }
        setTldrOwner((prev) => applyHighlightOwnerEvent(prev, { type: 'ack', generation: ack.generation }));
        void fetchSnapshot();
      })
      .catch((err: unknown) => {
        if (!aliveRef.current) return;
        setTldrOwner((prev) => applyHighlightOwnerEvent(prev, { type: 'reject' }));
        if (shouldRenderReviewError(err) && !isToastOwnedRpcError(err)) {
          onToast(err instanceof Error ? err.message : String(err), true);
        }
      });
  }, [autoLanguagePicked, fetchSnapshot, onToast, reviewCommand, sendCommand, snapshot.reviewId, snapshot.tldrState, tldrOwner]);
  const stopOperation = useCallback(
    (operation: CodeReviewStopOperation) => {
      sendCommand(reviewCommand(buildCodeReviewStopPayload(operation)))
        .then(() => {
          if (!aliveRef.current) return;
          void fetchSnapshot();
        })
        .catch((err: unknown) => {
          if (!isToastOwnedRpcError(err)) {
            onToast(err instanceof Error ? err.message : String(err), true);
          }
        });
    },
    [fetchSnapshot, onToast, reviewCommand, sendCommand],
  );



  // The composer is uncontrolled; typing never touches panel state. Auto-grow
  // mirrors the main composer fix: per-keystroke onInput only SCHEDULES the
  // resize, and the controller coalesces every frame's work into one layout
  // pass with static metrics measured once (see ./autoResize). flush() makes
  // a cleared composer collapse immediately after submit.
  const onComposerInput = useCallback((input: HTMLTextAreaElement) => {
    composerAutoResizeRef.current ??= createAutoResizeController<HTMLTextAreaElement>({
      maxLines: 6,
      measure: (el) => {
        const style = window.getComputedStyle(el);
        return {
          lineHeight: parseFloat(style.lineHeight) || 20,
          paddingVertical:
            (parseFloat(style.paddingTop) || 0) + (parseFloat(style.paddingBottom) || 0),
        };
      },
    });
    composerAutoResizeRef.current.resize(input);
  }, []);
  const flushComposerResize = useCallback((input: HTMLTextAreaElement) => {
    composerAutoResizeRef.current?.flush(input);
  }, []);

  /** Wire the uncontrolled composer textarea into the panel ref + autosize. */
  const setComposerRef = useCallback(
    (el: HTMLTextAreaElement | null) => {
      composerRef.current = el;
      if (el) onComposerInput(el);
    },
    [onComposerInput],
  );

  /** Commit the live (uncontrolled) composer text into the per-hunk draft map. */
  const commitSelectedDraft = useCallback(() => {
    const key = selectedHunkKey;
    const el = composerRef.current;
    if (!key || !el) return;
    const text = el.value;
    setDrafts((prev) => {
      if ((prev[key] ?? '') === text) return prev;
      const next = { ...prev };
      if (text) next[key] = text;
      else delete next[key];
      return next;
    });
  }, [selectedHunkKey]);

  const writeHunkAttachments = useCallback((key: string, next: readonly ComposerAttachment[]) => {
    const map = { ...hunkAttachmentsRef.current };
    if (next.length > 0) map[key] = [...next];
    else delete map[key];
    hunkAttachmentsRef.current = map;
    setHunkAttachments(map);
  }, []);

  const onAttachFiles = useCallback(
    (fileList: FileList) => {
      const key = selectedHunkKeyRef.current;
      if (!key) return;
      const current = hunkAttachmentsRef.current[key] ?? [];
      const plan = classifyImageAttachments(Array.from(fileList), {
        currentCount: current.length,
        currentWire: aggregateWire(current),
      });
      const skips: AttachSkip[] = [...plan.skipped];
      const accepted = plan.accepted;
      if (accepted.length === 0) {
        const summary = formatSkipSummary(skips);
        if (summary) onToast(summary, true);
        return;
      }
      const tasks = accepted.map((entry) => {
        const id = nextId('cr-att');
        return { id, entry, placeholder: placeholderFor(entry, id) };
      });
      const placeholderIds = new Set(tasks.map((task) => task.id));
      writeHunkAttachments(key, [...current, ...tasks.map((task) => task.placeholder)]);
      void (async () => {
        try {
          const results = await readAttachmentsInOrder(accepted, readAccepted);
          const readById = new Map<string, ReadResult>();
          results.forEach((result, index) => readById.set(tasks[index]!.id, result));
          const merged = mergeIntakeResults(tasks, readById);
          skips.push(...merged.skips);
          const base = (hunkAttachmentsRef.current[key] ?? []).filter(
            (attachment) => !placeholderIds.has(attachment.id),
          );
          const admitted = admitBuiltAttachments(base, merged.built);
          skips.push(...admitted.skipped);
          writeHunkAttachments(key, admitted.next);
        } catch (err) {
          writeHunkAttachments(
            key,
            (hunkAttachmentsRef.current[key] ?? []).filter(
              (attachment) => !placeholderIds.has(attachment.id),
            ),
          );
          onToast(err instanceof Error ? err.message : String(err), true);
        }
        const summary = formatSkipSummary(skips);
        if (summary) onToast(summary, true);
      })();
    },
    [onToast, writeHunkAttachments],
  );

  const onRemoveAttachment = useCallback(
    (id: string) => {
      const key = selectedHunkKeyRef.current;
      if (!key) return;
      writeHunkAttachments(
        key,
        (hunkAttachmentsRef.current[key] ?? []).filter((attachment) => attachment.id !== id),
      );
    },
    [writeHunkAttachments],
  );

  const { recording, transcribing, startRecording, stopRecording } = useSttRecorder({
    getOwner: () => (aliveRef.current ? owningSessionIdRef.current : null),
    sttConfigured: liveSettings,
    sendCommand,
    onTranscript: (text) => {
      const hunkKey = sttDraftHunkKeyRef.current;
      if (!hunkKey || !text) return;
      const el = composerRef.current;
      if (el && selectedHunkKeyRef.current === hunkKey) {
        const current = el.value;
        const separator = current.trim() && !current.endsWith('\n') ? '\n' : '';
        el.value = `${current}${separator}${text}`;
        onComposerInput(el);
        setDrafts((prev) => ({ ...prev, [hunkKey]: el.value }));
        return;
      }
      setDrafts((prev) => {
        const existing = prev[hunkKey] ?? '';
        const separator = existing.trim() && !existing.endsWith('\n') ? '\n' : '';
        return { ...prev, [hunkKey]: `${existing}${separator}${text}` };
      });
    },
    onError: (message) => onToast(message, true),
  });

  const onToggleRecording = useCallback(() => {
    if (recording) {
      stopRecording();
      return;
    }
    const hunkKey = selectedHunkKeyRef.current;
    if (!hunkKey) return;
    sttDraftHunkKeyRef.current = hunkKey;
    void startRecording();
  }, [recording, startRecording, stopRecording]);

  const selectFile = useCallback(
    (path: string, visibleIndex: number) => {
      // Commit the outgoing hunk's live text before the composer remounts.
      commitSelectedDraft();
      if (selectedPathRef.current !== path) {
        const leaving = selectedPathRef.current;
        for (const key of [leaving, path]) {
          if (!key) continue;
          selectionEpochRef.current[key] = (selectionEpochRef.current[key] ?? 0) + 1;
        }
        setLoadStates((prev) => {
          const stale = (key: string) => {
            const current = prev[key];
            if (!current || (!current.loading && current.error != null)) return false;
            return current.loading;
          };
          if (!stale(leaving) && !stale(path)) return prev;
          const next = { ...prev };
          for (const key of [leaving, path]) {
            if (stale(key)) next[key] = { loading: false, error: prev[key]?.error ?? null };
          }
          return next;
        });
        delete fetchPromisesRef.current[leaving];
        delete fetchPromisesRef.current[path];
      }
      selectedPathRef.current = path;
      selectedHunkKeyRef.current = null;
      selectedFindingRef.current = null;
      setSelectedFinding(null);
      setSelectedPath(path);
      setSelectedHunkKey(null);

      setHasUserSelectedFile(true);
      setFileFocusIndex(visibleIndex);
      // Every file opens on Source; Preview state stays per-session, not per-file.
      setPreviewSurface(false);
      if (isNarrow) setMobileTab('diff');
    },
    [commitSelectedDraft, isNarrow],
  );

  const toggleTreeDir = useCallback((nodeIndex: number, visibleIndex: number) => {
    setFileTree((prev) => treeToggleCollapse(prev, nodeIndex));
    setFileFocusIndex(visibleIndex);
  }, []);

  const activateHunk = useCallback(
    (key: string) => {
      commitSelectedDraft();
      selectedHunkKeyRef.current = key;
      selectedFindingRef.current = null;
      setSelectedFinding(null);
      setSelectedHunkKey(key);

      setCollapsedHunks((prev) => {
        if (!prev.has(key)) return prev;
        const next = new Set(prev);
        next.delete(key);
        return next;
      });
    },
    [commitSelectedDraft],
  );

  const selectHunk = useCallback(
    (key: string) => {
      activateHunk(key);
      if (isNarrow) setMobileTab('thread');
    },
    [activateHunk, isNarrow],
  );

  const navigateHunk = useCallback(
    (key: string, requiredWindow: number) => {
      activateHunk(key);
      const path = selectedPathRef.current;
      if (!path) return;
      const snapshotId = snapshot.snapshotId;
      setDiffViews((prev) => {
        const current = prev[path];
        const currentWindow = current?.snapshotId === snapshotId ? current.window : 0;
        const window = Math.min(
          FILE_RENDER_SOFT_CAP,
          Math.max(currentWindow, requiredWindow),
        );
        if (current?.snapshotId === snapshotId && current.window === window) return prev;
        return { ...prev, [path]: { snapshotId, window } };
      });
    },
    [activateHunk, snapshot.snapshotId],
  );

  const orderedFindings = useMemo(() => {
    const bodies = [];
    for (const file of snapshot.files) {
      const loaded = loadedDiffs[file.path];
      const hunks = loaded && loaded.snapshotId === snapshot.snapshotId && loaded.hunks.length > 0
        ? loaded.hunks
        : file.hunks;
      for (const hunk of hunks) {
        bodies.push({ identity: hunkIdentityFor(snapshot.snapshotId, file, hunk), lines: hunk.lines });
      }
    }
    return orderReviewFindings(snapshot.files, admittedFindings, bodies);
  }, [admittedFindings, loadedDiffs, snapshot.files, snapshot.snapshotId]);
  const issueIndex = orderedFindings.findIndex((entry) =>
    selectedFinding != null
    && entry.findingIndex === selectedFinding.findingIndex
    && entry.identity.snapshotId === selectedFinding.identity.snapshotId
    && entry.identity.path === selectedFinding.identity.path
    && entry.identity.contentHash === selectedFinding.identity.contentHash
    && entry.identity.oldStart === selectedFinding.identity.oldStart
    && entry.identity.oldCount === selectedFinding.identity.oldCount
    && entry.identity.newStart === selectedFinding.identity.newStart
    && entry.identity.newCount === selectedFinding.identity.newCount,
  );
  /** Grow the file window through an admitted finding's last physical row.
   *  No-op while the body is unloaded (fileRow -1); the ordered list
   *  recomputes when the body lands and the reveal effect retries. Growth
   *  stops at the review UI cap and never shrinks a larger window. */
  const revealFinding = useCallback((entry: OrderedReviewFinding | null) => {
    if (!entry || entry.fileRow < 0) return;
    const path = entry.identity.path;
    const endIndex = entry.fileRow + Math.max(0, entry.lastRow - entry.firstRow);
    const required = endIndex + 1;
    setDiffViews((prev) => {
      const current = prev[path];
      const currentWindow = current?.snapshotId === entry.identity.snapshotId ? current.window : 0;
      const window = Math.max(currentWindow, Math.min(FILE_DIFF_MAX_UI_LINES, required));
      if (current?.snapshotId === entry.identity.snapshotId && current.window === window) return prev;
      return { ...prev, [path]: { snapshotId: entry.identity.snapshotId, window } };
    });
  }, []);

  const navigateIssue = useCallback((direction: ReviewFindingDirection) => {
    const target = navigateReviewFinding(orderedFindings, selectedFindingRef.current, direction);
    if (!target) return;
    const ref = { identity: target.identity, findingIndex: target.findingIndex };
    commitSelectedDraft();
    const path = target.identity.path;
    selectedPathRef.current = path;
    const key = hunkKey(target.identity);

    selectedHunkKeyRef.current = key;
    selectedFindingRef.current = ref;
    setSelectedPath(path);
    setSelectedHunkKey(key);
    setSelectedFinding(ref);
    setPreviewSurface(false);
    setHasUserSelectedFile(true);
    setCollapsedHunks((prev) => {
      if (!prev.has(key)) return prev;
      const next = new Set(prev);
      next.delete(key);
      return next;
    });
    if (isNarrow) setMobileTab('diff');
    revealFinding(target);
  }, [commitSelectedDraft, isNarrow, orderedFindings, revealFinding]);
  const selectFindingIndex = useCallback((findingIndex: number) => {
    if (!selectedIdentity) return;
    const ref = { identity: selectedIdentity, findingIndex };
    selectedFindingRef.current = ref;
    setSelectedFinding(ref);
    revealFinding(orderedFindings.find((candidate) => findingRefMatches(ref, candidate)) ?? null);
  }, [orderedFindings, revealFinding, selectedIdentity]);

  // Body-load reveal: once the target hunk body lands, fileRow becomes real
  // and the window grows to cover it. Central for prev/next AND dock select.
  useEffect(() => {
    if (!selectedFinding) return;
    revealFinding(orderedFindings.find((candidate) => findingRefMatches(selectedFinding, candidate)) ?? null);
  }, [orderedFindings, revealFinding, selectedFinding]);

  // The TLDR pane survives a prestart poll while a start is pending owned;
  // only a real snapshot change after the owner releases closes it.
  useEffect(() => {
    if (tldrOwner.pending) return;
    if (snapshot.tldrSnapshotId !== snapshot.snapshotId) setTldrOpen(false);
  }, [snapshot.snapshotId, snapshot.tldrSnapshotId, tldrOwner.pending]);


  const toggleHunkCollapsed = useCallback((key: string) => {
    setCollapsedHunks((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  }, []);

  const submitComment = useCallback(() => {
    const key = selectedHunkKey;
    const input = composerRef.current;
    if (!key || !selectedIdentity || !snapshot.snapshotId || !input) return;
    if (utf8ByteLength(input.value) > CODE_REVIEW_MAX_COMMENT_BYTES) {
      onToast(`Comment exceeds the ${CODE_REVIEW_MAX_COMMENT_BYTES}-byte limit · draft kept`, true);
      return;
    }
    const text = input.value.trim();
    const identity = selectedIdentity;
    const submittedEl = input;
    const submittedText = input.value;
    const submittedKey = key;
    const queued = hunkAttachmentsRef.current[key] ?? [];
    const sentIds = new Set(
      queued.filter((a) => a.kind === 'image' && a.dataBase64 != null).map((a) => a.id),
    );
    const images = attachmentsToImageBlocks(
      queued.filter((a) => sentIds.has(a.id)),
    );
    if (!text && images.length === 0) return;
    if (mutatingRef.current) return;
    mutatingRef.current = true;
    const requestSeq = ++snapshotRequestSeqRef.current;
    setBusy('comment');
    sendCommand(
      reviewCommand({
        type: 'code_review_comment',
        snapshotId: identity.snapshotId || snapshot.snapshotId,
        path: identity.path,
        oldStart: identity.oldStart,
        oldCount: identity.oldCount,
        newStart: identity.newStart,
        newCount: identity.newCount,
        contentHash: identity.contentHash,
        comment: text,
        ...(images.length > 0 ? { images } : {}),
      }),
    )
      .then((data) => {
        if (!aliveRef.current || requestSeq !== snapshotRequestSeqRef.current) return;
        applyData(data);
        // Accepted comment ACK: emit a full-hunk-keyed follow event so the
        // dock follows THIS hunk's thread tail while pinned (rejection throws,
        // and a switched hunk never retargets the event).
        followSeqRef.current += 1;
        setAcceptedFollow({ key: submittedKey, seq: followSeqRef.current });
        const live = composerRef.current;
        if (live === submittedEl && live.value === submittedText) {
          live.value = '';
          flushComposerResize(live);
          setDrafts((prev) => {
            if (!(submittedKey in prev)) return prev;
            const next = { ...prev };
            delete next[submittedKey];
            return next;
          });
        }
        const current = hunkAttachmentsRef.current[submittedKey];
        if (current?.some((a) => sentIds.has(a.id))) {
          writeHunkAttachments(submittedKey, removeSentAttachments(current, sentIds));
        }
      })
      .catch((err: unknown) => {
        if (!aliveRef.current || requestSeq !== snapshotRequestSeqRef.current) return;
        if (!isToastOwnedRpcError(err)) {
          onToast(err instanceof Error ? err.message : String(err), true);
        }
      })
      .finally(() => {
        mutatingRef.current = false;
        if (aliveRef.current && requestSeq === snapshotRequestSeqRef.current) setBusy(null);
      });
  }, [
    applyData,
    flushComposerResize,
    onToast,
    reviewCommand,
    selectedHunkKey,
    selectedIdentity,
    sendCommand,
    snapshot.snapshotId,
    writeHunkAttachments,
  ]);

  // Close/refresh are guarded: with owned/queued review work (internalBusy)
  // or unsent drafts they surface an inline confirmation instead of acting.
  // Visible live status never drives this warning. The composer is
  // uncontrolled, so React `draftCount` is stale until blur / hunk switch.
  // Read composerRef.current.value synchronously, commit that live text,
  // then decide from the merged count — never the stale state.
  const countUnsentDraftsNow = useCallback(() => {
    const key = selectedHunkKey;
    const live = composerRef.current?.value;
    const merged: Record<string, string> = { ...drafts };
    if (key && live !== undefined) {
      if (live) merged[key] = live;
      else delete merged[key];
    }
    return Object.values(merged).filter((text) => text.trim().length > 0).length;
  }, [drafts, selectedHunkKey]);

  const requestRefresh = useCallback(() => {
    commitSelectedDraft();
    const unsent = countUnsentDraftsNow();
    if (unsent > 0 || hasInternalBusy) {
      setConfirmAction({ kind: 'refresh', draftCount: unsent });
      return;
    }
    refresh();
  }, [commitSelectedDraft, countUnsentDraftsNow, hasInternalBusy, refresh]);

  const requestClose = useCallback(() => {
    commitSelectedDraft();
    const unsent = countUnsentDraftsNow();
    if (unsent > 0 || hasInternalBusy) {
      setConfirmAction({ kind: 'close', draftCount: unsent });
      return;
    }
    onClose();
  }, [commitSelectedDraft, countUnsentDraftsNow, hasInternalBusy, onClose]);

  const cancelConfirm = useCallback(() => {
    setConfirmAction(null);
    composerRef.current?.focus({ preventScroll: true });
  }, []);

  const confirmPrimary = useCallback(() => {
    const action = confirmAction;
    setConfirmAction(null);
    if (!action) return;
    if (action.kind === 'close') onClose();
    else refresh();
  }, [confirmAction, onClose, refresh]);

  const handleComposerKeyDown = (e: ReactKeyboardEvent<HTMLTextAreaElement>) => {
    // Enter submits; Shift+Enter inserts a newline; the Enter that confirms
    // an IME composition (nativeEvent.isComposing) must never submit.
    // Modifiers other than Shift do not prevent submit.
    if (e.key !== 'Enter' || e.shiftKey || e.nativeEvent.isComposing) return;
    e.preventDefault();
    submitComment();
  };

  const handleFileListKeyDown = useCallback(
    (e: ReactKeyboardEvent<HTMLUListElement>) => {
      if (rows.length === 0) return;
      const activeIndex = rowButtonRefs.current.findIndex((el) => el === document.activeElement);
      const base = activeIndex >= 0 ? activeIndex : Math.max(0, fileFocusIndex);
      const action = treeKeyboardAction(rows, fileTree, base, e.key);
      if (action.kind === 'none') return;
      e.preventDefault();
      setFileFocusIndex(action.nextIndex);
      if (action.kind === 'move') {
        rowButtonRefs.current[action.nextIndex]?.focus();
        return;
      }
      if (action.kind === 'toggle') {
        setFileTree((prev) => treeToggleCollapse(prev, action.nodeIndex));
        rowButtonRefs.current[action.nextIndex]?.focus();
        return;
      }
      // select: open the focused file row.
      const fileIndex = treeFileIndexAt(rows, fileTree, action.nextIndex);
      const file = fileIndex >= 0 ? snapshot.files[fileIndex] : undefined;
      if (file) selectFile(file.path, action.nextIndex);
      rowButtonRefs.current[action.nextIndex]?.focus();
    },
    [fileFocusIndex, fileTree, rows, selectFile, snapshot.files],
  );

  // Desktop-only thread-column resizer: pointer drag + arrow keys. Mobile
  // never mounts handlers (resizer is not rendered when narrow).
  const applyThreadWidth = useCallback((next: number) => {
    const stored = writeStoredCodeReviewThreadWidth(
      typeof window !== 'undefined' ? window.localStorage : null,
      next,
    );
    setThreadWidth(stored);
  }, []);

  const onThreadResizePointerDown = useCallback(
    (e: ReactPointerEvent<HTMLDivElement>) => {
      if (isNarrow) return;
      e.preventDefault();
      const target = e.currentTarget;
      target.setPointerCapture(e.pointerId);
      resizeDragRef.current = { startX: e.clientX, startWidth: threadWidth };
    },
    [isNarrow, threadWidth],
  );

  const onThreadResizePointerMove = useCallback(
    (e: ReactPointerEvent<HTMLDivElement>) => {
      const drag = resizeDragRef.current;
      if (!drag) return;
      // Dragging left grows the thread column (resizer sits left of comments).
      applyThreadWidth(drag.startWidth + (drag.startX - e.clientX));
    },
    [applyThreadWidth],
  );

  const onThreadResizePointerUp = useCallback((e: ReactPointerEvent<HTMLDivElement>) => {
    if (!resizeDragRef.current) return;
    resizeDragRef.current = null;
    try {
      e.currentTarget.releasePointerCapture(e.pointerId);
    } catch {
      /* already released */
    }
  }, []);

  const onThreadResizeKeyDown = useCallback(
    (e: ReactKeyboardEvent<HTMLDivElement>) => {
      if (isNarrow) return;
      if (e.key === 'ArrowLeft') {
        e.preventDefault();
        applyThreadWidth(stepCodeReviewThreadWidth(threadWidth, 1));
      } else if (e.key === 'ArrowRight') {
        e.preventDefault();
        applyThreadWidth(stepCodeReviewThreadWidth(threadWidth, -1));
      } else if (e.key === 'Home') {
        e.preventDefault();
        applyThreadWidth(CODE_REVIEW_THREAD_WIDTH_MAX);
      } else if (e.key === 'End') {
        e.preventDefault();
        applyThreadWidth(CODE_REVIEW_THREAD_WIDTH_MIN);
      }
    },
    [applyThreadWidth, isNarrow, threadWidth],
  );

  // Desktop-only files-column resizer: mirrors the thread resizer exactly,
  // but sits RIGHT of the files column — dragging right (and ArrowRight)
  // grows it.
  const applyFilesWidth = useCallback((next: number) => {
    const stored = writeStoredCodeReviewFilesWidth(
      typeof window !== 'undefined' ? window.localStorage : null,
      next,
    );
    setFilesWidth(stored);
  }, []);

  const onFilesResizePointerDown = useCallback(
    (e: ReactPointerEvent<HTMLDivElement>) => {
      if (isNarrow) return;
      e.preventDefault();
      const target = e.currentTarget;
      target.setPointerCapture(e.pointerId);
      filesResizeDragRef.current = { startX: e.clientX, startWidth: filesWidth };
    },
    [filesWidth, isNarrow],
  );

  const onFilesResizePointerMove = useCallback(
    (e: ReactPointerEvent<HTMLDivElement>) => {
      const drag = filesResizeDragRef.current;
      if (!drag) return;
      // Dragging right grows the files column (resizer sits right of files).
      applyFilesWidth(drag.startWidth + (e.clientX - drag.startX));
    },
    [applyFilesWidth],
  );

  const onFilesResizePointerUp = useCallback((e: ReactPointerEvent<HTMLDivElement>) => {
    if (!filesResizeDragRef.current) return;
    filesResizeDragRef.current = null;
    try {
      e.currentTarget.releasePointerCapture(e.pointerId);
    } catch {
      /* already released */
    }
  }, []);

  const onFilesResizeKeyDown = useCallback(
    (e: ReactKeyboardEvent<HTMLDivElement>) => {
      if (isNarrow) return;
      if (e.key === 'ArrowRight') {
        e.preventDefault();
        applyFilesWidth(stepCodeReviewFilesWidth(filesWidth, 1));
      } else if (e.key === 'ArrowLeft') {
        e.preventDefault();
        applyFilesWidth(stepCodeReviewFilesWidth(filesWidth, -1));
      } else if (e.key === 'Home') {
        e.preventDefault();
        applyFilesWidth(CODE_REVIEW_FILES_WIDTH_MAX);
      } else if (e.key === 'End') {
        e.preventDefault();
        applyFilesWidth(CODE_REVIEW_FILES_WIDTH_MIN);
      }
    },
    [applyFilesWidth, filesWidth, isNarrow],
  );

  const fetchPagesUntil = useCallback(
    async (path: string): Promise<LoadedDiff | null> => {
      const start = diffCacheRef.current.entries[path];
      if (!start) return null;
      const seq = loadSeqRef.current[path];
      const epoch = selectionEpochRef.current[path] ?? 0;
      const existing = fetchPromisesRef.current[path];
      if (existing?.seq === seq && existing.epoch === epoch) return existing.promise;
      if (existing) delete fetchPromisesRef.current[path];
      const promise = (async () => {
        let next = start;
        try {
          let fetchedOnce = false;
          while (!next.hunksComplete && (next.hasMoreBackend || !fetchedOnce)) {
            if (!aliveRef.current || seq !== loadSeqRef.current[path] || epoch !== (selectionEpochRef.current[path] ?? 0)) return null;
            if (selectedPathRef.current !== path) return null;
            const data = await sendCommand(
              reviewCommand({
                type: 'code_review_file_diff',
                snapshotId: next.snapshotId,
                path: next.path,
                cursor: next.nextCursor,
                maxLines: FILE_PAGE_REQUEST_LINES,
              }),
            );
            if (!aliveRef.current || seq !== loadSeqRef.current[path] || epoch !== (selectionEpochRef.current[path] ?? 0)) return null;
            if (selectedPathRef.current !== path) return null;
            const page = normalizeFileDiffPage(data);
            if (!page) throw new Error('Invalid code_review_file_diff response');
            const merged = appendFileDiffPage(next, page);
            if (merged === next) {
              // Stale/duplicate page (snapshot, path, or cursor mismatch) —
              // stop; the stream advances only as far as what is verified.
              break;
            }
            fetchedOnce = true;
            next = merged;
            applyDiffCacheResult(
              loadedDiffCachePut(diffCacheRef.current, next, diffCacheGuards()),
            );
          }
          if (!aliveRef.current || seq !== loadSeqRef.current[path] || epoch !== (selectionEpochRef.current[path] ?? 0)) return null;
          return next;
        } catch (err) {
          if (!aliveRef.current || seq !== loadSeqRef.current[path] || epoch !== (selectionEpochRef.current[path] ?? 0)) return null;
          // Business failures latch inline (explicit Retry is the only
          // same-epoch retry); transport failures suppress the inline error
          // and stall the eager loader until a new connection epoch.
          const outcome: FileDiffFetchOutcome = shouldRenderReviewError(err)
            ? { kind: 'business-error', message: err instanceof Error ? err.message : String(err) }
            : { kind: 'transport-error' };
          if (outcome.kind === 'transport-error') fetchStalledRef.current[path] = true;
          setLoadStates((prev) => ({
            ...prev,
            [path]: nextFileDiffLoadState(outcome, prev[path] ?? { loading: true, error: null }),
          }));
          return null;
        } finally {
          const current = fetchPromisesRef.current[path];
          if (current?.seq === seq && current.epoch === epoch) delete fetchPromisesRef.current[path];
        }
      })();
      fetchPromisesRef.current[path] = { seq, epoch, promise };
      setLoadStates((prev) => ({ ...prev, [path]: { loading: true, error: null } }));
      const result = await promise;
      // Successful completion alone clears the error latch. A business
      // failure left the error set by catch (explicit Retry re-runs this); a
      // transport failure left a stall; a stale return (superseded by a newer
      // seq) leaves the latch to the new owner. Clear the stall only on success.
      if (aliveRef.current
        && seq === loadSeqRef.current[path]
        && epoch === (selectionEpochRef.current[path] ?? 0)
        && result) {
        delete fetchStalledRef.current[path];
        setLoadStates((prev) => ({
          ...prev,
          [path]: nextFileDiffLoadState({ kind: 'success' }, prev[path] ?? { loading: true, error: null }),
        }));
      }
      return result;
    },
    [applyDiffCacheResult, diffCacheGuards, reviewCommand, sendCommand],
  );

  const expandGap = useCallback(
    async (gap: DiffGap, from: GapExpandFrom, take: number) => {
      const run = async () => {
        const path = selectedPathRef.current;
        const snapshotId = snapshot.snapshotId;
        const file = snapshot.files.find((entry) => entry.path === path);
        if (!path || !snapshotId || !file || file.binary) return;
        const id = diffGapId(gap);
        setExpandingGapId(id);
        try {
          let remainingTake = take;
          let safety = FILE_RENDER_SOFT_CAP;
          while (remainingTake > 0 && safety > 0) {
            safety -= 1;
            const store = gapStoreRef.current[path];
            const state =
              store && store.snapshotId === snapshotId && store.states[id]
                ? store.states[id]
                : newGapExpandState(gap);
            const remaining = remainingDiffGap(state);
            const window = gapExpandWindow(
              remaining,
              from,
              Math.min(CONTEXT_EXPAND_LINES, remainingTake),
            );
            if (!window) return;
            const data = await sendCommand(
              reviewCommand({
                type: 'code_review_file_context',
                snapshotId,
                path,
                oldStart: window.oldStart,
                newStart: window.newStart,
                count: window.count,
              }),
            );
            if (!aliveRef.current || selectedPathRef.current !== path) return;
            const page = normalizeFileContextPage(data);
            if (!page || page.snapshotId !== snapshotId || page.path !== path) {
              throw new Error('Invalid code_review_file_context response');
            }
            const current = gapStoreRef.current[path];
            const base =
              current && current.snapshotId === snapshotId
                ? current
                : {
                    snapshotId,
                    oldLineCount: 0,
                    newLineCount: 0,
                    states: {},
                    lines: {},
                  };
            const priorState = base.states[id] ?? newGapExpandState(gap);
            const priorLines = base.lines[id] ?? [];
            const next = {
              ...gapStoreRef.current,
              [path]: {
                snapshotId,
                oldLineCount: page.oldLineCount,
                newLineCount: page.newLineCount,
                states: {
                  ...base.states,
                  [id]: recordGapExpand(priorState, from, page.lines.length),
                },
                lines: {
                  ...base.lines,
                  [id]: mergeExpandedLines(priorLines, page.lines, from),
                },
              },
            };
            gapStoreRef.current = next;
            setGapStore(next);
            if (page.lines.length === 0) return;
            remainingTake -= page.lines.length;
          }
        } catch (err: unknown) {
          // Transport failures stay with the shared outage toast; every
          // business failure toasts with context once per gap identity.
          if (!aliveRef.current || selectedPathRef.current !== path) return;
          reportBackgroundFailure(
            `gap-expand\0${path}\0${snapshot.snapshotId}\0${id}`,
            'Could not expand diff context',
            err,
          );
        } finally {
          if (aliveRef.current) setExpandingGapId((current) => (current === id ? null : current));
        }
      };
      const queued = expandChainRef.current.then(run, run);
      expandChainRef.current = queued.then(
        () => undefined,
        () => undefined,
      );
      await queued;
    },
    [reportBackgroundFailure, reviewCommand, sendCommand, snapshot.files, snapshot.snapshotId],
  );

  const growDiffWindow = useCallback(
    async (path: string) => {
      const loaded = diffCacheRef.current.entries[path];
      if (!loaded) return;
      const currentWindow = diffViews[path]?.window ?? 0;
      const plan = planDiffWindow(loaded, currentWindow);
      const hasError = loadStates[path]?.error != null;
      if (fetchStalledRef.current[path]) return;
      if (!plan.canLoadMore && !hasError) return;
      const fetched = await fetchPagesUntil(path);
      if (!fetched) return; // stale (dropped silently) or error already surfaced
      const target = Math.max(currentWindow, plan.target);
      setDiffViews((prev) => ({
        ...prev,
        [path]: {
          snapshotId: fetched.snapshotId,
          window: Math.min(target, fetched.lines.length, FILE_DIFF_MAX_UI_LINES),
        },
      }));
    },
    [diffViews, fetchPagesUntil, loadStates],
  );

  const onDiffNearBottom = useCallback(() => {
    const path = selectedPath;
    if (!path) return;
    if (loadStates[path]?.error != null) return;
    if (fetchStalledRef.current[path]) return;
    void growDiffWindow(path);
  }, [growDiffWindow, loadStates, selectedPath]);

  const retryDiffLoad = useCallback(() => {
    if (!selectedPath) return;
    void growDiffWindow(selectedPath);
  }, [growDiffWindow, selectedPath]);

  useEffect(() => {
    if (busy !== null) return;
    if (!selectedPath || !loadedDiff) return;
    if (loadedDiff.snapshotId !== snapshot.snapshotId) return;
    if (loadedDiff.hunksComplete) return;
    if (selectedFile?.binary) return;
    if (!shouldEagerFileDiffFetch(loadState, !!fetchStalledRef.current[selectedPath])) return;
    void fetchPagesUntil(selectedPath);
  }, [
    busy,
    fetchPagesUntil,
    loadState.error,
    loadState.loading,
    loadedDiff,
    selectedFile?.binary,
    selectedPath,
    snapshot.snapshotId,
  ]);

  // Reconcile once per authoritative connection epoch (App passes a
  // monotonically increasing connectionEpoch after a ready reconnect). A
  // same-process reconnect probes the existing controller and preserves it
  // when the authoritative snapshot is still live; a real process restart
  // whose probe reports no open review reopens exactly once from the retained
  // openArgs. Transport failure of the probe never renders inline nor spins —
  // the next epoch retries. The eager file-diff stall is also cleared so a
  // transport-stalled fetch may retry after reconnect.
  useEffect(() => {
    if (lastEpochRef.current === connectionEpoch) return;
    lastEpochRef.current = connectionEpoch;
    if (!aliveRef.current) return;
    // Invalid scope probes nothing — reconnect included (fail-closed).
    if (!codeReviewOpenCommand(openArgs).ok) return;
    // A new authoritative epoch clears transport stalls so file-diff fetches
    // may retry (transport errors are never rendered inline here).
    fetchStalledRef.current = {};
    const requestSeq = ++snapshotRequestSeqRef.current;
    sendCommand(reviewCommand({ type: 'code_review_snapshot' }))
      .then((data) => {
        if (!aliveRef.current || requestSeq !== snapshotRequestSeqRef.current) return;
        const outcome = classifyReviewProbe(data, null);
        if (outcome.kind === 'preserve') {
          applyData(data);
          // Retry a transport-stalled file-diff fetch for the selected path:
          // the eager effect does not re-run on an unchanged snapshot id, so
          // the epoch drives the retry directly (single-flight dedupes).
          const path = selectedPathRef.current;
          const loaded = path ? diffCacheRef.current.entries[path] : null;
          if (path && loaded && !loaded.hunksComplete) {
            void fetchPagesUntil(path);
          }
        } else if (outcome.kind === 'reopen') {
          openReview(true);
        }
        // 'wait' (transport error): suppress; the next epoch retries.
      })
      .catch((err: unknown) => {
        if (!aliveRef.current || requestSeq !== snapshotRequestSeqRef.current) return;
        const outcome = classifyReviewProbe(null, err);
        if (outcome.kind === 'reopen' || isNoOpenCodeReviewError(err)) {
          if (outcome.kind === 'reopen') openReview(true);
          return;
        }
        // 'wait' = transport: suppressed, the next epoch retries, last-good
        // state kept. 'preserve' keeps the retained snapshot but toasts the
        // business cause once per epoch (probe business errors are no longer
        // silent).
        reportBackgroundFailure(`epoch-probe\0${connectionEpoch}`, 'Code review probe failed', err);
      });
  }, [connectionEpoch, applyData, fetchPagesUntil, openReview, reportBackgroundFailure, reviewCommand, sendCommand, openArgs]);

  // A file whose initial window was zero (a placeholder with no snapshot
  // lines) gets its first soft-cap window as soon as lines arrive.
  useEffect(() => {
    if (!selectedPath || !loadedDiff) return;
    if (loadedDiff.snapshotId !== snapshot.snapshotId) return;
    if (loadedDiff.lines.length === 0) return;
    const view = diffViews[selectedPath];
    if (view && view.snapshotId === snapshot.snapshotId && view.window > 0) return;
    setDiffViews((prev) => ({
      ...prev,
      [selectedPath]: {
        snapshotId: snapshot.snapshotId,
        window: Math.min(FILE_RENDER_SOFT_CAP, loadedDiff.lines.length),
      },
    }));
  }, [diffViews, loadedDiff, selectedPath, snapshot.snapshotId]);

  const globalBusy = busy === 'open' || busy === 'refresh';
  const threadBusy = busy === 'comment' || busy === 'abort' ? busy : null;
  const autoView = autoReviewButtonView(autoReviewButton, spinnerDue.autoReview);
  const autoBusy = autoView.busy;
  // The request owner covers the click → ACK → matching-snapshot gap; the
  // wire highlightState owns the visible running display once released.
  const highlightView = highlightButtonView(
    highlightOwnerBusy(highlightOwner),
    snapshot.highlightState,
    spinnerDue.highlight,
  );
  const highlightBusy = highlightView.busy;
  const tldrView = tldrButtonView(
    highlightOwnerBusy(tldrOwner),
    snapshot.tldrState,
    spinnerDue.tldr,
  );
  const tldrBusy = tldrView.busy;
  const tldrPaneMarkdown = tldrMarkdownForSnapshot(snapshot);
  const stopLiveGeneration = useCallback((): boolean => {
    if (autoBusy) {
      stopOperation('autoReview');
      return true;
    }
    if (highlightBusy) {
      stopOperation('highlight');
      return true;
    }
    if (tldrBusy) {
      stopOperation('tldr');
      return true;
    }
    return false;
  }, [autoBusy, highlightBusy, tldrBusy, stopOperation]);

  const handlePanelKeyDown = (e: ReactKeyboardEvent<HTMLElement>) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      if (stopLiveGeneration()) return;
      const target = e.target as HTMLElement | null;
      if (target && target.tagName === 'TEXTAREA') {
        if (selectedIsCancellable) {
          abort();
          return;
        }
        target.blur();
        return;
      }
      if (focusMode) {
        setFocusMode(false);
        return;
      }
      if (confirmAction) {
        cancelConfirm();
        return;
      }
      requestClose();
      return;
    }
    if (e.key !== 'Tab') return;
    const panel = panelRef.current;
    if (!panel) return;
    const scope = confirmAction
      ? panel.querySelector<HTMLElement>('.code-review__confirm')
      : panel;
    if (!scope) return;
    const focusables = Array.from(
      scope.querySelectorAll<HTMLElement>(
        'button:not(:disabled), [href], input:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex="-1"])',
      ),
    ).filter((el) => el.offsetParent !== null || el === document.activeElement);
    if (focusables.length === 0) return;
    const first = focusables[0];
    const last = focusables[focusables.length - 1];
    const active = document.activeElement as HTMLElement | null;
    if (e.shiftKey) {
      if (active === first || !scope.contains(active)) {
        e.preventDefault();
        last.focus();
      }
    } else if (active === last || !scope.contains(active)) {
      e.preventDefault();
      first.focus();
    }
  };

  // Document Escape matches in-panel: Stop autoReview, then highlight, then close.
  useEffect(() => {
    const onDocKeyDown = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return;
      if (panelRef.current && panelRef.current.contains(e.target as Node)) return;
      e.preventDefault();
      if (stopLiveGeneration()) return;
      if (focusMode) {
        setFocusMode(false);
        return;
      }
      requestClose();
    };
    document.addEventListener('keydown', onDocKeyDown);
    return () => document.removeEventListener('keydown', onDocKeyDown);
  }, [focusMode, requestClose, stopLiveGeneration]);
  const totals =
    snapshot.totalsComplete === true
      ? `+${filesForList.reduce((sum, file) => sum + file.insertions, 0)} / -${filesForList.reduce((sum, file) => sum + file.deletions, 0)}`
      : null;
  // scoped to the selected thread and do not freeze Refresh/file navigation.
  const langDefaultTag = REVIEW_LANGUAGE_OPTIONS.some(
    (option) => option.tag === snapshot.reviewLanguageDefault,
  )
    ? snapshot.reviewLanguageDefault ?? 'zh'
    : 'zh';
  const autoLangValue =
    reviewLanguageSelection(snapshot.reviewLanguageDefault, autoLanguagePicked) || langDefaultTag;
  const langOptions = REVIEW_LANGUAGE_OPTIONS.filter((option) => option.tag !== langDefaultTag);
  const langDefaultOption = REVIEW_LANGUAGE_OPTIONS.find((option) => option.tag === langDefaultTag)
    ?? REVIEW_LANGUAGE_OPTIONS[0]!;
  // One shared 151ms captionless spinner gate: one timer per busy slot.
  useEffect(() => {
    const busySlots = {
      autoReview: autoBusy,
      highlight: highlightBusy,
      tldr: tldrBusy,
      refresh: busy === 'refresh',
    };
    setSpinnerDue((prev) => {
      const next = { ...prev };
      let changed = false;
      for (const slot of SPINNER_SLOTS) {
        if (!busySlots[slot] && prev[slot]) {
          next[slot] = false;
          changed = true;
        }
      }
      return changed ? next : prev;
    });
    for (const slot of SPINNER_SLOTS) {
      const timer = spinnerTimersRef.current[slot];
      if (busySlots[slot]) {
        if (timer == null) {
          spinnerTimersRef.current[slot] = window.setTimeout(() => {
            delete spinnerTimersRef.current[slot];
            setSpinnerDue((prev) => (prev[slot] ? prev : { ...prev, [slot]: true }));
          }, AUTO_REVIEW_SPINNER_GATE_MS);
        }
      } else if (timer != null) {
        window.clearTimeout(timer);
        delete spinnerTimersRef.current[slot];
      }
    }
  }, [autoBusy, highlightBusy, tldrBusy, busy]);

  // Source|Preview: a file whose NEW version renders (md/html/svg via the
  // file explorer's own decision) gains the surface control; the preview body
  // streams in through the explorer's file-read RPC and renderers.
  const previewInfo: ReviewPreviewInfo | null = useMemo(() => {
    if (!selectedFile || !snapshot.snapshotId) return null;
    const kind = reviewPreviewKind(selectedFile);
    if (!kind) return null;
    const state = previewStates[selectedFile.path] ?? {
      loading: false,
      error: null,
      text: null,
      truncated: false,
    };
    return { kind, ...state };
  }, [previewStates, selectedFile, snapshot.snapshotId]);

  useEffect(() => {
    const path = selectedPath;
    if (!previewSurface || !previewInfo || !path || !owningSessionIdRef.current) return;
    if (previewInfo.text != null || previewInfo.loading || previewInfo.error != null) {
      if (previewInfo.error != null) setPreviewSurface(false);
      return;
    }
    const seq = (previewSeqRef.current[path] ?? 0) + 1;
    previewSeqRef.current[path] = seq;
    setPreviewStates((prev) => ({
      ...prev,
      [path]: { loading: true, error: null, text: null, truncated: false },
    }));
    // Staleness is owned by the per-path seq (a newer fetch for the same
    // path bumps it): the effect re-runs on the loading latch, and the
    // in-flight read must survive that transition.
    sendCommand({
      type: 'file_explorer_read',
      sessionId: owningSessionIdRef.current,
      path,
      cursor: 0,
      maxLines: FILE_EXPLORER_HARD_MAX_LINES,
    })
      .then((data) => {
        if (!aliveRef.current || previewSeqRef.current[path] !== seq) return;
        const page = normalizeRead(data);
        if (!page || page.path !== path) {
          throw new Error('Invalid file preview response');
        }
        setPreviewStates((prev) => ({
          ...prev,
          [path]: {
            loading: false,
            error: null,
            text: page.lines.join('\n'),
            truncated: page.truncated || page.nextCursor != null,
          },
        }));
      })
      .catch((err: unknown) => {
        if (!aliveRef.current || previewSeqRef.current[path] !== seq) return;
        // Fail closed: a failed/unavailable read disables Preview and falls
        // back to Source; business errors surface bounded inline.
        setPreviewStates((prev) => ({
          ...prev,
          [path]: {
            loading: false,
            error: shouldRenderReviewError(err)
              ? err instanceof Error
                ? err.message
                : String(err)
              : null,
            text: null,
            truncated: false,
          },
        }));
        setPreviewSurface(false);
      });
  }, [previewInfo, previewSurface, selectedPath, sendCommand]);

  let confirmMessage = '';
  let confirmPrimaryLabel = '';
  if (confirmAction?.kind === 'close') {
    confirmMessage = formatReviewCloseWarning(hasInternalBusy, confirmAction.draftCount) ?? '';
    confirmPrimaryLabel = 'Close panel';
  } else if (confirmAction?.kind === 'refresh') {
    confirmMessage = formatReviewRefreshWarning(hasInternalBusy, confirmAction.draftCount) ?? '';
    confirmPrimaryLabel = 'Refresh now';
  }

  return (
    <section
      id="code-review-panel"
      ref={panelRef}
      className="code-review-panel"
      role="dialog"
      aria-modal="true"
      aria-label="Code review"
      aria-busy={globalBusy}
      onKeyDown={handlePanelKeyDown}
    >
      <div
        className={`code-review${treeCollapsed ? ' code-review--tree-collapsed' : ''}${focusMode ? ' code-review--focus-mode' : ''}`}
        style={{
          ['--code-review-thread-width' as string]: `${threadWidth}px`,
          ['--code-review-files-width' as string]: `${filesWidth}px`,
        }}
      >
        <div className="code-review__head">
          <span className="code-review__title">Code review</span>
          <span className="code-review__label" title={safeText(snapshot.comparisonLabel)}>
            {safeText(snapshot.comparisonLabel || 'working tree')}
          </span>
          {totals !== null && (
            <span className="code-review__totals" aria-label="diff totals">
              {totals}
            </span>
          )}
          {activeRepliesLabel && (
            <span className="code-review__streaming" role="status">
              <span className="code-review__streaming-dot" aria-hidden="true" />
              {activeRepliesLabel}
            </span>
          )}
          {autoView.progress !== null && (
            <span className="code-review__totals code-review__auto-progress" role="status">
              {autoView.progress}
            </span>
          )}
          {highlightView.progress !== null && (
            <span className="code-review__totals code-review__highlight-progress" role="status">
              {highlightView.progress}
            </span>
          )}
          {tldrView.progress !== null && (
            <span className="code-review__totals code-review__tldr-progress" role="status">
              {tldrView.progress}
            </span>
          )}
          <div className="code-review__actions">
            <span
              className="code-review__model"
              title={
                reviewModelLabel(reviewModel)
                  ? `Review reply model · ${reviewModelLabel(reviewModel)}${reviewModelPending(reviewModel) ? ' (switching…)' : ''}`
                  : 'Review reply model (code_review_set_model)'
              }
            >
              <select
                className="code-review__model-select"
                aria-label="Review reply model"
                value={reviewModelValue}
                disabled={models.length === 0}
                onChange={(e) => onReviewModelChange(e.target.value)}
              >
                {reviewModelValue === '' && snapshot.reviewModelLabel && (
                  <option value="">{snapshot.reviewModelLabel}</option>
                )}
                {modelGroups.map((group) => (
                  <optgroup key={group.provider} label={group.provider}>
                    {group.models.map((m) => (
                      <option key={m.value} value={m.value}>
                        {m.label}
                      </option>
                    ))}
                  </optgroup>
                ))}
              </select>
            </span>
            <select
              className="code-review__auto-lang"
              aria-label="Auto review reply language"
              title={`Reply language · ${langDefaultOption.label}`}
              value={autoLangValue}
              onChange={(e) => setAutoLanguagePicked(e.target.value)}
            >
              <option value={langDefaultOption.tag}>{langDefaultOption.label}</option>
              {langOptions.map((option) => (
                <option key={option.tag} value={option.tag}>
                  {option.label}
                </option>
              ))}
            </select>
            <button
              type="button"
              id="code-review-auto-review"
              className={`code-review__action${autoBusy ? ' code-review__action--stop' : ''}`}
              disabled={globalBusy}
              onClick={() => (autoBusy ? stopOperation('autoReview') : runAutoReview())}
              title={
                autoBusy
                  ? 'Stop the Auto review batch'
                  : 'Run the two-stage automatic review (triage, then deep review of core hunks)'
              }
              aria-busy={autoBusy}
            >
              {autoBusy && spinnerDue.autoReview ? (
                <>
                  <span className="code-review__spinner" aria-hidden="true" />
                  ■ Stop
                </>
              ) : (
                <>
                  <span className="code-review__action-glyph" aria-hidden="true">◐</span>
                  Auto Review
                </>
              )}
            </button>
            <button
              type="button"
              id="code-review-highlight"
              className={`code-review__action${highlightBusy ? ' code-review__action--stop' : ''}`}
              disabled={globalBusy}
              onClick={() => (highlightBusy ? stopOperation('highlight') : runHighlight())}
              title={
                highlightBusy
                  ? 'Stop the Highlight run'
                  : 'Identify key changes in the full diff (one model call)'
              }
              aria-busy={highlightBusy}
            >
              {highlightBusy && spinnerDue.highlight ? (
                <>
                  <span className="code-review__spinner" aria-hidden="true" />
                  ■ Stop
                </>
              ) : (
                <>
                  <span className="code-review__action-glyph" aria-hidden="true">✦</span>
                  Highlight
                </>
              )}
            </button>
            <button
              type="button"
              id="code-review-tldr"
              className={`code-review__action${tldrBusy ? ' code-review__action--stop' : ''}`}
              disabled={globalBusy}
              onClick={() => (tldrBusy ? stopOperation('tldr') : runTldr())}
              title={tldrBusy ? 'Stop the TLDR summary' : 'Summarize the current pinned diff'}
              aria-busy={tldrBusy}
            >
              {tldrBusy && spinnerDue.tldr ? (
                <>
                  <span className="code-review__spinner" aria-hidden="true" />
                  ■ Stop
                </>
              ) : (
                <>
                  <span className="code-review__action-glyph" aria-hidden="true">¶</span>
                  TL;DR
                </>
              )}
            </button>
            <button
              type="button"
              className={`code-review__action${busy === 'refresh' ? ' is-refreshing' : ''}`}
              disabled={globalBusy}
              onClick={requestRefresh}
              title="Reload the diff snapshot"
            >
              {spinnerDue.refresh ? (
                <span className="code-review__spinner" aria-hidden="true" />
              ) : (
                <span className="code-review__action-glyph" aria-hidden="true">↻</span>
              )}
              Refresh
            </button>
            <div className="code-review__window-controls">
              <button
                type="button"
                id="code-review-tree-toggle"
                className="code-review__tree-toggle"
                aria-pressed={treeCollapsed}
                aria-label={treeCollapsed ? 'Expand file tree' : 'Collapse file tree'}
                title={treeCollapsed ? 'Expand file tree' : 'Collapse file tree'}
                onClick={() => setTreeCollapsed((prev) => !prev)}
              >
                ▾
              </button>
              <button
                type="button"
                id="code-review-focus-toggle"
                className={`code-review__focus-toggle${focusMode ? ' is-active' : ''}`}
                aria-pressed={focusMode}
                aria-label={focusMode ? 'Exit focus mode' : 'Focus mode: show only hunk review conversation'}
                title={focusMode ? 'Exit focus mode' : 'Focus mode: show only hunk review conversation'}
                onClick={() => setFocusMode((prev) => !prev)}
              >
                ⛶
              </button>
              <button
                type="button"
                className="code-review__close panel-close"
                onClick={requestClose}
                title="Close code review"
                aria-label="Close code review panel"
              >
                ✕
              </button>
            </div>
          </div>
        </div>

        {confirmAction && (
          <CodeReviewConfirmDialog
            message={confirmMessage}
            primaryLabel={confirmPrimaryLabel}
            onConfirm={confirmPrimary}
            onCancel={cancelConfirm}
          />
        )}

        {isNarrow && (
          <div className="code-review__tab-bar" aria-label="Review sections">
            <button
              type="button"
              className={`code-review__tab${mobileTab === 'files' ? ' is-active' : ''}`}
              aria-pressed={mobileTab === 'files'}
              onClick={() => setMobileTab('files')}
            >
              Files
            </button>
            <button
              type="button"
              className={`code-review__tab${mobileTab === 'diff' ? ' is-active' : ''}`}
              aria-pressed={mobileTab === 'diff'}
              onClick={() => setMobileTab('diff')}
            >
              Diff
            </button>
            <button
              type="button"
              className={`code-review__tab${mobileTab === 'thread' ? ' is-active' : ''}`}
              aria-pressed={mobileTab === 'thread'}
              onClick={() => setMobileTab('thread')}
            >
              Thread
            </button>
          </div>
        )}

        <div className={`code-review__body${busy === 'open' ? ' is-loading' : ''}`}>
          {busy === 'open' && (
            <div className="code-review__loader" role="status" aria-label="Loading diff" />
          )}

          <CodeReviewFileList
            files={filesForList}
            tree={fileTree}
            rows={rows}
            threads={snapshot.threads}
            autoByFile={autoByFile}
            autoByDir={autoByDir}
            selectedPath={selectedPath}
            fileQuery={fileQuery}
            busy={globalBusy ? busy : null}
            isHidden={isNarrow && mobileTab !== 'files'}
            untrackedNotShown={snapshot.untrackedNotShown}
            rowButtonRefs={rowButtonRefs}
            onFileQueryChange={setFileQuery}
            onSelectFile={selectFile}
            onToggleDir={toggleTreeDir}
            onFileListKeyDown={handleFileListKeyDown}
          />

          {!isNarrow && (
            <div
              className="code-review__files-resizer"
              role="separator"
              aria-orientation="vertical"
              aria-label="Resize file list column"
              aria-valuemin={CODE_REVIEW_FILES_WIDTH_MIN}
              aria-valuemax={CODE_REVIEW_FILES_WIDTH_MAX}
              aria-valuenow={filesWidth}
              tabIndex={0}
              onPointerDown={onFilesResizePointerDown}
              onPointerMove={onFilesResizePointerMove}
              onPointerUp={onFilesResizePointerUp}
              onPointerCancel={onFilesResizePointerUp}
              onKeyDown={onFilesResizeKeyDown}
            />
          )}

          <CodeReviewDiffPane
            selectedFile={selectedFile}
            loadedDiff={loadedDiff}
            windowLimit={windowLimit}
            filesCount={snapshot.files.length}
            snapshotId={snapshot.snapshotId}
            threads={snapshot.threads}
            selectedHunkKey={selectedHunkKey}
            collapsedHunks={collapsedHunks}
            isHidden={isNarrow && mobileTab !== 'diff'}
            isLoadingMore={loadState.loading}
            loadError={loadState.error}
            onNearBottom={onDiffNearBottom}
            onRetry={retryDiffLoad}
            untrackedNotShown={snapshot.untrackedNotShown}
            onSelectHunk={selectHunk}
            onNavigateHunk={navigateHunk}
            onToggleHunkCollapsed={toggleHunkCollapsed}
            gapStates={gapStore[selectedPath]?.snapshotId === snapshot.snapshotId ? gapStore[selectedPath].states : {}}
            gapLines={gapStore[selectedPath]?.snapshotId === snapshot.snapshotId ? gapStore[selectedPath].lines : {}}
            oldLineCount={gapStore[selectedPath]?.snapshotId === snapshot.snapshotId ? gapStore[selectedPath].oldLineCount : 0}
            newLineCount={gapStore[selectedPath]?.snapshotId === snapshot.snapshotId ? gapStore[selectedPath].newLineCount : 0}
            expandingGapId={expandingGapId}
            onExpandGap={(gap, from, take) => { void expandGap(gap, from, take); }}
            surface={previewSurface ? 'preview' : 'source'}
            onSurfaceChange={togglePreviewSurface}
            preview={previewInfo}
            reviewResults={snapshot.reviewFindings}
            selectedFinding={selectedFinding}
            onPreviousIssue={() => navigateIssue('previous')}
            onNextIssue={() => navigateIssue('next')}
            issueIndex={issueIndex}
            issueCount={orderedFindings.length}

          />

          {!isNarrow && (
            <div
              className="code-review__thread-resizer"
              role="separator"
              aria-orientation="vertical"
              aria-label="Resize comment column"
              aria-valuemin={CODE_REVIEW_THREAD_WIDTH_MIN}
              aria-valuemax={CODE_REVIEW_THREAD_WIDTH_MAX}
              aria-valuenow={threadWidth}
              tabIndex={0}
              onPointerDown={onThreadResizePointerDown}
              onPointerMove={onThreadResizePointerMove}
              onPointerUp={onThreadResizePointerUp}
              onPointerCancel={onThreadResizePointerUp}
              onKeyDown={onThreadResizeKeyDown}
            />
          )}

          <CodeReviewThreadDock
            selectedIdentity={selectedIdentity}
            acceptedFollow={acceptedFollow}
            selectedThread={selectedThread && threadLatestUserIsAuto(selectedThread)
              ? { ...selectedThread, error: null }
              : selectedThread}
            selectedDraft={selectedDraft}
            composerKey={selectedHunkKey}
            composerRef={setComposerRef}
            isNarrow={isNarrow}
            isHidden={isNarrow && mobileTab !== 'thread'}
            busy={threadBusy}
            sessionId={sessionId}
            snapshotId={snapshot.snapshotId}
            reviewResults={snapshot.reviewFindings}
            selectedFinding={selectedFinding}
            onSelectFinding={selectFindingIndex}
            onBackToDiff={() => setMobileTab('diff')}

            onRefresh={requestRefresh}
            onComposerInput={onComposerInput}
            onDraftCommit={commitSelectedDraft}
            onComposerKeyDown={handleComposerKeyDown}
            onSubmitComment={submitComment}
            onAbort={abort}
            attachments={selectedAttachments}
            attachmentAccept={imageAttachmentAccept()}
            isRecording={recording}
            isTranscribing={transcribing}
            onAttachFiles={onAttachFiles}
            onRemoveAttachment={onRemoveAttachment}
            onToggleRecording={onToggleRecording}
          />
        {tldrOpen && (
          <CodeReviewTldrPane
            markdown={tldrPaneMarkdown}
            state={snapshot.tldrState}
            sessionId={sessionId ?? undefined}
            onStop={() => stopOperation('tldr')}
            onClose={() => setTldrOpen(false)}
          />
        )}
        </div>
      </div>
    </section>
  );
}
