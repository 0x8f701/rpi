// Code-review fullscreen workspace — file list + selected file diff + per-hunk
// comment threads with streamed/polled replies. RPC surface:
//   code_review_open | snapshot | refresh | comment | abort | close
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
  FILE_DIFF_MAX_UI_LINES,
  FILE_LOAD_CHUNK_LINES,
  FILE_PAGE_REQUEST_LINES,
  FILE_RENDER_SOFT_CAP,
  CODE_REVIEW_THREAD_WIDTH_MAX,
  CODE_REVIEW_THREAD_WIDTH_MIN,
  type CodeReviewSnapshot,
  type DiffFile,
  type DiffHunk,
  type FileTree,
  type HunkIdentity,
  type LoadedDiff,
  type ReviewThread,
  appendFileDiffPage,
  buildCodeReviewAbortPayload,
  buildFileTree,
  emptyCodeReviewSnapshot,
  findThreadForHunk,
  formatActiveRepliesLabel,
  hunkIdentityFor,
  hunkKeyFor,
  initLoadedDiff,
  normalizeCodeReviewSnapshot,
  normalizeFileDiffPage,
  planDiffWindow,
  readStoredCodeReviewThreadWidth,
  stepCodeReviewThreadWidth,
  threadIsStreaming,
  treeFileIndexAt,
  treeFilterRows,
  treeKeyboardAction,
  treeToggleCollapse,
  writeStoredCodeReviewThreadWidth,
} from '../codeReview';
import { CodeReviewConfirmDialog } from './CodeReviewConfirmDialog';
import { CodeReviewDiffPane } from './CodeReviewDiffPane';
import { CodeReviewFileList } from './CodeReviewFileList';
import { CodeReviewThreadDock } from './CodeReviewThreadDock';

export interface CodeReviewOpenArgs {
  from?: string;
  to?: string;
}

interface CodeReviewPanelProps {
  /** sendCommand bound to the App websocket (resolves with RPC data). */
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>;
  /**
   * Owning session id captured at open time. Stamped on EVERY code_review_*
   * command (including unmount close) so A→B cleanup cannot target B.
   */
  sessionId: string | null;
  /** Revisions captured when `/code-review [from to]` opened the panel. */
  openArgs: CodeReviewOpenArgs;
  onClose: () => void;
}

const POLL_MS = 1500;
const NARROW_QUERY = '(max-width: 900px)';

