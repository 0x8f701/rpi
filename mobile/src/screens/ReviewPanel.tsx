// Session-workspace code review — the Android port of
// crates/pi-cli/web/src/panels/CodeReviewPanel.tsx.
//
// RPC surface: code_review_open | snapshot | refresh | comment | abort | close
// | file_diff | set_model | commits | tldr | stop. Every command carries the
// session the panel was mounted for, so an A→B switch can never act on B's
// controller, and leaving the panel closes exactly the review this panel owned.
//
// Wire shapes are normalized fail-closed in ../protocol (parseReviewSnapshot
// and friends): a malformed payload renders an actionable empty state, never a
// fabricated row, and every diff/comment string is redactSecrets()'d text.
//
// Semantics mirrored from /web: hunks are NEVER auto-selected (a comment
// target exists only after the user picks one), the per-file window grows in
// soft-cap chunks from pages the review can still verify, and Stop targets the
// exact hunk identity of the in-flight reply — it never closes the review.

import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { ScrollView, Text, TextInput, TouchableOpacity, View } from 'react-native';
import Icon from 'react-native-vector-icons/MaterialIcons';
import { MarkdownBody } from '../MarkdownBody';
import { parseAvailableModels } from '../codeReviewSnapshot';
import { normalizeThinkingNewlines } from '../markdown';
import {
  CODE_REVIEW_SCOPE_USAGE,
  REVIEW_DIFF_MAX_UI_LINES,
  REVIEW_PAGE_LINES,
  appendReviewDiffPage,
  buildCodeReviewAbortPayload,
  buildCodeReviewCommentPayload,
  buildCodeReviewStopPayload,
  buildCodeReviewTldrPayload,
  codeReviewSnapshotPollMs,
  CODE_REVIEW_BATCH_POLL_MS,
  emptyReviewSnapshot,
  findReviewThread,
  initReviewLoadedDiff,
  isNoOpenCodeReviewFailure,
  parseCodeReviewCommits,
  parseCodeReviewFileDiffPage,
  parseCodeReviewScope,
  parseCodeReviewSetModelAck,
  parseCodeReviewTldrAck,
  parseReviewSnapshot,
  planReviewDiffWindow,
  reviewAggregateCategory,
  reviewCategoryLabel,
  reviewFindingAtRef,
  reviewFindingNeighbor,
  reviewFindingRowRange,
  reviewFindingCoversRow,
  reviewHunkIdentityEqual,
  reviewFindingsForHunk,
  orderReviewFindings,
  reviewHunkIdentityFor,
  reviewHunkIsReady,
  reviewHunkKeyFor,
  reviewDiffStatusText,
  reviewEmptyComparisonMessage,
  reviewStageFeedback,
  reviewModelOptionValue,
  windowedReviewHunks,
  type CodeReviewCommitWire,
  type CodeReviewScopeMode,
  type CodeReviewTldrState,
  type DiffLine,
  type ReviewFinding,
  type ReviewFindingCategory,
  type ReviewFindingEntry,
  type ReviewFindingRef,
  type ReviewHunkIdentity,
  type ReviewLoadedDiff,
  type ReviewSnapshot,
} from '../protocol';
import { redactSecrets } from '../redact';
import type { TransportTimerHandle } from '../reconnect';
import type { RunnerClient } from '../runnerClient';
import { AppBar, styles, type Palette } from '../ui';

const INVALID_SNAPSHOT = 'Invalid code-review snapshot response';
const INVALID_DIFF_PAGE = 'Invalid code_review_file_diff response';
const INVALID_TLDR_ACK = 'Invalid code_review_tldr ack response';
/** Height bound applied only while collapsed; the pane opens expanded. */
const TLDR_COMPACT_MAX_HEIGHT = 240;

/** 'probe' while the mount snapshot is in flight, 'none' without a controller,
 *  'open' once a snapshot answered. */
type ReviewPhase = 'probe' | 'none' | 'open';
type ReviewBusy = 'open' | 'refresh' | 'comment' | 'abort' | 'close' | 'tldr' | 'tldrStop' | null;

const SCOPE_MODES: Array<{ mode: CodeReviewScopeMode; label: string }> = [
  { mode: 'workingTree', label: 'Working tree' },
  { mode: 'base', label: 'Base → tree' },
  { mode: 'range', label: 'From → to' },
  { mode: 'commit', label: 'Commit' },
];
const INVALID_COMMITS = 'Invalid code_review_commits response';

/** Collapsed thinking of a review reply — the same fold the transcript uses
 *  (markdown body, thinking newlines normalized), so a review reply's
 *  reasoning renders exactly like the chat it came from. */
function ThinkingFold(props: { thinking: string; palette: Palette }): React.JSX.Element {
  const [open, setOpen] = useState(false);
  return (
    <View>
      <TouchableOpacity onPress={() => setOpen((value) => !value)} accessibilityLabel="Thinking">
        <Text style={[styles.cardMeta, { color: props.palette.muted }]}>
          {open ? '▾ Thinking' : '▸ Thinking'}
        </Text>
      </TouchableOpacity>
      {open && (
        <View style={{ padding: 8 }}>
          <MarkdownBody text={normalizeThinkingNewlines(props.thinking)} palette={props.palette} />
        </View>
      )}
    </View>
  );
}

const TLDR_STATE_LABEL: Record<CodeReviewTldrState, string> = {
  idle: 'Idle',
  running: 'Generating',
  done: 'Ready',
  failed: 'Failed',
};

/** TL;DR pane over the snapshot the parent already owns. Mounted only while
 *  open, so each open starts expanded. Collapse and expand share one markdown
 *  body (never split by headings). Stop while the generation is live, and
 *  close only unmounts this section. Mirrors the /web CodeReviewTldrPane; the
 *  parent owns snapshot identity, generation guards, and the central error
 *  surface. Mermaid fences inside the body render through the same native
 *  MarkdownBody MermaidBlock the transcript uses — the mobile renderer
 *  genuinely hosts them, so no fallback copy. */
function ReviewTldrPane(props: {
  palette: Palette;
  state: CodeReviewTldrState;
  markdown: string;
  error: string | null;
  busy: boolean;
  onStop: () => void;
  onClose: () => void;
}): React.JSX.Element {
  const [expanded, setExpanded] = useState(true);
  const palette = props.palette;
  const running = props.state === 'running';
  return (
    <View
      style={[styles.card, { backgroundColor: palette.surface, borderColor: palette.border }]}
      accessibilityLabel="Code review summary">
      <View style={{ flexDirection: 'row', alignItems: 'center', gap: 8 }}>
        <Text style={[styles.rowTitle, { color: palette.fg, flex: 1 }]}>Summary</Text>
        <Text
          style={[styles.cardMeta, { color: running ? palette.accent : palette.muted }]}
          accessibilityLabel="TLDR state">
          {running ? 'Generating…' : TLDR_STATE_LABEL[props.state]}
        </Text>
        {running ? (
          <TouchableOpacity
            style={[styles.primaryButton, { marginTop: 0, flexGrow: 0, backgroundColor: palette.error }, props.busy && styles.faded]}
            disabled={props.busy}
            onPress={props.onStop}
            accessibilityLabel="Stop summary">
            <Text style={[styles.primaryButtonText, { color: palette.bg }]}>Stop</Text>
          </TouchableOpacity>
        ) : null}
        <TouchableOpacity onPress={props.onClose} accessibilityLabel="Close summary">
          <Icon name="close" size={20} color={palette.muted} />
        </TouchableOpacity>
      </View>
      {props.error ? (
        <Text style={[styles.inlineError, { color: palette.error }]}>{redactSecrets(props.error)}</Text>
      ) : null}
      {props.markdown !== '' ? (
        <>
          <View style={expanded ? undefined : { maxHeight: TLDR_COMPACT_MAX_HEIGHT, overflow: 'hidden' }}>
            <MarkdownBody text={props.markdown} palette={palette} />
          </View>
          <TouchableOpacity
            onPress={() => setExpanded((value) => !value)}
            accessibilityLabel={expanded ? 'Collapse details' : 'Expand details'}>
            <Text style={[styles.cardMeta, { color: palette.accent }]}>
              {expanded ? 'Collapse details' : 'Expand details'}
            </Text>
          </TouchableOpacity>
        </>
      ) : null}
    </View>
  );
}
/** Row tone of a finding category: critical red, overdesign accent, naming ok. */
function findingToneColor(category: ReviewFindingCategory, palette: Palette): string {
  if (category === 'critical') return palette.error;
  if (category === 'overdesign') return palette.accent;
  return palette.ok;
}

