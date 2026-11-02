import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  AppState,
  Clipboard,
  Image,
  Modal,
  ScrollView,
  Text,
  TextInput,
  TouchableOpacity,
  View,
  type AppStateStatus,
} from 'react-native';
import Icon from 'react-native-vector-icons/MaterialIcons';
import { WebView } from 'react-native-webview';
import {
  acceptPicked,
  attachmentMeta,
  attachmentPickerHtml,
  attachmentSkipMessage,
  beginPickStream,
  buildPromptCommand,
  canSendAttachmentToTmux,
  completePdfUpload,
  completeVideoUpload,
  finishPickStream,
  formatSkipSummary,
  markUploading,
  PASTED_TEXT_NAME,
  parsePickEvent,
  planPastedText,
  pushPickChunk,
  removeAttachment,
  sendAttachmentToTmux,
  tmuxFileDestination,
  uploadVideoAttachment,
  type Attachment,
  type AttachmentSkip,
  type PickBegin,
  type PickStream,
  type UploadResult,
} from '../attachments';
import { emptyActivityStripState, reconcileActivityStrip, applyActivityStripEvent, activityStripView } from '../activityStrip';
import { SteeringQueue } from '../SteeringQueue';
import { dropSteeringAt, steeringPreviewsFromMessages } from '../steeringQueue';
import { uploadPdfAttachment } from '../chatSync';
import {
  parseForkMessagesPage,
  parseLifecycleTarget,
  parseMessages,
  parseRuntimeIdentity,
  parseSessionCatalog,
  parseSessionCopyContext,
  parseSessionHandoff,
  type ForkMessageWire,
  type RunnerCommand,
  type RuntimeIdentity,
  type SessionCatalogRow,
} from '../protocol';
import { registry } from '../runtime';
import { readFrameType } from '../reconnect';
import type { RunnerClient } from '../runnerClient';
import {
  goalWire,
  loopWire,
  parseModelSpec,
  parseSlashLine,
  resolveSlashAction,
  WEB_COMPOSER_COMMANDS,
} from '../slash';
import { applySessionEvent, emptyTranscript, messagesToItems, type TranscriptState } from '../transcriptItems';
import { planTurnStreaming } from '../turnStreaming';
import { isRunnerDisconnected, RunnerDisconnectedError } from '../transportError';
import { TranscriptItemView } from '../TranscriptItemView';
import { AppBar, styles, type Palette } from '../ui';
import { ActivityPanel } from './ActivityPanel';
import { LoopPanel } from './LoopPanel';
import { FileExplorerPanel } from './FileExplorerPanel';
import { GoalPanel } from './GoalPanel';
import { JobsPanel } from './JobsPanel';
import { PersonasPanel } from './PersonasPanel';
import { ProcessesPanel } from './ProcessesPanel';
import { ReviewPanel } from './ReviewPanel';
import { SessionSettingsPanel } from './SessionSettingsPanel';
import { SettingsPanel } from './SettingsPanel';
import { TodoPanel } from './TodoPanel';
import { TokenEntryModal } from './TokenEntryModal';
import { WorkflowPanel } from './WorkflowPanel';

/** Every workspace panel screen, in selector reading order. One row carries
 *  both the chooser's icon/label and the component it routes to, so a panel can
 *  never be listed without being reachable. */
type PanelId =
  | 'goal'
  | 'todo'
  | 'activity'
  | 'jobs'
  | 'files'
  | 'review'
  | 'session-settings'
  | 'processes'
  | 'personas'
  | 'loop'
  | 'workflow'
  | 'settings';

/** The session menu is the one navigation row without a screen: it opens as an
 *  overlay of this workspace instead of replacing it. */
type Overlay = 'none' | 'panels' | 'sessions' | PanelId;

interface PanelScreenProps {
  palette: Palette;
  client: RunnerClient;
  sessionId: string;
  onClose: () => void;
  onError: (message: string) => void;
  onRegisterBackHandler?: (handler: (() => boolean) | null) => void;
}

const PANEL_ENTRIES: ReadonlyArray<{
  id: 'sessions' | PanelId;
  icon: string;
  label: string;
  screen?: React.ComponentType<PanelScreenProps>;
}> = [
  { id: 'sessions', icon: 'list', label: 'Sessions' },
  { id: 'goal', icon: 'flag', label: 'Goal', screen: GoalPanel },
  { id: 'todo', icon: 'checklist', label: 'Todo', screen: TodoPanel },
  { id: 'activity', icon: 'history', label: 'Activity', screen: ActivityPanel },
  { id: 'jobs', icon: 'work', label: 'Jobs', screen: JobsPanel },
  { id: 'files', icon: 'folder', label: 'Files', screen: FileExplorerPanel },
  { id: 'review', icon: 'rate-review', label: 'Review', screen: ReviewPanel },
  // Model and thinking level for THIS session — distinct from the global and
  // project Settings below, which is why neither row is called just "Settings".
  { id: 'session-settings', icon: 'settings', label: 'Session settings', screen: SessionSettingsPanel },
  { id: 'processes', icon: 'memory', label: 'Processes', screen: ProcessesPanel },
  { id: 'personas', icon: 'groups', label: 'Personas', screen: PersonasPanel },
  { id: 'loop', icon: 'bolt', label: 'Loop', screen: LoopPanel },
  { id: 'workflow', icon: 'account-tree', label: 'Workflow', screen: WorkflowPanel },
  { id: 'settings', icon: 'tune', label: 'Settings', screen: SettingsPanel },
];

/** Frame types that carry transcript or composer streaming state. Includes
 *  `turn_started` / `tool_execution_update` so Stop can latch with Working
 *  even when the reducer no-ops. Anything else — resync markers, unloads,
 *  todo/goal updates, heartbeats — is never routed through the transcript. */
const TRANSCRIPT_FRAME_TYPES: Readonly<Record<string, true>> = {
  subscription_snapshot: true,
  session_resync: true,
  turn_started: true,
  turn_start: true,
  message_start: true,
  message_update: true,
  message_end: true,
  tool_execution_start: true,
  tool_execution_update: true,
  tool_execution_end: true,
  bash_execution_end: true,
  turn_terminal: true,
};

/** How long the archived-session Undo stays offered (the /web toast window). */
const SESSION_ARCHIVE_UNDO_MS = 5000;

/** Append an older `get_fork_messages` page to the loaded list, dropping entry
 *  ids already present (the /web `mergeForkMessagePages`). */
function mergeForkMessages(
  current: readonly ForkMessageWire[],
  incoming: readonly ForkMessageWire[],
): ForkMessageWire[] {
  const seen = new Set(current.map((message) => message.entryId));
  const merged = [...current];
  for (const message of incoming) {
    if (seen.has(message.entryId)) continue;
    seen.add(message.entryId);
    merged.push(message);
  }
  return merged;
}

/** Undo token of a `session_delete` success: the response must confirm the
 *  archive, name the row that was captured, and carry a usable token. Anything
 *  else is a failure the caller surfaces instead of offering an Undo that would
 *  restore nothing. */
function deleteUndoToken(data: unknown, expectedSessionId: string): string | null {
  if (!data || typeof data !== 'object') return null;
  const row = data as Record<string, unknown>;
  if (row.archived !== true || row.sessionId !== expectedSessionId) return null;
  const token = typeof row.undoToken === 'string' ? row.undoToken.trim() : '';
  return token === '' ? null : token;
}