type MobileTab = 'files' | 'diff' | 'thread';
type ConfirmAction = { kind: 'close' } | { kind: 'refresh' };

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
  onClose,
}: CodeReviewPanelProps) {
  const [snapshot, setSnapshot] = useState<CodeReviewSnapshot>(() => emptyCodeReviewSnapshot());
  const [selectedPath, setSelectedPath] = useState<string>('');
  /** Explicit hunk selection only; null = nothing selected (never auto-picks). */
  const [selectedHunkKey, setSelectedHunkKey] = useState<string | null>(null);
  /** Per-hunk comment drafts keyed by hunkKey; survive hunk switches. */
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState<string | null>('open');
  const [localError, setLocalError] = useState<string | null>(null);
  const [pollFailed, setPollFailed] = useState(false);
  const [fileQuery, setFileQuery] = useState('');
  const [isNarrow, setIsNarrow] = useState(() => window.matchMedia(NARROW_QUERY).matches);
  const [mobileTab, setMobileTab] = useState<MobileTab>('files');
  const [confirmAction, setConfirmAction] = useState<ConfirmAction | null>(null);
  const [collapsedHunks, setCollapsedHunks] = useState<Set<string>>(() => new Set());
  const [hasUserSelectedFile, setHasUserSelectedFile] = useState(false);
  const [fileFocusIndex, setFileFocusIndex] = useState(-1);
  /** Collapsible path tree over snapshot.files (collapse set preserved across polls). */
  const [fileTree, setFileTree] = useState<FileTree>(() => buildFileTree([]));
  /** Per-path accumulated diff (snapshot hunk lines + appended backend pages). */
  const [loadedDiffs, setLoadedDiffs] = useState<Record<string, LoadedDiff>>({});
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
  const aliveRef = useRef(true);
  // Capture the owning session at mount so unmount close still targets A even
  // if the parent re-rendered with a newer sessionIdRef before cleanup ran.
  const owningSessionIdRef = useRef(sessionId);
  const panelRef = useRef<HTMLElement | null>(null);
  const rowButtonRefs = useRef<Array<HTMLButtonElement | null>>([]);
  const previousFocusRef = useRef<HTMLElement | null>(null);
  const settledOnceRef = useRef(false);
  const snapshotRequestSeqRef = useRef(0);
  /** Request seq per path: bumping discards in-flight code_review_file_diff calls. */
  const loadSeqRef = useRef<Record<string, number>>({});
  /** Per-path in-flight fetch promises: concurrent callers share one fetch. */
  const fetchPromisesRef = useRef<Record<string, Promise<LoadedDiff | null>>>({});
  const loadedDiffsRef = useRef<Record<string, LoadedDiff>>({});
  const selectedPathRef = useRef('');
  /** Drag start snapshot for the desktop thread-column resizer. */
  const resizeDragRef = useRef<{ startX: number; startWidth: number } | null>(null);
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

  const applyData = useCallback((data: unknown) => {
    const next = normalizeCodeReviewSnapshot(data);
    setSnapshot(next);
    setLocalError(null);
    setPollFailed(false);
    return next;
  }, []);

  const openReview = useCallback(() => {
    const requestSeq = ++snapshotRequestSeqRef.current;
    setBusy('open');
    setLocalError(null);
    const command: Record<string, unknown> = { type: 'code_review_open' };
    if (openArgs.from && openArgs.to) {
      command.from = openArgs.from;
      command.to = openArgs.to;
    }
    sendCommand(reviewCommand(command))
      .then((data) => {
        if (!aliveRef.current || requestSeq !== snapshotRequestSeqRef.current) return;
        applyData(data);
      })
      .catch((err: unknown) => {
        if (!aliveRef.current || requestSeq !== snapshotRequestSeqRef.current) return;
        const message = err instanceof Error ? err.message : String(err);
        setLocalError(message);
        setSnapshot(emptyCodeReviewSnapshot(message));
      })
      .finally(() => {
        if (aliveRef.current && requestSeq === snapshotRequestSeqRef.current) setBusy(null);
      });
  }, [applyData, openArgs.from, openArgs.to, reviewCommand, sendCommand]);

  // Open on mount; close the backend controller on unmount (session switch /
  // panel close). Closing the panel also calls onClose from the UI button,
  // which unmounts us — the cleanup still runs code_review_close with the
  // OWNING sessionId captured in the ref (not the active sessionIdRef). The
  // modal shell restores keyboard focus to the element that had it before.
  useEffect(() => {
    aliveRef.current = true;
    previousFocusRef.current =
      document.activeElement instanceof HTMLElement ? document.activeElement : null;
    openReview();
    const closeSid = owningSessionIdRef.current;
    return () => {
      aliveRef.current = false;
      const closeCmd: Record<string, unknown> = { type: 'code_review_close' };
      if (closeSid) closeCmd.sessionId = closeSid;
      sendCommand(closeCmd).catch(() => {});
      const previous = previousFocusRef.current;
      if (previous && previous.isConnected) {
        previous.focus({ preventScroll: true });
      }
    };
  }, [openReview, sendCommand]);

  // Poll snapshot only while the panel is mounted. Always poll while open so
  // threads catch up; 1.5s cadence is the contract (streaming included).
  useEffect(() => {
    if (busy === 'open') return;
    const poll = () => {
      const requestSeq = snapshotRequestSeqRef.current;
      sendCommand(reviewCommand({ type: 'code_review_snapshot' }))
        .then((data) => {
          if (!aliveRef.current || requestSeq !== snapshotRequestSeqRef.current) return;
          applyData(data);
        })
        .catch(() => {
          // Transient poll failures surface a stale-data notice; the last
          // good snapshot stays visible.
          if (aliveRef.current) setPollFailed(true);
        });
    };
    poll();
    const timer = window.setInterval(poll, POLL_MS);
    return () => window.clearInterval(timer);
  }, [applyData, busy, reviewCommand, sendCommand]);

  // Track the desktop/mobile breakpoint so the layout can re-flow live.
  useEffect(() => {
    const mq = window.matchMedia(NARROW_QUERY);
    const onChange = (event: MediaQueryListEvent) => setIsNarrow(event.matches);
    mq.addEventListener('change', onChange);
    return () => mq.removeEventListener('change', onChange);
  }, []);

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
  const draftCount = useMemo(
    () => Object.values(drafts).filter((text) => text.trim().length > 0).length,
    [drafts],
  );
  // Aggregate activity (close/refresh confirm + header badge). Composer/abort
  // gate only on the selected thread so other hunks can stream concurrently.
  const hasActiveStream = snapshot.isStreaming || snapshot.activeCount > 0;
  const selectedIsStreaming = threadIsStreaming(selectedThread);
  const activeRepliesLabel = formatActiveRepliesLabel(snapshot.activeCount);

  const selectedView = selectedPath ? diffViews[selectedPath] : undefined;
  const windowLimit =
    selectedView && selectedView.snapshotId === snapshot.snapshotId
      ? selectedView.window
      : 0;
  const diffPlan = useMemo(() => {
    if (!loadedDiff || loadedDiff.snapshotId !== snapshot.snapshotId) return null;
    return planDiffWindow(loadedDiff, windowLimit);
  }, [loadedDiff, snapshot.snapshotId, windowLimit]);
  const loadState = selectedPath
    ? (loadStates[selectedPath] ?? { loading: false, error: null })
    : { loading: false, error: null };

  // Rebuild the tree when the snapshot's file set changes, preserving the
  // user's collapse state for directories that still exist.
  useEffect(() => {
    setFileTree((prev) => {
      const next = buildFileTree(snapshot.files);
      if (prev.collapsed.size === 0) return next;
      const valid = new Set(next.nodes.map((n) => n.id));
      const keep = new Set<string>();
      for (const id of prev.collapsed) {
        if (valid.has(id)) keep.add(id);
      }
      next.collapsed = keep;
      return next;
    });
  }, [snapshot.files]);

  // Keep the selected-path ref in sync for async stale guards.
  useEffect(() => {
    selectedPathRef.current = selectedPath;
  }, [selectedPath]);

  // Keep a ref mirror of the loaded-diff map so async load flows can read the
  // latest value without re-creating callbacks on every state change.
  useEffect(() => {
    loadedDiffsRef.current = loadedDiffs;
  }, [loadedDiffs]);

  // Initialize (or re-initialize on refresh) the selected file's loaded diff
  // and its visible window. A stale loaded diff is replaced atomically with
  // the fresh snapshot's lines; the window restarts at the soft cap.
  useEffect(() => {
    if (!selectedPath) return;
    const file = snapshot.files.find((f) => f.path === selectedPath);
    if (!file) return;
    const current = loadedDiffsRef.current[selectedPath];
    if (current && current.snapshotId === snapshot.snapshotId && current.path === selectedPath) {
      return;
    }
    const next = initLoadedDiff(snapshot.snapshotId, file, snapshot.truncated);
    loadedDiffsRef.current = { ...loadedDiffsRef.current, [selectedPath]: next };
    setLoadedDiffs(loadedDiffsRef.current);
    setDiffViews((views) => ({
      ...views,
      [selectedPath]: {
        snapshotId: snapshot.snapshotId,
        window: Math.min(FILE_RENDER_SOFT_CAP, next.lines.length),
      },
    }));
    setLoadStates((states) => ({ ...states, [selectedPath]: { loading: false, error: null } }));
    // Invalidate any in-flight page request for this path (old snapshot).
    loadSeqRef.current[selectedPath] = (loadSeqRef.current[selectedPath] ?? 0) + 1;
  }, [selectedPath, snapshot.files, snapshot.snapshotId, snapshot.truncated]);

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

  // Drop drafts for hunks that no longer exist in the snapshot (a refresh can
  // change content hashes); drafts for surviving hunks are kept.
  useEffect(() => {
    const valid = new Set<string>();
    for (const file of snapshot.files) {
      for (const hunk of file.hunks) valid.add(hunkKeyFor(file, hunk));
    }
    setDrafts((prev) => {
      const keys = Object.keys(prev);
      if (keys.every((key) => valid.has(key))) return prev;
      const next: Record<string, string> = {};
      for (const key of keys) {
        if (valid.has(key)) next[key] = prev[key];
      }
      return next;
    });
  }, [snapshot.files]);

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
    const requestSeq = ++snapshotRequestSeqRef.current;
    setBusy('refresh');
    setLocalError(null);
    sendCommand(reviewCommand({ type: 'code_review_refresh' }))
      .then((data) => {
        if (!aliveRef.current || requestSeq !== snapshotRequestSeqRef.current) return;
        applyData(data);
      })
      .catch((err: unknown) => {
        if (!aliveRef.current || requestSeq !== snapshotRequestSeqRef.current) return;
        setLocalError(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        if (aliveRef.current && requestSeq === snapshotRequestSeqRef.current) setBusy(null);
      });
  }, [applyData, reviewCommand, sendCommand]);

  const abort = useCallback(() => {
    if (!selectedIdentity) return;
    // Capture identity at click time so a later selection change cannot
    // apply a stale abort response against a different hunk.
    const identity = selectedIdentity;
    const requestSeq = ++snapshotRequestSeqRef.current;
    setBusy('abort');
    setLocalError(null);
    sendCommand(reviewCommand(buildCodeReviewAbortPayload(identity)))
      .then((data) => {
        if (!aliveRef.current || requestSeq !== snapshotRequestSeqRef.current) return;
        applyData(data);
      })
      .catch((err: unknown) => {
        if (!aliveRef.current || requestSeq !== snapshotRequestSeqRef.current) return;
        setLocalError(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        if (aliveRef.current && requestSeq === snapshotRequestSeqRef.current) setBusy(null);
      });
  }, [applyData, reviewCommand, selectedIdentity, sendCommand]);

  const retry = useCallback(() => {
    if (snapshot.snapshotId) refresh();
    else openReview();
  }, [openReview, refresh, snapshot.snapshotId]);

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

  const selectFile = useCallback(
    (path: string, visibleIndex: number) => {
      // Commit the outgoing hunk's live text before the composer remounts.
      commitSelectedDraft();
      setSelectedPath(path);
      setSelectedHunkKey(null);
      setHasUserSelectedFile(true);
      setFileFocusIndex(visibleIndex);
      if (isNarrow) setMobileTab('diff');
    },
    [commitSelectedDraft, isNarrow],
  );

  const toggleTreeDir = useCallback((nodeIndex: number, visibleIndex: number) => {
    setFileTree((prev) => treeToggleCollapse(prev, nodeIndex));
    setFileFocusIndex(visibleIndex);
  }, []);

  const selectHunk = useCallback(
    (key: string) => {
      // Commit the outgoing hunk's live text before the composer remounts.
      commitSelectedDraft();
      setSelectedHunkKey(key);
      // Selecting a collapsed hunk reveals it so the user sees what they
      // picked.
      setCollapsedHunks((prev) => {
        if (!prev.has(key)) return prev;
        const next = new Set(prev);
        next.delete(key);
        return next;
      });
      if (isNarrow) setMobileTab('thread');
    },
    [commitSelectedDraft, isNarrow],
  );

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
    // The composer is uncontrolled; read the live text at submit time. A
    // streaming reply no longer blocks submit — the backend queues (bounded)
    // on the same hunk, so consecutive comments are allowed.
    const text = input.value.trim();
    if (!text) return;
    // Capture identity + element + raw text at submit time so a later hunk
    // switch cannot clear the wrong draft or apply a stale response.
    const identity = selectedIdentity;
    const submittedEl = input;
    const submittedText = input.value;
    const requestSeq = ++snapshotRequestSeqRef.current;
    setBusy('comment');
    setLocalError(null);
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
      }),
    )
      .then((data) => {
        if (!aliveRef.current || requestSeq !== snapshotRequestSeqRef.current) return;
        applyData(data);
        // Clear only when the composer still holds exactly the submitted
        // text on the same hunk element; anything typed after submit must
        // survive (no stale-draft loss under async responses).
        const live = composerRef.current;
        if (live === submittedEl && live.value === submittedText) {
          live.value = '';
          flushComposerResize(live);
          setDrafts((prev) => {
            if (!(key in prev)) return prev;
            const next = { ...prev };
            delete next[key];
            return next;
          });
        }
      })
      .catch((err: unknown) => {
        if (!aliveRef.current || requestSeq !== snapshotRequestSeqRef.current) return;
        setLocalError(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        if (aliveRef.current && requestSeq === snapshotRequestSeqRef.current) setBusy(null);
      });
  }, [
    applyData,
    flushComposerResize,
    reviewCommand,
    selectedHunkKey,
    selectedIdentity,
    sendCommand,
    snapshot.snapshotId,
  ]);

  // Close/refresh are guarded: with a streaming reply or unsent drafts they
  // surface an inline confirmation instead of acting (or window.confirm).
  // The live (uncommitted) composer text is committed first so the guard and
  // the confirm message count every unsent draft, typed or not.
  const requestRefresh = useCallback(() => {
    commitSelectedDraft();
    if (draftCount > 0 || hasActiveStream) {
      setConfirmAction({ kind: 'refresh' });
      return;
    }
    refresh();
  }, [commitSelectedDraft, draftCount, hasActiveStream, refresh]);

  const requestClose = useCallback(() => {
    commitSelectedDraft();
    if (draftCount > 0 || hasActiveStream) {
      setConfirmAction({ kind: 'close' });
      return;
    }
    onClose();
  }, [commitSelectedDraft, draftCount, hasActiveStream, onClose]);

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

  // Fetch backend pages until the loaded diff's backend line stream is fully
  // consumed (a terminal page with hasMore=false, or the 8 MiB per-file byte
  // cap). This makes the merged hunks complete — every hunk's descriptors and
  // line subsets arrive, so the file becomes comment-ready without any Load
  // more click. Per-path single-flight: concurrent callers (the eager effect
  // and a Load more retry) share one in-flight fetch. Stale guards: the
  // owning session is stamped by reviewCommand (session), the request carries
  // the loaded diff's snapshot id (snapshot), responses must match path +
  // cursor (per-file, via appendFileDiffPage), and a per-path request seq +
  // selected-path check discard responses that raced a refresh or a file
  // switch (request). The seq is bumped only by the init/refresh effect, so
  // an in-flight loop is never invalidated by a second fetch call.
  const fetchPagesUntil = useCallback(
    async (path: string): Promise<LoadedDiff | null> => {
      const start = loadedDiffsRef.current[path];
      if (!start) return null;
      const seq = loadSeqRef.current[path];
      const existing = fetchPromisesRef.current[path];
      if (existing) return existing;
      const promise = (async () => {
        let next = start;
        try {
          let fetchedOnce = false;
          while (!next.hunksComplete && (next.hasMoreBackend || !fetchedOnce)) {
            if (!aliveRef.current || seq !== loadSeqRef.current[path]) return null;
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
            if (!aliveRef.current || seq !== loadSeqRef.current[path]) return null;
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
            loadedDiffsRef.current = { ...loadedDiffsRef.current, [path]: next };
            setLoadedDiffs(loadedDiffsRef.current);
          }
          if (!aliveRef.current || seq !== loadSeqRef.current[path]) return null;
          return next;
        } catch (err) {
          if (!aliveRef.current || seq !== loadSeqRef.current[path]) return null;
          const message = err instanceof Error ? err.message : String(err);
          setLoadStates((prev) => ({ ...prev, [path]: { loading: false, error: message } }));
          return null;
        } finally {
          delete fetchPromisesRef.current[path];
        }
      })();
      fetchPromisesRef.current[path] = promise;
      setLoadStates((prev) => ({ ...prev, [path]: { loading: true, error: null } }));
      const result = await promise;
      if (aliveRef.current && seq === loadSeqRef.current[path]) {
        setLoadStates((prev) => ({ ...prev, [path]: { loading: false, error: null } }));
      }
      return result;
    },
    [reviewCommand, sendCommand],
  );

  /** Advance the selected file's visible window by one chunk (or to the end).
   *  Data is already eagerly fetched; this only widens the DOM window (an
   *  in-flight eager fetch is awaited via the single-flight promise). When
   *  an earlier fetch errored, Retry re-runs the fetch first. */
  const growDiffWindow = useCallback(
    async (path: string, toEnd: boolean) => {
      const loaded = loadedDiffsRef.current[path];
      if (!loaded) return;
      const currentWindow = diffViews[path]?.window ?? 0;
      const plan = planDiffWindow(loaded, currentWindow, FILE_LOAD_CHUNK_LINES, toEnd);
      const hasError = loadStates[path]?.error != null;
      if (!plan.canLoadMore && !hasError) return;
      const fetched = await fetchPagesUntil(path);
      if (!fetched) return; // stale (dropped silently) or error already surfaced
      const target = toEnd ? plan.target : Math.max(currentWindow, plan.target);
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

  const loadMore = useCallback(() => {
    if (!selectedPath) return;
    void growDiffWindow(selectedPath, false);
  }, [growDiffWindow, selectedPath]);

  const loadFull = useCallback(() => {
    if (!selectedPath) return;
    void growDiffWindow(selectedPath, true);
  }, [growDiffWindow, selectedPath]);

  // Eager per-file fetch on selection: every non-binary selected file pulls
  // its backend pages until the line stream is fully consumed, so all hunk
  // descriptors are known and every complete hunk becomes selectable without
  // any Load more click. The window stays at the soft cap (see the init
  // effect + the zero-window bump below); loading is lazy, metadata is not.
  // Fires once per file (hunksComplete flips after the fetch) and never
  // retries on its own after an error — Retry re-runs through Load more.
  useEffect(() => {
    if (busy !== null) return;
    if (!selectedPath || !loadedDiff) return;
    if (loadedDiff.snapshotId !== snapshot.snapshotId) return;
    if (loadedDiff.hunksComplete) return;
    if (selectedFile?.binary) return;
    if (loadState.loading || loadState.error) return;
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

  // Escape + focus trap on the modal shell. Escape in the composer blurs it
  // (never closes); elsewhere it dismisses a pending confirm or requests close
  // with the draft/streaming guards.
  const handlePanelKeyDown = (e: ReactKeyboardEvent<HTMLElement>) => {
    if (e.key === 'Escape') {
      const target = e.target as HTMLElement | null;
      if (target && target.tagName === 'TEXTAREA') {
        target.blur();
        e.stopPropagation();
        return;
      }
      e.preventDefault();
      e.stopPropagation();
      if (confirmAction) {
        setConfirmAction(null);
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

  // Escape anywhere while the workspace is mounted requests close (guards
  // included). Events inside the panel are handled by the section handler.
  useEffect(() => {
    const onDocKeyDown = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return;
      if (panelRef.current && panelRef.current.contains(e.target as Node)) return;
      e.preventDefault();
      requestClose();
    };
    document.addEventListener('keydown', onDocKeyDown);
    return () => document.removeEventListener('keydown', onDocKeyDown);
  }, [requestClose]);

  const displayError = localError || snapshot.error;
  const totals = `+${snapshot.totalInsertions} / -${snapshot.totalDeletions}`;
  // open/refresh remain global busy (snapshot mutation); comment/abort are
  // scoped to the selected thread and do not freeze Refresh/file navigation.
  const globalBusy = busy === 'open' || busy === 'refresh';
  const threadBusy = busy === 'comment' || busy === 'abort' ? busy : null;

  let confirmMessage = '';
  let confirmPrimaryLabel = '';
  if (confirmAction?.kind === 'close') {
    const reasons: string[] = [];
    if (hasActiveStream) {
      reasons.push(
        snapshot.activeCount > 1
          ? `${snapshot.activeCount} review replies are streaming`
          : 'a review reply is streaming',
      );
    }
    if (draftCount > 0) reasons.push(`${draftCount} unsent draft${draftCount === 1 ? '' : 's'}`);
    confirmMessage = `Close anyway? ${reasons.join(' and ')} will be discarded.`;
    confirmPrimaryLabel = 'Close panel';
  } else if (confirmAction?.kind === 'refresh') {
    confirmMessage =
      draftCount > 0
        ? `Refresh anyway? Your ${draftCount} draft${draftCount === 1 ? '' : 's'} will be kept, but threads may go stale.`
        : snapshot.activeCount > 1
          ? `Refresh anyway? ${snapshot.activeCount} review replies are streaming.`
          : 'Refresh anyway? A review reply is streaming.';
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
        className="code-review"
        style={{ ['--code-review-thread-width' as string]: `${threadWidth}px` }}
      >
        <div className="code-review__head">
          <span className="code-review__title">Code review</span>
          <span className="code-review__label" title={safeText(snapshot.comparisonLabel)}>
            {safeText(snapshot.comparisonLabel || 'working tree')}
          </span>
          <span className="code-review__totals" aria-label="diff totals">
            {totals}
            {snapshot.truncated ? ' · truncated' : ''}
          </span>
          {activeRepliesLabel && (
            <span className="code-review__streaming" role="status">
              <span className="code-review__streaming-dot" aria-hidden="true" />
              {activeRepliesLabel}
            </span>
          )}
          <div className="code-review__actions">
            <button
              type="button"
              className="code-review__action"
              disabled={globalBusy}
              onClick={requestRefresh}
              title="Reload the diff snapshot"
            >
              Refresh
            </button>
            <button
              type="button"
              className="code-review__close panel-close"
              onClick={requestClose}
              title="Close code review"
              aria-label="Close code review panel"
            >
              ×
            </button>
          </div>
        </div>

        {busy === 'refresh' && (
          <div className="code-review__busy" role="status">
            Refreshing…
          </div>
        )}
        {displayError && (
          <div className="code-review__error" role="alert">
            {safeText(displayError)}
            <span className="code-review__error-actions">
              <button type="button" className="code-review__link" onClick={retry}>
                Retry
              </button>
            </span>
          </div>
        )}
        {pollFailed && !globalBusy && !displayError && (
          <div className="code-review__banner code-review__banner--stale" role="status">
            Snapshot update failed — showing the last good data.{' '}
            <button type="button" className="code-review__link" onClick={refresh}>
              Refresh now
            </button>
          </div>
        )}

        {confirmAction && (
          <CodeReviewConfirmDialog
            message={confirmMessage}
            primaryLabel={confirmPrimaryLabel}
            onConfirm={confirmPrimary}
            onCancel={() => setConfirmAction(null)}
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
            <div className="code-review__loader" role="status">
              Loading diff…
            </div>
          )}

          <CodeReviewFileList
            files={snapshot.files}
            tree={fileTree}
            rows={rows}
            threads={snapshot.threads}
            selectedPath={selectedPath}
            fileQuery={fileQuery}
            busy={globalBusy ? busy : null}
            isHidden={isNarrow && mobileTab !== 'files'}
            rowButtonRefs={rowButtonRefs}
            onFileQueryChange={setFileQuery}
            onSelectFile={selectFile}
            onToggleDir={toggleTreeDir}
            onFileListKeyDown={handleFileListKeyDown}
          />

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
            canLoadMore={diffPlan?.canLoadMore ?? false}
            hardCapped={diffPlan?.hardCapped ?? false}
            moreCount={
              diffPlan && loadedDiff
                ? Math.max(
                    0,
                    Math.max(loadedDiff.lines.length, loadedDiff.totalLines) -
                      Math.min(windowLimit, Math.max(loadedDiff.lines.length, loadedDiff.totalLines)),
                  )
                : 0
            }
            isLoadingMore={loadState.loading}
            loadError={loadState.error}
            onSelectHunk={selectHunk}
            onToggleHunkCollapsed={toggleHunkCollapsed}
            onLoadMore={loadMore}
            onLoadFull={loadFull}
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
            selectedThread={selectedThread}
            selectedDraft={selectedDraft}
            composerKey={selectedHunkKey}
            composerRef={setComposerRef}
            isNarrow={isNarrow}
            isHidden={isNarrow && mobileTab !== 'thread'}
            isStreaming={selectedIsStreaming}
            busy={threadBusy}
            snapshotId={snapshot.snapshotId}
            onBackToDiff={() => setMobileTab('diff')}
            onRefresh={refresh}
            onComposerInput={onComposerInput}
            onDraftCommit={commitSelectedDraft}
            onComposerKeyDown={handleComposerKeyDown}
            onSubmitComment={submitComment}
            onAbort={abort}
          />
        </div>
      </div>
    </section>
  );
}
