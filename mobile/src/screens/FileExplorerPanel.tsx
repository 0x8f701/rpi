import React, { useCallback, useEffect, useRef, useState } from 'react';
import { ScrollView, Text, TouchableOpacity, View } from 'react-native';
import Icon from 'react-native-vector-icons/MaterialIcons';
import {
  FILE_EXPLORER_DEFAULT_MAX_LINES,
  FILE_EXPLORER_TOO_LARGE_MESSAGE,
  appendPagedLines,
  fileExplorerIsOversized,
  normalizeChanges,
  normalizeDiff,
  normalizeList,
  normalizeRead,
  normalizeBlame,
  parentExplorerPath,
  type FileExplorerChange,
  type FileExplorerDiffResult,
  type FileExplorerEntry,
  type FileExplorerReadResult,
  type FileExplorerBlameResult,
} from '../fileExplorer';
import { redactSecrets } from '../redact';
import type { RunnerClient } from '../runnerClient';
import { isRunnerDisconnected } from '../transportError';
import {
  buildFileConversationPromptPayload,
  createConversationId,
  parseFileConversationClose,
  parseFileConversationErrorCode,
  parseFileConversationSnapshot,
  type FileConversationRangeWire,
  type FileConversationSnapshotData,
} from '../protocol';
import { FileConversationView } from './FileConversationView';
import { AppBar, styles, type Palette } from '../ui';

/** Snapshot poll while Ask mode is open (side-chat cadence). */
const FILE_CONVERSATION_SNAPSHOT_POLL_MS = 2000;

/** One conversation per asked-about file: a client-minted UUID bound to the
 *  exact resolved preview path the file was opened with. Native has no dock
 *  tab strip, so dock parity is by path: the binding lives for the PANEL's
 *  lifetime and only the panel teardown closes it. */
interface FileConversationBinding {
  conversationId: string;
  path: string;
  /** The record IS the conversation's client-side state: draft and selection
   *  survive navigation without saving, pending is per file (a slow prompt on
   *  one file never blocks another), and the last verified snapshot plus the
   *  unreadable flag are the presented view's sole source. */
  draft: string;
  selection: FileConversationRangeWire | null;
  /** Bumped on every composer or selection edit. A prompt records the value
   *  at send and may clear only while this is still that revision. */
  composerRevision: number;
  pending: boolean;
  /** Set only after an open was sent and its answer was lost. A typed
   *  failure of the first answer still proves this id was never created. */
  openUncertain: boolean;
  /** Explicit close in flight. Ask must not resume or replace this record
   *  until that close settles. */
  closing: boolean;
  /** Per-record request generation: supersession and eviction guards are
   *  per binding, so another file's mutations never void this record's
   *  in-flight responses. */
  seq: number;
  snapshot: FileConversationSnapshotData | null;
  unreadable: boolean;
}

type ExplorerPane = 'tree' | 'file' | 'changes' | 'diff' | 'ask';