export function SessionWorkspaceScreen(props: {
  palette: Palette;
  runnerId: string;
  onError: (message: string) => void;
  onRegisterBackHandler?: (handler: (() => boolean) | null) => void;
  floatingAction?: React.ReactNode;
  /** The app-level error toast, rendered inside an open panel modal: the modal
   *  window covers the app layer, so an error raised while a panel is up would
   *  otherwise be invisible. */
  overlayToast?: React.ReactNode;
}): React.JSX.Element {
  const { palette, runnerId, onError } = props;
  const client = registry.client(runnerId);
  const [catalog, setCatalog] = useState<SessionCatalogRow[]>([]);
  const [active, setActive] = useState<RuntimeIdentity | null>(null);
  const [transcript, setTranscript] = useState<TranscriptState>(emptyTranscript());
  const [activityStrip, setActivityStrip] = useState(() => emptyActivityStripState(''));
  const [queuedSteering, setQueuedSteering] = useState<string[]>([]);
  const [queueBusy, setQueueBusy] = useState(false);
  const [draft, setDraft] = useState('');
  const [busy, setBusy] = useState(false);
  const [overlay, setOverlay] = useState<Overlay>('none');
  const panelBack = useRef<{
    owner: (handler: (() => boolean) | null) => void;
    handler: () => boolean;
  } | null>(null);
  const registerPanelBack = useMemo(() => {
    return function register(handler: (() => boolean) | null) {
      if (handler) panelBack.current = { owner: register, handler };
      else if (panelBack.current?.owner === register) panelBack.current = null;
    };
  }, [overlay, active?.sessionId, active?.runtimeIncarnation, client]);
  const closeOverlay = useCallback(() => setOverlay('none'), []);
  const registerBackHandler = props.onRegisterBackHandler;
  useEffect(() => {
    registerBackHandler?.(() => {
      if (overlay === 'none') return false;
      if (panelBack.current?.owner === registerPanelBack && panelBack.current.handler()) return true;
      closeOverlay();
      return true;
    });
    return () => registerBackHandler?.(null);
  }, [registerBackHandler, overlay, closeOverlay, registerPanelBack]);
  const [tokenOpen, setTokenOpen] = useState(false);
  const [tokenBusy, setTokenBusy] = useState(false);
  const [newCwd, setNewCwd] = useState('');
  const [renameDraft, setRenameDraft] = useState('');
  const [moveDraft, setMoveDraft] = useState('');
  const [pendingDelete, setPendingDelete] = useState<string | null>(null);
  const [pendingUndo, setPendingUndo] = useState<{ token: string; label: string } | null>(null);
  const [forkPicker, setForkPicker] = useState<{
    row: SessionCatalogRow;
    messages: ForkMessageWire[];
    nextCursor: string | null;
  } | null>(null);
  const [copyNote, setCopyNote] = useState<{ path: string; text: string } | null>(null);
  /** In-flight session actions, rendered so the matching controls disable. */
  const [lifecycleBusy, setLifecycleBusy] = useState(false);
  const [archiveBusy, setArchiveBusy] = useState(false);
  const [undoBusy, setUndoBusy] = useState(false);
  const [handoffBusy, setHandoffBusy] = useState(false);
  const [statsLine, setStatsLine] = useState('--');
  const [streaming, setStreaming] = useState(false);
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const activeRef = useRef(active);
  activeRef.current = active;
  const queuedSteeringRef = useRef<string[]>([]);
  queuedSteeringRef.current = queuedSteering;
  const queueGenRef = useRef(0);
  /** Synchronous exclusive lock for queue send/cancel: a second tap in the
   *  same tick still reads `queueBusy` as false, so the ref is the real gate. */
  const queueBusyRef = useRef(false);
  const attachmentsRef = useRef<Attachment[]>([]);
  /** The draft as rendered: a delayed ACK retires only the text it actually
   *  sent, never what the user typed after it. */
  const draftValueRef = useRef(draft);
  draftValueRef.current = draft;
  /** Synchronous in-flight gate for the composer: a second Send in the same
   *  tick would still read `busy` as false and duplicate the prompt. */
  const submitRef = useRef(false);
  const draftRef = useRef<TextInput>(null);
  const pickStreamRef = useRef<PickStream | null>(null);
  const pickSkipsRef = useRef<AttachmentSkip[]>([]);
  const undoTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const forkPickerRef = useRef(forkPicker);
  forkPickerRef.current = forkPicker;
  /** The receipt a restore in flight must retire by identity, never by guess. */
  const pendingUndoRef = useRef(pendingUndo);
  pendingUndoRef.current = pendingUndo;
  /** Newest session read wins: a late reply for a session the user already left
   *  must never publish over the current one. */
  const loadGenRef = useRef(0);
  const adoptGenRef = useRef(0);
  /** Transcript frames applied for the current owner: a read that started before
   *  them must not overwrite live authority when it lands. */
  const appliedFramesRef = useRef(0);
  /** Single-flight gates: one lifecycle cut, one archive, one restore, one
   *  handoff, and one older fork page at a time. Distinct actions may overlap
   *  (a new archive during an in-flight restore is a real flow), but a double
   *  tap on the SAME action must never mint a second session, archive twice, or
   *  spend a one-shot undo token twice. */
  const forkPageRef = useRef(false);
  const lifecycleRef = useRef(false);
  const archiveRef = useRef(false);
  const restoreRef = useRef(false);
  const handoffRef = useRef(false);
  /** Copy is latest-request-wins: an older artifact must never win the
   *  clipboard over a newer one. */
  const copyGenRef = useRef(0);
  /** Picker generation: bumped by every open AND every close, so a page that
   *  lands after the picker moved on — a cancel plus a reopen of the SAME row
   *  included — is dropped instead of mutating the list the user is looking
   *  at. Request identity, not timing. */
  const forkPickerGenRef = useRef(0);

  /** Close the picker and retire every page still in flight for it. */
  const closeForkPicker = useCallback(() => {
    forkPickerGenRef.current += 1;
    forkPageRef.current = false;
    setForkPicker(null);
  }, []);

  /** Cancel the bounded Undo window (a re-armed delete, an undo, or unmount). */
  const clearUndoTimer = useCallback(() => {
    if (undoTimerRef.current !== null) {
      clearTimeout(undoTimerRef.current);
      undoTimerRef.current = null;
    }
  }, []);

  useEffect(() => () => clearUndoTimer(), [clearUndoTimer]);

  const applyAttachments = useCallback((next: Attachment[]) => {
    attachmentsRef.current = next;
    setAttachments(next);
  }, []);

  // One upload path for both raw-byte kinds: the runner that owns the composer
  // POSTs the retained bytes (PDF → text layer, video → frames), and the
  // outcome settles the chip in place. A failure keeps the chip in an error
  // state with its bytes, so Retry re-runs exactly this call. The queue is read
  // only AFTER the round-trip resolves: an intake or a removal that lands while
  // the upload is in flight must never be clobbered by a pre-await snapshot.
  const uploadAttachment = useCallback(async (id: string) => {
    const item = attachmentsRef.current.find((row) => row.id === id);
    if (!item || item.bytes === undefined) return;
    const runner = registry.snapshot().find((row) => row.id === runnerId);
    const unavailable = { ok: false as const, message: 'the runner is not available' };
    const settle = (done: UploadResult) => {
      applyAttachments(done.items);
      if (done.error) onError(done.error);
    };
    if (item.kind === 'video') {
      const outcome = runner === undefined
        ? unavailable
        : await uploadVideoAttachment(runner.origin, runner.token, item.name, item.bytes);
      settle(completeVideoUpload(attachmentsRef.current, id, outcome));
      return;
    }
    const outcome = runner === undefined
      ? unavailable
      : await uploadPdfAttachment(runner.origin, runner.token, item.name, item.bytes);
    settle(completePdfUpload(attachmentsRef.current, id, outcome));
  }, [applyAttachments, onError, runnerId]);

  const retryUpload = useCallback((id: string) => {
    applyAttachments(markUploading(attachmentsRef.current, id).items);
    void uploadAttachment(id);
  }, [applyAttachments, uploadAttachment]);

  const sendToTerminal = useCallback(async (item: Attachment) => {
    const result = await sendAttachmentToTmux(item, tmuxFileDestination());
    if (!result.ok) onError(result.message);
  }, [onError]);

  const handlePickMessage = useCallback((raw: string) => {
    const event = parsePickEvent(raw);
    if (!event) return;
    if (event.type === 'begin') {
      pickStreamRef.current = beginPickStream(event);
      return;
    }
    if (event.type === 'chunk') {
      const stream = pickStreamRef.current;
      if (stream) pushPickChunk(stream, event.data);
      return;
    }
    if (event.type === 'reject') {
      pickStreamRef.current = null;
      pickSkipsRef.current.push({ name: event.name, size: event.size, reason: event.reason });
      return;
    }
    if (event.type === 'batch-end') {
      // One grouped summary for the whole selection: every refused file is
      // named once, in its category, instead of one message per file.
      const skips = pickSkipsRef.current;
      pickSkipsRef.current = [];
      const summary = formatSkipSummary(skips);
      if (summary) onError(summary);
      return;
    }
    const stream = pickStreamRef.current;
    pickStreamRef.current = null;
    if (!stream) return;
    const accepted = acceptPicked(stream.header, finishPickStream(stream), attachmentsRef.current);
    applyAttachments(accepted.items);
    if (accepted.skip) pickSkipsRef.current.push(accepted.skip);
    if (accepted.upload) void uploadAttachment(accepted.upload.id);
  }, [applyAttachments, onError, uploadAttachment]);

  // A large paste becomes a `pasted-text.txt` chip instead of materializing in
  // the input; the draft the user had typed is preserved. A controlled
  // TextInput does not re-push an unchanged value, so the pasted block is
  // cleared on the native side explicitly.
  const handleDraftChange = useCallback((next: string) => {
    const plan = planPastedText(draft, next);
    if (plan.type === 'native') {
      setDraft(next);
      return;
    }
    draftRef.current?.setNativeProps?.({ text: draft });
    if (plan.type === 'oversize') {
      onError(attachmentSkipMessage('oversize', PASTED_TEXT_NAME));
      return;
    }
    const header: PickBegin = {
      type: 'begin',
      name: PASTED_TEXT_NAME,
      mimeType: 'text/plain',
      size: plan.size,
      encoding: 'text',
      total: 1,
    };
    const accepted = acceptPicked(header, plan.text, attachmentsRef.current);
    applyAttachments(accepted.items);
    if (accepted.error) onError(accepted.error);
  }, [applyAttachments, draft, onError]);

  const pickerSource = useMemo(() => ({
    html: attachmentPickerHtml({
      background: palette.bg,
      foreground: palette.fg,
      border: palette.border,
    }),
    baseUrl: 'file:///android_asset/',
  }), [palette.bg, palette.fg, palette.border]);

  const sessionId = active?.sessionId ?? '';
  const canOpenPanels = active !== null && sessionId !== '';


  const refreshCatalog = useCallback(async () => {
    if (!client || client.state !== 'connected') return;
    const response = await client.command({ type: 'session_list', scope: 'all_projects' });
    const rows = response.success ? parseSessionCatalog(response.data) : null;
    if (rows) setCatalog(rows);
  }, [client]);

  /** Publish a session read only while it is still the newest one and no live
   *  frame has superseded it: a late reply for a session the user already left
   *  must never overwrite the view, live events outrank the snapshot a slower
   *  read returns, and a refused read keeps the last-good transcript of the
   *  SAME owner instead of blanking it — a distinct target is blanked at the cut
   *  and waits for its own authoritative content. */
  const loadSession = useCallback(async (identity: RuntimeIdentity, resetCursor = false) => {
    if (!client) return;
    const gen = loadGenRef.current + 1;
    loadGenRef.current = gen;
    const previous = activeRef.current;
    const sameOwner = previous !== null
      && previous.sessionId === identity.sessionId
      && previous.runtimeIncarnation === identity.runtimeIncarnation;
    setActive(identity);
    // The owner ref follows the request synchronously: frames for this runtime
    // are admissible from the moment its subscription is sent.
    activeRef.current = identity;
    if (!sameOwner) {
      setActivityStrip(emptyActivityStripState(identity.sessionId));
      queueGenRef.current += 1;
      queuedSteeringRef.current = [];
      setQueuedSteering([]);
      // A distinct target must never wear the previous session's transcript,
      // stats, or streaming state while its own content is in flight; the
      // subscription snapshot and this read both repopulate it.
      setTranscript(emptyTranscript());
      setStatsLine('--');
      setStreaming(false);
    }
    const queueGen = ++queueGenRef.current;
    const appliedAtStart = appliedFramesRef.current;
    client.subscribe(identity, resetCursor ? 0 : undefined);
    void (async () => {
      try {
        const response = await client.command({ type: 'queue_list' }, identity.sessionId);
        if (!response.success) throw new Error(response.error || 'the runner refused the queue read');
        const steering = response.data && typeof response.data === 'object'
          ? (response.data as Record<string, unknown>).steering : null;
        if (!Array.isArray(steering) || !steering.every((item): item is string => typeof item === 'string')) {
          throw new Error('invalid steering array');
        }
        if (gen !== loadGenRef.current || queueGen !== queueGenRef.current
          || registry.client(runnerId) !== client
          || activeRef.current?.sessionId !== identity.sessionId
          || activeRef.current?.runtimeIncarnation !== identity.runtimeIncarnation) return;
        queueGenRef.current += 1;
        queuedSteeringRef.current = steering;
        setQueuedSteering(steering);
      } catch (err) {
        // Superseded load ownership is nonfatal: it no longer owns the screen's error channel.
        if (gen !== loadGenRef.current || registry.client(runnerId) !== client
          || activeRef.current?.sessionId !== identity.sessionId
          || activeRef.current?.runtimeIncarnation !== identity.runtimeIncarnation) return;
        // A disconnected read keeps the last-good queue until reconnect retries.
        if (isRunnerDisconnected(err)) return;
        onError(`queue_list: ${err instanceof Error ? err.message : String(err)}`);
      }
    })();
    const messages = await client.command({ type: 'get_messages' }, identity.sessionId);
    if (gen !== loadGenRef.current) return;
    const list = messages.success ? parseMessages(messages.data) : null;
    if (list === null) {
      onError(messages.error || 'session messages are unavailable');
    } else if (appliedFramesRef.current === appliedAtStart) {
      setTranscript({ items: messagesToItems(list), activeAssistantId: null });
    }
    const stats = await client.command({ type: 'get_session_stats' }, identity.sessionId);
    if (gen !== loadGenRef.current) return;
    if (!stats.success) {
      onError(stats.error || 'session stats are unavailable');
      return;
    }
    const data = stats.data && typeof stats.data === 'object' ? stats.data as Record<string, unknown> : {};
    const usage = data && typeof data === 'object' && 'contextUsage' in (data as Record<string, unknown>)
      ? (data as Record<string, unknown>).contextUsage
      : data.contextUsage;
    if (usage && typeof usage === 'object') {
      const row = usage as Record<string, unknown>;
      const tokens = typeof row.tokens === 'number' ? row.tokens : null;
      const window = typeof row.contextWindow === 'number' ? row.contextWindow : null;
      const percent = typeof row.percent === 'number' ? row.percent : null;
      if (tokens === null && window === null && percent === null) setStatsLine('--');
      else setStatsLine(`${tokens ?? '--'} / ${window ?? '--'} (${percent ?? '--'}%)`);
    } else {
      setStatsLine('--');
    }
  }, [client, onError, runnerId]);

  useEffect(() => {
    if (!client) return;
    let mounted = true;
    if (client.state === 'connected') {
      void (async () => {
        try {
          await refreshCatalog();
          const probe = client.probe;
          if (probe?.sessionId && probe.runtimeIncarnation && mounted) {
            await loadSession({ sessionId: probe.sessionId, runtimeIncarnation: probe.runtimeIncarnation }, true);
          }
        } catch (err) {
          onError(err instanceof Error ? err.message : String(err));
        }
      })();
    }
    const off = client.subscribeFrames((frame) => {
      if (!mounted) return;
      const type = readFrameType(frame);
      const record = frame as { type?: string; sessionId?: string; turnId?: string; [k: string]: unknown };
      const stripOwner = activeRef.current;
      if (typeof record.sessionId === 'string' && record.sessionId !== ''
        && stripOwner !== null && record.sessionId === stripOwner.sessionId) {
        setActivityStrip((current) => applyActivityStripEvent(current, frame, stripOwner.sessionId));
        if (type === 'subscription_snapshot' || type === 'session_resync') {
          setActivityStrip((current) => reconcileActivityStrip(
            current,
            stripOwner.sessionId,
            typeof record.activeTurnId === 'string' && record.activeTurnId !== '',
            typeof record.activeTurnId === 'string' ? record.activeTurnId : '',
          ));
        }
      }
      // ONLY transcript-bearing frames reach the reducer, and each must name the
      // CURRENT runtime in full. Connection/lifecycle frames (resync markers,
      // unloads, todo/goal updates, heartbeats) and unknown types are handled
      // elsewhere or nowhere — a sessionless or foreign frame must never mutate
      // the transcript or flip its streaming state.
      if (type === 'queue_update' && typeof record.sessionId === 'string' && record.sessionId !== ''
        && stripOwner !== null && record.sessionId === stripOwner.sessionId
        && record.runtimeIncarnation === stripOwner.runtimeIncarnation) {
        queueGenRef.current += 1;
        const previews = steeringPreviewsFromMessages(record.steering);
        queuedSteeringRef.current = previews;
        setQueuedSteering(previews);
      }
      if (TRANSCRIPT_FRAME_TYPES[type] !== true) return;
      const owner = parseRuntimeIdentity(frame);
      const activeIdentity = activeRef.current;
      if (owner === null || activeIdentity === null
        || owner.sessionId !== activeIdentity.sessionId
        || owner.runtimeIncarnation !== activeIdentity.runtimeIncarnation) return;
      appliedFramesRef.current += 1;
      setTranscript((current) => applySessionEvent(current, frame));
      setStreaming((current) => planTurnStreaming(type, current, record.activeTurnId));
    });
    const offReconnect = client.subscribeReconnect(() => {
      if (!mounted) return;
      const identity = activeRef.current;
      const adoptGen = adoptGenRef.current;
      if (identity) {
        void (async () => {
          try {
            const response = await client.command({ type: 'session_list', scope: 'all_projects' });
            if (!mounted) return;
            if (adoptGenRef.current !== adoptGen) return;
            if (activeRef.current?.sessionId !== identity.sessionId
              || activeRef.current?.runtimeIncarnation !== identity.runtimeIncarnation) return;
            const rows = response.success ? parseSessionCatalog(response.data) : null;
            if (rows) setCatalog(rows);
            const probe = client.probe;
            const listed = rows !== null && rows.length > 0;
            const missing = listed && !rows.some((row) => row.sessionId === identity.sessionId);
            const next = missing && probe?.sessionId && probe.runtimeIncarnation
              ? { sessionId: probe.sessionId, runtimeIncarnation: probe.runtimeIncarnation }
              : identity;
            await loadSession(next, missing);
          } catch (err) {
            if (isRunnerDisconnected(err)) return;
            onError(err instanceof Error ? err.message : String(err));
          }
        })();
        return;
      }
      void refreshCatalog().catch((err) => {
        if (isRunnerDisconnected(err)) return;
        onError(err instanceof Error ? err.message : String(err));
      });
    });
    const appState = AppState.addEventListener('change', (next: AppStateStatus) => {
      if (next !== 'active' || !mounted) return;
      const identity = activeRef.current;
      if (!identity) return;
      if (client.hasOpenSocket()) {
        client.subscribe(identity);
        void loadSession(identity, false).catch((err) => {
          if (isRunnerDisconnected(err)) {
            client.resumeFromBackground();
            return;
          }
          onError(err instanceof Error ? err.message : String(err));
        });
        return;
      }
      client.resumeFromBackground();
    });
    return () => {
      mounted = false;
      loadGenRef.current += 1;
      queueGenRef.current += 1;
      off();
      offReconnect();
      appState.remove();
    };
  }, [client, loadSession, onError, refreshCatalog]);

  const sendTyped = async (command: RunnerCommand) => {
    if (!client) throw new RunnerDisconnectedError();
    return client.command(command, sessionId);
  };

  /** Abort the in-flight turn. Streaming is owned by turn_terminal; a refused
   *  or thrown abort surfaces through onError and leaves the Stop control up. */
  const abortTurn = async () => {
    try {
      const response = await sendTyped({ type: 'abort' });
      if (!response.success) onError(response.error || 'Stop failed');
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    }
  };

  const sameQueueOwner = (owner: RuntimeIdentity | null): boolean => (
    owner !== null
    && activeRef.current !== null
    && activeRef.current.sessionId === owner.sessionId
    && activeRef.current.runtimeIncarnation === owner.runtimeIncarnation
  );

  /** Await the queue command. Refusal or throw keeps the local rows and
   *  surfaces onError. A successful cancel may drop locally only when this
   *  session is still the owner and no newer queue_update has landed. */
  const sendQueueCommand = async (command: RunnerCommand, afterSuccess?: () => void) => {
    if (queueBusyRef.current) return;
    queueBusyRef.current = true;
    setQueueBusy(true);
    const owner = activeRef.current;
    const gen = queueGenRef.current;
    try {
      const response = await sendTyped(command);
      if (!response.success) {
        onError(response.error || 'the runner refused the queue command');
        return;
      }
      if (!afterSuccess || registry.client(runnerId) !== client
        || !sameQueueOwner(owner) || queueGenRef.current !== gen) return;
      afterSuccess();
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    } finally {
      queueBusyRef.current = false;
      setQueueBusy(false);
    }
  };

  const sendQueuedNow = () => {
    if (queueBusyRef.current) return;
    void sendQueueCommand({ type: 'queue_send_now' });
  };

  const cancelQueuedAt = (index: number) => {
    if (queueBusyRef.current) return;
    const target = queuedSteeringRef.current[index];
    void sendQueueCommand({ type: 'queue_cancel', scope: 'steering', index }, () => {
      const current = queuedSteeringRef.current;
      const at = target !== undefined && current[index] === target
        ? index
        : (target === undefined ? -1 : current.indexOf(target));
      if (at < 0) return;
      const next = dropSteeringAt(current, at);
      queueGenRef.current += 1;
      queuedSteeringRef.current = next;
      setQueuedSteering(next);
    });
  };

  const switchTo = async (row: SessionCatalogRow) => {
    if (!client) return;
    // Claim the adopt generation before switch_session leaves the device: a
    // slower earlier Resume must not adopt after the user already issued a
    // newer cut (another Resume, New, or fork).
    const gen = adoptGenRef.current + 1;
    adoptGenRef.current = gen;
    const response = await client.command({ type: 'switch_session', sessionPath: row.path }, sessionId);
    if (gen !== adoptGenRef.current) return;
    const identity = parseRuntimeIdentity(response.data) ?? (row.runtimeIncarnation
      ? { sessionId: row.sessionId, runtimeIncarnation: row.runtimeIncarnation }
      : null);
    if (!response.success || !identity) {
      onError(response.error || 'Switch failed');
      return;
    }
    await loadSession(identity);
    await refreshCatalog();
    setOverlay('none');
  };

  /** Surface a session action's transport failure through the modal's own error
   *  channel instead of an unhandled rejection. */
  const sessionAction = async (run: () => Promise<void>) => {
    try {
      await run();
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    }
  };

  /** Adopt one lifecycle cut (clone / fork / fork-latest / new_session): the
   *  frozen target contract is validated BEFORE any mutation, then the target
   *  becomes the active view and the catalog is re-read. The child composer
   *  starts from the response's one-shot `editorText` — the chosen fork prompt,
   *  empty for a clone or a new session — so a source draft or attachment never
   *  leaks into the fresh session. */
  const runLifecycle = async (command: RunnerCommand, failure: string) => {
    if (!client || lifecycleRef.current) return;
    if (command.type === 'new_session') {
      const cwd = command.cwd.trim();
      if (!cwd.startsWith('/')) {
        onError('new session cwd must be an absolute directory path');
        return;
      }
    }
    lifecycleRef.current = true;
    setLifecycleBusy(true);
    try {
      const gen = adoptGenRef.current + 1;
      adoptGenRef.current = gen;
      const response = await client.command(command, sessionId);
      if (gen !== adoptGenRef.current) return;
      const target = response.success ? parseLifecycleTarget(response.data) : null;
      if (!target) {
        onError(response.error || failure);
        return;
      }
      if (command.type === 'new_session') setNewCwd('');
      // The target is adopted: only now does the composer belong to the child
      // (its one-shot `editorText`, empty for a clone), so a failure above
      // leaves the source draft and attachments untouched.
      const payload = response.data && typeof response.data === 'object'
        ? response.data as Record<string, unknown>
        : {};
      setDraft(typeof payload.editorText === 'string' ? payload.editorText : '');
      applyAttachments([]);
      await loadSession({ sessionId: target.sessionId, runtimeIncarnation: target.runtimeIncarnation });
      await refreshCatalog();
      closeForkPicker();
      setOverlay('none');
    } finally {
      lifecycleRef.current = false;
      setLifecycleBusy(false);
    }
  };

  /** Page 1 of a row's forkable messages; the picker opens only on a validated
   *  page with at least one candidate, and only if this request still owns the
   *  picker. */
  const openForkPicker = async (row: SessionCatalogRow) => {
    if (!client) return;
    const gen = forkPickerGenRef.current + 1;
    forkPickerGenRef.current = gen;
    const response = await client.command({ type: 'get_fork_messages', sessionPath: row.path }, sessionId);
    if (gen !== forkPickerGenRef.current) return;
    const page = response.success ? parseForkMessagesPage(response.data) : null;
    if (!page) {
      onError(response.error || 'Fork messages failed');
      return;
    }
    if (page.messages.length === 0) {
      onError('no forkable messages');
      return;
    }
    setForkPicker({ row, messages: page.messages, nextCursor: page.nextCursor });
  };

  /** Older fork page: one request at a time, merged into the list the user is
   *  looking at. A failure (a malformed page included) keeps the loaded
   *  candidates AND the cursor, so paging stays retryable, and a page that
   *  settles after the picker moved on is dropped. */
  const loadOlderForkMessages = async () => {
    const picker = forkPickerRef.current;
    if (!client || !picker || picker.nextCursor === null || forkPageRef.current) return;
    const cursor = picker.nextCursor;
    const gen = forkPickerGenRef.current;
    forkPageRef.current = true;
    try {
      const response = await client.command({ type: 'get_fork_messages', sessionPath: picker.row.path, cursor }, sessionId);
      const page = response.success ? parseForkMessagesPage(response.data) : null;
      if (gen !== forkPickerGenRef.current) return;
      if (!page) {
        onError(response.error || 'Fork messages failed');
        return;
      }
      const current = forkPickerRef.current;
      if (!current) return;
      setForkPicker({
        ...current,
        messages: mergeForkMessages(current.messages, page.messages),
        nextCursor: page.nextCursor,
      });
    } finally {
      if (gen === forkPickerGenRef.current) forkPageRef.current = false;
    }
  };

  /** Fork the CHOSEN message: the entry id the user tapped is what travels. */
  const forkFromEntry = async (entryId: string) => {
    const picker = forkPickerRef.current;
    if (!picker) return;
    await runLifecycle({ type: 'fork', entryId, sessionPath: picker.row.path }, 'Fork failed');
  };

  /** Copy the session context artifact the runner renders for this row. Latest
   *  request wins: an older artifact never reaches the clipboard after a newer
   *  copy was asked for. */
  const copyContext = async (row: SessionCatalogRow) => {
    if (!client) return;
    const gen = copyGenRef.current + 1;
    copyGenRef.current = gen;
    const response = await client.command({ type: 'session_copy_context', sessionPath: row.path }, sessionId);
    if (gen !== copyGenRef.current) return;
    const artifact = response.success ? parseSessionCopyContext(response.data) : null;
    if (!artifact) {
      onError(response.error || 'session copy context response is invalid');
      return;
    }
    Clipboard.setString(artifact.text);
    setCopyNote({ path: row.path, text: artifact.truncated ? 'Copied context (truncated)' : 'Copied context' });
  };

  /** Hand off to a fresh session: one in-flight request at a time, and the
   *  envelope's session is subscribed from cursor 0 like any other cut. The
   *  child is a NEW session, so on adoption its composer starts empty — the
   *  source draft and attachments never ride along. */
  const handOff = async () => {
    if (!client || sessionId === '' || handoffRef.current) return;
    handoffRef.current = true;
    setHandoffBusy(true);
    try {
      const gen = adoptGenRef.current + 1;
      adoptGenRef.current = gen;
      const response = await client.command({ type: 'session_handoff', responseMode: 'subscribe-handoff-v1' }, sessionId);
      if (gen !== adoptGenRef.current) return;
      const envelope = response.success ? parseSessionHandoff(response.data) : null;
      if (!envelope) {
        onError(response.error || 'session handoff failed');
        return;
      }
      setDraft('');
      applyAttachments([]);
      await loadSession({ sessionId: envelope.sessionId, runtimeIncarnation: envelope.runtimeIncarnation }, true);
      await refreshCatalog();
      setOverlay('none');
    } finally {
      handoffRef.current = false;
      setHandoffBusy(false);
    }
  };

  /** Delete = archive: the row counts as deleted only once the response proves
   *  it, one archive at a time, and the Undo window keeps the token that
   *  restores it. */
  const archiveRow = async (row: SessionCatalogRow) => {
    if (!client || archiveRef.current) return;
    archiveRef.current = true;
    setArchiveBusy(true);
    try {
      const response = await client.command({ type: 'session_delete', sessionPath: row.path }, sessionId);
      setPendingDelete(null);
      const token = response.success ? deleteUndoToken(response.data, row.sessionId) : null;
      if (token === null) {
        onError(response.error || 'session delete response is invalid');
      } else {
        clearUndoTimer();
        undoTimerRef.current = setTimeout(() => {
          undoTimerRef.current = null;
          setPendingUndo(null);
        }, SESSION_ARCHIVE_UNDO_MS);
        setPendingUndo({ token, label: row.name || row.sessionId });
      }
      await refreshCatalog();
    } finally {
      archiveRef.current = false;
      setArchiveBusy(false);
    }
  };

  /** Restore the archived session. The receipt is retired only by a CONFIRMED
   *  success or by its own expiry: a failed (or rejected) restore keeps the
   *  token and its original window, so Undo stays retryable, and one restore
   *  runs at a time so the one-shot token is never spent twice. An older
   *  request settling never retires a newer archive's receipt — the token
   *  identifies which one this call may clear. */
  const undoArchive = async () => {
    const entry = pendingUndoRef.current;
    if (!client || !entry || restoreRef.current) return;
    restoreRef.current = true;
    setUndoBusy(true);
    try {
      const response = await client.command({ type: 'session_restore', undoToken: entry.token }, sessionId);
      if (!response.success) onError(response.error || 'session restore failed');
      else if (pendingUndoRef.current?.token === entry.token) {
        clearUndoTimer();
        setPendingUndo(null);
      }
      await refreshCatalog();
    } finally {
      restoreRef.current = false;
      setUndoBusy(false);
    }
  };

  const submit = async () => {
    const text = draft.trim();
    const queued = attachmentsRef.current;
    // `busy` is rendered state; a second tap in the same tick would still see
    // it false, so the synchronous ref is the real gate.
    if ((!text && queued.length === 0) || busy || submitRef.current || !client) return;
    // Exactly what this submission sends: a delayed ACK retires only this text
    // and these chips, never anything the user added while it was in flight.
    const sentText = draft;
    const sentIds = new Set(queued.map((item) => item.id));
    submitRef.current = true;
    setBusy(true);
    try {
      const slash = queued.length === 0 ? parseSlashLine(text) : null;
      if (slash && slash.name in WEB_COMPOSER_COMMANDS) {
        const action = resolveSlashAction(slash.name, slash.args);
        if (action.type === 'error') {
          onError(action.message);
          return;
        }
        if (action.type === 'goal' && action.action.op === 'show' && !action.action.explicit) {
          setOverlay('goal');
          setDraft('');
          return;
        }
        if (action.type === 'todo' && action.action.op === 'open') {
          setOverlay('todo');
          setDraft('');
          return;
        }
        // A slash command reports acceptance like a prompt does: a refused
        // command keeps the typed line so it can be corrected and retried.
        let slashAccepted = true;
        const runSlash = async (command: RunnerCommand) => {
          const response = await sendTyped(command);
          slashAccepted = response.success;
          if (!response.success) onError(response.error || `/${slash.name} failed`);
        };
        if (action.type === 'goal') {
          await runSlash(goalWire(action.action) as RunnerCommand);
        } else if (action.type === 'loop') {
          await runSlash(loopWire(action.action) as RunnerCommand);
        } else if (action.type === 'compact') {
          await runSlash(action.mode === 'snap'
            ? { type: 'snapcompact' }
            : { type: 'compact', customInstructions: action.customInstructions || undefined });
        } else if (action.type === 'skill') {
          await runSlash({ type: 'skill', name: action.name });
        } else if (action.type === 'model') {
          const spec = parseModelSpec(action.spec);
          if (!spec) {
            onError('usage: /model <provider/id>');
            return;
          }
          await runSlash({ type: 'set_model', provider: spec.provider, modelId: spec.modelId });
        } else if (action.type === 'todo') {
          if (action.action.op === 'clear') await runSlash({ type: 'todo_op', op: 'rm', cascade: true });
          else if (action.action.op !== 'open') {
            await runSlash({ type: 'todo_op', op: action.action.op, task: action.action.task });
          }
        }
        if (slashAccepted && draftValueRef.current === sentText) setDraft('');
        return;
      }
      if (slash) {
        onError(`/${slash.name} is not available.`);
        return;
      }
      const built = buildPromptCommand(queued, text, streaming ? 'steer' : 'prompt', sessionId);
      if (!built.ok) {
        onError(built.error);
        return;
      }
      // The composer owner at send time: a lifecycle cut while the ACK is in
      // flight hands the composer to a child, and the child's own draft and
      // queue must not be cleared by the source session's acceptance.
      const owner = activeRef.current;
      const queueGen = queueGenRef.current;
      const response = await sendTyped(built.command);
      if (!response.success) {
        // A refused submission is still the user's: the draft and every chip
        // stay exactly as they were typed.
        onError(response.error || 'the runner refused the message');
        return;
      }
      const stillOwner = registry.client(runnerId) === client && owner !== null && activeRef.current !== null
        && activeRef.current.sessionId === owner.sessionId
        && activeRef.current.runtimeIncarnation === owner.runtimeIncarnation;
      if (stillOwner) {
        if (draftValueRef.current === sentText) setDraft('');
        applyAttachments(attachmentsRef.current.filter((item) => !sentIds.has(item.id)));
        if (built.command.type === 'steer' && queueGenRef.current === queueGen) {
          const preview = built.command.message;
          if (preview !== '' && !queuedSteeringRef.current.includes(preview)) {
            const next = [...queuedSteeringRef.current, preview];
            queueGenRef.current += 1;
            queuedSteeringRef.current = next;
            setQueuedSteering(next);
          }
        }
      }
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    } finally {
      submitRef.current = false;
      setBusy(false);
    }
  };

  const picker = useMemo(() => {
    if (!draft.startsWith('/')) return [];
    const query = draft.slice(1).toLowerCase();
    return Object.keys(WEB_COMPOSER_COMMANDS).filter((name) => name.startsWith(query) || `/${name}`.startsWith(draft.toLowerCase()));
  }, [draft]);

  const canSend = (draft.trim() !== '' || attachments.length > 0)
    && !attachments.some((item) => item.state !== 'ready');

  // The token_required surface saves through the same registry path the runner
  // detail uses: updating the token retires the tokenless client and connects a
  // fresh one, so the workspace leaves token_required on the existing
  // connection logic instead of a second reconnect path of its own.
  const saveToken = async (token: string) => {
    setTokenBusy(true);
    try {
      await registry.updateToken(runnerId, token);
      setTokenOpen(false);
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    } finally {
      setTokenBusy(false);
    }
  };

  if (!client) {
    return (
      <View style={styles.emptyBox}>
        <Text style={[styles.emptyText, { color: palette.muted }]}>Connect the runner to open its session.</Text>
      </View>
    );
  }

  if (client.state === 'token_required') {
    return (
      <View style={styles.emptyBox}>
        <Text style={[styles.emptyText, { color: palette.muted }]}>
          This runner needs its listen token before the session can open.
        </Text>
        <TouchableOpacity
          style={[styles.primaryButton, { backgroundColor: palette.accent, paddingHorizontal: 24 }]}
          onPress={() => setTokenOpen(true)}
          accessibilityLabel="Set token">
          <Icon name="vpn-key" size={20} color={palette.bg} />
          <Text style={[styles.primaryButtonText, { color: palette.bg }]}>Set token</Text>
        </TouchableOpacity>
        <TokenEntryModal
          visible={tokenOpen}
          palette={palette}
          busy={tokenBusy}
          onSave={(token) => {
            void saveToken(token);
          }}
          onCancel={() => setTokenOpen(false)}
          overlayToast={props.overlayToast}
        />
      </View>
    );
  }

  if (client.state === 'offline' && !client.probe) {
    return (
      <View style={styles.emptyBox}>
        <Text style={[styles.emptyText, { color: palette.muted }]}>Connect the runner to open its session.</Text>
      </View>
    );
  }

  // A panel is mounted for the runtime identity it was opened on: a session cut
  // while a panel is up remounts it, so the new owner never inherits the old
  // owner's rows, selection, or in-flight reads.
  const stripView = activityStripView(activityStrip, client.state, false);
  const ownerKey = active === null ? 'none' : `${active.sessionId}:${active.runtimeIncarnation}`;
  const entry = PANEL_ENTRIES.find((row) => row.id === overlay);
  if (entry?.screen && active !== null && client) {
    const Screen = entry.screen;
    return (
      <Screen
        key={ownerKey}
        palette={palette}
        client={client}
        sessionId={active.sessionId}
        onClose={closeOverlay}
        onError={onError}
        onRegisterBackHandler={registerPanelBack}
      />
    );
  }

  return (
    <View style={styles.screen}>
      <View style={{ flexDirection: 'row', borderBottomWidth: 1, borderBottomColor: palette.border, backgroundColor: palette.surface }}>
        {/* One bounded control: the panel list outgrew a phone-width icon row,
            so the chooser carries it instead of the header growing. */}
        <TouchableOpacity
          style={styles.iconButton}
          onPress={() => {
            if (!canOpenPanels) return;
            setOverlay('panels');
          }}
          accessibilityLabel="Panels">
          <Icon name="apps" size={22} color={palette.fg} />
        </TouchableOpacity>
        <View style={{ flex: 1 }} />
        {client.state === 'reconnecting' && (
          <Text style={[styles.cardMeta, { color: palette.muted, alignSelf: 'center', marginRight: 8 }]}>Reconnecting</Text>
        )}
        <Text style={[styles.cardMeta, { color: palette.muted, alignSelf: 'center', marginRight: 12 }]}>{statsLine}</Text>
        {streaming && (
          <TouchableOpacity
            style={styles.iconButton}
            onPress={() => { void abortTurn(); }}
            accessibilityLabel="Stop">
            <Text style={{ color: palette.error, fontSize: 18 }}>■</Text>
          </TouchableOpacity>
        )}
      </View>
      <ScrollView style={styles.list}>
        {transcript.items.map((item) => (
          <TranscriptItemView key={item.id} item={item} palette={palette} />
        ))}
        {transcript.items.length === 0 && (
          <Text style={[styles.emptyText, { color: palette.muted, padding: 24 }]}>No messages.</Text>
        )}
        <View style={styles.bottomPad} />
      </ScrollView>
      {picker.length > 0 && (
        <View style={{ backgroundColor: palette.surface, borderTopColor: palette.border, borderTopWidth: 1 }}>
          {picker.map((name) => (
            <TouchableOpacity key={name} style={styles.listRow} onPress={() => setDraft(`/${name} `)}>
              <Text style={[styles.rowTitle, { color: palette.accent }]}>/{name}</Text>
            </TouchableOpacity>
          ))}
        </View>
      )}
      {attachments.length > 0 && (
        <ScrollView
          horizontal
          style={[styles.attachmentBar, { backgroundColor: palette.surface, borderTopColor: palette.border }]}
          contentContainerStyle={styles.attachmentBarContent}>
          {attachments.map((item) => (
            <View
              key={item.id}
              style={[styles.attachmentChip, {
                backgroundColor: palette.bg,
                borderColor: item.state === 'error' ? palette.error : palette.border,
              }]}>
              {item.previewUrl ? (
                <Image source={{ uri: item.previewUrl }} style={styles.attachmentThumb} />
              ) : (
                <View style={styles.attachmentChipIcon}>
                  <Icon
                    name={item.kind === 'video' ? 'movie' : item.kind === 'pdf' ? 'picture-as-pdf' : 'description'}
                    size={20}
                    color={palette.muted}
                  />
                </View>
              )}
              <View style={styles.attachmentChipBody}>
                <Text style={[styles.attachmentChipName, { color: palette.fg }]} numberOfLines={1}>{item.name}</Text>
                <Text
                  style={[styles.attachmentChipMeta, { color: item.state === 'error' ? palette.error : palette.muted }]}
                  numberOfLines={1}>
                  {item.state === 'error' ? (item.error ?? 'upload failed') : attachmentMeta(item)}
                </Text>
              </View>
              {item.state === 'error' && (
                <TouchableOpacity
                  style={styles.attachmentRemove}
                  onPress={() => retryUpload(item.id)}
                  accessibilityLabel={`Retry ${item.name}`}>
                  <Icon name="refresh" size={18} color={palette.accent} />
                </TouchableOpacity>
              )}
              {canSendAttachmentToTmux(item) && (
                <TouchableOpacity
                  style={styles.attachmentRemove}
                  onPress={() => void sendToTerminal(item)}
                  accessibilityLabel={`Send ${item.name} to terminal`}>
                  <Icon name="north-east" size={18} color={palette.muted} />
                </TouchableOpacity>
              )}
              <TouchableOpacity
                style={styles.attachmentRemove}
                onPress={() => applyAttachments(removeAttachment(attachmentsRef.current, item.id))}
                accessibilityLabel={`Remove ${item.name}`}>
                <Icon name="close" size={18} color={palette.muted} />
              </TouchableOpacity>
            </View>
          ))}
        </ScrollView>
      )}
      {(stripView || queuedSteering.length > 0) && (
        <View testID="composer-status-row" style={{
          flexDirection: 'row',
          alignItems: 'flex-start',
          backgroundColor: palette.surface,
          paddingHorizontal: 12,
          paddingVertical: 4,
          gap: 8,
        }}>
          {stripView && (
            <View testID="activity-strip" accessibilityLabel={stripView.text}
              style={{ flexShrink: 1, maxWidth: queuedSteering.length > 0 ? '42%' : '100%', paddingVertical: 4 }}>
              <Text numberOfLines={1} style={[styles.cardMeta, { color: palette.muted }]}>{stripView.text}</Text>
            </View>
          )}
          <SteeringQueue
            palette={palette}
            previews={queuedSteering}
            busy={queueBusy}
            onSendNow={sendQueuedNow}
            onCancelAt={cancelQueuedAt}
          />
        </View>
      )}
      <View style={[styles.composerRow, { backgroundColor: palette.surface, borderTopColor: palette.border }]}>
        <WebView
          style={[styles.attachButton, { backgroundColor: palette.bg, borderColor: palette.border }]}
          source={pickerSource}
          originWhitelist={['*']}
          javaScriptEnabled
          scrollEnabled={false}
          overScrollMode="never"
          onMessage={(event) => handlePickMessage(event.nativeEvent.data)}
          accessibilityLabel="Attach a file"
        />
        <TextInput
          ref={draftRef}
          style={[styles.composerInput, { backgroundColor: palette.bg, borderColor: palette.border, color: palette.fg }]}
          value={draft}
          onChangeText={handleDraftChange}
          placeholder={streaming ? 'Steer' : 'Message'}
          placeholderTextColor={palette.muted}
          onSubmitEditing={() => void submit()}
          returnKeyType="send"
        />
        {streaming ? (
          <>
            <TouchableOpacity
              style={[styles.primaryButton, { backgroundColor: palette.error, marginTop: 0, paddingVertical: 10, paddingHorizontal: 14 }]}
              onPress={() => { void abortTurn(); }}
              accessibilityLabel="Stop">
              <Text style={[styles.primaryButtonText, { color: palette.bg }]}>■ Stop</Text>
            </TouchableOpacity>
            <TouchableOpacity
              style={[styles.primaryButton, { backgroundColor: palette.accent, marginTop: 0, paddingVertical: 10, paddingHorizontal: 14, minHeight: 44, minWidth: 44 }, (!canSend || busy) && styles.faded]}
              disabled={!canSend || busy}
              onPress={() => void submit()}
              accessibilityLabel="Steer">
              <Text style={[styles.primaryButtonText, { color: palette.bg }]}>Steer</Text>
            </TouchableOpacity>
          </>
        ) : (
          <TouchableOpacity
            style={[styles.primaryButton, { backgroundColor: palette.accent, marginTop: 0, paddingVertical: 10, paddingHorizontal: 14 }, (!canSend || busy) && styles.faded]}
            disabled={!canSend || busy}
            onPress={() => void submit()}
            accessibilityLabel="Send">
            <Icon name="send" size={20} color={palette.bg} />
          </TouchableOpacity>
        )}
      </View>

      <Modal visible={overlay === 'panels'} animationType="slide" onRequestClose={() => setOverlay('none')}>
        <View style={[styles.screen, { backgroundColor: palette.bg }]}>
          <AppBar
            palette={palette}
            title="Panels"
            actions={[{ icon: 'close', label: 'Close panels', onPress: () => setOverlay('none') }]}
          />
          <ScrollView contentContainerStyle={styles.form}>
            {PANEL_ENTRIES.map((row) => (
              <TouchableOpacity
                key={row.id}
                style={[styles.listRow, { backgroundColor: palette.surface, borderColor: palette.border }]}
                onPress={() => setOverlay(row.id)}
                accessibilityLabel={row.id}>
                <Icon name={row.icon} size={22} color={palette.fg} />
                <Text style={[styles.rowTitle, { color: palette.fg }]}>{row.label}</Text>
              </TouchableOpacity>
            ))}
          </ScrollView>
          {props.overlayToast}
        </View>
      </Modal>

      <Modal visible={overlay === 'sessions'} animationType="slide" onRequestClose={() => setOverlay('none')}>
        <View style={[styles.screen, { backgroundColor: palette.bg }]}>
          <AppBar palette={palette} title="Sessions" onBack={() => setOverlay('none')} />
          <ScrollView contentContainerStyle={styles.form}>
            {pendingUndo !== null && (
              <View style={[styles.listRow, { backgroundColor: palette.surface, borderColor: palette.border }]}>
                <View style={{ flex: 1 }}>
                  <Text style={[styles.rowTitle, { color: palette.fg }]}>Session archived</Text>
                  <Text style={[styles.cardMeta, { color: palette.muted }]} numberOfLines={1}>{pendingUndo.label}</Text>
                </View>
                <TouchableOpacity
                  disabled={undoBusy}
                  onPress={() => void sessionAction(undoArchive)}
                  accessibilityLabel={`Undo archive ${pendingUndo.label}`}>
                  <Text style={{ color: palette.accent }}>Undo</Text>
                </TouchableOpacity>
              </View>
            )}
            {forkPicker !== null && (
              <View style={[styles.card, { backgroundColor: palette.surface, borderColor: palette.border }]}>
                <Text style={[styles.cardTitle, { color: palette.fg }]} numberOfLines={1}>
                  Fork from a message · {forkPicker.row.name || forkPicker.row.sessionId}
                </Text>
                {forkPicker.messages.map((message) => (
                  <TouchableOpacity
                    key={message.entryId}
                    disabled={lifecycleBusy}
                    style={[styles.listRow, { borderColor: palette.border }]}
                    onPress={() => void sessionAction(() => forkFromEntry(message.entryId))}
                    accessibilityLabel={`Fork from ${message.entryId}`}>
                    <Text style={[styles.cardMeta, { color: palette.fg }]} numberOfLines={3}>{message.text}</Text>
                  </TouchableOpacity>
                ))}
                {forkPicker.nextCursor !== null && (
                  <TouchableOpacity
                    style={[styles.primaryButton, { backgroundColor: palette.border }]}
                    onPress={() => void sessionAction(loadOlderForkMessages)}
                    accessibilityLabel="Load older fork messages">
                    <Text style={[styles.primaryButtonText, { color: palette.fg }]}>Load older messages</Text>
                  </TouchableOpacity>
                )}
                <TouchableOpacity
                  style={[styles.primaryButton, { backgroundColor: palette.border }]}
                  onPress={closeForkPicker}
                  accessibilityLabel="Cancel fork from message">
                  <Text style={[styles.primaryButtonText, { color: palette.fg }]}>Cancel</Text>
                </TouchableOpacity>
              </View>
            )}
            <TextInput
              style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg }]}
              value={newCwd}
              onChangeText={setNewCwd}
              placeholder="cwd for new session"
              placeholderTextColor={palette.muted}
              autoCapitalize="none"
            />
            <TouchableOpacity
              style={[styles.primaryButton, { backgroundColor: palette.accent }, (!newCwd.trim().startsWith('/') || lifecycleBusy) && styles.faded]}
              disabled={newCwd.trim() === '' || lifecycleBusy}
              onPress={() => void sessionAction(() => runLifecycle({ type: 'new_session', cwd: newCwd.trim() }, 'New session failed'))}
              accessibilityLabel="New">
              <Text style={[styles.primaryButtonText, { color: palette.bg }]}>New</Text>
            </TouchableOpacity>
            {forkPicker === null && catalog.map((row) => {
              const label = row.name || row.sessionId;
              return (
                <View key={`${row.sessionId}:${row.path}`} style={[styles.card, { backgroundColor: palette.surface, borderColor: palette.border }]}>
                  <TouchableOpacity onPress={() => void sessionAction(() => switchTo(row))} accessibilityLabel={`Resume ${label}`}>
                    <Text style={[styles.cardTitle, { color: palette.fg }]}>{label}</Text>
                    <Text style={[styles.cardMeta, { color: palette.muted }]} numberOfLines={1}>{row.cwd || row.path}</Text>
                  </TouchableOpacity>
                  <View style={{ flexDirection: 'row', flexWrap: 'wrap', gap: 12, marginTop: 8 }}>
                    <TouchableOpacity
                      disabled={lifecycleBusy}
                      onPress={() => void sessionAction(() => runLifecycle({ type: 'session_fork_latest', sessionPath: row.path }, 'Fork failed'))}
                      accessibilityLabel={`Fork ${label}`}>
                      <Text style={{ color: palette.accent }}>Fork</Text>
                    </TouchableOpacity>
                    <TouchableOpacity
                      disabled={lifecycleBusy}
                      onPress={() => void sessionAction(() => openForkPicker(row))}
                      accessibilityLabel={`Fork from ${label}`}>
                      <Text style={{ color: palette.accent }}>Fork from…</Text>
                    </TouchableOpacity>
                    <TouchableOpacity
                      disabled={lifecycleBusy}
                      onPress={() => void sessionAction(() => runLifecycle({ type: 'clone', sessionPath: row.path }, 'Clone failed'))}
                      accessibilityLabel={`Clone ${label}`}>
                      <Text style={{ color: palette.accent }}>Clone</Text>
                    </TouchableOpacity>
                    <TouchableOpacity
                      onPress={() => void sessionAction(() => copyContext(row))}
                      accessibilityLabel={`Copy context ${label}`}>
                      <Text style={{ color: palette.accent }}>Copy context</Text>
                    </TouchableOpacity>
                    <TouchableOpacity
                      disabled={archiveBusy}
                      onPress={() => void sessionAction(async () => {
                        if (pendingDelete !== row.path) {
                          setPendingDelete(row.path);
                          return;
                        }
                        await archiveRow(row);
                      })}
                      accessibilityLabel={pendingDelete === row.path ? `Confirm delete ${label}` : `Delete session ${label}`}>
                      <Text style={{ color: palette.error }}>{pendingDelete === row.path ? 'Delete?' : '×'}</Text>
                    </TouchableOpacity>
                  </View>
                  {copyNote?.path === row.path && (
                    <Text style={[styles.cardMeta, { color: palette.muted, marginTop: 6 }]}>{copyNote.text}</Text>
                  )}
                </View>
              );
            })}
            <Text style={[styles.formSection, { color: palette.muted }]}>Hand off</Text>
            <TouchableOpacity
              style={[styles.primaryButton, { backgroundColor: palette.accent }, (sessionId === '' || handoffBusy) && styles.faded]}
              disabled={sessionId === '' || handoffBusy}
              onPress={() => void sessionAction(handOff)}
              accessibilityLabel="Hand off to a new session">
              <Text style={[styles.primaryButtonText, { color: palette.bg }]}>Hand off…</Text>
            </TouchableOpacity>
            <Text style={[styles.formSection, { color: palette.muted }]}>Rename current</Text>
            <TextInput
              style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg }]}
              value={renameDraft}
              onChangeText={setRenameDraft}
              placeholder="name"
              placeholderTextColor={palette.muted}
            />
            <TouchableOpacity
              style={[styles.primaryButton, { backgroundColor: palette.accent }, renameDraft.trim() === '' && styles.faded]}
              disabled={renameDraft.trim() === ''}
              accessibilityLabel="Rename"
              onPress={() => void sessionAction(async () => {
                const row = catalog.find((item) => item.sessionId === sessionId);
                if (!row) return;
                const response = await client.command({ type: 'set_session_name', name: renameDraft.trim(), sessionPath: row.path }, sessionId);
                if (!response.success) {
                  onError(response.error || 'Rename failed');
                  return;
                }
                setRenameDraft('');
                await refreshCatalog();
              })}>
              <Text style={[styles.primaryButtonText, { color: palette.bg }]}>Rename</Text>
            </TouchableOpacity>
            <Text style={[styles.formSection, { color: palette.muted }]}>Move current</Text>
            <TextInput
              style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg }]}
              value={moveDraft}
              onChangeText={setMoveDraft}
              placeholder="destination directory"
              placeholderTextColor={palette.muted}
              autoCapitalize="none"
            />
            <TouchableOpacity
              style={[styles.primaryButton, { backgroundColor: palette.accent }, (!moveDraft.trim().startsWith('/') || streaming) && styles.faded]}
              disabled={moveDraft.trim() === '' || streaming}
              accessibilityLabel="Move"
              onPress={() => void sessionAction(async () => {
                const targetCwd = moveDraft.trim();
                if (!targetCwd.startsWith('/')) {
                  onError('destination directory must be an absolute path');
                  return;
                }
                const row = catalog.find((item) => item.sessionId === sessionId);
                if (!row) return;
                const response = await client.command({ type: 'session_move', sessionPath: row.path, targetCwd }, sessionId);
                if (!response.success) {
                  onError(response.error || 'Move failed');
                  return;
                }
                setMoveDraft('');
                await refreshCatalog();
              })}>
              <Text style={[styles.primaryButtonText, { color: palette.bg }]}>Move</Text>
            </TouchableOpacity>
          </ScrollView>
          {props.overlayToast}
        </View>
      </Modal>
      {props.floatingAction}
    </View>
  );
}
