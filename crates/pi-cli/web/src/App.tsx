import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent, type PointerEvent, type RefObject, type UIEvent } from 'react';
import { useDesktopActions } from './desktopBridge';
import { createScrollPin, type ScrollPinController } from './scrollPin';
import { createAutoResizeController, type AutoResizeController } from './autoResize';
import { safeText } from './redact';
import { AnsiText } from './AnsiText';
import { CopyButton } from './CopyButton';
import { humanToolTitle } from './toolTitle';
import {
  SessionModelMutationQueue,
  applySessionModelEvent,
  groupModelsByProvider,
  keepSelectedModelOption,
  reconcileSessionModel,
  requestSessionModel,
  sessionModelView,
  settleSessionModel,
  wireModelKey,
  EMPTY_SESSION_MODEL_STATE,
  type ModelProviderGroup,
  type ModelSelectInput,
  type SessionModelState,
} from './modelSelect';
import { EMPTY_HEADER_CATALOGS, normalizeHeaderLevels, normalizeHeaderModels, type HeaderCatalogs } from './headerCatalog';
import {
  EMPTY_THINKING_LEVEL_STATE,
  reconcileThinkingLevel,
  requestThinkingLevel,
  setThinkingLevelCommand,
  settleThinkingRequest,
  thinkingCommandTargetSessionId,
  thinkingLevelClampMessage,
  thinkingLevelView,
  type ThinkingLevelState,
} from './thinkingLevel';
import { SteeringQueue } from './SteeringQueue';
import { HostConnectionChip, HOST_TOKEN_REQUIRED_ERROR } from './hostConnectionChip';
import {
  applyQueueUpdatePreviews,
  applyStateQueuedSteering,
  dropConsumedPreview,
  dropLastMatchingPreview,
  dropQueuedPreviewAfterCancel,
  runExclusiveQueueCommand,
  settleQueueCommand,
  steeringPreviewsFromMessages,
  steeringPreviewsFromState,
  userContentPreview,
} from './steeringQueueHelpers';
import { renderBlocks, renderStreamingMarkdown, hydrateMermaid, thinkingSummaryHtml, markdownOpenValidatedPath, subscribeMarkdownOpenPath, markdownOpenMissingFileVerdict, markdownOpenMissingFileToast, type MarkdownPathOwner } from './markdown';
import { configurePlantumlServer } from './plantumlEncode';
import { MarkdownBody } from './MarkdownBody';
import { TodoPanel } from './panels/TodoPanel';
import { ActivityPanel } from './panels/ActivityPanel';
import { SubagentsPanel } from './panels/SubagentsPanel';
import { PersonasPanel } from './panels/PersonasPanel';
import { GoalPanel, type GoalEventWire, type GoalStateWire } from './panels/GoalPanel';
import { WorkflowPanel, type WorkflowEventPayload } from './panels/WorkflowPanel';
import {
  applyTodoEvent,
  projectTodoPhases,
  todoNavigationKey,
  type TodoFailureReason,
  type TodoSessionSnapshot,
} from './panels/todoPanelState';
import {
  applyActivityEvent,
  activityNavigationKey,
  type ActivitySessionSnapshot,
} from './panels/todoActivity';
import {
  SideChatPanel,
  planSideChatSttInsertThrowToast,
  planSideChatSttInsertToast,
  planSideChatSwitchCatchToast,
  readSideChatSwitchAck,
  sideChatPromptAccepted,
  SIDE_CHAT_STT_CLOSED_TOAST,
} from './panels/SideChatPanel';
import type { SideChatInsertTranscript, SideChatSnapshot, SideChatSwitchResult } from './panels/SideChatPanel';
import { SessionPanel, parseLifecycleResult, type SessionRowWire } from './panels/SessionPanel';
import { SettingsPanel } from './panels/SettingsPanel';
import { TokenPairingCard } from './panels/TokenPairingCard';
import { LoopPanel } from './panels/LoopPanel';
import { ProcessesPanel } from './panels/ProcessesPanel';
import { latestCompactionSummary, SessionSidebar, buildRestoreSwitchTarget } from './panels/SessionSidebar';
import { downloadSessionExport } from './sessionExport';
import { CodeReviewPanel, type CodeReviewOpenArgs } from './panels/CodeReviewPanel';
import { CommitPicker } from './panels/CommitPicker';
import { buildCodeReviewCommitsPayload, codeReviewOpenCommand, codeReviewOpenSnapshotError, normalizeCodeReviewCommits, normalizeCodeReviewSnapshot, type CodeReviewCommitRow } from './codeReview';
import { FileExplorerPanel } from './panels/FileExplorerPanel';
import {
  applyDrawerMode,
  applyOpenPath,
  closeFileConversationsDroppedBySlotPatch,
  fileExplorerCanonicalPath,
  fileExplorerCommandForSession,
  fileExplorerSyncPageCacheWithSlots,
  readFileExplorerSlot,
  updateFileExplorerSlot,
  type FileExplorerSessionSlot,
  type FileExplorerTab,
} from './fileExplorer';
import { isBusyCapacityError, rpcErrorCode, type ContentBlock, type EventFrame, type RpcResponse } from './types';
import { withResolvers } from './withResolvers';
import {
  activityStripView,
  applyActivityStripEvent,
  applyTaskSpawnDetails,
  emptyActivityStripState,
  reconcileActivityStrip,
  type ActivityStripState,
} from './activityStrip';
import {
  type Item,
  type TaskCardView,
  nextId,
  contentText,
  userMessageProjection,
  messagesToItems,
  mergeAuthoritativeItems,
  boundOutput,
  customToItem,
  applyToolSnapshot,
  applyToolStartToItems,
  toolCardCompletionStatus,
  applyToolResultToItems,
  toolMedia,
  applyJobUpdated,
  applyJobSnapshot,
  taskJobIds,
  applyAgentUpdated,
  assistantFailureMessage,
  applyMessageDelivered,
  resolveTaskCardView,
  taskCardStatus,
  firstLinePreview,
  taskChildActionLine,
  resolveTodoCardView,
  parseEditCard,
  compactToolArgs,
  parseCommandCardArgs,
  parseProcessCardArgs,
  parseWriteCardArgs,
  parseReadCard,
  authorizedToolResultOpenPath,
  parseHubCard,
  ircTitle,
  IRC_COMPACT_LINE_LIMIT,
  diffLines,
  planReconnectTurn,
  planSettledStreamRefillRequest,
  STREAM_WATCHDOG_MS,
  createStreamWatchdog,
  reduceStreamStall,
  planLiveSequenceGap,
  planLiveGapCatchUp,
  planStreamCatchUpToast,
  messagesFromCatchUpResponse,
  liveHasStreamingAssistant,
  finalizeStreamingAssistant,
  shouldNormalizeThinkingNewlines,
  normalizeThinkingNewlines,
  unescapeThinkingNewlines,
  BASH_OUTPUT_LINE_LIMIT,
  TOOL_OUTPUT_LINE_LIMIT,
  transcriptPathContext,
} from './transcript';
import {
  buildRealtimeSessionConfig,
  setupRealtimeCall,
  isRealtimeLiveMode,
  reduceRealtimeVoiceFrame,
  reducePendingVoiceFlush,
  parseRealtimeChannelFrames,
  classifyRealtimeConnectionState,
  realtimeCreateCallCommand,
  realtimeStopCommand,
  REALTIME_DELEGATION_QUIESCENCE_MS,
  shouldStopRealtimeOnSessionSwitch,
} from './realtime';
import { createPageClientNamespace, nextCommandId, planReconnectBootstrap, planReconnectDisplayedCatchup, reconnectSubscribeCursor, applyReconnectRefillCatch, planDisplayedIncarnationRebind, restoreReconnectCatalog, shouldPersistPrimaryPreference, bootstrapSavedSwitchNeedsRetry, commandRejectedBySwitchPending, planSessionSnapshotApply, stampSessionStateIncarnation, reconnectRefillStillOwned, adoptReconnectRefillHold, planReconnectRefillStep, reconnectRefillFinallyApplies, planReconnectDisplayedResync, type ReconnectCatalogRow, type ReconnectRefillHold } from './protocol';
import {
  SWITCH_PENDING_ERROR,
  acceptAuthoritativeCut,
  SWITCH_HANDOFF_RESPONSE_MODE,
  acceptSwitchHandoff,
  planFastSessionSwitch,
  beginSessionSwitch,
  buildSubscribeFrame,
  buildSwitchCommand,
  emptyViewSlot,
  failSessionSwitch,
  identityFromWire,
  parseRuntimeKey,
  parseSessionUnload,
  parseSubscriptionCut,
  parseSubscriptionError,
  parseSessionHandoffEnvelope,
  parseSwitchHandoff,
  switchFailureShouldToast,
  warmContinuityNeedsHistoryConfirm,
  planWarmHistoryConfirm,
  warmHistoryConfirmIsCurrent,
  preserveLiveBindCursor,
  acceptedCutLiveCursor,
  SwitchProtocolError,
  planSubscriptionError,
  shouldRetrySnapshotInProgress,
  runtimeKey,
  sameRuntime,
  lifecycleReadyGate,
  unloadRuntime,
  upsertViewSlot,
  AUTHORITATIVE_CUT_TIMEOUT_MS,
  SNAPSHOT_IN_PROGRESS_RETRY_MS,
  planAuthoritativeCutTick,
  authoritativeCutTimerDelayMs,
  type ReadyRuntime,
  type RequestSessionSwitch,
  type RuntimeIdentity,
  type RuntimeKey,
  type SessionDisplayState,
  type SessionSwitchPhase,
  type SessionViewSlot,
  type SwitchAction,
} from './sessionSwitch';
import {
  captureSttRecordingDestination,
  planSttTranscriptRoute,
  useSttRecorder,
  type SttRecordingDestination,
} from './useSttRecorder';
import { STALE_ABORT, isCurrentPending, shouldScheduleReconnect, detachTransportHandlers, ReadyGate, HEARTBEAT_TIMEOUT_MS, HEARTBEAT_PING_INTERVAL_MS, HEARTBEAT_PONG_DEADLINE_MS, HEARTBEAT_PING_FRAME, heartbeatMayRetire, shouldCloseForSilence, shouldCloseForMissedPong, isPongFrame, planHeartbeatPing, shouldRetireHandshake, TRANSPORT_HANDSHAKE_DEADLINE_MS } from './socket';
import { RECONNECT_INITIAL_DELAY_MS, nextReconnectDelay, reconnectOverlayVisible, reconnectOverlayText, withBootstrapRetry, waitReconnectRetry, planTransportOutageToast, planRefusedHandshake } from './reconnect';
import { ReconnectOverlay } from './reconnectOverlay';
import { PendingRegistry, classifyResponseOwnership, RESPONSE_OWNERSHIP_REJECTIONS, rpcFailureToastOwned, isToastOwnedRpcError, type PendingCommandError } from './pending';
import {
  TRANSCRIPT_FOLDS_SELECTOR,
  TRANSCRIPT_PROJECTION_EDGE_PX,
  applyRestoredViewport,
  applySurfaceVisibility,
  compensatePrependedStage,
  normalizeTranscriptWindow,
  noteLiveViewportFold,
  noteLiveViewportScroll,
  planDemandProjectionDecision,
  planParkedWindow,
  projectionStartForAnchor,
  projectTranscriptWindow,
  readTranscriptViewport,
  seedLiveViewport,
  tailProjectionStart,
  transcriptFoldStamp,
  transcriptItemStamp,
  type LiveTranscriptViewport,
  type TranscriptContentRoot,
  type TranscriptHitSurface,
  type ViewportState,
} from './transcriptView';

/** The engine hit-test surface the O(1) scroll-event anchor resolution
 *  (./transcriptView.ts) reads. One module-level adapter: real client
 *  coordinates in, the elements stacked at that point out. */
const DOCUMENT_HIT_SURFACE: TranscriptHitSurface = {
  elementsFromPoint: (x, y) => document.elementsFromPoint(x, y),
};

/** Adapt a real transcript content wrapper to the transcriptView content
 *  root surface (stamped children + stamped folds). The O(1) event paths
 *  never touch it; only the negotiated scan and the restore do. */
function transcriptContentRoot(content: HTMLElement): TranscriptContentRoot {
  return {
    children: content.children,
    stampedFolds: () => content.querySelectorAll<HTMLDetailsElement>(TRANSCRIPT_FOLDS_SELECTOR),
  };
}

import { isTransportError, markdownOpenRejectionOwnership, shouldToastUnhandledCommandError } from './transportError';
import {
  loadInitialAuthorityToken,
  loadTokenForAuthority,
  normalizeHostAuthority,
  saveTokenForAuthority,
  type StorageLike,
} from './hostToken';
import {
  decideBootConnection,
  fetchBootStatus,
  probeListenAuth,
  shouldReprobeTokenRequirement,
  shouldReleaseTokenHold,
  tokenCommitWhileHeld,
  wakeTransportAction,
  TOKEN_HOLD_REPROBE_MS,
} from './bootPreflight';
import {
  loadSessionPreference,
  saveSessionPreference,
} from './sessionPreference';
import { routeCommandSession, goalGetCommand, goalJournalCommand } from './goal';
import { CommandPicker } from './CommandPicker';
import {
  appendDraft,
  parseSupportedCommand,
  parseSlashLine,
  extensionCommandNames,
  formatExtensionResult,
  normalizeCommands,
  type CommandEntry,
} from './commands';
import {
  formatCompactReport,
  formatGoalResult,
  formatLoopResult,
  formatProcessList,
  formatSkillResult,
  goalWire,
  loopRequiresIdle,
  loopWire,
  psWire,
  resolveSlashAction,
  todoWire,
  unsupportedAliasMessage,
} from './slashDispatch';
import {
  abortIdleSnapshotToasts,
  applyComposerAbortTap,
  displayedSessionMatches,
  composerAbortChrome,
  composerActionClickKind,
  dropSessionAborting,
  shouldHoldOptimisticStop,
} from './composerAbort';
import {
  composerCommandSessionId,
  dispatchComposerSubmit,
  planComposerSubmitKind,
  rpcFailureToastSuppressed,
} from './composerSubmit';
import {
  beginCompact,
  compactAdmission,
  compactBadgeLabel,
  compactStatusText,
  compactSendAdmission,
  reconcileCompacting,
  settleCompact,
  type CompactBook,
  type CompactMode,
} from './compactProgress';
import { beginContextUsageRequest, contextMeterAccessible, contextMeterMobile, formatContextMeter, ownsContextUsageResponse, parseContextUsage, sessionStatsCommand, type ContextUsageWire } from './contextMeter';



const RPI_AUTH_PREFIX = 'rpi-auth.';
// localStorage may throw on access in private mode / blocked cookies; resolve
// a safe backend once. Tokens and session preferences remain listener-scoped.
const tokenStorage: StorageLike | null = (() => {
  try {
    return typeof window !== 'undefined' && window && window.localStorage ? window.localStorage : null;
  } catch {
    return null;
  }
})();
const HEARTBEAT_STABILITY_MS = 5000;

/* ------------------------------------------------------------------ *
 * Shared bottom-drawer height for ordinary `.panel` mounts
 * ------------------------------------------------------------------ *
 * Desktop only: one resizer + one localStorage key for every ordinary
 * side panel (todo/goal/workflow/session/settings/subagents/personas/
 * sidechat). Code review owns the full viewport and is
 * excluded. Mobile never mounts the resizer. */
const PANEL_DRAWER_SIZE_KEY = 'rpi-panel-drawer-size';
const PANEL_DRAWER_MIN_VH = 25;
const PANEL_DRAWER_MAX_VH = 90;
const PANEL_DRAWER_DEFAULT_VH = 90;

function clampPanelDrawerVh(vh: number): number {
  if (!Number.isFinite(vh)) return PANEL_DRAWER_DEFAULT_VH;
  return Math.min(PANEL_DRAWER_MAX_VH, Math.max(PANEL_DRAWER_MIN_VH, vh));
}

function readStoredPanelDrawerVh(): number {
  try {
    const raw = window.localStorage.getItem(PANEL_DRAWER_SIZE_KEY);
    if (raw == null || raw === '') return PANEL_DRAWER_DEFAULT_VH;
    const n = Number(raw);
    if (!Number.isFinite(n)) return PANEL_DRAWER_DEFAULT_VH;
    return clampPanelDrawerVh(n);
  } catch {
    return PANEL_DRAWER_DEFAULT_VH;
  }
}

function writeStoredPanelDrawerVh(vh: number): void {
  try {
    window.localStorage.setItem(PANEL_DRAWER_SIZE_KEY, String(clampPanelDrawerVh(vh)));
  } catch {
    /* private mode / blocked storage: height lives in CSS only */
  }
}

function applyPanelDrawerVh(vh: number): void {
  document.documentElement.style.setProperty(
    '--panel-drawer-height',
    `${clampPanelDrawerVh(vh)}vh`,
  );
}

/** Focus the session panel's New-session cwd input (`#session-new-cwd-input`)
 *  after the panel mounts. The panel commits on the next render, so the
 *  lookup retries a BOUNDED number of animation frames before giving up —
 *  focus is a best-effort affordance and must never throw or loop. Kept at
 *  module scope: it touches only the document, no App state. */
function focusNewSessionCwdInput(): void {
  let attempts = 0;
  const attempt = (): void => {
    const input = document.getElementById('session-new-cwd-input');
    if (input) {
      input.focus({ preventScroll: true });
      return;
    }
    attempts += 1;
    if (attempts < 10) window.requestAnimationFrame(attempt);
  };
  attempt();
}

/* ------------------------------------------------------------------ *
 * File explorer outer drawer width (desktop)
 * ------------------------------------------------------------------ *
 * One width authority for the right-side Files drawer: React writes the
 * clamped px onto `--file-explorer-width` on <html>; the stylesheet consumes
 * the SAME min(var(...), 70vw, 900px, calc(100vw - 576px)) expression in
 * .app-main's reservation, the drawer width, and the resizer offset. The
 * conversation-min-width guard and the SESSION SIDEBAR (230px open / 44px
 * collapsed rail, see the constants below) are both subtracted before the
 * drawer — the old naive viewport − 576 formula let a 704px drawer cover
 * the Send button at 1280px whenever the rail was open. When even the 360px
 * floor does not fit (fileExplorerOverlayActive), the drawer flips to an
 * overlay spanning the full viewport width: App adds the
 * app-layout--files-overlay class and the header Hide » button remains the
 * in-drawer close; the stored width is ignored. One boring global
 * localStorage key (never per-session, never anything but a bare clamped
 * number). Mobile (≤720px) is CSS-covered: the resizer is display:none and
 * the drawer covers the full viewport width.
 */
export const FILE_EXPLORER_WIDTH_KEY = 'rpi-file-explorer-width';
export const FILE_EXPLORER_WIDTH_MIN = 360;
export const FILE_EXPLORER_WIDTH_MAX = 900;
export const FILE_EXPLORER_WIDTH_DEFAULT = 607;
export const FILE_EXPLORER_WIDTH_STEP = 16;

/** Explicit conversation-min-width invariant (DeepSeek D05 real-browser
 * measurements): the main column never shrinks below 576px beside the
 * drawer — Send stays hittable down to that width. Matches the stylesheet's
 * calc(100vw - 576px) guard; the ceiling below subtracts the sidebar too,
 * so the guard applies to the width LEFT OVER after the rail. */
export const FILE_EXPLORER_CONVERSATION_MIN = 576;

/** App session-sidebar width consumed by the same row as drawer +
 * conversation: 230px with the drawer open (styles.css .session-sidebar),
 * 44px collapsed rail. Part of the math so 1280 + 230 can never yield the
 * old naive 704 ceiling. */
export const FILE_EXPLORER_SIDEBAR_OPEN = 230;
export const FILE_EXPLORER_SIDEBAR_COLLAPSED = 44;

/** Largest allowed drawer width at the given viewport: the conversation
 * minimum (576px) and the session sidebar (230px open / 44px collapsed) are
 * both reserved before the drawer, saturated at the hard 900px cap.
 * Viewports too narrow to fit all three (ceiling below the 360px floor)
 * return the bare floor — the drawer flips to a full-viewport overlay there
 * and the stored width is simply unusable. Non-finite viewport falls back to
 * the hard cap; non-finite sidebar is treated as the wider open rail (the
 * conservative bound). */
export function fileExplorerWidthCeiling(viewportWidth: number, sidebarPx: number = FILE_EXPLORER_SIDEBAR_OPEN): number {
  if (!Number.isFinite(viewportWidth)) return FILE_EXPLORER_WIDTH_MAX;
  const sidebar = Number.isFinite(sidebarPx) ? sidebarPx : FILE_EXPLORER_SIDEBAR_OPEN;
  const beside = viewportWidth - sidebar - FILE_EXPLORER_CONVERSATION_MIN;
  if (beside < FILE_EXPLORER_WIDTH_MIN) return FILE_EXPLORER_WIDTH_MIN;
  return Math.min(FILE_EXPLORER_WIDTH_MAX, beside);
}

/** True when the viewport cannot fit sidebar + conversation minimum + the
 * 360px drawer floor at all: the drawer flips to a full-viewport overlay
 * (header Hide » still closes; stored width unusable). Distinct from the
 * ceiling helper because the ceiling CLAMPS to the floor at exactly this
 * boundary — the overlay decision needs the raw leftover width. */
export function fileExplorerOverlayActive(viewportWidth: number, sidebarPx: number = FILE_EXPLORER_SIDEBAR_OPEN): boolean {
  if (!Number.isFinite(viewportWidth)) return false;
  return viewportWidth - sidebarPx - FILE_EXPLORER_CONVERSATION_MIN < FILE_EXPLORER_WIDTH_MIN;
}

export function clampFileExplorerWidth(px: number, viewportWidth: number, sidebarPx: number = FILE_EXPLORER_SIDEBAR_OPEN): number {
  if (!Number.isFinite(px)) return FILE_EXPLORER_WIDTH_DEFAULT;
  return Math.min(
    fileExplorerWidthCeiling(viewportWidth, sidebarPx),
    Math.max(FILE_EXPLORER_WIDTH_MIN, Math.round(px)),
  );
}

export function readStoredFileExplorerWidth(viewportWidth: number, sidebarPx: number = FILE_EXPLORER_SIDEBAR_OPEN): number {
  try {
    const raw = window.localStorage.getItem(FILE_EXPLORER_WIDTH_KEY);
    if (raw == null || raw === '') return FILE_EXPLORER_WIDTH_DEFAULT;
    return clampFileExplorerWidth(Number(raw), viewportWidth, sidebarPx);
  } catch {
    return FILE_EXPLORER_WIDTH_DEFAULT;
  }
}

export function writeStoredFileExplorerWidth(px: number, viewportWidth: number, sidebarPx: number = FILE_EXPLORER_SIDEBAR_OPEN): void {
  try {
    window.localStorage.setItem(
      FILE_EXPLORER_WIDTH_KEY,
      String(clampFileExplorerWidth(px, viewportWidth, sidebarPx)),
    );
  } catch {
    /* private mode / blocked storage: width lives in CSS only */
  }
}

export function applyFileExplorerWidth(px: number, viewportWidth: number, sidebarPx: number = FILE_EXPLORER_SIDEBAR_OPEN): void {
  document.documentElement.style.setProperty(
    '--file-explorer-width',
    `${clampFileExplorerWidth(px, viewportWidth, sidebarPx)}px`,
  );
}

/** Keyboard step for the outer drawer separator (direction +1 grows; the
 * resizer sits at the drawer's LEFT edge, so ArrowLeft = drag the edge
 * leftward = grow). */
export function stepFileExplorerWidth(
  current: number,
  direction: -1 | 1,
  viewportWidth: number,
  sidebarPx: number = FILE_EXPLORER_SIDEBAR_OPEN,
): number {
  return clampFileExplorerWidth(current + direction * FILE_EXPLORER_WIDTH_STEP, viewportWidth, sidebarPx);
}

/* ------------------------------------------------------------------ *
 * App header height — ONE top offset authority
 * ------------------------------------------------------------------ *
 * The header's live height (phone chrome wraps; desktop is one row) is
 * measured once and written as --app-header-height on <html>. The header
 * itself, the content area below it, and the fixed file-explorer drawer all
 * derive their top from this SAME rounded integer, so a seam between the
 * header and the drawer is impossible by construction (the old independent
 * fixed offset drifted to a fractional 57.7px and left a 2.7px gap). */
export const HEADER_HEIGHT_FLOOR = 44;

/** Rounded integer header height in px, floored at 44px so a collapsed or
 * mid-layout header can never pull the drawer up under the chrome.
 * Fullscreen Files hides the header: only that path returns 0 and bypasses
 * the floor. A single argument keeps the existing measured behavior. */
export function headerHeightPx(measuredHeight: number, headerHidden = false): number {
  if (headerHidden) return 0;
  if (!Number.isFinite(measuredHeight)) return HEADER_HEIGHT_FLOOR;
  return Math.max(HEADER_HEIGHT_FLOOR, Math.round(measuredHeight));
}

export function applyHeaderHeight(measuredHeight: number, headerHidden = false): void {
  document.documentElement.style.setProperty(
    '--app-header-height',
    `${headerHeightPx(measuredHeight, headerHidden)}px`,
  );
}

/** Files fullscreen is the open drawer expanded over the app header. */
export function fileExplorerFullscreenActive(open: boolean, expanded: boolean): boolean {
  return open && expanded;
}

/* ------------------------------------------------------------------ *
 * App footer height — ONE bottom offset authority
 * ------------------------------------------------------------------ *
 * Ordinary desktop drawers sit above the composer. App measures the live
 * footer and writes --app-footer-height; the stylesheet uses that same
 * integer as panel `bottom` so Send is never covered. Code review and the
 * Files explorer still own the viewport bottom. */
export const FOOTER_HEIGHT_FLOOR = 44;

export function footerHeightPx(measuredHeight: number): number {
  if (!Number.isFinite(measuredHeight)) return FOOTER_HEIGHT_FLOOR;
  return Math.max(FOOTER_HEIGHT_FLOOR, Math.round(measuredHeight));
}

export function applyFooterHeight(measuredHeight: number): void {
  document.documentElement.style.setProperty(
    '--app-footer-height',
    `${footerHeightPx(measuredHeight)}px`,
  );
}

/** Inline min-height 0 for the read, then leave it 0 so `min-height: var(--app-*-height)` cannot feed the next measure. */
export function measureChromeHeight(el: {
  style: { minHeight: string };
  getBoundingClientRect(): { height: number };
}): number {
  el.style.minHeight = '0';
  return el.getBoundingClientRect().height;
}

/** `token_required` is the truthful boot projection for a listener whose
 *  /status preflight proves a token is required while none is stored: no
 *  socket is opened, the reconnect timer stays suppressed, and the
 *  actionable Settings → Connection panel owns recovery. */
type ConnState = 'off' | 'connecting' | 'on' | 'reconnecting' | 'token_required';

// The Item shape, nextId/contentText helpers, and messagesToItems live in
// ./transcript (shared with CollabGuestView) and are re-exported here so
// existing `import { … } from './App'` callers keep compiling.
export { nextId, contentText, messagesToItems, type Item } from './transcript';

interface SessionLifecycleView {
  displayedSessionId: string;
  commandSessionId: string;
  pendingGeneration: number | null;
  pendingPhase: SessionSwitchPhase | null;
  authoritativeReady: boolean;
  stateMessages: number;
  projectedDom: number;
}

const EMPTY_LIFECYCLE_VIEW: SessionLifecycleView = {
  displayedSessionId: '',
  commandSessionId: '',
  pendingGeneration: null,
  pendingPhase: null,
  authoritativeReady: false,
  stateMessages: 0,
  projectedDom: 0,
};

const EMPTY_ITEMS: Item[] = [];

interface TranscriptSurfaceProps {
  rk: RuntimeKey;
  sessionId: string;
  items: Item[];
  active: boolean;
  connState: ConnState;
  workspaceCwd: string;
  initialStart: number;
  itemsVersion: number;
  getViewport: (rk: RuntimeKey) => LiveTranscriptViewport | null;
  scrollRef: RefObject<HTMLDivElement | null>;
  pinIfPinned: () => void;
  forcePin: () => void;
  onContentEl: (rk: RuntimeKey, el: HTMLDivElement | null) => void;
  onCommit: (rk: RuntimeKey, itemsVersion: number, projectedCount: number) => void;
  /** O(1) item id -> projection index resolver (App-owned id->index map). */
  getAnchorIndex: (sessionId: string, anchorItemId: string) => number;
}

const SurfaceItems = memo(function SurfaceItems({ items, sessionId, workspaceCwd, pinIfPinned }: {
  items: Item[];
  sessionId: string;
  workspaceCwd: string;
  pinIfPinned: () => void;
}) {
  const pathContext = transcriptPathContext(items);
  return (
    <>
      {items.map((item) => {
        switch (item.kind) {
          case 'user': {
            const userImagesList = item.images ?? [];
            const analysis = item.analysis;
            const largeText = item.text !== '' ? largeTextDisplay(item.text) : null;
            return (
              <div key={item.id} {...transcriptItemStamp(item.id)} className={`msg msg--user${item.optimistic ? ' optimistic' : ''}`} data-session-id={sessionId ?? ''} data-workspace-cwd={workspaceCwd}>
                {userImagesList.length > 0 && (
                  <div className={`msg--user__images${userImagesList.length > 1 ? ' msg--user__images--grid' : ''}`}>
                    {userImagesList.map((image, index) => (
                      <img
                        key={`image-${index}`}
                        className="msg--user__image"
                        src={`data:${image.mimeType};base64,${image.data}`}
                        alt="Attached image"
                        loading="lazy"
                      />
                    ))}
                  </div>
                )}
                {largeText ? (
                  <details className="msg--user__large-text" {...transcriptFoldStamp(item.id, 0, 'msg--user__large-text')}>
                    <summary>
                      Large message · {largeText.characters} characters · {largeText.bytes} bytes
                    </summary>
                    <pre>{largeText.preview}</pre>
                  </details>
                ) : item.text !== '' ? (
                  <MarkdownBody className="msg--user__text" text={item.text} pathContext={pathContext} onLayoutChange={pinIfPinned} />
                ) : null}
                {analysis && (
                  <details className="msg--user__analysis" {...transcriptFoldStamp(item.id, 0, 'msg--user__analysis')}>
                    <summary className="msg--user__analysis-summary">
                      <span className="msg--user__analysis-label">Image analysis</span>
                      <span className="msg--user__analysis-model">{safeText(analysis.model)}</span>
                    </summary>
                    <div className="msg--user__analysis-body">{safeText(analysis.description)}</div>
                  </details>
                )}
              </div>
            );
          }
          case 'assistant':
            return item.status === 'streaming' ? (
              <StreamingAssistant key={item.id} sid={sessionId} id={item.id} workspaceCwd={workspaceCwd} pathContext={pathContext} />
            ) : (
              <FinalAssistant key={item.id} blocks={item.blocks} errorMessage={item.errorMessage} sessionId={sessionId} workspaceCwd={workspaceCwd} onLayoutChange={pinIfPinned} itemId={item.id} pathContext={pathContext} />
            );
          case 'toolCard':
            return <div key={item.id} {...transcriptItemStamp(item.id)} data-session-id={sessionId ?? ''} data-workspace-cwd={workspaceCwd}><ToolCard item={item} onLayoutChange={pinIfPinned} /></div>;
          case 'toolResult':
            return (
              <BashCard
                key={item.id}
                label="tool output"
                output={item.text === '' ? '(empty tool result)' : item.text}
                itemId={item.id}
              />
            );
          case 'bash':
            return <BashCard key={item.id} command={item.command} output={item.output} status={item.status} itemId={item.id} />;
          case 'custom':
            return (
              <div key={item.id} className="msg msg--custom" role="note" {...transcriptItemStamp(item.id)} data-session-id={sessionId ?? ''} data-workspace-cwd={workspaceCwd}>
                <span className="msg--custom__label">{safeText(item.label)}</span>
                <MarkdownBody className="msg--custom__text" text={item.text} pathContext={pathContext} onLayoutChange={pinIfPinned} />
              </div>
            );
          case 'irc':
            return <div key={item.id} {...transcriptItemStamp(item.id)} data-session-id={sessionId ?? ''} data-workspace-cwd={workspaceCwd}><IrcCard item={item} onLayoutChange={pinIfPinned} /></div>;
          case 'summary':
            return (
              <div key={item.id} className="msg msg--summary" role="note" {...transcriptItemStamp(item.id)} data-session-id={sessionId ?? ''} data-workspace-cwd={workspaceCwd}>
                <span className="msg--summary__label">{safeText(item.label)}</span>
                <MarkdownBody className="msg--summary__text" text={item.text} pathContext={pathContext} onLayoutChange={pinIfPinned} />
              </div>
            );
          case 'handoff-divider':
            return (
              <div key={item.id} className="msg msg--handoff-divider" role="separator" aria-label="Session handoff" {...transcriptItemStamp(item.id)} data-session-id={sessionId ?? ''} data-workspace-cwd={workspaceCwd}>
                <span className="msg--handoff-divider__label">Session handoff</span>
                <pre className="msg--handoff-divider__doc">{item.text}</pre>
              </div>
            );
          case 'approval':
            return (
              <div key={item.id} className="approval" role="status" {...transcriptItemStamp(item.id)}>
                <div className="approval__head">
                  <span className="approval__method">{item.method}</span>
                  <span className="approval__title">{safeText(item.title)}</span>
                  {item.extensionId && <span className="approval__extension">{item.extensionId}</span>}
                </div>
                {item.message !== '' && <div className="approval__question">{safeText(item.message)}</div>}
                <div className="approval__note">Answer in the terminal — remote answering is disabled by design.</div>
              </div>
            );
        }
      })}
    </>
  );
});

export const TranscriptSurface = memo(function TranscriptSurface({
  rk,
  sessionId,
  items,
  itemsVersion,
  active,
  connState,
  workspaceCwd,
  initialStart,
  getViewport,
  scrollRef,
  pinIfPinned,
  forcePin,
  onContentEl,
  onCommit,
  getAnchorIndex,
}: TranscriptSurfaceProps) {
  const [windowStart, setWindowStart] = useState(initialStart);
  const windowStartRef = useRef(windowStart);
  windowStartRef.current = windowStart;
  const itemsRef = useRef(items);
  itemsRef.current = items;
  const activeRef = useRef(active);
  activeRef.current = active;
  // The walk latch as STATE (not a ref): a scroll that actually mutates the
  // window sets it, a resting/no-op scroll clears it, and every
  // activation/deactivation resets it — each transition re-renders so the
  // parked planner re-plans immediately (a ref flip would be invisible until
  // an unrelated render).
  const [walkOwned, setWalkOwned] = useState(false);
  const liveViewport = getViewport(rk);
  const follow = liveViewport?.follow ?? true;
  const anchorItemId = liveViewport?.anchorItemId ?? null;
  // The STORED demand window must equal the RENDERED window. The render-time
  // normalize masks a stale stored start while following (a surface mounted
  // BEFORE its messages arrived — every negotiated cold target: ensureSurface
  // runs while the item cache is still empty, so initialStart =
  // tailProjectionStart(0) = 0 — rides the tail through normalize, and the
  // mount can even batch with the first data so no later length change ever
  // re-seats it) — but decide()'s demand math reads the STORED start, so a
  // masked mismatch leaves it operating on a window that was never rendered:
  // a parked walk sees start=0, the older-shift can never fire, and the walk
  // can never reach the session head. Syncing the stored start to the
  // normalized one is a render-phase update: React re-renders before
  // committing, the DOM never sees an intermediate window, and decide()
  // always reads the truth.
  const normalizedStart = normalizeTranscriptWindow(items.length, windowStart, follow);
  if (normalizedStart !== windowStart) setWindowStart(normalizedStart);
  // PARKED coverage: a parked surface must render the saved reading position
  // AND the session tail (the ownership marker) together. planParkedWindow:
  // a tail-chunk anchor rides the canonical tail window; an anchor BELOW the
  // tail chunk switches the projection to bounded DISJOINT retained slices
  // (the anchor neighborhood + the tail) — REGARDLESS of whether the stored
  // window already covers the anchor (a pre-existing anchor-centered stored
  // window alone would render the reading position without the tail
  // marker). The plan applies ONLY to the parked/retained view state: any
  // demand shift (decide()) ends slice mode (its collapse restores a single
  // anchor-centered window for the walk's exact-window contract), and every
  // activation resets the walk latch, via demandShiftedRef below. The index
  // resolves through the O(1) id->index map — never a sibling walk.
  const anchorIndex = !follow && anchorItemId !== null && items.length > 0
    ? getAnchorIndex(sessionId, anchorItemId)
    : -1;
  const parkedPlan = !walkOwned && !follow
    ? (anchorIndex >= 0
      ? planParkedWindow(items.length, anchorIndex)
      : { kind: 'tail' as const, start: tailProjectionStart(items.length) })
    : null;
  if (parkedPlan?.kind === 'tail') {
    if (parkedPlan.start !== windowStart) setWindowStart(parkedPlan.start);
  }
  // kind 'stored': the canonical resting window already covers the anchor —
  // the single-window projection below renders it unchanged.
  const parkedSlices = parkedPlan?.kind === 'slices' ? parkedPlan : null;
  const sliceModeRef = useRef(false);
  const sliceAnchorRef = useRef(-1);
  sliceModeRef.current = parkedSlices !== null;
  sliceAnchorRef.current = parkedSlices ? anchorIndex : -1;
  const projection = useMemo(
    () => projectTranscriptWindow(items.length, normalizeTranscriptWindow(items.length, windowStart, follow)),
    [follow, items.length, windowStart],
  );
  const projectedItems = useMemo(
    () => parkedSlices
      ? [
        ...items.slice(parkedSlices.anchor.start, parkedSlices.anchor.end),
        ...items.slice(parkedSlices.tail.start, parkedSlices.tail.end),
      ]
      : (projection.complete ? items : items.slice(projection.start, projection.end)),
    [items, parkedSlices, projection],
  );
  const contentRef = useRef<HTMLDivElement | null>(null);
  const registerContent = useCallback((el: HTMLDivElement | null) => {
    // The demand-shift machinery (decide() and the stage compensation below)
    // reads this exact element; a ref callback that only forwards to
    // onContentEl leaves it null forever and both effects early-return, so
    // the window never slides and the demand walk can never reach the head.
    contentRef.current = el;
    onContentEl(rk, el);
  }, [onContentEl, rk]);
  const surfaceRef = useRef<HTMLDivElement | null>(null);
  useLayoutEffect(() => {
    const surface = surfaceRef.current;
    if (surface) applySurfaceVisibility(surface, active);
  }, [active]);
  // Versioned commit: ready re-arms only after this surface rendered the current items sequence.
  useLayoutEffect(() => {
    onCommit(rk, itemsVersion, projectedItems.length);
  }, [itemsVersion, onCommit, projection.start, projection.end, projectedItems.length, rk]);
  // Every activation/deactivation re-plans the parked view: the walk latch
  // only guards the window WHILE a demand walk owns it.
  useLayoutEffect(() => {
    setWalkOwned(false);
    if (!active) return;
    const viewport = getViewport(rk);
    if (viewport && viewport.follow === false && viewport.anchorItemId !== null) return;
    forcePin();
  }, [active, forcePin, getViewport, rk]);
  useLayoutEffect(() => {
    if (active) pinIfPinned();
  }, [active, items, pinIfPinned]);
  // Demand scroll: near either edge the window slides by CHUNK - OVERLAP (older at top, newer at bottom), keeping a rendered overlap so the measured anchor node survives its own commit; DOM stays bounded; inactive surfaces never shift.
  const shiftRafRef = useRef<number | null>(null);
  const removalPxRef = useRef(0);
  useEffect(() => {
    if (!active) return;
    const decide = () => {
      const scroll = scrollRef.current;
      const content = contentRef.current;
      if (!scroll || !content) return;
      const total = itemsRef.current.length;
      if (total === 0) return;
      const liveFollow = getViewport(rk)?.follow ?? true;
      const storedStart = windowStartRef.current;
      const planned = planDemandProjectionDecision({
        total,
        start: storedStart,
        follow: liveFollow,
        atOlderEdge: scroll.scrollTop <= TRANSCRIPT_PROJECTION_EDGE_PX,
        atNewerEdge: scroll.scrollTop + scroll.clientHeight >= scroll.scrollHeight - TRANSCRIPT_PROJECTION_EDGE_PX,
        inSliceMode: sliceModeRef.current,
        sliceAnchor: sliceAnchorRef.current,
        selection: null,
        root: null,
        gestureActive: false,
      });
      if (planned.kind === 'sliceCollapse') sliceModeRef.current = false;
      if (planned.kind === 'shift' && planned.nextStart > storedStart) {
        const children = content.children;
        const kept = children.length > planned.nextStart - storedStart
          ? children[planned.nextStart - storedStart]
          : null;
        removalPxRef.current = kept
          ? Math.max(0, kept.getBoundingClientRect().top - children[0].getBoundingClientRect().top)
          : 0;
      }
      if (planned.commit) {
        setWindowStart(planned.nextStart);
        setWalkOwned(true);
      }
    };
    const onScroll = () => {
      if (shiftRafRef.current !== null) return;
      shiftRafRef.current = window.requestAnimationFrame(() => {
        shiftRafRef.current = null;
        if (activeRef.current) decide();
      });
    };
    const scroll = scrollRef.current;
    if (!scroll) return;
    scroll.addEventListener('scroll', onScroll, { passive: true });
    return () => {
      scroll.removeEventListener('scroll', onScroll);
      if (shiftRafRef.current !== null) window.cancelAnimationFrame(shiftRafRef.current);
    };
  }, [active, getViewport, rk, scrollRef]);
  const stageRef = useRef<number | null>(null);
  useLayoutEffect(() => {
    const previousStage = stageRef.current;
    stageRef.current = projection.start;
    const scroll = scrollRef.current;
    const content = contentRef.current;
    if (!active || previousStage === null || scroll === null || content === null) return;
    const live = getViewport(rk);
    const prepended = previousStage - projection.start;
    if (prepended > 0) {
      const outcome = compensatePrependedStage({
        scroll,
        content: transcriptContentRoot(content),
        prepended,
        openFoldKeys: live?.openFoldKeys ?? new Set<string>(),
      });
      if (live?.follow) pinIfPinned();
      else if (outcome.deltaPx > 0) scroll.scrollTop += outcome.deltaPx;
      return;
    }
    const dropped = projection.start - previousStage;
    if (dropped <= 0) return;
    const removedPx = removalPxRef.current;
    removalPxRef.current = 0;
    if (live?.follow) pinIfPinned();
    else if (removedPx > 0) scroll.scrollTop = Math.max(0, scroll.scrollTop - removedPx);
  }, [active, getViewport, pinIfPinned, projection.start, projection.end, rk, scrollRef]);
  return (
    <div
      className="transcript-surface"
      data-runtime-key={rk}
      data-window-start={windowStart}
      data-live-follow={follow ? 'true' : 'false'}
      data-live-anchor={anchorItemId ?? ''}
      ref={(el) => {
        surfaceRef.current = el;
        if (el) applySurfaceVisibility(el, active);
      }}
    >
      <div className="transcript-content" ref={registerContent}>
        {items.length === 0 && (
          <div className="empty-hint">
            {connState === 'on' ? (
              <>Send a prompt to start the session.</>
            ) : connState === 'token_required' ? (
              <>
                This listener requires an access token. Paste it in the pairing
                card — it is stored for this listener only and connects
                immediately. No reconnect is attempted until the token is
                saved.
              </>
            ) : (
              <>
                Connecting to the control plane… The token is optional: leave it blank when
                the listener runs without <code>--listen-token-file</code> (the page
                auto-connects); if the listener was started with a token, set it under
                Settings → Connection and the page reconnects immediately.
              </>
            )}
          </div>
        )}
        <SurfaceItems items={projectedItems} sessionId={sessionId} workspaceCwd={workspaceCwd} pinIfPinned={pinIfPinned} />
      </div>
    </div>
  );
});

function jobsFromJobList(data: unknown): unknown[] | undefined {
  if (!data || typeof data !== 'object' || !('jobs' in data) || !Array.isArray(data.jobs)) {
    return undefined;
  }
  return data.jobs;
}

export interface LiveNodes {
  textEl: HTMLDivElement | null;
  thinkingDetails: HTMLDetailsElement | null;
  thinkingBody: HTMLDivElement | null;
  pathContext: string;
}

export interface StreamBuffer {
  text: string;
  thinking: string;
}

// Module-level registries for the hot streaming path. Assistant text buffers
// are rendered at most once per animation frame; thinking deltas keep their
// O(delta) text-node append path. Both registries use the exact
// `streamKey(sessionId, itemId)`, so equal item ids in different sessions can
// never cross-wire.
export const liveNodes = new Map<string, LiveNodes>();
export const streamBuf = new Map<string, StreamBuffer>();

// Stream keys whose thinking body already shows full-buffer-normalized text
// (escaped-newline content re-rendered from the buffer once it qualified via
// shouldNormalizeThinkingNewlines). Subsequent deltas keep the normalized
// form so the DOM never regresses to a literal `\n` mid-stream.
export const normalizedThinkingKeys = new Set<string>();

interface PendingTextRender {
  raf: number;
  owner: HTMLDivElement;
}

// One provisional Markdown commit per exact session+assistant key per frame.
// The full accumulated text is re-rendered through the ordinary sanitizer so
// an incomplete inline/fence tail can safely change meaning on the next chunk.
const pendingTextRenders = new Map<string, PendingTextRender>();

function cancelTextRender(key: string, owner?: HTMLDivElement | null): void {
  const pending = pendingTextRenders.get(key);
  if (!pending || (owner && pending.owner !== owner)) return;
  window.cancelAnimationFrame(pending.raf);
  pendingTextRenders.delete(key);
}

function cancelTextRendersForSession(sid: string): void {
  const prefix = `${sid}\u0000`;
  for (const key of [...pendingTextRenders.keys()]) {
    if (key.startsWith(prefix)) cancelTextRender(key);
  }
}

function cancelAllTextRenders(): void {
  for (const key of [...pendingTextRenders.keys()]) cancelTextRender(key);
}

function clearStreamStateForKey(key: string): void {
  cancelTextRender(key);
  streamBuf.delete(key);
  normalizedThinkingKeys.delete(key);
  liveNodes.delete(key);
}

function clearStreamStateForSession(sid: string): void {
  const prefix = `${sid}\u0000`;
  cancelTextRendersForSession(sid);
  for (const key of [...streamBuf.keys()]) {
    if (key.startsWith(prefix)) streamBuf.delete(key);
  }
  for (const key of [...normalizedThinkingKeys]) {
    if (key.startsWith(prefix)) normalizedThinkingKeys.delete(key);
  }
  for (const key of [...liveNodes.keys()]) {
    if (key.startsWith(prefix)) liveNodes.delete(key);
  }
}

function clearAllStreamState(): void {
  cancelAllTextRenders();
  streamBuf.clear();
  normalizedThinkingKeys.clear();
  liveNodes.clear();
}

function scheduleTextRender(sid: string, itemId: string, onCommitted?: () => void): void {
  const key = streamKey(sid, itemId);
  if (pendingTextRenders.has(key)) return;
  const owner = liveNodes.get(key)?.textEl;
  if (!owner) return;
  const pending: PendingTextRender = { raf: 0, owner };
  pending.raf = window.requestAnimationFrame(() => {
    if (pendingTextRenders.get(key) !== pending) return;
    pendingTextRenders.delete(key);
    const current = liveNodes.get(key);
    const buf = streamBuf.get(key);
    // The exact node advertises its key in the DOM. A remount, final
    // replacement, projection invalidation, or session cutover revokes this
    // callback even if an item id is later reused.
    if (
      !owner.isConnected
      || owner.dataset.streamKey !== key
      || !current
      || current.textEl !== owner
      || !buf
    ) return;
    owner.innerHTML = renderStreamingMarkdown(buf.text, current.pathContext || buf.text);
    onCommitted?.();
  });
  pendingTextRenders.set(key, pending);
}

export function streamKey(sid: string | null, itemId: string): string {
  return `${sid ?? ''}\u0000${itemId}`;
}

/** Route one delta to the exact mounted `(sid, itemId)` node. Text rendering
 * is coalesced through the full-buffer Markdown renderer above; thinking uses
 * an in-place text-node append so long reasoning streams stay O(delta). */
function appendDeltaText(el: HTMLElement, safe: string): void {
  const first = el.firstChild;
  if (first && first.nodeType === Node.TEXT_NODE) {
    (first as Text).appendData(safe);
  } else {
    el.textContent += safe;
  }
}

export function applyDeltaToNode(
  sid: string | null,
  itemId: string,
  delta: string,
  kind: 'text' | 'thinking',
  onTextCommitted?: () => void,
): void {
  if (!sid) return;
  const key = streamKey(sid, itemId);
  const node = liveNodes.get(key);
  if (!node) return;
  if (kind === 'text') {
    scheduleTextRender(sid, itemId, onTextCommitted);
  } else if (node.thinkingBody && node.thinkingDetails) {
    node.thinkingDetails.hidden = false;
    // Escaped-newline normalization is a whole-text decision, so it reads the
    // accumulated buffer (the caller appends to it BEFORE routing the delta
    // here). Once the buffer qualifies (no real newlines + multiple literal
    // `\n`), the body is re-rendered from the normalized buffer once and the
    // key is marked so later deltas append in the same normalized form —
    // keeping O(delta) appends for the rest of the stream.
    const buf = streamBuf.get(key);
    if (buf && shouldNormalizeThinkingNewlines(buf.thinking)) {
      node.thinkingBody.textContent = normalizeThinkingNewlines(buf.thinking);
      normalizedThinkingKeys.add(key);
    } else if (normalizedThinkingKeys.has(key)) {
      appendDeltaText(node.thinkingBody, safeText(unescapeThinkingNewlines(delta)));
    } else {
      appendDeltaText(node.thinkingBody, safeText(delta));
    }
  }
}

/** Event types that bump a BACKGROUND session's unread badge. High-frequency
 *  deltas (message_update / tool_execution_update) stay silent so a slow
 *  background stream counts once, not per chunk. */
const UNREAD_EVENT_TYPES: Record<string, true> = {
  turn_start: true,
  message_start: true,
  message_end: true,
  agent_settled: true,
  run_failed: true,
  tool_execution_start: true,
  tool_execution_end: true,
  todo_updated: true,
  todo_child_routed: true,
  todo_reminder: true,
  workflow_updated: true,
  workflow_status_changed: true,
  workflow_removed: true,
  goal_updated: true,
  goal_usage_charged: true,
  job_updated: true,
  agent_updated: true,
  message_delivered: true,
  extension_ui_request: true,
};

// nextId, contentText, and messagesToItems (plus the output-bounding and
// custom-visibility helpers used by the live handlers below) live in
// ./transcript so live events and restored entries share one normalization.

/* ------------------------------------------------------------------ *
 * Small presentational components
 * ------------------------------------------------------------------ */

export function StreamingAssistant({ sid, id, workspaceCwd, pathContext = '' }: { sid: string; id: string; workspaceCwd?: string; pathContext?: string }) {
  const textRef = useRef<HTMLDivElement>(null);
  const thinkingRef = useRef<HTMLDetailsElement>(null);
  const thinkingBodyRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const key = streamKey(sid, id);
    const entry: LiveNodes = {
      textEl: textRef.current,
      thinkingDetails: thinkingRef.current,
      thinkingBody: thinkingBodyRef.current,
      pathContext,
    };
    liveNodes.set(key, entry);
    const buf = streamBuf.get(key);
    if (buf) {
      if (buf.text && textRef.current) textRef.current.innerHTML = renderStreamingMarkdown(buf.text, pathContext || buf.text);
      if (buf.thinking && thinkingBodyRef.current && thinkingRef.current) {
        thinkingRef.current.hidden = false;
        // Late-mount catch-up writes the WHOLE buffer at once, so the same
        // conservative normalization the live delta path applies fits here.
        thinkingBodyRef.current.textContent = normalizeThinkingNewlines(buf.thinking);
      }
    }
    return () => {
      cancelTextRender(key, entry.textEl);
      if (liveNodes.get(key) === entry) liveNodes.delete(key);
    };
  }, [sid, id, pathContext]);
  return (
    <div className="msg msg--assistant" {...transcriptItemStamp(id)} data-session-id={sid} data-workspace-cwd={workspaceCwd}>
      {/* `open` = thinking body visible by default while streaming, still
          collapsible via the summary; `hidden` keeps the whole block away
          until the first thinking delta arrives. The stamped fold key lets
          the live viewport's capture listener record toggles in O(1). */}
      <details className="thinking" open hidden ref={thinkingRef} {...transcriptFoldStamp(id, 0, 'thinking')}>
        {/* Shared React-free header (markdown.ts): brain icon + Thinking —
            identical for the live stream and restored/final rendering. */}
        <summary className="thinking__summary" dangerouslySetInnerHTML={{ __html: thinkingSummaryHtml() }} />
        <div className="thinking__body" ref={thinkingBodyRef} />
      </details>
      <div
        className="assistant-text"
        data-stream-key={streamKey(sid, id)}
        data-stream-session-id={sid}
        data-stream-item-id={id}
        ref={textRef}
      />
    </div>
  );
}

export function FinalAssistant({ blocks, errorMessage, sessionId = '', workspaceCwd, onLayoutChange, itemId, pathContext }: { blocks: ContentBlock[]; errorMessage?: string; sessionId?: string; workspaceCwd?: string; onLayoutChange?: () => void; itemId?: string; pathContext?: string }) {
  const textRef = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const node = textRef.current;
    if (!node) return;
    // Commit-time pin, BEFORE paint: the streaming->final render replaces
    // the streamed text with the rendered blocks, which can transiently
    // shrink scrollHeight and let the browser clamp scrollTop mid-content.
    // Re-pinning in the same layout pass (before the clamped position is
    // painted) keeps every painted frame glued to the bottom; the parent's
    // post-paint passive effect would land one frame late.
    onLayoutChange?.();
    // Mermaid fences render asynchronously: hydrate the hosts after
    // dangerouslySetInnerHTML commits. Scoped to the final text node so the
    // streaming path (textContent deltas) is never touched. `onLayoutChange`
    // is invoked synchronously after each host mutation so the transcript
    // pin re-glues before the next frame. Optional callers without a pin
    // controller keep the previous behavior.
    void hydrateMermaid(node, onLayoutChange);
    // Final-block images (markdown `![..](..)` and image content blocks)
    // decode asynchronously and grow layout with no React commit and no
    // delta. A scoped capture load handler re-pins synchronously with the
    // decode; the pin controller preserves a deliberate user scroll-away
    // (unpinned freeze). Data-URL decodes can race ahead of this layout
    // effect, so a complete image pins once immediately instead of waiting
    // for a load event that already fired.
    const imgs = Array.from(node.querySelectorAll<HTMLImageElement>('img.md-image'));
    const listeners: Array<[HTMLImageElement, () => void]> = [];
    for (const img of imgs) {
      if (img.complete) {
        onLayoutChange?.();
        continue;
      }
      const onLoad = () => onLayoutChange?.();
      img.addEventListener('load', onLoad, { capture: true, once: true });
      listeners.push([img, onLoad]);
    }
    if (listeners.length === 0) return;
    return () => {
      for (const [img, onLoad] of listeners) img.removeEventListener('load', onLoad, { capture: true });
    };
  }, [onLayoutChange]);
  return (
    <div className="msg msg--assistant" {...(itemId !== undefined ? transcriptItemStamp(itemId) : {})} data-session-id={sessionId} data-workspace-cwd={workspaceCwd}>
      <div className="assistant-text" ref={textRef} dangerouslySetInnerHTML={{ __html: renderBlocks(blocks, pathContext) }} />
      {errorMessage ? <div className="msg--assistant__error" role="note">{safeText(errorMessage)}</div> : null}
    </div>
  );
}

// TUI Todo-DAG task markers (crates/pi-cli/src/todo_dag_view.rs `task_marker`):
// ○ pending, ● in_progress, ✓ completed, × abandoned. Status is conveyed by
// the marker glyph + color; the in-progress task additionally gets the
// --active row highlight.
const TODO_TASK_MARKERS: Record<string, string> = {
  pending: '\u25CB',
  in_progress: '\u25CF',
  completed: '\u2713',
  abandoned: '\u00D7',
};

/** Shared typed orchestration IRC card (host App.tsx and CollabGuestView
 *  render the SAME component so the transcripts can never drift). Title =
 *  direction arrow + party (`IRC ← sender` / `IRC → recipient`, mirroring the
 *  TUI's `OrchestrationIrcView::label`); the body renders through the SHARED
 *  MarkdownBody pipeline (escapeHtml-first, whitelisted links/images, code
 *  fences, Mermaid hosts hydrated after commit) on the visual quote rail —
 *  never as pre-wrapped raw text, so headings/lists/inline code display
 *  structurally and hostile HTML stays literal; replyTo is an independent
 *  muted line (never embedded in the body); compact clamps to 6 visual lines
 *  with an expand toggle up to the 40-line parse bound. Mermaid/image
 *  mutations re-pin via onLayoutChange (same contract as FinalAssistant). */
export function IrcCard({ item, onLayoutChange }: {
  item: Extract<Item, { kind: 'irc' }>;
  onLayoutChange?: () => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const bodyRef = useRef<HTMLDivElement>(null);
  const [overflows, setOverflows] = useState(false);
  useEffect(() => {
    const el = bodyRef.current;
    if (!el) return;
    // The compact clamp hides overflow (scrollHeight > clientHeight); a body
    // that fits fully (or a single long wrapped line) still gets a toggle
    // when it visually overflows.
    const check = () => setOverflows(el.scrollHeight > el.clientHeight + 1);
    check();
    const ro = new ResizeObserver(check);
    ro.observe(el);
    return () => ro.disconnect();
  }, [item.body]);
  const manyLines = item.body.split('\n').length > IRC_COMPACT_LINE_LIMIT;
  const showToggle = manyLines || overflows;
  return (
    <div
      className={`msg msg--irc msg--irc--${item.direction}`}
      data-irc-direction={item.direction}
      data-irc-from={item.from}
      data-irc-to={item.to}
      role="note"
    >
      <div className="msg--irc__head">
        <span className="msg--irc__title">{safeText(ircTitle(item.from, item.to))}</span>
        {item.replyTo ? (
          <span className="msg--irc__reply">reply to {safeText(item.replyTo)}</span>
        ) : null}
      </div>
      <MarkdownBody
        bodyRef={bodyRef}
        className={`msg--irc__body${expanded ? ' is-expanded' : ''}`}
        text={item.body}
        onLayoutChange={onLayoutChange}
      />
      {showToggle && (
        <button
          type="button"
          className="msg--irc__toggle"
          aria-expanded={expanded}
          onClick={() => setExpanded((value) => !value)}
        >
          {expanded ? 'collapse' : 'expand'}
        </button>
      )}
    </div>
  );
}

/** Hub tool-card Markdown fold: wait/send bodies and fallback notes render
 *  through the SHARED MarkdownBody component (escapeHtml-first, whitelisted
 *  links/images, code fences, Mermaid hosts hydrated after commit) — never
 *  as raw text, so Markdown bullets/inline code/path lists display
 *  structurally and hostile HTML stays literal. Compact clamps to
 *  IRC_COMPACT_LINE_LIMIT visual lines with an expand toggle up to the
 *  parse-time bound, mirroring the IRC card fold. Mermaid/image mutations
 *  re-pin via onLayoutChange (same contract as FinalAssistant). */
function HubMarkdownFold({ text, className, onLayoutChange }: {
  text: string;
  className?: string;
  onLayoutChange?: () => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const bodyRef = useRef<HTMLDivElement>(null);
  const [overflows, setOverflows] = useState(false);
  useEffect(() => {
    const el = bodyRef.current;
    if (!el) return;
    // The compact clamp hides overflow (scrollHeight > clientHeight); content
    // that fits fully (or a single long wrapped line) still gets a toggle
    // when it visually overflows.
    const check = () => setOverflows(el.scrollHeight > el.clientHeight + 1);
    check();
    const ro = new ResizeObserver(check);
    ro.observe(el);
    return () => ro.disconnect();
  }, [text]);
  const manyLines = text.split('\n').length > IRC_COMPACT_LINE_LIMIT;
  const showToggle = manyLines || overflows;
  return (
    <div className={className ?? ''}>
      <MarkdownBody
        bodyRef={bodyRef}
        className={`tool-card__hub-md${expanded ? ' is-expanded' : ''}`}
        text={text}
        onLayoutChange={onLayoutChange}
      />
      {showToggle && (
        <button
          type="button"
          className="tool-card__hub-toggle"
          aria-expanded={expanded}
          onClick={() => setExpanded((value) => !value)}
        >
          {expanded ? 'collapse' : 'expand'}
        </button>
      )}
    </div>
  );
}
export function TaskToolCardBody({ item, taskView }: {
  item: Extract<Item, { kind: 'toolCard' }>;
  taskView: TaskCardView;
}) {
  // A terminal parent without spawn identities is an unrelated/preflight
  // failure: settle from item.status + item.result, never args-preview rows.
  const terminal = item.status === 'error' || item.status === 'cancelled';
  const children = terminal && !taskView.children.some((child) => child.jobId)
    ? []
    : taskView.children;
  return (
    <div className="tool-card__task">
      {taskView.sections.goal !== '' && (
        <TaskSectionFold label="Goal" body={taskView.sections.goal} />
      )}
      {taskView.sections.constraints !== '' && (
        <TaskSectionFold label="Constraints" body={taskView.sections.constraints} />
      )}
      {taskView.sections.contract !== '' && (
        <TaskSectionFold label="Contract" body={taskView.sections.contract} />
      )}
      <div className="tool-card__children">
        {children.map((child, index) => (
          <div
            key={child.jobId || child.agentId || `${child.name}-${index}`}
            className="tool-card__child"
            data-job-id={child.jobId || undefined}
            data-agent-id={child.agentId || undefined}
            data-parent-id={child.parentId || undefined}
            data-todo-task-id={child.todoTaskId || undefined}
            data-status={child.status}
          >
            <div className="tool-card__child-head">
              <span className="tool-card__child-name">{safeText(child.name)}</span>
              <span className="tool-card__child-agent">{safeText(child.agent)}</span>
              <span className="tool-card__child-status">{safeText(child.status)}</span>
            </div>
            <div className="tool-card__child-action">{safeText(taskChildActionLine(child))}</div>
            {child.error ? <div className="tool-card__child-error">{safeText(child.error)}</div> : null}
            {child.target !== '' && child.target !== taskChildActionLine(child) && (
              <details className="tool-card__child-brief">
                <summary>assignment</summary>
                <div className="tool-card__child-target">{safeText(child.target)}</div>
              </details>
            )}
            {child.result ? <div className="tool-card__child-result">{safeText(child.result)}</div> : null}
          </div>
        ))}
      </div>
      {item.result ? (
        <pre className="tool-card__output">{safeText(boundOutput(item.result, TOOL_OUTPUT_LINE_LIMIT).text)}</pre>
      ) : null}
      <details className="tool-card__raw" {...transcriptFoldStamp(item.id, 0, 'tool-card__raw')}>
        <summary>raw args</summary>
        <pre className="tool-card__args">{safeText(JSON.stringify(item.args, null, 2))}</pre>
      </details>
    </div>
  );
}

function TaskSectionFold({ label, body }: { label: string; body: string }) {
  const preview = firstLinePreview(body);
  const folded = body.includes('\n') || body.length > preview.length;
  if (!folded) {
    return (
      <div className="tool-card__section">
        <div className="tool-card__section-label">{label}</div>
        <div className="tool-card__section-body">{safeText(body)}</div>
      </div>
    );
  }
  return (
    <details className="tool-card__section">
      <summary className="tool-card__section-summary">
        <span className="tool-card__section-label">{label}</span>
        <span className="tool-card__section-preview">{safeText(preview)}</span>
      </summary>
      <div className="tool-card__section-body">{safeText(body)}</div>
    </details>
  );
}

export function ToolCard({ item, onLayoutChange }: { item: Extract<Item, { kind: 'toolCard' }>; onLayoutChange?: () => void }) {
  const toolName = item.toolName.toLowerCase();
  const visibleStatus = taskCardStatus(item);
  const taskView = toolName === 'task' ? resolveTaskCardView(item.args, item.details, item.status) : null;
  const todoView = toolName === 'todo' ? resolveTodoCardView(item.args, item.details, item.result, item.status) : null;
  const editView = toolName === 'edit' ? parseEditCard(item.args, item.details, item.result) : null;
  const commandView = toolName === 'bash' ? parseCommandCardArgs(item.args) : null;
  const processView = toolName === 'process' ? parseProcessCardArgs(item.args) : null;
  const writeView = toolName === 'write' ? parseWriteCardArgs(item.args, item.result, item.status) : null;
  const readView = toolName === 'read' ? parseReadCard(item.args, item.result, item.status) : null;
  const hubView = toolName === 'hub' ? parseHubCard(item.args, item.details, item.result, item.status) : null;
  const variant = todoView ? 'todo' : taskView ? 'task' : editView ? 'edit' : commandView ? 'command'
    : processView ? 'process' : writeView ? 'write' : readView ? 'read' : hubView ? 'hub' : 'generic';
  const statusClass = visibleStatus === 'error' ? ' tool-card--error'
    : visibleStatus === 'cancelled' ? ' tool-card--cancelled'
    : visibleStatus === 'done' ? ' tool-card--done' : '';
  const isStructuredVariant = variant === 'task' || variant === 'edit';
  const isTodo = variant === 'todo';
  // Todo/command/process/write/read convey status by border color only (the
  // presentation lane forbids a visible "done"/"error" label); task/edit keep
  // their structured state label; every card labels its running state.
  const showStateLabel = isTodo
    ? visibleStatus === 'running' || visibleStatus === 'cancelled'
    : isStructuredVariant || visibleStatus === 'running' || visibleStatus === 'cancelled';
  const stateLabel = visibleStatus === 'running' ? 'running\u2026' : visibleStatus;
  const cardTitle = variant === 'todo' ? 'Todo'
    : variant === 'command' ? 'Command'
    : variant === 'process' ? 'Process'
    : variant === 'write' ? 'Write'
    : readView ? readView.title
    : hubView ? hubView.title
    : safeText(humanToolTitle(item.toolName));
  const titleClass = isStructuredVariant || isTodo ? 'tool-card__name' : 'tool-card__title';
  const media = item.media ?? [];
  const [readExpanded, setReadExpanded] = useState(false);
  const authorizedOpen = authorizedToolResultOpenPath(item.toolName, item.args);
  return (
    <div
      className={`tool-card tool-card--${variant}${statusClass}`}
      data-tool-name={item.toolName}
      data-tool-call-id={item.toolCallId}
      data-tool-status={visibleStatus}
    >
      <div className="tool-card__head">
        <span className={titleClass}>{cardTitle}</span>
        {showStateLabel && (
          <span className={`tool-card__state tool-card__state--${visibleStatus}`}>{stateLabel}</span>
        )}
        {commandView && (
          <CopyButton className="tool-card__command-copy" kind="command" text={commandView.command} />
        )}
        {processView && (
          <CopyButton className="tool-card__process-copy" kind="command" text={processView.command} />
        )}
      </div>
      {taskView ? (
        <TaskToolCardBody item={item} taskView={taskView} />
      ) : editView ? (
        <div className="tool-card__edit">
          <div className="tool-card__edit-meta">
            {authorizedOpen ? (
              <button
                type="button"
                className="tool-card__edit-path tool-card__path"
                data-md-path={authorizedOpen.path}
                {...(authorizedOpen.line != null ? { 'data-md-line': String(authorizedOpen.line) } : {})}
                aria-label={safeText(`Open ${authorizedOpen.path} in file explorer`)}
              >
                {safeText(editView.path)}
              </button>
            ) : (
              <span className="tool-card__edit-path">{safeText(editView.path)}</span>
            )}
            <span className="tool-card__edit-op">{safeText(editView.operation)}</span>
          </div>
          {editView.diff !== '' && (
            <pre className="tool-card__diff">
              {diffLines(editView.diff).map((line, index) => (
                <span key={`${index}-${line.kind}`} className={`tool-card__diff-line tool-card__diff-line--${line.kind}`}>
                  {safeText(line.text)}
                  {'\n'}
                </span>
              ))}
            </pre>
          )}
          <details className="tool-card__raw" {...transcriptFoldStamp(item.id, 0, 'tool-card__raw')}>
            <summary>raw args / details</summary>
            <pre className="tool-card__args">{safeText(JSON.stringify({ args: item.args, details: item.details }, null, 2))}</pre>
          </details>
        </div>
      ) : commandView ? (
        <div className="tool-card__command">
          <div className="tool-card__command-line">
            <span className="tool-card__prompt">$</span>
            <span className="tool-card__command-text">{safeText(commandView.command)}</span>
          </div>
          {item.result !== '' && <pre className="tool-card__output"><AnsiText text={item.result} /></pre>}
        </div>
      ) : processView ? (
        <div className="tool-card__summary">
          <div className="tool-card__summary-line">{safeText(processView.label)}</div>
          {item.status === 'error' && item.result !== '' && (
            <div className="tool-card__summary-error">{safeText(item.result)}</div>
          )}
          {item.status !== 'error' && item.result !== '' && (
            <pre className="tool-card__output"><AnsiText text={item.result} /></pre>
          )}
        </div>
      ) : writeView ? (
        <div className="tool-card__summary">
          {authorizedOpen ? (
            <button
              type="button"
              className="tool-card__summary-path tool-card__path"
              data-md-path={authorizedOpen.path}
              {...(authorizedOpen.line != null ? { 'data-md-line': String(authorizedOpen.line) } : {})}
              aria-label={safeText(`Open ${authorizedOpen.path} in file explorer`)}
            >
              {safeText(writeView.path)}
            </button>
          ) : (
            <div className="tool-card__summary-path">{safeText(writeView.path)}</div>
          )}
          <div className="tool-card__summary-text">{safeText(writeView.summary)}</div>
        </div>
      ) : readView ? (
        <div className="tool-card__summary">
          {readView.headline !== '' && (
            authorizedOpen ? (
              <button
                type="button"
                className="tool-card__summary-path tool-card__path"
                data-md-path={authorizedOpen.path}
                {...(authorizedOpen.line != null ? { 'data-md-line': String(authorizedOpen.line) } : {})}
                aria-label={safeText(`Open ${authorizedOpen.path} in file explorer`)}
              >
                {safeText(readView.headline)}
              </button>
            ) : (
              <div className="tool-card__summary-path">{safeText(readView.headline)}</div>
            )
          )}
          {readView.skill && readView.skill.description !== '' && (
            <div className="tool-card__summary-text">{safeText(readView.skill.description)}</div>
          )}
          {(readExpanded ? readView.expandedBody : readView.compactBody) !== '' && (
            <pre className="tool-card__output">
              <AnsiText text={readExpanded ? readView.expandedBody : readView.compactBody} />
            </pre>
          )}
          {readView.omitted > 0 && (
            <button
              type="button"
              className="tool-card__hub-toggle"
              aria-expanded={readExpanded}
              onClick={() => setReadExpanded((value) => !value)}
            >
              {readExpanded ? 'collapse' : 'expand'}
            </button>
          )}
        </div>
      ) : todoView ? (
        <div className="tool-card__todo">
          {todoView.error !== '' && (
            <div className="tool-card__summary-error">{safeText(todoView.error)}</div>
          )}
          {todoView.phases.length > 0 ? (
            <div className="tool-card__todo-phases">
              {todoView.phases.map((phase, pIndex) => (
                <div
                  key={`${pIndex}-${phase.name || 'phase'}`}
                  className="tool-card__todo-phase"
                  data-phase-name={phase.name || undefined}
                >
                  {phase.name !== '' && (
                    <div className="tool-card__todo-phase-name">{safeText(phase.name)}</div>
                  )}
                  {phase.tasks.length > 0 && (
                    <ul className="tool-card__todo-tasks">
                      {phase.tasks.map((task, tIndex) => (
                        <li
                          key={`${pIndex}-${tIndex}`}
                          className={`tool-card__todo-task${task.status === 'in_progress' ? ' tool-card__todo-task--active' : ''}`}
                          data-status={task.status}
                        >
                          <span
                            className={`tool-card__todo-marker tool-card__todo-marker--${task.status}`}
                            title={task.status}
                            aria-hidden="true"
                          >
                            {TODO_TASK_MARKERS[task.status] ?? ''}
                          </span>
                          <span className="tool-card__todo-content">{safeText(task.content)}</span>
                          {task.waitingOn.length > 0 && (
                            <span className="tool-card__todo-waiting">
                              waiting on {safeText(task.waitingOn.join(', '))}
                            </span>
                          )}
                        </li>
                      ))}
                    </ul>
                  )}
                </div>
              ))}
            </div>
          ) : todoView.fallback !== '' ? (
            <pre className="tool-card__output"><AnsiText text={todoView.fallback} /></pre>
          ) : (
            <div className="tool-card__todo-empty">no tasks</div>
          )}
          {todoView.completed.length > 0 && (
            <div className="tool-card__todo-completed">
              completed: {safeText(todoView.completed.join(', '))}
            </div>
          )}
          {todoView.phases.length > 0 && (
            <details className="tool-card__raw" {...transcriptFoldStamp(item.id, 0, 'tool-card__raw')}>
              <summary>raw args / details</summary>
              <pre className="tool-card__args">{safeText(JSON.stringify({ args: item.args, details: item.details }, null, 2))}</pre>
            </details>
          )}
        </div>
      ) : hubView ? (
        // Hub tool card: humanized wait/send/other-op view — the raw
        // `{ids, op, timeoutMs}` args envelope and internal UUIDs never
        // render in the default view (no .tool-card__raw expander). Body and
        // note content render as SAFE Markdown (shared renderMarkdown +
        // hydrateMermaid) with a compact fold + expand toggle; a running
        // wait keeps its fixed 'Waiting' headline + state label.
        <div className="tool-card__hub">
          {hubView.headline !== '' && (
            <div className="tool-card__hub-headline">{safeText(hubView.headline)}</div>
          )}
          {hubView.body !== '' && (
            <div className="tool-card__hub-body">
              <HubMarkdownFold text={hubView.body} onLayoutChange={onLayoutChange} />
              {hubView.metadata ? (
                <span className="tool-card__hub-meta">{safeText(hubView.metadata)}</span>
              ) : null}
            </div>
          )}
          {hubView.note !== '' && (
            <HubMarkdownFold text={hubView.note} className="tool-card__hub-note" onLayoutChange={onLayoutChange} />
          )}
          {hubView.reply && (
            <div className="tool-card__hub-reply">
              <div className="tool-card__hub-headline">{safeText(hubView.reply.headline)}</div>
              <div className="tool-card__hub-body">
                <HubMarkdownFold text={hubView.reply.body} onLayoutChange={onLayoutChange} />
                {hubView.reply.metadata ? (
                  <span className="tool-card__hub-meta">{safeText(hubView.reply.metadata)}</span>
                ) : null}
              </div>
            </div>
          )}
        </div>
      ) : (
        <div className="tool-card__summary">
          {compactToolArgs(item.args) !== '' && (
            <div className="tool-card__summary-line">{safeText(compactToolArgs(item.args))}</div>
          )}
          {item.result !== '' && <pre className="tool-card__output"><AnsiText text={item.result} /></pre>}
          <details className="tool-card__raw" {...transcriptFoldStamp(item.id, 0, 'tool-card__raw')}>
            <summary>raw args</summary>
            <pre className="tool-card__args">{safeText(JSON.stringify(item.args, null, 2))}</pre>
          </details>
        </div>
      )}
      {media.length > 0 && (
        <div className="tool-media">
          {media.map((asset, index) => asset.kind === 'image' ? (
            <img
              key={`image-${index}`}
              className="tool-media__image"
              src={`data:${asset.mimeType};base64,${asset.data}`}
              alt={asset.alt}
              onLoad={onLayoutChange}
            />
          ) : (
            <video
              key={`video-${index}`}
              className="tool-media__video"
              src={`data:${asset.mimeType};base64,${asset.data}`}
              aria-label={asset.alt}
              controls
              preload="metadata"
              onLoadedMetadata={onLayoutChange}
            />
          ))}
        </div>
      )}
    </div>
  );
}

/** Bash / command-output card — the web mirror of the TUI tool card
 *  (crates/pi-cli/src/tool_card_adapter.rs). When a `command` is present the
 *  header shows the fixed title "Command" (never the raw tool name) and the
 *  body leads with the command line (`$ …`, line-clamped to 2 lines) followed
 *  by the scrollable mono output. `command` omitted renders a bare output
 *  card (unmatched toolResult) with a muted label. Status is conveyed by
 *  border color only — green success, amber cancellation, red failure, and
 *  the default border while running. Shared by the session transcript and
 *  the collab guest view. */
export function BashCard({ command, label, output, status, itemId }: { command?: string; label?: string; output: string; status?: string; itemId?: string }) {
  const commandText = command ?? '';
  const hasCommand = commandText !== '';
  const statusClass = status === 'error' ? ' msg--bash--error'
    : status === 'cancelled' ? ' msg--bash--cancelled'
    : status === 'done' ? ' msg--bash--done' : '';
  return (
    <div className={`msg msg--bash${statusClass}`} {...(itemId !== undefined ? transcriptItemStamp(itemId) : {})} data-bash-status={status || undefined}>
      <div className="bash-head">
        {hasCommand ? (
          <span className="bash-cmd bash-cmd--title">Command</span>
        ) : (
          <span className="bash-cmd bash-cmd--label">{label ?? 'output'}</span>
        )}
        {status === 'running' && (
          <span className="tool-card__state tool-card__state--running">running…</span>
        )}
        <CopyButton
          className="bash-copy"
          kind={hasCommand ? 'command' : 'output'}
          text={hasCommand ? commandText : output}
        />
      </div>
      {hasCommand && (
        <div className="bash-command-line">
          <span className="bash-prompt">$</span>
          <span className="bash-command-text">{safeText(commandText)}</span>
        </div>
      )}
      <pre className="bash-output"><AnsiText text={output} /></pre>
    </div>
  );
}

export type AppToast = {
  id: string;
  message: string;
  error: boolean;
  kind?: 'transport';
  action?: { label: string; onAction: () => void };
};

export function ToastList({ toasts, dismiss }: { toasts: AppToast[]; dismiss: (id: string) => void }) {
  return (
    <div id="toasts" role="status" aria-live="polite" aria-atomic="false">
      {toasts.map((t) => (
        <div
          key={t.id}
          role={t.error ? 'alert' : undefined}
          className={`toast${t.error ? ' toast--error' : ''}${t.action ? ' toast--action' : ''}`}
          data-toast-kind={t.kind}
          onClick={() => { if (!t.action) dismiss(t.id); }}
        >
          <span className="toast__message">{t.message}</span>
          {t.action && (
            <button
              type="button"
              className="toast__action"
              onClick={(event) => {
                event.stopPropagation();
                dismiss(t.id);
                t.action?.onAction();
              }}
            >
              {t.action.label}
            </button>
          )}
        </div>
      ))}
    </div>
  );
}

/** Pending session-switch placeholder: content-shaped rows (user bubble /
 *  assistant text line / tool card) with a slow opacity-only shimmer, shown
 *  centered in the transcript while a negotiated switch is in flight. All
 *  surfaces are hidden while pending, so this is the only visible content. */
export function TranscriptSkeleton() {
  return (
    <div className="transcript-skeleton" role="status" aria-live="polite">
      <div className="transcript-skeleton__row transcript-skeleton__row--user" />
      <div className="transcript-skeleton__row transcript-skeleton__row--assistant" />
      <div className="transcript-skeleton__row transcript-skeleton__row--tool" />
      <p className="transcript-skeleton__status">Loading session…</p>
    </div>
  );
}

/* ------------------------------------------------------------------ *
 * Composer attachments + voice (file upload / hold-to-talk)
 * ------------------------------------------------------------------ */

// Composer attachment types, classification/limits/order helpers, and the
// image ContentBlock wire mapping live in ./attachments (shared by the three
// intake paths — paste, drag/drop, file picker — and unit-tested there).
import {
  type ComposerAttachment,
  type ImageContentBlock,
  type IntakeContext,
  admitBuiltAttachments,
  type PdfUploadOutcome,
  type ReadResult,
  type VideoUploadOutcome,
  MAX_COMMAND_ID,
  MAX_PROMPT_FRAME_BYTES,
  MAX_TOTAL_WIRE_BYTES,
  aggregateWire,
  attachmentAccept,
  attachmentsToImageBlocks,
  buildCodeMessage,
  buildPdfMessage,
  buildVideoMessage,
  classifyAttachments,
  codeBadgeLabel,
  commandFrameBytes,
  formatSkipSummary,
  IntakeBudgetTracker,
  intakeReservation,
  mergeIntakeResults,
  placeholderFor,
  uploadPdfFile,
  uploadVideoFile,
  videoMetaLabel,
  readAccepted,
  readAttachmentsInOrder,
  reconcileIntakeBudget,
  removeSentAttachments,
  settlePdfOutcome,
  settleVideoOutcome,
} from './attachments';
import {
  canSendAttachmentToTmux,
  sendComposerAttachmentToTmux,
} from './tmuxFile';
import {
  composerBeforeInputShouldSubmit,
  composerEnterShouldPreventDefault,
  composerEnterShouldSubmit,
  largeTextDisplay,
  nativeEventShiftKey,
  nextPasteEnterGuard,
  planLargeTextPaste,
} from './composerPaste';

/** Live voice settings as projected by the backend's Web wire
 *  (`runtimeSettings.live`): `enabled`, `mode`, `sttConfigured`,
 *  `realtimeConfigured`, `realtimeModel`, `voice`. Endpoint URLs and API
 *  keys are NEVER on this wire — they are server-held, and the browser
 *  reaches both voice paths only through the backend RPC proxy. */
interface LiveSettingsWire {
  enabled?: boolean;
  mode?: string;
  sttConfigured?: boolean;
  realtimeConfigured?: boolean;
  realtimeModel?: string;
  voice?: string;
}

/** Fixed, never-blank detail for a user-owned markdown-open rejection.
 *  An Error whose message trims empty (new Error(''), whitespace-only) and
 *  null/undefined rejections render the fixed `request failed` so the App
 *  toast can never be blank; non-Error rejections keep String(error). */
export function markdownOpenErrorDetail(error: unknown): string {
  const raw = error instanceof Error
    ? error.message
    : (error === null || error === undefined ? 'request failed' : String(error));
  return raw.trim() === '' ? 'request failed' : raw;
}

export {
  MARKDOWN_OPEN_MISSING_PATH_MAX_CHARS,
  markdownOpenBoundedPathText,
  markdownOpenMissingFileVerdict,
  markdownOpenMissingFileToast,
} from './markdown';

/**
 * Shared header <select> for the model and thinking-level pickers. The
 * empty-value placeholder is ALWAYS the first option: while a session's key
 * is still unknown ('') a browser would otherwise silently display the first
 * option, letting "no selection yet" masquerade as a real model until
 * get_state (applyState) fills the true key. A non-empty key missing from the
 * catalog still renders as its own option so a persisted session model never
 * snaps back to the placeholder.
 */
export function HeaderSelect({
  id,
  title,
  value,
  disabled,
  placeholder,
  groups,
  levels,
  onChange,
}: {
  id: string;
  title: string;
  value: string;
  disabled: boolean;
  placeholder: string;
  /** Provider-grouped model options (model select); exclusive with levels. */
  groups?: ModelProviderGroup[];
  /** Flat thinking-level options (thinking select); exclusive with groups. */
  levels?: readonly string[];
  onChange: (value: string) => void;
}) {
  return (
    <select
      id={id}
      title={title}
      disabled={disabled}
      value={value}
      onChange={(e) => onChange(e.target.value)}
    >
      <option value="">{placeholder}</option>
      {groups !== undefined
        ? groups.map((group) => (
          <optgroup key={group.provider} label={group.provider}>
            {group.models.map((m) => (
              <option key={m.value} value={m.value}>
                {m.label}
              </option>
            ))}
          </optgroup>
        ))
        : (levels ?? []).map((level) => (
          <option key={level} value={level}>
            {level}
          </option>
        ))}
    </select>
  );
}

/* ------------------------------------------------------------------ *
 * App
 * ------------------------------------------------------------------ */

export function App() {
  const [connState, setConnState] = useState<ConnState>('off');
  // Multi-session (Main contract): per-session item caches plus per-session
  // streaming/unread state. The ACTIVE session's view renders from
  // itemsBySessionId[activeSessionId]; previously-visited sessions restore
  // their cached items on switch-back (no server replay), unseen sessions
  // start empty. Events without a sessionId route to the ACTIVE session
  // (current single-Application behavior); the backend sessionId contract
  // (MultiSessionRuntimeManager) routes background sessions when it lands.
  const [itemsBySessionId, setItemsBySessionId] = useState<Record<string, Item[]>>({});
  // Synchronous mirror of the item cache (same contract as
  // streamingBySessionIdRef below): a `job_list` request captures its Task
  // omission-settlement cut from this ref at issue time, so job_updated/Task
  // spawns landing inside the request window cannot be cancelled by the
  // older authoritative response. Written only through updateItemsMirror.
  const itemsBySessionIdRef = useRef<Record<string, Item[]>>({});
  // Monotonic per-session items version: the exact sequence a surface commit must catch up to before ready re-arms.
  const itemsVersionBySessionIdRef = useRef<Record<string, number>>({});
  const [itemVersionsBySessionId, setItemVersionsBySessionId] = useState<Record<string, number>>({});
  // The ref is the sole synchronous source: each update computes `next`
  // from the ref (which already includes every prior queued update), writes
  // it back, and schedules React with the value. Same contract as
  // streamingBySessionIdRef below — every mutation MUST go through here.
  const updateItemsMirror = useCallback((updater: (prev: Record<string, Item[]>) => Record<string, Item[]>) => {
    const previous = itemsBySessionIdRef.current;
    const next = updater(previous);
    itemsBySessionIdRef.current = next;
    let versionsChanged = false;
    for (const sessionId in next) {
      if (next[sessionId] === previous[sessionId]) continue;
      const index = new Map<string, number>();
      const items = next[sessionId];
      for (let i = 0; i < items.length; i += 1) index.set(items[i].id, i);
      itemIndexBySessionIdRef.current.set(sessionId, index);
      itemsVersionBySessionIdRef.current = {
        ...itemsVersionBySessionIdRef.current,
        [sessionId]: (itemsVersionBySessionIdRef.current[sessionId] ?? 0) + 1,
      };
      versionsChanged = true;
    }
    for (const sessionId of itemIndexBySessionIdRef.current.keys()) {
      if (!(sessionId in next)) itemIndexBySessionIdRef.current.delete(sessionId);
    }
    if (versionsChanged) setItemVersionsBySessionId({ ...itemsVersionBySessionIdRef.current });
    setItemsBySessionId(() => next);
  }, []);
  const [streamingBySessionId, setStreamingBySessionId] = useState<Record<string, boolean>>({});
  const [abortingBySessionId, setAbortingBySessionId] = useState<Record<string, boolean>>({});
  const [steeringBySessionId, setSteeringBySessionId] = useState<Record<string, string[]>>({});
  /** Steer/follow_up submissions the backend REJECTED: kept visible with an
   *  error state (never silently drained by the next snapshot) until the user
   *  dismisses them. */
  const [failedSteeringBySessionId, setFailedSteeringBySessionId] = useState<Record<string, string[]>>({});
  const steeringLiveBySessionIdRef = useRef<Record<string, number>>({});
  const steeringBySessionIdRef = useRef<Record<string, string[]>>({});
  const steeringQueueGenRef = useRef(0);
  const steeringBusyRef = useRef(false);
  const gapCatchUpInFlightRef = useRef<Record<string, boolean>>({});
  const gapCatchUpSlotRef = useRef<Record<string, number>>({});
  const settledStreamRefillInFlightRef = useRef<Record<string, boolean>>({});
  const requestSettledStreamRefillRef = useRef<(sid: string) => void>(() => {});
  // Mirror of the streaming flags, read/written synchronously by the hot event
  // path. The delta path (message_update per chunk) previously called
  // setStreamingBySessionId on EVERY chunk; each call produced a NEW state
  // object, forcing a full App re-render per delta (with the whole transcript
  // re-rendered, including renderBlocks/KaTeX of every final message). The
  // mirror lets the same-value transition (true->true during a stream) skip
  // React entirely while the observable state sequence stays identical.
  const streamingBySessionIdRef = useRef<Record<string, boolean>>({});
  const abortPendingBySessionIdRef = useRef<Record<string, boolean>>({});
  const extensionNamesRef = useRef<Set<string>>(new Set());
  const cursorByRuntimeKeyRef = useRef(new Map<RuntimeKey, number>());
  const residentBySessionIdRef = useRef<Record<string, string>>({});
  const readyBySessionIdRef = useRef<Record<string, ReadyRuntime>>({});
  const viewSlotsByRuntimeKeyRef = useRef(new Map<RuntimeKey, SessionViewSlot>());
  const displayStateRef = useRef<SessionDisplayState | null>(null);
  const pendingSwitchResolversRef = useRef(new Map<number, (ok: boolean) => void>());
  const pendingSwitchPromisesRef = useRef(new Map<number, Promise<boolean>>());
  const authoritativeCutTimerRef = useRef<number | null>(null);
  const lastCutProgressAtRef = useRef(0);
  const refillInFlightRef = useRef<string | null>(null);
  const refillSlotGenerationRef = useRef(0);
  const snapshotInProgressRetryTimerRef = useRef<number | null>(null);
  const snapshotInProgressRetryGenerationRef = useRef<number | null>(null);
  const activatedRuntimeRef = useRef<RuntimeIdentity | null>(null);
  const liveViewportsRef = useRef(new Map<RuntimeKey, LiveTranscriptViewport>());
  const displayedRuntimeKeyRef = useRef<RuntimeKey | null>(null);
  const surfaceCommittedVersionRef = useRef(new Map<RuntimeKey, number>());
  // Retained mounted transcript surfaces keyed by exact runtime: inactive surfaces stay mounted hidden+inert+aria-hidden; an exact-ready switch flips only visibility.
  const surfaceKeysRef = useRef<RuntimeKey[]>([]);
  const [surfaceKeys, setSurfaceKeys] = useState<RuntimeKey[]>([]);
  const surfaceEpochRef = useRef(0);
  const [surfaceEpoch, setSurfaceEpoch] = useState(0);
  const surfaceContentElsRef = useRef(new Map<RuntimeKey, HTMLDivElement | null>());
  const projectedDomBySurfaceRef = useRef(new Map<RuntimeKey, number>());
  const itemIndexBySessionIdRef = useRef(new Map<string, Map<string, number>>());
  const initialStartBySurfaceRef = useRef(new Map<RuntimeKey, number>());
  const [displayedRuntimeKey, setDisplayedRuntimeKey] = useState<RuntimeKey | null>(null);
  const [switchPending, setSwitchPending] = useState(false);
  // A cleared pending switch stays in the marker until the ready gate passes (ready and pending-clear publish as one observable cut).
  const heldPendingRef = useRef<{ generation: number; phase: SessionSwitchPhase; targetSessionId: string } | null>(null);
  const [lifecycleView, setLifecycleView] = useState<SessionLifecycleView>(EMPTY_LIFECYCLE_VIEW);
  const ensureSurface = useCallback((identity: RuntimeIdentity) => {
    const key = runtimeKey(identity);
    if (surfaceKeysRef.current.includes(key)) return;
    const slot = viewSlotsByRuntimeKeyRef.current.get(key);
    const items = itemsBySessionIdRef.current[identity.sessionId] ?? [];
    let start = tailProjectionStart(items.length);
    if (slot && !slot.transcriptScroll.follow && slot.transcriptScroll.anchorItemId !== null) {
      const anchorIndex = itemIndexBySessionIdRef.current.get(identity.sessionId)?.get(slot.transcriptScroll.anchorItemId);
      if (anchorIndex !== undefined) start = projectionStartForAnchor(items.length, anchorIndex);
    }
    initialStartBySurfaceRef.current.set(key, start);
    surfaceKeysRef.current = [...surfaceKeysRef.current, key];
    setSurfaceKeys(surfaceKeysRef.current);
    surfaceEpochRef.current += 1;
    setSurfaceEpoch(surfaceEpochRef.current);
  }, []);
  const removeSurface = useCallback((key: RuntimeKey) => {
    if (!surfaceKeysRef.current.includes(key)) return;
    surfaceKeysRef.current = surfaceKeysRef.current.filter((candidate) => candidate !== key);
    setSurfaceKeys(surfaceKeysRef.current);
    surfaceCommittedVersionRef.current.delete(key);
    projectedDomBySurfaceRef.current.delete(key);
    liveViewportsRef.current.delete(key);
    surfaceContentElsRef.current.delete(key);
    initialStartBySurfaceRef.current.delete(key);
    surfaceEpochRef.current += 1;
    setSurfaceEpoch(surfaceEpochRef.current);
  }, []);
  const publishLifecycle = useCallback((state: SessionDisplayState | null) => {
    const displayed = state?.displayed ?? null;
    const ready = displayed ? readyBySessionIdRef.current[displayed.sessionId] : undefined;
    const displayedKey = displayedRuntimeKeyRef.current;
    const pending = state?.pending ?? null;
    const gate = lifecycleReadyGate({
      displayed,
      commandTarget: state?.commandTarget ?? null,
      pending,
      ready,
      socketGeneration: socketGenRef.current,
      displayedKey,
      committedItemsVersion: displayedKey !== null
        ? (surfaceCommittedVersionRef.current.get(displayedKey) ?? null)
        : null,
      currentItemsVersion: displayed ? (itemsVersionBySessionIdRef.current[displayed.sessionId] ?? 0) : 0,
    });
    let shownGeneration = pending?.generation ?? null;
    let shownPhase = pending?.phase ?? null;
    if (pending) {
      heldPendingRef.current = {
        generation: pending.generation,
        phase: pending.phase,
        targetSessionId: pending.targetIdentity?.sessionId ?? pending.targetRequest.catalogSessionId,
      };
    } else if (heldPendingRef.current) {
      const held = heldPendingRef.current;
      if (gate || displayed === null || displayed.sessionId !== held.targetSessionId) {
        heldPendingRef.current = null;
      } else {
        shownGeneration = held.generation;
        shownPhase = held.phase;
      }
    }
    const next: SessionLifecycleView = {
      displayedSessionId: displayed?.sessionId ?? '',
      commandSessionId: state?.commandTarget.sessionId ?? '',
      pendingGeneration: shownGeneration,
      pendingPhase: shownPhase,
      authoritativeReady: gate,
      stateMessages: displayed ? (itemsBySessionIdRef.current[displayed.sessionId]?.length ?? 0) : 0,
      projectedDom: displayedKey !== null ? (projectedDomBySurfaceRef.current.get(displayedKey) ?? 0) : 0,
    };
    setLifecycleView((prev) => (
      prev.displayedSessionId === next.displayedSessionId
      && prev.commandSessionId === next.commandSessionId
      && prev.pendingGeneration === next.pendingGeneration
      && prev.pendingPhase === next.pendingPhase
      && prev.authoritativeReady === next.authoritativeReady
      && prev.stateMessages === next.stateMessages
      && prev.projectedDom === next.projectedDom
        ? prev
        : next
    ));
  }, []);
  const loadedSessionIdsRef = useRef<Set<string>>(new Set());
  const streamWatchdogRef = useRef<{ arm: (sid: string) => void; clear: (sid: string) => void; probe: (sid: string) => void } | null>(null);
  const armStreamWatchdogRef = useRef<(sid: string) => void>(() => {});
  const markStreamingFor = useCallback((sid: string | null, on: boolean) => {
    if (!sid) return;
    if (shouldHoldOptimisticStop(abortPendingBySessionIdRef.current[sid] === true, on)) return;
    if (!on) {
      setAbortingBySessionId((prev) => dropSessionAborting(prev, sid));
      streamWatchdogRef.current?.clear(sid);
    }
    if (streamingBySessionIdRef.current[sid] === on) return;
    streamingBySessionIdRef.current = { ...streamingBySessionIdRef.current, [sid]: on };
    setStreamingBySessionId((prev) => ({ ...prev, [sid]: on }));
    if (on) armStreamWatchdogRef.current(sid);
  }, []);
  const [activityStripBySessionId, setActivityStripBySessionId] = useState<Record<string, ActivityStripState>>({});
  const activityStripBySessionIdRef = useRef<Record<string, ActivityStripState>>({});
  const [streamStallBySessionId, setStreamStallBySessionId] = useState<Record<string, boolean>>({});
  const noteStreamStall = useCallback((sid: string, event: Parameters<typeof reduceStreamStall>[1]) => {
    if (!sid) return;
    setStreamStallBySessionId((prev) => {
      const next = reduceStreamStall(prev[sid] === true, event);
      if (next === (prev[sid] === true)) return prev;
      if (!next) {
        const copy = { ...prev };
        delete copy[sid];
        return copy;
      }
      return { ...prev, [sid]: true };
    });
  }, []);
  const noteStreamingSocketDrop = useCallback(() => {
    for (const [sid, on] of Object.entries(streamingBySessionIdRef.current)) {
      if (on === true) noteStreamStall(sid, { type: 'socket_drop', streaming: true });
    }
  }, [noteStreamStall]);
  const updateActivityStripFor = useCallback((sid: string | null, update: (state: ActivityStripState) => ActivityStripState) => {
    if (!sid) return;
    const previous = activityStripBySessionIdRef.current[sid] ?? emptyActivityStripState(sid);
    const next = update(previous);
    if (next === previous) return;
    activityStripBySessionIdRef.current = { ...activityStripBySessionIdRef.current, [sid]: next };
    setActivityStripBySessionId((current) => ({ ...current, [sid]: next }));
  }, []);
  const [compactBook, setCompactBook] = useState<CompactBook>({ compacting: {}, mode: {}, token: {}, settled: {} });
  const compactBookRef = useRef<CompactBook>({ compacting: {}, mode: {}, token: {}, settled: {} });
  const writeCompactBook = useCallback((next: CompactBook) => {
    compactBookRef.current = next;
    setCompactBook(next);
  }, []);

  const [unreadBySessionId, setUnreadBySessionId] = useState<Record<string, number>>({});
  // Per-session shell/feature state (Main contract: model, thinking, Todo,
  // Goal, Side chat are session-scoped and must never leak across sessions).
  // The ACTIVE session's slots drive the header + panels; events for
  // background sessions update only the owning slot's cache/unread.
  const [sessionCwdBySessionId, setSessionCwdBySessionId] = useState<Record<string, string>>({});
  const [sessionModelState, setSessionModelState] = useState<SessionModelState>(EMPTY_SESSION_MODEL_STATE);
  const sessionModelStateRef = useRef<SessionModelState>(EMPTY_SESSION_MODEL_STATE);
  const writeSessionModelState = useCallback((next: SessionModelState) => {
    sessionModelStateRef.current = next;
    setSessionModelState(next);
  }, []);
  const [thinkingLevelState, setThinkingLevelState] = useState<ThinkingLevelState>(EMPTY_THINKING_LEVEL_STATE);
  const thinkingLevelStateRef = useRef<ThinkingLevelState>(EMPTY_THINKING_LEVEL_STATE);
  const writeThinkingLevelState = useCallback((next: ThinkingLevelState) => {
    thinkingLevelStateRef.current = next;
    setThinkingLevelState(next);
  }, []);
  // Composer context-window meter (get_session_stats -> SessionStats
  // .contextUsage). Session-scoped like the slots above: a background
  // session's refresh never leaks into another session's meter.
  const [contextUsageBySessionId, setContextUsageBySessionId] = useState<Record<string, ContextUsageWire | null>>({});
  const contextUsageSeqBySessionIdRef = useRef<Record<string, number>>({});
  // Header option catalogs (model rows + thinking levels) for the header
  // selects. Session-scoped: refetched whenever the displayed session or its
  // model changes (see the refresh effect after sendCommand).
  const [headerCatalogs, setHeaderCatalogs] = useState<HeaderCatalogs>(EMPTY_HEADER_CATALOGS);
  const [token, setToken] = useState('');
  const [toasts, setToasts] = useState<AppToast[]>([]);
  // Todo has exactly one top-level panel value (`todo`). Ordinary DAG tasks
  // and durable child sessions are simultaneous sections of that page.
  const [activePanel, setActivePanel] = useState<string>('');
  // File explorer drawer is independent of activePanel and keyed by sessionId
  // so collapse/close/reopen and A→B switches rebind without dropping tabs.
  const [fileExplorerBySessionId, setFileExplorerBySessionId] = useState<Record<string, FileExplorerSessionSlot>>({});
  // Last COMMITTED slot map, written only inside the close-diff effect below
  // (never render-assigned): the effect diffs it against the newly committed
  // map to derive removed tab identities, keeping the state updater pure.
  const fileExplorerCommittedSlotsRef = useRef<Record<string, FileExplorerSessionSlot>>({});

  const patchFileExplorer = useCallback((sid: string, updater: (slot: FileExplorerSessionSlot) => FileExplorerSessionSlot) => {
    setFileExplorerBySessionId((prev) => updateFileExplorerSlot(prev, sid, updater).slots);
  }, []);

  // Pure exact-key removal for a VALIDATED session deletion: an ordinary
  // updater patch would retain an emptied slot for an existing key and
  // re-trigger the committed-diff effect forever; deleting the key lets the
  // diff effect below derive the victim's conversation closes exactly once.
  const deleteFileExplorerSlot = useCallback((sid: string): void => {
    if (!sid) return;
    setFileExplorerBySessionId((prev) => {
      if (!(sid in prev)) return prev;
      const next = { ...prev };
      delete next[sid];
      return next;
    });
  }, []);

  const openFileInDrawer = useCallback((path: string, line?: number, ownerSessionId?: string, view: FileExplorerTab['view'] = 'file') => {
    const sid = ownerSessionId?.trim();
    if (!sid || !path.trim()) return;
    patchFileExplorer(sid, (slot) => applyOpenPath(slot, path, line, view));
  }, [patchFileExplorer]);


  const toggleFileExplorer = useCallback(() => {
    const sid = sessionIdRef.current;
    if (!sid) return;
    let opening = false;
    patchFileExplorer(sid, (slot) => {
      const next = slot.mode === 'closed' ? 'open' : 'closed';
      opening = next === 'open';
      return applyDrawerMode(slot, next);
    });
    if (opening) setActivePanel('');
  }, [patchFileExplorer]);



  // Revisions captured when `/code-review [<base> | <from> <to>]` opens the
  // panel. Cleared on close/session reset so a later bare open does not reuse
  // stale revs.
  const [codeReviewOpenArgs, setCodeReviewOpenArgs] = useState<CodeReviewOpenArgs>({});
  const activePanelRef = useRef(activePanel);
  useEffect(() => {
    activePanelRef.current = activePanel;
  }, [activePanel]);

  const [subagentsInitialPage, setSubagentsInitialPage] = useState<'jobs' | 'irc'>('jobs');
  // Monotonic authoritative-ready epoch. Mounted panels use it to reconcile
  // server-owned state after a transport generation finishes bootstrap.
  const [connectionEpoch, setConnectionEpoch] = useState(0);
  // Persistent session sidebar (left rail) + mobile drawer toggle.
  const [sessionId, setSessionId] = useState<string | null>(null);
  // Monotonic counter bumped exactly once per successful new/fork/clone
  // lifecycle cut (after activation, preference save, and subscription).
  // Every change asks the Sessions sidebar for one silent authoritative
  // catalog refresh; failures never bump and never roll the activation back.
  const [catalogRefreshRevision, setCatalogRefreshRevision] = useState(0);
  // ONE bounded lifecycle row overlay: the backend-validated authoritative
  // `session` row from the MOST RECENT successful new/fork/clone cut, keyed
  // by its exact `source::sessionId::path` identity. App owns this single
  // row and hands it to the Sessions sidebar, which merges it synchronously
  // into the rendered catalog BEFORE any `session_list` — so the active
  // child row paints in the same lifecycle cut instead of waiting for the
  // catalog refresh, and a failed or delayed catalog retains it. The sidebar
  // clears the overlay (absorbs it) only when a catalog response contains
  // the exact row; no row is ever guessed client-side — only a
  // parseLifecycleResult-validated backend row lands here.
  const [lifecycleRow, setLifecycleRow] = useState<SessionRowWire | null>(null);
  const clearLifecycleRow = useCallback(() => setLifecycleRow(null), []);
  /** Copy-context seam: the LATEST compaction summary text from a loaded
   *  session's transcript items ({kind:'summary', label:'Compaction summary'}
   *  — transcript.ts), or null when the session has none / is not loaded.
   *  Unloaded rows fall back to the handoff brief in the sidebar. */
  const getCompactionSummary = useCallback((sid: string): string | null =>
    latestCompactionSummary(itemsBySessionId[sid] ?? []), [itemsBySessionId]);
  /** Copy-context load gate: a session is LOADED only when a runtime is
   *  registered for it (resident mirror). Unloaded historical rows have no
   *  registered runtime — the on-demand compact RPC would reject with
   *  'unknown session' at the admission resolver — so Copy context takes the
   *  path-based session_copy_context fallback for them instead. */
  const isSessionLoaded = useCallback((sid: string): boolean =>
    sid !== '' && residentBySessionIdRef.current[sid] !== undefined, []);
  /** DEFECT A (cwd split-brain): the sidebar's validated session_move cutover
   *  lands the destination cwd in App's mirror IMMEDIATELY — before any
   *  render/refetch — so markdown path probes (which read workspaceCwd from
   *  this mirror) never compute a relative path against a stale cwd. */
  const onSessionCwdChanged = useCallback((sid: string, cwd: string) => {
    if (!sid || !cwd) return;
    setSessionCwdBySessionId((prev) => (prev[sid] === cwd ? prev : { ...prev, [sid]: cwd }));
  }, []);
  // Mirrors `sessionId` so callbacks can read the active session synchronously
  // (the React state lags one render). Same-id refreshes (reconnect, settings
  // apply, rename) keep the active session; a different non-empty id is a
  // session cutover that switches the ACTIVE view (per-session caches below).
  const sessionIdRef = useRef<string | null>(null);
  const fileExplorerSlot = readFileExplorerSlot(fileExplorerBySessionId, sessionId);
  const activeSessionCwd = sessionId ? sessionCwdBySessionId[sessionId] : undefined;
  const fileExplorerOpen = fileExplorerSlot.mode !== 'closed';
  // Full-app expand resets the moment the drawer closes (the panel unmounts).
  useEffect(() => {
    if (!fileExplorerOpen) setFileExplorerExpanded(false);
  }, [fileExplorerOpen]);
  const [fileExplorerWidth, setFileExplorerWidth] = useState(() =>
    typeof window !== 'undefined' ? readStoredFileExplorerWidth(window.innerWidth) : FILE_EXPLORER_WIDTH_DEFAULT
  );
  const fileExplorerWidthRef = useRef(fileExplorerWidth);
  fileExplorerWidthRef.current = fileExplorerWidth;
  // One visibility state drives BOTH the mobile drawer and the desktop
  // collapsible rail: open by default on desktop, closed on mobile.
  const [sidebarOpen, setSidebarOpen] = useState(() =>
    typeof window !== 'undefined' && window.matchMedia('(min-width: 721px)').matches
  );
  // The BREAKPOINT owns the visibility default on every crossing: moving to
  // ≤720px collapses an inherited-open desktop rail into the closed mobile
  // drawer (the documented mobile default — without this sync the first
  // toggle press on mobile would CLOSE the unrequested open drawer instead
  // of opening it), while moving back to ≥721px never force-opens the rail —
  // sidebarOpen=false there is the supported collapsed-rail state and
  // forcing it would clobber the user's collapse choice. Dedicated stable
  // listener: mq 'change' fires on crossings only, so boot behavior and
  // explicit toggles are untouched.
  useEffect(() => {
    const mq = window.matchMedia('(max-width: 720px)');
    const collapse = (event: MediaQueryListEvent) => {
      if (event.matches) setSidebarOpen(false);
    };
    mq.addEventListener('change', collapse);
    return () => mq.removeEventListener('change', collapse);
  }, []);
  const fileExplorerResizeDragRef = useRef<{ startX: number; startWidth: number } | null>(null);
  const reconnectReviewOwnerRef = useRef<string | null>(null);
  const liveSettingsBySessionIdRef = useRef<Record<string, LiveSettingsWire | null>>({});
  // Sidebar consumed by the same row as drawer + conversation (230px open /
  // 44px collapsed rail). Live value: the resizer/keyboard handlers and the
  // resize-reclamp read it so the ceiling tracks the rail the user sees.
  const fileExplorerSidebarPx = sidebarOpen ? FILE_EXPLORER_SIDEBAR_OPEN : FILE_EXPLORER_SIDEBAR_COLLAPSED;
  const [viewportWidth, setViewportWidth] = useState(() =>
    typeof window !== 'undefined' ? window.innerWidth : 1280
  );
  // Overlay flip: the viewport cannot fit sidebar + conversation minimum +
  // the 360px drawer floor (see fileExplorerOverlayActive). Adds a layout
  // class so styles.css can take the drawer over the full viewport width,
  // re-home the close affordance, and drop the desktop reservation; the
  // stored width is meaningless there. Viewport width is React state so a
  // Playwright/user resize recomputes the class instead of keeping a stale
  // full-width overlay from a previous 390px capture.
  const fileExplorerOverlay =
    fileExplorerOpen
    && fileExplorerOverlayActive(viewportWidth, fileExplorerSidebarPx);
  // One width authority: --file-explorer-width on <html>; reservation (in
  // styles.css) and drawer read the same var. Layout effect so the restored
  // width lands before first paint (no drawer jump on reload).
  useLayoutEffect(() => {
    if (typeof window === 'undefined') return;
    applyFileExplorerWidth(fileExplorerWidthRef.current, window.innerWidth, fileExplorerSidebarPx);
  }, [fileExplorerWidth, fileExplorerSidebarPx]);

  // Reclamp the desktop drawer when the viewport shrinks so React width and
  // aria-valuenow never exceed the live ceiling (CSS already clamps via
  // calc(100vw - 576px)). Overlay ≤720px ignores the
  // stored width. The same listener publishes viewportWidth so overlay
  // class tracks the live innerWidth.
  useLayoutEffect(() => {
    if (typeof window === 'undefined') return;
    const reclamp = () => {
      const viewportWidthNow = window.innerWidth;
      setViewportWidth(viewportWidthNow);
      if (window.matchMedia('(max-width: 720px)').matches) return;
      const next = clampFileExplorerWidth(fileExplorerWidthRef.current, viewportWidthNow, sidebarOpen ? FILE_EXPLORER_SIDEBAR_OPEN : FILE_EXPLORER_SIDEBAR_COLLAPSED);
      const changed = next !== fileExplorerWidthRef.current;
      fileExplorerWidthRef.current = next;
      setFileExplorerWidth(next);
      applyFileExplorerWidth(next, viewportWidthNow, fileExplorerSidebarPx);
      if (changed) writeStoredFileExplorerWidth(next, viewportWidthNow, fileExplorerSidebarPx);
    };
    reclamp();
    window.addEventListener('resize', reclamp);
    const mq = window.matchMedia('(max-width: 720px)');
    mq.addEventListener('change', reclamp);
    return () => {
      window.removeEventListener('resize', reclamp);
      mq.removeEventListener('change', reclamp);
    };
  }, [fileExplorerSidebarPx, sidebarOpen]);
  // Open/toggle a feature panel. setActivePanel stays a pure updater. Closing
  // Files is a separate write so Settings/Todo/etc. are not covered by the
  // explorer. Desktop keeps the session rail open.
  const openPanel = useCallback((name: string, opts?: { force?: boolean }) => {
    setActivePanel((current) => (!opts?.force && current === name ? '' : name));
    const sid = sessionIdRef.current;
    if (sid) patchFileExplorer(sid, (slot) => applyDrawerMode(slot, 'closed'));
  }, [patchFileExplorer]);

  // Any non-empty activePanel (feature-nav toggle, Manage session, /code-review
  // slash command) collapses the mobile drawer at <=720px before paint; the
  // drawer-close is keyed off state here instead of firing from inside
  // openPanel's updater. Desktop (>=721px) keeps the rail open.
  // File explorer FULL-APP expand (user requirement): the same-row ⛶ button
  // toggles an app-layout class that hides the sidebar + transcript +
  // composer so the file tree + content viewports fill the entire app area.
  // Resets when the drawer closes (the open-branch unmount owns the panel).
  const [fileExplorerExpanded, setFileExplorerExpanded] = useState(false);
  useLayoutEffect(() => {
    if (activePanel !== '' && window.matchMedia('(max-width: 720px)').matches) {
      setSidebarOpen(false);
    }
  }, [activePanel]);

  // ONE top authority: the measured height is rounded to an integer and
  // written as --app-header-height (applyHeaderHeight); the header, the
  // content area, and the file-explorer drawer all read that same var, so a
  // seam between them is impossible by construction.
  // The query is scoped to App's OWN chrome header via the ref bound to the
  // <header> below: a bare document-wide <header> tag query is order-
  // dependent and order-fragile across re-mounts.
  const appHeaderRef = useRef<HTMLElement | null>(null);
  const appFooterRef = useRef<HTMLElement | null>(null);
  useLayoutEffect(() => {
    const footer = appFooterRef.current;
    if (!footer || typeof ResizeObserver === 'undefined') return;
    const apply = () => {
      applyFooterHeight(measureChromeHeight(footer));
    };
    apply();
    const ro = new ResizeObserver(apply);
    ro.observe(footer);
    window.addEventListener('resize', apply);
    return () => {
      ro.disconnect();
      window.removeEventListener('resize', apply);
    };
  }, []);
  const filesFullscreen = fileExplorerFullscreenActive(fileExplorerOpen, fileExplorerExpanded);
  const headerHiddenRef = useRef(filesFullscreen);
  headerHiddenRef.current = filesFullscreen;
  useLayoutEffect(() => {
    const header = appHeaderRef.current;
    if (!header || typeof ResizeObserver === 'undefined') return;
    const apply = () => {
      applyHeaderHeight(measureChromeHeight(header), headerHiddenRef.current);
    };
    apply();
    const ro = new ResizeObserver(apply);
    ro.observe(header);
    window.addEventListener('resize', apply);
    return () => {
      ro.disconnect();
      window.removeEventListener('resize', apply);
    };
  }, []);
  useLayoutEffect(() => {
    const header = appHeaderRef.current;
    if (!header) return;
    applyHeaderHeight(measureChromeHeight(header), filesFullscreen);
  }, [filesFullscreen]);


  // Shared desktop height for ordinary bottom drawers (vh). Applied as
  // --panel-drawer-height on <html>; one resizer serves every ordinary panel.
  const [panelDrawerVh, setPanelDrawerVh] = useState(() =>
    typeof window !== 'undefined' ? readStoredPanelDrawerVh() : PANEL_DRAWER_DEFAULT_VH
  );
  const panelDrawerVhRef = useRef(panelDrawerVh);
  panelDrawerVhRef.current = panelDrawerVh;
  const panelResizeDragRef = useRef<{ startY: number; startVh: number } | null>(null);
  // Ordinary Todo and Activity are independent session-keyed stores. Their
  // navigation keys remain distinct from Workflow and autonomous Todo child.
  const [todoBySessionId, setTodoBySessionId] = useState<Record<string, TodoSessionSnapshot>>({});
  const [todoNavigationByKey, setTodoNavigationByKey] = useState<Record<string, { selectedTaskId: string | null }>>({});
  const [activityBySessionId, setActivityBySessionId] = useState<Record<string, ActivitySessionSnapshot>>({});
  const [activityNavigationByKey, setActivityNavigationByKey] = useState<Record<string, { page: 'recent' | 'settled' }>>({});
  // Goal panel — current goal snapshot + journal replay (live via
  // goal_updated / goal_usage_charged events, refreshed via goal_get /
  // goal_journal). Keyed by session: every refresh/event carries the owning
  // sessionId, so a background session's goal can never mutate the active
  // panel and an async refreshGoal() can never apply to the wrong session.
  const [goalStateBySessionId, setGoalStateBySessionId] = useState<Record<string, GoalStateWire | null>>({});
  const [goalJournalBySessionId, setGoalJournalBySessionId] = useState<Record<string, GoalEventWire[]>>({});
  // Side chat (parallel /btw sessions) snapshot; polled while the
  // 'sidechat' panel is open, keyed by owning session.
  const [sideChatBySessionId, setSideChatBySessionId] = useState<Record<string, SideChatSnapshot | null>>({});
  const sideChatBySessionIdRef = useRef(sideChatBySessionId);
  sideChatBySessionIdRef.current = sideChatBySessionId;

  const activeStreaming = sessionId ? !!streamingBySessionId[sessionId] : false;
  const activeAborting = sessionId ? abortingBySessionId[sessionId] === true : false;
  const stopArmed = activeStreaming || activeAborting;
  const activeSteering = sessionId ? (steeringBySessionId[sessionId] ?? []) : [];
  const activeFailedSteering = sessionId ? (failedSteeringBySessionId[sessionId] ?? []) : [];
  const activeCompacting = sessionId ? compactBook.compacting[sessionId] === true : false;
  const activeActivityStrip = sessionId ? activityStripBySessionId[sessionId] : undefined;
  const activeStreamStall = sessionId ? streamStallBySessionId[sessionId] === true : false;
  const activeActivityStripView = activityStripView(activeActivityStrip, connState, activeStreamStall);
  const activeCompactMode: CompactMode | null = sessionId ? (compactBook.mode[sessionId] ?? null) : null;

  const models = headerCatalogs.models;
  const levels = headerCatalogs.levels;
  const activeModelKey = sessionId ? (sessionModelView(sessionModelState)[sessionId] ?? '') : '';
  const modelGroups = useMemo(
    () => keepSelectedModelOption(groupModelsByProvider(models), activeModelKey),
    [activeModelKey, models],
  );
  const activeThinkingLevel = sessionId ? (thinkingLevelView(thinkingLevelState)[sessionId] ?? '') : '';
  const activeContextMeter = formatContextMeter(contextUsageBySessionId[sessionId ?? ''] ?? null);
  const activeContextMeterMobile = contextMeterMobile(contextUsageBySessionId[sessionId ?? ''] ?? null);
  const activeContextMeterAccessible = contextMeterAccessible(contextUsageBySessionId[sessionId ?? ''] ?? null);
  const activeTodoSnapshot: TodoSessionSnapshot = sessionId
    ? (todoBySessionId[sessionId] ?? { sessionId, phases: [], sequence: 0, executionStatus: 'dormant', failureReason: null })
    : { sessionId: '', phases: [], sequence: 0, executionStatus: 'dormant', failureReason: null };
  const activeTodoNavigationKey = todoNavigationKey(sessionId ?? '');
  const activeTodoNavigation = todoNavigationByKey[activeTodoNavigationKey] ?? { selectedTaskId: null };
  const activeActivitySnapshot: ActivitySessionSnapshot = sessionId
    ? (activityBySessionId[sessionId] ?? { sessionId, entries: [], sequence: 0, actors: {} })
    : { sessionId: '', entries: [], sequence: 0, actors: {} };
  const activeActivityNavigationKey = activityNavigationKey(sessionId ?? '');
  const activeActivityNavigation = activityNavigationByKey[activeActivityNavigationKey] ?? { page: 'recent' };
  const activeGoalState = sessionId ? (goalStateBySessionId[sessionId] ?? null) : null;
  const activeGoalJournal = sessionId ? (goalJournalBySessionId[sessionId] ?? []) : [];
  const activeSideChat = sessionId ? (sideChatBySessionId[sessionId] ?? null) : null;

  // Composer file attachments: images -> prompt `images` ContentBlocks; UTF-8
  // text/code files -> filename + fenced-code blocks inside the prompt
  // `message`. Classification/limits/order are enforced in ./attachments.
  const [attachments, setAttachments] = useState<ComposerAttachment[]>([]);
  const attachmentsRef = useRef<ComposerAttachment[]>([]);
  const updateAttachments = useCallback((update: (current: ComposerAttachment[]) => ComposerAttachment[]) => {
    setAttachments((current) => {
      const next = update(current);
      attachmentsRef.current = next;
      return next;
    });
  }, []);
  attachmentsRef.current = attachments;
  // Drag/drop drop-target highlight. A depth counter (dragDepthRef) avoids the
  // flicker from dragenter/dragleave firing on child elements of the footer.
  const [dropActive, setDropActive] = useState(false);
  // Live/STT settings surfaced by get_state (`runtimeSettings.live`) when the
  // backend exposes them; null means "not advertised" (mic still shown).
  const [liveSettings, setLiveSettings] = useState<LiveSettingsWire | null>(null);

  const wsRef = useRef<WebSocket | null>(null);
  const subscribeRuntime = useCallback((identity: RuntimeIdentity, cursor: number) => {
    ensureSurface(identity);
    const ws = wsRef.current;
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    ws.send(JSON.stringify(buildSubscribeFrame(identity, cursor)));
  }, [ensureSurface]);
  // Monotonic socket generation: connect() bumps it; every pending command
  // and every onOpen bootstrap continuation stamps/captures it so a socket
  // superseded mid-bootstrap (A dropped, B reconnected healthy) can never
  // applyState / setConnState / refreshGoal / scheduleReconnect on B's behalf
  // — its pending is drained on replace and its continuation bails via
  // STALE_ABORT / `!alive()`. See ./socket.ts for the pure invariant.
  const socketGenRef = useRef(0);
  // Bounded ready gate for auto/background loads (sidebar + session panel
  // `load()` on mount). On a fresh page the sidebar effect runs while
  // `connect()` is still in CONNECTING, so a direct `sendCommand` would reject
  // with `not connected` and surface a persistent `load failed: not
  // connected`. The gate lets those auto-loads WAIT for the current socket's
  // open (bounded by READY_GATE_TIMEOUT_MS) instead of failing immediately;
  // `onOpen` resolves every pending waiter, and unmount clears them. Active
  // actions (New / Switch / rename) bypass the gate and still fail fast. See
  // ./socket.ts ReadyGate for the generation-safety + bounded-wait contract.
  const readyGateRef = useRef<ReadyGate | null>(null);
  // Fresh boot starts at the page authority. Tokens and session preferences
  // remain scoped to whichever authority is active for this page lifetime.
  const hostRef = useRef(normalizeHostAuthority(
    typeof window !== 'undefined' && window.location ? window.location.host : '',
  ));
  const tokenRef = useRef(token);
  // Pending-command registry (lazily created after removeItem is defined, so
  // its timeout hook can remove optimistic bubbles); see ./pending.ts.
  const pendingRegistryRef = useRef<PendingRegistry | null>(null);
  const seqRef = useRef(0);
  // Stable per-page client namespace: sendCommand ids are `c<ns>-<seq>` so a
  // reload cannot reuse `c1`/`c2` against the resident manager ledger.
  const pageNamespaceRef = useRef(createPageClientNamespace());
  const delayRef = useRef(RECONNECT_INITIAL_DELAY_MS);
  const retryTimerRef = useRef<number | null>(null);
  // Heartbeat bookkeeping: lastMessageAtRef slides the 60s silence window on
  // every inbound frame. pingSentAtRef is the outstanding application ping;
  // only `{type:"pong"}` clears it. pingTimerRef drives the cadence;
  // pongTimerRef is the missed-pong deadline. All live only while a socket
  // is open (armed in onOpen, cleared on close/replace/unload), plus the
  // stability timer that resets backoff after the connection stays alive >5s.
  const lastMessageAtRef = useRef(0);
  const pingSentAtRef = useRef<number | null>(null);
  const pingTimerRef = useRef<number | null>(null);
  const pongTimerRef = useRef<number | null>(null);
  const silenceTimerRef = useRef<number | null>(null);
  const handshakeTimerRef = useRef<number | null>(null);
  const handshakeStartedAtRef = useRef<number | null>(null);
  const stabilityTimerRef = useRef<number | null>(null);
  // Per-session streaming assistant id + optimistic bubble queue: switching
  // sessions swaps the bucket, so each session's in-flight state is isolated.
  const activeAssistantBySessionIdRef = useRef<Record<string, string>>({});
  const abortGestureClickRef = useRef(false);
  // Model and thinking mutations are ordered per session. Auth resolution is
  // asynchronous, so two rapid selections must finish in user-request order;
  // different sessions retain independent queues.
  const modelSettingsQueueRef = useRef(new SessionModelMutationQueue());
  // True once any socket has reached OPEN: distinguishes the initial boot
  // probe/refusal (never opened) from a MID-SESSION abnormal drop (1006), so
  // the close surface can show an accurate cause instead of the token hint.
  const everConnectedRef = useRef(false);
  // `token_required` boot hold: set when the bounded /status preflight proves
  // the listener is token-gated and no token is stored. While held, the
  // reconnect timer is never armed (no silent 401 retry loop); a non-empty
  // token save releases the hold and reconnects exactly once.
  const tokenHoldRef = useRef(false);
  // Boot-preflight generation: bumped by token commits and unmount so an
  // in-flight /status probe can never open a superseded second connection.
  const preflightGenRef = useRef(0);
  /** Handoff seam for scheduleReconnect's re-probes: the limbo guards call
   *  bootWithPreflight through this ref because its callback identity
   *  depends on connect(), which depends on scheduleReconnect — a dependency
   *  cycle refs cannot express. Assigned during every render. */
  const bootProbeHandoffRef = useRef<() => void>(() => {});
  const wakeProbeHandoffRef = useRef<() => void>(() => {});
  const connectRef = useRef<() => void>(() => {});
  const tearTransportRef = useRef<(reason: string) => void>(() => {});
  const transcriptRef = useRef<HTMLDivElement | null>(null);
  const transcriptContentRef = useRef<HTMLDivElement | null>(null);
  const transportOutageActiveRef = useRef(false);
  const optimisticQueueBySessionIdRef = useRef<Record<string, string[]>>({});
  /** Outstanding steer submits per session: a stale settle get_state built
   *  before the submit reaches the server must never wipe the optimistic
   *  tray row (the vanish-before-chat bug). */
  const steeringSubmitInFlightBySessionRef = useRef<Record<string, number>>({});
  const composerSendingRef = useRef(false);
  /** Consecutive reconnect attempts whose socket closed before onopen — the
   *  never-opened counter that triggers the /status re-probe (limbo: a
   *  listener restart can flip a tokenless-accepted endpoint into
   *  token-required mode, looping silent refused handshakes forever). */
  const neverOpenedFailuresRef = useRef(0);
  /** Bounded hold re-probe timer: while held in token_required the page
   *  re-probes /status so a listener restart into tokenless mode releases
   *  the hold (a hold that never re-probes pins the page in Settings). */
  const holdReprobeTimerRef = useRef<number | null>(null);
  const pinCtrlRef = useRef<ScrollPinController | null>(null);
  if (pinCtrlRef.current === null) pinCtrlRef.current = createScrollPin();
  const pinCtrl = pinCtrlRef.current;
  const onTranscriptScroll = useCallback((event: UIEvent<HTMLDivElement>) => {
    pinCtrl.onScroll(event.currentTarget);
  }, [pinCtrl]);
  const pinIfPinned = useCallback(() => {
    pinCtrl.followIfPinned(transcriptRef.current);
  }, [pinCtrl]);
  const forcePin = useCallback(() => {
    pinCtrl.forcePin(transcriptRef.current);
  }, [pinCtrl]);
  const promptInputRef = useRef<HTMLTextAreaElement>(null);
  const sideChatPromptInputRef = useRef<HTMLTextAreaElement>(null);
  const sideChatInsertTranscriptRef = useRef<SideChatInsertTranscript | null>(null);
  const sideChatSwitchSeqRef = useRef(0);
  // Some browsers synthesize Enter keydowns for each newline in a clipboard
  // paste. Arm this window on Ctrl/Cmd+V and onPaste so those Enters cannot
  // submit the draft (or flood prompt RPCs) while the paste is landing.
  const pasteEnterGuardUntilRef = useRef(0);
  const composerComposingRef = useRef(false);
  const composerEnterHandledRef = useRef(false);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const dragDepthRef = useRef(0);
  // Authoritative concurrent attachment budget (queued exact + every
  // outstanding intake reservation). See onFilesChosen's CONCURRENCY
  // INVARIANT. The queued exact is recomputed from `attachments` inside every
  // setAttachments updater; each in-flight intake reserves and later releases
  // ONLY its own key, so overlapping intakes never drop each other's
  // reservations (which would let a third intake bypass the front-end caps).
  const intakeBudgetTrackerRef = useRef(new IntakeBudgetTracker());
  // Video ids pending in the currently painted composer projection. Projection
  // resets clear these ids, while pendingVideoOwnersRef separately retains the
  // immutable upload authentication needed for a terminal old-token settle.
  const pendingUploadIdsRef = useRef(new Set<string>());
  // Immutable upload ownership survives composer projection resets. The active
  // pending-id set is UI state; this map is authentication retained only until
  // the upload settles (or the user removes the attachment).
  const pendingVideoOwnersRef = useRef(new Map<string, {
    owner: RuntimeIdentity;
    generation: number;
    attachment: ComposerAttachment;
  }>());
  // Intake generation: bumped on every full composer reset. Async completion
  // captures the exact runtime identity + generation at selection time and
  // writes that projection or its saved slot; a genuine reset/invalidation
  // makes the completion stale instead of migrating it to another composer.
  const intakeGenRef = useRef(0);
  // Commit attachment batches in selection order. A later fast intake waits
  // for every earlier intake's commit, so global chip and wire order matches
  // user selection order even when reads finish out of order.
  const intakeCommitQueueRef = useRef(Promise.resolve());
  const restoreViewRafRef = useRef<number | null>(null);
  const restoreViewGenerationRef = useRef(0);
  const mediaStreamRef = useRef<MediaStream | null>(null);
  // Codex Live realtime voice (WebRTC): active while the call is up; the
  // `oai-events` data channel delivers transcript/delegation events for the
  // overlay. The transcript text is accumulated in a ref and written straight
  // into the overlay node (no React re-render per delta — same hot-path
  // decision as the streaming transcript); realtimeDelegation is a rare state
  // change.
  const [realtimeActive, setRealtimeActive] = useState(false);
  const [realtimeDelegation, setRealtimeDelegation] = useState<string | null>(null);
  // Realtime WebRTC connection-state bucket (RTCPeerConnectionState via
  // classifyRealtimeConnectionState). Surfaced in the overlay so the user can
  // SEE a stuck/failed connection instead of staring at a silent mic button;
  // 'disconnected' is recoverable (toast, no teardown) while 'failed' is
  // terminal (toast + teardown).
  const [realtimeConnState, setRealtimeConnState] = useState<string | null>(null);
  const [, setRealtimeAudioBlocked] = useState(false);
  const realtimePcRef = useRef<RTCPeerConnection | null>(null);
  // The `oai-events` RTCDataChannel on the realtime peer connection carries
  // session.update + server events (transcript/delegation/error). It rides
  // the WebRTC DTLS transport (no browser-side Bearer header, no mixed-content
  // block) — the direct sideband WebSocket was removed for those reasons.
  const realtimeDcRef = useRef<RTCDataChannel | null>(null);
  // Synchronous start-guard: getUserMedia/offer/answer is async, so a quick
  // double-click could race two call setups before realtimeActive re-renders.
  const realtimeBusyRef = useRef(false);
  // Monotonic setup generation. Session cutover/stop invalidates the active
  // attempt so late getUserMedia/create-call continuations cannot resurrect A
  // in B's UI.
  const realtimeAttemptRef = useRef(0);
  // Session that owned the in-flight realtime call. Captured at start so
  // create/stop/RPC cleanup always target that owner, never the post-switch
  // current session.
  const realtimeOwnerRef = useRef<string | null>(null);
  const stopRealtimeRef = useRef<() => void>(() => {});
  const realtimeAudioRef = useRef<HTMLAudioElement | null>(null);
  const realtimeTranscriptRef = useRef('');
  // Last finalized USER input transcript committed to the composer, used to
  // dedup the two V1 final-event variants (input_transcript.done +
  // input_audio_transcription.completed) for one utterance. Reset per call.
  const lastCommittedTranscriptRef = useRef('');
  const realtimeTranscriptNodeRef = useRef<HTMLDivElement | null>(null);
  // Suppresses RPC error toasts during the reconnection bootstrap sequence
  // (get_state rebind) so a phone waking from sleep doesn't spam "command
  // get_state failed" before the rebind-to-primary lands.
  const bootRef = useRef(false);
  // Orchestration event handlers registered by the Subagents panel. A Set
  // keeps the panel subscription additive alongside other live panels.
  const subagentsHandlersRef = useRef(new Set<(frame: EventFrame) => void>());
  const workflowHandlersRef = useRef(new Set<(frame: WorkflowEventPayload) => void>());
  const onEventRef = useRef<(frame: EventFrame) => void>(() => {});

  const subscribeWorkflowEvents = useCallback((handler: (frame: WorkflowEventPayload) => void) => {
    workflowHandlersRef.current.add(handler);
    return () => {
      workflowHandlersRef.current.delete(handler);
    };
  }, []);

  const subscribeSubagentsEvents = useCallback((handler: (frame: EventFrame) => void) => {
    subagentsHandlersRef.current.add(handler);
    return () => {
      subagentsHandlersRef.current.delete(handler);
    };
  }, []);

  const toast = useCallback((
    message: string,
    error = false,
    opts?: { action?: { label: string; onAction: () => void }; timeoutMs?: number | null; kind?: AppToast['kind'] },
  ) => {
    const id = nextId('t');
    setToasts((prev) => {
      const retained = opts?.kind === 'transport'
        ? prev.filter((entry) => entry.kind !== 'transport')
        : prev;
      return [...retained.slice(-4), {
        id,
        message: safeText(message),
        error,
        action: opts?.action,
        kind: opts?.kind,
      }];
    });
    if (opts?.timeoutMs !== null) {
      window.setTimeout(() => {
        setToasts((prev) => prev.filter((t) => t.id !== id));
      }, opts?.timeoutMs ?? 7000);
    }
  }, []);

  const toastTransportOutage = useCallback((message: string): boolean => {
    const plan = planTransportOutageToast({
      everConnected: everConnectedRef.current,
      outageActive: transportOutageActiveRef.current,
      message,
    });
    if (plan.kind === 'skip') return false;
    transportOutageActiveRef.current = true;
    toast(plan.message, true, { kind: plan.toastKind, timeoutMs: plan.timeoutMs });
    return true;
  }, [toast]);

  const appendToLiveNode = useCallback((sid: string | null, itemId: string, delta: string, kind: 'text' | 'thinking') => {
    applyDeltaToNode(
      sid,
      itemId,
      delta,
      kind,
      kind === 'text' && sid === sessionIdRef.current ? pinIfPinned : undefined,
    );
    // Thinking mutates immediately; text pins only after its RAF HTML commit.
    if (kind === 'thinking' && sid === sessionIdRef.current) pinIfPinned();
  }, [pinIfPinned]);

  /** Mutate ONE session's item cache (null sid = active session). */
  const updateItemsFor = useCallback((sid: string | null, updater: (prev: Item[]) => Item[]) => {
    if (!sid) return;
    updateItemsMirror((prev) => ({ ...prev, [sid]: updater(prev[sid] ?? []) }));
  }, [updateItemsMirror]);


  const writeSteering = useCallback((updater: (prev: Record<string, string[]>) => Record<string, string[]>) => {
    const previous = steeringBySessionIdRef.current;
    const next = updater(previous);
    if (next === previous) return previous;
    steeringBySessionIdRef.current = next;
    setSteeringBySessionId(next);
    return next;
  }, []);
  const pushItemFor = useCallback(
    (sid: string | null, item: Item) => updateItemsFor(sid, (prev) => [...prev, item]),
    [updateItemsFor]
  );

  const patchItemFor = useCallback(
    (sid: string | null, id: string, patch: (item: Item) => Item) =>
      updateItemsFor(sid, (prev) => prev.map((item) => (item.id === id ? patch(item) : item))),
    [updateItemsFor]
  );

  const removeItemFor = useCallback(
    (sid: string | null, id: string) => updateItemsFor(sid, (prev) => prev.filter((item) => item.id !== id)),
    [updateItemsFor]
  );


  // Pending-command registry: owns the id -> pending map and per-command
  // bounded timers. A timeout removes the optimistic bubble and surfaces the
  // exact timeout message as an App error toast exactly when the command is
  // owned; the registry stamps the rejection's toastOwned from that boolean.
  // Socket close/replace drains retain bubbles (frame may be delivered). The heartbeat
  // silence window is the sole transport-stale/reconnect authority.
  if (pendingRegistryRef.current === null) {
    pendingRegistryRef.current = new PendingRegistry({
      scheduler: {
        setTimeout: (fn, ms) => window.setTimeout(fn, ms),
        clearTimeout: (timer) => window.clearTimeout(timer as number),
        now: () => Date.now(),
      },
      onTimeout: (entry, timeoutMessage) => {
        if (entry.bubbleId) removeItemFor(entry.sessionId ?? null, entry.bubbleId);
        const toastOwned = rpcFailureToastOwned({
          type: entry.type,
          boot: bootRef.current,
          sessionId: entry.sessionId,
          activeSessionId: sessionIdRef.current,
          auxiliary: entry.auxiliary,
        });
        if (toastOwned) {
          toast(timeoutMessage, true);
        }
        return toastOwned;
      },
    });
  }
  const pendingRegistry = pendingRegistryRef.current as PendingRegistry;
  // Lazily create the authoritative-bootstrap gate with the real window
  // scheduler. Like the pending registry, it lives for the App's lifetime;
  // successful bootstrap resolves waiters and unmount cleanup clears them.
  if (readyGateRef.current === null) {
    readyGateRef.current = new ReadyGate({
      setTimeout: (fn, ms) => window.setTimeout(fn, ms),
      clearTimeout: (timer) => window.clearTimeout(timer as number),
    });
  }
  const readyGate = readyGateRef.current;

  /** Bounded wait for authoritative WebSocket bootstrap. Auto/background
   *  loads (sidebar + session panel mount/poll) call this before `sendCommand`.
   *  Raw transport OPEN is insufficient: ReadyGate resolves only after the
   *  current generation has bound its session and restored its preference.
   *  Active actions bypass the gate and fail fast via `sendCommand`. */
  const waitForReady = useCallback((): Promise<void> => readyGate.wait(), [readyGate]);

  /** The session an event frame belongs to. Unowned frames fail closed rather
   *  than mutating whichever session happens to be active. */
  const frameSession = useCallback((frame: EventFrame): string | null => {
    const sid = (frame as { sessionId?: unknown }).sessionId;
    return typeof sid === 'string' && sid !== '' ? sid : null;
  }, []);

  /** Bump the unread badge when an event targets a NON-active session. */
  const markBackgroundEvent = useCallback((sid: string | null) => {
    if (!sid || sid === sessionIdRef.current) return;
    setUnreadBySessionId((prev) => ({ ...prev, [sid]: (prev[sid] ?? 0) + 1 }));
  }, []);

  /* ---------------- RPC plumbing ---------------- */

  const sendCommand = useCallback((
    command: Record<string, unknown>,
    bubbleId?: string,
    opts?: { auxiliary?: boolean; allowDuringSwitch?: boolean },
  ): Promise<unknown> => {
    const display = displayStateRef.current;
    if (commandRejectedBySwitchPending(!!display?.pending, opts)) {
      return Promise.reject(Object.assign(new Error('session switch pending'), { code: SWITCH_PENDING_ERROR }));
    }
    // Bootstrap/reconnect first command is the one id-less get_state that
    // discovers the new listener primary. commandTarget still names the
    // previous page session, so boot routing uses only sessionIdRef (cleared
    // in onOpen) until that bind lands.
    const implicitTarget = bootRef.current
      ? sessionIdRef.current
      : (display?.commandTarget.sessionId ?? sessionIdRef.current);
    const sid = routeCommandSession(command, implicitTarget);
    const ws = wsRef.current;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      if (bubbleId) removeItemFor(sid, bubbleId);
      // Ownership is metadata, never message matching: while an outage
      // surface is ACTIVE (persistent outage toast visible from a prior
      // close in this outage) the rejection is already owned and consumers
      // stay silent. Initial CONNECTING has no active outage surface — the
      // panel's user-action toast is then the single surface.
      const error = new Error('not connected') as Error & { transport?: boolean; toastOwned?: boolean };
      error.transport = true;
      if (transportOutageActiveRef.current) error.toastOwned = true;
      return Promise.reject(error);
    }
    const { promise, resolve, reject } = withResolvers<unknown>();
    const id = nextCommandId(pageNamespaceRef.current, ++seqRef.current);
    // MultiSessionRuntimeManager contract: every command carries a top-level
    // sessionId so the listener routes it to the owning runtime. An explicit
    // `command.sessionId` (lifecycle targeting another session) wins over the
    // ACTIVE session; boot/create commands omit it (null -> no field). The
    // manager strips sessionId BEFORE command deserialization (parse_input),
    // so deny_unknown_fields schemas (workflow_*) accept it safely on the
    // multi-session backend — every command, workflow included, routes to the
    // owning runtime instead of defaulting to the primary.
    // The routing rule lives in ./goal (routeCommandSession) so the
    // background goal_updated / lifecycle A→B cross-write invariant is
    // testable without a socket: an explicit command.sessionId wins over the
    // active session; a null/empty/absent sessionId falls back to active.
    const frame = sid ? { ...command, id, sessionId: sid } : { ...command, id };
    // Per-command bounded timeout (see ./pending.ts): the registry arms a
    // class-appropriate timer, names the command + elapsed seconds in the
    // timeout error, and calls back to remove the optimistic bubble only on a
    // timeout (drain paths keep the bubble for retry semantics).
    // Auxiliary is per-call, never inferred from command.type: a user-facing
    // get_session_stats still toasts; only callers that pass auxiliary:true
    // (context-meter refresh) reject quietly.
    const type = typeof command.type === 'string' ? command.type : 'command';
    pendingRegistry.add(id, {
      resolve,
      reject,
      bubbleId,
      sessionId: sid ?? undefined,
      gen: socketGenRef.current,
      type,
      auxiliary: opts?.auxiliary === true,
    });

    try {
      ws.send(JSON.stringify(frame));
    } catch (cause) {
      const pending = pendingRegistry.take(id);
      if (bubbleId) removeItemFor(pending?.sessionId ?? sid, bubbleId);
      const message = cause instanceof Error ? cause.message : String(cause);
      // Same ownership rule as the not-open reject above: an ACTIVE outage
      // surface owns this rejection; initial CONNECTING stays unowned.
      const error = new Error(`send failed: ${message}`) as Error & { transport?: boolean; toastOwned?: boolean };
      error.transport = true;
      if (transportOutageActiveRef.current) error.toastOwned = true;
      reject(error);
      tearTransportRef.current('send failed');
    }
    return promise;
  }, [removeItemFor]);
  // Invariant: layout-phase admission must precede every child passive read begin, or a fresh tab's first response is discarded.
  useLayoutEffect(() => {
    fileExplorerSyncPageCacheWithSlots(fileExplorerBySessionId);
  }, [fileExplorerBySessionId]);
  // Committed-slot diff → conversation closes. The ref above holds the last
  // COMMITTED slot map and is written only here (never render-assigned), so
  // the state updater stays pure of side effects; the diff derives each
  // removed tab's own conversationId from the PREV committed map and fires
  // the close keyed by that victim session id — never the currently
  // displayed session. A StrictMode repeat of the same committed state sees
  // prev === next and no new removals.
  useEffect(() => {
    const prev = fileExplorerCommittedSlotsRef.current;
    const next = fileExplorerBySessionId;
    if (prev === next) return;
    const droppedPathsBySession: Record<string, string[]> = {};
    for (const [ownerKey, prevSlot] of Object.entries(prev)) {
      const kept = new Set<string>();
      const nextSlot = next[ownerKey];
      if (nextSlot) {
        for (const tab of nextSlot.tabs) {
          kept.add(`${fileExplorerCanonicalPath(tab.path)}\u0000${tab.conversationId ?? ''}`);
        }
      }
      const dropped: string[] = [];
      for (const tab of prevSlot.tabs) {
        const path = fileExplorerCanonicalPath(tab.path);
        // Identity pair, not path alone: a same-path replacement holding a
        // DIFFERENT (or no) conversation id must retire the old identity.
        if (kept.has(`${path}\u0000${tab.conversationId ?? ''}`)) continue;
        if (!dropped.includes(path)) dropped.push(path);
      }
      if (dropped.length > 0) droppedPathsBySession[ownerKey] = dropped;
    }
    fileExplorerCommittedSlotsRef.current = next;
    if (Object.keys(droppedPathsBySession).length === 0) return;
    closeFileConversationsDroppedBySlotPatch(
      prev,
      { slots: next, evictedSessions: [], droppedPathsBySession },
      (command, ownerKey) => sendCommand(
        fileExplorerCommandForSession(ownerKey, command),
        undefined,
        { auxiliary: true, allowDuringSwitch: true },
      ),
      (message) => toast(`file conversation close failed: ${message}`, true),
    );
  }, [fileExplorerBySessionId, sendCommand, toast]);
  // Header option catalogs are scoped to the DISPLAYED session: thinking
  // levels derive from that session's model and the model catalog from its
  // auth. The old bootstrap-only fetch left every other session's real level
  // outside the options list, so the header select stayed on its placeholder
  // ("thinking…" / "model…") forever; refetch whenever the displayed session
  // or its model key changes, and on every (re)connect. A response for a
  // superseded session never commits.
  useEffect(() => {
    if (connState !== 'on' || !sessionId) return;
    const target = sessionId;
    sendCommand({ type: 'get_available_models', sessionId: target })
      .then((data) => {
        if (sessionIdRef.current !== target) return;
        const rows = normalizeHeaderModels((data as { models?: unknown } | null)?.models);
        setHeaderCatalogs((prev) => ({ ...prev, models: rows }));
      })
      .catch(() => {});
    sendCommand({ type: 'get_available_thinking_levels', sessionId: target })
      .then((data) => {
        if (sessionIdRef.current !== target) return;
        const list = normalizeHeaderLevels((data as { levels?: unknown } | null)?.levels);
        setHeaderCatalogs((prev) => ({ ...prev, levels: list }));
      })
      .catch(() => {});
  }, [activeModelKey, connState, sendCommand, sessionId]);
  const requestSettledStreamRefill = useCallback((sid: string) => {
    if (!sid || refillInFlightRef.current === sid) return;
    if (settledStreamRefillInFlightRef.current[sid] === true) return;
    settledStreamRefillInFlightRef.current = { ...settledStreamRefillInFlightRef.current, [sid]: true };
    void sendCommand({ type: 'get_messages', sessionId: sid }, undefined, { allowDuringSwitch: true, auxiliary: true })
      .then((data) => {
        if (streamingBySessionIdRef.current[sid] === true) return;
        const messages = messagesFromCatchUpResponse(data);
        clearStreamStateForSession(sid);
        delete activeAssistantBySessionIdRef.current[sid];
        updateItemsMirror((prev) => ({
          ...prev,
          [sid]: mergeAuthoritativeItems(
            messagesToItems(messages),
            prev[sid] ?? [],
            false,
          ),
        }));
      })
      .catch((err: Error & { toastOwned?: boolean }) => {
        if (planStreamCatchUpToast({
          toastOwned: isToastOwnedRpcError(err),
          transport: isTransportError(err),
          busyCapacity: isBusyCapacityError(err),
        }) === 'silent') return;
        toast(`transcript catch-up failed: ${err.message}`, true);
      })
      .finally(() => {
        const next = { ...settledStreamRefillInFlightRef.current };
        delete next[sid];
        settledStreamRefillInFlightRef.current = next;
        if (streamingBySessionIdRef.current[sid] !== true) {
          noteStreamStall(sid, { type: 'refill_finished' });
        }
      });
  }, [noteStreamStall, sendCommand, toast, updateItemsMirror]);
  requestSettledStreamRefillRef.current = requestSettledStreamRefill;
  const reconcileTaskJobs = useCallback((sessionId: string) => {
    const cut = taskJobIds(itemsBySessionIdRef.current[sessionId] ?? []);
    void sendCommand({ type: 'job_list', sessionId }, undefined, { auxiliary: true, allowDuringSwitch: true })
      .then((jobData) => {
        const jobs = jobsFromJobList(jobData);
        updateItemsFor(sessionId, (items) => applyJobSnapshot(items, jobs, cut));
      })
      .catch(() => {});
  }, [sendCommand, updateItemsFor]);



  /** Dedicated catalog sender for the SessionSidebar's background
   *  `session_list` loads: `{auxiliary: true}` bypasses the switch-pending
   *  gate (a mid-switch poll/revision refresh is a deferred sidebar intent,
   *  never a user-visible rejection) and keeps every catalog failure silent
   *  at the global RPC layer (rpcFailureToastOwned auxiliary arm). The
   *  sidebar retains its last-good rows on any rejection. Ordinary user
   *  actions keep the plain sendCommand and still toast. */
  const sendCatalogCommand = useCallback(
    (command: Record<string, unknown>): Promise<unknown> =>
      sendCommand(command, undefined, { auxiliary: true }),
    [sendCommand],
  );
  const rememberExtensionCatalog = useCallback((commands: CommandEntry[]) => {
    extensionNamesRef.current = new Set(extensionCommandNames(commands));
  }, []);
  // One App-lifecycle subscriber for document-delegated markdown path
  // clicks (transcript, FinalAssistant, tool/hub/IRC). The dispatcher has
  // already resolved any candidate against this exact rendered owner's cwd
  // and issued a synchronous preventDefault; the drawer opens only after an
  // async exact-owner file_explorer_read (maxLines 1) proves the file is
  // readable. Probe rejection is user-owned: it toasts here exactly once —
  // the shared transport toast already owns connection failures and
  // `toastOwned` marks RPC failures the global layer surfaced, so neither
  // is ever doubled.
  const onMarkdownOpenPath = useCallback((path: string, line: number | undefined, owner: MarkdownPathOwner, completion?: string) => {
    // A stale-cwd `file_unavailable` probe refreshes state and retries once.
    const attempt = (workspaceCwd: string, retried: boolean) => {
      void markdownOpenValidatedPath(
        path,
        line,
        { ...owner, workspaceCwd },
        (command) => sendCommand(command, undefined, { auxiliary: true }),
        (relative, openLine) => openFileInDrawer(relative, openLine, owner.sessionId),
        (error) => {
          if (markdownOpenRejectionOwnership(error) !== 'local') return;
          // Post-retry `file_unavailable` is a stable verdict: the server's
          // CURRENT workspace no longer has the referenced path. Name the
          // path (bounded) instead of the generic wording. A pre-retry
          // failure or a failed get_state refresh keeps the generic detail —
          // the refresh+retry affordance may still succeed on a later click.
          if (markdownOpenMissingFileVerdict(retried, error)) {
            toast(markdownOpenMissingFileToast(path), true);
            return;
          }
          const detail = markdownOpenErrorDetail(error);
          if (retried || (error as PendingCommandError | null)?.rpcCode !== 'file_unavailable') {
            toast(`open file failed: ${detail}`, true);
            return;
          }
          sendCommand({ type: 'get_state', sessionId: owner.sessionId })
            .then((state) => {
              const d = (state && typeof state === 'object' ? state : {}) as { cwd?: unknown };
              const fresh = typeof d.cwd === 'string' ? d.cwd.trim() : '';
              if (fresh) onSessionCwdChanged(owner.sessionId, fresh);
              attempt(fresh, true);
            })
            .catch(() => {
              toast(`open file failed: ${detail}`, true);
            });
        },
        completion,
      );
    };
    attempt(sessionCwdBySessionId[owner.sessionId] ?? '', false);
  }, [onSessionCwdChanged, openFileInDrawer, sendCommand, toast, sessionCwdBySessionId]);

  useEffect(() => subscribeMarkdownOpenPath(onMarkdownOpenPath), [onMarkdownOpenPath]);
  const onResponse = useCallback((frame: RpcResponse) => {
    const id = frame.id || '';
    const pending = pendingRegistry.take(id);
    if (!pending) return;
    // A response for a command sent on a superseded socket: the close/replace
    // drain already rejected its promise. Drop the stale settlement so it can
    // neither continue a dead bootstrap nor settle a newer socket's pending.
    if (!isCurrentPending(pending.gen, socketGenRef.current)) return;
    // Ownership gate (./pending.ts classifyResponseOwnership): command
    // identity first — a frame that does not NAME the pending command can
    // never own it — then session ownership. Ordinary source-owned commands
    // (and lifecycle FAILURES, which the backend tags with the request
    // source) must answer on the session the command was sent for; the
    // target-owned exemption exists ONLY for a SUCCESS envelope of a
    // lifecycle command, verified against the payload sessionId inside the
    // classifier. A rejected frame removes the optimistic bubble and rejects
    // exactly once with nothing mutated.
    const ownership = classifyResponseOwnership(frame, pending);
    if (ownership !== 'accept') {
      if (pending.bubbleId) removeItemFor(pending.sessionId ?? null, pending.bubbleId);
      pending.reject(new Error(RESPONSE_OWNERSHIP_REJECTIONS[ownership]));
      return;
    }
    if (frame.success) {
      const data = frame.data || {};
      if (pending.type === 'close_session') {
        const removedSid = data && typeof data === 'object' && 'sessionId' in data
          && typeof data.sessionId === 'string' ? data.sessionId : '';
        if (removedSid) {
          clearStreamStateForSession(removedSid);
          delete activeAssistantBySessionIdRef.current[removedSid];
          delete optimisticQueueBySessionIdRef.current[removedSid];
          const incarnation = residentBySessionIdRef.current[removedSid];
          if (incarnation) {
            cursorByRuntimeKeyRef.current.delete(runtimeKey({ sessionId: removedSid, runtimeIncarnation: incarnation }));
            // Invalidate BEFORE any later fast click can bind the retired
            // identity: drop the ready stamp and the resident overlay now,
            // not only when the session_unloaded frame lands.
            const nextReady = { ...readyBySessionIdRef.current };
            delete nextReady[removedSid];
            readyBySessionIdRef.current = nextReady;
            const nextResident = { ...residentBySessionIdRef.current };
            delete nextResident[removedSid];
            residentBySessionIdRef.current = nextResident;
            removeSurface(runtimeKey({ sessionId: removedSid, runtimeIncarnation: incarnation }));
          }
          loadedSessionIdsRef.current.delete(removedSid);
          updateItemsMirror((prev) => {
            if (!(removedSid in prev)) return prev;
            const next = { ...prev };
            delete next[removedSid];
            return next;
          });
        }
      }
      pending.resolve(data);
    } else {
      const toastOwned = rpcFailureToastOwned({
        type: pending.type,
        boot: bootRef.current,
        sessionId: pending.sessionId,
        activeSessionId: sessionIdRef.current,
        auxiliary: pending.auxiliary,
      });
      const rpcCode = rpcErrorCode(frame.data);
      const showToast = toastOwned && !rpcFailureToastSuppressed({ commandType: pending.type, rpcCode });
      if (showToast) {
        toast(`command ${frame.command} failed: ${frame.error || 'unknown error'}`, true);
      }
      const error: PendingCommandError = new Error(frame.error || 'rpc failed');
      error.rpc = true;
      if (showToast) error.toastOwned = true;
      error.rpcCode = rpcCode;
      pending.reject(error);
    }
  }, [removeItemFor, toast]);

  /** Reject + delete every pending command NOT on the keep generation, clearing
   *  its timeout and keeping optimistic bubbles (the frame may already have
   *  been delivered). Called from onclose when the socket drops — so pending
   *  commands settle IMMEDIATELY with the connection reason instead of hanging
   *  until their timeout or firing a stale continuation — and defensively from
   *  connect() when a new socket supersedes a replaced one (whose onclose was
   *  detached). */
  const rejectPendingExcept = useCallback((keepGen: number, reason: string, rpc = false, toastOwned = false) => {
    pendingRegistry.drainExcept(keepGen, reason, rpc, toastOwned);
  }, []);

  /** Dispatch a flattened pi_coding::TodoOp over the `todo_op` RPC. */
  const runTodoOp = useCallback((op: Record<string, unknown>) => {
    // Capture the target session at SEND time; the response applies to that
    // session's cache only (a concurrent switch cannot mis-route it).
    const sid = sessionIdRef.current;
    sendCommand({ type: 'todo_op', ...op, sessionId: sid })
      .then((data) => {
        if (!sid || !data || typeof data !== 'object' || !('phases' in data)) return;
        const phases = projectTodoPhases(data.phases);
        setTodoBySessionId((prev) => {
          const previous = prev[sid] ?? { sessionId: sid, phases: [], sequence: 0, executionStatus: 'dormant', failureReason: null };
          return { ...prev, [sid]: { ...previous, phases } };
        });
      })
      .catch((err: Error & { rpc?: boolean }) => {
        if (!err.rpc) toast(`todo op failed: ${err.message}`, true);
      });
  }, [sendCommand, toast]);

  /* ---------------- side chat ---------------- */

  /** Snapshot responses are applied to the sid captured at SEND time, so a
   *  session cutover mid-poll can never mis-route the reply. */
  const refreshSideChat = useCallback((sid: string | null) => {
    sendCommand({ type: 'side_chat_list', sessionId: sid })
      .then((data) => {
        if (!sid) return;
        setSideChatBySessionId((prev) => ({ ...prev, [sid]: data as SideChatSnapshot }));
      })
      .catch((err: Error & { rpc?: boolean }) => {
        // Transport errors (1006, connection closed) are handled by the
        // transport layer; only toast genuine RPC/business errors.
        if (shouldToastUnhandledCommandError(err)) {
          toast(`side chat: ${err.message}`, true);
        }
      });
  }, [sendCommand, toast]);

  const sideChatNew = useCallback((name: string) => {
    const sid = sessionIdRef.current;
    sendCommand({ type: 'side_chat_new', name, sessionId: sid })
      .then((data) => {
        if (!sid) return;
        setSideChatBySessionId((prev) => ({ ...prev, [sid]: data as SideChatSnapshot }));
      })
      .catch((err: Error & { rpc?: boolean }) => {
        if (shouldToastUnhandledCommandError(err)) {
          toast(`side chat new: ${err.message}`, true);
        }
      });
  }, [sendCommand, toast]);

  const sideChatSwitch = useCallback(async (name: string): Promise<SideChatSwitchResult> => {
    const sid = sessionIdRef.current;
    const seq = sideChatSwitchSeqRef.current + 1;
    sideChatSwitchSeqRef.current = seq;
    try {
      const data = await sendCommand({ type: 'side_chat_switch', name, sessionId: sid });
      if (seq !== sideChatSwitchSeqRef.current) return { kind: 'discardClosed' };
      if (!sid || sessionIdRef.current !== sid) {
        return {
          kind: 'error',
          message: `side chat switch: session changed (session=${sid ?? ''} tab=${name})`,
          toasted: false,
        };
      }
      if (activePanelRef.current !== 'sidechat') return { kind: 'discardClosed' };
      const ack = readSideChatSwitchAck(data);
      if (!ack) {
        const message = `side chat switch: malformed ACK (session=${sid} tab=${name})`;
        toast(message, true);
        return { kind: 'error', message, toasted: true };
      }
      if (ack.active !== name || !ack.tabs.includes(name)) return { kind: 'discardClosed' };
      if (seq !== sideChatSwitchSeqRef.current) return { kind: 'discardClosed' };
      const snapshot = data as SideChatSnapshot;
      setSideChatBySessionId((prev) => ({ ...prev, [sid]: snapshot }));
      return { kind: 'ok', snapshot };
    } catch (err) {
      const error = err as Error & { toastOwned?: boolean };
      const message = `side chat switch: ${error.message} (session=${sid ?? ''} tab=${name})`;
      const catchToast = planSideChatSwitchCatchToast({
        toastOwned: isToastOwnedRpcError(error),
        message,
      });
      if (catchToast.kind === 'error') {
        toast(catchToast.message, true);
      }
      return { kind: 'error', message, toasted: true };
    }
  }, [sendCommand, toast]);

  const sideChatClose = useCallback((name?: string) => {
    const sid = sessionIdRef.current;
    const command = name ? { type: 'side_chat_close', name } : { type: 'side_chat_close' };
    sendCommand(command)
      .then((data) => {
        if (!sid) return;
        setSideChatBySessionId((prev) => ({ ...prev, [sid]: data as SideChatSnapshot }));
      })
      .catch((err: Error & { rpc?: boolean }) => {
        if (shouldToastUnhandledCommandError(err)) {
          toast(`side chat close: ${err.message}`, true);
        }
      });
  }, [sendCommand, toast]);

  const sideChatPrompt = useCallback(async (message: string, images: ImageContentBlock[] = []): Promise<boolean> => {
    const sid = sessionIdRef.current;
    try {
      const data = await sendCommand({
        type: 'side_chat_prompt',
        message,
        ...(images.length > 0 ? { images } : {}),
        sessionId: sid,
      });
      if (!sid) return false;
      if (data === null || typeof data !== 'object') return false;
      const snapshot = data as SideChatSnapshot;
      setSideChatBySessionId((prev) => ({ ...prev, [sid]: snapshot }));
      if (snapshot.busy) toast('side chat is busy — wait for the current reply', true);
      return sideChatPromptAccepted(data);
    } catch (err) {
      const error = err as Error & { rpc?: boolean };
      if (shouldToastUnhandledCommandError(error)) {
        toast(`side chat send failed: ${error.message}`, true);
      }
      return false;
    }
  }, [sendCommand, toast]);

  const sideChatAbort = useCallback(() => {
    const sid = sessionIdRef.current;
    sendCommand({ type: 'side_chat_abort', sessionId: sid })
      .then((data) => {
        if (!sid) return;
        setSideChatBySessionId((prev) => ({ ...prev, [sid]: data as SideChatSnapshot }));
      })
      .catch((err: Error & { rpc?: boolean }) => {
        if (shouldToastUnhandledCommandError(err)) {
          toast(`side chat stop failed: ${err.message}`, true);
        }
      });
  }, [sendCommand, toast]);

  // Poll the side-chat snapshot while the panel is open (the side agent runs
  // detached; snapshots drain controller events on the server). The polled
  // session is captured per tick; the panel remounts on session switch, so
  // its responses can never land in another session's cache.
  useEffect(() => {
    if (activePanel !== 'sidechat') return;
    const poll = () => refreshSideChat(sessionIdRef.current);
    poll();
    const timer = window.setInterval(poll, 2000);
    return () => window.clearInterval(timer);
  }, [activePanel, refreshSideChat, sessionId]);

  /** Refresh the Goal panel snapshot + journal for ONE session. The
   *  target sid is captured at call time and every response is keyed to it —
   *  an async refreshGoal() can never query one session and paint another. */
  const refreshGoal = useCallback((sid: string | null): Promise<void> => {
    if (!sid) return Promise.resolve();
    return Promise.all([
      sendCommand(goalGetCommand(sid))
        .then((data) => {
          setGoalStateBySessionId((prev) => ({ ...prev, [sid]: data as GoalStateWire }));
        })
        .catch(() => {}),
      sendCommand(goalJournalCommand(sid))
        .then((data) => {
          if (Array.isArray(data)) {
            setGoalJournalBySessionId((prev) => ({ ...prev, [sid]: data as GoalEventWire[] }));
          }
        })
        .catch(() => {}),
    ]).then(() => undefined);
  }, [sendCommand]);
  /** Re-fetch the context-window usage meter for ONE session
   *  (`get_session_stats` -> `SessionStats.contextUsage`). SILENT on failure —
   *  a transient RPC error must not toast; the next snapshot/turn end
   *  re-refreshes. Targeted at the owning session (explicit `sessionId`
   *  routing, goal_get precedent) so a background refresh can never crosstalk
   *  the active session's meter. */
  const refreshContextUsage = useCallback((target: string | null) => {
    const sid = target ?? sessionIdRef.current;
    if (!sid) return;
    const cut = beginContextUsageRequest(contextUsageSeqBySessionIdRef.current, sid);
    contextUsageSeqBySessionIdRef.current = cut.bySessionId;
    sendCommand(sessionStatsCommand(sid), undefined, { auxiliary: true })
      .then((data) => {
        if (!ownsContextUsageResponse(contextUsageSeqBySessionIdRef.current, sid, cut.sequence)) return;
        setContextUsageBySessionId((prev) => ({ ...prev, [sid]: parseContextUsage(data) }));
      })
      .catch(() => {});
  }, [sendCommand]);


  /** Apply a session state snapshot to ONE session's slots. The target is the
   *  explicit `targetSid` (lifecycle snapshots, refreshState captures) when
   *  given, else the state's own sessionId, else the active session. Every
   *  field is written to the target session's cache only — model/thinking/
   *  name/Todo/Goal/streaming never leak across sessions. A target that
   *  differs from the currently active session is activated only when
   *  `shouldActivate` is true; background refreshes update their cache without
   *  pulling the active view back to an older request's session. */
  const applyState = useCallback((
    data: unknown,
    targetSid?: string | null,
    shouldActivate = true,
  ) => {
    const d = (data || {}) as {
      model?: { id?: string; provider?: string } | null;
      activeTurnModel?: { id?: string; provider?: string } | null;
      activeTurnId?: string | null;
      thinkingLevel?: string;
      isStreaming?: boolean;
      isCompacting?: boolean;
      sessionName?: string | null;
      sessionId?: string | null;
      todoPhases?: unknown;
      todoExecutionStatus?: unknown;
      todoFailureReason?: unknown;
      goal?: GoalStateWire;
      runtimeSettings?: { live?: LiveSettingsWire; plantumlServer?: unknown };
      runtimeIncarnation?: string | null;
      cwd?: string | null;
      queuedSteering?: unknown;
    };
    const target =
      targetSid && targetSid !== ''
        ? targetSid
        : typeof d.sessionId === 'string' && d.sessionId !== ''
          ? d.sessionId
          : sessionIdRef.current;
    if (!target) return false;
    const snapshotPlan = planSessionSnapshotApply({
      shouldActivate,
      wireIncarnation: d.runtimeIncarnation,
      residentIncarnation: residentBySessionIdRef.current[target],
      displayedSessionId: sessionIdRef.current,
      targetSessionId: target,
    });
    if (!snapshotPlan.apply) return false;
    const identity = identityFromWire(target, d.runtimeIncarnation);
    if (identity) {
      ensureSurface(identity);
      const resident = residentBySessionIdRef.current[target];
      if (resident !== identity.runtimeIncarnation) {
        residentBySessionIdRef.current = { ...residentBySessionIdRef.current, [target]: identity.runtimeIncarnation };
      }
      if (!displayStateRef.current && shouldActivate) {
        displayStateRef.current = { displayed: identity, commandTarget: identity, pending: null, nextGeneration: 1 };
        publishLifecycle(displayStateRef.current);
      }
    }
    const cwd = typeof d.cwd === 'string' ? d.cwd.trim() : '';
    setSessionCwdBySessionId((prev) => {
      if (cwd) return prev[target] === cwd ? prev : { ...prev, [target]: cwd };
      if (!(target in prev)) return prev;
      const next = { ...prev };
      delete next[target];
      return next;
    });

    const modelKey = wireModelKey(d.model);
    if (modelKey) {
      writeSessionModelState(reconcileSessionModel(sessionModelStateRef.current, target, modelKey));
    }
    if (d.thinkingLevel) {
      writeThinkingLevelState(reconcileThinkingLevel(thinkingLevelStateRef.current, target, d.thinkingLevel));
    }
    if (typeof d.isStreaming === 'boolean') {
      const incomingStreaming = d.isStreaming === true;
      if (shouldHoldOptimisticStop(abortPendingBySessionIdRef.current[target] === true, incomingStreaming)) {
        updateActivityStripFor(target, (state) => reconcileActivityStrip(state, target, false, ''));
      } else {
        streamingBySessionIdRef.current = { ...streamingBySessionIdRef.current, [target]: incomingStreaming };
        setStreamingBySessionId((prev) => ({ ...prev, [target]: incomingStreaming }));
        if (!incomingStreaming) {
          const toastAborted = abortIdleSnapshotToasts(
            abortPendingBySessionIdRef.current[target] === true,
            incomingStreaming,
            displayedSessionMatches(target, displayStateRef.current?.displayed.sessionId),
          );
          abortPendingBySessionIdRef.current[target] = false;
          setAbortingBySessionId((prev) => dropSessionAborting(prev, target));
          if (toastAborted) toast('run aborted');
        }
        armStreamWatchdogRef.current(target);
        updateActivityStripFor(target, (state) => reconcileActivityStrip(
          state,
          target,
          incomingStreaming,
          typeof d.activeTurnId === 'string' ? d.activeTurnId : '',
        ));
      }
    }
    const queuedSteering = steeringPreviewsFromState(d);
    const liveFromQueueUpdate = steeringLiveBySessionIdRef.current[target] === socketGenRef.current;
    const submitInFlight = (steeringSubmitInFlightBySessionRef.current[target] ?? 0) > 0;
    writeSteering((prev) => applyStateQueuedSteering(
      prev,
      target,
      queuedSteering,
      liveFromQueueUpdate,
      d.isStreaming === true,
      submitInFlight,
    ));
    const settledRefill = planSettledStreamRefillRequest({
      snapshotStreaming: d.isStreaming,
      liveHasStreamingAssistant: liveHasStreamingAssistant(itemsBySessionIdRef.current[target] ?? []),
      reconnectRefillSessionId: refillInFlightRef.current,
      targetSessionId: target,
    });
    if (settledRefill === 'request') {
      requestSettledStreamRefillRef.current(target);
    }
    if (typeof d.isStreaming === 'boolean' && d.isStreaming !== true) {
      noteStreamStall(target, { type: 'snapshot_idle', refill: settledRefill });
    }
    if (typeof d.isCompacting === 'boolean') {
      writeCompactBook(reconcileCompacting(compactBookRef.current, target, d.isCompacting));
    }

    if (Array.isArray(d.todoPhases)) {
      setTodoBySessionId((prev) => {
        const previous = prev[target] ?? { sessionId: target, phases: [], sequence: 0, executionStatus: 'dormant', failureReason: null };
        const executionStatus = d.todoExecutionStatus === 'active' || d.todoExecutionStatus === 'settled' || d.todoExecutionStatus === 'failed' ? d.todoExecutionStatus : 'dormant';
        const source = d.todoFailureReason && typeof d.todoFailureReason === 'object' ? d.todoFailureReason as { kind?: unknown; message?: unknown } : null;
        const kind = source?.kind;
        const failureReason: TodoFailureReason | null =
          source != null
          && (kind === 'no_progress' || kind === 'spawn' || kind === 'reconcile')
          && typeof source.message === 'string'
            ? { kind, message: safeText(source.message) }
            : null;
        return { ...prev, [target]: { ...previous, phases: projectTodoPhases(d.todoPhases), executionStatus, failureReason } };
      });
    }
    if (d.goal && typeof d.goal === 'object') {
      setGoalStateBySessionId((prev) => ({ ...prev, [target]: d.goal as GoalStateWire }));
    }
    const runtime = d.runtimeSettings;
    if (runtime && typeof runtime === 'object') {
      const live = runtime.live && typeof runtime.live === 'object' ? runtime.live : null;
      liveSettingsBySessionIdRef.current = { ...liveSettingsBySessionIdRef.current, [target]: live };
      if (shouldActivate || sessionIdRef.current === target) setLiveSettings(live);
      configurePlantumlServer(runtime.plantumlServer);
    }
    // Context-window usage rides every applied snapshot (boot, reconnect,
    // panel refreshes, lifecycle cutover): one bounded get_session_stats per
    // get_state, silent on failure.
    refreshContextUsage(target);
    if (!shouldActivate) return true;
    // Session cutover (previous non-empty id -> DIFFERENT non-empty id): the
    // active view switches to the new session's cache. Same-id refreshes
    // (reconnect, settings apply, rename) keep the transcript untouched.
    if (sessionIdRef.current !== target) {
      // Activating a session clears its unread badge.
      setUnreadBySessionId((prev) => ({ ...prev, [target]: 0 }));
    }
    const previousSession = sessionIdRef.current;
    if (shouldStopRealtimeOnSessionSwitch(realtimeOwnerRef.current, previousSession, target)) {
      stopRealtimeRef.current();
    }
    if (previousSession && previousSession !== target) cancelTextRendersForSession(previousSession);
    sessionIdRef.current = target;
    setLiveSettings(liveSettingsBySessionIdRef.current[target] ?? null);
    setSessionId(target);
    return true;
  }, [noteStreamStall, publishLifecycle, refreshContextUsage, toast, updateActivityStripFor, writeCompactBook, writeSessionModelState, writeSteering, writeThinkingLevelState]);

  const streamWatchdogIoRef = useRef({ applyState, sendCommand, toast, noteStreamStall });
  streamWatchdogIoRef.current = { applyState, sendCommand, toast, noteStreamStall };
  const streamWatchdog = useMemo(() => createStreamWatchdog({
    watchdogMs: STREAM_WATCHDOG_MS,
    now: () => Date.now(),
    schedule: (fn, ms) => window.setTimeout(fn, ms),
    cancel: (id) => window.clearTimeout(id),
    isStreaming: (sid) => streamingBySessionIdRef.current[sid] === true,
    onSilence: (sid) => {
      const io = streamWatchdogIoRef.current;
      if (gapCatchUpInFlightRef.current[sid] === true || refillInFlightRef.current === sid) {
        if (streamingBySessionIdRef.current[sid] === true) streamWatchdogRef.current?.arm(sid);
        return;
      }
      void io.sendCommand({ type: 'get_state', sessionId: sid }, undefined, { allowDuringSwitch: true, auxiliary: true })
        .then((state) => {
          io.noteStreamStall(sid, { type: 'watchdog_probe_alive' });
          io.applyState(state, sid, false);
        })
        .catch((err: Error & { toastOwned?: boolean }) => {
          io.noteStreamStall(sid, { type: 'watchdog_probe_failed' });
          if (planStreamCatchUpToast({
            toastOwned: isToastOwnedRpcError(err),
            transport: isTransportError(err),
            busyCapacity: isBusyCapacityError(err),
          }) === 'silent') return;
          io.toast(`stream catch-up failed: ${err.message}`, true);
        })
        .finally(() => {
          if (streamingBySessionIdRef.current[sid] === true) streamWatchdogRef.current?.arm(sid);
        });
    },
  }), []);
  streamWatchdogRef.current = streamWatchdog;
  armStreamWatchdogRef.current = streamWatchdog.arm;

  /** Copy-context on-demand seam (user requirement: Copy context is ALWAYS
   *  effective, like pi's /context): when a LOADED session has no stored
   *  compaction summary, the sidebar triggers this to run the SAME /compact
   *  wire path (compactAdmission guards apply) with handoff instructions and
   *  show the existing compactProgress book. ROOT-CAUSE FIX (live defect:
   *  'compacted but produced no summary'): the compact RpcResponse data IS
   *  the backend CompactionResult (crates/pi-coding/src/compaction.rs —
   *  camelCase {summary, firstKeptEntryId, tokensBefore, ...}) — the summary
   *  PROSE rides the response, so read it straight from the settled data
   *  (zero extra fetch). get_messages refetch is a defensive fallback only
   *  for backends whose response omits `summary`. Resolves '' when the
   *  compact legitimately produced no summary (session with nothing to
   *  summarize) — the SIDEBAR then falls back to the transcript brief with a
   *  truthful toast. Refusal (streaming/compacting) rejects with the bounded
   *  admission reason. */
  const requestSessionCompactSummary = useCallback(async (targetSid: string): Promise<string> => {
    const admission = compactAdmission({
      streaming: !!(targetSid && streamingBySessionIdRef.current[targetSid]),
      compacting: !!(targetSid && compactBookRef.current.compacting[targetSid]),
    });
    if (!admission.ok) throw new Error(admission.message);
    const command: Record<string, unknown> = {
      type: 'compact',
      customInstructions: 'summarize this session for handoff',
    };
    if (targetSid) command.sessionId = targetSid;
    const started = beginCompact(compactBookRef.current, targetSid, 'llm');
    try {
      writeCompactBook(started.book);
      const data = await sendCommand(command) as { summary?: unknown } | null;
      const direct = typeof data?.summary === 'string' ? data.summary.trim() : '';
      if (direct !== '') return direct;
      // Defensive fallback (older backends without CompactionResult.summary):
      // the summary lands in the transcript as a compactionSummary message —
      // refetch the authoritative history and read it from the mirror.
      const page = await sendCommand({ type: 'get_messages', sessionId: targetSid });
      const messages = (page && typeof page === 'object' && Array.isArray((page as { messages?: unknown }).messages))
        ? (page as { messages: unknown[] }).messages
        : [];
      const streaming = streamingBySessionIdRef.current[targetSid] ?? false;
      updateItemsFor(targetSid, (items) => mergeAuthoritativeItems(messagesToItems(messages), items, streaming));
      return latestCompactionSummary(itemsBySessionIdRef.current[targetSid] ?? []) ?? '';
    } finally {
      writeCompactBook(settleCompact(compactBookRef.current, targetSid, started.token));
      refreshContextUsage(targetSid);
    }
  }, [refreshContextUsage, sendCommand, updateItemsFor, writeCompactBook]);

  // Re-fetch get_state into the app shell; panels call this after mutations
  // (settings apply, rename) so the header stays authoritative. The target
  // sid is captured at SEND time so the response applies to the session that
  // was queried, even if the active session changes while in flight.
  const refreshState = useCallback(() => {
    const sid = sessionIdRef.current;
    return sendCommand({ type: 'get_state', sessionId: sid })
      .then((data) => applyState(data, sid, false))
      .catch(() => {});
  }, [applyState, sendCommand]);
  /** Clear every composer attachment intake surface at a successful context
   *  cutover. Bumping the generation invalidates ordinary reads; video upload
   *  authentication remains separate until terminal settle/removal. */
  const resetComposerAttachments = useCallback(() => {
    attachmentsRef.current = [];
    setAttachments([]);
    setDropActive(false);
    dragDepthRef.current = 0;
    intakeBudgetTrackerRef.current.reset();
    pendingUploadIdsRef.current.clear();
    intakeGenRef.current += 1;
    intakeCommitQueueRef.current = Promise.resolve();
  }, []);


  const panelRootFor = useCallback((name: string): HTMLElement | null => {
    const selectorByPanel: Record<string, string> = {
      todo: '#todo-panel',
      activity: '#activity-panel',
      goal: '#goal-panel',
      workflow: '#workflow-panel',
      sidechat: '#side-chat-panel',
      subagents: '#subagents-panel',
      personas: '#personas-panel',
      session: '#session-panel',
      settings: '#settings-panel',
      'code-review': '.code-review-panel',
    };
    const selector = selectorByPanel[name];
    return selector ? document.querySelector<HTMLElement>(selector) : null;
  }, []);

  const saveDisplayedView = useCallback((identity = displayStateRef.current?.displayed, scanTranscript = true) => {
    const display = displayStateRef.current;
    if (!display || !identity) return;
    const input = promptInputRef.current;
    const transcript = transcriptRef.current;
    const content = surfaceContentElsRef.current.get(runtimeKey(identity)) ?? null;
    const current = viewSlotsByRuntimeKeyRef.current.get(runtimeKey(identity))
      ?? emptyViewSlot(identity);
    // The full DOM scan (scroll anchor + open fold keys) is only needed when
    // the source view is about to be left for a NEGOTIATED switch. The
    // frontend-only fast path determines eligibility BEFORE any save and
    // reads the incrementally maintained live viewport state
    // (./transcriptView.ts) with zero DOM reads: the scroll handler resolves
    // the anchor per event through bounded hit-test probes and fold toggles
    // record one stamped key, so the saved anchor and folds are the LATEST
    // user state. The scan path also seeds the live state so subsequent
    // per-event updates start exact.
    const liveKey = runtimeKey(identity);
    const live = liveViewportsRef.current.get(liveKey) ?? null;
    const viewport: ViewportState = readTranscriptViewport({
      live,
      current: {
        anchorItemId: current.transcriptScroll.anchorItemId,
        anchorOffsetPx: current.transcriptScroll.anchorOffsetPx,
        openFoldKeys: current.openFoldKeys,
      },
      transcript,
      content: content ? transcriptContentRoot(content) : null,
      scan: scanTranscript,
    });
    const focused = document.activeElement as HTMLElement | null;
    const focusedElementKey = focused?.id
      ? `id:${focused.id}`
      : focused?.getAttribute('name')
        ? `name:${focused.getAttribute('name')}`
        : focused?.getAttribute('aria-label')
          ? `aria:${focused.tagName.toLowerCase()}:${focused.getAttribute('aria-label')}`
          : null;
    const panel = panelRootFor(activePanelRef.current);
    const slot: SessionViewSlot = {
      ...current,
      identity,
      lastUsed: Date.now(),
      draft: input?.value ?? '',
      selectionStart: input?.selectionStart ?? 0,
      selectionEnd: input?.selectionEnd ?? 0,
      attachments: [...attachmentsRef.current],
      intakeGeneration: intakeGenRef.current,
      pendingUploadIds: new Set(pendingUploadIdsRef.current),
      transcriptScroll: {
        follow: viewport.follow,
        anchorItemId: viewport.anchorItemId,
        anchorOffsetPx: viewport.anchorOffsetPx,
        fallbackScrollTop: viewport.fallbackScrollTop,
      },
      focusedElementKey,
      openFoldKeys: viewport.openFoldKeys,
      activePanel: activePanelRef.current,
      panelScrollTop: panel?.scrollTop ?? 0,
    };
    viewSlotsByRuntimeKeyRef.current = upsertViewSlot(
      viewSlotsByRuntimeKeyRef.current,
      slot,
      identity,
      display.pending,
    );
    // The live viewport re-seeds from the saved slot after EVERY save so the
    // per-event scroll/fold updates always start from the authoritative
    // state — including the fast path, whose saved anchor/folds came from the
    // live state itself.
    liveViewportsRef.current.set(liveKey, seedLiveViewport(liveKey, {
      follow: slot.transcriptScroll.follow,
      anchorItemId: slot.transcriptScroll.anchorItemId,
      anchorOffsetPx: slot.transcriptScroll.anchorOffsetPx,
      fallbackScrollTop: slot.transcriptScroll.fallbackScrollTop,
      openFoldKeys: slot.openFoldKeys,
    }).viewport);
  }, [panelRootFor, transcriptRef]);

  const restoreDisplayedView = useCallback((identity: RuntimeIdentity) => {
    const slot = viewSlotsByRuntimeKeyRef.current.get(runtimeKey(identity));
    const input = promptInputRef.current;
    if (input) {
      const draft = slot?.draft ?? '';
      input.value = draft;
      const start = Math.min(slot?.selectionStart ?? draft.length, draft.length);
      const end = Math.min(slot?.selectionEnd ?? start, draft.length);
      input.setSelectionRange(start, end);
    }
    const restoredAttachments = (slot?.attachments ?? []).filter(
      (attachment) => (attachment.kind !== 'video' && attachment.kind !== 'pdf') || attachment.videoState !== 'uploading',
    );
    attachmentsRef.current = restoredAttachments;
    setAttachments(restoredAttachments);
    intakeBudgetTrackerRef.current.setQueued(reconcileIntakeBudget(restoredAttachments));
    pendingUploadIdsRef.current = new Set();
    // Generations are monotonic; restoring an old slot value would create an
    // ABA window where a pre-switch async read can mutate the new projection.
    intakeGenRef.current += 1;
    if (slot && slot.lastUsed !== 0) {
      activePanelRef.current = slot.activePanel;
      setActivePanel(slot.activePanel);
    }
    if (restoreViewRafRef.current !== null) window.cancelAnimationFrame(restoreViewRafRef.current);
    const generation = ++restoreViewGenerationRef.current;
    restoreViewRafRef.current = window.requestAnimationFrame(() => {
      restoreViewRafRef.current = null;
      if (restoreViewGenerationRef.current !== generation || !sameRuntime(activatedRuntimeRef.current, identity)) return;
      const transcript = transcriptRef.current;
      const content = surfaceContentElsRef.current.get(runtimeKey(identity)) ?? null;
      // The saved viewport is written through the production restore
      // (./transcriptView.ts): the anchor item is resolved by its stamped
      // key (one bounded scan per restore) and the fold set is applied to
      // the stamped folds — never through an items/index mapping. The saved
      // folds re-open BEFORE the follow pin: a re-opened fold grows
      // scrollHeight, so the pin must land on the POST-fold bottom, not the
      // pre-fold one (which would leave the restored view short of the new
      // bottom until the next async observer pass). A non-following restore
      // writes the anchor offset here and reconciles the pin controller.
      const restored: ViewportState = {
        follow: slot?.transcriptScroll.follow ?? true,
        anchorItemId: slot?.transcriptScroll.anchorItemId ?? null,
        anchorOffsetPx: slot?.transcriptScroll.anchorOffsetPx ?? 0,
        fallbackScrollTop: slot?.transcriptScroll.fallbackScrollTop ?? 0,
        openFoldKeys: slot?.openFoldKeys ?? new Set<string>(),
      };
      if (content) applyRestoredViewport(transcript, transcriptContentRoot(content), restored);
      // An UNPINNED restore re-arms the App-global pin controller BEFORE any
      // scroll note or observer callback: the controller still carries the
      // previous session's pinned=true from its follow-restore, and the
      // surface swap itself changes scrollHeight — a stale pinned state would
      // snap the freshly restored view to the bottom (destroying the retained
      // reading position) on this very note or on the re-created
      // ResizeObserver's initial callback. The adoption is EXPLICITLY
      // unpinned — never remaining-1 derived from the restored position —
      // because the saved slot says follow=false: a view restored near the
      // bottom must stay frozen until the user genuinely scrolls (the
      // restore's own programmatic scroll event carries the adopted
      // scrollTop and cannot re-classify). A FOLLOW restore keeps the
      // forcePin write below.
      if (restored.follow || restored.anchorItemId === null) forcePin();
      else if (transcript) pinCtrl.adoptUnpinned(transcript);
      // The synthetic onTranscriptScroll reconcile is gone: it routed through
      // pinCtrl.onScroll and re-derived pin state from geometry, re-pinning a
      // near-bottom follow=false restore. The live viewport reconciles
      // through its own mechanism — the reseed below from the restored slot,
      // then the programmatic write's real scroll event through
      // noteLiveTranscriptScroll (pin state untouched thanks to
      // adoptUnpinned's programmatic-echo recognition).
      const panel = panelRootFor(slot?.activePanel ?? '');
      if (panel) panel.scrollTop = slot?.panelScrollTop ?? 0;
      const focusKey = slot?.focusedElementKey;
      if (focusKey?.startsWith('id:')) {
        document.querySelector<HTMLElement>(`[id="${CSS.escape(focusKey.slice(3))}"]`)?.focus();
      } else if (focusKey?.startsWith('name:')) {
        const name = focusKey.slice(5);
        document.querySelector<HTMLElement>(`[name="${CSS.escape(name)}"]`)?.focus();
      } else if (focusKey?.startsWith('aria:')) {
        const separator = focusKey.indexOf(':', 5);
        const tag = focusKey.slice(5, separator);
        const label = focusKey.slice(separator + 1);
        Array.from(document.querySelectorAll<HTMLElement>(tag)).find((node) => node.getAttribute('aria-label') === label)?.focus();
      }
      // The live viewport seeds from the restored slot AFTER the DOM writes:
      // the programmatic folds fire toggle events and the programmatic scroll
      // fires a scroll event, and both handlers must reconcile against exactly
      // the restored state.
      liveViewportsRef.current.set(runtimeKey(identity), seedLiveViewport(runtimeKey(identity), {
        follow: slot?.transcriptScroll.follow ?? true,
        anchorItemId: slot?.transcriptScroll.anchorItemId ?? null,
        anchorOffsetPx: slot?.transcriptScroll.anchorOffsetPx ?? 0,
        fallbackScrollTop: slot?.transcriptScroll.fallbackScrollTop ?? 0,
        openFoldKeys: slot?.openFoldKeys ?? new Set<string>(),
      }).viewport);
    });
  }, [forcePin, panelRootFor, pinCtrl, transcriptRef]);

  /** Advance the live viewport from one scroll event through the production
   *  note (./transcriptView.ts noteLiveViewportScroll): the anchor resolves
   *  via a bounded x×y engine hit-test probe grid — a CONSTANT number of
   *  operations per event, so a first→last scrollbar jump costs the same as
   *  a one-pixel nudge and a fast switch's saveDisplayedView(false) always
   *  reads the LATEST scroll state. The probe rect composes the two surfaces
   *  the grid needs: the Y origin is the SCROLL CONTAINER's viewport top
   *  (the content wrapper's top rides the scroll — once scrolled away it is
   *  far above the viewport, where elementsFromPoint finds nothing and every
   *  parked view would save a NULL anchor — the negotiated scan has always
   *  used the container top; the note must agree), and the x-span is the
   *  content wrapper's rendered column (the container's rect would include
   *  the scrollbar and the centered-column margins). */
  const noteLiveTranscriptScroll = useCallback(() => {
    const key = displayedRuntimeKeyRef.current;
    const contentRect = transcriptContentRef.current?.getBoundingClientRect();
    const containerRect = transcriptRef.current?.getBoundingClientRect();
    noteLiveViewportScroll(
      key ? (liveViewportsRef.current.get(key) ?? null) : null,
      DOCUMENT_HIT_SURFACE,
      transcriptRef.current,
      contentRect && containerRect
        ? { top: containerRect.top, left: contentRect.left, width: contentRect.width }
        : null,
      pinCtrl.isFollowing(),
    );
  }, [pinCtrl, transcriptContentRef, transcriptRef]);
  const handleTranscriptScroll = useCallback((event: UIEvent<HTMLDivElement>) => {
    onTranscriptScroll(event);
    noteLiveTranscriptScroll();
  }, [noteLiveTranscriptScroll, onTranscriptScroll]);
  const getSurfaceViewport = useCallback((rk: RuntimeKey) => liveViewportsRef.current.get(rk) ?? null, []);
  const handleSurfaceContentEl = useCallback((rk: RuntimeKey, el: HTMLDivElement | null) => {
    if (el) surfaceContentElsRef.current.set(rk, el);
    else surfaceContentElsRef.current.delete(rk);
  }, []);
  const handleSurfaceCommit = useCallback((rk: RuntimeKey, itemsVersion: number, projectedCount: number) => {
    projectedDomBySurfaceRef.current.set(rk, projectedCount);
    surfaceCommittedVersionRef.current.set(rk, itemsVersion);
    publishLifecycle(displayStateRef.current);
  }, [publishLifecycle]);
  /** O(1) item id -> projection index resolver for the parked-window fix:
   *  the id->index map is App-owned (rebuilt by updateItemsMirror) so the
   *  demand-window coverage check never walks siblings. */
  const getSurfaceAnchorIndex = useCallback((sessionId: string, anchorItemId: string): number => {
    return itemIndexBySessionIdRef.current.get(sessionId)?.get(anchorItemId) ?? -1;
  }, []);
  useLayoutEffect(() => {
    transcriptContentRef.current = displayedRuntimeKey
      ? (surfaceContentElsRef.current.get(displayedRuntimeKey) ?? null)
      : null;
  }, [displayedRuntimeKey, surfaceEpoch, transcriptContentRef]);

  useEffect(() => {
    if (!displayedRuntimeKey) return;
    const container = transcriptRef.current;
    const content = surfaceContentElsRef.current.get(displayedRuntimeKey) ?? null;
    if (!container || !content) return;
    const observer = new ResizeObserver(() => pinCtrl.handleResize(container));
    observer.observe(container);
    observer.observe(content);
    return () => observer.disconnect();
  }, [displayedRuntimeKey, pinCtrl, surfaceEpoch, transcriptRef]);

  // Fold state maintenance: native `toggle` does not bubble, but the capture
  // phase still descends to the target, so ONE listener on the content wrapper
  // observes every transcript fold and records the toggled element's stamped
  // key in the live viewport key set — O(1) per toggle, no child/sibling
  // enumeration. Programmatic folds (restoreDisplayedView) fire the same
  // event and reconcile the live set against the restored slot.
  useEffect(() => {
    const content = transcriptContentRef.current;
    if (!content) return;
    const onToggle = (event: Event) => {
      const target = event.target;
      if (!(target instanceof HTMLDetailsElement)) return;
      const key = displayedRuntimeKeyRef.current;
      noteLiveViewportFold(key ? (liveViewportsRef.current.get(key) ?? null) : null, target);
    };
    content.addEventListener('toggle', onToggle, true);
    return () => content.removeEventListener('toggle', onToggle, true);
  }, [displayedRuntimeKey, surfaceEpoch, transcriptContentRef]);

  const activateRuntime = useCallback((identity: RuntimeIdentity, restoreView: boolean) => {
    const previous = sessionIdRef.current;
    // A review workspace belongs to ONE session. The transition is judged on
    // the DISPLAYED identity this activation is replacing —
    // `activatedRuntimeRef`, the runtime the display was showing — never on
    // the active session ref: a warm reconnect re-binds that ref to the
    // listener primary while the displayed surface stays where the user was,
    // so an active-ref comparison misses the switch that actually leaves the
    // review's session (displayed B, ref A, switch to A). The open revisions
    // are dropped and the review panel is closed (its unmount stamps its own
    // close to the session it was showing), so the panel a restored view slot
    // re-opens is mounted under THIS session with no revisions carried over.
    // A same-session reincarnation (rebind) and a warm reconnect that keeps
    // the displayed session leave both intact.
    const previousDisplayed = activatedRuntimeRef.current;
    if (previousDisplayed && previousDisplayed.sessionId !== identity.sessionId) {
      setActivePanel((panel) => (panel === 'code-review' ? '' : panel));
      setCodeReviewOpenArgs({});
    }
    if (shouldStopRealtimeOnSessionSwitch(realtimeOwnerRef.current, previous, identity.sessionId)) {
      stopRealtimeRef.current();
    }
    if (previous && previous !== identity.sessionId) cancelTextRendersForSession(previous);
    activatedRuntimeRef.current = identity;
    sessionIdRef.current = identity.sessionId;
    setSessionId(identity.sessionId);
    setLiveSettings(liveSettingsBySessionIdRef.current[identity.sessionId] ?? null);
    setUnreadBySessionId((prev) => ({ ...prev, [identity.sessionId]: 0 }));
    const key = runtimeKey(identity);
    if (previous !== identity.sessionId) ensureSurface(identity);
    displayedRuntimeKeyRef.current = key;
    setDisplayedRuntimeKey(key);
    // The live viewport seeds from the target slot (empty slot -> empty view)
    // BEFORE the restore, so the first post-activation scroll/toggle refines
    // the authoritative state instead of carrying the previous session's.
    const slot = viewSlotsByRuntimeKeyRef.current.get(key);
    liveViewportsRef.current.set(key, seedLiveViewport(key, {
      follow: slot?.transcriptScroll.follow ?? true,
      anchorItemId: slot?.transcriptScroll.anchorItemId ?? null,
      anchorOffsetPx: slot?.transcriptScroll.anchorOffsetPx ?? 0,
      fallbackScrollTop: slot?.transcriptScroll.fallbackScrollTop ?? 0,
      openFoldKeys: slot?.openFoldKeys ?? new Set<string>(),
    }).viewport);
    if (restoreView) restoreDisplayedView(identity);
    publishLifecycle(displayStateRef.current);
  }, [ensureSurface, publishLifecycle, restoreDisplayedView]);
  /** New/fork/clone consume their full-snapshot result through the frozen
   *  cross-node contract, validated BEFORE any mutation: data.sessionId,
   *  state.sessionId, session.sessionId, runtimeIncarnation, cwd,
   *  path/sessionFile, and source/status/loaded/incarnation must ALL agree
   *  (parseLifecycleResult) or the result rejects with nothing mutated — no
   *  activation, no overlay row, no catalog bump. Negotiated switch handoffs
   *  are rejected here and must flow through requestSessionSwitch. */
  const onLifecycleResult = useCallback(
    (result: unknown): Promise<unknown> => {
      const d = (result || {}) as {
        messages?: unknown;
        notice?: unknown;
        editorText?: unknown;
        lifecycle?: { activeTurnId?: unknown };
        subscriptionRequired?: unknown;
      };
      if (d.subscriptionRequired === true) {
        return Promise.reject(new Error('negotiated handoff must use requestSessionSwitch'));
      }
      const target = parseLifecycleResult(result);
      const targetSession = target.sessionId;
      const identity = { sessionId: target.sessionId, runtimeIncarnation: target.runtimeIncarnation };
      const previousSession = sessionIdRef.current;
      // The SOURCE view (composer draft + attachments + scroll/fold state) is
      // saved into its slot BEFORE any mutation of the live composer or DOM:
      // the source session keeps its exact view across the lifecycle cut, and
      // a later switch back restores it.
      const previousRuntime = activatedRuntimeRef.current;
      if (previousRuntime) saveDisplayedView(previousRuntime);
      if (previousSession && previousSession !== targetSession) cancelTextRendersForSession(previousSession);
      resetComposerAttachments();
      // Child composer: New/Clone start EMPTY (a source draft or attachment
      // never leaks into the fresh target); a Fork's `editorText` (the
      // selected user prompt) rides ONLY in the child.
      const childDraft = typeof d.editorText === 'string' ? d.editorText : '';
      const childInput = promptInputRef.current;
      if (childInput) {
        childInput.value = childDraft;
        childInput.dispatchEvent(new Event('input', { bubbles: true }));
        childInput.setSelectionRange(childDraft.length, childDraft.length);
        if (childDraft !== '') childInput.focus();
      }
      // Fresh EMPTY target view slot: a lifecycle cut opens a NEW view — the
      // child composer/attachments start empty (fork carries only its
      // editorText draft), folds closed, viewport pinned — never a stale slot
      // from an earlier incarnation of the same session id.
      viewSlotsByRuntimeKeyRef.current = upsertViewSlot(
        viewSlotsByRuntimeKeyRef.current,
        {
          ...emptyViewSlot(identity),
          draft: childDraft,
          selectionStart: childDraft.length,
          selectionEnd: childDraft.length,
          activePanel: activePanelRef.current,
        },
        displayStateRef.current?.displayed ?? identity,
        displayStateRef.current?.pending ?? null,
      );
      const isStreaming = (target.state as { isStreaming?: unknown } | null)?.isStreaming === true;
      const activeTurnId = typeof d.lifecycle?.activeTurnId === 'string' ? d.lifecycle.activeTurnId : '';
      // Activation stays out of applyState: activateRuntime below is the ONE
      // isolated activation path (session id, live settings, unread, view
      // restore, realtime stop). Nested RpcSessionState has no incarnation;
      // install the overlay first so shouldActivate=false apply can match it.
      residentBySessionIdRef.current = { ...residentBySessionIdRef.current, [targetSession]: identity.runtimeIncarnation };
      applyState(stampSessionStateIncarnation(target.state, identity.runtimeIncarnation), targetSession, false);
      if (isStreaming && activeTurnId !== '') {
        updateActivityStripFor(targetSession, (state) => reconcileActivityStrip(state, targetSession, true, activeTurnId));
      }
      if (Array.isArray(d.messages)) {
        if (isStreaming) cancelTextRendersForSession(targetSession);
        else clearStreamStateForSession(targetSession);
        const history = messagesToItems(d.messages);
        updateItemsMirror((prev) => ({
          ...prev,
          [targetSession]: mergeAuthoritativeItems(history, prev[targetSession] ?? [], isStreaming),
        }));
        reconcileTaskJobs(targetSession);
      }
      if (typeof d.notice === 'string' && d.notice.trim() !== '') toast(d.notice, false);
      const preservedReviewOwner = reconnectReviewOwnerRef.current;
      if (activePanel === 'code-review' && (!preservedReviewOwner || targetSession !== preservedReviewOwner)) {
        setActivePanel('');
        setCodeReviewOpenArgs({});
      }
      refreshGoal(targetSession);
      {
        const display = displayStateRef.current;
        const pending = display?.pending ?? null;
        if (pending && sameRuntime(pending.source, identity)) {
          // Lifecycle result belongs to the pending switch's source authority:
          displayStateRef.current = { ...display!, displayed: identity, commandTarget: identity, pending };
          publishLifecycle(displayStateRef.current);
        } else {
          if (pending) {
            // Authority truly changes to a different session: settle the exact
            // pending generation false and clear the switch UI so the caller
            // never hangs on a transaction the new authority supersedes.
            pendingSwitchResolversRef.current.get(pending.generation)?.(false);
            pendingSwitchResolversRef.current.delete(pending.generation);
            pendingSwitchPromisesRef.current.delete(pending.generation);
            setSwitchPending(false);
          }
          displayStateRef.current = {
            displayed: identity,
            commandTarget: identity,
            pending: null,
            nextGeneration: display?.nextGeneration ?? 1,
          };
        }
        publishLifecycle(displayStateRef.current);
        // Install the exact resident overlay + zero cursor BEFORE the
        // subscribe frame: the backend answers the subscription with an
        // authoritative cut for this exact runtime, and the cut handler
        // gates on residentBySessionIdRef matching the cut identity. Without
        // the pre-install, a cut that lands before any later install would
        // be dropped as foreign, leaving the target without its transcript
        // base. The cursor is also installed first so the cut's replay
        // premise (contiguous from 0) holds.
        residentBySessionIdRef.current = { ...residentBySessionIdRef.current, [targetSession]: identity.runtimeIncarnation };
        cursorByRuntimeKeyRef.current.set(runtimeKey(identity), 0);
        subscribeRuntime(identity, 0);
      }
      // THE one isolated activation path: session id, live settings, unread
      // clear, live viewport seed + view restore (from the fresh empty target
      // slot installed above), and realtime stop all happen here and nowhere
      // else in the lifecycle cut.
      activateRuntime(identity, true);
      saveSessionPreference(tokenStorage, hostRef.current, targetSession);
      // Inject the validated authoritative child row BEFORE the catalog
      // revision bump: the sidebar merges the overlay synchronously in the
      // same lifecycle cut, so the active child row paints immediately even
      // while the session_list this bump triggers is in flight (or fails).
      setLifecycleRow(target.session);
      // The lifecycle cut is now fully authoritative: exact identity accepted,
      // state/messages applied, target activated, preference saved, runtime
      // subscribed. Bump exactly once per response so the Sessions sidebar
      // fetches the authoritative catalog including the new row immediately.
      setCatalogRefreshRevision((revision) => revision + 1);
      return Promise.resolve();
    },
    [activePanel, activateRuntime, applyState, publishLifecycle, reconcileTaskJobs, refreshGoal, resetComposerAttachments, saveDisplayedView, subscribeRuntime, toast, updateActivityStripFor, updateItemsFor]
  );

  const settlePendingSwitch = useCallback((generation: number, ok: boolean) => {
    if (authoritativeCutTimerRef.current !== null) {
      window.clearTimeout(authoritativeCutTimerRef.current);
      authoritativeCutTimerRef.current = null;
    }
    if (snapshotInProgressRetryTimerRef.current !== null) {
      window.clearTimeout(snapshotInProgressRetryTimerRef.current);
      snapshotInProgressRetryTimerRef.current = null;
    }
    pendingSwitchResolversRef.current.get(generation)?.(ok);
    pendingSwitchResolversRef.current.delete(generation);
    pendingSwitchPromisesRef.current.delete(generation);
  }, []);

  const rollbackPendingSwitch = useCallback((generation: number): boolean => {
    const current = displayStateRef.current;
    if (!current?.pending || current.pending.generation !== generation) return false;
    const source = current.pending.source;
    displayStateRef.current = failSessionSwitch(current, generation);
    publishLifecycle(displayStateRef.current);
    // Exact activated identity is the rollback test seam: a cold pending switch
    // never left source, so restoring its slot would overwrite live DOM edits.
    if (!sameRuntime(activatedRuntimeRef.current, source)) {
      activateRuntime(source, true);
      saveSessionPreference(tokenStorage, hostRef.current, source.sessionId);
    }
    settlePendingSwitch(generation, false);
    setSwitchPending(false);
    return true;
  }, [activateRuntime, publishLifecycle, settlePendingSwitch]);

  const invalidateRuntime = useCallback((identity: RuntimeIdentity) => {
    const key = runtimeKey(identity);
    if (sameRuntime(activatedRuntimeRef.current, identity)) {
      if (restoreViewRafRef.current !== null) window.cancelAnimationFrame(restoreViewRafRef.current);
      restoreViewRafRef.current = null;
      restoreViewGenerationRef.current += 1;
    }
    const ownsSessionState = residentBySessionIdRef.current[identity.sessionId] === identity.runtimeIncarnation;
    if (ownsSessionState) {
      const next = { ...residentBySessionIdRef.current };
      delete next[identity.sessionId];
      residentBySessionIdRef.current = next;
    }
    if (sameRuntime(readyBySessionIdRef.current[identity.sessionId]?.identity, identity)) {
      const next = { ...readyBySessionIdRef.current };
      delete next[identity.sessionId];
      readyBySessionIdRef.current = next;
    }
    cursorByRuntimeKeyRef.current.delete(key);
    viewSlotsByRuntimeKeyRef.current.delete(key);
    removeSurface(key);
    if (!ownsSessionState) return;
    loadedSessionIdsRef.current.delete(identity.sessionId);
    clearStreamStateForSession(identity.sessionId);
    delete activeAssistantBySessionIdRef.current[identity.sessionId];
    delete optimisticQueueBySessionIdRef.current[identity.sessionId];
    delete steeringLiveBySessionIdRef.current[identity.sessionId];
    markStreamingFor(identity.sessionId, false);
  }, [markStreamingFor, removeSurface]);
  const settleSwitchForTransportLoss = useCallback(() => {
    const pending = displayStateRef.current?.pending;
    if (!pending) return;
    if (pending.targetIdentity && !sameRuntime(readyBySessionIdRef.current[pending.targetIdentity.sessionId]?.identity, pending.targetIdentity)) {
      invalidateRuntime(pending.targetIdentity);
    }
    rollbackPendingSwitch(pending.generation);
  }, [invalidateRuntime, rollbackPendingSwitch]);

  const applySwitchActions = useCallback((actions: readonly SwitchAction[]) => {
    for (const action of actions) {
      switch (action.type) {
        case 'invalidate-runtime':
          invalidateRuntime(action.identity);
          break;
        case 'install-cursor':
          ensureSurface(action.identity);
          cursorByRuntimeKeyRef.current.set(runtimeKey(action.identity), action.cursor);
          residentBySessionIdRef.current = { ...residentBySessionIdRef.current, [action.identity.sessionId]: action.identity.runtimeIncarnation };
          break;
        case 'subscribe':
          subscribeRuntime(action.identity, action.cursor);
          break;
        case 'mark-ready':
          cursorByRuntimeKeyRef.current.set(runtimeKey(action.identity), action.cursor);
          readyBySessionIdRef.current = {
            ...readyBySessionIdRef.current,
            [action.identity.sessionId]: {
              identity: action.identity,
              cursor: action.cursor,
              socketGeneration: socketGenRef.current,
            },
          };
          break;
        case 'display-warm':
          activateRuntime(action.identity, true);
          break;
        case 'apply-state':
          applyState(
            stampSessionStateIncarnation(action.state, action.identity.runtimeIncarnation),
            action.identity.sessionId,
            false,
          );
          break;
        case 'apply-messages': {
          const streaming = (streamingBySessionIdRef.current[action.identity.sessionId] ?? false);
          updateItemsFor(action.identity.sessionId, (items) => mergeAuthoritativeItems(
            messagesToItems(action.messages),
            items,
            action.preserveInflight && streaming,
          ));
          reconcileTaskJobs(action.identity.sessionId);
          break;
        }
        case 'replay-events': {
          for (const event of action.events) onEventRef.current(event);
          const replayCursor = acceptedCutLiveCursor(action.events, 0);
          if (action.events.length > 0) {
            cursorByRuntimeKeyRef.current.set(runtimeKey(action.identity), replayCursor);
          }
          break;
        }
        case 'bind-authority':
          cursorByRuntimeKeyRef.current.set(runtimeKey(action.identity), action.cursor);
          readyBySessionIdRef.current = {
            ...readyBySessionIdRef.current,
            [action.identity.sessionId]: {
              identity: action.identity,
              cursor: action.cursor,
              socketGeneration: socketGenRef.current,
            },
          };
          // Test handoff seam: preserve edits made during the awaiting-cut window.
          // Warm success already paints the exact target, so bind authority without
          // replaying its pre-request slot; cold success saves source, then cuts once.
          saveDisplayedView(activatedRuntimeRef.current ?? undefined);
          if (!sameRuntime(activatedRuntimeRef.current, action.identity)) activateRuntime(action.identity, true);
          break;
        case 'save-preference':
          saveSessionPreference(tokenStorage, hostRef.current, action.sessionId);
          break;
        case 'toast-notice':
          toast(action.notice);
          break;
        case 'toast-error':
          toast(action.message, true);
          break;
        case 'fail-pending':
          break;
        case 'resubscribe-zero':
          cursorByRuntimeKeyRef.current.set(runtimeKey(action.identity), 0);
          subscribeRuntime(action.identity, 0);
          break;
      }
    }
  }, [activateRuntime, applyState, ensureSurface, invalidateRuntime, reconcileTaskJobs, saveDisplayedView, subscribeRuntime, toast, updateItemsFor]);

  const requestSessionSwitch = useCallback<RequestSessionSwitch>((target, options) => {
    let generation: number | null = null;
    let settled: Promise<boolean> | null = null;
    try {
      const current = displayStateRef.current;
      if (!current) return Promise.resolve(false);
      // Frontend-only exact-ready fast path: when the catalog tuple is
      // rpi/native and the row's advisory hint exactly equals the client's
      // ready identity (hint + resident overlay + restorable view all agree)
      // on the CURRENT socket generation, the switch completes synchronously
      // with ZERO switch_session/subscribe frames: save the source view,
      // atomically move displayed+commandTarget, restore the cached target
      // view, save the preference, clear unread via activateRuntime, settle
      // any superseded DIFFERENT-target pending false (a same-catalog pending
      // coalesces below instead). Any miss — including stale ready stamps
      // across a reconnect — is fail-closed to the negotiated path below;
      // the ready cache stays authoritative and target messages/state are
      // never mutated here.
      const readyTarget = readyBySessionIdRef.current[target.catalogSessionId];
      const fastPlan = options.origin === 'user' || options.origin === 'bootstrap'
        ? planFastSessionSwitch(current, target, {
          ready: readyTarget,
          residentIncarnation: residentBySessionIdRef.current[target.catalogSessionId] ?? null,
          viewRestorable: readyTarget !== undefined
            && viewSlotsByRuntimeKeyRef.current.has(runtimeKey(readyTarget.identity)),
          socketGeneration: socketGenRef.current,
        })
        : { kind: 'negotiated' as const };
      // Fast eligibility is decided BEFORE any save: the fast path skips the
      // O(transcript) DOM scan and reads the incrementally maintained live
      // viewport state O(1) (./transcriptView.ts) — the scroll handler and
      // fold toggles keep the anchor/follow/fold state current while the
      // session is displayed, so the saved view is the LATEST user state and
      // the return trip restores exactly that. Only a NEGOTIATED switch
      // (which leaves the source for a real RPC round-trip) pays the full
      // DOM scan.
      saveDisplayedView(undefined, fastPlan.kind !== 'fast');
      if (fastPlan.kind === 'fast') {
        displayStateRef.current = fastPlan.state;
        if (fastPlan.supersededGeneration !== null) {
          settlePendingSwitch(fastPlan.supersededGeneration, false);
        }
        setSwitchPending(false);
        if (!sameRuntime(activatedRuntimeRef.current, fastPlan.state.displayed)) {
          activateRuntime(fastPlan.state.displayed, true);
        }
        applySwitchActions(fastPlan.actions);
        publishLifecycle(displayStateRef.current);
        return Promise.resolve(true);
      }
      const begun = beginSessionSwitch(current, target);
      if (begun.coalesced) return pendingSwitchPromisesRef.current.get(begun.generation) ?? Promise.resolve(false);
      if (begun.replacedGeneration !== null) {
        settlePendingSwitch(begun.replacedGeneration, false);
      }
      generation = begun.generation;
      displayStateRef.current = begun.state;
      publishLifecycle(displayStateRef.current);
      if (!sameRuntime(activatedRuntimeRef.current, begun.state.displayed)) activateRuntime(begun.state.displayed, true);
      setSwitchPending(true);
      settled = new Promise<boolean>((resolve) => pendingSwitchResolversRef.current.set(begun.generation, resolve));
      pendingSwitchPromisesRef.current.set(begun.generation, settled);
      void sendCommand(
        buildSwitchCommand(target, sessionIdRef.current || current.commandTarget.sessionId),
        undefined,
        { allowDuringSwitch: true },
      ).then((result) => {
        const handoff = parseSwitchHandoff(result, target);
        const ready = readyBySessionIdRef.current[handoff.sessionId];
        const display = displayStateRef.current;
        if (!display) {
          settlePendingSwitch(begun.generation, false);
          setSwitchPending(false);
          return;
        }
        const liveCursor = cursorByRuntimeKeyRef.current.get(
          runtimeKey({ sessionId: handoff.sessionId, runtimeIncarnation: handoff.runtimeIncarnation }),
        ) ?? 0;
        const plan = acceptSwitchHandoff(display, begun.generation, handoff, ready, liveCursor);
        if (plan.stale) return;
        setLifecycleRow(handoff.session);
        displayStateRef.current = plan.state;
        applySwitchActions(plan.actions);
        publishLifecycle(displayStateRef.current);
        if (plan.actions.some((action) => action.type === 'fail-pending')) {
          settlePendingSwitch(begun.generation, false);
          setSwitchPending(false);
          return;
        }
        if (displayStateRef.current?.pending?.phase === 'awaiting-authoritative-cut') {
          const cutGeneration = begun.generation;
          const cutOrigin = options.origin;
          const cutSocketGeneration = socketGenRef.current;
          const cutIdentity = displayStateRef.current.pending.targetIdentity ?? {
            sessionId: handoff.sessionId,
            runtimeIncarnation: handoff.runtimeIncarnation,
          };
          lastCutProgressAtRef.current = Date.now();
          const armCutTimer = () => {
            if (authoritativeCutTimerRef.current !== null) {
              window.clearTimeout(authoritativeCutTimerRef.current);
            }
            const delay = authoritativeCutTimerDelayMs({
              now: Date.now(),
              lastProgressAt: lastCutProgressAtRef.current,
              timeoutMs: AUTHORITATIVE_CUT_TIMEOUT_MS,
              refillInFlight: refillInFlightRef.current !== null,
            });
            authoritativeCutTimerRef.current = window.setTimeout(() => {
              authoritativeCutTimerRef.current = null;
              if (socketGenRef.current !== cutSocketGeneration) return;
              const pending = displayStateRef.current?.pending;
              if (!pending
                || pending.generation !== cutGeneration
                || pending.phase !== 'awaiting-authoritative-cut'
                || !pending.targetIdentity
                || !sameRuntime(pending.targetIdentity, cutIdentity)) {
                return;
              }
              const tick = planAuthoritativeCutTick({
                now: Date.now(),
                lastProgressAt: lastCutProgressAtRef.current,
                timeoutMs: AUTHORITATIVE_CUT_TIMEOUT_MS,
                refillInFlight: refillInFlightRef.current !== null,
              });
              if (tick === 'hold') {
                armCutTimer();
                return;
              }
              const identity = pending.targetIdentity;
              if (identity && !sameRuntime(readyBySessionIdRef.current[identity.sessionId]?.identity, identity)) {
                invalidateRuntime(identity);
              }
              const rolledBack = rollbackPendingSwitch(cutGeneration);
              if (rolledBack && switchFailureShouldToast(cutOrigin)) {
                toast('switch timed out waiting for subscription snapshot', true);
              }
            }, delay);
          };
          armCutTimer();
        }
      }).catch((error) => {
        const rolledBack = rollbackPendingSwitch(begun.generation);
        // Single toast owner: pending.ts never toasts switch_session; an
        // already-owned transport toast is never duplicated here.
        const alreadyOwned = (error as { toastOwned?: boolean } | null)?.toastOwned === true;
        if (rolledBack && switchFailureShouldToast(options.origin) && !alreadyOwned) {
          const code = (error as PendingCommandError | null)?.rpcCode;
          const detail = error instanceof Error ? error.message : String(error);
          toast(`switch to ${target.catalogSessionId} failed${code ? ` (${code})` : ''}: ${detail}`, true);
        }
      });
      return settled;
    } catch (error) {
      if (generation !== null && settled) {
        // The transaction was already registered; roll it back so the stored
        // promise settles false instead of hanging, and the UI clears.
        rollbackPendingSwitch(generation);
        return settled;
      }
      return Promise.reject(error);
    }
  }, [activateRuntime, applySwitchActions, invalidateRuntime, publishLifecycle, rollbackPendingSwitch, saveDisplayedView, sendCommand, settlePendingSwitch, toast]);

  /** Hand off to a NEW session (omp session.handoff parity): the backend
   *  renders the handoff envelope, creates a session seeded with the
   *  <handoff-context> doc as its first record, and answers with the SAME
   *  subscribe-handoff-v1 envelope the catalog switch consumes. The new
   *  session becomes the sole authority (display + commandTarget), the
   *  authoritative cut rides the subscription at cursor 0, and the catalog
   *  refresh picks up the new row. Streaming refusals surface exactly one
   *  error toast (the backend refuses; the composer draft is preserved). */
  const handoffPendingRef = useRef(false);
  const requestSessionHandoff = useCallback((customInstructions: string | null): boolean => {
    if (handoffPendingRef.current) return false;
    const sourceSessionId = sessionIdRef.current;
    if (!sourceSessionId) return false;
    handoffPendingRef.current = true;
    setSwitchPending(true);
    const command: Record<string, unknown> = {
      type: 'session_handoff',
      responseMode: SWITCH_HANDOFF_RESPONSE_MODE,
      sessionId: sourceSessionId,
    };
    if (customInstructions !== null && customInstructions !== '') {
      command.customInstructions = customInstructions;
    }
    void sendCommand(command)
      .then((result) => {
        const envelope = parseSessionHandoffEnvelope(result);
        const identity = { sessionId: envelope.sessionId, runtimeIncarnation: envelope.runtimeIncarnation };
        // Authority cut to the NEW session — mirrors onLifecycleResult's
        // else branch: any coincident in-flight switch transaction settles
        // false, then display + commandTarget bind to the handoff target.
        const pending = displayStateRef.current?.pending ?? null;
        if (pending) {
          pendingSwitchResolversRef.current.get(pending.generation)?.(false);
          pendingSwitchResolversRef.current.delete(pending.generation);
          pendingSwitchPromisesRef.current.delete(pending.generation);
        }
        // The handoff gate clears UNCONDITIONALLY here: the authority cut
        // released the composer/switch machinery, so a lingering gate would
        // skeleton-gate the UI until reload (P1-2).
        setSwitchPending(false);
        displayStateRef.current = {
          displayed: identity,
          commandTarget: identity,
          pending: null,
          nextGeneration: (displayStateRef.current?.nextGeneration ?? 0) + 1,
        };
        publishLifecycle(displayStateRef.current);
        residentBySessionIdRef.current = {
          ...residentBySessionIdRef.current,
          [envelope.sessionId]: envelope.runtimeIncarnation,
        };
        cursorByRuntimeKeyRef.current.set(runtimeKey(identity), 0);
        subscribeRuntime(identity, 0);
        if (!sameRuntime(activatedRuntimeRef.current, identity)) activateRuntime(identity, true);
        saveSessionPreference(tokenStorage, hostRef.current, envelope.sessionId);
        refreshGoal(envelope.sessionId);
        refreshContextUsage(envelope.sessionId);
        // The seeded session has content records: the catalog must list it.
        setCatalogRefreshRevision((revision) => revision + 1);
      })
      .catch((error) => {
        const message = error instanceof Error ? error.message : String(error);
        toast(`session handoff failed: ${message}`, true);
      })
      .finally(() => {
        handoffPendingRef.current = false;
        // Refusal path: no authority cut happened, but the handoff is over —
        // the composer/switch machinery must never stay gated (P1-2).
        setSwitchPending(false);
      });
    return true;
  }, [publishLifecycle, refreshContextUsage, refreshGoal, sendCommand, setCatalogRefreshRevision, subscribeRuntime, toast]);

  const onSessionHandoff = useCallback((customInstructions: string | null): boolean => {
    if (!requestSessionHandoff(customInstructions)) {
      toast('a session handoff is already in progress', true);
      return false;
    }
    return true;
  }, [requestSessionHandoff, toast]);

  /* ---------------- connection ---------------- */
  const scheduleReconnect = useCallback(() => {
    if (retryTimerRef.current !== null) window.clearTimeout(retryTimerRef.current);
    if (tokenHoldRef.current) {
      // token_required hold: the preflight proved a token is required and none
      // is stored. Never arm the silent retry loop — the Settings →
      // Connection panel owns recovery via handleTokenChange. LIMBO GUARD
      // (hold re-probe): the listener may have restarted TOKENLESS — re-probe
      // /status on a bounded cadence so the hold releases itself instead of
      // pinning the page in Settings forever.
      if (holdReprobeTimerRef.current === null) {
        holdReprobeTimerRef.current = window.setTimeout(() => {
          holdReprobeTimerRef.current = null;
          bootProbeHandoffRef.current();
        }, TOKEN_HOLD_REPROBE_MS);
      }
      setConnState('token_required');
      return;
    }
    // LIMBO GUARD (never-opened re-probe): after repeated reconnect attempts
    // whose socket closed BEFORE onopen, re-run the /status preflight — a
    // listener restart can flip a tokenless-accepted endpoint into
    // token-required mode, and the refused handshakes would otherwise loop
    // silently forever (the page connected once, so the ever-connected
    // close surface is suppressed).
    if (shouldReprobeTokenRequirement(neverOpenedFailuresRef.current)) {
      neverOpenedFailuresRef.current = 0;
      bootProbeHandoffRef.current();
      return;
    }
    const delay = delayRef.current;
    delayRef.current = nextReconnectDelay(delayRef.current);
    setConnState('reconnecting');
    retryTimerRef.current = window.setTimeout(() => {
      retryTimerRef.current = null;
      connectRef.current();
    }, delay);
  }, []);

  const closeForHeartbeat = useCallback((reason: string, opts?: { reconnect?: 'scheduled' | 'immediate' }) => {
    // Hidden-tab timers are not liveness. Keep the still-OPEN socket;
    // visibilitychange/pageshow/online will ping or reconnect.
    if (!heartbeatMayRetire(document.visibilityState === 'visible')) return;
    const ws = wsRef.current;
    if (!ws) return;
    // Heartbeat failure must not wait for WebSocket.onclose. A dead TLS
    // peer after listener restart can sit in OPEN/CLOSING without ever
    // delivering close. Retire this generation locally, then best-effort close.
    detachTransportHandlers(ws);
    wsRef.current = null;
    clearHeartbeatTimers();
    setConnState('reconnecting');
    noteStreamingSocketDrop();
    const closedReason = `connection closed (code 4000): ${reason}`;
    const toastOwned = toastTransportOutage(`${closedReason}, reconnecting…`)
      || transportOutageActiveRef.current;
    if (toastOwned) transportOutageActiveRef.current = true;
    rejectPendingExcept(++socketGenRef.current, closedReason, false, toastOwned);
    settleSwitchForTransportLoss();
    try {
      ws.close(4000, reason);
    } catch {
      /* already closed */
    }
    delayRef.current = RECONNECT_INITIAL_DELAY_MS;
    if (opts?.reconnect === 'immediate') return;
    window.setTimeout(() => scheduleReconnect(), 0);
  }, [noteStreamingSocketDrop]);

  /** Restart the silence window from the last received message. */
  const rescheduleSilenceTimer = useCallback(() => {
    if (silenceTimerRef.current !== null) window.clearTimeout(silenceTimerRef.current);
    const remaining = Math.max(HEARTBEAT_TIMEOUT_MS - (Date.now() - lastMessageAtRef.current), 1);
    silenceTimerRef.current = window.setTimeout(() => {
      silenceTimerRef.current = null;
      if (shouldCloseForSilence(lastMessageAtRef.current, Date.now())) {
        closeForHeartbeat('heartbeat timeout');
        return;
      }
      rescheduleSilenceTimer();
    }, remaining);
  }, [closeForHeartbeat]);

  const armPongDeadline = useCallback(() => {
    if (pongTimerRef.current !== null) window.clearTimeout(pongTimerRef.current);
    pongTimerRef.current = window.setTimeout(() => {
      pongTimerRef.current = null;
      if (shouldCloseForMissedPong(pingSentAtRef.current, Date.now(), lastMessageAtRef.current)) {
        closeForHeartbeat('heartbeat timeout');
      }
    }, HEARTBEAT_PONG_DEADLINE_MS);
  }, [closeForHeartbeat]);

  const sendHeartbeatPing = useCallback(() => {
    const ws = wsRef.current;
    const plan = planHeartbeatPing(ws ? ws.readyState : null, pingSentAtRef.current, Date.now(), lastMessageAtRef.current);
    if (plan === 'idle' || plan === 'wait-pong') return;
    if (plan === 'retire') {
      closeForHeartbeat('heartbeat timeout');
      return;
    }
    if (!ws) return;
    try {
      ws.send(HEARTBEAT_PING_FRAME);
    } catch {
      closeForHeartbeat('heartbeat timeout');
      return;
    }
    pingSentAtRef.current = Date.now();
    armPongDeadline();
  }, [armPongDeadline, closeForHeartbeat]);

  /** Stop every heartbeat/stability timer (socket closed or being replaced). */
  const clearHeartbeatTimers = useCallback(() => {
    if (pingTimerRef.current !== null) window.clearInterval(pingTimerRef.current);
    pingTimerRef.current = null;
    if (pongTimerRef.current !== null) window.clearTimeout(pongTimerRef.current);
    pongTimerRef.current = null;
    pingSentAtRef.current = null;
    if (silenceTimerRef.current !== null) window.clearTimeout(silenceTimerRef.current);
    silenceTimerRef.current = null;
    if (handshakeTimerRef.current !== null) window.clearTimeout(handshakeTimerRef.current);
    handshakeTimerRef.current = null;
    handshakeStartedAtRef.current = null;
    if (stabilityTimerRef.current !== null) window.clearTimeout(stabilityTimerRef.current);
    stabilityTimerRef.current = null;
  }, []);

  const onOpen = useCallback(() => {
    // Heartbeat: ping immediately, then on the cadence. A missed pong
    // deadline closes the socket; inbound frames still slide the silence
    // window as a backstop for a fully frozen peer.
    everConnectedRef.current = true;
    // The bootstrap window: RPC failures stay silent at the global layer
    // (pending.ts rpcFailureToastOwned reads `boot`); auto-load waiters are
    // gated separately by the ReadyGate, which resolves only after this
    // bootstrap chain binds the session and restores the preference.
    bootRef.current = true;
    lastMessageAtRef.current = Date.now();
    pingSentAtRef.current = null;
    if (handshakeTimerRef.current !== null) window.clearTimeout(handshakeTimerRef.current);
    handshakeTimerRef.current = null;
    handshakeStartedAtRef.current = null;
    rescheduleSilenceTimer();
    sendHeartbeatPing();
    if (pingTimerRef.current === null) {
      pingTimerRef.current = window.setInterval(() => {
        sendHeartbeatPing();
      }, HEARTBEAT_PING_INTERVAL_MS);
    }
    // The reconnect backoff resets ONLY after the connection has proven
    // stable for >5s; a connection that dies inside the window keeps its
    // accumulated delay so flapping connections keep backing off.
    stabilityTimerRef.current = window.setTimeout(() => {
      stabilityTimerRef.current = null;
      const ws = wsRef.current;
      if (ws && ws.readyState === WebSocket.OPEN) {
        delayRef.current = RECONNECT_INITIAL_DELAY_MS;
      }
    }, HEARTBEAT_STABILITY_MS);
    // Capture THIS socket's generation; every bootstrap continuation checks
    // `alive()` so a socket superseded mid-bootstrap (A dropped, B reconnected
    // healthy) bails via STALE_ABORT / `!alive()` and can never applyState /
    // setConnState / refreshGoal / scheduleReconnect on B's behalf. Its pending
    // commands were already rejected by connect()'s replace drain, so the
    // rejection flowing here is caught and discarded, not retried.
    const gen = socketGenRef.current;
    const alive = () => socketGenRef.current === gen;
    // Capture the ACTUAL mounted review owner: the panel is mounted under the
    // DISPLAY authority, not the active ref — a prior reconnect can leave the
    // active ref on the listener primary while the review belongs to the
    // displayed session, and capturing the ref would both close a valid review
    // and record the wrong owner. The post-bootstrap check below compares the
    // displayed authority AFTER the rebind against this, so the panel survives
    // a reconnect that still displays its owner (its own connectionEpoch probe
    // reconciles the controller) and closes only when the bootstrap genuinely
    // moved the display off it.
    const reviewOwnerAtConnect = displayStateRef.current?.displayed.sessionId ?? sessionIdRef.current;
    reconnectReviewOwnerRef.current = activePanelRef.current === 'code-review'
      ? reviewOwnerAtConnect
      : null;

    // A process restart may recover a different primary before reopening the
    // page's previously active runtime. Preserve the rendered session/cache,
    // but clear command routing so this generation's first command is the one
    // id-less get_state allowed to discover the new listener primary. Stale
    // subscriptions remain deferred until session_list confirms them loaded.
    const reconnectPlan = planReconnectBootstrap(
      Object.keys(residentBySessionIdRef.current),
      Object.keys(readyBySessionIdRef.current),
      sessionIdRef.current,
    );
    sessionIdRef.current = null;
    // Captured BEFORE the id-less get_state: display authority is either
    // already bound (reconnect — the displayed surface stays) or this chain
    // performs the COLD first bind below.
    const coldBind = displayStateRef.current === null;
    // get_messages closes the settlement race: settled sessions replace the
    // transcript outright, while a still-running session keeps its server-id
    // streaming assistant so later deltas/message_end remain routable.
    sendCommand(reconnectPlan.firstCommand)
      .then((state) => {
        if (!alive()) throw STALE_ABORT;
        const bindSid = state && typeof state === 'object' && typeof (state as { sessionId?: unknown }).sessionId === 'string'
          ? (state as { sessionId: string }).sessionId
          : '';
        if (bindSid !== '') refillInFlightRef.current = bindSid;
        applyState(state);
        const target = sessionIdRef.current;
        if (!target) throw new Error('state response did not bind a session');
        const primaryIncarnation = residentBySessionIdRef.current[target];
        if (!primaryIncarnation) throw new Error('state response missing runtime incarnation');
        const primaryIdentity = { sessionId: target, runtimeIncarnation: primaryIncarnation };
        const displayed = displayStateRef.current;
        const displayedNeedsRebind = displayed !== null
          && displayed.displayed.sessionId === primaryIdentity.sessionId
          && displayed.displayed.runtimeIncarnation !== primaryIdentity.runtimeIncarnation;
        if (coldBind || displayedNeedsRebind) {
          const previousDisplayed = displayed?.displayed ?? null;
          if (displayedNeedsRebind) {
            const pendingTargetsPrevious = displayed.pending !== null
              && sameRuntime(displayed.pending.targetIdentity, previousDisplayed);
            displayStateRef.current = {
              displayed: primaryIdentity,
              commandTarget: displayed.commandTarget.sessionId === primaryIdentity.sessionId
                ? primaryIdentity
                : displayed.commandTarget,
              pending: pendingTargetsPrevious ? null : displayed.pending,
              nextGeneration: displayed.nextGeneration,
            };
          } else if (displayStateRef.current === null) {
            displayStateRef.current = {
              displayed: primaryIdentity,
              commandTarget: primaryIdentity,
              pending: null,
              nextGeneration: 1,
            };
          }
          activateRuntime(primaryIdentity, true);
          if (previousDisplayed && !sameRuntime(previousDisplayed, primaryIdentity)) {
            invalidateRuntime(previousDisplayed);
          }
        }
        const catchup = planReconnectDisplayedCatchup(
          primaryIdentity,
          displayStateRef.current?.displayed ?? primaryIdentity,
        );
        for (const identity of catchup.subscribe) {
          residentBySessionIdRef.current = {
            ...residentBySessionIdRef.current,
            [identity.sessionId]: identity.runtimeIncarnation,
          };
          const cursor = reconnectSubscribeCursor(cursorByRuntimeKeyRef.current.get(runtimeKey(identity)));
          cursorByRuntimeKeyRef.current.set(runtimeKey(identity), cursor);
          subscribeRuntime(identity, cursor);
        }
        const refillSessionId = catchup.refillSessionId;
        const refillIncarnation = residentBySessionIdRef.current[refillSessionId];
        if (!refillIncarnation) throw new Error('refill lost runtime identity');
        let refillHold: ReconnectRefillHold = {
          sessionId: refillSessionId,
          runtimeIncarnation: refillIncarnation,
          socketGeneration: gen,
        };
        const requireRefillOwned = () => {
          refillHold = adoptReconnectRefillHold(
            refillHold,
            refillHold.sessionId,
            residentBySessionIdRef.current[refillHold.sessionId],
            socketGenRef.current,
          );
          if (!reconnectRefillStillOwned(
            refillHold,
            residentBySessionIdRef.current[refillHold.sessionId],
            socketGenRef.current,
          )) {
            throw STALE_ABORT;
          }
        };
        const adoptRefillFromWire = (wireIncarnation: unknown) => {
          if (typeof wireIncarnation !== 'string' || wireIncarnation === '') return;
          refillHold = adoptReconnectRefillHold(
            refillHold,
            refillHold.sessionId,
            wireIncarnation,
            socketGenRef.current,
          );
          const previous = residentBySessionIdRef.current[refillHold.sessionId];
          if (previous && previous !== refillHold.runtimeIncarnation) {
            invalidateRuntime({ sessionId: refillHold.sessionId, runtimeIncarnation: previous });
          }
          residentBySessionIdRef.current = {
            ...residentBySessionIdRef.current,
            [refillHold.sessionId]: refillHold.runtimeIncarnation,
          };
        };
        const refillSlot = refillSlotGenerationRef.current + 1;
        refillSlotGenerationRef.current = refillSlot;
        refillInFlightRef.current = null;
        lastCutProgressAtRef.current = Date.now();
        const refillOpts = { allowDuringSwitch: true as const };
        const refillWork = () => {
          const displayed = displayStateRef.current?.displayed;
          const displayedIncarnation = displayed?.sessionId === refillSessionId
            ? displayed.runtimeIncarnation
            : undefined;
          refillHold = adoptReconnectRefillHold(
            refillHold,
            refillSessionId,
            displayedIncarnation ?? residentBySessionIdRef.current[refillSessionId],
            gen,
          );
          if (!refillHold.runtimeIncarnation) {
            return Promise.reject(new Error('refill lost runtime identity'));
          }
          const previousResident = residentBySessionIdRef.current[refillSessionId];
          if (previousResident && previousResident !== refillHold.runtimeIncarnation) {
            invalidateRuntime({ sessionId: refillSessionId, runtimeIncarnation: previousResident });
          }
          residentBySessionIdRef.current = {
            ...residentBySessionIdRef.current,
            [refillSessionId]: refillHold.runtimeIncarnation,
          };
          refillInFlightRef.current = refillSessionId;
          lastCutProgressAtRef.current = Date.now();
          return sendCommand({ type: 'get_messages', sessionId: refillSessionId }, undefined, refillOpts).then((data) => {
          requireRefillOwned();
          if (!data || typeof data !== 'object' || !('messages' in data) || !Array.isArray(data.messages)) {
            throw new Error('messages response missing messages');
          }
          const messages = data.messages;
          return sendCommand({ type: 'get_state', sessionId: refillSessionId }, undefined, refillOpts).then((latestState) => {
            requireRefillOwned();
            const latest = (latestState || {}) as { isStreaming?: boolean; activeTurnId?: string | null; runtimeIncarnation?: unknown };
            adoptRefillFromWire(latest.runtimeIncarnation);
            if (planReconnectRefillStep({
              hold: refillHold,
              residentIncarnation: residentBySessionIdRef.current[refillHold.sessionId],
              socketGenerationNow: socketGenRef.current,
              wireIncarnation: latest.runtimeIncarnation,
              displayedSessionId: sessionIdRef.current,
            }) === 'reject') {
              throw STALE_ABORT;
            }
            cancelTextRendersForSession(refillSessionId);
            if (!applyState(latestState, refillSessionId, false)) throw STALE_ABORT;
            markStreamingFor(refillSessionId, latest.isStreaming === true);
            if (latest.isStreaming === true) {
              updateItemsMirror((prev) => ({
                ...prev,
                [refillSessionId]: mergeAuthoritativeItems(messagesToItems(messages), prev[refillSessionId] ?? []),
              }));
              const jobListCut = taskJobIds(itemsBySessionIdRef.current[refillSessionId] ?? []);
              return sendCommand({ type: 'job_list', sessionId: refillSessionId }, undefined, refillOpts).then((jobData) => {
                requireRefillOwned();
                updateItemsMirror((prev) => {
                  const history = messagesToItems(messages);
                  const live = prev[refillSessionId] ?? [];
                  const merged = mergeAuthoritativeItems(history, live);
                  const jobs = jobsFromJobList(jobData);
                  const next = applyJobSnapshot(merged, jobs, jobListCut);
                  const turn = planReconnectTurn({
                    isStreaming: true,
                    liveItems: next,
                    activeAssistantId: activeAssistantBySessionIdRef.current[refillSessionId],
                  });
                  if (turn.kind === 'streaming' && turn.assistantId !== null) {
                    activeAssistantBySessionIdRef.current[refillSessionId] = turn.assistantId;
                  } else {
                    delete activeAssistantBySessionIdRef.current[refillSessionId];
                  }
                  return { ...prev, [refillSessionId]: next };
                });
                delete optimisticQueueBySessionIdRef.current[refillSessionId];
              });
            }
            return sendCommand({ type: 'get_messages', sessionId: refillSessionId }, undefined, refillOpts).then((settledData) => {
              requireRefillOwned();
              if (!settledData || typeof settledData !== 'object' || !('messages' in settledData) || !Array.isArray(settledData.messages)) {
                throw new Error('settled messages response missing messages');
              }
              clearStreamStateForSession(refillSessionId);
              const settledJobListCut = (() => {
                updateItemsMirror((prev) => ({
                  ...prev,
                  [refillSessionId]: mergeAuthoritativeItems(messagesToItems(settledData.messages), prev[refillSessionId] ?? [], false),
                }));
                return taskJobIds(itemsBySessionIdRef.current[refillSessionId] ?? []);
              })();
              return sendCommand({ type: 'job_list', sessionId: refillSessionId }, undefined, refillOpts).then((jobData) => {
                requireRefillOwned();
                updateItemsMirror((prev) => {
                  const history = messagesToItems(settledData.messages);
                  const merged = mergeAuthoritativeItems(history, prev[refillSessionId] ?? [], false);
                  const jobs = jobsFromJobList(jobData);
                  return { ...prev, [refillSessionId]: applyJobSnapshot(merged, jobs, settledJobListCut) };
                });
                delete optimisticQueueBySessionIdRef.current[refillSessionId];
                delete activeAssistantBySessionIdRef.current[refillSessionId];
              });
            });
          });
        }).catch((error) => {
          requireRefillOwned();
          applyReconnectRefillCatch(error, STALE_ABORT, toast);
          return withBootstrapRetry(
            () => sendCommand({ type: 'get_state', sessionId: refillSessionId }, undefined, refillOpts).then((recoveryState) => {
              requireRefillOwned();
              const recovery = (recoveryState || {}) as { isStreaming?: boolean; runtimeIncarnation?: unknown };
              adoptRefillFromWire(recovery.runtimeIncarnation);
              if (planReconnectRefillStep({
                hold: refillHold,
                residentIncarnation: residentBySessionIdRef.current[refillHold.sessionId],
                socketGenerationNow: socketGenRef.current,
                wireIncarnation: recovery.runtimeIncarnation,
                displayedSessionId: sessionIdRef.current,
              }) === 'reject') {
                throw STALE_ABORT;
              }
              if (!applyState(recoveryState, refillSessionId, false)) throw STALE_ABORT;
              markStreamingFor(refillSessionId, recovery.isStreaming === true);
            }),
            {
              isAbort: (error) => error === STALE_ABORT,
              wait: waitReconnectRetry,
            },
          );
        }).finally(() => {
          if (!reconnectRefillFinallyApplies(refillSlot, refillSlotGenerationRef.current)) return;
          refillInFlightRef.current = null;
          lastCutProgressAtRef.current = Date.now();
          if (streamingBySessionIdRef.current[refillSessionId] !== true) {
            noteStreamStall(refillSessionId, { type: 'refill_finished' });
          }
        });
        };
        const restore = withBootstrapRetry(
          () => sendCommand({ type: 'session_list', scope: 'all_projects', sessionId: sessionIdRef.current })
          .then((data) => {
            if (!alive()) throw STALE_ABORT;
            const list = data && typeof data === 'object' ? data : null;
            const wireSessions = list !== null && 'sessions' in list ? list.sessions : undefined;
            const rows: ReconnectCatalogRow[] = Array.isArray(wireSessions) ? wireSessions : [];
            const saved = loadSessionPreference(tokenStorage, hostRef.current);
            const primarySessionId = sessionIdRef.current;
            if (!primarySessionId) throw new Error('catalog restore lost primary session');
            let bootstrapSwitchOk = true;
            let retryTarget: ReturnType<typeof buildRestoreSwitchTarget> = null;
            return restoreReconnectCatalog({
              rows,
              savedSessionId: saved,
              primarySessionId,
              displayedSessionId: displayStateRef.current?.displayed?.sessionId,
              displayedRuntimeIncarnation: displayStateRef.current?.displayed?.runtimeIncarnation,
              switchSession: (target) => {
                const switchTarget = buildRestoreSwitchTarget(target);
                if (!switchTarget) return Promise.resolve(null);
                retryTarget = switchTarget;
                return requestSessionSwitch(switchTarget, { origin: 'bootstrap' })
                  .then((ok) => {
                    if (!alive()) throw STALE_ABORT;
                    bootstrapSwitchOk = ok;
                    if (!ok) return null;
                    return displayStateRef.current?.commandTarget.sessionId ?? null;
                  });
              },
              retireResident: ({ authoritative, omissionSafe }) => {
                if (!alive()) throw STALE_ABORT;
                const displayedSessionId = displayStateRef.current?.displayed?.sessionId ?? null;
                for (const [sessionId, incarnation] of Object.entries(residentBySessionIdRef.current)) {
                  if (sessionId === primarySessionId) continue;
                  if (sessionId === displayedSessionId) continue;
                  const authoritativeIncarnation = authoritative.get(sessionId);
                  if (authoritativeIncarnation === incarnation) continue;
                  if (authoritativeIncarnation === undefined && !omissionSafe) continue;
                  invalidateRuntime({ sessionId, runtimeIncarnation: incarnation });
                }
              },
              subscribeSession: (identity) => {
                if (!alive()) throw STALE_ABORT;
                const previous = residentBySessionIdRef.current[identity.sessionId];
                if (previous && previous !== identity.runtimeIncarnation) {
                  invalidateRuntime({ sessionId: identity.sessionId, runtimeIncarnation: previous });
                }
                const rebind = planDisplayedIncarnationRebind(displayStateRef.current, identity);
                if (rebind) {
                  const pending = displayStateRef.current?.pending ?? null;
                  const nextGeneration = displayStateRef.current?.nextGeneration ?? 1;
                  displayStateRef.current = { ...rebind, pending, nextGeneration };
                  activateRuntime(identity, true);
                }
                residentBySessionIdRef.current = {
                  ...residentBySessionIdRef.current,
                  [identity.sessionId]: identity.runtimeIncarnation,
                };
                const key = runtimeKey(identity);
                if (!viewSlotsByRuntimeKeyRef.current.has(key)) {
                  viewSlotsByRuntimeKeyRef.current = upsertViewSlot(
                    viewSlotsByRuntimeKeyRef.current,
                    emptyViewSlot(identity),
                    displayStateRef.current?.displayed ?? identity,
                    displayStateRef.current?.pending ?? null,
                  );
                }
                ensureSurface(identity);
                subscribeRuntime(identity, reconnectSubscribeCursor(cursorByRuntimeKeyRef.current.get(key)));
              },
            }).then((target) => {
              if (!alive()) throw STALE_ABORT;
              if (shouldPersistPrimaryPreference(saved, target)) {
                saveSessionPreference(tokenStorage, hostRef.current, primarySessionId);
              }
              return {
                retry: bootstrapSavedSwitchNeedsRetry(saved, bootstrapSwitchOk),
                retryTarget,
              };
            });
          }),
          {
            isAbort: (error) => error === STALE_ABORT,
            wait: waitReconnectRetry,
          },
        );
        return restore.then((restoreResult) => {
          if (!alive()) throw STALE_ABORT;
          const displayed = displayStateRef.current?.displayed;
          if (planReconnectDisplayedResync({
            boundSessionId: sessionIdRef.current,
            displayedSessionId: displayed?.sessionId,
          }) === 'resync' && displayed) {
            sessionIdRef.current = displayed.sessionId;
            setSessionId(displayed.sessionId);
            setLiveSettings(liveSettingsBySessionIdRef.current[displayed.sessionId] ?? null);
          }
          const reviewOwner = reconnectReviewOwnerRef.current;
          // Decide preservation against the DISPLAYED authority as it stands
          // after the bootstrap, not the rebound active ref (which a reconnect
          // points at the listener primary while the user still looks at the
          // review's session). Unchanged displayed owner → the review panel
          // stays mounted and its epoch probe reconciles; a genuinely lost or
          // changed owner → close it and drop its revisions.
          if (reviewOwner && (displayStateRef.current?.displayed.sessionId ?? null) !== reviewOwner) {
            setActivePanel('');
            setCodeReviewOpenArgs({});
          }
          reconnectReviewOwnerRef.current = null;
          const afterSwitch = restoreResult && restoreResult.retry && restoreResult.retryTarget
            ? requestSessionSwitch(restoreResult.retryTarget, { origin: 'bootstrap' }).then(() => undefined)
            : Promise.resolve(undefined);
          return afterSwitch.then(() => refillWork());
        })
          .then(async () => {
            if (!alive()) throw STALE_ABORT;
            setConnectionEpoch((epoch) => epoch + 1);
            await refreshGoal(sessionIdRef.current);
            try {
              const data = await sendCommand({ type: 'get_commands' });
              rememberExtensionCatalog(normalizeCommands(data));
            } catch (error: unknown) {
              if (!alive()) throw STALE_ABORT;
              const message = error instanceof Error ? error.message : String(error);
              toast(`commands failed: ${message}`, true);
            }
            if (!alive()) throw STALE_ABORT;
            transportOutageActiveRef.current = false;
            delayRef.current = RECONNECT_INITIAL_DELAY_MS;
            bootRef.current = false;
            readyGate.markReady();
            setConnState('on');
            setToasts((prev) => prev.filter((entry) => entry.kind !== 'transport'));
          })
      })
      .catch((err) => {
        if (alive() && refillInFlightRef.current) refillInFlightRef.current = null;
        // Stale socket (superseded, or its pending was drained on replace):
        // the current generation owns recovery. A live bootstrap failure must
        // close its own OPEN transport before scheduling the next generation;
        // otherwise the stability timer can reset backoff while a poisoned
        // socket remains current and its late close can cancel the retry.
        if (!shouldScheduleReconnect(err, alive())) return;
        bootRef.current = false;
        clearHeartbeatTimers();
        const failed = wsRef.current;
        if (failed) {
          detachTransportHandlers(failed);
          wsRef.current = null;
          try {
            failed.close(4000, 'bootstrap failed');
          } catch {
            /* already closed */
          }
        }
        rejectPendingExcept(++socketGenRef.current, 'bootstrap failed', true, false);
        scheduleReconnect();
      });
  }, [activateRuntime, applyState, clearHeartbeatTimers, invalidateRuntime, markStreamingFor, noteStreamingSocketDrop, readyGate, refreshGoal, rejectPendingExcept, requestSessionSwitch, rescheduleSilenceTimer, scheduleReconnect, sendCommand, sendHeartbeatPing, subscribeRuntime]);

  const connect = useCallback(() => {
    // A new transport generation is not authoritative-ready until its
    // bootstrap continuation binds/restores the active session.
    // Test handoff seam: replacement is transport loss too. Settle an
    // awaiting-cut switch exactly once before the old close is detached and
    // before the new bootstrap can be blocked by SWITCH_PENDING_ERROR.
    settleSwitchForTransportLoss();
    readyGate.markNotReady();
    // A real connect supersedes any in-flight empty-token /status preflight
    // so a late token_required/tokenless decision cannot replace this socket.
    preflightGenRef.current += 1;
    if (retryTimerRef.current !== null) {
      window.clearTimeout(retryTimerRef.current);
      retryTimerRef.current = null;
    }
    // A real connect supersedes any pending hold re-probe (its /status probe
    // would otherwise fire later and open a second connection).
    if (holdReprobeTimerRef.current !== null) {
      window.clearTimeout(holdReprobeTimerRef.current);
      holdReprobeTimerRef.current = null;
    }
    // Any timers or provisional renders still running belong to the previous
    // socket; stop them before replacing its projection ownership.
    cancelAllTextRenders();
    clearHeartbeatTimers();
    const old = wsRef.current;
    wsRef.current = null;
    if (old) {
      // Detach ALL four handlers (not just onclose) before closing: a
      // CONNECTING old socket whose onopen fires after replacement would
      // otherwise run onOpen against the NEW generation/wsRef and bootstrap a
      // second time, and a late onmessage would route old frames into the new
      // socket. detachTransportHandlers nulls onopen/onmessage/onerror/onclose.
      detachTransportHandlers(old);
      try {
        old.close(1000, 'replaced');
      } catch {
        /* already closed */
      }
    }
    // Advance the socket generation and reject every pending command from a
    // PRIOR socket (defensive: a genuine drop already drained them in onclose;
    // this catches a replaced socket whose onclose was detached). rpc=true
    // keeps this quiet — it only ever fires on bootstrap-path commands, which
    // must stay silent. The new socket's commands (sent from onOpen) stamp
    // the new gen; their bootstrap
    // continuations check `alive()` so a superseded socket can never
    // applyState/setConnState/refreshGoal/scheduleReconnect on the healthy
    // socket's behalf.
    rejectPendingExcept(++socketGenRef.current, 'connection replaced', true);
    // The host is the serving page authority (hostRef) and the token comes
    // from the Settings panel (tokenRef) — both kept in sync with React
    // state so this callback never re-creates across renders.
    const tokenValue = tokenRef.current;
    const hostValue = hostRef.current;
    setConnState('connecting');
    const protocols = tokenValue ? [`${RPI_AUTH_PREFIX}${tokenValue}`] : [];
    let ws: WebSocket;
    try {
      const scheme = location.protocol === 'https:' ? 'wss://' : 'ws://';
      ws = new WebSocket(`${scheme}${hostValue}/ws`, protocols);
    } catch {
      setConnState('reconnecting');
      toastTransportOutage('cannot open WebSocket, reconnecting…');
      scheduleReconnect();
      return;
    }
    wsRef.current = ws;
    handshakeStartedAtRef.current = Date.now();
    if (handshakeTimerRef.current !== null) window.clearTimeout(handshakeTimerRef.current);
    handshakeTimerRef.current = window.setTimeout(() => {
      handshakeTimerRef.current = null;
      if (wsRef.current !== ws) return;
      if (!shouldRetireHandshake(ws.readyState, handshakeStartedAtRef.current, Date.now())) return;
      if (ws.readyState !== WebSocket.OPEN) neverOpenedFailuresRef.current += 1;
      closeForHeartbeat('heartbeat timeout');
    }, TRANSPORT_HANDSHAKE_DEADLINE_MS);
    let socketOpened = false;
    ws.onopen = () => {
      // A successful open clears the never-opened failure streak: the
      // token-less /status re-probe guard only counts REFUSED handshakes.
      socketOpened = true;
      neverOpenedFailuresRef.current = 0;
      onOpen();
    };
    ws.onmessage = (event) => {
      const raw = event.data as string;
      lastMessageAtRef.current = Date.now();
      rescheduleSilenceTimer();
      pingSentAtRef.current = null;
      if (pongTimerRef.current !== null) {
        window.clearTimeout(pongTimerRef.current);
        pongTimerRef.current = null;
      }
      if (isPongFrame(raw)) return;
      onMessage(raw);
    };
    ws.onclose = (event) => {
      if (event.target !== wsRef.current) return; // superseded by a newer socket
      if (!socketOpened) neverOpenedFailuresRef.current += 1;
      wsRef.current = null;
      clearHeartbeatTimers();
      noteStreamingSocketDrop();
      // Reject and delete every pending command from this transport
      // immediately, while preserving optimistic bubbles/attachments for
      // retry. The shared transport toast owns the truthful close cause so
      // panels and sidebar actions suppress local duplicates.
      const reason = `connection closed (code ${event.code})${event.reason ? `: ${event.reason}` : ''}`;
      let toastOwned = toastTransportOutage(`${reason}, reconnecting…`);
      // toastTransportOutage returns false when the persistent outage toast
      // is ALREADY visible (a prior close in this same outage emitted it).
      // Every drain of this generation is then owned too — metadata, never
      // message matching — so user-action consumers never duplicate it.
      // Initial CONNECTING (never connected, no active outage surface)
      // stays unowned: the panel's user-action toast is the single surface.
      if (!toastOwned && transportOutageActiveRef.current) {
        toastOwned = true;
      }
      const candidateAuthHold = !socketOpened && !everConnectedRef.current && Boolean(tokenValue);
      if (candidateAuthHold) {
        // Overlay while POST /rpc confirms 401 vs a down listener. Browser
        // WS 401 is 1006, the same shape as connection refused.
        setConnState('reconnecting');
      } else {
        const handshakePlan = planRefusedHandshake({
          socketOpened,
          everConnected: everConnectedRef.current,
          hasToken: Boolean(tokenValue),
          authFailed: false,
        });
        setConnState(handshakePlan.connState);
      }
      rejectPendingExcept(++socketGenRef.current, reason, false, toastOwned);
      settleSwitchForTransportLoss();
      if (!candidateAuthHold) {
        scheduleReconnect();
        return;
      }
      const gen = socketGenRef.current;
      const host = hostValue;
      const presented = tokenValue;
      void probeListenAuth(host, presented).then((probe) => {
        if (gen !== socketGenRef.current) return;
        if (wsRef.current) return;
        const handshakePlan = planRefusedHandshake({
          socketOpened: false,
          everConnected: false,
          hasToken: true,
          authFailed: probe === 'unauthorized',
        });
        setConnState(handshakePlan.connState);
        if (handshakePlan.kind === 'auth_hold') {
          transportOutageActiveRef.current = true;
          toast('connection failed (wrong or missing token). Set the token in the Settings panel.', true, { kind: 'transport', timeoutMs: null });
          return;
        }
        scheduleReconnect();
      });
    };
    ws.onerror = () => {
      /* the close event carries the failure */
    };
  }, [clearHeartbeatTimers, closeForHeartbeat, onOpen, readyGate, rejectPendingExcept, rescheduleSilenceTimer, scheduleReconnect, settleSwitchForTransportLoss, toast, toastTransportOutage]);
  connectRef.current = connect;
  tearTransportRef.current = closeForHeartbeat;
  /** Settings-panel token commit: persist under the CURRENT host's scoped key
   *  (rpi-web-token:<authority> — never a global key) so each host's token
   *  stays separate, and reconnect so the new token takes effect
   *  (rpi-auth.<token> subprotocol). Persist even when unchanged so the stored
   *  value reflects the panel, but reconnect only on change. */
  const handleTokenChange = useCallback((nextToken: string) => {
    const trimmed = nextToken.trim();
    const host = hostRef.current; // snapshot the authority this token belongs to
    saveTokenForAuthority(tokenStorage, host, trimmed);
    if (trimmed === tokenRef.current) return; // same token: persisted, no reconnect
    preflightGenRef.current += 1; // an in-flight boot preflight is superseded
    everConnectedRef.current = false;
    tokenRef.current = trimmed;
    setToken(trimmed);
    if (tokenHoldRef.current) {
      // token_required hold: a NON-EMPTY token releases the hold and
      // reconnects EXACTLY once; an empty save stays held (connecting
      // tokenless would only be refused again).
      if (tokenCommitWhileHeld(trimmed) === 'stay-held') return;
      tokenHoldRef.current = false;
    }
    // A real connect supersedes any pending hold re-probe (its later probe
    // would otherwise open a second connection against the fresh socket).
    if (holdReprobeTimerRef.current !== null) {
      window.clearTimeout(holdReprobeTimerRef.current);
      holdReprobeTimerRef.current = null;
    }
    connect();
  }, [connect]);

  /** Arm the bounded hold re-probe (limbo guard): while held in
   *  token_required, /status is re-decided on a fixed cadence so a listener
   *  restart into tokenless mode releases the hold instead of pinning the
   *  page in Settings forever. */
  const armHoldReprobe = useCallback(() => {
    if (holdReprobeTimerRef.current !== null) return;
    holdReprobeTimerRef.current = window.setTimeout(() => {
      holdReprobeTimerRef.current = null;
      bootProbeHandoffRef.current();
    }, TOKEN_HOLD_REPROBE_MS);
  }, []);

  /** Bounded unauthenticated /status preflight for the empty-token boot. A
   *  token-gated listener flips to the truthful `token_required` state and
   *  opens Settings (its Connection section hosts the token input) instead of
   *  opening a WebSocket that can only be refused; every other outcome —
   *  tokenless listener, preflight unavailable or malformed — preserves the
   *  existing connect attempt so proxy/degraded setups are never bricked. */
  const bootWithPreflight = useCallback(() => {
    const gen = ++preflightGenRef.current;
    void fetchBootStatus(hostRef.current).then((status) => {
      if (gen !== preflightGenRef.current) return; // a token commit superseded this probe
      if (decideBootConnection(status, tokenRef.current) !== 'token_required') {
        // P2: the hold re-probe found a healthy TOKENLESS listener (or the
        // probe is unavailable — never brick). Release the hold BEFORE
        // connecting, or scheduleReconnect would return token_required on
        // the very next drop and re-pin the page.
        if (shouldReleaseTokenHold(status, tokenRef.current)) {
          tokenHoldRef.current = false;
        }
        connect();
        return;
      }
      if (gen !== preflightGenRef.current) return;
      tokenHoldRef.current = true;
      setConnState('token_required');
      // Hold re-probe (limbo guard): keep re-deciding /status so a listener
      // restart into tokenless mode releases the hold by itself.
      armHoldReprobe();
    });
  }, [armHoldReprobe, connect]);
  bootProbeHandoffRef.current = bootWithPreflight;
  wakeProbeHandoffRef.current = () => {
    const action = wakeTransportAction({
      tokenHold: tokenHoldRef.current,
      socketOpen: !!wsRef.current && wsRef.current.readyState === WebSocket.OPEN,
      hasStoredToken: tokenRef.current.trim() !== '',
    });
    if (action === 'preflight') {
      bootProbeHandoffRef.current();
      return;
    }
    if (action === 'ping') {
      // Background timer throttling can leave an outstanding ping older than
      // the 10s pong deadline, and a silence timer whose remaining window is
      // already 1ms. That wall-clock is not a liveness signal: drop both and
      // send a fresh probe instead of retiring the still-OPEN socket.
      pingSentAtRef.current = null;
      if (pongTimerRef.current !== null) {
        window.clearTimeout(pongTimerRef.current);
        pongTimerRef.current = null;
      }
      if (silenceTimerRef.current !== null) {
        window.clearTimeout(silenceTimerRef.current);
        silenceTimerRef.current = null;
      }
      sendHeartbeatPing();
      for (const [sid, on] of Object.entries(streamingBySessionIdRef.current)) {
        if (on === true) streamWatchdogRef.current?.probe(sid);
      }
      return;
    }
    if (retryTimerRef.current !== null) {
      window.clearTimeout(retryTimerRef.current);
      retryTimerRef.current = null;
    }
    delayRef.current = RECONNECT_INITIAL_DELAY_MS;
    connect();
  };

  /* ---------------- event dispatch ---------------- */
  //
  // ORDERING CONTRACT: every handler referenced by onEvent is declared ABOVE
  // it (executable-safe order) and listed in onEvent's dependency array —
  // there is no use-before-declaration (TDZ) and no suppressed dependency.

  const onMessageStart = useCallback((frame: EventFrame) => {
    const message = (frame.message || {}) as {
      role?: string;
      content?: unknown;
      provider?: string;
      model?: string;
      toolCallId?: string;
      details?: unknown;
      display?: boolean;
      customType?: string;
      command?: string;
      output?: string;
    };
    const role = message.role;
    const sid = frameSession(frame);
    if (role === 'user') {
      const projected = userMessageProjection(message.content);
      const queue = optimisticQueueBySessionIdRef.current;
      const queueId = sid ? (queue[sid] ?? []).shift() : undefined;
      if (queueId) {
        patchItemFor(sid, queueId, (item) => (item.kind === 'user' ? { ...item, optimistic: false } : item));
      } else {
        pushItemFor(sid, {
          kind: 'user',
          id: typeof frame.itemId === 'string' && frame.itemId !== '' ? frame.itemId : nextId('u'),
          text: projected.text,
          optimistic: false,
          ...(projected.images.length > 0 ? { images: projected.images } : {}),
          ...(projected.analysis ? { analysis: projected.analysis } : {}),
        });
      }
      // A queued steering message that enters the chat has been consumed:
      // drop its tray row now. The consuming turn boundary publishes the
      // pre-drain queue, so without this the row lingers until the next
      // boundary or settle. Preview text is derived with the SAME extractor
      // the wire queue_update rows use, so optimistic and authoritative rows
      // match.
      if (sid) {
        writeSteering((prev) => {
          const current = prev[sid];
          if (!current || current.length === 0) return prev;
          const next = dropConsumedPreview(current, userContentPreview(message.content));
          if (next === current) return prev;
          return { ...prev, [sid]: next };
        });
      }
      return;
    }
    if (role === 'assistant') {
      markStreamingFor(sid, true);
      const id = typeof frame.itemId === 'string' && frame.itemId !== '' ? frame.itemId : nextId('a');
      if (sid) {
        clearStreamStateForKey(streamKey(sid, id));
        activeAssistantBySessionIdRef.current[sid] = id;
      }
      pushItemFor(sid, { kind: 'assistant', id, status: 'streaming', blocks: [] });
      return;
    }
    if (role === 'toolResult') {
      const text = boundOutput(contentText(message.content), TOOL_OUTPUT_LINE_LIMIT).text;
      const toolCallId = typeof message.toolCallId === 'string' ? message.toolCallId : '';
      const media = toolMedia(message.content, message.details);
      updateItemsFor(sid, (prev) => applyToolResultToItems(prev, toolCallId, text, message.details, media));
      return;
    }
    if (role === 'bashExecution') {
      pushItemFor(sid, {
        kind: 'bash',
        id: typeof frame.itemId === 'string' && frame.itemId !== '' ? frame.itemId : nextId('b'),
        command: message.command || '',
        output: boundOutput(message.output || '', BASH_OUTPUT_LINE_LIMIT).text,
      });
      return;
    }
    if (role === 'custom') {
      const item = customToItem(message);
      if (item) pushItemFor(sid, frame.itemId ? { ...item, id: frame.itemId } : item);
    }
  }, [frameSession, markStreamingFor, patchItemFor, pushItemFor, updateItemsFor, writeSteering]);

  const onMessageUpdate = useCallback((frame: EventFrame) => {
    const ev = (frame.assistantMessageEvent || {}) as { type?: string; delta?: string; content?: string };
    if (!ev.type) return;
    const sid = frameSession(frame);
    const targetId = typeof frame.itemId === 'string' && frame.itemId !== ''
      ? frame.itemId
      : sid ? (activeAssistantBySessionIdRef.current[sid] ?? '') : '';
    if (!targetId) return;
    markStreamingFor(sid, true);
    const key = streamKey(sid, targetId);
    let buf = streamBuf.get(key);
    if (!buf) {
      buf = { text: '', thinking: '' };
      streamBuf.set(key, buf);
    }
    switch (ev.type) {
      case 'text_delta':
        if (ev.delta) {
          buf.text += ev.delta;
          appendToLiveNode(sid, targetId, ev.delta, 'text');
        }
        break;
      case 'thinking_delta':
        if (ev.delta) {
          buf.thinking += ev.delta;
          appendToLiveNode(sid, targetId, ev.delta, 'thinking');
        }
        break;
      case 'toolcall_delta':
        break;
      default:
        break;
    }
  }, [appendToLiveNode, frameSession, markStreamingFor]);

  const onMessageEnd = useCallback((frame: EventFrame) => {
    const message = (frame.message || {}) as { role?: string; content?: ContentBlock[]; stopReason?: unknown; errorMessage?: unknown };
    if (message.role !== 'assistant') return;
    const sid = frameSession(frame);
    const targetId = typeof frame.itemId === 'string' && frame.itemId !== ''
      ? frame.itemId
      : sid ? (activeAssistantBySessionIdRef.current[sid] ?? '') : '';
    if (!sid || !targetId) return;
    const key = streamKey(sid, targetId);
    cancelTextRender(key);
    const buf = streamBuf.get(key);
    const hasRenderable = Array.isArray(message.content)
      ? (message.content as ContentBlock[]).some(
          (block) => block && ((block.type === 'text' && block.text) || (block.type === 'thinking' && block.thinking))
        )
      : false;
    const blocks: ContentBlock[] = hasRenderable
      ? (Array.isArray(message.content) ? message.content : []) as ContentBlock[]
      : [
          ...(buf && buf.text ? [{ type: 'text', text: buf.text } as ContentBlock] : []),
          ...(buf && buf.thinking ? [{ type: 'thinking', thinking: buf.thinking } as ContentBlock] : []),
        ];
    // message_end is final authority: revoke the live node/key before the
    // React final commit so a later stale delta or RAF cannot repaint it.
    clearStreamStateForKey(key);
    const failureMessage = hasRenderable ? undefined : assistantFailureMessage(message.stopReason, message.errorMessage);
    if (activeAssistantBySessionIdRef.current[sid] === targetId) {
      delete activeAssistantBySessionIdRef.current[sid];
    }
    patchItemFor(sid, targetId, (item) =>
      item.kind === 'assistant'
        ? { ...item, status: 'final' as const, blocks, ...(failureMessage ? { errorMessage: failureMessage } : {}) }
        : item
    );
  }, [frameSession, patchItemFor]);

  const onToolStart = useCallback((frame: EventFrame) => {
    const sid = frameSession(frame);
    markStreamingFor(sid, true);
    updateItemsFor(sid, (prev) => applyToolStartToItems(prev, {
      kind: 'toolCard',
      id: typeof frame.itemId === 'string' && frame.itemId !== '' ? frame.itemId : nextId('tc'),
      toolCallId: (frame.toolCallId as string) || '',
      toolName: safeText(frame.toolName || 'tool'),
      args: frame.args,
      status: 'running',
      result: '',
    }));
  }, [frameSession, markStreamingFor, updateItemsFor]);

  const onToolUpdate = useCallback((frame: EventFrame) => {
    const toolCallId = (frame.toolCallId as string) || '';
    const text = contentText((frame.partialResult as { content?: unknown } | undefined)?.content);
    if (!text) return;
    updateItemsFor(frameSession(frame), (prev) => applyToolSnapshot(prev, toolCallId, text));
  }, [frameSession, updateItemsFor]);
  const onToolEnd = useCallback((frame: EventFrame) => {
    const toolCallId = (frame.toolCallId as string) || '';
    const rawResult = frame.result;
    let content: unknown;
    let details: unknown;
    if (rawResult && typeof rawResult === 'object') {
      if ('content' in rawResult) content = rawResult.content;
      if ('details' in rawResult) details = rawResult.details;
    }
    const text = contentText(content);
    const isError = !!frame.isError;
    // Bound the rendered result to its tail like the TUI compact tool card so
    // a huge tool output never dominates the transcript while the error
    // status and trailing lines stay visible. Carry structured details for
    const media = toolMedia(content, details);
    // Task (TaskSpawn[]) / Edit (diff) default views.
    updateItemsFor(frameSession(frame), (prev) =>
      applyToolSnapshot(prev, toolCallId, text, toolCardCompletionStatus(isError, content, details), details, media)
    );
  }, [frameSession, updateItemsFor]);

  const confirmAllOptimisticFor = useCallback(
    (sid: string | null) => {
      if (!sid) return;
      optimisticQueueBySessionIdRef.current[sid] = [];
      updateItemsFor(sid, (prev) => {
        if (!prev.some((i) => i.kind === 'user' && i.optimistic)) return prev;
        return prev.map((i) => (i.kind === 'user' && i.optimistic ? { ...i, optimistic: false } : i));
      });
    },
    [updateItemsFor]
  );

  const onBashExecutionEnd = useCallback((frame: EventFrame) => {
    const message = (frame.message || {}) as {
      command?: string;
      output?: string;
      exitCode?: number | null;
      cancelled?: boolean;
    };
    const status: 'done' | 'cancelled' | 'error' | undefined = message.cancelled === true
      ? 'cancelled'
      : typeof message.exitCode === 'number' && message.exitCode !== 0
        ? 'error'
        : typeof message.exitCode === 'number'
          ? 'done'
          : undefined;
    pushItemFor(frameSession(frame), {
      kind: 'bash',
      id: nextId('b'),
      command: message.command || '',
      output: boundOutput(message.output || '', BASH_OUTPUT_LINE_LIMIT).text,
      ...(status ? { status } : {}),
    });
  }, [frameSession, pushItemFor]);

  const finalizeActiveAssistantFor = useCallback((sid: string | null) => {
    if (!sid) return;
    const targetId = activeAssistantBySessionIdRef.current[sid] ?? '';
    if (targetId === '') return;
    const key = streamKey(sid, targetId);
    cancelTextRender(key);
    const streamedText = streamBuf.get(key)?.text ?? '';
    clearStreamStateForKey(key);
    delete activeAssistantBySessionIdRef.current[sid];
    updateItemsFor(sid, (prev) => finalizeStreamingAssistant(prev, targetId, streamedText));
  }, [updateItemsFor]);

  /** Projected extension UI requests. Interactive asks (confirm/input/
   * select/editor) cannot be answered remotely by design, so they surface as
   * a transcript notice card + toast pointing the user to the terminal.
   * Notifications become toasts. All other projections are ignored. The item
   * is pushed to the OWNING session's cache; toasts fire only when the owning
   * session is the active view (a background session's approval must never
   * interrupt the active session's user). */
  const onExtensionUiRequest = useCallback((frame: EventFrame) => {
    const method = typeof frame.method === 'string' ? frame.method : '';
    const title = typeof frame.title === 'string' ? frame.title : '';
    const message = typeof frame.message === 'string' ? frame.message : '';
    const extensionId = typeof frame.extensionId === 'string' ? frame.extensionId : undefined;
    const owner = frameSession(frame);
    const active = owner === sessionIdRef.current;
    if (['confirm', 'input', 'select', 'editor'].includes(method)) {
      pushItemFor(owner, {
        kind: 'approval',
        id: nextId('ap'),
        method,
        title: title || method,
        message: message || title || '',
        extensionId,
      });
      if (active) {
        toast(`Approval needed (${method}${extensionId ? ` · ${extensionId}` : ''}) — answer in the terminal`);
      }
    } else if (method === 'notify' && active) {
      toast(title || message || 'extension notification');
    }
  }, [frameSession, pushItemFor, toast]);

  const onEvent = useCallback((frame: EventFrame) => {
    // Route every owned event to its session. Background-session events update
    // ONLY the owning session's cache/unread — never the active panel's state
    // (Todo/Goal/Workflow/Subagents/Side chat/model/thinking). Unowned frames
    // fail closed instead of falling through to the active session.
    const sid = frameSession(frame);
    if (!sid) return;
    const active = sid === sessionIdRef.current;
    const abortPending = abortPendingBySessionIdRef.current[sid] === true;
    if (!(shouldHoldOptimisticStop(abortPending, true) && frame.type !== 'turn_terminal')) {
      updateActivityStripFor(sid, (state) => applyActivityStripEvent(state, frame, sid));
    }
    const setStreaming = (on: boolean) => markStreamingFor(sid, on);
    switch (frame.type) {
      case 'turn_started':
        abortPendingBySessionIdRef.current[sid] = false;
        setStreaming(true);
        break;
      case 'queue_update': {
        const previews = steeringPreviewsFromMessages(frame.steering);
        steeringLiveBySessionIdRef.current = { ...steeringLiveBySessionIdRef.current, [sid]: socketGenRef.current };
        steeringQueueGenRef.current += 1;
        writeSteering((prev) => applyQueueUpdatePreviews(prev, sid, previews));
        break;
      }
      case 'turn_start':
      case 'turn_end':
        // Inner model-loop events are advisory; outer turn_terminal owns completion.
        break;
      case 'message_start':
        onMessageStart(frame);
        break;
      case 'message_update':
        onMessageUpdate(frame);
        break;
      case 'message_end':
        onMessageEnd(frame);
        break;
      case 'tool_execution_start':
        onToolStart(frame);
        break;
      case 'tool_execution_update':
        onToolUpdate(frame);
        break;
      case 'tool_execution_end':
        onToolEnd(frame);
        break;
      case 'bash_execution_end':
        onBashExecutionEnd(frame);
        break;
      case 'turn_terminal': {
        confirmAllOptimisticFor(sid);
        setStreaming(false);
        finalizeActiveAssistantFor(sid);
        // A completed turn is the main thing that moves context usage; the
        // owning session's meter cannot wait for the next full snapshot.
        refreshContextUsage(sid);
        setCatalogRefreshRevision((revision) => revision + 1);
        const status = typeof frame.status === 'string' ? frame.status : '';
        if (sid && abortPendingBySessionIdRef.current[sid]) {
          abortPendingBySessionIdRef.current[sid] = false;
          if (abortIdleSnapshotToasts(
            true,
            false,
            displayedSessionMatches(sid, displayStateRef.current?.displayed.sessionId),
          )) toast('run aborted');
        } else if (active && status === 'failed') {
          toast(typeof frame.message === 'string' ? frame.message : 'run failed', true);
        }
        break;
      }
      case 'agent_settled':
      case 'run_failed':
        // Advisory diagnostics only. turn_terminal is the sole completion authority.
        break;
      case 'todo_child_routed': {
        const receipt = frame.receipt as { childSessionId?: unknown; id?: unknown } | undefined;
        const childSessionId = typeof receipt?.childSessionId === 'string' ? receipt.childSessionId : '';
        const receiptId = typeof receipt?.id === 'string' ? receipt.id : nextId('route');
        confirmAllOptimisticFor(sid);
        pushItemFor(sid, {
          kind: 'summary',
          id: `todo-route-${receiptId}`,
          label: 'todo',
          text: childSessionId ? `Routed to Todo · ${childSessionId}` : 'Routed to Todo',
        });
        break;
      }
      case 'todo_updated':
      case 'todo_reminder':
        setTodoBySessionId((prev) => {
          const snapshot = prev[sid] ?? { sessionId: sid, phases: [], sequence: 0, executionStatus: 'dormant', failureReason: null };
          const next = applyTodoEvent(snapshot, frame as Parameters<typeof applyTodoEvent>[1]);
          return next === snapshot ? prev : { ...prev, [sid]: next };
        });
        break;
      case 'workflow_updated':
      case 'workflow_status_changed':
      case 'workflow_removed':
        if (active) {
          workflowHandlersRef.current.forEach((handler) => handler(frame as WorkflowEventPayload));
        }
        break;
      case 'goal_updated':
        // Every goal mutation (create/pin/unpin/pause/resume/complete/
        // drop) pushes the resulting snapshot into the OWNING session's cache;
        // refresh that session's journal too (sid captured -> no race).
        if (sid && frame.state && typeof frame.state === 'object') {
          setGoalStateBySessionId((prev) => ({ ...prev, [sid]: frame.state as GoalStateWire }));
        }
        refreshGoal(sid);
        break;
      case 'goal_usage_charged':
        if (sid && frame.state && typeof frame.state === 'object') {
          setGoalStateBySessionId((prev) => ({ ...prev, [sid]: frame.state as GoalStateWire }));
        }
        break;
      case 'model_select': {
        const modelKey = wireModelKey(frame.model as ModelSelectInput);
        if (sid && modelKey) {
          writeSessionModelState(applySessionModelEvent(sessionModelStateRef.current, sid, modelKey));
        }
        break;
      }
      case 'thinking_level_select':
        if (sid && typeof frame.thinkingLevel === 'string') {
          writeThinkingLevelState(reconcileThinkingLevel(thinkingLevelStateRef.current, sid, frame.thinkingLevel));
        }
        break;
      case 'job_updated':
      case 'agent_updated':
      case 'message_delivered':
        // Live Task cards (transcript) + Subagents panel share these events.
        // Project child status/activity/result onto matching Task tool cards
        // for the OWNING session (background sessions keep their own cache).
        if (sid) {
          if (frame.type === 'job_updated') {
            updateItemsFor(sid, (prev) => applyJobUpdated(prev, { job: frame.job }));
          } else if (frame.type === 'agent_updated') {
            updateItemsFor(sid, (prev) => applyAgentUpdated(prev, { agent: frame.agent }));
          } else {
            updateItemsFor(sid, (prev) => applyMessageDelivered(prev, { message: frame.message }));
          }
        }
        setActivityBySessionId((prev) => {
          const snapshot = prev[sid] ?? { sessionId: sid, entries: [], sequence: 0, actors: {} };
          const next = applyActivityEvent(snapshot, frame as Parameters<typeof applyActivityEvent>[1]);
          return next === snapshot ? prev : { ...prev, [sid]: next };
        });
        // Live orchestration events also refresh the active session's mounted
        // Subagents panel; background-session events are never forwarded.
        if (active) {
          subagentsHandlersRef.current.forEach((handler) => handler(frame));
        }
        break;
      case 'extension_ui_request':
        // Projected extension UI events are non-interactive over RPC —
        // remote answering is rejected by design, so interactive asks render
        // as a notice card + toast ("answer in the terminal"). Item push is
        // owning-session scoped inside onExtensionUiRequest.
        onExtensionUiRequest(frame);
        break;
      default:
        break; // session/process/workflow events: v1 ignores
    }
    if (frame.type === 'tool_execution_end') {
      const rawResult = frame.result;
      const details = rawResult && typeof rawResult === 'object' && 'details' in rawResult
        ? (rawResult as { details?: unknown }).details
        : undefined;
      if (typeof frame.toolName === 'string' && frame.toolName.toLowerCase() === 'task') {
        updateActivityStripFor(sid, (state) => applyTaskSpawnDetails(
          state,
          sid,
          typeof frame.turnId === 'string' ? frame.turnId : '',
          details,
        ));
      }
    }
    // Background activity bumps the owning session's unread badge (one bump
    // per meaningful event; high-frequency deltas stay silent).
    if (sid && !active && UNREAD_EVENT_TYPES[frame.type]) {
      markBackgroundEvent(sid);
    }
  }, [
    frameSession,
    markBackgroundEvent,
    markStreamingFor,
    confirmAllOptimisticFor,
    finalizeActiveAssistantFor,
    onMessageStart,
    onMessageUpdate,
    onMessageEnd,
    onBashExecutionEnd,
    onToolStart,
    onToolUpdate,
    onToolEnd,
    onExtensionUiRequest,
    updateActivityStripFor,
    toast,
    updateItemsFor,
    writeSteering,
    refreshGoal,
    refreshContextUsage,
  ]);
  onEventRef.current = onEvent;

  const onMessage = useCallback((raw: string) => {
    if (isPongFrame(raw)) return;
    let frame: EventFrame | RpcResponse;
    try {
      frame = JSON.parse(raw);
    } catch {
      return;
    }
    if (frame && frame.type === 'response') {
      onResponse(frame as RpcResponse);
      return;
    }
    const eventFrame = frame as EventFrame;
    if (eventFrame.type === 'extension_ui_request') {
      const identity = identityFromWire(eventFrame.sessionId, eventFrame.runtimeIncarnation);
      if (!identity || residentBySessionIdRef.current[identity.sessionId] !== identity.runtimeIncarnation) return;
      onEvent(eventFrame);
      return;
    }
    if (frame.type === 'session_unloaded') {
      try {
        const unloaded = parseSessionUnload(frame);
        const identity = { sessionId: unloaded.sessionId, runtimeIncarnation: unloaded.runtimeIncarnation };
        const display = displayStateRef.current;
        if (!display) {
          invalidateRuntime(identity);
          return;
        }
        const pendingGeneration = display.pending?.generation ?? null;
        let fallback: RuntimeIdentity | null = null;
        if (display.pending && !sameRuntime(display.pending.source, identity)) {
          fallback = display.pending.source;
        } else {
          for (const ready of Object.values(readyBySessionIdRef.current)) {
            if (!sameRuntime(ready.identity, identity)) {
              fallback = ready.identity;
              break;
            }
          }
          if (!fallback) {
            for (const [sessionId, runtimeIncarnation] of Object.entries(residentBySessionIdRef.current)) {
              const candidate = { sessionId, runtimeIncarnation };
              if (!sameRuntime(candidate, identity)) {
                fallback = candidate;
                break;
              }
            }
          }
        }
        const plan = unloadRuntime(display, identity, fallback);
        displayStateRef.current = plan.state;
        publishLifecycle(plan.state);
        invalidateRuntime(identity);
        if (plan.state) {
          if (!sameRuntime(activatedRuntimeRef.current, plan.state.displayed)) {
            activateRuntime(plan.state.displayed, true);
            saveSessionPreference(tokenStorage, hostRef.current, plan.state.commandTarget.sessionId);
            subscribeRuntime(plan.state.displayed, cursorByRuntimeKeyRef.current.get(runtimeKey(plan.state.displayed)) ?? 0);
          }
        } else {
          activatedRuntimeRef.current = null;
          sessionIdRef.current = null;
          setSessionId(null);
          setLiveSettings(null);
        }
        if (plan.failedPending && pendingGeneration !== null) {
          settlePendingSwitch(pendingGeneration, false);
          setSwitchPending(false);
        }
      } catch {
        return;
      }
      return;
    }
    if (frame.type === 'session_resync_required') {
      const identity = identityFromWire(eventFrame.sessionId, eventFrame.runtimeIncarnation);
      if (!identity) return;
      cursorByRuntimeKeyRef.current.set(runtimeKey(identity), 0);
      subscribeRuntime(identity, 0);
      return;
    }
    if (frame.type === 'subscription_snapshot' || frame.type === 'session_resync') {
      let cut;
      try {
        cut = parseSubscriptionCut(frame);
      } catch {
        const identity = identityFromWire(eventFrame.sessionId, eventFrame.runtimeIncarnation);
        const pending = displayStateRef.current?.pending;
        if (identity && pending?.targetIdentity && sameRuntime(pending.targetIdentity, identity)) {
          if (!sameRuntime(readyBySessionIdRef.current[identity.sessionId]?.identity, identity)) invalidateRuntime(identity);
          rollbackPendingSwitch(pending.generation);
        } else if (identity) {
          cursorByRuntimeKeyRef.current.set(runtimeKey(identity), 0);
          subscribeRuntime(identity, 0);
        }
        return;
      }
      const identity = { sessionId: cut.sessionId, runtimeIncarnation: cut.runtimeIncarnation };
      const resident = residentBySessionIdRef.current[identity.sessionId];
      if (resident !== identity.runtimeIncarnation) return;
      loadedSessionIdsRef.current.add(identity.sessionId);
      const pending = displayStateRef.current?.pending;
      if (pending?.targetIdentity && sameRuntime(pending.targetIdentity, identity)) {
        lastCutProgressAtRef.current = Date.now();
      }
      const generation = pending && sameRuntime(pending.targetIdentity, identity) ? pending.generation : 0;
      if (generation === 0) {
        const ready = readyBySessionIdRef.current[identity.sessionId];
        const liveCursor = cursorByRuntimeKeyRef.current.get(runtimeKey(identity)) ?? 0;
        if (ready !== undefined
          && ready.socketGeneration === socketGenRef.current
          && sameRuntime(ready.identity, identity)
          && cut.watermark <= liveCursor) {
          // Stale superseded cut for an already exact-ready identity on THIS
          // socket generation (e.g. a leftover negotiated subscribe after a
          // fast bind): the ready cache is authoritative and applying its
          // snapshot at clientCursor=0 would clobber the live transcript.
          // The LIVE event cursor (cursorByRuntimeKeyRef) decides — ordinary
          // events advance it past the ready stamp — and a cut above the live
          // cursor is fresh data, not a stale duplicate. A PRIOR-generation
          // ready entry is never a suppression basis: the cut applies and
          // mark-ready re-stamps the current generation.
          return;
        }
      }
      const display = displayStateRef.current;
      if (!display) return;
      const plan = acceptAuthoritativeCut(display, generation, cut);
      const finishCut = (committed: typeof plan.state, settlesPromise: boolean) => {
        publishLifecycle(displayStateRef.current);
        if (settlesPromise) {
          settlePendingSwitch(generation, true);
          setSwitchPending(false);
        } else if (!committed.pending && pending) {
          if (!sameRuntime(activatedRuntimeRef.current, committed.displayed)) {
            activateRuntime(committed.displayed, true);
            saveSessionPreference(tokenStorage, hostRef.current, committed.commandTarget.sessionId);
          }
          settlePendingSwitch(pending.generation, false);
          setSwitchPending(false);
        }
      };
      const bindAt = plan.actions.findIndex((action) => action.type === 'bind-authority');
      if (!warmContinuityNeedsHistoryConfirm(cut.reason, cut.messages, plan.actions) || bindAt < 0) {
        displayStateRef.current = plan.state;
        applySwitchActions(plan.actions);
        finishCut(plan.state, plan.settlesPromise);
        return;
      }
      const liveItems = itemsBySessionIdRef.current[identity.sessionId] ?? [];
      if (mergeAuthoritativeItems([], liveItems, false).length > 0) {
        displayStateRef.current = plan.state;
        applySwitchActions(plan.actions);
        finishCut(plan.state, plan.settlesPromise);
        return;
      }
      const hold = planWarmHistoryConfirm(display, plan, identity, socketGenRef.current);
      if (!hold) {
        displayStateRef.current = plan.state;
        applySwitchActions(plan.actions);
        finishCut(plan.state, plan.settlesPromise);
        return;
      }
      applySwitchActions(hold.preBindActions);
      cursorByRuntimeKeyRef.current.set(runtimeKey(identity), acceptedCutLiveCursor(cut.events, cut.watermark));
      void sendCommand({ type: 'get_messages', sessionId: identity.sessionId }, undefined, { allowDuringSwitch: true }).then((data) => {
        if (!warmHistoryConfirmIsCurrent(hold, displayStateRef.current, socketGenRef.current)) return;
        if (data && typeof data === 'object' && Array.isArray((data as { messages?: unknown }).messages)) {
          updateItemsFor(identity.sessionId, (items) => mergeAuthoritativeItems(
            messagesToItems((data as { messages: unknown[] }).messages),
            items,
            true,
          ));
        }
        const liveCursor = cursorByRuntimeKeyRef.current.get(runtimeKey(identity)) ?? 0;
        displayStateRef.current = hold.committed;
        applySwitchActions(preserveLiveBindCursor(hold.commitActions, liveCursor));
        finishCut(hold.committed, hold.settlesPromise);
      }).catch(() => {
        if (!warmHistoryConfirmIsCurrent(hold, displayStateRef.current, socketGenRef.current)) return;
        rollbackPendingSwitch(hold.generation);
      });
      return;
    }
    if (frame.type === 'subscription_error') {
      let parsed;
      try {
        parsed = parseSubscriptionError(frame);
      } catch (error) {
        if (!(error instanceof SwitchProtocolError)) throw error;
        const identity = identityFromWire(eventFrame.sessionId, eventFrame.runtimeIncarnation);
        const pending = displayStateRef.current?.pending;
        if (identity && pending?.targetIdentity && sameRuntime(pending.targetIdentity, identity)) {
          if (!sameRuntime(readyBySessionIdRef.current[identity.sessionId]?.identity, identity)) {
            invalidateRuntime(identity);
          }
          rollbackPendingSwitch(pending.generation);
          toast(`session subscription failed: ${error.message}`, true);
        }
        return;
      }
      const identity = { sessionId: parsed.sessionId, runtimeIncarnation: parsed.runtimeIncarnation };
      const display = displayStateRef.current;
      const plan = display ? planSubscriptionError(display, identity) : null;
      if (!plan) return;
      if (shouldRetrySnapshotInProgress(
        parsed.code,
        snapshotInProgressRetryGenerationRef.current === plan.generation,
      )) {
        snapshotInProgressRetryGenerationRef.current = plan.generation;
        if (snapshotInProgressRetryTimerRef.current !== null) {
          window.clearTimeout(snapshotInProgressRetryTimerRef.current);
        }
        snapshotInProgressRetryTimerRef.current = window.setTimeout(() => {
          snapshotInProgressRetryTimerRef.current = null;
          const pendingNow = displayStateRef.current?.pending;
          if (!pendingNow
            || pendingNow.generation !== plan.generation
            || !pendingNow.targetIdentity
            || !sameRuntime(pendingNow.targetIdentity, identity)) {
            return;
          }
          subscribeRuntime(identity, cursorByRuntimeKeyRef.current.get(runtimeKey(identity)) ?? 0);
        }, SNAPSHOT_IN_PROGRESS_RETRY_MS);
        return;
      }
      if (!sameRuntime(readyBySessionIdRef.current[identity.sessionId]?.identity, identity)) {
        invalidateRuntime(identity);
      }
      rollbackPendingSwitch(plan.generation);
      toast(`session subscription failed: ${parsed.error}`, true);
      return;
    }
    const event = frame as EventFrame;
    const identity = identityFromWire(event.sessionId, event.runtimeIncarnation);
    if (!identity || typeof event.sequence !== 'number') return;
    if (residentBySessionIdRef.current[identity.sessionId] !== identity.runtimeIncarnation) return;
    const key = runtimeKey(identity);
    const cursor = cursorByRuntimeKeyRef.current.get(key) ?? 0;
    const gap = planLiveSequenceGap({ cursor, sequence: event.sequence });
    if (gap === 'duplicate') return;
    if (gap === 'gap') {
      const sid = identity.sessionId;
      const hold = {
        sessionId: sid,
        runtimeIncarnation: identity.runtimeIncarnation,
        socketGeneration: socketGenRef.current,
      };
      if (
        planLiveGapCatchUp({ recoveryInFlight: gapCatchUpInFlightRef.current[sid] === true }) === 'coalesce'
        || refillInFlightRef.current === sid
        || settledStreamRefillInFlightRef.current[sid] === true
      ) {
        return;
      }
      subscribeRuntime(identity, cursor);
      const slot = (gapCatchUpSlotRef.current[sid] ?? 0) + 1;
      gapCatchUpSlotRef.current = { ...gapCatchUpSlotRef.current, [sid]: slot };
      gapCatchUpInFlightRef.current = { ...gapCatchUpInFlightRef.current, [sid]: true };
      const catchUpOpts = { allowDuringSwitch: true as const, auxiliary: true as const };
      void sendCommand({ type: 'get_state', sessionId: sid }, undefined, catchUpOpts)
        .then((state) => {
          if (!reconnectRefillStillOwned(
            hold,
            residentBySessionIdRef.current[hold.sessionId],
            socketGenRef.current,
          )) return;
          applyState(state, sid, false);
        })
        .catch((err: Error & { toastOwned?: boolean }) => {
          if (!reconnectRefillStillOwned(
            hold,
            residentBySessionIdRef.current[hold.sessionId],
            socketGenRef.current,
          )) return;
          if (planStreamCatchUpToast({
            toastOwned: isToastOwnedRpcError(err),
            transport: isTransportError(err),
            busyCapacity: isBusyCapacityError(err),
          }) === 'silent') return;
          toast(`stream catch-up failed: ${err.message}`, true);
        })
        .finally(() => {
          if ((gapCatchUpSlotRef.current[sid] ?? 0) !== slot) return;
          const next = { ...gapCatchUpInFlightRef.current };
          delete next[sid];
          gapCatchUpInFlightRef.current = next;
        });
      return;
    }
    cursorByRuntimeKeyRef.current.set(key, event.sequence);
    const pendingTarget = displayStateRef.current?.pending?.targetIdentity;
    if (pendingTarget && sameRuntime(pendingTarget, identity)) {
      lastCutProgressAtRef.current = Date.now();
    }
    onEvent(event);
    noteStreamStall(identity.sessionId, { type: 'live_event' });
    if (streamingBySessionIdRef.current[identity.sessionId] === true) {
      armStreamWatchdogRef.current(identity.sessionId);
    }
  }, [activateRuntime, applyState, applySwitchActions, invalidateRuntime, noteStreamStall, onResponse, onEvent, publishLifecycle, rollbackPendingSwitch, sendCommand, settlePendingSwitch, subscribeRuntime, toast, updateItemsFor]);

  /* ---------------- composer ---------------- */

  // Single-line composer: grow only to ~3 lines (measured from the input's
  // own line-height + vertical padding, so the cap stays right at any font
  // size) and collapse back to 1 line whenever the content shrinks.
  //
  // Per-keystroke onInput only SCHEDULES the resize here; the layout work
  // (height:auto reset → scrollHeight read → height write, which forces a
  // synchronous reflow per event) is coalesced into ONE pass per animation
  // frame, and the static metrics are measured once and cached (see
  // ./autoResize). submit() flushes so a cleared composer collapses
  // immediately. Declared before `submit` because the submit deps array
  // references flushComposerResize eagerly.
  const autoResizeRef = useRef<AutoResizeController<HTMLTextAreaElement> | null>(null);
  const autoResize = useCallback((input: HTMLTextAreaElement) => {
    autoResizeRef.current ??= createAutoResizeController<HTMLTextAreaElement>({
      maxLines: 3,
      measure: (el) => {
        const style = window.getComputedStyle(el);
        return {
          lineHeight: parseFloat(style.lineHeight) || 20,
          paddingVertical:
            (parseFloat(style.paddingTop) || 0) + (parseFloat(style.paddingBottom) || 0),
        };
      },
    });
    autoResizeRef.current.resize(input);
  }, []);
  // The placeholder itself can wrap to 2-3 lines at narrow composer widths
  // before any input event fires (rows=1 element, autoResize is input-driven)
  // — re-measure on mount and on viewport changes so the wrapped placeholder
  // grows the textarea instead of clipping at the footer edge.
  useEffect(() => {
    const input = promptInputRef.current;
    if (!input) return;
    autoResize(input);
    const onWindowResize = () => {
      if (promptInputRef.current) autoResize(promptInputRef.current);
    };
    window.addEventListener('resize', onWindowResize);
    return () => window.removeEventListener('resize', onWindowResize);
  }, [autoResize, promptInputRef]);
  const flushComposerResize = useCallback((input: HTMLTextAreaElement) => {
    autoResizeRef.current?.flush(input);
  }, []);

  /* ---------------- /commit picker + shared code-review opener ---------------- */

  // Recent-commit chooser for bare `/commit`: one fetch per open, guarded by
  // the captured owner session AND a monotonic generation. Closing or
  // reopening the picker (or a session switch between open and settle)
  // invalidates the in-flight response, so a stale payload can never paint
  // another session's rows. A failed listing keeps the picker open — the
  // typed ref survives for a retry — and surfaces exactly one central error.
  const [commitPickerOpen, setCommitPickerOpen] = useState(false);
  const [commitRows, setCommitRows] = useState<CodeReviewCommitRow[]>([]);
  const [commitsLoading, setCommitsLoading] = useState(false);
  const commitPickerLoadRef = useRef({ sequence: 0, ownerSessionId: '' });

  const closeCommitPicker = useCallback((): void => {
    commitPickerLoadRef.current.sequence += 1;
    commitPickerLoadRef.current.ownerSessionId = '';
    setCommitPickerOpen(false);
    setCommitsLoading(false);
  }, []);
  useEffect(() => {
    const owner = commitPickerLoadRef.current.ownerSessionId;
    if (owner && owner !== composerCommandSessionId(displayStateRef.current?.commandTarget, sessionIdRef.current)) {
      closeCommitPicker();
      setCommitRows([]);
    }
  }, [lifecycleView.displayedSessionId, lifecycleView.commandSessionId, closeCommitPicker]);

  const openCommitPicker = useCallback((): void => {
    const load = commitPickerLoadRef.current;
    load.sequence += 1;
    const generation = load.sequence;
    const ownerSid = composerCommandSessionId(displayStateRef.current?.commandTarget, sessionIdRef.current);
    load.ownerSessionId = ownerSid;
    setCommitPickerOpen(true);
    setCommitRows([]);
    setCommitsLoading(true);
    sendCommand({ ...buildCodeReviewCommitsPayload(), sessionId: ownerSid }, undefined, { auxiliary: true })
      .then((data) => {
        const current = commitPickerLoadRef.current;
        if (current.sequence !== generation || current.ownerSessionId !== ownerSid
          || ownerSid !== composerCommandSessionId(displayStateRef.current?.commandTarget, sessionIdRef.current)) return;
        const parsed = normalizeCodeReviewCommits(data);
        if (parsed.ok) {
          setCommitRows(parsed.list.commits);
          setCommitsLoading(false);
          return;
        }
        setCommitsLoading(false);
        toast(parsed.error, true);
      })
      .catch((err: Error & { toastOwned?: boolean }) => {
        const current = commitPickerLoadRef.current;
        if (current.sequence !== generation || current.ownerSessionId !== ownerSid
          || ownerSid !== composerCommandSessionId(displayStateRef.current?.commandTarget, sessionIdRef.current)) return;
        setCommitsLoading(false);
        if (isToastOwnedRpcError(err) || isTransportError(err)) return;
        toast(`Code review commits: ${err.message}`, true);
      });
  }, [sendCommand, toast]);

  /** Owner-stamped code-review open shared by `/code-review` and the
   *  `/commit` picker: the probe resolves its revision in the workspace of
   *  the session the user is LOOKING AT (displayed command target), its
   *  snapshot-error and moved-display-authority settles own the cleanup
   *  close, and success retains the args on the mounted panel. The composer
   *  draft clears only when unchanged on success; rejections preserve it. */
  const openCodeReview = useCallback((openArgs: CodeReviewOpenArgs): Promise<unknown> => {
    const reviewSid = composerCommandSessionId(displayStateRef.current?.commandTarget, sessionIdRef.current);
    const opened = codeReviewOpenCommand(openArgs);
    if (!opened.ok) {
      toast(opened.error, true);
      return Promise.resolve(undefined);
    }
    const draft = promptInputRef.current?.value ?? '';
    return sendCommand({ ...opened.command, sessionId: reviewSid })
      .then((data) => {
        const snapshotError = codeReviewOpenSnapshotError(data);
        if (snapshotError) {
          const reviewId = normalizeCodeReviewSnapshot(data).reviewId;
          if (reviewId) {
            // Owner-targeted cleanup of the review this probe just
            // opened: the probe may settle inside a pending switch
            // window, where an ungated send is rejected
            // (commandRejectedBySwitchPending) and the review would be
            // left open on its owner — the explicit sessionId makes the
            // during-switch allowance safe.
            void sendCommand({ type: 'code_review_close', reviewId, sessionId: reviewSid }, undefined, { allowDuringSwitch: true }).catch((err: Error) => {
              if (isToastOwnedRpcError(err) || isTransportError(err)) return;
              toast(`Code review: ${snapshotError}; close failed: ${err.message}`, true);
            });
          }
          toast(`Code review: ${snapshotError}`, true);
          return;
        }
        // The user moved the DISPLAYED authority away from the session
        // this review belongs to while the probe was in flight — a
        // negotiated warm switch paints the new session while the command
        // target still names its source, and a warm reconnect re-binds the
        // active ref to the listener primary. The review is closed rather
        // than opened over a session the user is no longer looking at;
        // the active ref is the fallback only while no display authority
        // is bound (boot). The close is allowed during the switch that
        // caused the mismatch — that pending window is exactly where it
        // runs — and its explicit owner sessionId keeps it on that
        // session.
        if ((displayStateRef.current?.displayed.sessionId ?? sessionIdRef.current) !== reviewSid) {
          const reviewId = normalizeCodeReviewSnapshot(data).reviewId;
          if (reviewId) {
            void sendCommand({ type: 'code_review_close', reviewId, sessionId: reviewSid }, undefined, { allowDuringSwitch: true }).catch((err: Error) => {
              if (isToastOwnedRpcError(err) || isTransportError(err)) return;
              toast(`Code review: close failed: ${err.message}`, true);
            });
          }
          return;
        }
        if (promptInputRef.current && promptInputRef.current.value === draft) {
          promptInputRef.current.value = '';
          flushComposerResize(promptInputRef.current);
        }
        setCodeReviewOpenArgs({ ...openArgs, alreadyOpen: true });
        openPanel('code-review', { force: true });
      })
      .catch((err: Error) => {
        toast(`Code review: ${err.message}`, true);
      });
  }, [flushComposerResize, openPanel, sendCommand, toast]);

  const updateOwnedAttachments = useCallback((
    owner: RuntimeIdentity | null,
    generation: number,
    update: (attachments: ComposerAttachment[], pendingUploadIds: ReadonlySet<string>) => ComposerAttachment[],
    updatePending?: (pendingUploadIds: Set<string>) => void,
    authenticated?: { attachments: ComposerAttachment[]; pendingUploadIds: ReadonlySet<string> },
  ): boolean => {
    if (!owner) return false;
    const ownerPainted = sameRuntime(activatedRuntimeRef.current, owner);
    const exactProjection = ownerPainted && intakeGenRef.current === generation;
    const key = runtimeKey(owner);
    const saved = viewSlotsByRuntimeKeyRef.current.get(key);
    if (!exactProjection && (!authenticated || !ownerPainted) && (!saved || saved.intakeGeneration !== generation)) return false;

    const useAuthenticatedOwner = ownerPainted && authenticated !== undefined;
    const ownedAttachments = exactProjection
      ? attachmentsRef.current
      : useAuthenticatedOwner ? authenticated.attachments : saved!.attachments;
    const ownedPending = exactProjection
      ? pendingUploadIdsRef.current
      : useAuthenticatedOwner ? authenticated.pendingUploadIds : saved!.pendingUploadIds;
    const settledAttachments = update(ownedAttachments, ownedPending);
    const settledPending = new Set(ownedPending);
    updatePending?.(settledPending);

    let projectedAttachments = settledAttachments;
    let projectedPending = settledPending;
    let projectedGeneration = generation;
    if (ownerPainted && !exactProjection) {
      // Returning to the same exact runtime advances the composer generation.
      // Authenticate against the captured slot, then merge only this settle's
      // added, changed, or removed immutable ids into the fresh projection.
      const removedIds = new Set(
        ownedAttachments
          .filter((previous) => !settledAttachments.some((next) => next.id === previous.id))
          .map((attachment) => attachment.id),
      );
      const changed = settledAttachments.filter((next) => {
        const previous = ownedAttachments.find((attachment) => attachment.id === next.id);
        return (!previous || previous !== next)
          && ((next.kind !== 'video' && next.kind !== 'pdf') || next.videoState !== 'uploading');
      });
      const changedById = new Map(changed.map((attachment) => [attachment.id, attachment]));
      projectedAttachments = attachmentsRef.current
        .filter((attachment) => !removedIds.has(attachment.id))
        .map((attachment) => changedById.get(attachment.id) ?? attachment);
      const projectedIds = new Set(projectedAttachments.map((attachment) => attachment.id));
      for (const attachment of changed) {
        if (!projectedIds.has(attachment.id)) projectedAttachments.push(attachment);
      }
      projectedPending = new Set(pendingUploadIdsRef.current);
      for (const id of ownedPending) {
        if (!settledPending.has(id)) projectedPending.delete(id);
      }
      projectedGeneration = intakeGenRef.current;
    }

    const slot: SessionViewSlot = {
      ...(saved ?? emptyViewSlot(owner)),
      identity: owner,
      lastUsed: Date.now(),
      attachments: projectedAttachments,
      intakeGeneration: projectedGeneration,
      pendingUploadIds: projectedPending,
    };
    const display = displayStateRef.current;
    if (!display) return false;
    viewSlotsByRuntimeKeyRef.current = upsertViewSlot(
      viewSlotsByRuntimeKeyRef.current,
      slot,
      display.displayed,
      display.pending,
    );
    if (ownerPainted) {
      attachmentsRef.current = projectedAttachments;
      setAttachments(projectedAttachments);
      pendingUploadIdsRef.current = projectedPending;
      intakeBudgetTrackerRef.current.setQueued(reconcileIntakeBudget(projectedAttachments));
    }
    return true;
  }, []);
  const submit = useCallback((kind: 'prompt' | 'steer') => {
    const input = promptInputRef.current;
    if (!input) return;
    if (composerSendingRef.current) return;
    const targetSid = sessionIdRef.current ?? '';
    const resolvedKind = planComposerSubmitKind({
      streaming: targetSid !== '' && streamingBySessionIdRef.current[targetSid] === true,
      requested: kind,
    });
    if (resolvedKind === 'prompt') {
      const sendGate = compactSendAdmission(
        targetSid !== '' && compactBookRef.current.compacting[targetSid] === true,
      );
      if (!sendGate.ok) {
        toast(sendGate.message, true);
        return;
      }
    }
    if (intakeBudgetTrackerRef.current.hasOutstanding()) {
      toast('attachments are still processing — wait for them to finish', true);
      return;
    }
    const text = input.value.trim();
    // Attached images + extracted video frames -> prompt `images`
    // ContentBlocks; attached UTF-8 code files -> filename + fenced-code
    // blocks prepended to the prompt `message`; attached videos -> the
    // backend's bounded chronological-frame instruction prepended to the
    // `message` (marker) with the frames riding `images`; attached PDFs ->
    // extracted text + instruction prepended to the `message`. All mappings
    // live in ./attachments.
    const codeMessage = buildCodeMessage(attachments);
    const videoMarker = buildVideoMessage(attachments);
    const pdfMarker = buildPdfMessage(attachments);
    const images = attachmentsToImageBlocks(attachments);
    const unreadyUpload = attachments.find(
      (a) => (a.kind === 'video' || a.kind === 'pdf') && a.videoState !== 'ready',
    );
    if (unreadyUpload) {
      const label = unreadyUpload.kind === 'pdf' ? 'PDF' : 'video';
      if (unreadyUpload.videoState === 'error') {
        toast(
          `${label} ${unreadyUpload.name} failed to process${unreadyUpload.videoError ? `: ${unreadyUpload.videoError}` : ''} — remove it and try again`,
          true,
        );
      } else {
        toast(`${label} ${unreadyUpload.name} is still processing — wait for it to finish`, true);
      }
      return;
    }
    if (!text && codeMessage === '' && videoMarker === '' && pdfMarker === '' && images.length === 0) return;
    composerSendingRef.current = true;
    let dispatched: Promise<unknown> | null = null;
    try {

      // Intercept Web-supported slash commands when there are no attachments,
      // for BOTH submit kinds (prompt and steer — while a turn streams, Enter
      // dispatches a steer, and a typed slash command must still route to the
      // command surface instead of the model; the /loop streaming guard then
      // rejects create/update exactly like the TUI). Command selection only
      // drafts; this path is the real dispatch. Unknown slashes
      // (parseSupportedCommand → null) fall through as normal prompts/steers —
      // EXCEPT the TUI loop aliases (loops/loop-update/loop-delete/loop-cancel),
      // which are intercepted with an actionable error and NEVER reach the
      // model. Intercepted commands NEVER get an optimistic user bubble.
      // TUI parity for the composer: rejections (usage errors, the /loop
      // streaming guard, aliases) PRESERVE the draft so the user can correct
      // it — only accepted dispatch/panel actions clear the composer.
      if (text && attachments.length === 0) {
        const parsed = parseSupportedCommand(text);
        const sid = sessionIdRef.current;
        const acceptCommand = () => {
          input.value = '';
          flushComposerResize(input);
        };

        if (!parsed) {
          const alias = unsupportedAliasMessage(text);
          if (alias) {
            toast(alias, true);
            return;
          }
          const slash = parseSlashLine(text);
          if (slash && extensionNamesRef.current.has(slash.name)) {
            const command: Record<string, unknown> = {
              type: 'run',
              name: slash.name,
              args: slash.args,
            };
            // Slash dispatch stamps an EXPLICIT sessionId, which WINS over the
            // active session (./goal routeCommandSession), so it names the
            // DISPLAYED command target — the identity a typed prompt's implicit
            // target resolves to. sessionIdRef is NOT that identity after a warm
            // reconnect: the bootstrap re-binds it to the listener PRIMARY while
            // the displayed surface stays where the user was (see
            // ./composerSubmit composerCommandSessionId).
            const runSid = composerCommandSessionId(displayStateRef.current?.commandTarget, sessionIdRef.current);
            if (runSid) command.sessionId = runSid;
            acceptCommand();
            dispatched = sendCommand(command)
              .then((data) => {
                pushItemFor(runSid, {
                  kind: 'summary',
                  id: nextId('s'),
                  label: slash.name,
                  text: formatExtensionResult(data, slash.name),
                });
              })
              .catch((err: Error & { rpc?: boolean }) => {
                const message = err.message || String(err);
                pushItemFor(runSid, {
                  kind: 'summary',
                  id: nextId('s'),
                  label: slash.name,
                  text: message,
                });
                toast(`/${slash.name} failed: ${message}`, true);
              });
            return;
          }
          // Otherwise fall through to the normal prompt path below (which
          // consumes the text and clears the composer itself).
        } else {
          const action = resolveSlashAction(parsed.name, parsed.args);

          if (action.type === 'error') {
            toast(action.message, true);
            return;
          }

          if (action.type === 'code-review') {
            const openArgs: CodeReviewOpenArgs = {};
            if (action.from && action.to) {
              openArgs.from = action.from;
              openArgs.to = action.to;
            } else if (action.from && action.includeWorkingTree) {
              openArgs.from = action.from;
              openArgs.includeWorkingTree = true;
            }
            dispatched = openCodeReview(openArgs);
            return;
          }

          if (action.type === 'commit') {
            // Direct `/commit <ref>` and a picker selection share the SAME
            // owner-stamped opener; bare `/commit` opens the chooser and
            // fetches the displayed session's recent commits.
            if (action.ref) {
              dispatched = openCodeReview({ commit: action.ref });
              return;
            }
            openCommitPicker();
            return;
          }

          if (action.type === 'jobs' || action.type === 'irc') {
            acceptCommand();
            setSubagentsInitialPage(action.type === 'irc' ? 'irc' : 'jobs');
            openPanel('subagents', { force: true });
            return;
          }
          if (action.type === 'todo') {
            if (action.action.op === 'open') {
              acceptCommand();
              openPanel('todo', { force: true });
              return;
            }
            if (!sid) {
              toast('todo requires an active session', true);
              return;
            }
            const wire = todoWire(action.action, sid);
            if (!wire) return;
            wire.sessionId = sid;
            acceptCommand();
            // Child operations open the controller only after settlement; once
            // mounted it owns subsequent polling and panel-control errors.
            const childAction = String(wire.type).startsWith('todo_child_');
            if (!childAction) openPanel('todo', { force: true });
            dispatched = sendCommand(wire)
              .then((data) => {
                if (action.action.op === 'start'
                  || action.action.op === 'done'
                  || action.action.op === 'drop'
                  || action.action.op === 'clear') {
                  if (data && typeof data === 'object' && 'phases' in data) {
                    const phases = projectTodoPhases(data.phases);
                    setTodoBySessionId((prev) => {
                      const previous = prev[sid] ?? {
                        sessionId: sid,
                        phases: [],
                        sequence: 0,
                        executionStatus: 'dormant',
                        failureReason: null,
                      };
                      return { ...prev, [sid]: { ...previous, phases } };
                    });
                  }
                }
                if (childAction) openPanel('todo', { force: true });
              })
              .catch((err: Error & { toastOwned?: boolean }) => {
                if (childAction) {
                  openPanel('todo', { force: true });
                }
                if (!err.toastOwned || childAction) {
                  toast(`todo failed: ${err.message || String(err)}`, true);
                }
              });
            return;
          }

          if (action.type === 'compact') {
            const isSnap = action.mode === 'snap';
            const targetSid = sid ?? '';
            const admission = compactAdmission({
              streaming: !!(targetSid && streamingBySessionIdRef.current[targetSid]),
              compacting: !!(targetSid && compactBookRef.current.compacting[targetSid]),
            });
            if (!admission.ok) {
              toast(admission.message, true);
              return;
            }
            const command: Record<string, unknown> = isSnap
              ? { type: 'snapcompact' }
              : {
                  type: 'compact',
                  ...(action.customInstructions
                    ? { customInstructions: action.customInstructions }
                    : {}),
                };
            if (targetSid) command.sessionId = targetSid;
            const label = isSnap ? 'Snapcompact' : 'Compact';
            let compactToken = 0;
            if (targetSid) {
              const started = beginCompact(compactBookRef.current, targetSid, isSnap ? 'snap' : 'llm');
              compactToken = started.token;
              writeCompactBook(started.book);
            }
            acceptCommand();
            const finishCompact = () => {
              if (!targetSid || compactToken === 0) return;
              writeCompactBook(settleCompact(compactBookRef.current, targetSid, compactToken));
            };
            dispatched = sendCommand(command)
              .then((data) => {
                pushItemFor(sid, {
                  kind: 'summary',
                  id: nextId('s'),
                  label: isSnap ? 'snapcompact' : 'compact',
                  text: formatCompactReport(data, label),
                });
                finishCompact();
                refreshContextUsage(targetSid);
              })
              .catch((err: Error & { rpc?: boolean }) => {
                const message = err.message || String(err);
                pushItemFor(sid, {
                  kind: 'summary',
                  id: nextId('s'),
                  label: isSnap ? 'snapcompact' : 'compact',
                  text: message,
                });
                toast(`${label} failed: ${message}`, true);
                finishCompact();
              });
            return;
          }

          if (action.type === 'skill') {
            const command: Record<string, unknown> = { type: 'skill', name: action.name };
            if (sid) command.sessionId = sid;
            acceptCommand();
            dispatched = sendCommand(command)
              .then((data) => {
                pushItemFor(sid, {
                  kind: 'summary',
                  id: nextId('s'),
                  label: 'skill',
                  text: formatSkillResult(data, action.name),
                });
              })
              .catch((err: Error & { rpc?: boolean }) => {
                const message = err.message || String(err);
                pushItemFor(sid, {
                  kind: 'summary',
                  id: nextId('s'),
                  label: 'skill',
                  text: message,
                });
                toast(`skill failed: ${message}`, true);
              });
            return;
          }

          // /loop and /goal share ONE success/error summary path: dispatch the
          // typed RPC on the ACTIVE session (explicit sessionId, same as the
          // compact/skill paths), then render the formatted result — or the
          // actionable error, including the TUI-equivalent "no active loop with
          // id" case where the backend resolves `false` instead of failing —
          // as a visible summary bubble. Intercepted commands NEVER get an
          // optimistic user bubble. The wire mapping + text live in the pure
          // helpers in ./slashDispatch (mirrors the TUI parsers exactly).
          if (action.type === 'loop' || action.type === 'goal') {
            // TUI parity guard: /loop create|update are unavailable while
            // another turn is running (the TUI loop dispatch rejects exactly
            // these two ops while streaming). Rejected locally — no RPC leaves
            // the client and the draft stays in the composer, matching the TUI
            // status message verbatim.
            if (
              action.type === 'loop' &&
              loopRequiresIdle(action.action) &&
              (sid ? streamingBySessionIdRef.current[sid] : false)
            ) {
              toast('/loop is unavailable while another turn is running', true);
              return;
            }
            // TUI parity: BARE `/goal` opens the Goal panel (the TUI maps a
            // bare /goal to its goal panel); explicit show/get/inspect
            // dispatch the goal_get RPC below.
            if (action.type === 'goal' && action.action.op === 'show' && !action.action.explicit) {
              acceptCommand();
              refreshGoal(sid);
              openPanel('goal', { force: true });
              return;
            }
            acceptCommand();
            const wire = action.type === 'loop' ? loopWire(action.action) : goalWire(action.action);
            if (sid) wire.sessionId = sid;
            const label = action.type === 'loop' ? 'loop' : 'goal';
            // /goal create|resume dispatch with `activate: true` (TUI parity:
            // they start/queue goal work and resolve with the activation
            // outcome), so the summary path chains a goal_get for the state
            // line. All other commands format their own response directly.
            const activateWork =
              action.type === 'goal' &&
              (action.action.op === 'create' || action.action.op === 'resume');
            const run = activateWork
              ? sendCommand(wire).then((outcome) => {
                  const stateWire = goalWire({ op: 'show' });
                  if (sid) stateWire.sessionId = sid;
                  return sendCommand(stateWire).then((state) => ({ data: outcome, state }));
                })
              : sendCommand(wire).then((data) => ({ data, state: undefined }));
            dispatched = run.then(({ data, state }) => {
                const result =
                  action.type === 'loop'
                    ? formatLoopResult(action.action, data)
                    : formatGoalResult(action.action, data, state);
                if (result.ok) {
                  pushItemFor(sid, {
                    kind: 'summary',
                    id: nextId('s'),
                    label,
                    text: result.text,
                  });
                } else {
                  pushItemFor(sid, {
                    kind: 'summary',
                    id: nextId('s'),
                    label,
                    text: result.message,
                  });
                  toast(`${label} failed: ${result.message}`, true);
                }
              })
              .catch((err: Error & { rpc?: boolean }) => {
                const message = err.message || String(err);
                pushItemFor(sid, {
                  kind: 'summary',
                  id: nextId('s'),
                  label,
                  text: message,
                });
                toast(`${label} failed: ${message}`, true);
              });
            return;
          }

          // /ps reuses the accepted-command summary pattern: dispatch the
          // process_list RPC on the ACTIVE session (explicit sessionId, same
          // as the compact/skill/loop/goal paths), then render the bounded
          // TUI-parity process listing — or the actionable RPC error — as a
          // visible summary bubble. Intercepted commands NEVER get an
          // optimistic user bubble; only accepted dispatch clears the composer
          // (a `/ps extra` usage error preserves the draft and sends no RPC,
          // handled above by resolveSlashAction).
          if (action.type === 'ps') {
            const wire = psWire();
            if (sid) wire.sessionId = sid;
            acceptCommand();
            dispatched = sendCommand(wire)
              .then((data) => {
                pushItemFor(sid, {
                  kind: 'summary',
                  id: nextId('s'),
                  label: 'ps',
                  text: formatProcessList(data),
                });
              })
              .catch((err: Error & { rpc?: boolean }) => {
                const message = err.message || String(err);
                pushItemFor(sid, {
                  kind: 'summary',
                  id: nextId('s'),
                  label: 'ps',
                  text: message,
                });
                toast(`ps failed: ${message}`, true);
              });
            return;
          }
        }
      }

      // Hard front-end cap: the whole prompt frame (message + every image
      // base64) must fit under MAX_TOTAL_WIRE_BYTES. Intake + the video-settle
      // check keep this invariant, but re-check at submit so an edge case can
      // never overrun the 4 MiB RPC frame (the backend PAYLOAD_TOO_LARGE stays
      // the hard backstop).
      if (aggregateWire(attachments) > MAX_TOTAL_WIRE_BYTES) {
        toast('attachments exceed the total send size limit — remove some attachments', true);
        return;
      }

      // Code-file fences + video markers prepend the user's typed text; image/
      // frame-only sends render the thumbnails themselves (no placeholder
      // text). The bubble carries the same image payloads as the prompt frame
      // (minus the wire-only `type` tag), so the optimistic bubble and the
      // backend's persisted user message render identically and reconcile
      // without flicker.
      const message = [codeMessage, videoMarker, pdfMarker, text].filter(Boolean).join('\n\n');
      // Build the EXACT wire command before any UI mutation: the frame-byte
      // gate below must reject with the draft + chips fully intact. Images are
      // the same shape the backend persists (minus the wire-only `type` tag
      // carried by `command.images`).
      const command: Record<string, unknown> =
        resolvedKind === 'steer' ? { type: 'steer', message } : { type: 'prompt', message };
      if (images.length > 0) command.images = images;
      const sid = sessionIdRef.current;
      // EXACT pre-dispatch frame-byte check against the backend's 4 MiB RPC
      // frame cap (MAX_RPC_MESSAGE_BYTES — the backend rejects a frame at
      // exactly the cap, LF not counted): the FULL serialized command — typed
      // text + code fences + video markers + every image/frame base64 + the
      // envelope — counted with the same JSON escaping and UTF-8 bytes the
      // transport uses. `MAX_COMMAND_ID` is a fixed max-length id stand-in, so
      // the check is a conservative UPPER BOUND that never depends on
      // predicting sendCommand's counter. An over-cap frame is refused HERE,
      // before the optimistic bubble or the composer clear, so the draft is
      // preserved for editing (the backend PAYLOAD_TOO_LARGE stays the hard
      // backstop for any path this gate cannot see).
      if (commandFrameBytes(command, MAX_COMMAND_ID, sid) >= MAX_PROMPT_FRAME_BYTES) {
        toast('message and attachments exceed the 4 MiB send limit — shorten the message or remove some attachments', true);
        return;
      }
      const bubbleId = resolvedKind === 'steer' ? undefined : nextId('u');
      const paintedSteer = { value: resolvedKind === 'steer' };
      if (resolvedKind === 'steer') {
        // Immediate tray feedback: the steer text shows in the queue before
        // the next queue_update reconcile replaces it with the authoritative
        // list (identical content is a no-op, a rejection drains on the next
        // snapshot).
        if (sid) {
          writeSteering((prev) => ({
            ...prev,
            [sid]: [...(prev[sid] ?? []), message],
          }));
        }
      } else {
        pushItemFor(sid, {
          kind: 'user',
          id: bubbleId as string,
          text: message,
          optimistic: true,
          ...(images.length > 0
            ? { images: images.map(({ data, mimeType }) => ({ mimeType, data })) }
            : {}),
        });
        if (sid) (optimisticQueueBySessionIdRef.current[sid] ??= []).push(bubbleId as string);
      }
      input.value = '';
      flushComposerResize(input);
      // Capture the exact owner/generation and sent ids before dispatch. ACK
      // clears that owner only; failure preserves its slot for retry.
      const sentIds =
        attachments.length > 0 ? new Set(attachments.map((a) => a.id)) : null;
      const sendOwner = activatedRuntimeRef.current;
      const sendGeneration = intakeGenRef.current;
      // Enter resolves to the active run's prompt/steer verb. The primary
      // button is intentionally different while streaming: it becomes Stop,
      // while Enter keeps the typed steering-message shortcut.
      dispatched = dispatchComposerSubmit({
        kind: resolvedKind,
        send: (submitKind) => {
          const wire: Record<string, unknown> =
            submitKind === 'steer' ? { type: 'steer', message } : { type: 'prompt', message };
          if (images.length > 0) wire.images = images;
          if (submitKind === 'steer' && sid) {
            steeringSubmitInFlightBySessionRef.current[sid] = (steeringSubmitInFlightBySessionRef.current[sid] ?? 0) + 1;
          }
          return sendCommand(wire, submitKind === 'prompt' ? bubbleId : undefined).finally(() => {
            if (submitKind === 'steer' && sid) {
              const remaining = (steeringSubmitInFlightBySessionRef.current[sid] ?? 1) - 1;
              if (remaining <= 0) delete steeringSubmitInFlightBySessionRef.current[sid];
              else steeringSubmitInFlightBySessionRef.current[sid] = remaining;
            }
          });
        },
        onConvertToSteer: () => {
          paintedSteer.value = true;
          if (bubbleId) {
            if (sid) {
              const queue = optimisticQueueBySessionIdRef.current[sid];
              if (queue) {
                const index = queue.indexOf(bubbleId);
                if (index >= 0) queue.splice(index, 1);
              }
            }
            removeItemFor(sid, bubbleId);
          }
          if (sid) {
            markStreamingFor(sid, true);
            writeSteering((prev) => ({
              ...prev,
              [sid]: [...(prev[sid] ?? []), message],
            }));
          }
        },
      })
        .then((outcome) => {
          const accepted = outcome.data as { turnId?: unknown };
          if (sid && outcome.kind === 'prompt' && typeof accepted.turnId === 'string') {
            markStreamingFor(sid, true);
          }
          if (sentIds) {
            updateOwnedAttachments(
              sendOwner,
              sendGeneration,
              (prev) => removeSentAttachments(prev, sentIds),
              (pending) => sentIds.forEach((id) => pending.delete(id)),
            );
          }
        })
        .catch((err: Error & { rpc?: boolean; toastOwned?: boolean }) => {
          if (sid && paintedSteer.value) {
            // A rejected steer must stay visible: the row moves to the tray's
            // failed section (error styling + dismiss) instead of silently
            // draining on the next snapshot, and a persistent toast explains it.
            writeSteering((prev) => {
              const current = prev[sid];
              if (!current) return prev;
              const next = dropLastMatchingPreview(current, message);
              if (next === current) return prev;
              return { ...prev, [sid]: next };
            });
            setFailedSteeringBySessionId((prev) => ({
              ...prev,
              [sid]: [...(prev[sid] ?? []), message],
            }));
            if (!isTransportError(err)) {
              toast(`steer failed: ${err.message}`, true, { timeoutMs: null });
            }
            return;
          }
          if (
            !paintedSteer.value
            && /timed out after \d+s/.test(err.message)
            && sid
          ) {
            // A prompt timeout does not mean the turn failed: the frame may
            // still be in flight or already recorded. Keep the optimistic
            // bubble and re-probe the authoritative state instead of reporting
            // a failure.
            sendCommand({ type: 'get_state', sessionId: sid })
              .then((state) => applyState(state, sid, false))
              .catch(() => {});
            return;
          }
          if (isToastOwnedRpcError(err) || isTransportError(err)) return;
          toast(`send failed: ${err.message}`, true);
        });
    } finally {
      if (dispatched) {
        void dispatched.finally(() => {
          composerSendingRef.current = false;
        });
      } else {
        composerSendingRef.current = false;
      }
    }
  }, [attachments, flushComposerResize, markStreamingFor, openPanel, pushItemFor, refreshContextUsage, refreshGoal, removeItemFor, sendCommand, toast, updateOwnedAttachments, writeCompactBook, writeSteering]);

  const submitComposerEnter = useCallback(() => {
    submit('prompt');
  }, [submit]);

  const abortActiveRun = useCallback(() => {
    const displayed = displayStateRef.current?.displayed.sessionId;
    const sid = (typeof displayed === 'string' && displayed !== '')
      ? displayed
      : sessionIdRef.current;
    const tap = applyComposerAbortTap(sid);
    if (!sid || !tap.command) return;
    abortGestureClickRef.current = true;
    abortPendingBySessionIdRef.current[sid] = true;
    markStreamingFor(sid, false);
    noteStreamStall(sid, { type: 'abort' });
    window.setTimeout(() => {
      abortGestureClickRef.current = false;
    }, 0);
    updateActivityStripFor(sid, (state) => reconcileActivityStrip(state, sid, false, ''));
    sendCommand({ type: 'abort', sessionId: sid }, undefined, { allowDuringSwitch: true })
      .then(() => sendCommand({ type: 'get_state', sessionId: sid }, undefined, { allowDuringSwitch: true }))
      .then((state) => {
        applyState(state, sid, false);
      })
      .catch((err: Error & { rpc?: boolean; toastOwned?: boolean }) => {
        abortPendingBySessionIdRef.current[sid] = false;
        markStreamingFor(sid, true);
        if (isToastOwnedRpcError(err) || isTransportError(err)) return;
        toast(`stop failed: ${err.message}`, true);
      });
  }, [applyState, markStreamingFor, noteStreamStall, sendCommand, toast, updateActivityStripFor]);

  const [steeringBusy, setSteeringBusy] = useState(false);
  const sendSteeringNow = useCallback(() => {
    void runExclusiveQueueCommand(steeringBusyRef, setSteeringBusy, async () => {
      const sid = sessionIdRef.current;
      if (!sid) return;
      const gen = steeringQueueGenRef.current;
      await settleQueueCommand(
        () => sendCommand({ type: 'queue_send_now', sessionId: sid }),
        gen,
        () => steeringQueueGenRef.current,
        (error) => {
          const err = error as Error & { rpc?: boolean };
          if (!err.rpc && !isTransportError(err)) {
            toast(`send now failed: ${err instanceof Error ? err.message : String(err)}`, true);
          }
        },
      );
    });
  }, [sendCommand, toast]);
  const cancelSteeringAt = useCallback((index: number) => {
    void runExclusiveQueueCommand(steeringBusyRef, setSteeringBusy, async () => {
      const sid = sessionIdRef.current;
      if (!sid) return;
      const preview = (steeringBySessionIdRef.current[sid] ?? [])[index];
      const gen = steeringQueueGenRef.current;
      await settleQueueCommand(
        () => sendCommand({ type: 'queue_cancel', scope: 'steering', index, sessionId: sid }, undefined, { auxiliary: true }),
        gen,
        () => steeringQueueGenRef.current,
        (error) => {
          const err = error as Error & { toastOwned?: boolean };
          if (err.toastOwned === true || isTransportError(err)) return;
          toast(`cancel failed: ${err instanceof Error ? err.message : String(err)}`, true);
        },
        () => {
          writeSteering((prev) => {
            const current = prev[sid] ?? [];
            const next = dropQueuedPreviewAfterCancel(current, index, preview);
            if (next === current) return prev;
            return { ...prev, [sid]: next };
          });
        },
      );
    });
  }, [sendCommand, toast, writeSteering]);

  /* ---------------- composer: file attachments ---------------- */

  // Unified attachment intake for all three paths (paste / drop / file
  // picker): classify against per-file/aggregate/count limits, then queue the
  // accepted files IN INTAKE ORDER in ONE state push. READY INVARIANT:
  // image/code files are read to completion BEFORE that push (they enter the
  // queue fully built), while videos enter as uploading placeholders and
  // settle in place via the backend POST /upload/video endpoint (raw bytes
  // never ride the prompt frame; the returned chronological JPEG frames +
  // bounded instruction do). Late rejects (invalid UTF-8 / unreadable) are
  // collected into the skip toast, never built; a failed video upload KEEPS
  // its chip in an actionable error state (draft preserved), never a silent
  // drop.
  //
  // CONCURRENCY INVARIANT: `attachments` state is a closure snapshot, so two
  // Async intake completion is owned by the exact runtime + composer generation
  // captured at selection. If that runtime is no longer painted, patch its saved
  // slot instead of the new runtime's global composer projection.

  // intakes firing before a re-render (e.g. paste while a drop's reads are in
  // flight) would both classify against the same stale queued state and could
  // bypass the front-end caps. `intakeBudgetTrackerRef` is authoritative:
  // admission = queued exact + the SUM of every outstanding intake reservation.
  // Each intake registers its own key synchronously BEFORE its await (or before
  // a paste's setAttachments call), then its updater recomputes queued state and
  // releases ONLY that key. One intake settling can therefore never drop another
  // in-flight reservation; remove/send/reset keep queued state exact and late
  // skips/stale intakes release only their own reservation.
  // When a video's frames land, the aggregate wire is re-checked against
  // MAX_TOTAL_WIRE_BYTES and an over-budget video flips to an actionable
  // error state. The backend PAYLOAD_TOO_LARGE frame limit remains the hard
  // backstop.
  const onFilesChosen = useCallback(
    async (fileList: FileList | null) => {
      if (!fileList || fileList.length === 0) return;
      const intakeOwner = activatedRuntimeRef.current;
      const intakeContext: IntakeContext = {
        gen: intakeGenRef.current,
        host: hostRef.current,
        token: tokenRef.current,
        session: sessionIdRef.current,
      };
      const budget = intakeBudgetTrackerRef.current.admission();
      const plan = classifyAttachments(Array.from(fileList), {
        currentCount: budget.count,
        currentWire: budget.wire,
      });
      const intakeId = nextId('intake');
      if (plan.accepted.length > 0) {
        intakeBudgetTrackerRef.current.reserve(intakeId, intakeReservation(plan.accepted));
      }
      const skips = [...plan.skipped];
      if (plan.accepted.length > 0) {
        const previousCommit = intakeCommitQueueRef.current;
        const commitGate = withResolvers<void>();
        intakeCommitQueueRef.current = previousCommit.then(() => commitGate.promise, () => commitGate.promise);
        const tasks = plan.accepted.map((entry) => {
          const id = nextId('a');
          const pending = entry.kind === 'video' || entry.kind === 'pdf';
          return {
            entry,
            id,
            pendingAttachment: pending ? placeholderFor(entry, id) : null,
          };
        });
        for (const task of tasks) {
          if (!task.pendingAttachment || !intakeOwner) continue;
          if (task.entry.kind !== 'video' && task.entry.kind !== 'pdf') continue;
          pendingUploadIdsRef.current.add(task.id);
          pendingVideoOwnersRef.current.set(task.id, {
            owner: intakeOwner,
            generation: intakeContext.gen,
            attachment: task.pendingAttachment,
          });
        }
        const settleUpload = (
          id: string,
          name: string,
          kind: 'video' | 'pdf',
          outcome: VideoUploadOutcome | PdfUploadOutcome,
        ) => {
          const authenticated = pendingVideoOwnersRef.current.get(id);
          if (!authenticated
            || !sameRuntime(authenticated.owner, intakeOwner)
            || authenticated.generation !== intakeContext.gen) return;
          pendingVideoOwnersRef.current.delete(id);
          let eligible = false;
          let surface = !outcome.ok;
          const applied = updateOwnedAttachments(
            authenticated.owner,
            authenticated.generation,
            (prev, pending) => {
              if (!pending.has(id)) return prev;
              eligible = true;
              const settled = kind === 'pdf'
                ? settlePdfOutcome(prev, id, outcome as PdfUploadOutcome)
                : settleVideoOutcome(prev, id, outcome as VideoUploadOutcome);
              surface = settled.surface;
              return settled.next;
            },
            (pending) => pending.delete(id),
            { attachments: [authenticated.attachment], pendingUploadIds: new Set([id]) },
          );
          window.setTimeout(() => {
            if (applied && eligible && sameRuntime(activatedRuntimeRef.current, intakeOwner) && surface && !outcome.ok) {
              const label = kind === 'pdf' ? 'PDF' : 'video';
              toast(`${label} ${name} failed: ${outcome.message}`, true);
            }
          }, 0);
        };
        let built: ComposerAttachment[] = [];
        try {
          const reads = tasks.filter((task) => task.entry.kind !== 'video' && task.entry.kind !== 'pdf');
          const results = await readAttachmentsInOrder(reads.map((task) => task.entry), readAccepted);
          const readById = new Map<string, ReadResult>();
          results.forEach((result, index) => readById.set(reads[index]!.id, result as ReadResult));
          const merged = mergeIntakeResults(tasks, readById);
          built = merged.built;
          skips.push(...merged.skips);
        } catch {
          if (sameRuntime(activatedRuntimeRef.current, intakeOwner)) {
            toast('failed to read one or more attachments', true);
          }
          for (const task of tasks) {
            if (task.pendingAttachment) built.push(task.pendingAttachment);
          }
        }
        await previousCommit;
        const applied = updateOwnedAttachments(intakeOwner, intakeContext.gen, (prev) => {
          const admitted = admitBuiltAttachments(prev, built);
          skips.push(...admitted.skipped);
          return admitted.next;
        });
        if (!applied) {
          for (const task of tasks) {
            pendingUploadIdsRef.current.delete(task.id);
            pendingVideoOwnersRef.current.delete(task.id);
          }
          intakeBudgetTrackerRef.current.release(intakeId);
          commitGate.resolve();
          return;
        }
        intakeBudgetTrackerRef.current.release(intakeId);
        commitGate.resolve();
        const uploadOpts = {
          hostAuthority: intakeContext.host,
          token: intakeContext.token,
          pageProtocol: location.protocol,
        };
        await Promise.all(tasks
          .filter((task) => task.entry.kind === 'video' || task.entry.kind === 'pdf')
          .map((task) => {
            if (task.entry.kind === 'pdf') {
              return uploadPdfFile(task.entry.file, uploadOpts)
                .then((outcome) => settleUpload(task.id, task.entry.file.name, 'pdf', outcome));
            }
            return uploadVideoFile(task.entry.file, uploadOpts)
              .then((outcome) => settleUpload(task.id, task.entry.file.name, 'video', outcome));
          }));
      }
      const summary = formatSkipSummary(skips);
      if (summary && sameRuntime(activatedRuntimeRef.current, intakeOwner)) toast(summary, true);
    },
    [toast, updateOwnedAttachments],
  );

  const onComposerPaste = useCallback(
    (event: React.ClipboardEvent<HTMLTextAreaElement>) => {
      pasteEnterGuardUntilRef.current = nextPasteEnterGuard(performance.now());
      const files = event.clipboardData.files;
      if (files.length > 0) {
        event.preventDefault();
        void onFilesChosen(files);
        return;
      }

      const plan = planLargeTextPaste(event.clipboardData.getData('text/plain'));
      if (plan.type === 'native') return;

      event.preventDefault();
      if (plan.type === 'oversize') {
        toast(`pasted text exceeds the attachment limit (${plan.size} bytes)`, true);
        return;
      }

      const budget = intakeBudgetTrackerRef.current.admission();
      const classified = classifyAttachments(
        [{ type: plan.attachment.mimeType, name: plan.attachment.name, size: plan.attachment.size }],
        { currentCount: budget.count, currentWire: budget.wire },
      );
      if (classified.accepted.length === 0) {
        const summary = formatSkipSummary(classified.skipped);
        toast(summary ?? 'pasted text could not be attached', true);
        return;
      }
      const pasteIntakeId = nextId('intake');
      intakeBudgetTrackerRef.current.reserve(pasteIntakeId, intakeReservation(classified.accepted));
      const intakeOwner = activatedRuntimeRef.current;
      const intakeContext: IntakeContext = {
        gen: intakeGenRef.current,
        host: hostRef.current,
        token: tokenRef.current,
        session: sessionIdRef.current,
      };
      const previousCommit = intakeCommitQueueRef.current;
      const commitGate = withResolvers<void>();
      intakeCommitQueueRef.current = previousCommit.then(() => commitGate.promise, () => commitGate.promise);

      void previousCommit.then(() => {
        // Test handoff seam: a queued large-text paste follows the same exact
        // runtime/generation writeback as file reads and video settlements.
        updateOwnedAttachments(
          intakeOwner,
          intakeContext.gen,
          (prev) => [...prev, plan.attachment],
        );
        intakeBudgetTrackerRef.current.release(pasteIntakeId);
        commitGate.resolve();
      });
    },
    [onFilesChosen, updateOwnedAttachments],
  );


  const removeAttachment = useCallback((id: string) => {
    // A removed chip's in-flight upload outcome becomes STALE: the settle
    // gate must not resurrect the chip or toast a phantom failure.
    pendingUploadIdsRef.current.delete(id);
    pendingVideoOwnersRef.current.delete(id);
    updateAttachments((prev) => {
      const next = prev.filter((attachment) => attachment.id !== id);
      intakeBudgetTrackerRef.current.setQueued(reconcileIntakeBudget(next));
      return next;
    });
  }, [updateAttachments]);

  /* ---------------- composer: hold-to-talk voice ---------------- */

  const sttPendingDestinationRef = useRef<SttRecordingDestination | null>(null);

  const onSttTranscript = useCallback((text: string, owner: string, destination: SttRecordingDestination | null) => {
    const sessionId = sessionIdRef.current;
    const snapshot = sessionId ? (sideChatBySessionIdRef.current[sessionId] ?? null) : null;
    const route = planSttTranscriptRoute({
      destination,
      owner,
      currentSessionId: sessionId,
      sideChatPanelOpen: activePanelRef.current === 'sidechat',
      sideChatTabs: (snapshot?.tabs ?? []).map((tab) => tab.name),
      text,
      streaming: !!(owner && streamingBySessionIdRef.current[owner]),
    });
    if (route.kind === 'drop') return;
    if (route.kind === 'discardClosed') {
      toast(SIDE_CHAT_STT_CLOSED_TOAST, true);
      return;
    }
    if (route.kind === 'sideChatInsert') {
      const tabId = route.tabId;
      void (async () => {
        const insert = sideChatInsertTranscriptRef.current;
        if (!insert) {
          toast(SIDE_CHAT_STT_CLOSED_TOAST, true);
          return;
        }
        try {
          const outcome = await insert(text, tabId);
          const toastPlan = planSideChatSttInsertToast(outcome);
          if (toastPlan.kind === 'closed') {
            toast(SIDE_CHAT_STT_CLOSED_TOAST, true);
            return;
          }
          if (toastPlan.kind === 'error') {
            toast(toastPlan.message, true);
            return;
          }
          if (outcome.kind !== 'insert') return;
          sideChatPromptInputRef.current?.focus();
        } catch (err) {
          toast(planSideChatSttInsertThrowToast(err, { sessionId, tabId }), true);
        }
      })();
      return;
    }
    const input = promptInputRef.current;
    if (!input || !text) return;
    const current = input.value;
    const separator = current.trim() ? (current.endsWith('\n') ? '' : '\n') : '';
    input.value = `${current}${separator}${text}`;
    autoResize(input);
    input.focus();
    submit(route.plan);
  }, [autoResize, submit, toast]);

  const { recording, transcribing, startRecording, stopRecording } = useSttRecorder({
    getOwner: () => sessionIdRef.current,
    getDestination: () => sttPendingDestinationRef.current,
    sttConfigured: liveSettings,
    sendCommand,
    onTranscript: onSttTranscript,
    onError: (message) => toast(message, true),
  });

  const startMainSttRecording = useCallback(() => {
    sttPendingDestinationRef.current = captureSttRecordingDestination(sessionIdRef.current, 'MainComposer');
    void startRecording();
  }, [startRecording]);

  const startSideChatSttRecording = useCallback(() => {
    const sessionId = sessionIdRef.current;
    const tabId = sessionId ? (sideChatBySessionIdRef.current[sessionId]?.active ?? null) : null;
    sttPendingDestinationRef.current = captureSttRecordingDestination(sessionId, { kind: 'SideChatTab', tabId });
    void startRecording();
  }, [startRecording]);

  /* ---------------- composer: realtime voice (Codex Live WebRTC) ---------------- */

  /** True when the backend advertises live.enabled and realtime mode (trim +
   *  ASCII lowercase, centralized in realtime.ts). A missing/disabled live
   *  block never selects realtime, and applyState clears stale liveSettings
   *  when runtimeSettings omits live. */
  const isRealtimeMode = isRealtimeLiveMode(liveSettings);

  /** Commit a final transcript to the composer textarea, mirroring the STT
   *  flow's commit so both voice paths land the draft the same way. */
  const commitTranscriptToComposer = useCallback(
    (text: string) => {
      const input = promptInputRef.current;
      if (!input || !text) return;
      const current = input.value;
      const separator = current.trim() ? (current.endsWith('\n') ? '' : '\n') : '';
      input.value = `${current}${separator}${text}`;
      autoResize(input);
      input.focus();
    },
    [autoResize]
  );
  /** Insert a selected slash command into the composer textarea and focus it
   *  WITHOUT submitting — the user confirms and Main dispatches on Enter. The
   *  caret lands at the end so a param-requiring command's trailing space is
   *  immediately typeable. appendDraft preserves existing draft text (own
   *  line, no double newline), mirroring commitTranscriptToComposer's rule. */
  const insertCommandText = useCallback(
    (text: string) => {
      const input = promptInputRef.current;
      if (!input || !text) return;
      input.value = appendDraft(input.value, text);
      autoResize(input);
      const end = input.value.length;
      input.setSelectionRange(end, end);
      input.focus();
    },
    [autoResize]
  );
  /** Stage a picked SKILL candidate in the composer WITHOUT submitting: insert
   *  `/skill <name>`, focus the textarea, and SELECT the whole draft so the
   *  ready state is visible (pressing Enter dispatches, typing replaces the
   *  selection). A toast states the exact draft + Enter-to-run action — the
   *  picker never auto-submits, so this is the explicit confirmation cue. */
  const stageSkillCommandText = useCallback(
    (text: string) => {
      const input = promptInputRef.current;
      if (!input || !text) return;
      input.value = appendDraft(input.value, text);
      autoResize(input);
      const start = input.value.length - text.length;
      input.setSelectionRange(start, input.value.length);
      input.focus();
      toast(`${text} ready — press Enter to run`);
    },
    [autoResize, toast]
  );
  /** `oai-events` data-channel event dispatch: USER input transcript deltas
   *  stream into the overlay, GA segment events SUPERSEDE the running preview,
   *  a final transcript commits to the composer (deduped across the V1
   *  final-event variants) and clears the preview, transcription failures
   *  surface a bounded toast and clear the stale preview, delegations get a
   *  notification row, errors surface as toasts. Transport-agnostic — the
   *  handler parses JSON frames regardless of transport; the direct sideband
   *  WebSocket was removed (it could carry no browser Bearer header and would
   *  mixed-content-block on HTTPS→http), so events now arrive over the
   *  `oai-events` RTCDataChannel. Assistant OUTPUT transcript events are
   *  intentionally NOT routed here (classifyInputTranscriptEvent returns null
   *  for them) so they can never be committed to the composer as a user
   *  draft; output audio rides the WebRTC track, so output_audio.delta is a
   *  no-op. */
  // Quiescence-window delegation delivery state (round-10). The pending
  // delegation parks {owner, text} while the input-transcript deltas catch
  // up; the timer delivers the RICHER text; a final/switch/stop cancels.
  const pendingDelegationRef = useRef<{ owner: string; text: string } | null>(null);
  const delegationQuiesceTimerRef = useRef<number | null>(null);
  const deliverPendingDelegationRef = useRef<() => void>(() => {});
  const cancelPendingDelegationRef = useRef<() => void>(() => {});
  const submitVoicePrompt = useCallback((text: string) => {
    lastCommittedTranscriptRef.current = text;
    realtimeTranscriptRef.current = '';
    const node = realtimeTranscriptNodeRef.current;
    if (node) node.textContent = '';
    commitTranscriptToComposer(text);
  }, [commitTranscriptToComposer]);
  const clearRealtimeOverlay = useCallback(() => {
    realtimeTranscriptRef.current = '';
    const node = realtimeTranscriptNodeRef.current;
    if (node) node.textContent = '';
  }, []);
  const handleRealtimeFrame = useCallback(
    (frame: Record<string, unknown>) => {
      const action = reduceRealtimeVoiceFrame(frame, {
        owner: realtimeOwnerRef.current,
        current: sessionIdRef.current,
        overlay: realtimeTranscriptRef.current,
        lastCommitted: lastCommittedTranscriptRef.current,
        pendingText: pendingDelegationRef.current?.text ?? '',
      });
      switch (action.kind) {
        case 'noop':
          return;
        case 'overlay':
          lastCommittedTranscriptRef.current = '';
          realtimeTranscriptRef.current = action.text;
          {
            const node = realtimeTranscriptNodeRef.current;
            if (node) node.textContent = action.text;
          }
          return;
        case 'failed':
          toast(action.message, true);
          clearRealtimeOverlay();
          return;
        case 'deliver':
          if (action.cancelPending) cancelPendingDelegationRef.current();
          submitVoicePrompt(action.text);
          return;
        case 'consume-final':
          cancelPendingDelegationRef.current();
          clearRealtimeOverlay();
          return;
        case 'park':
          setRealtimeDelegation(action.text);
          cancelPendingDelegationRef.current();
          pendingDelegationRef.current = { owner: action.owner, text: action.text };
          delegationQuiesceTimerRef.current = window.setTimeout(() => {
            delegationQuiesceTimerRef.current = null;
            deliverPendingDelegationRef.current();
          }, REALTIME_DELEGATION_QUIESCENCE_MS);
          return;
        case 'error':
          toast(action.message, true);
          return;
        case 'empty-handoff':
          return;
        case 'unhandled':
          console.debug(`[realtime] unhandled event type: ${action.type.slice(0, 120)}`);
          return;
      }
    },
    [clearRealtimeOverlay, submitVoicePrompt, toast]
  );

  // Quiescence-window delivery (round-10): deliver the RICHER of (parked
  // delegation text, accumulated preview) once the window closes — owner-
  // gated, deduped against the shared anchor, preview cleared like a final.
  useEffect(() => {
    deliverPendingDelegationRef.current = () => {
      const pending = pendingDelegationRef.current;
      pendingDelegationRef.current = null;
      const action = reducePendingVoiceFlush(
        pending,
        realtimeTranscriptRef.current,
        sessionIdRef.current,
        lastCommittedTranscriptRef.current,
      );
      clearRealtimeOverlay();
      if (action.kind === 'empty-handoff') {
        toast('Voice delegation carried no transcribed text — not delivered', true);
        return;
      }
      if (action.kind !== 'deliver') return;
      submitVoicePrompt(action.text);
      const bounded = action.text.length > 120 ? `${action.text.slice(0, 119)}…` : action.text;
      toast(`Voice draft ready: ${bounded}`, false);
    };
    cancelPendingDelegationRef.current = () => {
      if (delegationQuiesceTimerRef.current !== null) {
        window.clearTimeout(delegationQuiesceTimerRef.current);
        delegationQuiesceTimerRef.current = null;
      }
      pendingDelegationRef.current = null;
    };
    return () => {
      if (delegationQuiesceTimerRef.current !== null) window.clearTimeout(delegationQuiesceTimerRef.current);
      delegationQuiesceTimerRef.current = null;
      pendingDelegationRef.current = null;
    };
  }, [clearRealtimeOverlay, submitVoicePrompt, toast]);

  /** Tear down the realtime call: close the `oai-events` data channel, close
   *  the RTCPeerConnection, stop the mic tracks, and tell the backend the
   *  session is over. Idempotent; `silent` skips the realtime_stop RPC (unmount
   *  path: silent — no user-visible toast, no re-entrant teardown). */
  const stopRealtime = useCallback(
    (opts?: { silent?: boolean }) => {
      realtimeAttemptRef.current += 1;
      realtimeBusyRef.current = false;
      const hadSession = realtimePcRef.current !== null || realtimeDcRef.current !== null;
      // Capture-then-clear: a concurrent A→B switch must not retarget stop
      // at B. The owner was stored at start; current session is never used.
      const owner = realtimeOwnerRef.current;
      realtimeOwnerRef.current = null;
      const dc = realtimeDcRef.current;
      realtimeDcRef.current = null;
      if (dc) {
        dc.onopen = null;
        dc.onmessage = null;
        dc.onerror = null;
        dc.onclose = null;
        try {
          dc.close();
        } catch {
          /* already closed */
        }
      }
      const pc = realtimePcRef.current;
      realtimePcRef.current = null;
      if (pc) {
        pc.ontrack = null;
        pc.onconnectionstatechange = null;
        try {
          pc.close();
        } catch {
          /* already closed */
        }
      }
      if (mediaStreamRef.current) {
        mediaStreamRef.current.getTracks().forEach((track) => track.stop());
        mediaStreamRef.current = null;
      }
      const audio = realtimeAudioRef.current;
      if (audio) audio.srcObject = null;
      realtimeTranscriptRef.current = '';
      lastCommittedTranscriptRef.current = '';
      const node = realtimeTranscriptNodeRef.current;
      if (node) node.textContent = '';
      setRealtimeDelegation(null);
      cancelPendingDelegationRef.current();
      setRealtimeActive(false);
      setRealtimeConnState(null);
      setRealtimeAudioBlocked(false);
      if (hadSession && !opts?.silent && owner) {
        sendCommand(realtimeStopCommand(owner)).catch(() => {});
      }
    },
    [sendCommand]
  );

  stopRealtimeRef.current = () => stopRealtime();

  /** Start a Codex Live realtime call: mic track -> RTCPeerConnection ->
   *  `oai-events` RTCDataChannel (created BEFORE the offer so it is negotiated
   *  in the SDP) -> SDP offer over the realtime_create_call RPC -> answer.
   *  session.update + server events (transcript/delegation/error) flow over
   *  the data channel, which rides the WebRTC DTLS transport — no browser-side
   *  Bearer header and no HTTPS/http mixed-content block (the reasons the
   *  direct sideband WebSocket was removed). Incoming audio rides the WebRTC
   *  track. */
  const startRealtime = useCallback(async () => {
    if (realtimeActive || realtimeBusyRef.current) return;
    if (liveSettings?.realtimeConfigured !== true) {
      toast(
        'Realtime voice is not configured — set Settings.live.realtimeBaseUrl and realtimeApiKey (and realtimeModel/voice) in the terminal',
        true
      );
      return;
    }
    if (!navigator.mediaDevices?.getUserMedia) {
      toast('microphone requires HTTPS (or localhost). Use https:// or connect via 127.0.0.1.', true);
      return;
    }
    if (typeof RTCPeerConnection === 'undefined') {
      toast('This browser does not support WebRTC (RTCPeerConnection)', true);
      return;
    }
    const owner = sessionIdRef.current;
    if (!owner) {
      toast('realtime voice requires an active session', true);
      return;
    }
    const model = (liveSettings?.realtimeModel ?? '').trim() || 'gpt-realtime-1.5';
    const voice = (liveSettings?.voice ?? '').trim() || 'sol';
    // Capture the owner at START. ICE gather / create-call is async, so a
    // switch to B before POST must still create (and later stop) A's call.
    realtimeOwnerRef.current = owner;
    realtimeBusyRef.current = true;
    const attempt = ++realtimeAttemptRef.current;
    const isCurrentAttempt = () =>
      realtimeAttemptRef.current === attempt
      && realtimeOwnerRef.current === owner
      && sessionIdRef.current === owner;
    try {
      await setupRealtimeCall({
        getUserMedia: () =>
          navigator.mediaDevices.getUserMedia({ audio: true }).then((stream) => {
            if (!isCurrentAttempt()) {
              stream.getTracks().forEach((track) => track.stop());
              throw new Error('realtime setup was superseded');
            }
            mediaStreamRef.current = stream;
            return stream;
          }),
        createPeerConnection: () => new RTCPeerConnection(),
        sendCreateCall: (sdpOffer) =>
          sendCommand(realtimeCreateCallCommand(owner ?? '', sdpOffer)).then((raw) => {
            const r = (raw || {}) as { sdp?: unknown; callId?: unknown };
            const sdp = typeof r?.sdp === 'string' ? r.sdp : '';
            const callId = typeof r?.callId === 'string' ? r.callId : '';
            return { sdp, callId };
          }),
        onPeerConnection: (pcArg) => {
          if (!isCurrentAttempt()) {
            try { pcArg.close(); } catch { /* already closed */ }
            return;
          }
          // Set the pc ref first so a partial-failure catch can release it.
          realtimePcRef.current = pcArg;
          setRealtimeConnState(classifyRealtimeConnectionState(pcArg.connectionState));
          pcArg.ontrack = (event) => {
            const audio = realtimeAudioRef.current;
            if (audio && event.streams.length > 0) {
              audio.srcObject = event.streams[0];
              // Autoplay can reject under the browser's autoplay policy (no
              // prior user gesture, or the gesture was "consumed" by another
              // play). Surface it as an actionable overlay control instead of
              // silently swallowing it — the user clicks #realtime-audio-resume
              // (a fresh gesture) to resume.
              void audio.play().catch(() => {
                if (realtimePcRef.current !== pcArg) return;
                toast('realtime audio blocked by autoplay — tap the mic button to restart', true);
              });
            }
          };
          pcArg.onconnectionstatechange = () => {
            const bucket = classifyRealtimeConnectionState(pcArg.connectionState);
            setRealtimeConnState(bucket);
            // 'disconnected' is a transient ICE path loss (recoverable): toast
            // so the user knows audio/events may stall, but do NOT tear down —
            // ICE can re-connect on its own. 'failed' is terminal: toast and
            // tear the call down. 'closed' from a user-initiated stopRealtime
            // is a no-op (the ref is already null).
            if (bucket === 'disconnected') {
              toast('realtime connection interrupted — reconnecting', true);
            } else if (bucket === 'failed') {
              toast('realtime connection failed', true);
              if (realtimePcRef.current === pcArg) stopRealtime();
            } else if (bucket === 'closed') {
              if (realtimePcRef.current === pcArg) stopRealtime();
            } else if (bucket === 'connected') {
              // Connection (re)established — clear any stale interrupt toast
              // state by leaving the bucket visible; audio block stays as-is.
            }
          };
        },
        onDataChannel: (_pcArg, dcArg) => {
          if (!isCurrentAttempt()) {
            try { dcArg.close(); } catch { /* already closed */ }
            return;
          }
          // Set the dc ref first so a partial-failure catch / onclose can
          // release it; wiring happens before createOffer (the channel is
          // created inside setupRealtimeCall before the offer).
          realtimeDcRef.current = dcArg;
          dcArg.binaryType = 'arraybuffer';
          dcArg.onopen = () => {
            // Re-advertise the same V1 session config sent during create-call
            // so the established data-channel path applies the configured
            // model and voice after connection. The update is idempotent;
            // voice remains nested at audio.output.voice.
            try {
              dcArg.send(JSON.stringify({ type: 'session.update', session: buildRealtimeSessionConfig(model, voice) }));
            } catch {
              /* channel closed between open and send; onclose handles teardown */
            }
          };
          dcArg.onmessage = (event) => {
            for (const frame of parseRealtimeChannelFrames(event.data)) {
              handleRealtimeFrame(frame);
            }
          };
          dcArg.onerror = () => {
            if (realtimeDcRef.current === dcArg) toast('realtime data channel error', true);
          };
          dcArg.onclose = () => {
            // An unexpected close mid-call tears the call down (idempotent: a
            // user-initiated stopRealtime nulls the ref first, so this no-ops).
            if (realtimeDcRef.current === dcArg) stopRealtime();
          };
        },
      });
      if (!isCurrentAttempt()) {
        return;
      }
      lastCommittedTranscriptRef.current = '';
      realtimeTranscriptRef.current = '';
      const node = realtimeTranscriptNodeRef.current;
      if (node) node.textContent = '';
      setRealtimeDelegation(null);
      cancelPendingDelegationRef.current();
      setRealtimeAudioBlocked(false);
      setRealtimeActive(true);
    } catch (err) {
      const shouldSurface = isCurrentAttempt();
      if (shouldSurface) {
        // A partial CURRENT setup leaves refs incrementally assigned; release
        // them without firing realtime_stop because the call never came up.
        stopRealtime({ silent: true });
        toast(`realtime call failed: ${err instanceof Error ? err.message : String(err)}`, true);
      }
    }
  }, [realtimeActive, liveSettings, sendCommand, handleRealtimeFrame, toast, stopRealtime]);


  // If the backend stops advertising realtime mode while a call is up
  useEffect(() => {
    const owner = realtimeOwnerRef.current;
    if (shouldStopRealtimeOnSessionSwitch(owner, owner, sessionId)) stopRealtime();
  }, [sessionId, stopRealtime]);

  // (settings changed), tear the call down cleanly.
  useEffect(() => {
    if (realtimeActive && !isRealtimeMode) stopRealtime();
  }, [realtimeActive, isRealtimeMode, stopRealtime]);

  const onModelChange = useCallback((key: string) => {
    if (!key) return;
    const target = sessionIdRef.current;
    if (!target) return;
    const slash = key.indexOf('/');
    const provider = key.slice(0, slash);
    const modelId = key.slice(slash + 1);
    const decision = requestSessionModel(sessionModelStateRef.current, target, key);
    if (!decision.send) {
      if (sessionModelStateRef.current.pending.has(target) && sessionModelStateRef.current.pending.get(target)?.key !== key) {
        toast('model switch already in progress', true);
      }
      return;
    }
    writeSessionModelState(decision.state);
    const request = decision.state.pending.get(target);
    if (!request) return;
    void modelSettingsQueueRef.current.enqueue(target, async () => {
      let response: unknown;
      try {
        response = await sendCommand({ type: 'set_model', provider, modelId, sessionId: target });
      } catch {
        writeSessionModelState(settleSessionModel(sessionModelStateRef.current, target, request.seq, false));
        if (sessionIdRef.current === target) toast('model change failed', true);
        return;
      }
      const effective = wireModelKey(response as ModelSelectInput) || key;
      writeSessionModelState(settleSessionModel(sessionModelStateRef.current, target, request.seq, true, effective));
      await sendCommand({ type: 'get_state', sessionId: target })
        .then((state) => applyState(state, target, false))
        .catch(() => {});
    });
  }, [applyState, sendCommand, toast, writeSessionModelState]);

  const onThinkingChange = useCallback((level: string) => {
    const display = displayStateRef.current;
    const target = thinkingCommandTargetSessionId({
      commandTargetSessionId: display?.commandTarget.sessionId,
      displayedSessionId: display?.displayed.sessionId,
    }, sessionIdRef.current);
    const decision = requestThinkingLevel(thinkingLevelStateRef.current, target, level, levels);
    if (!decision.send) return;
    writeThinkingLevelState(decision.state);
    const request = decision.state.pending.get(target);
    if (!request) return;
    void modelSettingsQueueRef.current.enqueue(target, async () => {
      let response: unknown;
      try {
        response = await sendCommand(setThinkingLevelCommand(target, level));
      } catch {
        writeThinkingLevelState(settleThinkingRequest(thinkingLevelStateRef.current, target, request.seq, false));
        if (sessionIdRef.current === target) toast('thinking level change failed', true);
        return;
      }
      const effective = response && typeof response === 'object' && 'level' in response
        && typeof response.level === 'string' ? response.level : level;
      writeThinkingLevelState(settleThinkingRequest(thinkingLevelStateRef.current, target, request.seq, true, effective));
      // A backend clamp silently reverts the select — surface the backend's
      // own explanation (non-error toast) so the revert is never read as a
      // silent no-op. Gated to the displayed session like the failure toast.
      if (sessionIdRef.current === target) {
        const clampMessage = thinkingLevelClampMessage(response);
        if (clampMessage !== null) toast(clampMessage, false);
      }
      await sendCommand({ type: 'get_state', sessionId: target })
        .then((state) => applyState(state, target, false))
        .catch(() => {});
    });
  }, [applyState, levels, sendCommand, toast, writeThinkingLevelState]);

  /* ---------------- effects ---------------- */

  // Restore the token for the INITIAL page authority and connect on boot. The
  // legacy global token (rpi-web-token) is migrated to the initial authority's
  // scoped key ONCE and the legacy key deleted — it never leaks to a different
  // host. The ref/state are set synchronously (including empty) so the very
  // first connection already uses the correct token.
  useEffect(() => {
    // QR / link pairing: `#<token>` in the URL is trusted only because it
    // came from the operator's own terminal banner. Save it for this
    // authority, strip the fragment from the address bar (it must not linger
    // in history or shared tabs), and continue the ordinary boot.
    const fragment = window.location.hash.replace(/^#/, '');
    if (fragment !== '') {
      saveTokenForAuthority(tokenStorage, hostRef.current, fragment);
      history.replaceState(null, '', window.location.pathname + window.location.search);
    }
    const saved = loadInitialAuthorityToken(tokenStorage, hostRef.current);
    tokenRef.current = saved;
    setToken(saved);
    // Empty-token boot runs the bounded /status preflight first so a
    // token-gated listener never opens a socket it can only 401; a saved
    // token connects immediately (the probe would only add latency).
    if (saved === '') bootWithPreflight();
    else connect();
    const onWake = () => wakeProbeHandoffRef.current();
    const onVisibility = () => {
      if (document.visibilityState !== 'visible') return;
      onWake();
    };
    document.addEventListener('visibilitychange', onVisibility);
    window.addEventListener('online', onWake);
    window.addEventListener('pageshow', onWake);
    return () => {
      document.removeEventListener('visibilitychange', onVisibility);
      window.removeEventListener('online', onWake);
      window.removeEventListener('pageshow', onWake);
      preflightGenRef.current += 1; // invalidate an in-flight preflight on unmount
      if (restoreViewRafRef.current !== null) window.cancelAnimationFrame(restoreViewRafRef.current);
      restoreViewGenerationRef.current += 1;
      for (const resolve of pendingSwitchResolversRef.current.values()) resolve(false);
      pendingSwitchResolversRef.current.clear();
      pendingSwitchPromisesRef.current.clear();
      if (authoritativeCutTimerRef.current !== null) {
        window.clearTimeout(authoritativeCutTimerRef.current);
        authoritativeCutTimerRef.current = null;
      }
      if (snapshotInProgressRetryTimerRef.current !== null) {
        window.clearTimeout(snapshotInProgressRetryTimerRef.current);
        snapshotInProgressRetryTimerRef.current = null;
      }
      if (retryTimerRef.current !== null) window.clearTimeout(retryTimerRef.current);
      clearAllStreamState();
      clearHeartbeatTimers();
      // Reject every pending ready-gate waiter so its bounded timer does not
      // leak past unmount and no load settles into a torn-down component.
      readyGateRef.current?.clear();
      // The hold re-probe timer must not fire into a torn-down component.
      if (holdReprobeTimerRef.current !== null) {
        window.clearTimeout(holdReprobeTimerRef.current);
        holdReprobeTimerRef.current = null;
      }
      autoResizeRef.current?.cancel();
      // Release the mic/WebRTC/data-channel resources; the realtime_stop RPC is
      // skipped (silent) because the main socket is closing underneath us.
      stopRealtime({ silent: true });
      const ws = wsRef.current;
      wsRef.current = null;
      // Detach the main socket's handlers before close so a late onopen /
      // onmessage / onerror / onclose cannot fire post-unmount (e.g. a
      // CONNECTING socket whose onopen would otherwise bootstrap into a torn-
      // down component).
      if (ws) {
        detachTransportHandlers(ws);
        try {
          ws.close(1000, 'unload');
        } catch {
          /* already closed */
        }
      }
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);


  // Keep the shared drawer CSS variable in sync with state (and the
  // initial localStorage read). Mobile CSS ignores the variable.
  useLayoutEffect(() => {
    applyPanelDrawerVh(panelDrawerVh);
  }, [panelDrawerVh]);

  const onPanelResizerPointerDown = useCallback((e: PointerEvent<HTMLDivElement>) => {
    e.preventDefault();
    const target = e.currentTarget;
    target.setPointerCapture(e.pointerId);
    panelResizeDragRef.current = {
      startY: e.clientY,
      startVh: panelDrawerVhRef.current,
    };
  }, []);

  const onPanelResizerPointerMove = useCallback((e: PointerEvent<HTMLDivElement>) => {
    const drag = panelResizeDragRef.current;
    if (!drag) return;
    // Dragging the top edge UP increases height (negative dy → +vh).
    const dyPx = drag.startY - e.clientY;
    const dyVh = (dyPx / window.innerHeight) * 100;
    const next = clampPanelDrawerVh(drag.startVh + dyVh);
    panelDrawerVhRef.current = next;
    setPanelDrawerVh(next);
    applyPanelDrawerVh(next);
  }, []);

  const endPanelResizeDrag = useCallback((e: PointerEvent<HTMLDivElement>) => {
    if (!panelResizeDragRef.current) return;
    panelResizeDragRef.current = null;
    try {
      e.currentTarget.releasePointerCapture(e.pointerId);
    } catch {
      /* already released */
    }
    writeStoredPanelDrawerVh(panelDrawerVhRef.current);
  }, []);

  const onPanelResizerKeyDown = useCallback((e: KeyboardEvent<HTMLDivElement>) => {
    if (typeof window !== 'undefined' && window.matchMedia('(max-width: 768px)').matches) return;
    const step = e.shiftKey ? 10 : 2;
    let next: number | null = null;
    if (e.key === 'ArrowUp') next = clampPanelDrawerVh(panelDrawerVhRef.current + step);
    else if (e.key === 'ArrowDown') next = clampPanelDrawerVh(panelDrawerVhRef.current - step);
    else if (e.key === 'Home') next = PANEL_DRAWER_MAX_VH;
    else if (e.key === 'End') next = PANEL_DRAWER_MIN_VH;
    if (next == null) return;
    e.preventDefault();
    panelDrawerVhRef.current = next;
    setPanelDrawerVh(next);
    applyPanelDrawerVh(next);
    writeStoredPanelDrawerVh(next);
  }, []);

  // ---- Files explorer OUTER drawer width resizer (desktop only) ----
  // Mirror of the shared panel height resizer above: pointer capture on down,
  // clamp-on-move, persist once on up. The resizer sits at the drawer's LEFT
  // edge, so dragging leftward (startX - clientX > 0) GROWS the drawer and
  // ArrowLeft grows it too.
  const onFileExplorerResizerPointerDown = useCallback((e: PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return;
    // Desktop only — mobile never shows the resizer, but guard anyway.
    if (typeof window !== 'undefined' && window.matchMedia('(max-width: 720px)').matches) return;
    e.preventDefault();
    e.currentTarget.setPointerCapture(e.pointerId);
    fileExplorerResizeDragRef.current = {
      startX: e.clientX,
      startWidth: fileExplorerWidthRef.current,
    };
  }, []);

  const onFileExplorerResizerPointerMove = useCallback((e: PointerEvent<HTMLDivElement>) => {
    const drag = fileExplorerResizeDragRef.current;
    if (!drag) return;
    // Dragging the left edge LEFT increases width (negative dx → grow).
    const dxPx = drag.startX - e.clientX;
    const next = clampFileExplorerWidth(drag.startWidth + dxPx, window.innerWidth, fileExplorerSidebarPx);
    fileExplorerWidthRef.current = next;
    setFileExplorerWidth(next);
    applyFileExplorerWidth(next, window.innerWidth, fileExplorerSidebarPx);
  }, [fileExplorerSidebarPx]);

  const endFileExplorerResizeDrag = useCallback((e: PointerEvent<HTMLDivElement>) => {
    if (!fileExplorerResizeDragRef.current) return;
    fileExplorerResizeDragRef.current = null;
    try {
      e.currentTarget.releasePointerCapture(e.pointerId);
    } catch {
      /* already released */
    }
    writeStoredFileExplorerWidth(fileExplorerWidthRef.current, window.innerWidth, fileExplorerSidebarPx);
  }, [fileExplorerSidebarPx]);

  const onFileExplorerResizerKeyDown = useCallback((e: KeyboardEvent<HTMLDivElement>) => {
    if (typeof window !== 'undefined' && window.matchMedia('(max-width: 720px)').matches) return;
    let next: number | null = null;
    if (e.key === 'ArrowLeft') next = stepFileExplorerWidth(fileExplorerWidthRef.current, 1, window.innerWidth, fileExplorerSidebarPx);
    else if (e.key === 'ArrowRight') next = stepFileExplorerWidth(fileExplorerWidthRef.current, -1, window.innerWidth, fileExplorerSidebarPx);
    else if (e.key === 'Home') next = fileExplorerWidthCeiling(window.innerWidth, fileExplorerSidebarPx);
    else if (e.key === 'End') next = FILE_EXPLORER_WIDTH_MIN;
    if (next == null) return;
    e.preventDefault();
    fileExplorerWidthRef.current = next;
    setFileExplorerWidth(next);
    applyFileExplorerWidth(next, window.innerWidth, fileExplorerSidebarPx);
    writeStoredFileExplorerWidth(next, window.innerWidth, fileExplorerSidebarPx);
  }, [fileExplorerSidebarPx]);

  /* ---------------- render ---------------- */

  const openNewSession = () => {
    openPanel('session', { force: true });
    focusNewSessionCwdInput();
  };
  useDesktopActions({
    'new-workspace': openNewSession,
    'new-session': openNewSession,
    settings: () => openPanel('settings'),
    attach: sessionId ? () => fileInputRef.current?.click() : undefined,
    stop: stopArmed ? abortActiveRun : undefined,
  });

  return (
    <>
      <ReconnectOverlay visible={reconnectOverlayVisible(connState)} text={reconnectOverlayText(connState)} />
      <header ref={appHeaderRef} hidden={filesFullscreen}>
        <div className="brand">
          rpi<span className="brand-sub">web</span>
        </div>
        <HostConnectionChip
          connState={connState}
          stableError={connState === 'token_required' ? HOST_TOKEN_REQUIRED_ERROR : ''}
          streamStall={activeStreamStall}
        />
        <span
          id="compact-badge"
          className="badge badge--compact"
          hidden={!activeCompacting}
          data-mode={activeCompactMode ?? undefined}
        >
          {compactBadgeLabel(activeCompactMode)}
        </span>

        <HeaderSelect
          id="model-select"
          title="Model (set_model)"
          disabled={switchPending || (models.length === 0 && activeModelKey === '')}
          value={activeModelKey}
          placeholder="model…"
          groups={modelGroups}
          onChange={onModelChange}
        />
        <HeaderSelect
          id="thinking-select"
          title="Thinking level (set_thinking_level)"
          disabled={switchPending || levels.length === 0}
          value={activeThinkingLevel}
          placeholder="thinking…"
          levels={levels}
          onChange={onThinkingChange}
        />
        <button
          type="button"
          id="sidebar-toggle-btn"
          className={sidebarOpen ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
          aria-pressed={sidebarOpen}
          title="Toggle the session sidebar"
          onClick={() => setSidebarOpen((open) => !open)}
        >
          ☰
        </button>
      </header>

      <div className={`app-layout${sidebarOpen ? ' app-layout--drawer-open' : ''}${fileExplorerOverlay ? ' app-layout--files-overlay' : ''}${filesFullscreen ? ' app-layout--files-expanded' : ''}${fileExplorerSlot.mode === 'open' ? ' app-layout--files-open' : fileExplorerSlot.mode === 'collapsed' ? ' app-layout--files-collapsed' : ''}${activePanel === 'code-review' ? ' app-layout--code-review' : ''}`}>
        <div
          id="session-lifecycle"
          data-displayed-session-id={lifecycleView.displayedSessionId}
          data-command-session-id={lifecycleView.commandSessionId}
          data-pending-generation={lifecycleView.pendingGeneration === null ? '' : String(lifecycleView.pendingGeneration)}
          data-pending-phase={lifecycleView.pendingPhase ?? ''}
          data-authoritative-ready={lifecycleView.authoritativeReady ? 'true' : 'false'}
          data-state-messages={lifecycleView.stateMessages}
          data-projected-dom={lifecycleView.projectedDom}
        />
        {sidebarOpen && (
          <div className="sidebar-backdrop" onClick={() => setSidebarOpen(false)} />
        )}
        <SessionSidebar
          sendCatalogCommand={sendCatalogCommand}
          sendCommand={sendCommand}
          downloadSessionExport={(downloadId, fileName) => downloadSessionExport(
            downloadId,
            fileName,
            {
              hostAuthority: hostRef.current,
              pageProtocol: location.protocol,
            },
          )}
          waitForReady={waitForReady}
          onLifecycleResult={onLifecycleResult}
          onSwitchSession={requestSessionSwitch}
          catalogRefreshRevision={catalogRefreshRevision}
          switchPending={switchPending}
          lifecycleRowOverlay={lifecycleRow}
          onLifecycleRowAbsorbed={clearLifecycleRow}
          getCompactionSummary={getCompactionSummary}
          requestSessionCompactSummary={requestSessionCompactSummary}
          activeSessionId={sessionId}
          isSessionLoaded={isSessionLoaded}
          onSessionCwdChanged={onSessionCwdChanged}
          onSessionDeleted={deleteFileExplorerSlot}
          unreadBySessionId={unreadBySessionId}
          streamingBySessionId={streamingBySessionId}
          onCollapse={() => setSidebarOpen(false)}
          onReopenRail={() => setSidebarOpen(true)}
          onOpenManage={openNewSession}
          onSwitchComplete={() => {
            // Mobile: closing the drawer after a pick; desktop keeps the rail.
            if (window.matchMedia('(max-width: 720px)').matches) setSidebarOpen(false);
          }}
          onToast={toast}
          featureNav={
            <>
              <a
                id="chat-console-link"
                className="panel-toggle"
                href="/chat"
                title="Open chat"
                aria-label="Open chat"
              >
                Chat
              </a>
              <button
                id="todo-toggle-btn"
                type="button"
                className={activePanel === 'todo' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'todo'}
                title="Todo module (durable child sessions and ordinary tasks)"
                aria-label={activePanel === 'todo' ? 'Close Todo module' : 'Open Todo module'}
                onClick={() => openPanel('todo')}
              >
                Todo
              </button>
              <button
                id="activity-toggle-btn"
                type="button"
                className={activePanel === 'activity' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'activity'}
                title="Ordinary session activity (jobs and orchestration messages)"
                aria-label={activePanel === 'activity' ? 'Close Activity panel' : 'Open Activity panel'}
                onClick={() => openPanel('activity')}
              >
                Activity
              </button>
              <button
                id="goal-panel-btn"
                type="button"
                className={activePanel === 'goal' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'goal'}
                title="Goal panel (objective, status, token budget, pins, journal)"
                aria-label={activePanel === 'goal' ? 'Close Goal panel' : 'Open Goal panel'}
                onClick={() => openPanel('goal')}
              >
                Goal
              </button>
              <button
                id="workflow-toggle-btn"
                type="button"
                className={activePanel === 'workflow' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'workflow'}
                title="Workflow panel (list, detail, live workers, create/pause/resume/cancel/integrate/remove)"
                aria-label={activePanel === 'workflow' ? 'Close Workflows panel' : 'Open Workflows panel'}
                onClick={() => openPanel('workflow')}
              >
                Workflows
              </button>
              <button
                id="sidechat-toggle-btn"
                type="button"
                className={activePanel === 'sidechat' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'sidechat'}
                title="Side chat: parallel /btw sessions (own tab, transcript, prompt)"
                aria-label={activePanel === 'sidechat' ? 'Close Side chat panel' : 'Open Side chat panel'}
                onClick={() => openPanel('sidechat')}
              >
                Side chat
              </button>
              <button
                id="subagents-toggle-btn"
                type="button"
                className={activePanel === 'subagents' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'subagents'}
                title="Subagents: live jobs (id/type/status/activity/elapsed), spawn task, hub message, cancel, output view"
                aria-label={activePanel === 'subagents' ? 'Close Subagents panel' : 'Open Subagents panel'}
                onClick={() => openPanel('subagents')}
              >
                Subagents
              </button>
              <button
                id="personas-toggle-btn"
                type="button"
                className={activePanel === 'personas' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'personas'}
                title="Personas: persistent persona definitions (list/view/create/edit/remove/purge/select/run)"
                aria-label={activePanel === 'personas' ? 'Close Personas panel' : 'Open Personas panel'}
                onClick={() => openPanel('personas')}
              >
                Personas
              </button>
              <button
                id="session-toggle-btn"
                type="button"
                className={activePanel === 'session' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'session'}
                title="Session panel: current session info, new/switch/fork/clone/rename"
                aria-label={activePanel === 'session' ? 'Close Session panel' : 'Open Session panel'}
                onClick={() => openPanel('session')}
              >
                Session
              </button>
              <button
                id="loop-toggle-btn"
                type="button"
                className={activePanel === 'loop' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'loop'}
                title="Loop panel: create, list, and cancel recurring loop prompts"
                aria-label={activePanel === 'loop' ? 'Close Loop panel' : 'Open Loop panel'}
                onClick={() => openPanel('loop')}
              >
                Loop
              </button>
              <button
                id="processes-toggle-btn"
                type="button"
                className={activePanel === 'processes' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'processes'}
                title="Processes panel: spawn, inspect output, and stop supervised processes"
                aria-label={activePanel === 'processes' ? 'Close Processes panel' : 'Open Processes panel'}
                onClick={() => openPanel('processes')}
              >
                Processes
              </button>
              <button
                id="file-explorer-toggle-btn"
                type="button"
                className={fileExplorerOpen ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={fileExplorerOpen}
                title="File explorer: workspace tree and selected file"
                aria-label={fileExplorerOpen ? 'Close file explorer' : 'Open file explorer'}
                onClick={toggleFileExplorer}
              >
                Files
              </button>
              <button
                id="settings-toggle-btn"
                type="button"
                className={activePanel === 'settings' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'settings'}
                title="Settings panel: browse by category, typed edit, draft/apply"
                aria-label={activePanel === 'settings' ? 'Close Settings panel' : 'Open Settings panel'}
                onClick={() => openPanel('settings')}
              >
                Settings
            </button>
          </>
        }
        />
        <div className="app-main">
      <main id="transcript" aria-live="polite" data-switch-pending={switchPending ? 'true' : undefined} ref={transcriptRef} onScroll={handleTranscriptScroll}>
        {surfaceKeys.length === 0 && connState === 'token_required' && (
          <div className="empty-hint">
            This listener requires an access token. Paste it in the pairing card.
            No reconnect is attempted until the token is saved.
          </div>
        )}
        {surfaceKeys.map((rk) => {
          const identity = parseRuntimeKey(rk);
    if (!identity) return null;
    return (
      <TranscriptSurface
              key={rk}
              rk={rk}
              sessionId={identity.sessionId}
              items={itemsBySessionId[identity.sessionId] ?? EMPTY_ITEMS}
              itemsVersion={itemVersionsBySessionId[identity.sessionId] ?? 0}
              active={rk === displayedRuntimeKey}
              connState={connState}
              workspaceCwd={sessionCwdBySessionId[identity.sessionId] ?? ''}
              initialStart={initialStartBySurfaceRef.current.get(rk) ?? tailProjectionStart((itemsBySessionId[identity.sessionId] ?? EMPTY_ITEMS).length)}
              getViewport={getSurfaceViewport}
              scrollRef={transcriptRef}
              pinIfPinned={pinIfPinned}
              forcePin={forcePin}
              onContentEl={handleSurfaceContentEl}
              onCommit={handleSurfaceCommit}
              getAnchorIndex={getSurfaceAnchorIndex}
            />
          );
        })}
        {switchPending && <TranscriptSkeleton />}
      </main>

      {/* panel mount point — every panel is KEYED by (panel, session) so its
          internal drafts/controllers (todo form, goal objective, workflow
          create fields, side-chat prompt, settings draft, subagent message
          drafts) can never survive an A→B session switch as
          though they belonged to B. Each panel re-fetches from the backend
          for the active session on mount (sendCommand injects the top-level
          sessionId).

          Ordinary panels share ONE desktop height resizer (ARIA separator)
          mounted here — never per-panel handlers. Code review is excluded
          (full-viewport, owns its own thread-column resizer). */}
      {activePanel !== '' && activePanel !== 'code-review' && activePanel !== 'sidechat' && (
        <div
          id="panel-drawer-resizer"
          className="panel-drawer-resizer"
          role="separator"
          aria-orientation="horizontal"
          aria-label="Resize panel height"
          aria-valuemin={PANEL_DRAWER_MIN_VH}
          aria-valuemax={PANEL_DRAWER_MAX_VH}
          aria-valuenow={Math.round(panelDrawerVh)}
          tabIndex={0}
          onPointerDown={onPanelResizerPointerDown}
          onPointerMove={onPanelResizerPointerMove}
          onPointerUp={endPanelResizeDrag}
          onPointerCancel={endPanelResizeDrag}
          onKeyDown={onPanelResizerKeyDown}
        />
      )}
      {activePanel === 'todo' && (
        <TodoPanel
          key={`todo:${sessionId ?? ''}`}
          phases={activeTodoSnapshot.phases}
          executionStatus={activeTodoSnapshot.executionStatus}
          failureReason={activeTodoSnapshot.failureReason}
          navigation={activeTodoNavigation}
          onNavigationChange={(navigation) => {
            setTodoNavigationByKey((prev) => ({ ...prev, [activeTodoNavigationKey]: navigation }));
          }}
          onOp={runTodoOp}
          parentSessionId={sessionId ?? ''}
          sendCommand={sendCommand}
          onClose={() => setActivePanel('')}
        />
      )}
      {activePanel === 'activity' && (
        <ActivityPanel
          key={`activity:${sessionId ?? ''}`}
          activity={activeActivitySnapshot.entries}
          navigation={activeActivityNavigation}
          onNavigationChange={(navigation) => {
            setActivityNavigationByKey((prev) => ({ ...prev, [activeActivityNavigationKey]: navigation }));
          }}
          onClose={() => setActivePanel('')}
        />
      )}
      {activePanel === 'subagents' && (
        <SubagentsPanel
          key={`subagents:${sessionId ?? ''}:${subagentsInitialPage}`}
          sendCommand={sendCommand}
          connectionEpoch={connectionEpoch}
          subscribeEvents={subscribeSubagentsEvents}
          initialPage={subagentsInitialPage}
          onClose={() => setActivePanel('')}
        />
      )}
      {activePanel === 'personas' && (
        <PersonasPanel
          key={`personas:${sessionId ?? ''}`}
          sendCommand={sendCommand}
          onClose={() => setActivePanel('')}
        />
      )}
      {activePanel === 'goal' && (
        <GoalPanel
          key={`goal:${sessionId ?? ''}`}
          state={activeGoalState}
          journal={activeGoalJournal}
          sendCommand={sendCommand}
          onChanged={() => refreshGoal(sessionIdRef.current)}
          onClose={() => setActivePanel('')}
        />
      )}
      {activePanel === 'workflow' && sessionId && (
        <WorkflowPanel
          key={`workflow:${sessionId}`}
          sessionId={sessionId}
          sendCommand={sendCommand}
          subscribeEvents={subscribeWorkflowEvents}
          onClose={() => setActivePanel('')}
        />
      )}
      {activePanel === 'sidechat' && (
        <SideChatPanel
          key={`sidechat:${sessionId ?? ''}`}
          snapshot={activeSideChat}
          onNew={sideChatNew}
          onSwitch={sideChatSwitch}
          onClose={sideChatClose}
          onPrompt={sideChatPrompt}
          onAbort={sideChatAbort}
          onClosePanel={() => setActivePanel('')}
          promptTextareaRef={sideChatPromptInputRef}
          insertTranscriptRef={sideChatInsertTranscriptRef}
          isRecording={recording}
          isTranscribing={transcribing}
          onStartRecording={startSideChatSttRecording}
          onStopRecording={stopRecording}
          onToast={toast}
        />
      )}
      {activePanel === 'session' && (
        <SessionPanel
          key={`session:${sessionId ?? ''}`}
          sessionId={sessionId ?? ''}
          sendCommand={sendCommand}
          waitForReady={waitForReady}
          refreshState={refreshState}
          onLifecycleResult={onLifecycleResult}
          onToast={toast}
          onSessionHandoff={onSessionHandoff}
          onClose={() => setActivePanel('')}
        />
      )}
      {activePanel === 'loop' && (
        <LoopPanel
          key={`loop:${sessionId ?? ''}`}
          sendCommand={sendCommand}
          onClose={() => setActivePanel('')}
        />
      )}
      {activePanel === 'processes' && (
        <ProcessesPanel
          key={`processes:${sessionId ?? ''}`}
          sendCommand={sendCommand}
          onClose={() => setActivePanel('')}
          workspaceCwd={activeSessionCwd ?? ''}
        />
      )}
      {connState === 'token_required' && (
        <TokenPairingCard authority={hostRef.current} onTokenChange={handleTokenChange} />
      )}
      {activePanel === 'settings' && (
        <SettingsPanel
          key={`settings:${sessionId ?? ''}`}
          sendCommand={sendCommand}
          refreshState={refreshState}
          catalogEnabled={connState === 'on'}
          token={token}
          onTokenChange={handleTokenChange}
          onClose={() => setActivePanel('')}
        />
      )}
      {/* A review belongs to the session that opened it, and the session the
          user is looking at is the DISPLAY's authority — not the active
          session ref, which a warm reconnect re-binds to the listener primary
          while the displayed surface stays where the user was (see
          ./composerSubmit composerCommandSessionId). The owner follows the
          display, so a cross-session switch remounts the panel under the new
          session (its unmount stamps the previous owner's close) and a
          reconnect that keeps the displayed session keeps this panel. */}
      {activePanel === 'code-review' && (
        <CodeReviewPanel
          key={`code-review:${lifecycleView.displayedSessionId || sessionId || ''}`}
          connectionEpoch={connectionEpoch}
          sendCommand={sendCommand}
          sessionId={lifecycleView.displayedSessionId || sessionId}
          openArgs={codeReviewOpenArgs}
          liveSettings={liveSettings}
          onToast={toast}
          onClose={() => {
            setActivePanel('');
            setCodeReviewOpenArgs({});
          }}
        />
      )}
      {fileExplorerOpen && (
        <FileExplorerPanel
          key={`file-explorer:${sessionId ?? ''}`}
          sendCommand={sendCommand}
          sessionId={sessionId}
          connectionEpoch={connectionEpoch}
          workspaceCwd={activeSessionCwd}
          slot={fileExplorerSlot}
          hostAuthority={hostRef.current}
          pageProtocol={typeof location !== 'undefined' ? location.protocol : 'http:'}
          downloadOrigin={typeof location !== 'undefined' ? location.origin : `http://${hostRef.current}`}
          onOpenPath={(path, line, view) => openFileInDrawer(path, line, sessionId ?? undefined, view)}
          expanded={fileExplorerExpanded}
          onToggleExpanded={() => setFileExplorerExpanded((prev) => !prev)}
          onPatchSlot={(updater) => { if (sessionId) patchFileExplorer(sessionId, updater); }}
          onConversationError={(message) => toast(message, true)}
          onClose={() => {
            if (!sessionId) return;
            patchFileExplorer(sessionId, (slot) => applyDrawerMode(slot, 'closed'));
          }}
        />
      )}
      {/* OUTER drawer width resizer (desktop only; CSS hides it ≤720px).
          Sits flush with the drawer's LEFT edge; writes --file-explorer-width
          (the shared reservation/drawer width authority) on drag and
          Left/Right/Home/End keyboard. Kept INSIDE the fileExplorerOpen
          render branch so the seam control only exists with the drawer. */}
      {fileExplorerOpen && (
        <div
          id="file-explorer-resizer"
          className="file-explorer-resizer"
          role="separator"
          aria-orientation="vertical"
          aria-label="Resize file explorer width"
          aria-valuemin={FILE_EXPLORER_WIDTH_MIN}
          aria-valuemax={typeof window !== 'undefined' ? Math.round(fileExplorerWidthCeiling(window.innerWidth, fileExplorerSidebarPx)) : FILE_EXPLORER_WIDTH_MAX}
          aria-valuenow={fileExplorerWidth}
          tabIndex={0}
          onPointerDown={onFileExplorerResizerPointerDown}
          onPointerMove={onFileExplorerResizerPointerMove}
          onPointerUp={endFileExplorerResizeDrag}
          onPointerCancel={endFileExplorerResizeDrag}
          onKeyDown={onFileExplorerResizerKeyDown}
        />
      )}

      <footer
        ref={appFooterRef}
        data-drop-active={dropActive ? 'true' : undefined}
        onDragEnter={(e) => {
          // Only file drags activate the drop target; text-drag defaults pass
          // through to the textarea. Some Chromium synthetic DataTransfers
          // expose files but omit the `Files` type, so accept either signal.
          if (e.dataTransfer.files.length === 0 && !Array.from(e.dataTransfer.types).includes('Files')) return;
          e.preventDefault();
          e.dataTransfer.dropEffect = 'copy';
          dragDepthRef.current += 1;
          setDropActive(true);
        }}
        onDragOver={(e) => {
          // HTML5 DnD: `drop` fires only when `dragover` is canceled — keep
          // the composer a live drop target for the whole drag.
          if (e.dataTransfer.files.length === 0 && !Array.from(e.dataTransfer.types).includes('Files')) return;
          e.preventDefault();
          e.dataTransfer.dropEffect = 'copy';
        }}
        onDragLeave={(e) => {
          if (e.dataTransfer.files.length === 0 && !Array.from(e.dataTransfer.types).includes('Files')) return;
          e.preventDefault();
          dragDepthRef.current = Math.max(0, dragDepthRef.current - 1);
          if (dragDepthRef.current === 0) setDropActive(false);
        }}
        onDrop={(e) => {
          if (e.dataTransfer.files.length === 0 && !Array.from(e.dataTransfer.types).includes('Files')) return;
          e.preventDefault();
          dragDepthRef.current = 0;
          setDropActive(false);
          void onFilesChosen(e.dataTransfer.files);
        }}
      >
        {dropActive && (
          <div className="composer-drop" aria-hidden="true">
            <span className="composer-drop__hint">Drop images, videos, PDFs, or code files to attach</span>
          </div>
        )}
        <div id="composer-main">
          {realtimeActive && (
            <div id="realtime-transcript">
              <div className="realtime-transcript__head">
                <span className="realtime-transcript__dot" aria-hidden="true" />
                <span className="realtime-transcript__label">realtime voice</span>
                <span id="realtime-conn-state" data-state={realtimeConnState || ''}>{realtimeConnState || '--'}</span>
              </div>
              {realtimeDelegation ? (
                <div className="realtime-transcript__delegation">{realtimeDelegation}</div>
              ) : null}
              <div className="realtime-transcript__text" ref={realtimeTranscriptNodeRef} />
            </div>
          )}

          {attachments.length > 0 && (
            <div id="composer-attachments" aria-label="Attached files">
              {attachments.map((attachment) => {
                const isVideo = attachment.kind === 'video';
                const isPdf = attachment.kind === 'pdf';
                const uploadFailed = (isVideo || isPdf) && attachment.videoState === 'error';
                return (
                  <span
                    key={attachment.id}
                    className={`composer-attachment${uploadFailed ? ' composer-attachment--error' : ''}`}
                    title={attachment.name}
                  >
                    {isVideo && attachment.previewUrl ? (
                      <img className="composer-attachment__thumb" src={attachment.previewUrl} alt="" />
                    ) : isVideo ? (
                      <span className="composer-attachment__badge composer-attachment__badge--video">VIDEO</span>
                    ) : isPdf ? (
                      <span className="composer-attachment__badge">PDF</span>
                    ) : attachment.kind === 'image' && attachment.previewUrl ? (
                      <img className="composer-attachment__thumb" src={attachment.previewUrl} alt="" />
                    ) : (
                      <span className="composer-attachment__badge">
                        {codeBadgeLabel(attachment.name)}
                      </span>
                    )}
                    <span className="composer-attachment__meta">
                      <span className="composer-attachment__name">{safeText(attachment.name)}</span>
                      {(isVideo || isPdf) && attachment.videoState !== 'ready' ? (
                        <span
                          className="composer-attachment__status"
                          data-state={attachment.videoState === 'error' ? 'error' : 'uploading'}
                          role="status"
                          aria-live="polite"
                        >
                          {attachment.videoState === 'error'
                            ? (attachment.videoError ?? 'processing failed')
                            : 'uploading & processing…'}
                        </span>
                      ) : isVideo ? (
                        <span className="composer-attachment__size">{videoMetaLabel(attachment)}</span>
                      ) : (
                        <span className="composer-attachment__size">{attachment.size} bytes</span>
                      )}
                    </span>
                    {canSendAttachmentToTmux(attachment) && (
                      <button
                        type="button"
                        className="composer-attachment__tmux"
                        title="Send to terminal"
                        aria-label={`Send ${attachment.name} to terminal`}
                        onClick={() => {
                          void sendComposerAttachmentToTmux(attachment, {
                            loadToken: (authority) => loadTokenForAuthority(tokenStorage, authority),
                            pageProtocol: typeof location !== 'undefined' ? location.protocol : 'http:',
                          }).then((result) => {
                            if (result.ok) return;
                            toast(result.message, true);
                          });
                        }}
                      >
                        <span aria-hidden="true">↗</span>
                      </button>
                    )}
                    <button
                      type="button"
                      className="composer-attachment__remove"
                      title="Remove attachment"
                      aria-label={`Remove ${attachment.name}`}
                      onClick={() => removeAttachment(attachment.id)}
                    >
                      ✕
                    </button>
                  </span>
                );
              })}
            </div>
          )}
          {(activeActivityStripView || activeSteering.length > 0 || activeFailedSteering.length > 0) && (
            <div className="composer-status-row" id="composer-status-row">
          {activeActivityStripView && (
            <div
              id="activity-strip"
              role="status"
              aria-live="polite"
              className={activeActivityStripView.state === 'reconnecting' ? 'chat-slow-pulse' : undefined}
              data-state={activeActivityStripView.state}
              data-session-id={sessionId ?? undefined}
              data-turn-id={activeActivityStrip?.turnId || undefined}
              data-tool-call-id={activeActivityStrip?.tools.at(-1)?.id || undefined}
              data-job-id={activeActivityStrip?.children.at(-1)?.jobId || undefined}
              data-agent-id={activeActivityStrip?.children.at(-1)?.agentId || undefined}
              title={activeActivityStripView.text}
            >
              <span className="activity-strip__dot" aria-hidden="true" />
              <span className="activity-strip__text">{activeActivityStripView.text}</span>
            </div>
          )}
          <SteeringQueue
            previews={activeSteering}
            failedPreviews={activeFailedSteering}
            onDismissFailedAt={(index) => {
              if (!sessionId) return;
              setFailedSteeringBySessionId((prev) => {
                const current = prev[sessionId];
                if (!current) return prev;
                const next = current.filter((_, i) => i !== index);
                if (next.length === current.length) return prev;
                return { ...prev, [sessionId]: next };
              });
            }}
            busy={steeringBusy}
            onSendNow={sendSteeringNow}
            onCancelAt={cancelSteeringAt}
          />
            </div>
          )}
          <div id="composer-row">
            <CommandPicker
              connected={connState === 'on'}
              sendCommand={sendCommand}
              onSelect={insertCommandText}
              onSkillSelect={stageSkillCommandText}
              onError={(message) => toast(message, true)}
              getComposerValue={() => promptInputRef.current?.value ?? ''}
              onCatalog={rememberExtensionCatalog}
            />
            <textarea
              id="prompt-input"
              ref={promptInputRef}
              rows={1}
              enterKeyHint="send"
              placeholder="Message the agent… (Enter to send, Shift+Enter for a newline, Esc to stop)"
              onCompositionStart={() => {
                composerComposingRef.current = true;
              }}
              onCompositionEnd={() => {
                composerComposingRef.current = false;
              }}
              onKeyDown={(e) => {
                if ((e.ctrlKey || e.metaKey) && !e.altKey && (e.key === 'v' || e.key === 'V')) {
                  pasteEnterGuardUntilRef.current = nextPasteEnterGuard(performance.now());
                }
                if (e.key === 'Escape') {
                  abortActiveRun();
                  return;
                }
                const enterDecision = {
                  key: e.key,
                  shiftKey: e.shiftKey,
                  isComposing: e.nativeEvent.isComposing,
                  keyCode: e.nativeEvent.keyCode,
                  which: e.nativeEvent.which,
                  compositionActive: composerComposingRef.current,
                  now: performance.now(),
                  pasteGuardUntil: pasteEnterGuardUntilRef.current,
                };
                if (composerEnterShouldPreventDefault(enterDecision)) {
                  e.preventDefault();
                }
                if (composerEnterShouldSubmit(enterDecision)) {
                  composerEnterHandledRef.current = true;
                  queueMicrotask(() => {
                    composerEnterHandledRef.current = false;
                  });
                  submitComposerEnter();
                }
              }}
              onBeforeInput={(e) => {
                if (
                  !composerBeforeInputShouldSubmit({
                    inputType: e.nativeEvent.inputType,
                    shiftKey: nativeEventShiftKey(e.nativeEvent),
                    compositionActive: composerComposingRef.current,
                    keydownHandled: composerEnterHandledRef.current,
                    now: performance.now(),
                    pasteGuardUntil: pasteEnterGuardUntilRef.current,
                  })
                ) {
                  return;
                }
                e.preventDefault();
                submitComposerEnter();
              }}
              onInput={(e) => autoResize(e.currentTarget)}
              // File paste uses the normal attachment reader. Multiline or
              // large plain-text paste becomes a text attachment so Chromium
              // never lays out the glyphs (or synthesizes Enter-to-send).
              onPaste={onComposerPaste}
            />
            <CommitPicker
              open={commitPickerOpen}
              rows={commitRows}
              loading={commitsLoading}
              onSelect={(ref) => {
                const owner = commitPickerLoadRef.current.ownerSessionId;
                const currentOwner = composerCommandSessionId(displayStateRef.current?.commandTarget, sessionIdRef.current);
                if (!owner || owner !== currentOwner) {
                  closeCommitPicker();
                  return;
                }
                closeCommitPicker();
                void openCodeReview({ commit: ref });
              }}
              onClose={closeCommitPicker}
              onError={(message) => toast(message, true)}
            />
        <div id="composer-buttons">
          {/* Context-window usage meter (TUI composer-footer parity). Always
              mounted so the layout never reflows when a snapshot/turn end flips
              `hidden`; hidden when the session has no model context window. */}
          <span
            id="composer-context"
            className="composer-context"
            role="status"
            aria-label={activeContextMeterAccessible ? `Context window usage: ${activeContextMeterAccessible}` : 'Context window usage'}
            title={activeContextMeterAccessible ? `Context window usage: ${activeContextMeterAccessible}` : 'Context window usage'}
            data-state={activeContextMeter === '' ? 'none' : 'live'}
            hidden={activeContextMeter === ''}
          >
            {/* All breakpoints render the bar form (`.composer-context__text`
                is display:none): thin usage bar + tiny percent label +
                abbreviated total. Fill steps accent → warn (≥70%) → error (≥90%). */}
            <span className="composer-context__text">{activeContextMeter}</span>
            {activeContextMeterMobile !== null && (
              <>
                <span
                  className={`composer-context__bar${activeContextMeterMobile.percent >= 90 ? ' composer-context__bar--hot' : activeContextMeterMobile.percent >= 70 ? ' composer-context__bar--warn' : ''}`}
                  aria-hidden="true"
                >
                  <span
                    className="composer-context__fill"
                    style={{ width: `${activeContextMeterMobile.percent}%` }}
                  />
                </span>
                <span className="composer-context__percent">{activeContextMeterMobile.percent}%</span>
                <span className="composer-context__total">{activeContextMeterMobile.total}</span>
              </>
            )}
          </span>
          <button
            id="attach-btn"
            type="button"
            title="Attach images, videos, or code/text files"
            aria-label="Attach files"
            onClick={() => fileInputRef.current?.click()}
          >
            <span aria-hidden="true">📎</span>
          </button>
          <button
            id="mic-btn"
            type="button"
            aria-label="Microphone"
            aria-pressed={isRealtimeMode ? realtimeActive : recording}
            disabled={!isRealtimeMode && transcribing}
            className={
              isRealtimeMode
                ? realtimeActive
                  ? 'recording'
                  : ''
                : recording
                  ? 'recording'
                  : transcribing
                    ? 'transcribing'
                    : ''
            }
            title={
              isRealtimeMode
                ? realtimeActive
                  ? 'Realtime voice on — click to stop'
                  : 'Realtime voice — click to talk'
                : recording
                  ? 'Recording… release to transcribe'
                  : transcribing
                    ? 'Transcribing…'
                    : 'Hold to talk (voice to text)'
            }
            onClick={
              isRealtimeMode
                ? () => {
                    if (realtimeActive) stopRealtime();
                    else void startRealtime();
                  }
                : undefined
            }
            onPointerDown={
              isRealtimeMode
                ? undefined
                : (e) => {
                    if (e.button !== 0) return;
                    e.preventDefault();
                    void startMainSttRecording();
                  }
            }
            onPointerUp={
              isRealtimeMode
                ? undefined
                : (e) => {
                    if (e.button !== 0) return;
                    e.preventDefault();
                    stopRecording();
                  }
            }
            onPointerLeave={isRealtimeMode ? undefined : stopRecording}
            onPointerCancel={isRealtimeMode ? undefined : stopRecording}
            onContextMenu={(e) => e.preventDefault()}
          >
            <span aria-hidden="true">🎤</span>
            {(isRealtimeMode ? realtimeActive : recording) && <span className="mic-indicator" aria-hidden="true" />}
          </button>
          <button
            id="send-btn"
            type="button"
            className={
              stopArmed
                ? 'composer-action composer-action--stop'
                : activeCompacting
                  ? 'composer-action composer-action--busy'
                  : 'composer-action composer-action--send'
            }
            disabled={!stopArmed && (switchPending || activeCompacting || connState !== 'on')}
            aria-disabled={!stopArmed && (switchPending || activeCompacting || connState !== 'on') ? true : undefined}
            data-aborting={composerAbortChrome(activeAborting).dataAborting}
            aria-pressed={composerAbortChrome(activeAborting).ariaPressed}
            aria-label={
              stopArmed
                ? 'Stop generating'
                : activeCompacting
                  ? compactStatusText(activeCompactMode)
                  : 'Send message'
            }
            title={
              stopArmed
                ? 'Stop generating (Esc)'
                : activeCompacting
                  ? compactStatusText(activeCompactMode)
                  : 'Send message (Enter)'
            }
            onPointerDown={
              stopArmed
                ? (e) => {
                    if (e.button !== 0) return;
                    e.preventDefault();
                    abortActiveRun();
                  }
                : undefined
            }
            onClick={
              activeCompacting && !stopArmed
                ? undefined
                : () => {
                    const click = composerActionClickKind({
                      stopArmed,
                      abortGestureClick: abortGestureClickRef.current,
                    });
                    abortGestureClickRef.current = false;
                    if (click === 'ignore') return;
                    if (click === 'abort') {
                      abortActiveRun();
                      return;
                    }
                    submit('prompt');
                  }
            }

          >
            <span aria-hidden="true">{stopArmed ? '■' : '➤'}</span>
            <span className="composer-action__label">{stopArmed ? 'Stop' : 'Send'}</span>
          </button>
        </div>
        </div>
        </div>
      </footer>

      <input
        ref={fileInputRef}
        type="file"
        accept={attachmentAccept()}
        multiple
        hidden
        onChange={(e) => {
          onFilesChosen(e.target.files);
          e.target.value = '';
        }}
      />

        </div>
      </div>

      {/* Remote audio for the realtime call: ontrack wires the WebRTC stream
          into this hidden element (play() is called on the click gesture). */}
      <audio ref={realtimeAudioRef} autoPlay playsInline hidden />

      <ToastList
        toasts={toasts}
        dismiss={(id) => setToasts((prev) => prev.filter((t) => t.id !== id))}
      />
    </>
  );
}