export function FileExplorerPanel(props: {
  palette: Palette;
  client: RunnerClient;
  sessionId: string;
  onClose: () => void;
  onError: (message: string) => void;
  onRegisterBackHandler?: (handler: (() => boolean) | null) => void;
}): React.JSX.Element {
  const { palette } = props;
  const [pane, setPane] = useState<ExplorerPane>('tree');
  const [path, setPath] = useState('');
  const [entries, setEntries] = useState<FileExplorerEntry[] | null>(null);
  const [preview, setPreview] = useState<FileExplorerReadResult | null>(null);
  const previewRef = useRef(preview);
  previewRef.current = preview;
  const [changes, setChanges] = useState<FileExplorerChange[] | null>(null);
  const [diff, setDiff] = useState<FileExplorerDiffResult | null>(null);
  const [busy, setBusy] = useState(false);
  /** File-pane line selection. Ask's record remains the conversation owner;
   *  Ask force-open copies this range onto that record without clearing it. */
  const [sourceSelection, setSourceSelection] = useState<FileConversationRangeWire | null>(null);
  const [sourceFocus, setSourceFocus] = useState(0);
  const [blame, setBlame] = useState<FileExplorerBlameResult | null>(null);
  const [blamePending, setBlamePending] = useState(false);
  const blameGen = useRef(0);
  const mountedRef = useRef(true);
  const generationRef = useRef(0);

  const live = (generation: number): boolean => mountedRef.current && generationRef.current === generation;

  // ---- File conversations (Ask): one cohesive record per file path ----
  //
  // bindingsRef is the SINGLE authoritative store: each asked-about file owns
  // ONE record carrying its conversationId, bound path, draft, selection,
  // pending flag, last verified snapshot, and unreadable flag. The panel
  // keeps NO parallel copies — the presented view reads its record live and a
  // version counter re-renders after record mutations. Consequences, by
  // design: navigating away saves nothing (records are authoritative;
  // A → B → A reads its own record), a valid accepted ack updates the
  // retained record even off-screen while only the presented view toasts,
  // pending is per record (a slow prompt on A never disables B's composer),
  // and a failed explicit close retains the record whole. The explicit
  // Close-conversation header action is the one user-facing eviction; only
  // panel teardown closes everything else.
  //
  // Presentation and bookkeeping stay apart: each record's own `seq` voids
  // ITS superseded or evicted requests (another file's mutations never void
  // them), presentingBinding() (ask pane + current record) gates toasts and
  // forced navigation, and typed-not_found evictions / refused-open cleanup
  // stay truthful off-screen.

  const [conversation, setConversation] = useState<FileConversationBinding | null>(null);
  const [convVersion, setConvVersion] = useState(0);
  const bindingsRef = useRef<Map<string, FileConversationBinding>>(new Map());
  const convRef = useRef<FileConversationBinding | null>(null);
  const paneRef = useRef<ExplorerPane>(pane);
  // paneRef mirrors the committed pane for render-time reads; the ONE place
  // allowed to lead the render is entering Ask view below, where an immediate
  // refresh must see the presentation it just performed.
  paneRef.current = pane;

  /** Record mutations do not change object identity: bump the version so the
   *  presented view re-reads its record. */
  const bumpConv = useCallback(() => setConvVersion((version) => version + 1), []);

  /** Whether this record is the conversation the user is actually looking
   *  at: gates toasts and forced navigation, never bookkeeping. */
  const presentingBinding = (binding: FileConversationBinding): boolean =>
    paneRef.current === 'ask' && convRef.current === binding;

  /** A response applies only when it is THIS conversation AND the exact bound
   *  resolved preview path: parseFileConversationSnapshot enforces the id,
   *  the path comparison is deliberately the consumer's. The verified
   *  snapshot lands in the record — truthful off-screen too. */
  const applyConversationSnapshot = useCallback((data: unknown, binding: FileConversationBinding): boolean => {
    const parsed = parseFileConversationSnapshot(data, binding.conversationId);
    if (!parsed || parsed.path !== binding.path) return false;
    binding.snapshot = parsed;
    binding.unreadable = false;
    bumpConv();
    return true;
  }, [bumpConv]);

  /** Close exactly one controller. `file_conversation_not_found` is the one
   *  documented nonfatal cleanup (already closed/expired); every other
   *  failure — including during teardown — surfaces centrally. */
  const closeBinding = useCallback((binding: FileConversationBinding) => {
    void props.client.command(
      { type: 'file_conversation_close', conversationId: binding.conversationId },
      props.sessionId,
    ).then((response) => {
      const confirmed = response.success
        && parseFileConversationClose(response.data, binding.conversationId) !== null;
      if (confirmed) return;
      if (parseFileConversationErrorCode(response.data) === 'file_conversation_not_found') return;
      props.onError(response.error || 'Close failed');
    }).catch((err: unknown) => {
      props.onError(err instanceof Error ? err.message : String(err));
    });
  }, [props.client, props.onError, props.sessionId]);

  /** Panel teardown — closes every record minted here. */
  useEffect(() => {
    return () => {
      convRef.current = null;
      for (const binding of bindingsRef.current.values()) closeBinding(binding);
      bindingsRef.current.clear();
    };
  }, [closeBinding]);

  /** The engine no longer has this conversation (typed not_found): evict the
   *  record so no phantom resumes. On screen this also surfaces the detail
   *  once and leaves Ask mode; off-screen it is silent bookkeeping that
   *  never touches the live view. */
  const evictBinding = useCallback((binding: FileConversationBinding, detail: string) => {
    bindingsRef.current.delete(binding.path);
    binding.pending = false;
    if (paneRef.current === 'ask' && convRef.current === binding) {
      convRef.current = null;
      setConversation(null);
      setPane('file');
      props.onError(detail);
    }
    bumpConv();
  }, [props.onError, bumpConv]);

  const refreshConversationSnapshot = useCallback(async () => {
    const binding = convRef.current;
    if (!binding || !presentingBinding(binding)) return;
    const seq = binding.seq;
    try {
      const response = await props.client.command(
        { type: 'file_conversation_snapshot', conversationId: binding.conversationId },
        props.sessionId,
      );
      if (binding.seq !== seq || bindingsRef.current.get(binding.path) !== binding) return;
      if (!presentingBinding(binding)) return; // left Ask mid-flight: invalidated
      if (!response.success) {
        if (parseFileConversationErrorCode(response.data) === 'file_conversation_not_found') {
          evictBinding(binding, response.error || 'File conversation is gone');
          return;
        }
        props.onError(response.error || 'Snapshot failed');
        return;
      }
      if (!applyConversationSnapshot(response.data, binding)) {
        binding.unreadable = true;
        bumpConv();
        props.onError('Invalid file conversation snapshot');
      }
    } catch (err) {
      if (binding.seq !== seq || bindingsRef.current.get(binding.path) !== binding) return;
      if (!presentingBinding(binding)) return;
      props.onError(err instanceof Error ? err.message : String(err));
    }
  }, [props.client, props.onError, props.sessionId, applyConversationSnapshot, evictBinding]);

  useEffect(() => {
    if (pane !== 'ask' || conversation === null) return;
    const timer = setInterval(() => { void refreshConversationSnapshot(); }, FILE_CONVERSATION_SNAPSHOT_POLL_MS);
    return () => clearInterval(timer);
  }, [pane, conversation, refreshConversationSnapshot]);

  /** Ask press: present the file's record (ITS OWN draft, selection and
   *  snapshot — A → B → A loses nothing), or lazily mint a UUID and open one
   *  bound to the exact resolved preview path. */
  const openConversation = useCallback(async () => {
    if (!preview) return;
    const existing = bindingsRef.current.get(preview.path);
    if (existing && existing.closing) return;
    if (sourceSelection && existing) {
      existing.selection = sourceSelection;
      existing.composerRevision += 1;
    }
    if (existing && existing.snapshot !== null && convRef.current === existing) {
      // Confirmed record (source ⇄ Ask toggle): re-enter and refresh. An
      // unconfirmed record falls through and re-opens its retained id.
      setPane('ask');
      paneRef.current = 'ask'; // deliberate presentation: admission is true now
      void refreshConversationSnapshot();
      return;
    }
    let binding: FileConversationBinding;
    if (existing) {
      binding = existing;
    } else {
      binding = {
        conversationId: createConversationId(),
        path: preview.path,
        draft: '',
        selection: sourceSelection,
        composerRevision: 0,
        pending: true, // the open itself is this record's pending work
        openUncertain: false,
        closing: false,
        seq: 0,
        snapshot: null,
        unreadable: false,
      };
      bindingsRef.current.set(binding.path, binding);
    }
    convRef.current = binding;
    setConversation(binding);
    setPane('ask');
    paneRef.current = 'ask'; // deliberate presentation: admission is true now
    bumpConv();
    if (existing && existing.snapshot !== null) {
      // Resume of a confirmed record: re-read state, never re-open — the
      // conversationId stays the one this file already owns.
      await refreshConversationSnapshot();
      return;
    }
    // First open, or an idempotent retry of one whose ack was lost: the
    // retained id is reused, never minted again.
    binding.pending = true;
    bumpConv();
    const seq = binding.seq;
    try {
      const response = await props.client.command(
        { type: 'file_conversation_open', conversationId: binding.conversationId, path: binding.path },
        props.sessionId,
      );
      if (binding.seq !== seq || bindingsRef.current.get(binding.path) !== binding) return;
      binding.pending = false;
      if (!response.success) {
        const code = parseFileConversationErrorCode(response.data);
        // not_found is authoritative absence. Capacity is too: the engine
        // checks it only after this id is absent, so it proves the open was
        // never created. Any other typed code retires the id only before its
        // creation is uncertain. Once an open's answer was lost, a later
        // typed read failure can belong to a binding that ack already created.
        const absent = code === 'file_conversation_not_found' || code === 'file_conversation_capacity';
        if (absent || (code !== null && !binding.openUncertain)) {
          bindingsRef.current.delete(binding.path);
          if (presentingBinding(binding)) {
            convRef.current = null;
            setConversation(null);
            setPane('file');
            props.onError(response.error || 'Ask failed');
          }
          bumpConv();
          return;
        }
        if (presentingBinding(binding)) {
          props.onError(response.error || 'Ask failed');
          if (code !== null) {
            convRef.current = null;
            setConversation(null);
            setPane('file');
          }
        }
        bumpConv();
        return;
      }
      if (!applyConversationSnapshot(response.data, binding)) {
        // Open succeeded server-side; keep the record and let the poll heal
        // the view, but never render an unverified payload.
        binding.unreadable = true;
        if (presentingBinding(binding)) props.onError('Invalid file conversation response');
      }
      bumpConv();
    } catch (err) {
      if (binding.seq !== seq || bindingsRef.current.get(binding.path) !== binding) return;
      binding.pending = false;
      binding.openUncertain = true;
      // A disconnect or a timeout after send proves nothing: the server may
      // already have created this id. Keep it so the retry re-opens the SAME
      // conversation and teardown still closes it. A later typed read failure
      // is not authoritative absence. Only not_found, a typed refusal before
      // any answer was lost, or a confirmed close retires it.
      if (!isRunnerDisconnected(err) && presentingBinding(binding)) {
        props.onError(err instanceof Error ? err.message : String(err));
      }
      bumpConv();
    }
  }, [preview, sourceSelection, props.client, props.onError, props.sessionId, refreshConversationSnapshot, applyConversationSnapshot, bumpConv]);

  /** Send: the protocol builder is the ONLY prompt path (8192-byte UTF-8
   *  gate, all-or-nothing selection). Pending is per record, so a slow prompt
   *  on one file never disables another's composer. A valid accepted ack
   *  updates the retained record even off-screen; only the presented view
   *  toasts. */
  const sendConversationPrompt = useCallback(async () => {
    const binding = convRef.current;
    if (!binding || paneRef.current !== 'ask' || binding.pending) return;
    const message = binding.draft.trim();
    if (message === '') return;
    const payload = buildFileConversationPromptPayload(binding.conversationId, message, binding.selection ?? undefined);
    if (!payload.ok) {
      props.onError(payload.error);
      return;
    }
    const submittedRevision = binding.composerRevision;
    binding.pending = true;
    bumpConv();
    binding.seq += 1;
    const seq = binding.seq;
    try {
      const response = await props.client.command(payload.command, props.sessionId);
      if (binding.seq !== seq || bindingsRef.current.get(binding.path) !== binding) return;
      const presenting = presentingBinding(binding);
      if (!response.success) {
        if (parseFileConversationErrorCode(response.data) === 'file_conversation_not_found') {
          evictBinding(binding, response.error || 'File conversation is gone');
          return;
        }
        if (presenting) props.onError(response.error || 'Ask failed');
        return;
      }
      if (!applyConversationSnapshot(response.data, binding) || binding.snapshot === null) {
        binding.unreadable = true;
        if (presenting) props.onError('Invalid file conversation response');
        bumpConv();
        return;
      }
      const parsed = binding.snapshot;
      if (parsed.accepted === true && parsed.busy !== true
          && binding.composerRevision === submittedRevision) {
        binding.draft = '';
        binding.selection = null;
        if (previewRef.current?.path === binding.path) {
          blameGen.current += 1;
          setBlame(null);
          setBlamePending(false);
          setSourceSelection(null);
        }
      } else if (parsed.accepted === false && presenting) {
        props.onError('The file conversation is busy — wait for the current reply');
      }
      bumpConv();
    } catch (err) {
      if (binding.seq !== seq || bindingsRef.current.get(binding.path) !== binding) return;
      if (presentingBinding(binding)) props.onError(err instanceof Error ? err.message : String(err));
    } finally {
      // Only the current generation clears the flag it set: a superseded
      // request never cancels the work that superseded it.
      if (seq === binding.seq && binding.pending) {
        binding.pending = false;
        bumpConv();
      }
    }
  }, [props.client, props.onError, props.sessionId, applyConversationSnapshot, evictBinding, bumpConv]);

  /** Stop targets exactly the bound conversation id — never another file's
   *  conversation, and never a close. */
  const stopConversationReply = useCallback(async () => {
    const binding = convRef.current;
    if (!binding || paneRef.current !== 'ask' || binding.pending) return;
    binding.pending = true;
    bumpConv();
    binding.seq += 1;
    const seq = binding.seq;
    try {
      const response = await props.client.command(
        { type: 'file_conversation_abort', conversationId: binding.conversationId },
        props.sessionId,
      );
      if (binding.seq !== seq || bindingsRef.current.get(binding.path) !== binding) return;
      const presenting = presentingBinding(binding);
      if (!response.success) {
        if (parseFileConversationErrorCode(response.data) === 'file_conversation_not_found') {
          evictBinding(binding, response.error || 'File conversation is gone');
          return;
        }
        if (presenting) props.onError(response.error || 'Stop failed');
        return;
      }
      if (!applyConversationSnapshot(response.data, binding)) {
        binding.unreadable = true;
        if (presenting) props.onError('Invalid file conversation response');
        bumpConv();
      }
    } catch (err) {
      if (binding.seq !== seq || bindingsRef.current.get(binding.path) !== binding) return;
      if (presentingBinding(binding)) props.onError(err instanceof Error ? err.message : String(err));
    } finally {
      if (seq === binding.seq && binding.pending) {
        binding.pending = false;
        bumpConv();
      }
    }
  }, [props.client, props.onError, props.sessionId, applyConversationSnapshot, evictBinding, bumpConv]);

  /** Explicit Close conversation (header action): the one user-facing way to
   *  evict a record and free one of the engine's 8 slots. The record needs no
   *  saving on the way out — it IS the state; it is dropped only on a
   *  confirmed close or the typed not_found no-op, and a failed close keeps
   *  it whole (draft and selection included) and surfaces. */
  const closeCurrentConversation = useCallback(() => {
    const binding = convRef.current;
    if (!binding || paneRef.current !== 'ask' || binding.closing) return;
    binding.closing = true;
    convRef.current = null;
    setConversation(null);
    setPane('file');
    bumpConv();
    const finishClose = () => {
      binding.closing = false;
      if (bindingsRef.current.get(binding.path) === binding) bumpConv();
    };
    void props.client.command(
      { type: 'file_conversation_close', conversationId: binding.conversationId },
      props.sessionId,
    ).then((response) => {
      if (bindingsRef.current.get(binding.path) !== binding) return;
      const confirmed = response.success
        && parseFileConversationClose(response.data, binding.conversationId) !== null;
      if (confirmed || parseFileConversationErrorCode(response.data) === 'file_conversation_not_found') {
        if (bindingsRef.current.get(binding.path) === binding) bindingsRef.current.delete(binding.path);
        return;
      }
      props.onError(response.error || 'Close failed');
    }).catch((err: unknown) => {
      if (bindingsRef.current.get(binding.path) !== binding) return;
      props.onError(err instanceof Error ? err.message : String(err));
    }).finally(finishClose);
  }, [props.client, props.onError, props.sessionId, bumpConv]);

  const listDir = useCallback(async (dir: string) => {
    const generation = generationRef.current;
    setBusy(true);
    try {
      const command = dir ? { type: 'file_explorer_list' as const, path: dir } : { type: 'file_explorer_list' as const };
      const response = await props.client.command(command, props.sessionId);
      if (!live(generation)) return;
      if (!response.success) {
        setEntries(null);
        props.onError(response.error || 'Files failed');
        return;
      }
      const parsed = normalizeList(response.data);
      setEntries(parsed ? parsed.entries : null);
      setPath(dir);
      setPane('tree');
    } catch (err) {
      if (live(generation)) props.onError(err instanceof Error ? err.message : String(err));
    } finally {
      if (live(generation)) setBusy(false);
    }
  }, [props.client, props.onError, props.sessionId]);

  const openFile = async (filePath: string) => {
    const generation = generationRef.current;
    blameGen.current += 1;
    setSourceSelection(null);
    setBlame(null);
    setBlamePending(false);
    setBusy(true);
    try {
      const response = await props.client.command({
        type: 'file_explorer_read',
        path: filePath,
        maxLines: FILE_EXPLORER_DEFAULT_MAX_LINES,
      }, props.sessionId);
      if (!live(generation)) return;
      if (!response.success) {
        setPreview(null);
        props.onError(response.error || 'Read failed');
        return;
      }
      const parsed = normalizeRead(response.data);
      setPreview(parsed);
      setPane('file');
    } catch (err) {
      if (live(generation)) props.onError(err instanceof Error ? err.message : String(err));
    } finally {
      if (live(generation)) setBusy(false);
    }
  };

  const loadMore = async () => {
    if (!preview?.nextCursor || busy) return;
    const generation = generationRef.current;
    setBusy(true);
    try {
      const response = await props.client.command({
        type: 'file_explorer_read',
        path: preview.path,
        cursor: preview.nextCursor,
        maxLines: FILE_EXPLORER_DEFAULT_MAX_LINES,
      }, props.sessionId);
      if (!live(generation)) return;
      if (!response.success) {
        props.onError(response.error || 'Read failed');
        return;
      }
      const page = normalizeRead(response.data);
      if (page) setPreview(appendPagedLines(preview, page));
    } catch (err) {
      if (live(generation)) props.onError(err instanceof Error ? err.message : String(err));
    } finally {
      if (live(generation)) setBusy(false);
    }
  };

  const loadChanges = useCallback(async () => {
    const generation = generationRef.current;
    setBusy(true);
    try {
      const response = await props.client.command({ type: 'file_explorer_changes' }, props.sessionId);
      if (!live(generation)) return;
      if (!response.success) {
        setChanges(null);
        props.onError(response.error || 'Changes failed');
        return;
      }
      const parsed = normalizeChanges(response.data);
      setChanges(parsed ? parsed.files : null);
      setPane('changes');
    } catch (err) {
      if (live(generation)) props.onError(err instanceof Error ? err.message : String(err));
    } finally {
      if (live(generation)) setBusy(false);
    }
  }, [props.client, props.onError, props.sessionId]);

  const openDiff = async (filePath: string) => {
    const generation = generationRef.current;
    setBusy(true);
    try {
      const response = await props.client.command({
        type: 'file_explorer_diff',
        path: filePath,
        maxLines: FILE_EXPLORER_DEFAULT_MAX_LINES,
      }, props.sessionId);
      if (!live(generation)) return;
      if (!response.success) {
        setDiff(null);
        props.onError(response.error || 'Diff failed');
        return;
      }
      setDiff(normalizeDiff(response.data));
      setPane('diff');
    } catch (err) {
      if (live(generation)) props.onError(err instanceof Error ? err.message : String(err));
    } finally {
      if (live(generation)) setBusy(false);
    }
  };

  /** Blame the file pane's current selection. A path, session, or selection
   *  change bumps blameGen, so a late answer cannot paint another owner. */
  const requestBlame = async (range: FileConversationRangeWire) => {
    if (!preview || fileExplorerIsOversized(preview.sizeBytes)) return;
    const count = range.endLine - range.startLine + 1;
    if (range.startLine < 1 || range.endLine > preview.lines.length || count < 1 || count > 2000) return;
    const owner = { path: preview.path, sessionId: props.sessionId, startLine: range.startLine, endLine: range.endLine };
    const token = ++blameGen.current;
    setBlame(null);
    setBlamePending(true);
    try {
      const response = await props.client.command({
        type: 'file_explorer_blame',
        path: owner.path,
        startLine: owner.startLine,
        endLine: owner.endLine,
      }, owner.sessionId);
      if (token !== blameGen.current || !mountedRef.current) return;
      if (!response.success) {
        props.onError(redactSecrets(response.error || 'Blame failed'));
        return;
      }
      const parsed = normalizeBlame(response.data, owner);
      if (!parsed) {
        props.onError(redactSecrets('Invalid file blame'));
        return;
      }
      setBlame(parsed);
    } catch (err) {
      if (token !== blameGen.current || !mountedRef.current) return;
      props.onError(redactSecrets(err instanceof Error ? err.message : String(err)));
    } finally {
      if (token === blameGen.current) setBlamePending(false);
    }
  };

  const selectSourceLine = (line: number) => {
    blameGen.current += 1;
    setBlame(null);
    setBlamePending(false);
    setSourceSelection((current) => {
      if (current && line >= current.startLine && line <= current.endLine) return null;
      if (!current) return { startLine: line, endLine: line };
      return { startLine: Math.min(current.startLine, line), endLine: Math.max(current.endLine, line) };
    });
  };

  const askAboutSelection = () => {
    if (!preview || !sourceSelection) return;
    setSourceFocus((value) => value + 1);
    void openConversation();
  };


  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      generationRef.current += 1;
    };
  }, []);

  useEffect(() => {
    generationRef.current += 1;
    blameGen.current += 1;
    setSourceSelection(null);
    setBlame(null);
    setBlamePending(false);
    void listDir('');
  }, [listDir]);

  const goBack = useCallback(() => {
    // Ask → source keeps the conversation bound (dock parity); no navigation
    // inside the panel ever closes a conversation — only teardown does.
    if (pane === 'ask') {
      setPane('file');
      return;
    }
    if (pane === 'file' || pane === 'diff') {
      setPane(pane === 'diff' ? 'changes' : 'tree');
      return;
    }
    if (pane === 'changes') {
      setPane('tree');
      return;
    }
    const parent = parentExplorerPath(path);
    if (parent === null) {
      props.onClose();
      return;
    }
    void listDir(parent);
  }, [pane, path, props.onClose, listDir]);

  const registerBackHandler = props.onRegisterBackHandler;
  useEffect(() => {
    registerBackHandler?.(() => {
      goBack();
      return true;
    });
    return () => registerBackHandler?.(null);
  }, [registerBackHandler, goBack]);

  const title = (pane === 'file' || pane === 'ask') && preview
    ? preview.path
    : pane === 'diff' && diff
      ? diff.path
      : pane === 'changes'
        ? 'Changes'
        : (path || 'Files');

  return (
    <View style={[styles.screen, { backgroundColor: palette.bg }]}>
      <AppBar
        palette={palette}
        title={title}
        onBack={goBack}
        actions={pane === 'ask' && conversation ? [
          { icon: 'close', label: 'Close conversation', onPress: () => { closeCurrentConversation(); } },
        ] : undefined}
      />
      <View style={{ flexDirection: 'row', borderBottomWidth: 1, borderBottomColor: palette.border }}>
        <TouchableOpacity
          style={[styles.primaryButton, { flex: 1, marginTop: 0, backgroundColor: pane === 'tree' || pane === 'file' || pane === 'ask' ? palette.accent : palette.surface }]}
          onPress={() => { setPane('tree'); if (entries === null) void listDir(path); }}
          accessibilityLabel="Files">
          <Icon name="folder" size={18} color={pane === 'tree' || pane === 'file' || pane === 'ask' ? palette.bg : palette.fg} />
          <Text style={[styles.primaryButtonText, { color: pane === 'tree' || pane === 'file' || pane === 'ask' ? palette.bg : palette.fg }]}>Files</Text>
        </TouchableOpacity>
        <TouchableOpacity
          style={[styles.primaryButton, { flex: 1, marginTop: 0, backgroundColor: pane === 'changes' || pane === 'diff' ? palette.accent : palette.surface }]}
          onPress={() => void loadChanges()}
          accessibilityLabel="Changes">
          <Icon name="compare" size={18} color={pane === 'changes' || pane === 'diff' ? palette.bg : palette.fg} />
          <Text style={[styles.primaryButtonText, { color: pane === 'changes' || pane === 'diff' ? palette.bg : palette.fg }]}>Changes</Text>
        </TouchableOpacity>
      </View>
      {pane === 'ask' && preview && conversation && conversation.path === preview.path ? (
        <FileConversationView
          palette={palette}
          path={conversation.path}
          preview={fileExplorerIsOversized(preview.sizeBytes) ? null : preview}
          snapshot={conversation.snapshot}
          unreadable={conversation.unreadable}
          draft={conversation.draft}
          busy={conversation.pending}
          selection={conversation.selection}
          onDraftChange={(text) => {
            const binding = convRef.current;
            if (!binding) return;
            binding.draft = text;
            binding.composerRevision += 1;
            bumpConv();
          }}
          onSelectionChange={(next) => {
            const binding = convRef.current;
            if (!binding) return;
            binding.selection = next;
            binding.composerRevision += 1;
            blameGen.current += 1;
            setBlame(null);
            setBlamePending(false);
            setSourceSelection(next);
            bumpConv();
          }}
          onSend={() => { void sendConversationPrompt(); }}
          onStop={() => { void stopConversationReply(); }}
          focusComposer={sourceFocus}
          blame={blame}
          blamePending={blamePending}
          onBlame={conversation.selection ? () => { void requestBlame(conversation.selection!); } : undefined}
        />
      ) : (
      <ScrollView style={styles.list}>
        {pane === 'tree' && entries === null && (
          <Text style={[styles.emptyText, { color: palette.muted, padding: 24 }]}>{busy ? '…' : '--'}</Text>
        )}
        {pane === 'tree' && entries && entries.length === 0 && (
          <Text style={[styles.emptyText, { color: palette.muted, padding: 24 }]}>No files.</Text>
        )}
        {pane === 'tree' && entries?.map((entry) => (
          <TouchableOpacity
            key={entry.path}
            style={[styles.listRow, { backgroundColor: palette.surface, borderBottomColor: palette.border }]}
            onPress={() => { if (entry.kind === 'dir') void listDir(entry.path); else void openFile(entry.path); }}
            accessibilityLabel={entry.kind === 'dir' ? `Open folder ${entry.name}` : `Open file ${entry.path}`}>
            <Icon name={entry.kind === 'dir' ? 'folder' : 'description'} size={22} color={palette.accent} />
            <View style={styles.rowBody}>
              <Text style={[styles.rowTitle, { color: palette.fg }]} numberOfLines={1}>{entry.name}</Text>
              <Text style={[styles.rowSub, { color: palette.muted }]} numberOfLines={1}>{entry.path}</Text>
            </View>
          </TouchableOpacity>
        ))}
        {pane === 'file' && preview && (
          <View style={styles.form}>
            {fileExplorerIsOversized(preview.sizeBytes) ? (
              <Text style={[styles.rowTitle, { color: palette.muted }]}>{FILE_EXPLORER_TOO_LARGE_MESSAGE}</Text>
            ) : preview.lines.map((line, index) => {
              const lineNumber = index + 1;
              const selected = sourceSelection != null
                && lineNumber >= sourceSelection.startLine
                && lineNumber <= sourceSelection.endLine;
              return (
                <TouchableOpacity
                  key={lineNumber}
                  onPress={() => selectSourceLine(lineNumber)}
                  accessibilityRole="button"
                  accessibilityLabel={`Line ${lineNumber}`}
                  accessibilityState={{ selected }}
                  style={{ flexDirection: 'row', minHeight: 22, backgroundColor: selected ? palette.accent : palette.bg }}>
                  <Text style={{ width: 40, textAlign: 'right', paddingRight: 8, color: selected ? palette.bg : palette.muted, fontFamily: palette.mono, fontSize: 11 }}>
                    {lineNumber}
                  </Text>
                  <Text style={{ flex: 1, color: selected ? palette.bg : palette.fg, fontFamily: palette.mono, fontSize: 12 }} numberOfLines={1}>
                    {line.length === 0 ? ' ' : redactSecrets(line)}
                  </Text>
                </TouchableOpacity>
              );
            })}
            <Text style={[styles.cardMeta, { color: palette.muted }]}>
              {preview.lines.length} / {preview.totalLines}
            </Text>
            {preview.nextCursor != null && (
              <TouchableOpacity
                style={[styles.primaryButton, { backgroundColor: palette.accent }, busy && styles.faded]}
                disabled={busy}
                onPress={() => void loadMore()}
                accessibilityLabel="Load more">
                <Text style={[styles.primaryButtonText, { color: palette.bg }]}>Load more</Text>
              </TouchableOpacity>
            )}
            {sourceSelection && (
              <View style={{ flexDirection: 'row', alignItems: 'center' }} accessibilityLabel="Selected lines">
                <Text style={[styles.rowSub, { color: palette.fg, flex: 1 }]}>
                  Selected lines {sourceSelection.startLine}–{sourceSelection.endLine}
                </Text>
                <TouchableOpacity
                  onPress={askAboutSelection}
                  accessibilityLabel="Ask about selection"
                  style={{ minHeight: 44, justifyContent: 'center', paddingHorizontal: 8 }}>
                  <Text style={{ color: palette.accent }}>Ask</Text>
                </TouchableOpacity>
                <TouchableOpacity
                  onPress={() => { void requestBlame(sourceSelection); }}
                  disabled={blamePending}
                  accessibilityLabel="Blame selected lines"
                  style={{ minHeight: 44, justifyContent: 'center', paddingHorizontal: 8 }}>
                  <Text style={{ color: palette.accent }}>Blame</Text>
                </TouchableOpacity>
              </View>
            )}
            {blamePending && (
              <Text style={[styles.rowSub, { color: palette.muted, minHeight: 20 }]} accessibilityLabel="Blame loading">Blame…</Text>
            )}
            {blame && !blamePending && (
              <View accessibilityLabel="Blame attribution">
                {blame.lines.map((line) => (
                  <Text key={line.currentLine} style={[styles.rowSub, { color: palette.muted, fontFamily: palette.mono }]} numberOfLines={1}>
                    {`${line.currentLine} ${line.commit == null ? 'Uncommitted' : line.author}${line.commit == null ? '' : ` ${line.commit.slice(0, 7)}`} ${new Date(line.time * 1000).toISOString().slice(0, 10)}`}
                  </Text>
                ))}
              </View>
            )}
            <TouchableOpacity
              style={[styles.primaryButton, { backgroundColor: palette.accent }]}
              onPress={() => { void openConversation(); }}
              accessibilityLabel="Ask about this file">
              <Icon name="forum" size={18} color={palette.bg} />
              <Text style={[styles.primaryButtonText, { color: palette.bg }]}>Ask</Text>
            </TouchableOpacity>
          </View>
        )}
        {pane === 'changes' && changes === null && (
          <Text style={[styles.emptyText, { color: palette.muted, padding: 24 }]}>{busy ? '…' : '--'}</Text>
        )}
        {pane === 'changes' && changes && changes.length === 0 && (
          <Text style={[styles.emptyText, { color: palette.muted, padding: 24 }]}>No changes.</Text>
        )}
        {pane === 'changes' && changes?.map((change) => (
          <TouchableOpacity
            key={change.path}
            style={[styles.listRow, { backgroundColor: palette.surface, borderBottomColor: palette.border }]}
            onPress={() => void openDiff(change.path)}
            accessibilityLabel={`Diff ${change.path}`}>
            <View style={styles.rowBody}>
              <Text style={[styles.rowTitle, { color: palette.fg }]} numberOfLines={1}>{change.path}</Text>
              <Text style={[styles.rowSub, { color: palette.muted }]}>{change.status}</Text>
            </View>
          </TouchableOpacity>
        ))}
        {pane === 'diff' && diff && (
          <View style={styles.form}>
            <Text style={[styles.cardPre, { color: palette.fg, fontFamily: palette.mono }]}>
              {redactSecrets(diff.lines.join('\n'))}
            </Text>
          </View>
        )}
        <View style={styles.bottomPad} />
      </ScrollView>
      )}
    </View>
  );
}