const FINDING_TONE_RANK: Record<ReviewFindingCategory, number> = {
  critical: 2,
  overdesign: 1,
  naming: 0,
};

export function ReviewPanel(props: {
  palette: Palette;
  client: RunnerClient;
  sessionId: string;
  onClose: () => void;
  onError: (message: string) => void;
  onRegisterBackHandler?: (handler: (() => boolean) | null) => void;
}): React.JSX.Element {
  const { client, onError, palette, sessionId } = props;
  const [phase, setPhase] = useState<ReviewPhase>('probe');
  const [snapshot, setSnapshot] = useState<ReviewSnapshot | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState<ReviewBusy>('open');
  const [scopeMode, setScopeMode] = useState<CodeReviewScopeMode>('workingTree');
  const [scopeFrom, setScopeFrom] = useState('');
  const [scopeTo, setScopeTo] = useState('');
  const [scopeError, setScopeError] = useState<string | null>(null);
  const [selectedPath, setSelectedPath] = useState('');
  const [loaded, setLoaded] = useState<ReviewLoadedDiff | null>(null);
  const [windowLimit, setWindowLimit] = useState(0);
  const [diffError, setDiffError] = useState<string | null>(null);
  const [selectedHunkKey, setSelectedHunkKey] = useState('');
  const [draft, setDraft] = useState('');
  const [models, setModels] = useState<Array<{ id: string; name: string; provider: string }>>([]);
  const [modelPicker, setModelPicker] = useState(false);
  const [modelPending, setModelPending] = useState('');
  const [modelLabel, setModelLabel] = useState<string | null>(null);
  const [commitListOpen, setCommitListOpen] = useState(false);
  const [commitRows, setCommitRows] = useState<CodeReviewCommitWire[] | null>(null);
  const [commitsTruncated, setCommitsTruncated] = useState(false);
  const [commitsLoading, setCommitsLoading] = useState(false);
  const [commitsError, setCommitsError] = useState<string | null>(null);
  const [selectedFinding, setSelectedFinding] = useState<ReviewFindingRef | null>(null);
  /** Bumps when a navigation request is queued so the pending-navigation
   *  effect re-runs even when nothing else changed. */
  const [navTick, setNavTick] = useState(0);
  /** TL;DR pane visibility; the close action only hides the section. */
  const [tldrOpen, setTldrOpen] = useState(false);

  /** False once this panel is torn down: no response may touch state after. */
  const aliveRef = useRef(true);
  /** Request owner: only the newest request may apply its response. */
  const seqRef = useRef(0);
  const mutatingRef = useRef(false);
  const reviewIdRef = useRef('');
  const snapshotIdRef = useRef('');
  const selectedPathRef = useRef('');
  const loadedRef = useRef<ReviewLoadedDiff | null>(null);
  const windowRef = useRef(0);
  /** Newest recent-commit fetch. A superseded or unmounted answer is dropped. */
  const commitsSeqRef = useRef(0);
  /** The finding object captured at selection time: a snapshot whose findings
   *  for this hunk changed invalidates the reference instead of reusing an
   *  index that now names another finding. */
  const selectedFindingCapturedRef = useRef<ReviewFinding | null>(null);
  /** Queued finding navigation, driven by the effect below; a newer request
   *  or a user selection replaces it. */
  const pendingNavRef = useRef<{ entry: ReviewFindingEntry } | null>(null);
  const snapshotRef = useRef<ReviewSnapshot | null>(null);
  /** Generation of the running TLDR this panel acked but no snapshot has
   *  confirmed yet; 0 when none. A stale idle poll never erases it. */
  const tldrPendingRef = useRef(0);
  const scrollViewRef = useRef<ScrollView | null>(null);
  /** Measured y of each rendered line row, keyed `${hunkKey}#${rowIndex}`. */
  const linePositionsRef = useRef<Map<string, number>>(new Map());
  /** Measured y of each rendered hunk card in the scroll content, keyed by
   *  hunk key; combined with the line-local offset for exact scrolling. */
  const cardPositionsRef = useRef<Map<string, number>>(new Map());
  /** Row to scroll to once its layout is known. */
  const scrollTargetRef = useRef<{ hunkKey: string; row: number } | null>(null);
  /** Drop every trace of the review this panel was showing: it is gone, so no
   *  row of it may stay actionable. */
  const clearReviewView = useCallback(() => {
    reviewIdRef.current = '';
    snapshotIdRef.current = '';
    selectedPathRef.current = '';
    loadedRef.current = null;
    windowRef.current = 0;
    snapshotRef.current = null;
    pendingNavRef.current = null;
    selectedFindingCapturedRef.current = null;
    linePositionsRef.current.clear();
    cardPositionsRef.current.clear();
    scrollTargetRef.current = null;
    setSnapshot(null);
    setLoaded(null);
    setWindowLimit(0);
    setDiffError(null);
    setSelectedPath('');
    setSelectedHunkKey('');
    setSelectedFinding(null);
    setDraft('');
    tldrPendingRef.current = 0;
    setTldrOpen(false);
  }, []);

  /** Adopt a snapshot answer. `resetView` drops the previous file/hunk
   *  selection (a fresh scope); a changed snapshot identity always voids the
   *  loaded diff, because its pages were fetched for the old identity. */
  const applySnapshot = useCallback((data: unknown, resetView: boolean) => {
    const parsed = parseReviewSnapshot(data);
    if (!parsed) {
      setSnapshot(emptyReviewSnapshot(INVALID_SNAPSHOT));
      setPhase('open');
      return;
    }
    if (parsed.snapshotId !== snapshotIdRef.current) {
      loadedRef.current = null;
      windowRef.current = 0;
      pendingNavRef.current = null;
      // A new snapshot identity aborted the previous run: its pending ack
      // can never be confirmed by this snapshot.
      tldrPendingRef.current = 0;
      selectedFindingCapturedRef.current = null;
      linePositionsRef.current.clear();
      cardPositionsRef.current.clear();
      scrollTargetRef.current = null;
      setLoaded(null);
      setWindowLimit(0);
      setDiffError(null);
      setSelectedFinding(null);
    }
    snapshotIdRef.current = parsed.snapshotId;
    reviewIdRef.current = parsed.reviewId;
    // A same-owner snapshot OLDER than the accepted one cannot regress the
    // TLDR story: the newer generation's TLDR fields stay adopted (derived
    // from the accepted snapshot ref — no mirrored slot). A changed owner
    // resets: the identity branch above already cleared the run.
    const accepted = snapshotRef.current;
    const adopted = accepted !== null
      && accepted.snapshotId === parsed.snapshotId
      && accepted.tldrGeneration > parsed.tldrGeneration
      ? {
        ...parsed,
        tldrState: accepted.tldrState,
        tldrGeneration: accepted.tldrGeneration,
        tldrSnapshotId: accepted.tldrSnapshotId,
        tldrError: accepted.tldrError,
        tldrMarkdown: accepted.tldrMarkdown,
      }
      : parsed;
    snapshotRef.current = adopted;
    // Only a snapshot that has reached the acked generation owns the TLDR
    // story: an older poll — idle or not — never erases the running ack.
    if (tldrPendingRef.current !== 0 && adopted.tldrGeneration >= tldrPendingRef.current) {
      tldrPendingRef.current = 0;
    }
    setSnapshot(adopted);
    setPhase('open');
    if (resetView) {
      selectedPathRef.current = '';
      setSelectedPath('');
      setSelectedHunkKey('');
      setDraft('');
    }
  }, []);

  /** Ask the controller for its authoritative snapshot: the mount probe, a
   *  user Refresh, and the poll differ only in the command they send and in
   *  what a failure means. */
  const requestSnapshot = useCallback(async (mode: 'probe' | 'refresh' | 'poll') => {
    // Only a mutation takes a new sequence number. A poll reads the current
    // owner — so a mutation started after it still owns the cutover — but a
    // poll can never invalidate the mutation running beside it: that is what
    // would otherwise strand `busy` (and with it the composer) forever.
    const seq = mode === 'poll' ? seqRef.current : ++seqRef.current;
    if (mode === 'refresh') setBusy('refresh');
    try {
      const command = mode === 'refresh'
        ? { type: 'code_review_refresh' as const }
        : { type: 'code_review_snapshot' as const };
      const response = await client.command(command, sessionId);
      if (!aliveRef.current || seq !== seqRef.current) return;
      if (response.success) {
        applySnapshot(response.data, mode === 'probe');
        setNotice(null);
        return;
      }
      if (isNoOpenCodeReviewFailure(response.data)) {
        clearReviewView();
        setPhase('none');
        // No controller is the normal mount state; copy is only warranted when
        // a review the user was reading went away under them.
        setNotice(mode === 'poll' ? 'The review is no longer open.' : null);
        return;
      }
      const message = response.error || 'Code review is unavailable.';
      setNotice(message);
      // A failed probe leaves the scope form; a failed refresh keeps the last
      // good snapshot on screen and reports the failure.
      if (mode === 'probe') {
        clearReviewView();
        setPhase('none');
      } else if (mode === 'refresh') onError(message);
    } catch (err) {
      if (!aliveRef.current || seq !== seqRef.current) return;
      // Transport failures own their own surface, and the last good snapshot
      // stays: a poll never toasts.
      if (mode !== 'poll') onError(err instanceof Error ? err.message : String(err));
    } finally {
      if (aliveRef.current && seq === seqRef.current && mode !== 'poll') setBusy(null);
    }
  }, [applySnapshot, clearReviewView, client, onError, sessionId]);

  const loadModels = useCallback(async () => {
    try {
      const response = await client.command({ type: 'get_available_models' }, sessionId);
      if (!aliveRef.current) return;
      // A failed catalog is not a review failure: the picker keeps the
      // snapshot's model as its label.
      if (response.success) setModels(parseAvailableModels(response.data));
    } catch {
      // Offline: the picker keeps whatever catalog it had.
    }
  }, [client, sessionId]);

  const openReview = useCallback(async (commitOverride?: string) => {
    const from = commitOverride ?? scopeFrom;
    const to = commitOverride !== undefined ? '' : scopeTo;
    const scope = parseCodeReviewScope(scopeMode, from, to);
    if (!scope.ok) {
      setScopeError(scope.error);
      return;
    }
    if (commitOverride !== undefined) {
      setScopeFrom(commitOverride);
      setScopeTo('');
    }
    setScopeError(null);
    setCommitListOpen(false);
    setBusy('open');
    const seq = ++seqRef.current;
    try {
      const response = await client.command(scope.command, sessionId);
      if (!aliveRef.current || seq !== seqRef.current) return;
      if (!response.success) {
        setScopeError(response.error || 'Open failed');
        return;
      }
      applySnapshot(response.data, true);
      setNotice(null);
    } catch (err) {
      if (aliveRef.current && seq === seqRef.current) {
        setScopeError(err instanceof Error ? err.message : String(err));
      }
    } finally {
      if (aliveRef.current && seq === seqRef.current) setBusy(null);
    }
  }, [applySnapshot, client, sessionId, scopeFrom, scopeMode, scopeTo]);

  const loadRecentCommits = useCallback(async () => {
    const seq = ++commitsSeqRef.current;
    setCommitsLoading(true);
    try {
      const response = await client.command({ type: 'code_review_commits' }, sessionId);
      if (!aliveRef.current || seq !== commitsSeqRef.current) return;
      if (!response.success) {
        if (!isNoOpenCodeReviewFailure(response.data)) {
          setCommitRows(null);
          setCommitsError(response.error || 'Recent commits are unavailable.');
        }
      } else {
        const parsed = parseCodeReviewCommits(response.data);
        if (!parsed) {
          setCommitRows(null);
          setCommitsError(INVALID_COMMITS);
        } else {
          setCommitRows(parsed.commits);
          setCommitsTruncated(parsed.truncated);
          setCommitsError(null);
        }
      }
    } catch (err) {
      if (!aliveRef.current || seq !== commitsSeqRef.current) return;
      setCommitRows(null);
      setCommitsError(err instanceof Error ? err.message : String(err));
    } finally {
      if (aliveRef.current && seq === commitsSeqRef.current) setCommitsLoading(false);
    }
  }, [client, sessionId]);

  const chooseCommit = useCallback((commit: string) => {
    setCommitListOpen(false);
    void openReview(commit);
  }, [openReview]);

  const closeReview = useCallback(async () => {
    const reviewId = reviewIdRef.current;
    if (!reviewId) return;
    setBusy('close');
    const seq = ++seqRef.current;
    try {
      const response = await client.command({ type: 'code_review_close', reviewId }, sessionId);
      if (!aliveRef.current || seq !== seqRef.current) return;
      if (!response.success && !isNoOpenCodeReviewFailure(response.data)) {
        onError(response.error || 'Close failed');
        return;
      }
      // `closed: false` with `staleReview: true` is the typed no-op of a
      // retired panel: either way this panel no longer owns a controller.
      clearReviewView();
      setPhase('none');
      setNotice('Review closed.');
    } catch (err) {
      if (aliveRef.current) onError(err instanceof Error ? err.message : String(err));
    } finally {
      if (aliveRef.current && seq === seqRef.current) setBusy(null);
    }
  }, [clearReviewView, client, onError, sessionId]);

  const selectFile = useCallback((path: string) => {
    selectedPathRef.current = path;
    loadedRef.current = null;
    windowRef.current = 0;
    pendingNavRef.current = null;
    selectedFindingCapturedRef.current = null;
    setSelectedFinding(null);
    linePositionsRef.current.clear();
    cardPositionsRef.current.clear();
    scrollTargetRef.current = null;
    setSelectedPath(path);
    setLoaded(null);
    setWindowLimit(0);
    setDiffError(null);
    setSelectedHunkKey('');
    setDraft('');
  }, []);

  /** Open/grow the per-file window. The body blanks until the first page of
   *  this exact identity lands, and pages stop advancing the moment one of
   *  them cannot be verified — a stale page is never spliced in. */
  const growWindow = useCallback(async (force = false) => {
    const start = loadedRef.current;
    if (!start || start.path !== selectedPathRef.current) return false;
    const path = start.path;
    const snapshotId = start.snapshotId;
    if (snapshotId !== snapshotIdRef.current) return false;
    const plan = planReviewDiffWindow(start, windowRef.current);
    if (!plan.canLoadMore && !force) return false;
    let next = start;
    let fetched = false;
    try {
      while (!next.hunksComplete && (next.hasMore || !fetched) && next.lines.length < REVIEW_DIFF_MAX_UI_LINES) {
        if (!aliveRef.current || selectedPathRef.current !== path || snapshotIdRef.current !== snapshotId) return false;
        const response = await client.command({
          type: 'code_review_file_diff',
          snapshotId,
          path,
          cursor: next.nextCursor,
          maxLines: REVIEW_PAGE_LINES,
        }, sessionId);
        if (!aliveRef.current || selectedPathRef.current !== path || snapshotIdRef.current !== snapshotId) return false;
        if (!response.success) {
          setDiffError(response.error || 'Diff page failed');
          return false;
        }
        const page = parseCodeReviewFileDiffPage(response.data);
        if (!page) {
          setDiffError(INVALID_DIFF_PAGE);
          return false;
        }
        const merged = appendReviewDiffPage(next, page);
        // Duplicate or out-of-order page: the stream advances only as far as
        // what this review can still verify.
        if (merged === next) break;
        fetched = true;
        next = merged;
        loadedRef.current = next;
        setLoaded(next);
      }
    } catch (err) {
      if (!aliveRef.current) return false;
      setDiffError(err instanceof Error ? err.message : String(err));
      return false;
    }
    const limit = Math.min(
      Math.max(windowRef.current, plan.target),
      next.lines.length,
      REVIEW_DIFF_MAX_UI_LINES,
    );
    windowRef.current = limit;
    setWindowLimit(limit);
    setDiffError(null);
    return limit > Math.min(start.lines.length, REVIEW_DIFF_MAX_UI_LINES) || loadedRef.current !== start;
  }, [client, sessionId]);

  const selectHunk = useCallback((key: string, ready: boolean) => {
    // Selection is explicit and never carried across files; a hunk whose body
    // is still arriving is not a comment target, and the draft belongs to the
    if (!ready) return;
    setSelectedHunkKey((previous) => (previous === key ? '' : key));
    // An ordinary hunk click preserves the manual hunk selection and clears
    // the finding selection.
    pendingNavRef.current = null;
    selectedFindingCapturedRef.current = null;
    setSelectedFinding(null);
    scrollTargetRef.current = null;
    setDraft('');
  }, []);

  const activeDiff = loaded && snapshot && loaded.snapshotId === snapshot.snapshotId && loaded.path === selectedPath
    ? loaded
    : null;
  const selectedFile = snapshot?.files.find((file) => file.path === selectedPath) ?? null;
  const closeOrBack = useCallback(() => {
    if (commitListOpen) {
      setCommitListOpen(false);
      return;
    }
    if (selectedFile) selectFile('');
    else props.onClose();
  }, [commitListOpen, selectedFile, selectFile, props.onClose]);
  const registerBackHandler = props.onRegisterBackHandler;
  useEffect(() => {
    registerBackHandler?.(() => {
      closeOrBack();
      return true;
    });
    return () => registerBackHandler?.(null);
  }, [registerBackHandler, closeOrBack]);
  const selectedHunk = activeDiff && selectedHunkKey
    ? activeDiff.hunks.find((hunk) => reviewHunkKeyFor(activeDiff, hunk) === selectedHunkKey) ?? null
    : null;
  const selectedIdentity: ReviewHunkIdentity | null = snapshot && selectedFile && selectedHunk
    ? reviewHunkIdentityFor(snapshot.snapshotId, selectedFile, selectedHunk)
    : null;
  const selectedThread = selectedIdentity && snapshot
    ? findReviewThread(snapshot.threads, selectedIdentity)
    : undefined;
  // The derived global finding list: one ordered collection that navigation,
  // marks, and detail all read. There is no second verdict store.
  const findingEntries = useMemo(
    () => (snapshot ? orderReviewFindings(snapshot) : []),
    [snapshot],
  );
  const selectedFindingEntry = useMemo(() => {
    if (!selectedFinding) return null;
    return findingEntries.find((entry) => (
      entry.ref.findingIndex === selectedFinding.findingIndex
      && reviewHunkIdentityEqual(entry.ref.identity, selectedFinding.identity)
    )) ?? null;
  }, [findingEntries, selectedFinding]);
  const selectedHunkFindings = selectedIdentity && snapshot
    ? reviewFindingsForHunk(snapshot.reviewFindings, selectedIdentity)
    : [];
  const previousFinding = reviewFindingNeighbor(findingEntries, selectedFinding, 'previous');
  const nextFinding = reviewFindingNeighbor(findingEntries, selectedFinding, 'next');

  const clearFindingSelection = useCallback(() => {
    pendingNavRef.current = null;
    selectedFindingCapturedRef.current = null;
    scrollTargetRef.current = null;
    setSelectedFinding(null);
  }, []);

  /** Queue a finding navigation. The transaction runs in the effect below:
   *  select the target file (keeping the manual draft), grow the existing
   *  paged window until the exact rows are visible, then commit hunk and
   *  finding together and scroll. */
  const gotoFinding = useCallback((entry: ReviewFindingEntry) => {
    if (entry.path !== selectedPathRef.current) {
      selectedPathRef.current = entry.path;
      loadedRef.current = null;
      windowRef.current = 0;
      setSelectedPath(entry.path);
      setLoaded(null);
      setWindowLimit(0);
      setDiffError(null);
      setSelectedHunkKey('');
      linePositionsRef.current.clear();
      cardPositionsRef.current.clear();
    }
    pendingNavRef.current = { entry };
    setNavTick((tick) => tick + 1);
  }, []);

  /** Select a finding from the open hunk's detail list and scroll its exact
   *  range into view. The manual draft and hunk selection stay untouched. */
  const selectFindingIndex = useCallback((hunkKey: string, index: number) => {
    if (!selectedIdentity || !snapshot || !selectedHunk) return;
    const finding = selectedHunkFindings[index];
    if (!finding) return;
    // Coverage comes before any commit: an uncovered range surfaces an error
    // instead of committing an unscrollable selection.
    const rows = reviewFindingRowRange(selectedHunk.lines, finding);
    if (!rows) {
      setDiffError('The finding range is not present in the loaded diff.');
      return;
    }
    setSelectedFinding({ identity: selectedIdentity, findingIndex: index });
    selectedFindingCapturedRef.current = finding;
    scrollTargetRef.current = { hunkKey, row: rows.first };
    setNavTick((tick) => tick + 1);
  }, [selectedIdentity, snapshot, selectedHunk, selectedHunkFindings]);

  // A snapshot whose findings changed for the selected hunk invalidates the
  // reference instead of letting the index name another finding.
  useEffect(() => {
    if (!snapshot || !selectedFinding) return;
    const current = reviewFindingAtRef(snapshot.reviewFindings, selectedFinding);
    const captured = selectedFindingCapturedRef.current;
    if (!current || !captured || JSON.stringify(current) !== JSON.stringify(captured)) {
      clearFindingSelection();
    }
  }, [snapshot, selectedFinding, clearFindingSelection]);

  // Drive a queued navigation. The seed effect owns loading the target file's
  // diff; this effect waits for the exact hunk, grows the window until the
  // rows are inside it, and commits. A user selection or snapshot change
  // cancels the pending navigation instead of committing it late.
  useEffect(() => {
    const pending = pendingNavRef.current;
    if (!pending) return;
    const current = loaded;
    if (!snapshot || !current || current.snapshotId !== snapshot.snapshotId || current.path !== pending.entry.path) return;
    const hunkIndex = current.hunks.findIndex((hunk) => reviewHunkKeyFor(current, hunk) === pending.entry.hunkKey);
    const hunk = hunkIndex === -1 ? null : current.hunks[hunkIndex];
    const plan = planReviewDiffWindow(current, windowLimit);
    const growTowardTarget = () => {
      // Grow only while the exact target is not yet reachable; never expand
      // toward the whole file once the requested finding is present.
      if (plan.canLoadMore) {
        void growWindow(true);
      } else {
        pendingNavRef.current = null;
        setDiffError('The finding range is not reachable in this diff.');
      }
    };
    // A missing hunk, or one whose paged rows are still arriving, is not a
    // partial success: keep loading rather than guessing at its rows.
    if (!hunk || !reviewHunkIsReady(hunk)) {
      growTowardTarget();
      return;
    }
    const rows = reviewFindingRowRange(hunk.lines, pending.entry.finding);
    if (!rows) {
      // A ready hunk without full side-specific coverage is a real mismatch.
      pendingNavRef.current = null;
      setDiffError('The finding range is not present in the loaded diff.');
      return;
    }
    let before = 0;
    for (let i = 0; i < hunkIndex; i += 1) before += current.hunks[i].lines.length;
    if (windowLimit < before + hunk.lines.length) {
      growTowardTarget();
      return;
    }
    pendingNavRef.current = null;
    setDiffError(null);
    setSelectedHunkKey(pending.entry.hunkKey);
    setSelectedFinding(pending.entry.ref);
    selectedFindingCapturedRef.current = pending.entry.finding;
    scrollTargetRef.current = { hunkKey: pending.entry.hunkKey, row: rows.first };
  }, [snapshot, loaded, windowLimit, navTick, growWindow]);

  const submitComment = useCallback(async () => {
    const identity = selectedIdentity;
    const text = draft.trim();
    if (!identity || !identity.snapshotId || text === '' || mutatingRef.current) return;
    mutatingRef.current = true;
    setBusy('comment');
    const seq = ++seqRef.current;
    try {
      const response = await client.command(
        buildCodeReviewCommentPayload(identity, text),
        sessionId,
      );
      if (!aliveRef.current || seq !== seqRef.current) return;
      if (!response.success) {
        // The draft stays in the composer: a rejected comment is never shown as
        // a row, and its text is still there to retry.
        onError(response.error || 'Comment rejected');
        return;
      }
      applySnapshot(response.data, false);
      setDraft((current) => current.trim() === text ? '' : current);
    } catch (err) {
      if (aliveRef.current) onError(err instanceof Error ? err.message : String(err));
    } finally {
      mutatingRef.current = false;
      if (aliveRef.current && seq === seqRef.current) setBusy(null);
    }
  }, [applySnapshot, draft, client, onError, sessionId, selectedIdentity]);

  const stopReply = useCallback(async () => {
    // Identity captured at press time: a later selection change can never
    // retarget the stop, and only the backend may close the review.
    const identity = selectedIdentity;
    if (!identity || selectedThread?.isCancellable !== true || mutatingRef.current) return;
    mutatingRef.current = true;
    setBusy('abort');
    const seq = ++seqRef.current;
    try {
      const response = await client.command(buildCodeReviewAbortPayload(identity), sessionId);
      if (!aliveRef.current || seq !== seqRef.current) return;
      if (!response.success) {
        onError(response.error || 'Stop failed');
        return;
      }
      applySnapshot(response.data, false);
    } catch (err) {
      if (aliveRef.current) onError(err instanceof Error ? err.message : String(err));
    } finally {
      mutatingRef.current = false;
      if (aliveRef.current && seq === seqRef.current) setBusy(null);
    }
  }, [applySnapshot, client, onError, sessionId, selectedIdentity, selectedThread]);

  const pickModel = useCallback(async (value: string) => {
    if (mutatingRef.current) return;
    mutatingRef.current = true;
    setModelPending(value);
    const seq = ++seqRef.current;
    try {
      const response = await client.command(
        { type: 'code_review_set_model', modelId: value },
        sessionId,
      );
      if (!aliveRef.current || seq !== seqRef.current) return;
      if (!response.success) {
        setModelPending('');
        onError(response.error || 'Model change failed');
        return;
      }
      const ack = parseCodeReviewSetModelAck(response.data);
      setModelPending('');
      setModelLabel(ack ? ack.model : null);
    } catch (err) {
      if (!aliveRef.current) return;
      setModelPending('');
      onError(err instanceof Error ? err.message : String(err));
    } finally {
      mutatingRef.current = false;
    }
  }, [client, onError, sessionId]);

  // The TLDR view over the authoritative snapshot. A summary whose snapshot
  // id is not THIS snapshot's is stale: it never renders, and a done/failed
  // result from an older snapshot reads idle. A run in flight — owned, or
  // still only acked — reads running so Stop and the progress stay visible.
  const tldrState: CodeReviewTldrState = !snapshot
    ? 'idle'
    : tldrPendingRef.current !== 0 && snapshot.tldrGeneration < tldrPendingRef.current
      // A newer acked run supersedes whatever this older snapshot still says,
      // including its owned done/failed result: the old body must not show.
      ? 'running'
      : snapshot.tldrSnapshotId !== null && snapshot.tldrSnapshotId === snapshot.snapshotId
        ? snapshot.tldrState
        : snapshot.tldrState === 'running'
          ? 'running'
          : 'idle';
  const tldrMarkdown = snapshot
    && snapshot.tldrSnapshotId !== null
    && snapshot.tldrSnapshotId === snapshot.snapshotId
    && tldrState === 'done'
    ? snapshot.tldrMarkdown
    : '';
  const tldrError = snapshot
    && snapshot.tldrSnapshotId !== null
    && snapshot.tldrSnapshotId === snapshot.snapshotId
    && tldrState === 'failed'
    ? snapshot.tldrError
    : null;

  // Poll for the authoritative snapshot. Idle cadence, or the batch cadence
  // while an Auto Review / Highlight / TLDR run owns the controller; polls
  // pause while an open/refresh owns the cutover.
  const pollMs = tldrState === 'running' ? CODE_REVIEW_BATCH_POLL_MS : codeReviewSnapshotPollMs(snapshot);

  const startTldr = useCallback(async () => {
    if (mutatingRef.current) return;
    mutatingRef.current = true;
    setBusy('tldr');
    const seq = ++seqRef.current;
    try {
      const response = await client.command(buildCodeReviewTldrPayload(), sessionId);
      if (!aliveRef.current || seq !== seqRef.current) return;
      if (!response.success) {
        onError(response.error || 'Summary failed to start');
        return;
      }
      const ack = parseCodeReviewTldrAck(response.data);
      if (!ack) {
        // A malformed ack is never a running generation: the pane stays out
        // and the central error surface reports it instead.
        onError(INVALID_TLDR_ACK);
        return;
      }
      tldrPendingRef.current = ack.tldrGeneration;
      setTldrOpen(true);
    } catch (err) {
      if (aliveRef.current) onError(err instanceof Error ? err.message : String(err));
    } finally {
      mutatingRef.current = false;
      if (aliveRef.current && seq === seqRef.current) setBusy(null);
    }
  }, [client, onError, sessionId]);

  const stopTldr = useCallback(async () => {
    // The running state is captured at press time; a later snapshot can never
    // retarget the stop, and only the backend may close the review.
    if (mutatingRef.current || tldrState !== 'running') return;
    mutatingRef.current = true;
    setBusy('tldrStop');
    const seq = ++seqRef.current;
    try {
      const response = await client.command(buildCodeReviewStopPayload('tldr'), sessionId);
      if (!aliveRef.current || seq !== seqRef.current) return;
      if (!response.success) {
        onError(response.error || 'Stop failed');
        return;
      }
      // The stop ack carries no state: the pending ack is dropped and the
      // next authoritative snapshot settles the pane, so a run that completed
      // before the abort landed still shows its result.
      tldrPendingRef.current = 0;
    } catch (err) {
      if (aliveRef.current) onError(err instanceof Error ? err.message : String(err));
    } finally {
      mutatingRef.current = false;
      if (aliveRef.current && seq === seqRef.current) setBusy(null);
    }
  }, [client, onError, sessionId, tldrState]);

  // Mount, and every session switch: probe the controller, load the model
  // catalog, and make sure the review this panel owned is closed when it is
  // torn down (the /web unmount contract) or the session changes under it.
  useEffect(() => {
    aliveRef.current = true;
    seqRef.current += 1;
    clearReviewView();
    setPhase('probe');
    // The probe owns the panel's busy state for this session: a mutation of the
    // previous one can no longer clear it (its sequence is superseded).
    setBusy('open');
    void requestSnapshot('probe');
    void loadModels();
    return () => {
      aliveRef.current = false;
      seqRef.current += 1;
      const reviewId = reviewIdRef.current;
      reviewIdRef.current = '';
      if (reviewId) {
        void client.command({ type: 'code_review_close', reviewId }, sessionId).catch(() => undefined);
      }
    };
  }, [clearReviewView, client, loadModels, requestSnapshot, sessionId]);

  useEffect(() => {
    if (phase !== 'open' || busy === 'open' || busy === 'refresh') return;
    let cancelled = false;
    let timer: TransportTimerHandle | undefined;
    const tick = () => {
      void requestSnapshot('poll').finally(() => {
        if (cancelled) return;
        timer = setTimeout(tick, pollMs);
      });
    };
    tick();
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [busy, phase, pollMs, requestSnapshot]);

  // Seed (and re-seed) the selected file's diff for the snapshot it belongs to,
  // then open its first window. A selection that no longer exists renders the
  // empty state instead of silently jumping to another file.
  useEffect(() => {
    if (!snapshot || !selectedPath) return;
    const current = loadedRef.current;
    if (current && current.snapshotId === snapshot.snapshotId && current.path === selectedPath) return;
    const file = snapshot.files.find((entry) => entry.path === selectedPath);
    if (!file) {
      selectFile('');
      return;
    }
    const seeded = initReviewLoadedDiff(snapshot.snapshotId, file);
    loadedRef.current = seeded;
    windowRef.current = 0;
    setLoaded(seeded);
    setWindowLimit(0);
    setDiffError(null);
    void growWindow();
  }, [growWindow, selectFile, selectedPath, snapshot]);

  const lineColor = (kind: string): string => {
    if (kind === 'addition') return palette.ok;
    if (kind === 'deletion') return palette.error;
    if (kind === 'meta') return palette.muted;
    return palette.fg;
  };

  const hunkRows = useMemo(
    () => (activeDiff ? windowedReviewHunks(activeDiff, windowLimit) : []),
    [activeDiff, windowLimit],
  );
  // Scroll the committed finding's first marked row into view. Coordinates
  // combine the hunk card's offset in the scroll content with the line's
  // offset inside the card — a line's own onLayout y is card-relative. Both
  // the effect and the layout handlers complete the same transaction.
  const scrollToTarget = useCallback(() => {
    const target = scrollTargetRef.current;
    if (!target) return;
    const cardY = cardPositionsRef.current.get(target.hunkKey);
    const lineY = linePositionsRef.current.get(`${target.hunkKey}#${target.row}`);
    if (cardY === undefined || lineY === undefined) return;
    scrollTargetRef.current = null;
    scrollViewRef.current?.scrollTo({ y: Math.max(0, cardY + lineY - 72), animated: true });
  }, []);
  useEffect(() => {
    scrollToTarget();
  }, [scrollToTarget, selectedFinding, hunkRows, navTick]);

  /** Per-row decoration for one rendered hunk: only rows carrying a
   *  coordinate on the finding's side inside the range are marked (never the
   *  physical interval between matching rows); the highest category's tone
   *  wins an overlap, and the selected finding's rows additionally carry the
   *  emphasis background. */
  const findingTonesFor = (
    hunkKey: string,
    lines: readonly DiffLine[],
    hunkFindings: readonly ReviewFinding[],
  ): Map<number, { tone: string; rank: number; selected: boolean }> => {
    const tones = new Map<number, { tone: string; rank: number; selected: boolean }>();
    const selectedHere = selectedFindingEntry !== null && selectedFindingEntry.hunkKey === hunkKey
      ? selectedFindingEntry
      : null;
    hunkFindings.forEach((finding, findingIndex) => {
      const rank = FINDING_TONE_RANK[finding.category];
      const tone = findingToneColor(finding.category, palette);
      const selected = selectedHere !== null && selectedHere.ref.findingIndex === findingIndex;
      for (let index = 0; index < lines.length; index += 1) {
        if (!reviewFindingCoversRow(lines[index], finding)) continue;
        const existing = tones.get(index);
        if (!existing || rank > existing.rank) {
          tones.set(index, { tone, rank, selected: selected || (existing?.selected ?? false) });
        } else if (selected) {
          tones.set(index, { ...existing, selected: true });
        }
      }
    });
    return tones;
  };
  const diffPlan = activeDiff ? planReviewDiffWindow(activeDiff, windowLimit) : null;
  const diffStatus = activeDiff ? reviewDiffStatusText(activeDiff, windowLimit) : null;
  // A binary file has no line body, and an empty complete one has nothing to
  // wait for: neither may show the loading placeholder.
  const diffEmpty = !!activeDiff && activeDiff.hunksComplete && activeDiff.hunks.length === 0;
  const diffAwaiting = !!selectedFile
    && !selectedFile.binary
    && !diffEmpty
    && (!activeDiff || (windowLimit === 0 && diffError === null));
  const appliedModel = reviewModelOptionValue(snapshot);
  const shownModel = modelPending || appliedModel;
  // Without a controller there is nothing to act on: the scope form takes over.
  const scopeVisible = phase !== 'open'
    || !snapshot
    || snapshot.error !== null
    || snapshot.files.length === 0;

  return (
    <View style={[styles.screen, { backgroundColor: palette.bg }]}>
      <AppBar
        palette={palette}
        title={selectedFile ? selectedFile.path : 'Code review'}
        onBack={closeOrBack}
        actions={[
          {
            icon: 'tune',
            label: 'Review model',
            onPress: () => setModelPicker((previous) => !previous),
            disabled: phase !== 'open',
          },
          {
            icon: 'refresh',
            label: 'Refresh review',
            onPress: () => void requestSnapshot('refresh'),
            disabled: busy !== null || phase !== 'open',
          },
          {
            icon: 'close',
            label: 'Close review',
            onPress: () => void closeReview(),
            disabled: busy !== null || phase !== 'open',
          },
        ]}
      />
      <ScrollView ref={scrollViewRef} style={styles.list}>
        {phase === 'probe' && (
          <Text style={[styles.emptyText, { color: palette.muted, padding: 24 }]}>…</Text>
        )}

        {scopeVisible && (
          <View style={styles.form}>
            {snapshot?.error ? (
              <Text style={[styles.inlineError, { color: palette.error }]}>{redactSecrets(snapshot.error)}</Text>
            ) : null}
            {phase === 'open' && snapshot && !snapshot.error && snapshot.files.length === 0 && (
              <Text style={[styles.emptyText, { color: palette.muted }]}>
                {reviewEmptyComparisonMessage(snapshot.untrackedNotShown)}
              </Text>
            )}
            {notice ? (
              <Text style={[styles.inlineError, { color: palette.muted }]}>{notice}</Text>
            ) : null}
            <Text style={[styles.formSection, { color: palette.muted }]}>Comparison</Text>
            <View style={{ flexDirection: 'row', flexWrap: 'wrap', gap: 8 }}>
              {SCOPE_MODES.map((option) => (
                <TouchableOpacity
                  key={option.mode}
                  style={[styles.primaryButton, {
                    flexGrow: 1,
                    marginTop: 0,
                    backgroundColor: scopeMode === option.mode ? palette.accent : palette.surface,
                  }]}
                  onPress={() => {
                    setScopeMode(option.mode);
                    setScopeError(null);
                    if (option.mode !== 'commit') setCommitListOpen(false);
                  }}
                  accessibilityLabel={`Scope ${option.mode}`}>
                  <Text style={[styles.primaryButtonText, {
                    color: scopeMode === option.mode ? palette.bg : palette.fg,
                  }]}>{option.label}</Text>
                </TouchableOpacity>
              ))}
            </View>
            {(scopeMode === 'base' || scopeMode === 'range' || scopeMode === 'commit') && (
              <TextInput
                style={[styles.input, {
                  backgroundColor: palette.surface,
                  borderColor: palette.border,
                  color: palette.fg,
                  fontFamily: palette.mono,
                }]}
                value={scopeFrom}
                onChangeText={setScopeFrom}
                placeholder={scopeMode === 'base' ? 'Base revision' : scopeMode === 'commit' ? 'Commit or ref' : 'From revision'}
                placeholderTextColor={palette.muted}
                autoCapitalize="none"
                autoCorrect={false}
                accessibilityLabel={scopeMode === 'base' ? 'Base revision' : scopeMode === 'commit' ? 'Commit ref' : 'From revision'}
              />
            )}
            {scopeMode === 'commit' && (
              <TouchableOpacity
                style={[styles.primaryButton, {
                  backgroundColor: palette.surface,
                  borderWidth: 1,
                  borderColor: palette.border,
                }, busy !== null && styles.faded]}
                disabled={busy !== null}
                onPress={() => {
                  setCommitListOpen(true);
                  void loadRecentCommits();
                }}
                accessibilityLabel="Recent commits">
                <Text style={[styles.primaryButtonText, { color: palette.fg }]}>Recent commits</Text>
              </TouchableOpacity>
            )}
            {scopeMode === 'range' && (
              <TextInput
                style={[styles.input, {
                  backgroundColor: palette.surface,
                  borderColor: palette.border,
                  color: palette.fg,
                  fontFamily: palette.mono,
                }]}
                value={scopeTo}
                onChangeText={setScopeTo}
                placeholder="To revision"
                placeholderTextColor={palette.muted}
                autoCapitalize="none"
                autoCorrect={false}
                accessibilityLabel="To revision"
              />
            )}
            <TouchableOpacity
              style={[styles.primaryButton, { backgroundColor: palette.accent }, busy !== null && styles.faded]}
              disabled={busy !== null}
              onPress={() => void openReview()}
              accessibilityLabel="Open review">
              <Text style={[styles.primaryButtonText, { color: palette.bg }]}>Open review</Text>
            </TouchableOpacity>
            <Text style={[styles.cardMeta, { color: palette.muted }]}>{CODE_REVIEW_SCOPE_USAGE}</Text>
            {scopeError ? (
              <Text style={[styles.inlineError, { color: palette.error }]}>{scopeError}</Text>
            ) : null}
            {scopeMode === 'commit' && commitListOpen && (
              <View>
                <Text style={[styles.formSection, { color: palette.muted }]}>Recent commits</Text>
                {commitsLoading ? (
                  <Text style={[styles.emptyText, { color: palette.muted }]}>…</Text>
                ) : null}
                {commitsError ? (
                  <Text style={[styles.inlineError, { color: palette.error }]}>{redactSecrets(commitsError)}</Text>
                ) : null}
                {!commitsLoading && commitRows && commitRows.length === 0 && !commitsError ? (
                  <Text style={[styles.emptyText, { color: palette.muted }]}>No commits.</Text>
                ) : null}
                {(commitRows ?? []).map((row) => (
                  <TouchableOpacity
                    key={row.commit}
                    style={[styles.listRow, { backgroundColor: palette.surface, borderBottomColor: palette.border }]}
                    disabled={busy !== null}
                    onPress={() => chooseCommit(row.commit)}
                    accessibilityLabel={`Commit ${row.commit}`}>
                    <View style={styles.rowBody}>
                      <Text style={[styles.rowTitle, { color: palette.fg, fontFamily: palette.mono }]} numberOfLines={1}>
                        {row.commit}
                      </Text>
                      <Text style={[styles.rowSub, { color: palette.muted }]} numberOfLines={2}>
                        {row.subject}
                      </Text>
                      <Text style={[styles.cardMeta, { color: palette.muted }]}>
                        {`${row.author} · ${row.authorDate} · ${row.parentCommit ?? ''} · ${row.parentCount}`}
                      </Text>
                    </View>
                    <Icon name="chevron-right" size={22} color={palette.muted} />
                  </TouchableOpacity>
                ))}
                {commitsTruncated ? (
                  <Text style={[styles.cardMeta, { color: palette.muted }]}>More commits exist.</Text>
                ) : null}
              </View>
            )}
          </View>
        )}

        {phase === 'open' && snapshot && snapshot.files.length > 0 && !selectedFile && (
          <View style={styles.form}>
            <Text style={[styles.formSection, { color: palette.muted }]}>
              {snapshot.comparisonLabel || '--'}
            </Text>
            <Text style={[styles.rowSub, { color: palette.muted }]}>
              {`+${snapshot.totalInsertions} / -${snapshot.totalDeletions}${snapshot.totalsComplete ? '' : ' (partial)'}`}
            </Text>
            {snapshot.activeCount > 0 && (
              <Text style={[styles.rowSub, { color: palette.accent }]}>
                {`${snapshot.activeCount} ${snapshot.activeCount === 1 ? 'reply' : 'replies'} in flight`}
              </Text>
            )}
            {snapshot.internalBusy && (
              <Text style={[styles.rowSub, { color: palette.muted }]}>Review work is still settling.</Text>
            )}
            {reviewStageFeedback(snapshot) ? (
              <Text
                style={[styles.rowSub, { color: palette.accent }]}
                accessibilityLabel="Review stage">
                {reviewStageFeedback(snapshot)}
              </Text>
            ) : null}
            {notice ? (
              <Text style={[styles.inlineError, { color: palette.muted }]}>{notice}</Text>
            ) : null}
            <TouchableOpacity
              style={[styles.primaryButton, {
                marginTop: 0,
                flexGrow: 0,
                backgroundColor: palette.surface,
                borderWidth: 1,
                borderColor: palette.border,
              }, busy !== null && styles.faded]}
              disabled={busy !== null}
              onPress={() => {
                // Idle/failed start (or retry): the pane opens only once the
                // controller acks the run, so a failed start never leaves an
                // empty summary section behind. Otherwise just reopen it.
                if (tldrState === 'idle' || tldrState === 'failed') {
                  void startTldr();
                  return;
                }
                setTldrOpen(true);
              }}
              accessibilityLabel="TL;DR">
              <Text style={[styles.primaryButtonText, { color: palette.fg }]}>TL;DR</Text>
            </TouchableOpacity>
            {tldrOpen ? (
              <ReviewTldrPane
                palette={palette}
                state={tldrState}
                markdown={tldrMarkdown}
                error={tldrError}
                busy={busy !== null}
                onStop={() => void stopTldr()}
                onClose={() => setTldrOpen(false)}
              />
            ) : null}
          </View>
        )}

        {phase === 'open' && modelPicker && (
          <View style={styles.form}>
            <Text style={[styles.formSection, { color: palette.muted }]}>Review model</Text>
            <Text style={[styles.rowSub, { color: palette.muted }]}>
              {modelLabel || shownModel || 'Model unknown.'}
            </Text>
            {models.length === 0 && (
              <Text style={[styles.emptyText, { color: palette.muted }]}>No models.</Text>
            )}
            {models.map((model) => {
              const value = `${model.provider}/${model.id}`;
              const active = value === shownModel;
              return (
                <TouchableOpacity
                  key={value}
                  style={[styles.listRow, {
                    backgroundColor: active ? palette.border : palette.surface,
                    borderBottomColor: palette.border,
                  }]}
                  disabled={busy !== null}
                  onPress={() => void pickModel(value)}
                  accessibilityLabel={`Review model ${value}`}>
                  <View style={styles.rowBody}>
                    <Text style={[styles.rowTitle, { color: palette.fg }]} numberOfLines={1}>
                      {model.name || model.id}
                    </Text>
                    <Text style={[styles.rowSub, { color: palette.muted }]} numberOfLines={1}>{value}</Text>
                  </View>
                  {active && <Icon name="check" size={20} color={palette.ok} />}
                </TouchableOpacity>
              );
            })}
          </View>
        )}

        {phase === 'open' && !selectedFile && snapshot?.files.map((file) => (
          <TouchableOpacity
            key={file.path}
            style={[styles.listRow, { backgroundColor: palette.surface, borderBottomColor: palette.border }]}
            onPress={() => selectFile(file.path)}
            accessibilityLabel={`Review file ${file.path}`}>
            <View style={styles.rowBody}>
              <Text style={[styles.rowTitle, { color: palette.fg }]} numberOfLines={1}>{file.path}</Text>
              <Text style={[styles.rowSub, { color: palette.muted }]}>
                {`${file.status} · +${file.insertions} / -${file.deletions}`}
              </Text>
            </View>
            <Icon name="chevron-right" size={22} color={palette.muted} />
          </TouchableOpacity>
        ))}

        {phase === 'open' && selectedFile && (
          <View style={styles.form}>
            <Text style={[styles.rowSub, { color: palette.muted }]}>
              {`${selectedFile.status} · +${selectedFile.insertions} / -${selectedFile.deletions}`}
            </Text>
            {selectedFile.message ? (
              <Text style={[styles.rowSub, { color: palette.muted }]}>{redactSecrets(selectedFile.message)}</Text>
            ) : null}
            {selectedFile.binary && (
              <Text style={[styles.emptyText, { color: palette.muted }]}>Binary file — no line diff.</Text>
            )}
            {diffAwaiting && (
              <Text style={[styles.emptyText, { color: palette.muted }]}>…</Text>
            )}
            {diffEmpty && (
              <Text style={[styles.emptyText, { color: palette.muted }]}>No diff lines.</Text>
            )}
          </View>
        )}
        {phase === 'open' && selectedFile && findingEntries.length > 0 && (
          <View style={{ flexDirection: 'row', alignItems: 'center', gap: 8, marginTop: 8 }}>
            <TouchableOpacity
              style={[styles.primaryButton, {
                marginTop: 0,
                flexGrow: 0,
                backgroundColor: palette.surface,
                borderWidth: 1,
                borderColor: palette.border,
              }, previousFinding === null && styles.faded]}
              disabled={previousFinding === null}
              onPress={() => { if (previousFinding) gotoFinding(previousFinding); }}
              accessibilityLabel="Previous finding">
              <Icon name="navigate_before" size={20} color={palette.fg} />
            </TouchableOpacity>
            <TouchableOpacity
              style={[styles.primaryButton, {
                marginTop: 0,
                flexGrow: 0,
                backgroundColor: palette.surface,
                borderWidth: 1,
                borderColor: palette.border,
              }, nextFinding === null && styles.faded]}
              disabled={nextFinding === null}
              onPress={() => { if (nextFinding) gotoFinding(nextFinding); }}
              accessibilityLabel="Next finding">
              <Icon name="navigate_next" size={20} color={palette.fg} />
            </TouchableOpacity>
            <Text style={[styles.cardMeta, { color: palette.muted }]} accessibilityLabel="Finding position">
              {selectedFindingEntry
                ? `Finding ${findingEntries.indexOf(selectedFindingEntry) + 1} of ${findingEntries.length}`
                : `${findingEntries.length} findings`}
            </Text>
          </View>
        )}

        {phase === 'open' && selectedFile && hunkRows.map(({ hunk, lines }) => {
          const key = reviewHunkKeyFor(selectedFile, hunk);
          const ready = reviewHunkIsReady(hunk);
          const isSelected = key === selectedHunkKey;
          const hunkFindings = snapshot
            ? reviewFindingsForHunk(snapshot.reviewFindings, reviewHunkIdentityFor(snapshot.snapshotId, selectedFile, hunk))
            : [];
          const tones = findingTonesFor(key, lines, hunkFindings);
          const hunkAggregate = hunkFindings.length > 0
            ? reviewCategoryLabel(reviewAggregateCategory(hunkFindings))
            : null;
          return (
            <View
              key={`${key}:${hunk.index}`}
              onLayout={(event) => {
                cardPositionsRef.current.set(key, event.nativeEvent.layout.y);
                scrollToTarget();
              }}
              style={[styles.card, { backgroundColor: palette.surface, borderColor: palette.border }]}>
              <TouchableOpacity
                onPress={() => selectHunk(key, ready)}
                disabled={!ready}
                accessibilityLabel={`Hunk ${hunk.header}`}>
                <Text style={[styles.cardMeta, {
                  color: ready ? palette.accent : palette.muted,
                  fontFamily: palette.mono,
                }]}>
                  {`${hunk.header}${ready ? '' : ' · loading…'}`}
                </Text>
                {hunkAggregate ? (
                  <Text
                    style={[styles.cardMeta, { color: findingToneColor(reviewAggregateCategory(hunkFindings) as ReviewFindingCategory, palette) }]}
                    accessibilityLabel={`Auto review ${hunkAggregate}`}>
                    {`${hunkAggregate} · ${hunkFindings.length}`}
                  </Text>
                ) : null}
              </TouchableOpacity>
              {lines.map((line, index) => {
                const mark = tones.get(index);
                return (
                  <Text
                    key={`${key}:${index}`}
                    onLayout={(event) => {
                      linePositionsRef.current.set(`${key}#${index}`, event.nativeEvent.layout.y);
                      scrollToTarget();
                    }}
                    style={[styles.cardPre, {
                      color: lineColor(line.kind),
                      fontFamily: palette.mono,
                      borderLeftWidth: mark ? 3 : 0,
                      borderLeftColor: mark ? mark.tone : 'transparent',
                      backgroundColor: mark?.selected ? palette.border : 'transparent',
                    }]}
                  >
                    {redactSecrets(line.text)}
                  </Text>
                );
              })}
              {isSelected && (
                <View style={{ gap: 8, marginTop: 10 }}>
                  {selectedHunkFindings.length > 0 && (
                    <View style={{ gap: 6 }}>
                      <Text
                        style={[styles.cardMeta, { color: findingToneColor(reviewAggregateCategory(selectedHunkFindings) as ReviewFindingCategory, palette) }]}
                        accessibilityLabel={`Auto review ${reviewCategoryLabel(reviewAggregateCategory(selectedHunkFindings))}`}>
                        {`${reviewCategoryLabel(reviewAggregateCategory(selectedHunkFindings))} · ${selectedHunkFindings.length}`}
                      </Text>
                      {selectedHunkFindings.map((finding, index) => {
                        const isSelectedFinding = !!selectedFindingEntry
                          && selectedFindingEntry.hunkKey === key
                          && selectedFindingEntry.ref.findingIndex === index;
                        return (
                          <TouchableOpacity
                            key={`${key}:finding:${index}`}
                            style={{
                              borderWidth: 1,
                              borderColor: isSelectedFinding ? palette.accent : palette.border,
                              borderRadius: 4,
                              padding: 6,
                            }}
                            onPress={() => selectFindingIndex(key, index)}
                            accessibilityLabel={`Finding ${index}`}>
                            <Text
                              style={[styles.cardMeta, { color: findingToneColor(finding.category, palette) }]}
                              accessibilityLabel={`Finding ${index} category`}>
                              {`${reviewCategoryLabel(finding.category)} · lines ${finding.startLine}–${finding.endLine} · ${finding.side === 'old' ? 'old' : 'new'}`}
                            </Text>
                            <Text style={[styles.rowSub, { color: palette.fg }]}>{finding.summary}</Text>
                            <Text style={[styles.cardPre, { color: palette.muted, fontFamily: palette.mono }]}>{finding.evidence}</Text>
                          </TouchableOpacity>
                        );
                      })}
                    </View>
                  )}
                  {selectedThread?.comments.map((comment, index) => (
                    <View key={`${key}:comment:${index}`} accessibilityLabel={`Review comment ${index}`}>
                      {comment.thinking ? (
                        <ThinkingFold thinking={comment.thinking} palette={palette} />
                      ) : null}
                      <MarkdownBody text={comment.text} palette={palette} />
                      {comment.partial ? (
                        <Text style={[styles.cardMeta, { color: palette.muted }]}>…</Text>
                      ) : null}
                    </View>
                  ))}
                  {selectedThread?.streamingThinking ? (
                    <ThinkingFold thinking={selectedThread.streamingThinking} palette={palette} />
                  ) : null}
                  {selectedThread?.streamingText ? (
                    <MarkdownBody text={selectedThread.streamingText} palette={palette} />
                  ) : null}
                  {selectedThread?.isStreaming && !selectedThread.streamingText ? (
                    <Text style={[styles.cardMeta, { color: palette.muted }]}>Thinking…</Text>
                  ) : null}
                  {selectedThread?.error ? (
                    <Text style={[styles.inlineError, { color: palette.error }]}>
                      {redactSecrets(selectedThread.error)}
                    </Text>
                  ) : null}
                  <TextInput
                    style={[styles.composerInput, {
                      backgroundColor: palette.bg,
                      borderColor: palette.border,
                      color: palette.fg,
                    }]}
                    value={draft}
                    onChangeText={setDraft}
                    placeholder="Comment on this hunk"
                    placeholderTextColor={palette.muted}
                    multiline
                    accessibilityLabel="Hunk comment"
                  />
                  <View style={{ flexDirection: 'row', gap: 8 }}>
                    <TouchableOpacity
                      style={[styles.primaryButton, { flex: 1, marginTop: 0, backgroundColor: palette.accent }, (
                        busy !== null || draft.trim() === ''
                      ) && styles.faded]}
                      disabled={busy !== null || draft.trim() === ''}
                      onPress={() => void submitComment()}
                      accessibilityLabel="Comment">
                      <Text style={[styles.primaryButtonText, { color: palette.bg }]}>Comment</Text>
                    </TouchableOpacity>
                    {selectedThread?.isCancellable && (
                      <TouchableOpacity
                        style={[styles.primaryButton, {
                          flex: 1,
                          marginTop: 0,
                          backgroundColor: palette.error,
                        }, busy !== null && styles.faded]}
                        disabled={busy !== null}
                        onPress={() => void stopReply()}
                        accessibilityLabel="Stop reply">
                        <Text style={[styles.primaryButtonText, { color: palette.bg }]}>Stop</Text>
                      </TouchableOpacity>
                    )}
                  </View>
                </View>
              )}
            </View>
          );
        })}

        {phase === 'open' && selectedFile && !selectedFile.binary && (
          <View style={styles.form}>
            {diffStatus ? (
              <Text style={[styles.rowSub, { color: palette.muted }]}>{diffStatus}</Text>
            ) : null}
            {diffError ? (
              <TouchableOpacity
                style={[styles.primaryButton, {
                  backgroundColor: palette.surface,
                  borderWidth: 1,
                  borderColor: palette.border,
                }]}
                onPress={() => void growWindow(true)}
                accessibilityLabel="Retry diff load">
                <Text style={[styles.primaryButtonText, { color: palette.fg }]}>Retry</Text>
              </TouchableOpacity>
            ) : null}
            {diffPlan?.canLoadMore ? (
              <TouchableOpacity
                style={[styles.primaryButton, {
                  backgroundColor: palette.surface,
                  borderWidth: 1,
                  borderColor: palette.border,
                }, busy !== null && styles.faded]}
                disabled={busy !== null}
                onPress={() => void growWindow()}
                accessibilityLabel="Show more lines">
                <Text style={[styles.primaryButtonText, { color: palette.fg }]}>Show more lines</Text>
              </TouchableOpacity>
            ) : null}
          </View>
        )}

        <View style={styles.bottomPad} />
      </ScrollView>
    </View>
  );
}
