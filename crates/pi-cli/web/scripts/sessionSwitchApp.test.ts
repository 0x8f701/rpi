#!/usr/bin/env node
// Focused App-owned session-switch effect harness. Production remains untouched:
// this drives the exported reducer seam with fake send/subscribe/activate/close
// effects and locks the orchestration contracts that live in App.tsx.
import {
  acceptAuthoritativeCut,
  acceptSwitchHandoff,
  beginSessionSwitch,
  planFastSessionSwitch,
  failSessionSwitch,
  parseSubscriptionCut,
  parseSubscriptionError,
  parseSwitchHandoff,
  planSubscriptionError,
  identityFromWire,
  SwitchProtocolError,
  runtimeKey,
  MAX_LOADED_SESSIONS,
  AUTHORITATIVE_CUT_TIMEOUT_MS,
  SNAPSHOT_IN_PROGRESS_CODE,
  SNAPSHOT_IN_PROGRESS_RETRY_MS,
  shouldRetrySnapshotInProgress,
  switchFailureShouldToast,
  warmContinuityNeedsHistoryConfirm,
  planWarmHistoryConfirm,
  warmHistoryConfirmIsCurrent,
  preserveLiveBindCursor,
  acceptedCutLiveCursor,
  lifecycleReadyGate,
  planAuthoritativeCutTick,
  authoritativeCutTimerDelayMs,
  type RuntimeKey,
  sameRuntime,
  unloadRuntime,
  emptyViewSlot,
  upsertViewSlot,
  type ReadyRuntime,
  type RuntimeIdentity,
  type SessionDisplayState,
  type SessionSwitchTarget,
  type SessionViewSlot,
  type SwitchAction,
  type WarmHistoryConfirmHold,
} from '../src/sessionSwitch.ts';
import {
  planReconnectRoster,
  planReconnectDisplayedCatchup,
  reconnectSubscribeCursor,
  reconnectSavedSwitchIsRedundant,
  applyReconnectRefillCatch,
  planReconnectRefillFailure,
  reconnectRefillFinallyApplies,
  shouldPersistPrimaryPreference,
  bootstrapSavedSwitchNeedsRetry,
  planDisplayedIncarnationRebind,
  commandRejectedBySwitchPending,
  planSessionSnapshotApply,
  stampSessionStateIncarnation,
  reconnectRefillStillOwned,
  planReconnectRefillStep,
  type ReconnectCatalogRow,
  type ReconnectRosterPlan,
  type ReconnectRefillHold,
} from '../src/protocol.ts';
import { wireModelKey } from '../src/modelSelect.ts';
import type { SessionPreferenceRow } from '../src/sessionPreference.ts';
import {
  admitBuiltAttachments,
  aggregateWire,
  classifyAttachments,
  formatSkipSummary,
  mergeIntakeResults,
  placeholderFor,
  readAttachmentsInOrder,
  settleVideoOutcome,
  type AttachSkip,
  type ComposerAttachment,
  type ReadResult,
  type VideoUploadOutcome,
} from '../src/attachments.ts';
import { nextId, messagesToItems, mergeAuthoritativeItems, planReconnectTurn, planStreamStallReconcile, planSettledStreamRefillRequest, planLiveSequenceGap, planLiveGapCatchUp, planStreamCatchUpToast, reduceStreamStall, planStreamStallChrome, type Item, type ReconnectTurnPlan } from '../src/transcript.ts';
import { activityStripView, applyActivityStripEvent, emptyActivityStripState } from '../src/activityStrip.ts';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { BashCard, FinalAssistant, StreamingAssistant, ToolCard, TranscriptSurface } from '../src/App.tsx';
import { isPinned } from '../src/scrollPin.ts';
import type { PendingCommandError } from '../src/pending.ts';
import {
  TRANSCRIPT_FOLD_ATTRIBUTE,
  TRANSCRIPT_ITEM_ATTRIBUTE,
  TRANSCRIPT_PROJECTION_CHUNK,
  TRANSCRIPT_PROJECTION_EDGE_PX,
  applyOpenFoldKeys,
  applyRestoredViewport,
  applySurfaceVisibility,
  compensatePrependedStage,
  noteLiveViewportFold,
  normalizeTranscriptWindow,
  noteLiveViewportScroll,
  planParkedWindow,
  projectionStartForAnchor,
  projectRetainedSlices,
  projectTranscriptWindow,
  shiftProjectionWindowNewer,
  shiftProjectionWindowOlder,
  liveTextSelectionHoldsProjection,
  planDemandProjectionDecision,
  projectionStartForRender,
  resolveHeldRenderPlan,
  commitRenderPlanIfUnheld,
  tailProjectionStart,
  readTranscriptViewport,
  resetTranscriptStamps,
  seedLiveViewport,
  stampedTranscriptKeys,
  transcriptFoldStamp,
  transcriptItemStamp,
  type LiveTranscriptViewport,
  type TranscriptContentRoot,
  type TranscriptAnchorRect,
  type TranscriptProbeRect,
  type ViewportState,
} from '../src/transcriptView.ts';
import { parseLifecycleResult, type SessionRowWire } from '../src/panels/SessionPanel.tsx';
import {
  applyComposerAbortTap,
  composerAbortChrome,
  composerActionClickKind,
  dropSessionAborting,
  planComposerAbort,
  abortIdleSnapshotToasts,
  displayedSessionMatches,
  shouldHoldOptimisticStop,
} from '../src/composerAbort.ts';
import {
  SESSION_PROCESSING_CODE,
  composerCommandSessionId,
  dispatchComposerSubmit,
  planComposerSubmitKind,
  planPromptBusySteerRetry,
  rpcFailureToastSuppressed,
} from '../src/composerSubmit.ts';
import { routeCommandSession } from '../src/goal.ts';

// Fake transcript geometry: every child is exactly one item tall and the
// viewport shows three items — enough for real anchor resolution (multiple
// items above AND below the viewport top) with deterministic numbers.
const TRANSCRIPT_ITEM_HEIGHT = 100;
const TRANSCRIPT_VIEWPORT_HEIGHT = 300;
// Scrollbar-jump regression: a transcript long enough that any sibling walk
// or children scan would be measurable, so the constant-operation bound is
// discriminating rather than trivially small.
const JUMP_ITEM_COUNT = 64;

/** Fake scroll surface for one session's transcript: scrollTop lives in the
 *  harness map (clamped to a real container's range), geometry mirrors the
 *  real `#transcript` container shape, and scrollHeight includes the extra
 *  height of every OPEN fold — a re-opened fold really does grow the
 *  content, which is the premise the follow-restore pin order asserts. */
const FOLD_OPEN_HEIGHT_PX = 30;
class FakeTranscriptScroll {
  constructor(
    private readonly sessionId: string,
    private readonly heightOf: (sessionId: string) => number,
    private readonly scrolls: Map<string, number>,
  ) {}
  get scrollTop(): number { return this.scrolls.get(this.sessionId) ?? 0; }
  set scrollTop(value: number) {
    this.scrolls.set(this.sessionId, Math.max(0, Math.min(value, Math.max(0, this.heightOf(this.sessionId) - TRANSCRIPT_VIEWPORT_HEIGHT))));
  }
  get scrollHeight(): number { return this.heightOf(this.sessionId); }
  readonly clientHeight = TRANSCRIPT_VIEWPORT_HEIGHT;
  getBoundingClientRect(): { top: number; bottom: number; left: number; width: number } {
    return { top: 0, bottom: TRANSCRIPT_VIEWPORT_HEIGHT, left: 0, width: 800 };
  }
}

/** One stamped transcript content child: `data-item-id` (resolved through
 *  the production stamp API), scrollTop-dependent geometry, a bounded
 *  horizontal span (full content width by default; the narrow right-aligned
 *  user-bubble case overrides it), and its stamped `<details>` folds — the
 *  exact element surface the production anchor probe, fold capture, scan,
 *  and restore read. */
class FakeItem {
  readonly folds: FakeFold[];
  readonly parentElement: FakeItem | null = null;
  readonly stamp: { 'data-item-id': string };
  readonly span: { left: number; right: number };
  constructor(
    readonly id: string,
    readonly top: number,
    private readonly owner: FakeTranscript,
    span: { left: number; right: number } = { left: 40, right: 760 },
  ) {
    this.span = span;
    this.folds = [new FakeFold(this.id)];
    this.stamp = transcriptItemStamp(this.id);
  }
  getAttribute(name: string): string | null {
    if (name === TRANSCRIPT_ITEM_ATTRIBUTE) return this.id;
    return null;
  }
  getBoundingClientRect(): { top: number; bottom: number; left: number; right: number } {
    this.owner.harness.geometryReads += 1;
    const top = this.top - (this.owner.harness.scrollTops.get(this.owner.sessionId) ?? 0);
    return { top, bottom: top + TRANSCRIPT_ITEM_HEIGHT, left: this.span.left, right: this.span.right };
  }
}

class FakeFold {
  open = false;
  private readonly key: string;
  constructor(childId: string) {
    this.key = transcriptFoldStamp(childId, 0, 'tool-card__raw')[TRANSCRIPT_FOLD_ATTRIBUTE];
  }
  getAttribute(name: string): string | null {
    return name === TRANSCRIPT_FOLD_ATTRIBUTE ? this.key : null;
  }
}


/** Horizontal span of the narrow right-aligned user bubble (100px at the
 *  content box's right edge [40, 760]): the production probe columns
 *  (44 / 400 / 756) reach it only through the right one. */
const NARROW_BUBBLE_SPAN = { left: 660, right: 760 };
/** One session's fake transcript DOM: stamped children, stamped folds, one
 *  x-AWARE engine hit-test surface (a point only hits children whose box
 *  contains it — right-aligned narrow bubbles are invisible to the center
 *  column, exactly as in the real layout). Drives the REAL transcriptView
 *  functions — the same production surface App.tsx wires to its scroll
 *  handler, fold capture listener, saves, and restores. */
class FakeTranscript {
  readonly harness: AppSwitchHarness;
  readonly items: FakeItem[];
  constructor(
    readonly sessionId: string,
    itemIds: string[],
    harness: AppSwitchHarness,
    narrowRightItemIds: ReadonlySet<string> = new Set<string>(),
  ) {
    this.harness = harness;
    this.items = itemIds.map((id, index) => new FakeItem(
      id,
      index * TRANSCRIPT_ITEM_HEIGHT,
      this,
      narrowRightItemIds.has(id) ? { left: NARROW_BUBBLE_SPAN.left, right: NARROW_BUBBLE_SPAN.right } : { left: 40, right: 760 },
    ));
  }
  get scroll(): FakeTranscriptScroll {
    return new FakeTranscriptScroll(this.sessionId, () => this.height(), this.harness.scrollTops);
  }
  height(): number {
    let height = this.items.length * TRANSCRIPT_ITEM_HEIGHT;
    for (const child of this.items) {
      for (const fold of child.folds) if (fold.open) height += FOLD_OPEN_HEIGHT_PX;
    }
    return height;
  }
  /** The transcript CONTENT wrapper rect (mirrors desktop styles.css:
   *  `.transcript-content` is a centered 800px column with 20px edge
   *  padding — content box x ∈ [40, 760] — while its rect is what the
   *  production probe grid reads). */
  contentRect(): TranscriptProbeRect {
    return { top: 0, left: 20, width: 760 };
  }
  hitSurface(): TranscriptHitSurface {
    return {
      elementsFromPoint: (x: number, y: number) => {
        this.harness.hitTests += 1;
        const scroll = this.harness.scrollTops.get(this.sessionId) ?? 0;
        for (const child of this.items) {
          const top = child.top - scroll;
          if (top <= y && y < top + TRANSCRIPT_ITEM_HEIGHT && child.span.left <= x && x < child.span.right) return [child];
        }
        return [];
      },
    };
  }
  contentRoot(): TranscriptContentRoot {
    return {
      children: this.items,
      stampedFolds: () => this.items.flatMap((child) => child.folds),
    };
  }
}
const failures: string[] = [];
let ran = 0;
function check(name: string, condition: boolean, detail?: string): void {
  ran += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}
interface Deferred<T> {
  promise: Promise<T>;
  resolve: (value: T) => void;
  reject: (error: Error) => void;
}


function deferred<T>(): Deferred<T> {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((ok, fail) => {
    resolve = ok;
    reject = fail;
  });
  return { promise, resolve, reject };
}

const A: RuntimeIdentity = { sessionId: 'source', runtimeIncarnation: 'source-inc' };
const B: RuntimeIdentity = { sessionId: 'target', runtimeIncarnation: 'target-inc' };
const C: RuntimeIdentity = { sessionId: 'replacement', runtimeIncarnation: 'replacement-inc' };
const targetB: SessionSwitchTarget = {
  path: 'target.jsonl',
  catalogSource: 'rpi',
  catalogSessionId: B.sessionId,
  catalogStatus: 'native',
  catalogRuntimeIncarnation: B.runtimeIncarnation,
};
const targetC: SessionSwitchTarget = {
  path: 'replacement.jsonl',
  catalogSource: 'rpi',
  catalogSessionId: C.sessionId,
  catalogStatus: 'native',
  catalogRuntimeIncarnation: C.runtimeIncarnation,
};
const targetForeign: SessionSwitchTarget = {
  path: 'foreign.jsonl',
  catalogSource: 'codex',
  catalogSessionId: 'codex-catalog-1',
  catalogStatus: 'foreign',
  catalogRuntimeIncarnation: null,
};
const foreignRuntime: RuntimeIdentity = { sessionId: 'native-runtime-9', runtimeIncarnation: 'runtime-inc-9' };

function handoff(identity = B, cursor = 4): unknown {
  return {
    sessionId: identity.sessionId,
    runtimeIncarnation: identity.runtimeIncarnation,
    catalogSource: 'rpi',
    catalogSessionId: identity.sessionId,
    catalogStatus: 'native',
    subscriptionRequired: true,
    lifecycleResponseVersion: 1,
    cursor,
    session: switchSessionRow(identity),
  };
}
function switchSessionRow(identity: RuntimeIdentity): SessionRowWire {
  return {
    source: 'rpi',
    sessionId: identity.sessionId,
    runtimeIncarnation: identity.runtimeIncarnation,
    cwd: `<workspace>/${identity.sessionId}`,
    path: `<workspace>/sessions/${identity.sessionId}.jsonl`,
    status: 'native',
    loaded: true,
    messageCount: 1,
  };
}


function cut(identity = B, watermark = 4): unknown {
  return {
    type: 'subscription_snapshot',
    sessionId: identity.sessionId,
    runtimeIncarnation: identity.runtimeIncarnation,
    watermark,
    reason: 'cold-full',
    state: { sessionId: identity.sessionId },
    messages: [],
    events: [],
  };
}

function lifecycleData(identity: RuntimeIdentity, extra: Record<string, unknown> = {}): Record<string, unknown> {
  const sessionFile = `/store/${identity.sessionId}.jsonl`;
  const cwd = `/workspace/${identity.sessionId}`;
  return {
    sessionId: identity.sessionId,
    runtimeIncarnation: identity.runtimeIncarnation,
    state: { sessionId: identity.sessionId, cwd, sessionFile },
    messages: [],
    session: {
      source: 'rpi',
      sessionId: identity.sessionId,
      runtimeIncarnation: identity.runtimeIncarnation,
      cwd,
      path: sessionFile,
      status: 'native',
      loaded: true,
    },
    ...extra,
  };
}

class AppSwitchHarness {
  display: SessionDisplayState | null = {
    displayed: A,
    commandTarget: A,
    pending: null,
    nextGeneration: 1,
  };
  active: RuntimeIdentity | null = A;
  lifecycleRow: SessionRowWire | null = null;
  lifecycleRowClears = 0;
  readonly switchToasts: string[] = [];
  preference = A.sessionId;
  switchPending = false;
  lifecyclePayloadWrites = 0;
  readonly items: Record<string, string> = {
    [A.sessionId]: 'source transcript text',
  };

  readonly viewSlots = new Map<string, string>([
    [runtimeKey(A), 'source-view'],
    [runtimeKey(B), 'target-view'],
  ]);
  // Mirrors App.residentBySessionIdRef: the exact resident overlay per
  // session id, populated by install-cursor and cleared by invalidation.
  resident: Record<string, string> = {};
  readonly modelKey: Record<string, string> = {};
  readonly thinkingLevel: Record<string, string> = {};
  // Mirrors App.socketGenRef: bumped by connect()/onclose; ready entries are
  // stamped with the generation that produced their authoritative cut.
  socketGen = 1;
  fastSwitches = 0;
  // Mirrors App.saveDisplayedView's O(transcript) DOM scan: counted only on
  // NEGOTIATED switches (the fast path reuses the cached slot incrementally).
  transcriptScans = 0;
  readonly unreadClears: string[] = [];
  // Mirrors App.cursorByRuntimeKeyRef: the LIVE event cursor per exact
  // runtime, advanced by ordinary events past the ready stamp.
  readonly cursors = new Map<string, number>();
  readonly ready = new Map<string, ReadyRuntime>();
  readonly sends: SessionSwitchTarget[] = [];
  readonly subscriptions: Array<{ identity: RuntimeIdentity; cursor: number }> = [];
  readonly messageRefills: string[] = [];
  readonly appliedEvents: Array<{ sessionId: string; type: string; itemId?: string; delta?: string }> = [];
  readonly activeAssistant: Record<string, string | undefined> = {};
  readonly streaming: Record<string, boolean> = {};
  readonly aborting: Record<string, boolean> = {};
  readonly transcriptItems: Record<string, Item[]> = {};
  connState: 'off' | 'connecting' | 'reconnecting' | 'on' = 'off';
  refillInFlight = false;
  refillPending = false;
  refillHold: ReconnectRefillHold | null = null;
  refillSlot = 0;
  refillStartedSlot = 0;
  readonly refillMutations: string[] = [];
  readonly refillFollowOns: string[] = [];
  readonly refillCatchEffects: string[] = [];
  now = 0;
  lastCutProgressAt = 0;
  readonly historyConfirms: string[] = [];
  durableItemCount: Record<string, number> = {};
  readyMarked = 0;
  bootstrapRetries = 0;
  private pendingWarmBind: WarmHistoryConfirmHold | null = null;
  readonly activations: Array<{ identity: RuntimeIdentity; restoreView: boolean }> = [];
  readonly invalidations: RuntimeIdentity[] = [];
  readonly order: string[] = [];
  readonly removedIds: string[] = [];
  readonly pendingUploadIds = new Set<string>();
  readonly pendingVideoOwners = new Map<string, {
    owner: RuntimeIdentity;
    generation: number;
    attachment: ComposerAttachment;
  }>();
  readonly viewSlotsByRuntimeKey = new Map<string, {
    attachments: ComposerAttachment[];
    intakeGeneration: number;
    pendingUploadIds: Set<string>;
  }>();
  intakeGeneration = 1;
  intakeCommitQueue: Promise<void> = Promise.resolve();
  attachments: ComposerAttachment[] = [];
  activated: RuntimeIdentity = A;
  budgetQueued: { count: number; wire: number } = { count: 0, wire: 0 };
  // Mirrors App.catalogRefreshRevision: bumped exactly once per accepted
  // lifecycle result, never on negotiated-switch or rejected paths.
  catalogRefreshRevision = 0;
  composerDraft = '';
  // Mirrors App's displayed-view projection (the restored/active view id the
  // transcript DOM would render); written by the restore and read by the
  // slot/save assertions below.
  displayedView = '';
  readonly catalogRefreshRequests: Array<Promise<unknown>> = [];

  private readonly responses: Array<Deferred<unknown>> = [];
  readonly resolverCalls = new Map<number, boolean[]>();
  readonly preferenceWrites: string[] = [];

  private readonly resolvers = new Map<number, (ok: boolean) => void>();
  private readonly promises = new Map<number, Promise<boolean>>();
  createdTransactions = 0;
  private cutTimeoutId: number | null = null;
  private snapshotInProgressRetryTimerId: number | null = null;
  private snapshotInProgressRetryGeneration: number | null = null;
  private nextTimeoutId = 1;
  private readonly timeouts = new Map<number, { delay: number; fn: () => void }>();

  queueResponse(): Deferred<unknown> {
    const response = deferred<unknown>();
    this.responses.push(response);
    return response;
  }

  private send(target: SessionSwitchTarget): Promise<unknown> {
    this.sends.push(target);
    const response = this.responses.shift();
    if (!response) throw new Error('fake send response not queued');
    return response.promise;
  }

  private activate(identity: RuntimeIdentity, restoreView: boolean): void {
    this.active = identity;
    this.activations.push({ identity, restoreView });
    this.unreadClears.push(identity.sessionId);
    if (restoreView) this.restoreDisplayedViewMirror(identity);
  }

  invalidateRuntime(identity: RuntimeIdentity): void {
    this.invalidate(identity);
  }

  private invalidate(identity: RuntimeIdentity): void {
    this.invalidations.push(identity);
    const ownsSessionState = this.resident[identity.sessionId] === identity.runtimeIncarnation;
    if (ownsSessionState) {
      delete this.resident[identity.sessionId];
    }
    this.ready.delete(identity.sessionId);
    this.viewSlots.delete(runtimeKey(identity));
  }

  private settle(generation: number, ok: boolean): void {
    this.clearTimeout(this.cutTimeoutId);
    this.cutTimeoutId = null;
    this.clearTimeout(this.snapshotInProgressRetryTimerId);
    this.snapshotInProgressRetryTimerId = null;
    const calls = this.resolverCalls.get(generation) ?? [];
    calls.push(ok);
    this.resolverCalls.set(generation, calls);
    this.resolvers.get(generation)?.(ok);
    this.resolvers.delete(generation);
    this.promises.delete(generation);
  }

  private rollback(generation: number): boolean {
    const pending = this.display?.pending;
    if (!pending || pending.generation !== generation) return false;
    const source = pending.source;
    this.display = failSessionSwitch(this.display!, generation);
    this.order.push('pending-cleared');
    if (!sameRuntime(this.active, source)) {
      this.activate(source, true);
      this.preference = source.sessionId;
      this.preferenceWrites.push(source.sessionId);
    }
    this.settle(generation, false);
    this.switchPending = false;
    return true;
  }

  private apply(actions: readonly SwitchAction[]): void {
    for (const action of actions) {
      switch (action.type) {
        case 'invalidate-runtime':
          this.invalidate(action.identity);
          break;
        case 'install-cursor':
          this.resident[action.identity.sessionId] = action.identity.runtimeIncarnation;
          this.cursors.set(runtimeKey(action.identity), action.cursor);
          this.subscriptions.push({ identity: action.identity, cursor: action.cursor });
          break;
        case 'resubscribe-zero':
          this.cursors.set(runtimeKey(action.identity), 0);
          this.subscriptions.push({ identity: action.identity, cursor: 0 });
          break;
        case 'mark-ready':
          this.cursors.set(runtimeKey(action.identity), action.cursor);
          this.ready.set(action.identity.sessionId, { identity: action.identity, cursor: action.cursor, socketGeneration: this.socketGen });
          break;
        case 'display-warm':
          this.activate(action.identity, true);
          break;
        case 'apply-state': {
          const stamped = stampSessionStateIncarnation(
            action.state,
            action.identity.runtimeIncarnation,
          ) as {
            runtimeIncarnation?: unknown;
            model?: { provider?: string; id?: string };
            thinkingLevel?: unknown;
          };
          const snapshotPlan = planSessionSnapshotApply({
            shouldActivate: false,
            wireIncarnation: stamped.runtimeIncarnation,
            residentIncarnation: this.resident[action.identity.sessionId],
            displayedSessionId: this.display?.displayed.sessionId,
            targetSessionId: action.identity.sessionId,
          });
          if (!snapshotPlan.apply) break;
          const key = wireModelKey(stamped.model);
          if (key) this.modelKey[action.identity.sessionId] = key;
          if (typeof stamped.thinkingLevel === 'string' && stamped.thinkingLevel !== '') {
            this.thinkingLevel[action.identity.sessionId] = stamped.thinkingLevel;
          }
          break;
        }
        case 'replay-events': {
          for (const event of action.events) {
            this.appliedEvents.push({ sessionId: action.identity.sessionId, type: event.type });
          }
          if (action.events.length > 0) {
            this.cursors.set(runtimeKey(action.identity), acceptedCutLiveCursor(action.events, 0));
          }
          break;
        }
        case 'bind-authority':
          this.cursors.set(runtimeKey(action.identity), action.cursor);
          this.ready.set(action.identity.sessionId, {
            identity: action.identity,
            cursor: action.cursor,
            socketGeneration: this.socketGen,
          });
          this.viewSlots.set(runtimeKey(this.active), this.displayedView);
          this.activate(action.identity, true);
          break;
        case 'save-preference':
          this.preference = action.sessionId;
          break;
        default:
          break;
      }
    }
  }

  // Mirrors App.requestSessionSwitch: callers receive its async result while
  // same-target calls join the one stored transaction and resolver. User and
  // bootstrap saved-target failures toast; already-owned transport toasts
  // are not duplicated.
  request(target: SessionSwitchTarget, origin: 'user' | 'bootstrap' = 'user'): Promise<boolean> {
    const current = this.display;
    if (!current) return Promise.resolve(false);
    // Mirrors App.requestSessionSwitch's exact-ready fast path: the switch
    // completes synchronously with zero switch_session/subscribe frames and
    // settles any superseded pending generation false. Bootstrap origin and
    // any gate miss (including a stale socket-generation stamp) are
    // fail-closed to the negotiated begin/RPC flow below.
    const ready = this.ready.get(target.catalogSessionId);
    const fastPlan = origin === 'user' || origin === 'bootstrap'
      ? planFastSessionSwitch(current, target, {
        ready,
        residentIncarnation: this.resident[target.catalogSessionId] ?? null,
        viewRestorable: ready !== undefined && this.viewSlots.has(runtimeKey(ready.identity)),
        socketGeneration: this.socketGen,
      })
      : { kind: 'negotiated' as const };
    // Mirrors App.requestSessionSwitch: fast eligibility is decided BEFORE
    // any save; the fast save reads the incrementally maintained live
    // viewport O(1) (zero DOM geometry reads) while a negotiated switch pays
    // the full scan.
    if (this.active) this.saveDisplayedViewMirror(this.active, fastPlan.kind !== 'fast');
    if (fastPlan.kind !== 'fast') this.transcriptScans += 1;
    if (fastPlan.kind === 'fast') {
      this.display = fastPlan.state;
      if (fastPlan.supersededGeneration !== null) this.settle(fastPlan.supersededGeneration, false);
      this.switchPending = false;
      if (!sameRuntime(this.active, fastPlan.state.displayed)) this.activate(fastPlan.state.displayed, true);
      this.apply(fastPlan.actions);
      this.fastSwitches += 1;
      return Promise.resolve(true);
    }
    const begun = beginSessionSwitch(current, target);
    if (begun.coalesced) return this.promises.get(begun.generation) ?? Promise.resolve(false);
    if (begun.replacedGeneration !== null) this.settle(begun.replacedGeneration, false);
    this.display = begun.state;
    if (!sameRuntime(this.active, begun.state.displayed)) this.activate(begun.state.displayed, true);
    this.switchPending = true;
    const settled = new Promise<boolean>((resolve) => this.resolvers.set(begun.generation, resolve));
    this.createdTransactions += 1;
    this.promises.set(begun.generation, settled);
    void this.send(target).then((wire) => {
      const display = this.display;
      if (!display) {
        this.settle(begun.generation, false);
        this.switchPending = false;
        return;
      }
      const parsed = parseSwitchHandoff(wire, target);
      const plan = acceptSwitchHandoff(display, begun.generation, parsed, this.ready.get(parsed.sessionId));
      if (plan.stale) return;
      this.display = plan.state;
      this.lifecycleRow = parsed.session;
      this.apply(plan.actions);
      if (plan.actions.some((action) => action.type === 'fail-pending')) {
        this.settle(begun.generation, false);
        this.switchPending = false;
        return;
      }
      if (this.display?.pending?.phase === 'awaiting-authoritative-cut') {
        const cutGeneration = begun.generation;
        const cutOrigin = origin;
        const cutSocketGeneration = this.socketGen;
        const cutIdentity = this.display.pending.targetIdentity ?? {
          sessionId: parsed.sessionId,
          runtimeIncarnation: parsed.runtimeIncarnation,
        };
        this.lastCutProgressAt = this.now;
        const armCutTimer = () => {
          this.clearTimeout(this.cutTimeoutId);
          const delay = authoritativeCutTimerDelayMs({
            now: this.now,
            lastProgressAt: this.lastCutProgressAt,
            timeoutMs: AUTHORITATIVE_CUT_TIMEOUT_MS,
            refillInFlight: this.refillInFlight,
          });
          this.cutTimeoutId = this.setTimeout(() => {
            this.cutTimeoutId = null;
            if (this.socketGen !== cutSocketGeneration) return;
            const pendingNow = this.display?.pending;
            if (!pendingNow
              || pendingNow.generation !== cutGeneration
              || pendingNow.phase !== 'awaiting-authoritative-cut'
              || !pendingNow.targetIdentity
              || !sameRuntime(pendingNow.targetIdentity, cutIdentity)) {
              return;
            }
            const tick = planAuthoritativeCutTick({
              now: this.now,
              lastProgressAt: this.lastCutProgressAt,
              timeoutMs: AUTHORITATIVE_CUT_TIMEOUT_MS,
              refillInFlight: this.refillInFlight,
            });
            if (tick === 'hold') {
              armCutTimer();
              return;
            }
            const identity = pendingNow.targetIdentity;
            if (identity && !sameRuntime(this.ready.get(identity.sessionId)?.identity, identity)) this.invalidate(identity);
            const rolledBack = this.rollback(cutGeneration);
            if (rolledBack && switchFailureShouldToast(cutOrigin)) {
              this.switchToasts.push('switch timed out waiting for subscription snapshot');
            }
          }, delay);
        };
        armCutTimer();
      }
    }).catch((error) => {
      const rolledBack = this.rollback(begun.generation);
      const alreadyOwned = (error as { toastOwned?: boolean } | null)?.toastOwned === true;
      if (rolledBack && switchFailureShouldToast(origin) && !alreadyOwned) {
        // Mirrors App's switch failure toast, preserving the parsed RPC code.
        const code = (error as PendingCommandError | null)?.rpcCode;
        const detail = error instanceof Error ? error.message : String(error);
        this.switchToasts.push(`switch to ${target.catalogSessionId} failed${code ? ` (${code})` : ''}: ${detail}`);
      }
    });
    return settled;
  }
  // Mirrors App.onLifecycleResult: the frozen contract (data.sessionId,
  // state.sessionId, session.sessionId, incarnation, cwd, path/sessionFile,
  // source/status/loaded) is validated through the REAL parseLifecycleResult
  // BEFORE any mutation — a mismatch rejects and mutates nothing. A valid
  // result for the pending switch's source authority keeps the transaction;
  // a result for a truly different authority settles the exact pending
  // generation false. A valid identity applies the composer draft, injects
  // the authoritative `session` row as the sidebar overlay, schedules
  // exactly one background catalog refresh (fire-and-forget — activation
  // never awaits it), and bumps the catalog revision exactly once.
  async lifecycleResult(result: unknown): Promise<void> {
    const display = this.display;
    const pending = display?.pending ?? null;
    const target = parseLifecycleResult(result);
    const identity: RuntimeIdentity = { sessionId: target.sessionId, runtimeIncarnation: target.runtimeIncarnation };
    // Mirrors App.onLifecycleResult: the SOURCE view (composer draft +
    // attachments) is saved into its slot BEFORE any mutation, the child
    // composer starts EMPTY (a Fork's editorText rides ONLY in the child),
    // and the target view slot is a fresh EMPTY slot.
    if (this.active) this.saveDisplayedViewMirror(this.active, true);
    // Lifecycle extras (editorText) ride beside the parsed contract;
    // named-boundary read mirroring App.onLifecycleResult's own read.
    const payload = (result ?? {}) as { editorText?: unknown };
    const childDraft = typeof payload.editorText === 'string' ? payload.editorText : '';
    this.lifecyclePayloadWrites += 1;
    this.lifecycleRow = target.session;
    this.active = identity;
    this.composerDraft = childDraft;
    this.attachments = [];
    this.intakeGeneration += 1;
    this.viewScrolls.set(runtimeKey(identity), {
      draft: childDraft,
      attachments: [],
      follow: true,
      anchorItemId: null,
      anchorOffsetPx: 0,
      fallbackScrollTop: 0,
      openFoldKeys: new Set<string>(),
    });
    this.catalogRefreshRevision += 1;
    const catalogRefresh = deferred<void>();
    this.catalogRefreshRequests.push(catalogRefresh.promise);
    if (pending && sameRuntime(pending.source, identity)) {
      this.display = { ...display!, displayed: identity, commandTarget: identity, pending };
      return Promise.resolve();
    }
    if (pending) {
      this.settle(pending.generation, false);
      this.switchPending = false;
    }
    this.display = { displayed: identity, commandTarget: identity, pending: null, nextGeneration: display?.nextGeneration ?? 1 };
    // Mirrors App.onLifecycleResult: the exact resident overlay + zero
    // cursor are installed BEFORE the subscribe frame so the authoritative
    // cut for this exact runtime is accepted (the cut handler gates on the
    // resident overlay matching the cut identity).
    this.resident[identity.sessionId] = identity.runtimeIncarnation;
    this.cursors.set(runtimeKey(identity), 0);
    return Promise.resolve();
  }
  clearLifecycleRow(): void {
    this.lifecycleRow = null;
    this.lifecycleRowClears += 1;
  }
  unload(identity: RuntimeIdentity, fallback: RuntimeIdentity | null | undefined = undefined): void {
    const display = this.display;
    if (!display) {
      this.invalidate(identity);
      this.active = sameRuntime(this.active, identity) ? null : this.active;
      return;
    }
    const pendingGeneration = display.pending?.generation ?? null;
    let resolvedFallback = fallback;
    if (resolvedFallback === undefined) {
      resolvedFallback = null;
      if (display.pending && !sameRuntime(display.pending.source, identity)) {
        resolvedFallback = display.pending.source;
      } else {
        for (const ready of this.ready.values()) {
          if (!sameRuntime(ready.identity, identity)) {
            resolvedFallback = ready.identity;
            break;
          }
        }
      }
    }
    const plan = unloadRuntime(display, identity, resolvedFallback);
    this.display = plan.state;
    this.invalidate(identity);
    if (plan.state) {
      if (!sameRuntime(this.active, plan.state.displayed)) {
        this.activate(plan.state.displayed, true);
        this.preference = plan.state.commandTarget.sessionId;
        this.preferenceWrites.push(plan.state.commandTarget.sessionId);
        this.subscriptions.push({
          identity: plan.state.displayed,
          cursor: this.ready.get(plan.state.displayed.sessionId)?.cursor ?? 0,
        });
      }
    } else {
      this.active = null;
      this.displayedView = '';
    }
    if (plan.failedPending && pendingGeneration !== null) {
      this.settle(pendingGeneration, false);
      this.switchPending = false;
    }
  }


  // Mirrors App.onEvent's pump: only a contiguous sequence advances the
  // exact runtime's live cursor.
  backgroundEvent(identity: RuntimeIdentity, sequence: number): void {
    const key = runtimeKey(identity);
    if (this.cursors.get(key) !== sequence - 1) return;
    this.cursors.set(key, sequence);
  }

  pumpLiveFrame(identity: RuntimeIdentity, sequence: number): 'applied' | 'ignored' | 'resubscribe' {
    const key = runtimeKey(identity);
    const cursor = this.cursors.get(key) ?? 0;
    if (sequence <= cursor) return 'ignored';
    if (sequence !== cursor + 1) {
      this.subscriptions.push({ identity, cursor });
      return 'resubscribe';
    }
    this.cursors.set(key, sequence);
    return 'applied';
  }

  acceptCut(wire: unknown): void {
    const pending = this.display?.pending ?? null;
    const parsed = parseSubscriptionCut(wire);
    const identity = { sessionId: parsed.sessionId, runtimeIncarnation: parsed.runtimeIncarnation };
    const generation = pending && sameRuntime(pending.targetIdentity, identity) ? pending.generation : 0;
    if (generation === 0) {
      const ready = this.ready.get(identity.sessionId);
      const liveCursor = this.cursors.get(runtimeKey(identity)) ?? 0;
      if (ready !== undefined
        && ready.socketGeneration === this.socketGen
        && sameRuntime(ready.identity, identity)
        && parsed.watermark <= liveCursor) {
        // Mirrors App: a stale superseded cut for an already exact-ready
        // identity on THIS generation never clobbers the live transcript;
        // the LIVE cursor decides and prior-gen ready never suppresses.
        return;
      }
    }
    if (!this.display) return;
    const source = this.display;
    const plan = acceptAuthoritativeCut(source, generation, parsed);
    const bindAt = plan.actions.findIndex((action) => action.type === 'bind-authority');
    const finish = (committed: SessionDisplayState, settlesPromise: boolean) => {
      this.finishCut(pending, { settlesPromise, state: committed });
    };
    if (!warmContinuityNeedsHistoryConfirm(parsed.reason, parsed.messages, plan.actions) || bindAt < 0) {
      this.display = plan.state;
      this.apply(plan.actions);
      finish(plan.state, plan.settlesPromise);
      return;
    }
    if ((this.durableItemCount[identity.sessionId] ?? 0) > 0) {
      this.display = plan.state;
      this.apply(plan.actions);
      finish(plan.state, plan.settlesPromise);
      return;
    }
    const hold = planWarmHistoryConfirm(source, plan, identity, this.socketGen);
    if (!hold) {
      this.display = plan.state;
      this.apply(plan.actions);
      finish(plan.state, plan.settlesPromise);
      return;
    }
    this.apply(hold.preBindActions);
    this.cursors.set(runtimeKey(identity), acceptedCutLiveCursor(parsed.events, parsed.watermark));
    this.historyConfirms.push(identity.sessionId);
    this.pendingWarmBind = hold;
  }

  finishWarmHistoryConfirm(ok: boolean): void {
    const hold = this.pendingWarmBind;
    this.pendingWarmBind = null;
    if (!hold) return;
    if (!warmHistoryConfirmIsCurrent(hold, this.display, this.socketGen)) return;
    if (ok) {
      const liveCursor = this.cursors.get(runtimeKey(hold.identity)) ?? 0;
      this.display = hold.committed;
      this.apply(preserveLiveBindCursor(hold.commitActions, liveCursor));
      this.finishCut(hold.source.pending, { settlesPromise: hold.settlesPromise, state: hold.committed });
      return;
    }
    this.rollback(hold.generation);
  }

  pendingWarmBindHeld(): boolean {
    return this.pendingWarmBind !== null;
  }

  completeCatalogRestore(options: {
    saved: string;
    primarySessionId: string;
    restoredTarget: SessionPreferenceRow | null;
    switchOk: boolean;
    retryTarget: SessionSwitchTarget | null;
  }): void {
    if (shouldPersistPrimaryPreference(options.saved, options.restoredTarget)) {
      this.preference = options.primarySessionId;
      this.preferenceWrites.push(options.primarySessionId);
    }
    this.readyMarked += 1;
    if (bootstrapSavedSwitchNeedsRetry(options.saved, options.switchOk) && options.retryTarget) {
      this.bootstrapRetries += 1;
      void this.request(options.retryTarget, 'bootstrap');
    }
  }

  private finishCut(
    pending: SessionDisplayState['pending'],
    plan: { settlesPromise: boolean; state: SessionDisplayState },
  ): void {
    if (!pending) return;
    if (plan.settlesPromise) {
      this.settle(pending.generation, true);
      this.switchPending = false;
    } else if (!plan.state.pending) {
      if (!sameRuntime(this.active, plan.state.displayed)) {
        this.activate(plan.state.displayed, true);
        this.preference = plan.state.commandTarget.sessionId;
        this.preferenceWrites.push(plan.state.commandTarget.sessionId);
      }
      this.settle(pending.generation, false);
      this.switchPending = false;
    }
  }

  emitSubscriptionError(wire: unknown): void {
    let parsed;
    try {
      parsed = parseSubscriptionError(wire);
    } catch (error) {
      if (!(error instanceof SwitchProtocolError)) throw error;
      const record = wire && typeof wire === 'object' && !Array.isArray(wire)
        ? wire as Record<string, unknown>
        : {};
      const identity = identityFromWire(record.sessionId, record.runtimeIncarnation);
      const pending = this.display?.pending;
      if (identity && pending?.targetIdentity && sameRuntime(pending.targetIdentity, identity)) {
        if (!sameRuntime(this.ready.get(identity.sessionId)?.identity, identity)) this.invalidate(identity);
        this.rollback(pending.generation);
        this.switchToasts.push(`session subscription failed: ${error.message}`);
      }
      return;
    }
    const identity = { sessionId: parsed.sessionId, runtimeIncarnation: parsed.runtimeIncarnation };
    const plan = this.display ? planSubscriptionError(this.display, identity) : null;
    if (!plan) return;
    if (shouldRetrySnapshotInProgress(
      parsed.code,
      this.snapshotInProgressRetryGeneration === plan.generation,
    )) {
      this.snapshotInProgressRetryGeneration = plan.generation;
      this.clearTimeout(this.snapshotInProgressRetryTimerId);
      this.snapshotInProgressRetryTimerId = this.setTimeout(() => {
        this.snapshotInProgressRetryTimerId = null;
        const pendingNow = this.display?.pending;
        if (!pendingNow
          || pendingNow.generation !== plan.generation
          || !pendingNow.targetIdentity
          || !sameRuntime(pendingNow.targetIdentity, identity)) {
          return;
        }
        this.subscriptions.push({
          identity,
          cursor: this.cursors.get(runtimeKey(identity)) ?? 0,
        });
      }, SNAPSHOT_IN_PROGRESS_RETRY_MS);
      return;
    }
    if (!sameRuntime(this.ready.get(identity.sessionId)?.identity, identity)) this.invalidate(identity);
    this.rollback(plan.generation);
    this.switchToasts.push(`session subscription failed: ${parsed.error}`);
  }

  private setTimeout(fn: () => void, delay: number): number {
    const id = this.nextTimeoutId++;
    this.timeouts.set(id, { delay, fn });
    return id;
  }

  private clearTimeout(id: number | null): void {
    if (id === null) return;
    this.timeouts.delete(id);
  }

  scheduledCutDelay(): number | null {
    if (this.cutTimeoutId === null) return null;
    return this.timeouts.get(this.cutTimeoutId)?.delay ?? null;
  }

  advanceTime(ms: number): void {
    this.now += ms;
    const due: Array<() => void> = [];
    for (const [id, timer] of [...this.timeouts]) {
      const remaining = timer.delay - ms;
      if (remaining <= 0) {
        this.timeouts.delete(id);
        if (this.cutTimeoutId === id) this.cutTimeoutId = null;
        if (this.snapshotInProgressRetryTimerId === id) this.snapshotInProgressRetryTimerId = null;
        due.push(timer.fn);
      } else {
        timer.delay = remaining;
      }
    }
    for (const fn of due) fn();
  }

  // Mirrors App.connect/onclose: a new transport generation makes every
  // stale ready stamp ineligible for the fast path until a fresh cut.
  close(): void {
    const pending = this.display?.pending;
    if (pending) {
      const identity = pending.targetIdentity;
      if (identity && !sameRuntime(this.ready.get(identity.sessionId)?.identity, identity)) this.invalidate(identity);
      this.rollback(pending.generation);
    }
    this.socketGen += 1;
    this.order.push('reconnect');
  }

  noteCutProgress(): void {
    this.lastCutProgressAt = this.now;
  }

  reconnectSubscribe(identity: RuntimeIdentity): void {
    this.resident[identity.sessionId] = identity.runtimeIncarnation;
    const cursor = reconnectSubscribeCursor(this.cursors.get(runtimeKey(identity)));
    this.cursors.set(runtimeKey(identity), cursor);
    if (!this.viewSlots.has(runtimeKey(identity))) {
      this.viewSlots.set(runtimeKey(identity), 'reconnect-background-view');
    }
    this.subscriptions.push({ identity, cursor });
  }

  discoverPrimary(primaryIdentity: RuntimeIdentity, coldBind: boolean): void {
    this.resident[primaryIdentity.sessionId] = primaryIdentity.runtimeIncarnation;
    const displayed = this.display;
    const displayedNeedsRebind = displayed !== null
      && displayed.displayed.sessionId === primaryIdentity.sessionId
      && displayed.displayed.runtimeIncarnation !== primaryIdentity.runtimeIncarnation;
    if (coldBind || displayedNeedsRebind) {
      const previousDisplayed = displayed?.displayed ?? null;
      if (displayedNeedsRebind) {
        const pendingTargetsPrevious = displayed.pending !== null
          && sameRuntime(displayed.pending.targetIdentity, previousDisplayed);
        this.display = {
          displayed: primaryIdentity,
          commandTarget: displayed.commandTarget.sessionId === primaryIdentity.sessionId
            ? primaryIdentity
            : displayed.commandTarget,
          pending: pendingTargetsPrevious ? null : displayed.pending,
          nextGeneration: displayed.nextGeneration,
        };
      } else if (this.display === null) {
        this.display = {
          displayed: primaryIdentity,
          commandTarget: primaryIdentity,
          pending: null,
          nextGeneration: 1,
        };
      }
      this.activate(primaryIdentity, true);
      if (previousDisplayed && !sameRuntime(previousDisplayed, primaryIdentity)) {
        this.invalidate(previousDisplayed);
      }
    }
    const catchup = planReconnectDisplayedCatchup(primaryIdentity, this.display?.displayed ?? primaryIdentity);
    for (const identity of catchup.subscribe) {
      this.resident[identity.sessionId] = identity.runtimeIncarnation;
      const cursor = reconnectSubscribeCursor(this.cursors.get(runtimeKey(identity)));
      this.cursors.set(runtimeKey(identity), cursor);
      this.subscriptions.push({ identity, cursor });
    }
    this.messageRefills.push(catchup.refillSessionId);
    const refillIncarnation = this.resident[catchup.refillSessionId];
    this.refillHold = refillIncarnation
      ? {
        sessionId: catchup.refillSessionId,
        runtimeIncarnation: refillIncarnation,
        socketGeneration: this.socketGen,
      }
      : null;
    this.refillSlot += 1;
    this.refillStartedSlot = this.refillSlot;
    this.connState = 'on';
    this.refillPending = true;
    this.refillInFlight = true;
    this.lastCutProgressAt = this.now;
  }

  failReconnectRefill(error: unknown, recovery: { isStreaming: boolean }): void {
    const outcome = this.completeDeferredRefillCatch(this.refillStartedSlot, this.refillHold, error);
    if (outcome !== 'toast-and-recover') return;
    const sessionId = this.messageRefills[this.messageRefills.length - 1];
    if (sessionId) this.streaming[sessionId] = recovery.isStreaming === true;
  }

  completeDeferredRefillCatch(
    startedSlot: number,
    hold: ReconnectRefillHold | null,
    error: unknown,
  ): 'silent' | 'rethrow-stale' | 'toast-and-recover' {
    const staleAbort = { kind: 'stale' };
    const step = planReconnectRefillFailure({
      stillOwned: hold !== null && reconnectRefillStillOwned(
        hold,
        this.resident[hold.sessionId],
        this.socketGen,
      ),
      error,
      staleAbort,
    });
    if (step === 'silent' || step === 'rethrow-stale') {
      this.applyRefillFinally(startedSlot);
      return step;
    }
    this.refillCatchEffects.push('toast');
    applyReconnectRefillCatch(error, staleAbort, (message) => {
      this.switchToasts.push(message);
    });
    this.refillCatchEffects.push('get_state');
    this.refillFollowOns.push('get_state');
    this.applyRefillFinally(startedSlot);
    return 'toast-and-recover';
  }

  applyRefillFinally(startedSlot: number): void {
    if (!reconnectRefillFinallyApplies(startedSlot, this.refillSlot)) return;
    this.refillPending = false;
    this.refillInFlight = false;
    this.lastCutProgressAt = this.now;
  }

  rebindDisplayed(identity: RuntimeIdentity): void {
    const displayed = this.display;
    const rebind = planDisplayedIncarnationRebind(displayed, identity);
    if (!rebind || !displayed) return;
    this.invalidate(displayed.displayed);
    this.display = {
      ...displayed,
      displayed: rebind.displayed,
      commandTarget: rebind.commandTarget,
    };
    this.activate(identity, true);
    this.reconnectSubscribe(identity);
  }

  applyReconnectRefill(options: {
    isStreaming: boolean;
    history: Item[];
    settled?: Item[];
  }): ReconnectTurnPlan {
    const sessionId = this.messageRefills[this.messageRefills.length - 1];
    if (!sessionId) throw new Error('reconnect refill has no displayed session');
    const live = this.transcriptItems[sessionId] ?? [];
    const history = options.history;
    const merged = mergeAuthoritativeItems(history, live, options.isStreaming);
    const turn = planReconnectTurn({
      isStreaming: options.isStreaming,
      liveItems: merged,
      activeAssistantId: this.activeAssistant[sessionId],
    });
    this.streaming[sessionId] = turn.kind === 'streaming';
    this.refillPending = false;
    this.refillInFlight = false;
    this.lastCutProgressAt = this.now;
    if (turn.kind === 'settled') {
      this.transcriptItems[sessionId] = options.settled ?? history;
      this.activeAssistant[sessionId] = undefined;
      return turn;
    }
    this.transcriptItems[sessionId] = merged;
    this.activeAssistant[sessionId] = turn.assistantId ?? undefined;
    return turn;
  }

  continueDeferredRefill(
    kind: 'get_messages' | 'job_list' | 'get_state' | 'recovery',
    wire?: { isStreaming?: boolean; runtimeIncarnation?: unknown },
  ): 'apply' | 'reject' {
    const hold = this.refillHold;
    if (!hold) return 'reject';
    if (kind === 'get_messages' || kind === 'job_list') {
      if (!reconnectRefillStillOwned(hold, this.resident[hold.sessionId], this.socketGen)) {
        return 'reject';
      }
      this.refillFollowOns.push(kind === 'get_messages' ? 'get_state' : 'job_list');
      if (kind === 'job_list') this.refillMutations.push('job_list');
      return 'apply';
    }
    if (planReconnectRefillStep({
      hold,
      residentIncarnation: this.resident[hold.sessionId],
      socketGenerationNow: this.socketGen,
      wireIncarnation: wire?.runtimeIncarnation,
      displayedSessionId: this.display?.displayed.sessionId,
    }) === 'reject') {
      return 'reject';
    }
    if (!this.applyRefillSnapshot(hold.sessionId, wire ?? {})) return 'reject';
    this.markStreamingFor(hold.sessionId, wire?.isStreaming === true);
    return 'apply';
  }

  private applyRefillSnapshot(target: string, data: { isStreaming?: boolean; runtimeIncarnation?: unknown }): boolean {
    const snapshotPlan = planSessionSnapshotApply({
      shouldActivate: false,
      wireIncarnation: data.runtimeIncarnation,
      residentIncarnation: this.resident[target],
      displayedSessionId: this.display?.displayed.sessionId,
      targetSessionId: target,
    });
    if (!snapshotPlan.apply) return false;
    this.refillMutations.push('applyState');
    if (typeof data.isStreaming === 'boolean') {
      this.streaming[target] = data.isStreaming;
      if (data.isStreaming !== true) delete this.aborting[target];
    }
    return true;
  }

  private markStreamingFor(sessionId: string, isStreaming: boolean): void {
    this.refillMutations.push('markStreaming');
    this.streaming[sessionId] = isStreaming;
    if (!isStreaming) delete this.aborting[sessionId];
  }

  deliverRuntimeEvent(
    identity: RuntimeIdentity,
    sequence: number,
    frame: { type: string; itemId?: string; delta?: string },
  ): boolean {
    const subscribed = this.subscriptions.some((entry) => sameRuntime(entry.identity, identity));
    if (!subscribed) return false;
    const key = runtimeKey(identity);
    const cursor = this.cursors.get(key) ?? 0;
    if (sequence !== cursor + 1) return false;
    this.cursors.set(key, sequence);
    const pendingTarget = this.display?.pending?.targetIdentity;
    if (pendingTarget && sameRuntime(pendingTarget, identity)) {
      this.lastCutProgressAt = this.now;
    }
    const sessionId = identity.sessionId;
    if (frame.type === 'message_start' && frame.itemId) {
      this.activeAssistant[sessionId] = frame.itemId;
      const items = this.transcriptItems[sessionId] ?? [];
      this.transcriptItems[sessionId] = [
        ...items,
        { kind: 'assistant', id: frame.itemId, status: 'streaming', blocks: [] },
      ];
      this.streaming[sessionId] = true;
    } else if (frame.type === 'message_update' && frame.itemId && frame.delta) {
      const targetId = frame.itemId;
      const items = this.transcriptItems[sessionId] ?? [];
      const match = items.find((item) => item.kind === 'assistant' && item.id === targetId);
      if (!match) return false;
      this.transcriptItems[sessionId] = items.map((item) => {
        if (item.kind !== 'assistant' || item.id !== targetId) return item;
        const text = item.blocks[0]?.type === 'text' ? item.blocks[0].text : '';
        return { ...item, blocks: [{ type: 'text', text: text + frame.delta }] };
      });
    } else if (frame.type === 'turn_complete') {
      this.streaming[sessionId] = false;
    }
    this.appliedEvents.push({ sessionId, ...frame });
    return true;
  }


  // Mirrors App's retireResident seam: skip primary/displayed; mismatch retires; omission only when complete.
  retireResident(plan: ReconnectRosterPlan, primarySessionId: string): void {
    const displayedSessionId = this.display?.displayed?.sessionId ?? null;
    for (const [sessionId, incarnation] of Object.entries(this.resident)) {
      if (sessionId === primarySessionId) continue;
      if (sessionId === displayedSessionId) continue;
      const authoritativeIncarnation = plan.authoritative.get(sessionId);
      if (authoritativeIncarnation === incarnation) continue;
      if (authoritativeIncarnation === undefined && !plan.omissionSafe) continue;
      this.invalidate({ sessionId, runtimeIncarnation: incarnation });
    }
  }

  // Mirrors App.onResponse close_session invalidation: ready/resident drop
  // BEFORE the session_unloaded frame can arrive.
  closeSession(sessionId: string): void {
    this.ready.delete(sessionId);
    delete this.resident[sessionId];
  }

  // Mirrors App.restoreDisplayedView: a restored slot never resurrects an
  // uploading video placeholder, and the composer generation advances so a
  // pre-switch async read can never mutate the fresh projection.
  restoreComposer(identity: RuntimeIdentity): void {
    const slot = this.viewSlotsByRuntimeKey.get(runtimeKey(identity));
    const restored = (slot?.attachments ?? []).filter(
      (attachment) => attachment.kind !== 'video' || attachment.videoState !== 'uploading',
    );
    this.attachments = restored;
    this.pendingUploadIds.clear();
    this.intakeGeneration += 1;
    this.budgetQueued = { count: restored.length, wire: aggregateWire(restored) };
  }

  // Mirrors App.updateOwnedAttachments: exact-runtime/generation writeback
  // with the authenticated-owner fallback for a terminal old-token settle.
  updateOwnedAttachments(
    owner: RuntimeIdentity | null,
    generation: number,
    update: (attachments: ComposerAttachment[], pendingUploadIds: ReadonlySet<string>) => ComposerAttachment[],
    updatePending?: (pendingUploadIds: Set<string>) => void,
    authenticated?: { attachments: ComposerAttachment[]; pendingUploadIds: ReadonlySet<string> },
  ): boolean {
    if (!owner) return false;
    const ownerPainted = sameRuntime(this.activated, owner);
    const exactProjection = ownerPainted && this.intakeGeneration === generation;
    const key = runtimeKey(owner);
    const saved = this.viewSlotsByRuntimeKey.get(key);
    if (!exactProjection && (!authenticated || !ownerPainted) && (!saved || saved.intakeGeneration !== generation)) return false;

    const useAuthenticatedOwner = ownerPainted && authenticated !== undefined;
    const ownedAttachments = exactProjection
      ? this.attachments
      : useAuthenticatedOwner ? authenticated.attachments : saved!.attachments;
    const ownedPending = exactProjection
      ? this.pendingUploadIds
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
          && (next.kind !== 'video' || next.videoState !== 'uploading');
      });
      const changedById = new Map(changed.map((attachment) => [attachment.id, attachment]));
      projectedAttachments = this.attachments
        .filter((attachment) => !removedIds.has(attachment.id))
        .map((attachment) => changedById.get(attachment.id) ?? attachment);
      const projectedIds = new Set(projectedAttachments.map((attachment) => attachment.id));
      for (const attachment of changed) {
        if (!projectedIds.has(attachment.id)) projectedAttachments.push(attachment);
      }
      projectedPending = new Set(this.pendingUploadIds);
      for (const id of ownedPending) {
        if (!settledPending.has(id)) projectedPending.delete(id);
      }
      projectedGeneration = this.intakeGeneration;
    }

    this.viewSlotsByRuntimeKey.set(key, {
      attachments: projectedAttachments,
      intakeGeneration: projectedGeneration,
      pendingUploadIds: projectedPending,
    });
    if (ownerPainted) {
      this.attachments = projectedAttachments;
      this.pendingUploadIds = projectedPending;
      this.budgetQueued = { count: projectedAttachments.length, wire: aggregateWire(projectedAttachments) };
    }
    return true;
  }

  // Mirrors App.onFilesChosen's settleVideo closure: the upload outcome is
  // authenticated against the immutable owner/generation captured at intake;
  // a removed chip or a composer reset makes the settle silent.
  settleVideo(id: string, name: string, outcome: VideoUploadOutcome): void {
    const authenticated = this.pendingVideoOwners.get(id);
    if (!authenticated
      || !sameRuntime(authenticated.owner, this.activated)
      || authenticated.generation !== this.intakeGeneration) return;
    this.pendingVideoOwners.delete(id);
    let eligible = false;
    let surface = !outcome.ok;
    const applied = this.updateOwnedAttachments(
      authenticated.owner,
      authenticated.generation,
      (prev, pending) => {
        if (!pending.has(id)) return prev;
        eligible = true;
        const settled = settleVideoOutcome(prev, id, outcome);
        surface = settled.surface;
        return settled.next;
      },
      (pending) => pending.delete(id),
      { attachments: [authenticated.attachment], pendingUploadIds: new Set([id]) },
    );
    if (applied && eligible && sameRuntime(this.activated, authenticated.owner) && surface && !outcome.ok) {
      this.switchToasts.push(`video ${name} failed: ${outcome.message}`);
    }
  }

  // Mirrors App.removeAttachment: a removed chip's in-flight upload outcome
  // becomes stale — the settle gate must not resurrect the chip or toast a
  // phantom failure.
  removeAttachment(id: string): void {
    this.pendingUploadIds.delete(id);
    this.pendingVideoOwners.delete(id);
    this.removedIds.push(id);
    this.attachments = this.attachments.filter((attachment) => attachment.id !== id);
    this.budgetQueued = { count: this.attachments.length, wire: aggregateWire(this.attachments) };
  }

  // Mirrors App.onFilesChosen's intake pipeline: classify -> reserve ->
  // read (injected, in order) -> merge -> commit -> upload videos. The
  // commit is serialized behind the previous intake's gate, and the whole
  // intake is owned by the exact runtime + generation captured at selection.
  async intake(
    files: Array<{ type: string; name: string; size: number }>,
    read: (file: { type: string; name: string; size: number }, index: number) => Promise<ReadResult>,
  ): Promise<void> {
    const intakeOwner = this.activated;
    const intakeContext = { gen: this.intakeGeneration, host: 'h:8080', token: 'tok', session: A.sessionId };
    const plan = classifyAttachments(files);
    const tasks = plan.accepted.map((entry) => {
      const id = nextId('a');
      return {
        entry,
        id,
        pendingAttachment: entry.kind === 'video' ? placeholderFor(entry, id) : null,
      };
    });
    for (const task of tasks) {
      if (task.entry.kind !== 'video' || !task.pendingAttachment || !intakeOwner) continue;
      this.pendingUploadIds.add(task.id);
      this.pendingVideoOwners.set(task.id, {
        owner: intakeOwner,
        generation: intakeContext.gen,
        attachment: task.pendingAttachment,
      });
    }
    const previousCommit = this.intakeCommitQueue;
    let commitGateResolve!: () => void;
    this.intakeCommitQueue = previousCommit.then(() => new Promise<void>((resolve) => { commitGateResolve = resolve; }));
    let built: ComposerAttachment[] = [];
    const skips: AttachSkip[] = [];
    try {
      const reads = tasks.filter((task) => task.entry.kind !== 'video');
      const results = await readAttachmentsInOrder(reads.map((task) => task.entry), read);
      const readById = new Map<string, ReadResult>();
      results.forEach((result, index) => readById.set(reads[index]!.id, result as ReadResult));
      const merged = mergeIntakeResults(tasks, readById);
      built = merged.built;
      skips.push(...merged.skips);
    } catch {
      for (const task of tasks) {
        if (task.entry.kind === 'video' && task.pendingAttachment) built.push(task.pendingAttachment);
      }
    }
    await previousCommit;
    const applied = this.updateOwnedAttachments(intakeOwner, intakeContext.gen, (prev) => {
      const admitted = admitBuiltAttachments(prev, built);
      skips.push(...admitted.skipped);
      return admitted.next;
    });
    if (!applied) {
      for (const task of tasks) {
        this.pendingUploadIds.delete(task.id);
        this.pendingVideoOwners.delete(task.id);
      }
      commitGateResolve();
      return;
    }
    commitGateResolve();
    const summary = formatSkipSummary(skips);
    if (summary && sameRuntime(this.activated, intakeOwner)) this.switchToasts.push(summary);
  }

  // ---------------------------------------------------------------------------
  // Real fake-DOM transcript seam (not a counter): each session's transcript
  // is a stamped element tree (data-item-id children, data-fold-key folds,
  // scrollTop-dependent geometry, one engine hit-test surface). EVERY
  // viewport method below calls the REAL transcriptView production functions
  // — noteLiveViewportScroll, noteLiveViewportFold, readTranscriptViewport,
  // applyRestoredViewport, seedLiveViewport — exactly what App.tsx wires,
  // so nothing here reimplements the decision core.
  readonly viewScrolls = new Map<string, {
    draft: string;
    attachments: ComposerAttachment[];
    follow: boolean;
    anchorItemId: string | null;
    anchorOffsetPx: number;
    fallbackScrollTop: number;
    openFoldKeys: Set<string>;
  }>();
  liveViewport: { key: RuntimeKey; viewport: LiveTranscriptViewport } | null = null;
  readonly transcripts = new Map<string, FakeTranscript>();
  readonly scrollTops = new Map<string, number>();
  // DOM operations observed by the fake transcript. The fast save must
  // perform ZERO geometry reads (state comes from the live viewport); a
  // scroll note pays a CONSTANT number of probes + reads even for a
  // first→last scrollbar jump; negotiated scans pay one bounded full scan.
  geometryReads = 0;
  hitTests = 0;

  mountTranscript(sessionId: string, itemIds: string[], narrowRightItemIds: string[] = []): void {
    this.transcripts.set(sessionId, new FakeTranscript(sessionId, itemIds, this, new Set(narrowRightItemIds)));
    this.scrollTops.set(sessionId, 0);
  }
  // Mirrors App.handleTranscriptScroll: the production O(1) anchor note into
  // the live viewport — bounded hit-test probes, never a walk or scan. The
  // probe composes the two surfaces the grid needs, exactly like App: the Y
  // origin from the SCROLL CONTAINER's rect (the viewport top — the content
  // wrapper's top rides the scroll and would probe above the viewport) and
  // the x-span from the content wrapper's rendered column.
  noteScroll(sessionId: string, scrollTop: number): void {
    if (!this.active
      || !sameRuntime(this.active, { sessionId, runtimeIncarnation: this.resident[sessionId] ?? '' })
      || !this.liveViewport || this.liveViewport.key !== runtimeKey(this.active)) return;
    this.scrollTops.set(sessionId, scrollTop);
    const transcript = this.transcripts.get(sessionId) ?? null;
    const contentRect = transcript ? transcript.contentRect() : null;
    const containerRect = transcript ? transcript.scroll.getBoundingClientRect() : null;
    noteLiveViewportScroll(
      this.liveViewport.viewport,
      transcript ? transcript.hitSurface() : null,
      transcript ? transcript.scroll : null,
      contentRect && containerRect
        ? { top: containerRect.top, left: contentRect.left, width: contentRect.width }
        : null,
      transcript ? isPinned(transcript.scroll.scrollTop, transcript.scroll.scrollHeight, transcript.scroll.clientHeight) : false,
    );
  }

  // Mirrors App's fold-toggle capture listener: the fake DOM mutation plus
  // the production O(1) stamped-key note.
  toggleFold(sessionId: string, childIndex: number, foldIndex: number, open: boolean): void {
    const child = this.transcripts.get(sessionId)?.items[childIndex];
    const fold = child?.folds[foldIndex];
    if (!fold) return;
    fold.open = open;
    if (!this.active
      || !sameRuntime(this.active, { sessionId, runtimeIncarnation: this.resident[sessionId] ?? '' })) return;
    noteLiveViewportFold(this.liveViewport?.viewport ?? null, fold);
  }

  // Mirrors App.saveDisplayedView: scanTranscript=true pays the production
  // full-scan anchor/fold derivation; false reads the live viewport O(1)
  // with ZERO DOM reads. Both paths re-seed the live viewport from the slot.
  saveDisplayedViewMirror(identity: RuntimeIdentity, scanTranscript: boolean): void {
    const key = runtimeKey(identity);
    const saved = this.viewScrolls.get(key) ?? null;
    const transcript = this.transcripts.get(identity.sessionId) ?? null;
    const state = readTranscriptViewport({
      live: this.liveViewport?.key === key ? this.liveViewport.viewport : null,
      current: saved
        ? { anchorItemId: saved.anchorItemId, anchorOffsetPx: saved.anchorOffsetPx, openFoldKeys: saved.openFoldKeys }
        : null,
      transcript: transcript?.scroll ?? null,
      content: transcript?.contentRoot() ?? null,
      scan: scanTranscript,
    });
    this.viewScrolls.set(key, {
      draft: this.composerDraft,
      attachments: [...this.attachments],
      follow: state.follow,
      anchorItemId: state.anchorItemId,
      anchorOffsetPx: state.anchorOffsetPx,
      fallbackScrollTop: state.fallbackScrollTop,
      openFoldKeys: state.openFoldKeys,
    });
    this.liveViewport = seedLiveViewport(key, state);
  }

  // Mirrors App.restoreDisplayedView: the production restore re-applies the
  // stamped folds FIRST (a re-opened fold grows scrollHeight) and only then
  // pins a following restore through the pin controller's forcePin write
  // (scrollTop = scrollHeight, clamped to the POST-fold bottom) — so the
  // final position is the new bottom, never the pre-fold one. The live
  // viewport seeds from the restored slot.
  restoreDisplayedViewMirror(identity: RuntimeIdentity): void {
    const saved = this.viewScrolls.get(runtimeKey(identity));
    const sessionId = identity.sessionId;
    const transcript = this.transcripts.get(sessionId) ?? null;
    const restored: ViewportState = {
      follow: saved?.follow ?? true,
      anchorItemId: saved?.anchorItemId ?? null,
      anchorOffsetPx: saved?.anchorOffsetPx ?? 0,
      fallbackScrollTop: saved?.fallbackScrollTop ?? 0,
      openFoldKeys: saved?.openFoldKeys ?? new Set<string>(),
    };
    applyRestoredViewport(transcript?.scroll ?? null, transcript ? transcript.contentRoot() : null, restored);
    if (restored.follow || restored.anchorItemId === null) {
      const scroll = transcript?.scroll ?? null;
      if (scroll) scroll.scrollTop = scroll.scrollHeight;
    }
    // Composer half of the restore: the slot's draft + attachments (minus an
    // uploading video placeholder) are the exact pre-switch state.
    this.composerDraft = saved?.draft ?? '';
    this.attachments = (saved?.attachments ?? []).filter(
      (attachment) => attachment.kind !== 'video' || attachment.videoState !== 'uploading',
    );
    this.displayedView = this.viewSlots.get(runtimeKey(identity)) ?? 'missing-view';
    this.liveViewport = seedLiveViewport(runtimeKey(identity), restored);
  }
}

async function flush(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
}
// Superseding a switch settles its caller before the stale RPC responds.
{
  const app = new AppSwitchHarness();
  const staleResponse = app.queueResponse();
  const currentResponse = app.queueResponse();
  const staleSwitch = app.request(targetB);
  const currentSwitch = app.request(targetC);
  check('superseded switch resolves before its stale RPC response', await staleSwitch === false);
  check('supersession starts the replacement RPC without subscribing',
    app.sends.length === 2 && app.subscriptions.length === 0 && app.display.pending?.generation === 2);
  staleResponse.resolve(handoff(B, 4));
  await flush();
  check('stale handoff cannot affect replacement generation',
    app.subscriptions.length === 0 && app.display.pending?.generation === 2
      && app.resolverCalls.get(2) === undefined);
  currentResponse.resolve(handoff(C, 6));
  await flush();
  check('replacement handoff subscribes only its exact runtime',
    app.subscriptions.length === 1 && sameRuntime(app.subscriptions[0].identity, C));
  app.acceptCut(cut(C, 6));
  check('replacement transaction remains independently successful',
    await currentSwitch === true && app.resolverCalls.get(2)?.join(',') === 'true');
}


// LRU reopen: the catalog hint names an evicted incarnation while the backend
// reopened the runtime fresh. The handoff identity is authoritative; the
// switch proceeds cold and the authoritative cut still settles the caller true.
{
  const app = new AppSwitchHarness();
  const response = app.queueResponse();
  const switched = app.request(targetB);
  const reopened: RuntimeIdentity = { ...B, runtimeIncarnation: 'reopen-inc' };
  response.resolve(handoff(reopened));
  await flush();
  check('stale catalog incarnation after LRU reopen subscribes cold to the handoff runtime',
    app.subscriptions.length === 1 && app.subscriptions[0].identity.sessionId === B.sessionId
      && app.subscriptions[0].identity.runtimeIncarnation === reopened.runtimeIncarnation
      && app.subscriptions[0].cursor === 0 && app.switchPending);
  app.acceptCut(cut(reopened, 4));
  check('LRU reopen switch settles true on its own runtime cut', await switched === true
    && !app.switchPending && sameRuntime(app.display.displayed, reopened));
}

{
  const app = new AppSwitchHarness();
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.resolve(handoff(B, 0));
  await flush();
  app.acceptCut({
    type: 'subscription_snapshot',
    sessionId: B.sessionId,
    runtimeIncarnation: B.runtimeIncarnation,
    watermark: 0,
    reason: 'cold-full',
    state: {
      sessionId: B.sessionId,
      model: { provider: 'cliproxy', id: 'gpt-5.6-sol', name: 'GPT-5.6 Sol' },
      thinkingLevel: 'medium',
    },
    messages: [],
    events: [],
  });
  check('cut RpcSessionState without runtimeIncarnation still writes header model and thinking',
    await switched === true
      && app.modelKey[B.sessionId] === 'cliproxy/gpt-5.6-sol'
      && app.thinkingLevel[B.sessionId] === 'medium');
}

{
  const app = new AppSwitchHarness();
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.resolve(handoff(B, 4));
  await flush();
  const toastsBefore = app.switchToasts.length;
  app.emitSubscriptionError({
    type: 'subscription_error',
    sessionId: C.sessionId,
    runtimeIncarnation: C.runtimeIncarnation,
    error: 'wrong identity',
  });
  check('subscription_error for a non-target identity is ignored',
    app.display?.pending?.phase === 'awaiting-authoritative-cut'
      && app.switchPending === true
      && app.switchToasts.length === toastsBefore
      && app.items[A.sessionId] === 'source transcript text');
  app.emitSubscriptionError({
    type: 'subscription_error',
    sessionId: B.sessionId,
    runtimeIncarnation: B.runtimeIncarnation,
    error: 'owner stopped',
    code: 'runtime_unavailable',
  });
  check('owner_dies_after_handoff_subscription_error_preserves_source',
    await switched === false
      && app.switchPending === false
      && app.display?.pending === null
      && sameRuntime(app.display!.displayed, A)
      && app.items[A.sessionId] === 'source transcript text'
      && !app.invalidations.some((identity) => sameRuntime(identity, A))
      && app.invalidations.length === 1
      && sameRuntime(app.invalidations[0], B)
      && app.switchToasts.length === 1
      && app.switchToasts[0] === 'session subscription failed: owner stopped');
}

{
  const app = new AppSwitchHarness();
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.resolve(handoff(B, 4));
  await flush();
  const subscribed = app.subscriptions.length;
  const toastsBefore = app.switchToasts.length;
  app.emitSubscriptionError({
    type: 'subscription_error',
    sessionId: B.sessionId,
    runtimeIncarnation: B.runtimeIncarnation,
    error: 'subscription snapshot already in progress',
    code: SNAPSHOT_IN_PROGRESS_CODE,
  });
  check('typed snapshot_in_progress keeps the pending switch',
    app.display?.pending?.phase === 'awaiting-authoritative-cut'
      && app.switchPending === true
      && switched !== undefined
      && app.subscriptions.length === subscribed
      && app.switchToasts.length === toastsBefore
      && sameRuntime(app.display!.displayed, A)
      && app.items[A.sessionId] === 'source transcript text');
  app.advanceTime(SNAPSHOT_IN_PROGRESS_RETRY_MS - 1);
  check('snapshot_in_progress retry waits the typed delay',
    app.subscriptions.length === subscribed && app.switchPending === true);
  app.advanceTime(1);
  check('snapshot_in_progress retries subscribe once',
    app.subscriptions.length === subscribed + 1
      && sameRuntime(app.subscriptions[subscribed].identity, B)
      && app.switchPending === true
      && app.display?.pending?.phase === 'awaiting-authoritative-cut');
  app.acceptCut(cut(B, 4));
  check('retried subscribe still settles the switch',
    await switched === true
      && app.switchPending === false
      && sameRuntime(app.display!.displayed, B)
      && app.switchToasts.length === toastsBefore);
}

{
  const app = new AppSwitchHarness();
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.resolve(handoff(B, 4));
  await flush();
  const subscribed = app.subscriptions.length;
  app.emitSubscriptionError({
    type: 'subscription_error',
    sessionId: B.sessionId,
    runtimeIncarnation: B.runtimeIncarnation,
    error: 'subscription snapshot already in progress',
    code: SNAPSHOT_IN_PROGRESS_CODE,
  });
  app.advanceTime(SNAPSHOT_IN_PROGRESS_RETRY_MS);
  check('first snapshot_in_progress consumed the one retry',
    app.subscriptions.length === subscribed + 1 && app.switchPending === true);
  const toastsBefore = app.switchToasts.length;
  app.emitSubscriptionError({
    type: 'subscription_error',
    sessionId: B.sessionId,
    runtimeIncarnation: B.runtimeIncarnation,
    error: 'subscription snapshot already in progress',
    code: SNAPSHOT_IN_PROGRESS_CODE,
  });
  check('second snapshot_in_progress rolls back to source',
    await switched === false
      && app.switchPending === false
      && app.display?.pending === null
      && sameRuntime(app.display!.displayed, A)
      && app.items[A.sessionId] === 'source transcript text'
      && app.switchToasts.length === toastsBefore + 1
      && app.switchToasts[toastsBefore] === 'session subscription failed: subscription snapshot already in progress');
}

{
  const app = new AppSwitchHarness();
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.resolve(handoff(B, 4));
  await flush();
  const subscribed = app.subscriptions.length;
  app.emitSubscriptionError({
    type: 'subscription_error',
    sessionId: B.sessionId,
    runtimeIncarnation: B.runtimeIncarnation,
    error: 'subscription snapshot already in progress',
  });
  check('prose snapshot-in-progress without typed code rolls back',
    await switched === false
      && app.switchPending === false
      && app.subscriptions.length === subscribed
      && sameRuntime(app.display!.displayed, A)
      && app.items[A.sessionId] === 'source transcript text');
}

{
  const app = new AppSwitchHarness();
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.resolve(handoff(B, 4));
  await flush();
  check('awaiting-cut arms the bounded subscription timer',
    app.scheduledCutDelay() === AUTHORITATIVE_CUT_TIMEOUT_MS
      && app.display?.pending?.phase === 'awaiting-authoritative-cut'
      && AUTHORITATIVE_CUT_TIMEOUT_MS === 15_000);
  app.backgroundEvent(A, 1);
  const toastsBefore = app.switchToasts.length;
  app.advanceTime(AUTHORITATIVE_CUT_TIMEOUT_MS - 1);
  check('silent subscription stays pending while the socket stays responsive',
    app.display?.pending?.phase === 'awaiting-authoritative-cut'
      && app.switchPending === true
      && sameRuntime(app.display!.displayed, A)
      && switched !== undefined);
  app.advanceTime(1);
  check('silent cut timeout on a live socket rolls back to source',
    await switched === false
      && app.switchPending === false
      && app.display?.pending === null
      && sameRuntime(app.display!.displayed, A)
      && app.items[A.sessionId] === 'source transcript text'
      && !app.invalidations.some((identity) => sameRuntime(identity, A))
      && app.invalidations.length === 1
      && sameRuntime(app.invalidations[0], B)
      && app.switchToasts.length === toastsBefore + 1
      && app.switchToasts[0] === 'switch timed out waiting for subscription snapshot'
      && app.scheduledCutDelay() === null);
}

{
  const app = new AppSwitchHarness();
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.resolve(handoff(B, 4));
  await flush();
  app.refillInFlight = true;
  const toastsBefore = app.switchToasts.length;
  app.advanceTime(AUTHORITATIVE_CUT_TIMEOUT_MS);
  check('cut timer pauses while reconnect refill is in flight',
    app.display?.pending?.phase === 'awaiting-authoritative-cut'
      && app.switchPending === true
      && sameRuntime(app.display!.displayed, A)
      && app.scheduledCutDelay() === AUTHORITATIVE_CUT_TIMEOUT_MS
      && switched !== undefined);
  app.noteCutProgress();
  app.refillInFlight = false;
  app.advanceTime(AUTHORITATIVE_CUT_TIMEOUT_MS - 1);
  check('cut stays pending through refill progress plus almost one silence window',
    app.display?.pending?.phase === 'awaiting-authoritative-cut'
      && app.switchPending === true);
  app.advanceTime(1);
  check('15s silence after refill still rolls back',
    await switched === false
      && app.switchPending === false
      && app.display?.pending === null
      && sameRuntime(app.display!.displayed, A)
      && app.switchToasts.length === toastsBefore + 1
      && app.switchToasts[toastsBefore] === 'switch timed out waiting for subscription snapshot');
}

{
  const app = new AppSwitchHarness();
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.resolve(handoff(B, 4));
  await flush();
  app.socketGen += 1;
  app.advanceTime(AUTHORITATIVE_CUT_TIMEOUT_MS);
  check('stale cut timer after socket generation change does not roll back',
    app.display?.pending?.phase === 'awaiting-authoritative-cut'
      && app.switchPending === true
      && sameRuntime(app.display!.displayed, A)
      && switched !== undefined);
}

{
  const app = new AppSwitchHarness();
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.resolve(handoff(B, 4));
  await flush();
  app.emitSubscriptionError({
    type: 'not-subscription-error',
    sessionId: B.sessionId,
    runtimeIncarnation: B.runtimeIncarnation,
  });
  check('typed parse failure with trustworthy identity rolls back',
    await switched === false
      && app.display?.pending === null
      && sameRuntime(app.display!.displayed, A)
      && app.switchToasts.length === 1);
}

{
  const app = new AppSwitchHarness();
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.resolve(handoff(B, 4));
  await flush();
  app.emitSubscriptionError({
    type: 'not-subscription-error',
    sessionId: B.sessionId,
    runtimeIncarnation: B.runtimeIncarnation,
  });
  check('typed parse failure with trustworthy identity rolls back',
    await switched === false
      && app.display?.pending === null
      && sameRuntime(app.display!.displayed, A)
      && app.switchToasts.length === 1);
}

// A handoff describing a different catalog row (wrong catalog tuple) rolls the
// transaction back exactly once with a single user toast.
{
  const app = new AppSwitchHarness();
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.resolve({ ...handoff(), catalogSessionId: 'other-catalog' });
  await flush();
  check('wrong catalog tuple handoff rolls back to source without subscribing',
    sameRuntime(app.display.displayed, A) && sameRuntime(app.display.commandTarget, A)
      && app.display.pending === null && app.subscriptions.length === 0);
  check('wrong catalog tuple settles false and emits exactly one user toast',
    await switched === false && app.switchToasts.length === 1);
}

// A foreign catalog row resolves to a DISTINCT native runtime id: the handoff
// is accepted, and its own runtime cut settles the transaction true.
{
  const app = new AppSwitchHarness();
  const response = app.queueResponse();
  const switched = app.request(targetForeign);
  response.resolve({
    ...handoff(foreignRuntime, 3),
    catalogSource: 'codex',
    catalogSessionId: 'codex-catalog-1',
    catalogStatus: 'foreign',
    session: switchSessionRow(foreignRuntime),
  });
  await flush();
  check('accepted foreign handoff publishes the native row before its cut',
    app.lifecycleRow?.sessionId === foreignRuntime.sessionId
      && app.lifecycleRow?.runtimeIncarnation === foreignRuntime.runtimeIncarnation
      && app.lifecycleRow?.source === 'rpi'
      && app.lifecycleRow?.status === 'native'
      && app.lifecycleRow?.loaded === true);
  check('foreign switch installs the distinct native runtime cold',
    app.subscriptions.length === 1 && sameRuntime(app.subscriptions[0].identity, foreignRuntime)
      && app.subscriptions[0].cursor === 0);
  app.acceptCut(cut(foreignRuntime, 3));
  check('foreign switch settles true on the distinct runtime cut', await switched === true
    && sameRuntime(app.display.displayed, foreignRuntime)
    && app.preference === foreignRuntime.sessionId);
}

// A rejected switch RPC toasts exactly once for a user-origin rollback; a
// superseded or already-owned failure never duplicates it.
{
  const app = new AppSwitchHarness();
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.reject(Object.assign(new Error('rpc failed'), { toastOwned: true }));
  await flush();
  check('already-owned switch failure rolls back without a second toast',
    await switched === false && app.switchToasts.length === 0);
  const response2 = app.queueResponse();
  const switched2 = app.request(targetB, 'bootstrap');
  response2.reject(new Error('bootstrap rpc failed'));
  await flush();
  check('bootstrap-origin switch of a saved target toasts', app.switchToasts.length === 1
    && app.switchToasts[0] === 'switch to target failed: bootstrap rpc failed'
    && await switched2 === false);
  const response3 = app.queueResponse();
  void app.request(targetB);
  response3.reject(new Error('user rpc failed'));
  await flush();
  check('user-origin switch rollback emits exactly one toast naming the session',
    app.switchToasts.join('|') === 'switch to target failed: bootstrap rpc failed|switch to target failed: user rpc failed');
}

{
  const app = new AppSwitchHarness();
  const retry = app.queueResponse();
  app.completeCatalogRestore({
    saved: 'target',
    primarySessionId: A.sessionId,
    restoredTarget: { sessionId: 'target', path: 'target.jsonl' },
    switchOk: false,
    retryTarget: targetB,
  });
  check('failed saved bootstrap does not persist the listener primary',
    app.preferenceWrites.length === 0
      && app.preference === A.sessionId
      && app.readyMarked === 1
      && app.bootstrapRetries === 1);
  retry.reject(new Error('saved target still unavailable'));
  await flush();
  check('bootstrap retry after readyGate.markReady toasts the failed saved target',
    app.switchToasts.join('|') === 'switch to target failed: saved target still unavailable');
}

{
  const app = new AppSwitchHarness();
  app.preferenceWrites.length = 0;
  app.completeCatalogRestore({
    saved: '',
    primarySessionId: A.sessionId,
    restoredTarget: null,
    switchOk: true,
    retryTarget: targetB,
  });
  check('absent saved preference persists primary and does not retry a switch',
    app.preferenceWrites.join(',') === A.sessionId
      && app.bootstrapRetries === 0
      && app.readyMarked === 1);
}

// The parsed machine failure code reaches the switch toast unchanged.
{
  const app = new AppSwitchHarness();
  const response = app.queueResponse();
  void app.request(targetB);
  response.reject(Object.assign(
    new Error('session could not be opened right now; verify the selection and retry'),
    { rpcCode: 'runtime_unavailable' },
  ));
  await flush();
  check('failure toast names the session and the backend code',
    app.switchToasts.join('|')
      === 'switch to target failed (runtime_unavailable): session could not be opened right now; verify the selection and retry');
}

// A warm target may paint after handoff, but a transport close before the
// authoritative cut rolls every App-owned surface back to the source exactly once.
{
  const app = new AppSwitchHarness();
  app.ready.set(B.sessionId, { identity: B, cursor: 9, socketGeneration: app.socketGen });
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.resolve(handoff(B, 4));
  await flush();
  check('warm handoff paints target view while transaction remains pending',
    sameRuntime(app.display.displayed, B) && app.displayedView === 'target-view' && app.switchPending);
  app.close();
  check('close after handoff restores source displayed and command target',
    sameRuntime(app.display.displayed, A) && sameRuntime(app.display.commandTarget, A));
  check('warm close restores source exactly once and writes its preference once',
    app.activations.filter(({ identity }) => sameRuntime(identity, A)).length === 1
      && app.preferenceWrites.join(',') === A.sessionId
      && sameRuntime(app.active, A) && app.displayedView === 'source-view' && app.preference === A.sessionId);
  check('close resolves false exactly once and clears switch pending',
    await switched === false && app.resolverCalls.get(1)?.join(',') === 'false' && !app.switchPending);
  check('pending clears before reconnect scheduling', app.order.join(',') === 'pending-cleared,reconnect', app.order.join(','));
  app.close();
  check('later close cannot settle the completed generation twice', app.resolverCalls.get(1)?.length === 1);
}

// A handed-off target with no ready cache is provisional and must be invalidated
// before rollback/reconnect so the next connection cannot reuse its view/runtime.
{
  const app = new AppSwitchHarness();
  app.displayedView = 'live-source-dom-edits';
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.resolve(handoff());
  await flush();
  app.close();
  check('close invalidates an unready handed-off target',
    app.invalidations.length === 1 && sameRuntime(app.invalidations[0], B) && !app.viewSlots.has(runtimeKey(B)));
  check('cold close preserves live source DOM and skips redundant activation/preference write',
    await switched === false && sameRuntime(app.display.displayed, A)
      && app.displayedView === 'live-source-dom-edits' && app.activations.length === 0
      && app.preferenceWrites.length === 0 && app.preference === A.sessionId);
}

// Same-target callers join the one stored transaction. Only one resolver,
// switch RPC, and subscription are emitted, and a successful cut never reports false.
{
  const app = new AppSwitchHarness();
  const response = app.queueResponse();
  const first = app.request(targetB);
  const second = app.request({ ...targetB });
  check('same-target caller joins one stored transaction', app.createdTransactions === 1);
  check('same-target caller emits one switch RPC', app.sends.length === 1);
  response.resolve(handoff());
  await flush();
  check('same-target handoff emits one subscription', app.subscriptions.length === 1);
  app.acceptCut(cut());
  const results = await Promise.all([first, second]);
  check('same-target callers resolve true without a false settlement',
    results.join(',') === 'true,true' && app.resolverCalls.get(1)?.join(',') === 'true');
  check('successful cut clears pending and binds target command/display authority',
    !app.switchPending && app.display.pending === null
      && sameRuntime(app.display.displayed, B) && sameRuntime(app.display.commandTarget, B));
}

// A second same-target call returns the EXACT stored Promise object, so a
// caller can await the same settlement without a wrapper or re-resolution.
{
  const app = new AppSwitchHarness();
  app.queueResponse();
  const first = app.request(targetB);
  const second = app.request({ ...targetB });
  check('same-target second call returns the exact stored Promise object', first === second);
  check('same-target second call does not create a second transaction', app.createdTransactions === 1);
}

// Before initial authority binding, switching returns false without creating a transaction.
{
  const app = new AppSwitchHarness();
  app.display = null as unknown as SessionDisplayState;
  const switched = app.request(targetB);
  check('unbound display returns a resolved false promise', await switched === false);
  check('unbound display never starts a transaction or RPC',
    app.createdTransactions === 0 && app.sends.length === 0 && !app.switchPending);
}

// A current RPC rejection rolls back to the exact source, settles the caller
// false exactly once, and clears the switch UI.
{
  const app = new AppSwitchHarness();
  app.displayedView = 'live-source-dom-edits';
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.reject(new Error('rpc failed'));
  await flush();
  check('RPC rejection restores source authority and settles false once',
    await switched === false && app.display.pending === null
      && sameRuntime(app.display.displayed, A) && sameRuntime(app.display.commandTarget, A)
      && app.resolverCalls.get(1)?.join(',') === 'false' && !app.switchPending);
  check('RPC rejection preserves live source DOM and skips redundant activation',
    app.displayedView === 'live-source-dom-edits' && app.activations.length === 0
      && app.preferenceWrites.length === 0 && app.preference === A.sessionId);
}

// A stale superseded RPC rejection cannot affect the current generation: the
// replacement transaction stays pending and settles on its own outcome.
{
  const app = new AppSwitchHarness();
  const staleResponse = app.queueResponse();
  const currentResponse = app.queueResponse();
  const staleSwitch = app.request(targetB);
  const currentSwitch = app.request(targetC);
  check('superseded switch resolves false before its stale RPC rejects',
    await staleSwitch === false && app.resolverCalls.get(1)?.join(',') === 'false');
  staleResponse.reject(new Error('stale rpc failed'));
  await flush();
  check('stale RPC rejection cannot affect the replacement generation',
    app.display.pending?.generation === 2 && app.resolverCalls.get(2) === undefined
      && app.sends.length === 2 && app.subscriptions.length === 0);
  app.lifecycleResult(lifecycleData(A));
  await flush();
  currentResponse.resolve(handoff(C, 6));
  await flush();
  check('replacement handoff subscribes only its exact runtime',
    app.subscriptions.length === 1 && sameRuntime(app.subscriptions[0].identity, C));
  app.acceptCut(cut(C, 6));
  check('replacement transaction remains independently successful after stale rejection',
    await currentSwitch === true && app.resolverCalls.get(2)?.join(',') === 'true');
}

// A lifecycle result for the pending switch's source authority keeps the
// transaction; a result for a truly different authority settles the exact
// pending generation false and clears the switch UI.
{
  const app = new AppSwitchHarness();
  const response = app.queueResponse();
  const switched = app.request(targetB);
  app.lifecycleResult(lifecycleData(A));
  check('source-authority lifecycle result preserves the pending transaction',
    app.display.pending?.generation === 1 && app.resolverCalls.get(1) === undefined && app.switchPending);
  response.resolve(handoff());
  await flush();
  app.acceptCut(cut());
  check('preserved transaction still settles true on its own cut',
    await switched === true && app.resolverCalls.get(1)?.join(',') === 'true');
}
{
  const app = new AppSwitchHarness();
  app.ready.set(B.sessionId, { identity: B, cursor: 9, socketGeneration: app.socketGen });
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.resolve(handoff(B, 4));
  await flush();
  check('warm handoff activates target before source lifecycle result', sameRuntime(app.active, B));
  app.displayedView = 'fresh-source-lifecycle-view';
  app.lifecycleResult(lifecycleData(A));
  check('source lifecycle result synchronizes exact DOM owner without clearing pending',
    sameRuntime(app.active, A) && app.display.pending?.generation === 1);
  app.acceptCut(cut(B, 4));
  check('later target cut saves fresh source DOM and restores target slot',
    await switched === true && app.viewSlots.get(runtimeKey(A)) === 'fresh-source-lifecycle-view'
      && sameRuntime(app.active, B) && app.displayedView === 'target-view');
}

{
  const app = new AppSwitchHarness();
  app.ready.set(B.sessionId, { identity: B, cursor: 9, socketGeneration: app.socketGen });
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.resolve(handoff(B, 4));
  await flush();
  const handoffRow = app.lifecycleRow;
  let lifecycleError = '';
  await app.lifecycleResult(null).catch((error) => {
    lifecycleError = error instanceof Error ? error.message : String(error);
  });
  check('malformed lifecycle rejects before payload mutation',
    lifecycleError === 'lifecycle response is missing the target sessionId'
      && app.lifecyclePayloadWrites === 0 && app.lifecycleRow === handoffRow);
  check('malformed lifecycle identity bumps zero catalog revisions and schedules no refresh',
    app.catalogRefreshRevision === 0 && app.catalogRefreshRequests.length === 0);
  check('malformed lifecycle identity cannot mutate warm pending authority',
    sameRuntime(app.active, B) && app.display.pending?.generation === 1
      && app.resolverCalls.get(1) === undefined && app.switchPending);
  app.acceptCut(cut(B, 4));
  check('independent warm switch still succeeds without source slot corruption',
    await switched === true && app.viewSlots.get(runtimeKey(A)) === 'source-view'
      && sameRuntime(app.active, B) && app.displayedView === 'target-view');
}

{
  const app = new AppSwitchHarness();
  const response = app.queueResponse();
  const switched = app.request(targetB);
  app.lifecycleResult(lifecycleData(C));
  check('different-authority lifecycle result settles the exact generation false',
    await switched === false && app.resolverCalls.get(1)?.join(',') === 'false'
      && app.display.pending === null && !app.switchPending);
  response.resolve(handoff());
  await flush();
  check('stale handoff after lifecycle supersession cannot affect the cleared transaction',
    app.subscriptions.length === 0 && app.display.pending === null && app.resolverCalls.get(1)?.length === 1);
}

// A background authoritative cut marks exact readiness without changing the displayed session.
{
  const app = new AppSwitchHarness();
  app.acceptCut(cut(B, 9));
  check('background cut preserves displayed and command authority',
    sameRuntime(app.display.displayed, A) && sameRuntime(app.display.commandTarget, A)
      && sameRuntime(app.active, A) && app.display.pending === null);
  check('background cut installs exact ready watermark',
    sameRuntime(app.ready.get(B.sessionId)?.identity, B) && app.ready.get(B.sessionId)?.cursor === 9);
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.resolve(handoff(B, 9));
  await flush();
  check('later switch uses background continuity as warm',
    sameRuntime(app.active, B) && app.subscriptions[0]?.cursor === 9);
  app.acceptCut({
    type: 'subscription_snapshot',
    sessionId: B.sessionId,
    runtimeIncarnation: B.runtimeIncarnation,
    watermark: 9,
    reason: 'warm-continuity',
    state: { sessionId: B.sessionId },
    messages: [],
    events: [],
  });
  check('warm empty cut holds bind until get_messages confirms',
    app.historyConfirms.join(',') === B.sessionId
      && app.pendingWarmBindHeld()
      && app.display?.pending !== null);
  app.finishWarmHistoryConfirm(true);
  check('background-warmed switch settles on exact cut', await switched === true);
}

{
  const app = new AppSwitchHarness();
  app.ready.set(B.sessionId, { identity: B, cursor: 4, socketGeneration: app.socketGen });
  app.cursors.set(runtimeKey(B), 4);
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.resolve(handoff(B, 4));
  await flush();
  app.acceptCut({
    type: 'subscription_snapshot',
    sessionId: B.sessionId,
    runtimeIncarnation: B.runtimeIncarnation,
    watermark: 4,
    reason: 'warm-continuity',
    state: { sessionId: B.sessionId },
    messages: [],
    events: [],
  });
  check('warm empty bind waits for the get_messages confirmation',
    app.historyConfirms.join(',') === B.sessionId
      && app.pendingWarmBindHeld()
      && app.display?.pending !== null);
  app.finishWarmHistoryConfirm(false);
  check('warm confirm RPC error restores the source transaction instead of clearing the UI',
    await switched === false
      && sameRuntime(app.display?.displayed, A)
      && app.display?.pending === null
      && !app.pendingWarmBindHeld());
}

{
  const app = new AppSwitchHarness();
  app.ready.set(B.sessionId, { identity: B, cursor: 4, socketGeneration: app.socketGen });
  app.cursors.set(runtimeKey(B), 4);
  app.durableItemCount[B.sessionId] = 3;
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.resolve(handoff(B, 4));
  await flush();
  app.acceptCut({
    type: 'subscription_snapshot',
    sessionId: B.sessionId,
    runtimeIncarnation: B.runtimeIncarnation,
    watermark: 4,
    reason: 'warm-continuity',
    state: { sessionId: B.sessionId },
    messages: [],
    events: [],
  });
  check('warm empty cut skips confirm when merged durable history is already present',
    app.historyConfirms.length === 0
      && !app.pendingWarmBindHeld()
      && await switched === true);
}

{
  const app = new AppSwitchHarness();
  app.ready.set(B.sessionId, { identity: B, cursor: 4, socketGeneration: app.socketGen });
  app.cursors.set(runtimeKey(B), 4);
  const responseB = app.queueResponse();
  const switchedB = app.request(targetB);
  responseB.resolve(handoff(B, 4));
  await flush();
  app.acceptCut({
    type: 'subscription_snapshot',
    sessionId: B.sessionId,
    runtimeIncarnation: B.runtimeIncarnation,
    watermark: 4,
    reason: 'warm-continuity',
    state: { sessionId: B.sessionId },
    messages: [],
    events: [],
  });
  check('warm confirm keeps owned pending and does not stamp mark-ready before bind',
    app.pendingWarmBindHeld()
      && app.display?.pending !== null
      && app.ready.get(B.sessionId)?.cursor === 4);
  app.backgroundEvent(B, 5);
  check('live events during warm confirmation advance the live cursor',
    (app.cursors.get(runtimeKey(B)) ?? 0) === 5);
  const responseC = app.queueResponse();
  const switchedC = app.request(targetC);
  responseC.resolve(handoff(C, 0));
  await flush();
  app.finishWarmHistoryConfirm(true);
  check('same-socket switch C during B confirmation ignores the stale B commit',
    await switchedB === false
      && app.display?.pending !== null
      && sameRuntime(app.display?.pending?.targetIdentity, C)
      && (app.cursors.get(runtimeKey(B)) ?? 0) === 5);
  app.acceptCut(cut(C, 0));
  check('replacement switch C still settles after ignoring B confirmation',
    await switchedC === true && sameRuntime(app.display?.displayed, C));
}

{
  const app = new AppSwitchHarness();
  app.ready.set(B.sessionId, { identity: B, cursor: 4, socketGeneration: app.socketGen });
  app.cursors.set(runtimeKey(B), 4);
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.resolve(handoff(B, 4));
  await flush();
  app.acceptCut({
    type: 'subscription_snapshot',
    sessionId: B.sessionId,
    runtimeIncarnation: B.runtimeIncarnation,
    watermark: 6,
    reason: 'warm-continuity',
    state: { sessionId: B.sessionId },
    messages: [],
    events: [
      { type: 'turn_started', sequence: 5, sessionId: B.sessionId, runtimeIncarnation: B.runtimeIncarnation },
      { type: 'turn_started', sequence: 6, sessionId: B.sessionId, runtimeIncarnation: B.runtimeIncarnation },
    ],
  });
  check('warm replay tail stamps the live cursor before confirmation without granting ready',
    app.pendingWarmBindHeld()
    && app.display?.pending !== null
    && sameRuntime(app.display?.displayed, B)
    && sameRuntime(app.display?.commandTarget, A)
    && (app.ready.get(B.sessionId)?.cursor ?? 0) === 4
    && (app.cursors.get(runtimeKey(B)) ?? 0) === 6
    && app.pumpLiveFrame(B, 6) === 'ignored'
    && app.pumpLiveFrame(B, 7) === 'applied'
    && (app.cursors.get(runtimeKey(B)) ?? 0) === 7);
  const ungated = new AppSwitchHarness();
  ungated.cursors.set(runtimeKey(B), 4);
  check('without the replay-tail stamp a live frame 6 at cursor 4 resubscribes',
    ungated.pumpLiveFrame(B, 6) === 'resubscribe');
  app.finishWarmHistoryConfirm(true);
  check('warm replay-tail confirmation still settles after admitting live frames',
    await switched === true && sameRuntime(app.display?.displayed, B));
}

{
  const app = new AppSwitchHarness();
  app.ready.set(B.sessionId, { identity: B, cursor: 4, socketGeneration: app.socketGen });
  app.cursors.set(runtimeKey(B), 4);
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.resolve(handoff(B, 4));
  await flush();
  app.acceptCut({
    type: 'subscription_snapshot',
    sessionId: B.sessionId,
    runtimeIncarnation: B.runtimeIncarnation,
    watermark: 4,
    reason: 'warm-continuity',
    state: { sessionId: B.sessionId },
    messages: [],
    events: [],
  });
  app.backgroundEvent(B, 5);
  app.finishWarmHistoryConfirm(true);
  check('warm confirm bind preserves the advanced live cursor',
    await switched === true
      && sameRuntime(app.display?.displayed, B)
      && (app.cursors.get(runtimeKey(B)) ?? 0) === 5
      && app.ready.get(B.sessionId)?.cursor === 5);
}

// Fork lifecycle: the child result applies the exact composer draft, bumps the
// catalog revision exactly once, and schedules exactly one background catalog
// refresh that activation never awaits. Each later invocation bumps exactly
// once more.
{
  const app = new AppSwitchHarness();
  const activated = app.lifecycleResult(lifecycleData(
    { sessionId: 'fork-child', runtimeIncarnation: 'fork-child-inc' },
    { editorText: 'PONG fork draft exact' },
  ));
  check('fork lifecycle applies the child draft and bumps the catalog revision exactly once',
    app.active?.sessionId === 'fork-child' && app.composerDraft === 'PONG fork draft exact'
      && app.catalogRefreshRevision === 1 && app.catalogRefreshRequests.length === 1);
  check('fork lifecycle injects the validated authoritative session row as the overlay',
    app.lifecycleRow?.sessionId === 'fork-child' && app.lifecycleRow?.source === 'rpi'
      && app.lifecycleRow?.status === 'native' && app.lifecycleRow?.loaded === true
      && app.lifecycleRow?.cwd === '/workspace/fork-child'
      && app.lifecycleRow?.path === '/store/fork-child.jsonl'
      && app.lifecycleRow?.runtimeIncarnation === 'fork-child-inc');
  const refresh = app.catalogRefreshRequests[0]!;
  check('lifecycle activation resolves while its catalog refresh is still pending',
    await activated === undefined && refresh instanceof Promise);
  await app.lifecycleResult(lifecycleData({ sessionId: 'fork-child-2', runtimeIncarnation: 'fork-child-2-inc' }));
  check('second lifecycle invocation bumps the revision exactly once more and replaces the overlay',
    app.catalogRefreshRevision === 2 && app.catalogRefreshRequests.length === 2
      && app.lifecycleRow?.sessionId === 'fork-child-2');
  app.clearLifecycleRow();
  check('absorbed overlay clears to null exactly once', app.lifecycleRow === null && app.lifecycleRowClears === 1);
}

// A contract mismatch (authoritative row disagreeing with the target) rejects
// and mutates NOTHING: no activation, no overlay, no revision, no refresh.
{
  const app = new AppSwitchHarness();
  const before = {
    active: app.active,
    revision: app.catalogRefreshRevision,
    requests: app.catalogRefreshRequests.length,
    draft: app.composerDraft,
  };
  const mismatches: Array<[string, unknown]> = [
    ['wrong session row sessionId', {
      ...lifecycleData({ sessionId: 'child', runtimeIncarnation: 'inc' }),
      session: { source: 'rpi', sessionId: 'OTHER', runtimeIncarnation: 'inc', cwd: '/w/child', path: '/s/child.jsonl', status: 'native', loaded: true },
    }],
    ['wrong row incarnation', {
      ...lifecycleData({ sessionId: 'child', runtimeIncarnation: 'inc' }),
      session: { source: 'rpi', sessionId: 'child', runtimeIncarnation: 'OTHER', cwd: '/w/child', path: '/s/child.jsonl', status: 'native', loaded: true },
    }],
    ['row cwd disagrees with state', {
      sessionId: 'child', runtimeIncarnation: 'inc',
      state: { sessionId: 'child', cwd: '/w/child', sessionFile: '/s/child.jsonl' },
      session: { source: 'rpi', sessionId: 'child', runtimeIncarnation: 'inc', cwd: '/w/OTHER', path: '/s/child.jsonl', status: 'native', loaded: true },
    }],
    ['row path disagrees with state sessionFile', {
      ...lifecycleData({ sessionId: 'child', runtimeIncarnation: 'inc' }),
      session: { source: 'rpi', sessionId: 'child', runtimeIncarnation: 'inc', cwd: '/w/child', path: '/s/OTHER.jsonl', status: 'native', loaded: true },
    }],
    ['foreign source', {
      ...lifecycleData({ sessionId: 'child', runtimeIncarnation: 'inc' }),
      session: { source: 'codex', sessionId: 'child', runtimeIncarnation: 'inc', cwd: '/w/child', path: '/s/child.jsonl', status: 'native', loaded: true },
    }],
    ['non-native status', {
      ...lifecycleData({ sessionId: 'child', runtimeIncarnation: 'inc' }),
      session: { source: 'rpi', sessionId: 'child', runtimeIncarnation: 'inc', cwd: '/w/child', path: '/s/child.jsonl', status: 'foreign', loaded: true },
    }],
    ['not loaded', {
      ...lifecycleData({ sessionId: 'child', runtimeIncarnation: 'inc' }),
      session: { source: 'rpi', sessionId: 'child', runtimeIncarnation: 'inc', cwd: '/w/child', path: '/s/child.jsonl', status: 'native', loaded: false },
    }],
    ['missing state snapshot', {
      sessionId: 'child', runtimeIncarnation: 'inc',
      session: { source: 'rpi', sessionId: 'child', runtimeIncarnation: 'inc', cwd: '/w/child', path: '/s/child.jsonl', status: 'native', loaded: true },
    }],
  ];
  for (const [label, wire] of mismatches) {
    let rejected = '';
    await app.lifecycleResult(wire).catch((error: unknown) => {
      rejected = error instanceof Error ? error.message : String(error);
    });
    check(`contract mismatch (${label}) rejects and mutates nothing`,
      rejected.length > 0
        && app.lifecyclePayloadWrites === 0 && app.lifecycleRow === null
        && app.catalogRefreshRevision === before.revision
        && app.catalogRefreshRequests.length === before.requests
        && app.composerDraft === before.draft
        && sameRuntime(app.active, before.active)
        && (app.display === null || app.display.pending === null));
  }
}

// A negotiated switch succeeds through the separate requestSessionSwitch
// handoff/cut path: it must never bump the catalog revision nor schedule a
// lifecycle-driven catalog refresh.
{
  const app = new AppSwitchHarness();
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.resolve(handoff());
  await flush();
  app.acceptCut(cut());
  check('negotiated switch success never bumps the catalog revision',
    await switched === true && app.catalogRefreshRevision === 0
      && app.catalogRefreshRequests.length === 0);
}

// An unload of the exact cold pending target clears the transaction without
// redundantly restoring source view/preference when source stayed activated.
{
  const app = new AppSwitchHarness();
  app.displayedView = 'live-source-dom-edits';
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.resolve(handoff());
  await flush();
  app.unload(B);
  check('pending-target unload clears the transaction and restores source authority',
    app.display !== null && app.display.pending === null
      && sameRuntime(app.display.displayed, A) && sameRuntime(app.display.commandTarget, A));
  check('cold pending-target unload preserves live source DOM and preference',
    sameRuntime(app.active, A) && app.displayedView === 'live-source-dom-edits'
      && app.preference === A.sessionId && app.activations.length === 0 && app.preferenceWrites.length === 0);
  check('pending-target unload invalidates target and resolves false once',
    await switched === false && app.invalidations.some((identity) => sameRuntime(identity, B))
      && app.resolverCalls.get(1)?.join(',') === 'false' && !app.switchPending);
}

{
  const app = new AppSwitchHarness();
  app.display = { displayed: B, commandTarget: B, pending: null, nextGeneration: 1 };
  app.active = B;
  app.displayedView = 'target-view';
  app.preference = B.sessionId;
  app.ready.set(A.sessionId, { identity: A, cursor: 7, socketGeneration: app.socketGen });
  app.unload(B);
  check('idle displayed secondary unload restores live source authority',
    app.display !== null && sameRuntime(app.display.displayed, A) && sameRuntime(app.display.commandTarget, A)
      && app.display.pending === null);
  check('idle displayed secondary unload does not leave commandTarget on the dead identity',
    app.display !== null && !sameRuntime(app.display.commandTarget, B)
      && sameRuntime(app.active, A) && app.preference === A.sessionId);
  check('idle displayed secondary unload activates source view and keeps a live subscription',
    app.activations.length === 1 && sameRuntime(app.activations[0]?.identity, A) && app.activations[0]?.restoreView === true
      && app.displayedView === 'source-view'
      && app.subscriptions.some((entry) => sameRuntime(entry.identity, A))
      && app.invalidations.some((identity) => sameRuntime(identity, B)));
}

{
  const app = new AppSwitchHarness();
  app.display = { displayed: B, commandTarget: B, pending: null, nextGeneration: 1 };
  app.active = B;
  app.displayedView = 'target-view';
  app.preference = B.sessionId;
  app.unload(B, null);
  check('displayed unload without live fallback fails closed',
    app.display === null && app.active === null && app.displayedView === ''
      && app.invalidations.some((identity) => sameRuntime(identity, B))
      && app.subscriptions.length === 0 && app.activations.length === 0);
}

// ---------------------------------------------------------------------------
// Frontend-only exact-ready fast path (App.requestSessionSwitch): zero
// switch_session/subscribe frames, synchronous settlement, ready cache
// authoritative. Source stays untouched on every negotiated fallback.

function seedReady(app: AppSwitchHarness, identity: RuntimeIdentity, cursor = 4): void {
  app.resident[identity.sessionId] = identity.runtimeIncarnation;
  app.cursors.set(runtimeKey(identity), cursor);
  app.ready.set(identity.sessionId, { identity, cursor, socketGeneration: app.socketGen });
}

// Exact-ready target: zero sends/subscriptions, immediate
// active/view/commandTarget/preference, and A→B→A is frontend-only both ways.
{
  const app = new AppSwitchHarness();
  seedReady(app, B, 7);
  seedReady(app, A, 9);
  const switched = app.request(targetB);
  check('exact-ready switch resolves true synchronously', await switched === true);
  check('exact-ready switch skips the O(transcript) DOM scan (cached slot reused)',
    app.transcriptScans === 0);
  check('exact-ready switch activates target, restores its cached view, and clears unread',
    sameRuntime(app.active, B) && app.displayedView === 'target-view'
    && app.activations.length === 1 && app.activations[0].restoreView === true
    && app.unreadClears.join(',') === B.sessionId);
  check('exact-ready switch atomically moves displayed and commandTarget',
    sameRuntime(app.display.displayed, B) && sameRuntime(app.display.commandTarget, B)
    && app.display.pending === null && !app.switchPending);
  check('exact-ready switch saves the target preference', app.preference === B.sessionId);
  check('exact-ready switch preserves the ready cache and view slot of the target',
    app.ready.get(B.sessionId)?.cursor === 7 && app.viewSlots.has(runtimeKey(B)));
  const sendsBefore = app.sends.length;
  const back = app.request({
    path: 'source.jsonl',
    catalogSource: 'rpi',
    catalogSessionId: A.sessionId,
    catalogStatus: 'native',
    catalogRuntimeIncarnation: A.runtimeIncarnation,
  });
  check('reverse exact-ready switch is frontend-only too', await back === true
    && app.sends.length === sendsBefore && app.subscriptions.length === 0
    && sameRuntime(app.active, A) && app.displayedView === 'source-view'
    && sameRuntime(app.display.displayed, A) && sameRuntime(app.display.commandTarget, A)
    && app.preference === A.sessionId && app.fastSwitches === 2);
  check('round-trip fast switches never scan the transcript DOM',
    app.transcriptScans === 0);
}

// A still-streaming target keeps its exact subscription: the fast switch
// never tears down or re-subscribes the target runtime, and its ready cache
// (advanced by background events) survives untouched.
{
  const app = new AppSwitchHarness();
  seedReady(app, B, 41);
  await app.request(targetB);
  // Background events advance the exact ready cursor while B stays resident.
  app.ready.set(B.sessionId, { identity: B, cursor: 44, socketGeneration: app.socketGen });
  check('streaming target stays ready and resident across the fast switch',
    app.ready.get(B.sessionId)?.cursor === 44
    && app.resident[B.sessionId] === B.runtimeIncarnation
    && app.invalidations.length === 0 && app.subscriptions.length === 0);
}

// Stale hint, null hint, catalog mismatch, absent view slot, unloaded runtime,
// and foreign rows all fail closed to exactly ONE negotiated switch RPC.
{
  const variants: Array<[string, SessionSwitchTarget]> = [
    ['stale hint incarnation', { ...targetB, catalogRuntimeIncarnation: 'reopen-inc' }],
    ['null (unloaded) hint', { ...targetB, catalogRuntimeIncarnation: null }],
    ['catalog session mismatch', { ...targetB, catalogSessionId: 'other-catalog' }],
  ];
  for (const [name, variant] of variants) {
    const app = new AppSwitchHarness();
    seedReady(app, B, 7);
    app.queueResponse().reject(new Error('negotiated path taken'));
    const switched = app.request(variant);
    await flush();
    check(`${name} fails closed to exactly one negotiated switch RPC`,
      app.sends.length === 1 && app.fastSwitches === 0 && await switched === false
      && sameRuntime(app.display.displayed, A));
    check(`${name} never activates or subscribes the target frontend-only`,
      app.subscriptions.length === 0 && app.activations.length === 0);
    check(`${name} pays exactly one O(transcript) DOM scan (negotiated save)`,
      app.transcriptScans === 1);
  }
  // Resident overlay disagreement (stale catalog overlay vs ready runtime).
  {
    const app = new AppSwitchHarness();
    seedReady(app, B, 4);
    app.resident[B.sessionId] = 'reopen-inc';
    app.queueResponse().reject(new Error('negotiated path taken'));
    await app.request(targetB);
    await flush();
    check('resident overlay mismatch fails closed to the negotiated RPC',
      app.sends.length === 1 && app.fastSwitches === 0);
  }
  // Absent view slot: fail closed even though the runtime is exact-ready.
  {
    const app = new AppSwitchHarness();
    seedReady(app, B, 4);
    app.viewSlots.delete(runtimeKey(B));
    app.queueResponse().reject(new Error('negotiated path taken'));
    await app.request(targetB);
    await flush();
    check('missing view slot fails closed to the negotiated RPC',
      app.sends.length === 1 && app.fastSwitches === 0);
  }
  // Unloaded runtime: invalidate removes ready/resident/slot, so the next
  // request negotiates from scratch.
  {
    const app = new AppSwitchHarness();
    seedReady(app, B, 4);
    app.unload(B, A);
    app.queueResponse().reject(new Error('negotiated path taken'));
    await app.request(targetB);
    await flush();
    check('unloaded runtime fails closed to the negotiated RPC',
      app.sends.length === 1 && app.fastSwitches === 0);
  }
  // Foreign rows never fast-path.
  {
    const app = new AppSwitchHarness();
    app.queueResponse().reject(new Error('negotiated path taken'));
    await app.request(targetForeign);
    await flush();
    check('foreign catalog row fails closed to the negotiated RPC',
      app.sends.length === 1 && app.fastSwitches === 0);
  }
}

// A pending cold transaction superseded by an exact-ready click settles the
// old generation false, and the late handoff response is ignored stale.
{
  const app = new AppSwitchHarness();
  const coldResponse = app.queueResponse();
  const cold = app.request(targetC);
  seedReady(app, B, 6);
  const fast = app.request(targetB);
  check('cold pending superseded by exact-ready target settles false immediately',
    await fast === true && app.resolverCalls.get(1)?.join(',') === 'false'
    && app.display.pending === null && !app.switchPending && app.fastSwitches === 1
    && app.sends.length === 1 && app.subscriptions.length === 0);
  coldResponse.resolve(handoff(C, 4));
  await flush();
  check('late superseded handoff response is ignored stale',
    app.subscriptions.length === 0 && sameRuntime(app.display.displayed, B)
    && sameRuntime(app.active, B) && app.resolverCalls.get(1)?.length === 1
    && await cold === false && await fast === true);
}

// Unload after a fast switch invalidates fast eligibility: the next switch to
// that session negotiates again.
{
  const app = new AppSwitchHarness();
  seedReady(app, B, 4);
  seedReady(app, A, 2);
  await app.request(targetB);
  app.unload(B, A);
  check('unload clears ready, resident, and slot for the unloaded runtime',
    app.ready.get(B.sessionId) === undefined
    && app.resident[B.sessionId] === undefined
    && !app.viewSlots.has(runtimeKey(B)));
  const backToB = {
    path: 'target.jsonl',
    catalogSource: 'rpi' as const,
    catalogSessionId: B.sessionId,
    catalogStatus: 'native' as const,
    catalogRuntimeIncarnation: null,
  };
  app.queueResponse().reject(new Error('negotiated path taken'));
  await app.request(backToB);
  await flush();
  check('post-unload switch to the same row negotiates again',
    app.sends.length === 1 && app.fastSwitches === 1);
}


// G1: reconnect bumps the socket generation; every stale ready stamp is
// ineligible, so the previously-ready B negotiates (one RPC + one subscribe)
// and never fast-switches until a fresh cut stamps it this generation.
{
  const app = new AppSwitchHarness();
  seedReady(app, B, 9);
  app.close();
  app.queueResponse().reject(new Error('negotiated path taken'));
  await app.request(targetB);
  await flush();
  check('reconnect stale ready fails closed to one negotiated RPC',
    app.sends.length === 1 && app.fastSwitches === 0);
  check('reconnect stale ready still re-subscribes through the handoff contract',
    app.subscriptions.length === 0 && sameRuntime(app.display.displayed, A));
  // A fresh cut this generation re-stamps B; the next click is fast again.
  app.ready.set(B.sessionId, { identity: B, cursor: 3, socketGeneration: app.socketGen });
  const switched = await app.request(targetB);
  check('post-reconnect fresh cut re-arms the exact-ready fast path',
    switched === true && app.fastSwitches === 1 && app.sends.length === 1
    && sameRuntime(app.active, B));
}

// A loaded background runtime restored on reconnect must own a restorable
// view slot before its authoritative cut. The next click uses zero wire.
{
  const app = new AppSwitchHarness();
  app.viewSlots.delete(runtimeKey(B));
  app.reconnectSubscribe(B);
  app.acceptCut(cut(B, 6));
  const switched = app.request(targetB);
  await flush();
  check('reconnect background cut seeds a restorable view before fast click',
    await switched === true && app.fastSwitches === 1 && app.sends.length === 0
      && app.subscriptions.length === 1 && sameRuntime(app.active, B));
}
{
  const appSource = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../src/App.tsx'), 'utf8');
  const subscribeStart = appSource.indexOf('subscribeSession: (identity) => {');
  const subscribeEnd = appSource.indexOf('},\n            }).then((target)', subscribeStart);
  const reconnectSubscribe = appSource.slice(subscribeStart, subscribeEnd);
  check('production reconnect seeds a bounded empty slot before background subscribe',
    subscribeStart >= 0
      && subscribeEnd > subscribeStart
      && reconnectSubscribe.includes('if (!viewSlotsByRuntimeKeyRef.current.has(key))')
      && reconnectSubscribe.includes('emptyViewSlot(identity)')
      && reconnectSubscribe.indexOf('upsertViewSlot(') < reconnectSubscribe.indexOf('subscribeRuntime(identity,'),
    reconnectSubscribe);
  check('subscription cut apply-state stamps the cut identity onto nested RpcSessionState',
    appSource.includes("stampSessionStateIncarnation(action.state, action.identity.runtimeIncarnation)"));
  const lifecycleApplyAt = appSource.indexOf('applyState(stampSessionStateIncarnation(target.state, identity.runtimeIncarnation), targetSession, false)');
  const lifecycleResidentAt = appSource.indexOf(
    'residentBySessionIdRef.current = { ...residentBySessionIdRef.current, [targetSession]: identity.runtimeIncarnation }',
  );
  check('lifecycle applyState stamps identity after installing the resident overlay',
    lifecycleApplyAt >= 0 && lifecycleResidentAt >= 0 && lifecycleResidentAt < lifecycleApplyAt,
    String({ lifecycleResidentAt, lifecycleApplyAt }));
}

// Reconnect warm-slot retirement: full stale cache → cap-1 replacements via real upsertViewSlot.
{
  const REPLACEMENTS = MAX_LOADED_SESSIONS - 1;
  const staleSecondary = (index: number): RuntimeIdentity =>
    ({ sessionId: `stale-${index}`, runtimeIncarnation: `stale-inc-${index}` });
  const currentSecondary = (index: number): RuntimeIdentity =>
    ({ sessionId: `current-${index}`, runtimeIncarnation: `current-inc-${index}` });

  const replacementRows = (): ReconnectCatalogRow[] => [
    { sessionId: 'new-primary', path: 'new-primary.jsonl', loaded: true, runtimeIncarnation: 'new-primary-inc' },
    ...Array.from({ length: REPLACEMENTS }, (_, index): ReconnectCatalogRow => ({
      sessionId: `current-${index + 1}`,
      path: `current-${index + 1}.jsonl`,
      loaded: true,
      runtimeIncarnation: `current-inc-${index + 1}`,
    })),
  ];

  const seedSlot = (
    slots: Map<RuntimeKey, SessionViewSlot>,
    identity: RuntimeIdentity,
    lastUsed: number,
    draft: string,
    displayed: RuntimeIdentity,
  ): Map<RuntimeKey, SessionViewSlot> =>
    upsertViewSlot(slots, { ...emptyViewSlot(identity, lastUsed), draft }, displayed, null);

  const dropInvalidated = (
    slots: Map<RuntimeKey, SessionViewSlot>,
    identities: readonly RuntimeIdentity[],
  ): Map<RuntimeKey, SessionViewSlot> => {
    const next = new Map(slots);
    for (const identity of identities) next.delete(runtimeKey(identity));
    return next;
  };

  const seedReconnectSlot = (
    app: AppSwitchHarness,
    slots: Map<RuntimeKey, SessionViewSlot>,
    identity: RuntimeIdentity,
  ): Map<RuntimeKey, SessionViewSlot> => {
    app.resident[identity.sessionId] = identity.runtimeIncarnation;
    app.subscriptions.push({ identity, cursor: 0 });
    const displayed = app.display?.displayed ?? identity;
    const next = upsertViewSlot(slots, emptyViewSlot(identity), displayed, app.display?.pending ?? null);
    if (next.has(runtimeKey(identity))) app.viewSlots.set(runtimeKey(identity), 'reconnect-background-view');
    return next;
  };

  const buildStaleCache = (): {
    app: AppSwitchHarness;
    oldPrimary: RuntimeIdentity;
    slots: Map<RuntimeKey, SessionViewSlot>;
  } => {
    const app = new AppSwitchHarness();
    app.viewSlots.delete(runtimeKey(A));
    app.viewSlots.delete(runtimeKey(B));
    const oldPrimary: RuntimeIdentity = { sessionId: 'old-primary', runtimeIncarnation: 'old-primary-inc' };
    app.display = { displayed: oldPrimary, commandTarget: oldPrimary, pending: null, nextGeneration: 1 };
    app.active = oldPrimary;
    app.resident[oldPrimary.sessionId] = oldPrimary.runtimeIncarnation;
    app.viewSlots.set(runtimeKey(oldPrimary), 'stale-displayed-view');
    let slots = seedSlot(new Map(), oldPrimary, 4242, 'stale-displayed-draft', oldPrimary);
    for (let index = 1; index <= REPLACEMENTS; index += 1) {
      const identity = staleSecondary(index);
      app.resident[identity.sessionId] = identity.runtimeIncarnation;
      app.viewSlots.set(runtimeKey(identity), `stale-view-${index}`);
      app.ready.set(identity.sessionId, { identity, cursor: 5, socketGeneration: app.socketGen - 1 });
      slots = seedSlot(slots, identity, 1000 + index, `stale-draft-${index}`, oldPrimary);
    }
    app.resident['new-primary'] = 'new-primary-inc';
    return { app, oldPrimary, slots };
  };

  const currentTarget = (index: number): SessionSwitchTarget => ({
    path: `current-${index}.jsonl`,
    catalogSource: 'rpi',
    catalogSessionId: `current-${index}`,
    catalogStatus: 'native',
    catalogRuntimeIncarnation: `current-inc-${index}`,
  });

  {
    const { app, oldPrimary, slots: staleSlots } = buildStaleCache();
    const roster = planReconnectRoster(replacementRows());
    check('replacement roster is complete for omission retirement',
      roster.omissionSafe && roster.authoritative.size === REPLACEMENTS + 1);
    app.retireResident(roster, 'new-primary');
    const slots = dropInvalidated(staleSlots, app.invalidations);
    check('retirement frees exactly the replaced stale identities',
      app.invalidations.length === REPLACEMENTS
      && app.invalidations.every((identity) => identity.sessionId.startsWith('stale-'))
      && !slots.has(runtimeKey(staleSecondary(1)))
      && app.ready.get('stale-1') === undefined
      && app.resident['stale-1'] === undefined);
    check('the currently displayed stale surface keeps its draft until a replacement takes over',
      slots.get(runtimeKey(oldPrimary))?.draft === 'stale-displayed-draft'
      && slots.get(runtimeKey(oldPrimary))?.lastUsed === 4242
      && app.resident['old-primary'] === 'old-primary-inc');
    check('the bound primary is never retired',
      app.resident['new-primary'] === 'new-primary-inc' && !app.invalidations.some((identity) => identity.sessionId === 'new-primary'));

    let seeded = slots;
    for (let index = 1; index <= REPLACEMENTS; index += 1) {
      seeded = seedReconnectSlot(app, seeded, currentSecondary(index));
    }
    check('every current runtime keeps a lastUsed=0 slot under the unchanged cap',
      seeded.size === MAX_LOADED_SESSIONS
      && Array.from({ length: REPLACEMENTS }, (_, index) => index + 1)
        .every((index) => seeded.get(runtimeKey(currentSecondary(index)))?.lastUsed === 0));

    const staleTarget: SessionSwitchTarget = {
      path: 'stale-1.jsonl',
      catalogSource: 'rpi',
      catalogSessionId: 'stale-1',
      catalogStatus: 'native',
      catalogRuntimeIncarnation: 'stale-inc-1',
    };
    app.queueResponse().reject(new Error('negotiated path taken'));
    const staleSwitch = app.request(staleTarget);
    await flush();
    check('retired stale incarnation cannot fast-switch',
      await staleSwitch === false && app.fastSwitches === 0 && app.sends.length === 1);

    app.acceptCut(cut(currentSecondary(1), 6));
    const warmSwitch = app.request(currentTarget(1));
    await flush();
    check('reconnect-seeded current runtime takes the warm fast path',
      await warmSwitch === true && app.fastSwitches === 1 && app.sends.length === 1);
  }

  {
    const { app, oldPrimary, slots: staleSlots } = buildStaleCache();
    const retained = [REPLACEMENTS - 4, REPLACEMENTS - 3, REPLACEMENTS - 2, REPLACEMENTS - 1, REPLACEMENTS];
    const rows: ReconnectCatalogRow[] = replacementRows().concat(retained.map((index): ReconnectCatalogRow => ({
      sessionId: `stale-${index}`,
      path: `stale-${index}.jsonl`,
      loaded: true,
      runtimeIncarnation: `stale-inc-${index}`,
    })));
    const roster = planReconnectRoster(rows);
    check('partial replacement roster stays complete', roster.omissionSafe && roster.authoritative.size === REPLACEMENTS + 1 + retained.length);
    app.retireResident(roster, 'new-primary');
    let slots = dropInvalidated(staleSlots, app.invalidations);
    check('only the replaced identities retire; retained identities keep drafts',
      app.invalidations.length === REPLACEMENTS - retained.length
      && retained.every((index) => slots.get(runtimeKey(staleSecondary(index)))?.draft === `stale-draft-${index}`
        && slots.get(runtimeKey(staleSecondary(index)))?.lastUsed === 1000 + index
        && app.resident[`stale-${index}`] === `stale-inc-${index}`)
      && slots.size === 1 + retained.length
      && slots.get(runtimeKey(oldPrimary))?.draft === 'stale-displayed-draft');
    for (let index = 1; index <= REPLACEMENTS - retained.length; index += 1) {
      slots = seedReconnectSlot(app, slots, currentSecondary(index));
    }
    check('partial replacement stays within the cap with no eviction',
      slots.size === MAX_LOADED_SESSIONS
      && retained.every((index) => slots.get(runtimeKey(staleSecondary(index)))?.draft === `stale-draft-${index}`));
  }

  {
    const { app, slots } = buildStaleCache();
    const malformed = planReconnectRoster([
      { sessionId: 'new-primary', path: 'new-primary.jsonl', loaded: true, runtimeIncarnation: 'new-primary-inc' },
      { sessionId: 'broken', path: 'broken.jsonl', loaded: true },
    ]);
    check('a malformed loaded row marks the roster unsafe for omission pruning',
      malformed.omissionSafe === false && malformed.authoritative.size === 1);
    app.retireResident(malformed, 'new-primary');
    check('malformed catalog preserves every last-good slot and draft',
      app.invalidations.length === 0
      && slots.size === MAX_LOADED_SESSIONS
      && slots.get(runtimeKey(staleSecondary(1)))?.draft === 'stale-draft-1');

    const failed = planReconnectRoster([]);
    check('an empty roster can never authorize omission pruning',
      failed.omissionSafe === false && failed.authoritative.size === 0);
    app.retireResident(failed, 'new-primary');
    check('failed-list state keeps the full stale cache untouched',
      app.invalidations.length === 0 && slots.size === MAX_LOADED_SESSIONS);
  }

  {
    const appSource = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../src/App.tsx'), 'utf8');
    const retireStart = appSource.indexOf('retireResident: ({ authoritative, omissionSafe }) => {');
    const retireEnd = appSource.indexOf('subscribeSession: (identity) => {', retireStart);
    const retireBody = retireStart >= 0 && retireEnd > retireStart ? appSource.slice(retireStart, retireEnd) : '';
    const switchStart = appSource.indexOf('switchSession: (target) => {');
    const switchEnd = appSource.indexOf('retireResident:', switchStart);
    const switchBody = switchStart >= 0 && switchEnd > switchStart ? appSource.slice(switchStart, switchEnd) : '';
    check('production retirement precedes every restore subscription and fails closed',
      retireStart >= 0 && retireEnd > retireStart
      && retireBody.includes('sessionId === primarySessionId')
      && retireBody.includes('sessionId === displayedSessionId')
      && retireBody.includes('authoritative.get(sessionId)')
      && retireBody.includes('if (authoritativeIncarnation === undefined && !omissionSafe) continue;')
      && retireBody.includes('invalidateRuntime({ sessionId, runtimeIncarnation: incarnation });'),
      retireBody);
    check('bootstrap switch uses buildRestoreSwitchTarget and returns the bound commandTarget id',
      switchBody.includes('buildRestoreSwitchTarget(target)')
      && switchBody.includes('if (!alive()) throw STALE_ABORT')
      && switchBody.includes('displayStateRef.current?.commandTarget.sessionId')
      && !switchBody.includes('catalogSessionId'),
      switchBody);
    check('bootstrap first get_state stays id-less while bootRef is set',
      appSource.includes('const implicitTarget = bootRef.current')
      && appSource.includes('? sessionIdRef.current')
      && appSource.includes(': (display?.commandTarget.sessionId ?? sessionIdRef.current)'),
      appSource.slice(appSource.indexOf('const implicitTarget ='), appSource.indexOf('const implicitTarget =') + 280));
    check('negotiated switch_session source is the bound sessionIdRef, not a stale commandTarget',
      appSource.includes('buildSwitchCommand(target, sessionIdRef.current || current.commandTarget.sessionId)'),
      appSource.slice(appSource.indexOf('buildSwitchCommand(target,'), appSource.indexOf('buildSwitchCommand(target,') + 90));
    const invalidateStart = appSource.indexOf('const invalidateRuntime = useCallback');
    const invalidateEnd = appSource.indexOf('const settleSwitchForTransportLoss', invalidateStart);
    const invalidateBody = invalidateStart >= 0 && invalidateEnd > invalidateStart
      ? appSource.slice(invalidateStart, invalidateEnd)
      : '';
    check('invalidateRuntime keeps last-good items for get_messages refill',
      invalidateBody.includes('loadedSessionIdsRef.current.delete(identity.sessionId)')
      && !invalidateBody.includes('updateItemsMirror'),
      invalidateBody.slice(0, 400));
    check('apply-messages reconciles Task children against job_list after the history merge',
      appSource.includes('case \'apply-messages\': {')
      && appSource.includes('reconcileTaskJobs(action.identity.sessionId)'),
      appSource.slice(appSource.indexOf("case 'apply-messages':"), appSource.indexOf("case 'apply-messages':") + 420));
    check('Task job_list reconcile is allowed during a pending switch and stays silent',
      appSource.includes('const reconcileTaskJobs = useCallback((sessionId: string) => {')
      && appSource.includes("void sendCommand({ type: 'job_list', sessionId }, undefined, { auxiliary: true, allowDuringSwitch: true })")
      && appSource.includes('applyJobSnapshot(items, jobs, cut)'),
      appSource.slice(appSource.indexOf('const reconcileTaskJobs'), appSource.indexOf('const reconcileTaskJobs') + 420));
  }
}

{
  const oldPrimary: RuntimeIdentity = { sessionId: 'primary', runtimeIncarnation: 'old-inc' };
  const newPrimary: RuntimeIdentity = { sessionId: 'primary', runtimeIncarnation: 'new-inc' };
  const savedOther: RuntimeIdentity = { sessionId: 'saved-other', runtimeIncarnation: 'saved-inc' };

  {
    const app = new AppSwitchHarness();
    app.display = { displayed: oldPrimary, commandTarget: oldPrimary, pending: null, nextGeneration: 4 };
    app.active = oldPrimary;
    app.resident[oldPrimary.sessionId] = oldPrimary.runtimeIncarnation;
    app.ready.set(oldPrimary.sessionId, { identity: oldPrimary, cursor: 9, socketGeneration: app.socketGen - 1 });
    app.viewSlots.set(runtimeKey(oldPrimary), 'stale-primary-view');
    app.cursors.set(runtimeKey(oldPrimary), 9);
    app.discoverPrimary(newPrimary, false);
    check('same-id primary reincarnation rebinds displayed and command authority',
      sameRuntime(app.display?.displayed ?? null, newPrimary)
      && sameRuntime(app.display?.commandTarget ?? null, newPrimary)
      && sameRuntime(app.active, newPrimary)
      && app.resident[newPrimary.sessionId] === newPrimary.runtimeIncarnation
      && app.cursors.get(runtimeKey(newPrimary)) === 0);
    check('same-id primary reincarnation retires the old exact runtime',
      app.invalidations.some((identity) => sameRuntime(identity, oldPrimary))
      && app.resident[oldPrimary.sessionId] === newPrimary.runtimeIncarnation
      && app.ready.get(oldPrimary.sessionId) === undefined
      && !app.viewSlots.has(runtimeKey(oldPrimary)));
    check('same-id primary reincarnation subscribes the new identity from cursor 0',
      app.subscriptions.some((entry) => sameRuntime(entry.identity, newPrimary) && entry.cursor === 0));
  }

  {
    const app = new AppSwitchHarness();
    app.display = { displayed: savedOther, commandTarget: savedOther, pending: null, nextGeneration: 2 };
    app.active = savedOther;
    app.resident[savedOther.sessionId] = savedOther.runtimeIncarnation;
    app.viewSlots.set(runtimeKey(savedOther), 'saved-other-view');
    app.discoverPrimary(newPrimary, false);
    check('a different displayed session stays bound across primary reincarnation',
      sameRuntime(app.display?.displayed ?? null, savedOther)
      && sameRuntime(app.display?.commandTarget ?? null, savedOther)
      && sameRuntime(app.active, savedOther)
      && app.invalidations.length === 0
      && app.viewSlots.get(runtimeKey(savedOther)) === 'saved-other-view'
      && app.resident[newPrimary.sessionId] === newPrimary.runtimeIncarnation);
  }

  {
    const app = new AppSwitchHarness();
    app.display = null;
    app.discoverPrimary(newPrimary, true);
    check('cold bootstrap installs display authority before a restore switch can run',
      sameRuntime(app.display?.displayed ?? null, newPrimary)
      && sameRuntime(app.display?.commandTarget ?? null, newPrimary)
      && sameRuntime(app.active, newPrimary)
      && app.resident[newPrimary.sessionId] === newPrimary.runtimeIncarnation);
  }

  {
    const app = new AppSwitchHarness();
    app.display = { displayed: savedOther, commandTarget: savedOther, pending: null, nextGeneration: 2 };
    app.cursors.set(runtimeKey(savedOther), 7);
    app.discoverPrimary(newPrimary, false);
    check('reconnect refill targets the displayed session, not the listener primary',
      app.messageRefills.join(',') === savedOther.sessionId);
    check('reconnect subscribes the displayed runtime at the stored cursor on the new generation',
      app.subscriptions.some((entry) => sameRuntime(entry.identity, savedOther) && entry.cursor === 7)
      && app.subscriptions.some((entry) => sameRuntime(entry.identity, newPrimary) && entry.cursor === 0));
  }

  {
    const lastGood: Item[] = [
      { kind: 'user', id: 'u-cache', text: 'cached prompt', optimistic: false },
      { kind: 'assistant', id: 'a-cache', status: 'final', blocks: [{ type: 'text', text: 'last-good answer' }] },
    ];
    const app = new AppSwitchHarness();
    app.display = { displayed: savedOther, commandTarget: savedOther, pending: null, nextGeneration: 2 };
    app.cursors.set(runtimeKey(savedOther), 42);
    app.transcriptItems[savedOther.sessionId] = lastGood;
    app.activeAssistant[savedOther.sessionId] = 'msg-server';
    app.discoverPrimary(newPrimary, false);
    check('reconnect paints cached last-good before refill and leaves the composer live',
      app.connState === 'on'
      && app.refillPending === true
      && app.transcriptItems[savedOther.sessionId] === lastGood
      && app.transcriptItems[savedOther.sessionId]?.[1]?.kind === 'assistant'
      && reconnectSubscribeCursor(42) === 42
      && reconnectSavedSwitchIsRedundant(
        savedOther.sessionId,
        newPrimary.sessionId,
        savedOther.sessionId,
        { loaded: true, runtimeIncarnation: savedOther.runtimeIncarnation },
      ) === true
      && app.subscriptions.some((entry) => sameRuntime(entry.identity, savedOther) && entry.cursor === 42)
      && app.subscriptions.some((entry) => sameRuntime(entry.identity, newPrimary) && entry.cursor === 0));
    const streamed = app.deliverRuntimeEvent(savedOther, 43, {
      type: 'message_start',
      itemId: 'msg-server',
    });
    check('live reconnect frames apply incrementally before the refill RPC lands',
      streamed === true
      && app.refillPending === true
      && app.transcriptItems[savedOther.sessionId]?.some((item) => item.kind === 'assistant' && item.id === 'msg-server') === true
      && app.transcriptItems[savedOther.sessionId]?.some((item) => item.kind === 'user' && item.id === 'u-cache') === true);
    const turn = app.applyReconnectRefill({
      isStreaming: true,
      history: lastGood,
    });
    check('authoritative refill merges without a 15s rollback',
      turn.kind === 'streaming'
      && turn.assistantId === 'msg-server'
      && app.refillPending === false
      && app.refillInFlight === false
      && app.display?.pending === null
      && app.switchToasts.length === 0);
  }

  {
    const app = new AppSwitchHarness();
    app.display = { displayed: savedOther, commandTarget: savedOther, pending: null, nextGeneration: 2 };
    app.activeAssistant[savedOther.sessionId] = 'msg-server';
    app.transcriptItems[savedOther.sessionId] = [
      { kind: 'user', id: 'u1', text: 'go', optimistic: false },
      { kind: 'assistant', id: 'msg-server', status: 'streaming', blocks: [{ type: 'text', text: 'partial' }] },
    ];
    app.discoverPrimary(newPrimary, false);
    const turn = app.applyReconnectRefill({
      isStreaming: true,
      history: [{ kind: 'user', id: 'u1', text: 'go', optimistic: false }],
    });
    check('mid-turn reconnect keeps the server assistant id',
      turn.kind === 'streaming' && turn.assistantId === 'msg-server'
      && app.streaming[savedOther.sessionId] === true);
    const attached = app.deliverRuntimeEvent(savedOther, 1, {
      type: 'message_update',
      itemId: 'msg-server',
      delta: ' more',
    });
    const assistant = app.transcriptItems[savedOther.sessionId]?.find(
      (item) => item.kind === 'assistant' && item.id === 'msg-server',
    );
    const text = assistant && assistant.kind === 'assistant' && assistant.blocks[0]?.type === 'text'
      ? assistant.blocks[0].text
      : '';
    check('post-refill delta on the displayed session attaches within one refill',
      attached && text === 'partial more',
      text);
  }

  {
    const lastGood: Item[] = [
      { kind: 'user', id: 'u-cache', text: 'cached prompt', optimistic: false },
      { kind: 'assistant', id: 'a-cache', status: 'final', blocks: [{ type: 'text', text: 'last-good answer' }] },
    ];
    const app = new AppSwitchHarness();
    app.display = { displayed: savedOther, commandTarget: savedOther, pending: null, nextGeneration: 2 };
    app.transcriptItems[savedOther.sessionId] = lastGood;
    app.discoverPrimary(newPrimary, false);
    app.streaming[savedOther.sessionId] = true;
    app.failReconnectRefill(new Error('messages response missing messages'), { isStreaming: false });
    check('refill failure keeps last-good transcript and toasts without minting an assistant',
      app.connState === 'on'
      && app.refillPending === false
      && app.transcriptItems[savedOther.sessionId] === lastGood
      && app.switchToasts[0] === 'reconnect refill failed: messages response missing messages'
      && app.activeAssistant[savedOther.sessionId] === undefined);
    check('refill failure reconciles Stop from a follow-up get_state',
      app.streaming[savedOther.sessionId] === false);
  }

  {
    const app = new AppSwitchHarness();
    app.display = { displayed: savedOther, commandTarget: savedOther, pending: null, nextGeneration: 2 };
    app.discoverPrimary(newPrimary, false);
    const reincarnated: RuntimeIdentity = {
      sessionId: savedOther.sessionId,
      runtimeIncarnation: 'saved-inc-new',
    };
    app.rebindDisplayed(reincarnated);
    check('displayed incarnation rebind activates the new runtime and drops the old surface',
      sameRuntime(app.active, reincarnated)
      && sameRuntime(app.display?.displayed ?? null, reincarnated)
      && app.invalidations.some((identity) => sameRuntime(identity, savedOther))
      && app.activations.some((entry) => sameRuntime(entry.identity, reincarnated) && entry.restoreView === true));
  }

  {
    const app = new AppSwitchHarness();
    app.display = { displayed: savedOther, commandTarget: savedOther, pending: null, nextGeneration: 2 };
    app.discoverPrimary(newPrimary, false);
    app.streaming[savedOther.sessionId] = true;
    const turn = app.applyReconnectRefill({
      isStreaming: false,
      history: [{ kind: 'user', id: 'u1', text: 'go', optimistic: false }],
      settled: [
        { kind: 'user', id: 'u1', text: 'go', optimistic: false },
        { kind: 'assistant', id: 'msg-final', status: 'final', blocks: [{ type: 'text', text: 'done while away' }] },
      ],
    });
    const assistant = app.transcriptItems[savedOther.sessionId]?.find(
      (item) => item.kind === 'assistant' && item.id === 'msg-final',
    );
    const text = assistant && assistant.kind === 'assistant' && assistant.blocks[0]?.type === 'text'
      ? assistant.blocks[0].text
      : '';
    check('turn that finished while away loads settled history and clears Thinking',
      turn.kind === 'settled'
      && app.streaming[savedOther.sessionId] === false
      && text === 'done while away',
      text);
  }

  {
    const app = new AppSwitchHarness();
    app.display = { displayed: oldPrimary, commandTarget: oldPrimary, pending: null, nextGeneration: 1 };
    app.discoverPrimary(newPrimary, true);
    check('cold-bind refill still follows the displayed session after rebind',
      app.messageRefills.join(',') === newPrimary.sessionId);
  }
}

{
  const oldB: RuntimeIdentity = { sessionId: B.sessionId, runtimeIncarnation: 'inc-old' };
  const newB: RuntimeIdentity = { sessionId: B.sessionId, runtimeIncarnation: 'inc-new' };
  const kept: Item[] = [{ kind: 'user', id: 'u-keep', text: 'keep-me', optimistic: false }];
  const app = new AppSwitchHarness();
  app.display = { displayed: oldB, commandTarget: oldB, pending: null, nextGeneration: 2 };
  app.active = oldB;
  app.resident[oldB.sessionId] = oldB.runtimeIncarnation;
  app.streaming[oldB.sessionId] = true;
  app.aborting[oldB.sessionId] = true;
  app.transcriptItems[oldB.sessionId] = kept;
  app.discoverPrimary(oldB, false);
  check('refill hold captures the exact runtime identity and socket generation',
    app.refillHold?.sessionId === oldB.sessionId
    && app.refillHold?.runtimeIncarnation === oldB.runtimeIncarnation
    && app.refillHold?.socketGeneration === app.socketGen);
  app.resident[oldB.sessionId] = newB.runtimeIncarnation;
  const messagesStep = app.continueDeferredRefill('get_messages');
  const recoveryStep = app.continueDeferredRefill('recovery', {
    isStreaming: false,
    runtimeIncarnation: oldB.runtimeIncarnation,
  });
  check('deferred refill after incarnation replacement rejects before mutation or follow-on RPC',
    messagesStep === 'reject'
    && recoveryStep === 'reject'
    && app.refillFollowOns.length === 0
    && app.refillMutations.length === 0
    && app.streaming[oldB.sessionId] === true
    && app.aborting[oldB.sessionId] === true
    && app.transcriptItems[oldB.sessionId] === kept);
  const idleRecovery = app.continueDeferredRefill('recovery', {
    isStreaming: false,
    runtimeIncarnation: oldB.runtimeIncarnation,
  });
  check('inc-old recovery after inc-new resident performs zero mutations',
    idleRecovery === 'reject'
    && app.refillMutations.length === 0
    && app.streaming[oldB.sessionId] === true
    && app.aborting[oldB.sessionId] === true
    && app.transcriptItems[oldB.sessionId] === kept);
}

{
  const oldB: RuntimeIdentity = { sessionId: B.sessionId, runtimeIncarnation: 'inc-old' };
  const newB: RuntimeIdentity = { sessionId: B.sessionId, runtimeIncarnation: 'inc-new' };
  const app = new AppSwitchHarness();
  app.display = { displayed: oldB, commandTarget: oldB, pending: null, nextGeneration: 2 };
  app.active = oldB;
  app.resident[oldB.sessionId] = oldB.runtimeIncarnation;
  app.now = 10;
  app.discoverPrimary(oldB, false);
  const staleHold = app.refillHold;
  const staleSlot = app.refillStartedSlot;
  check('stale refill captures a slot token before replacement',
    staleSlot === 1 && app.refillInFlight === true && app.lastCutProgressAt === 10);
  app.now = 99;
  app.resident[oldB.sessionId] = newB.runtimeIncarnation;
  app.discoverPrimary(newB, false);
  const replacementInFlight = app.refillInFlight;
  const replacementProgress = app.lastCutProgressAt;
  const outcome = app.completeDeferredRefillCatch(
    staleSlot,
    staleHold,
    new Error('messages response missing messages'),
  );
  check('delayed ordinary rejection after incarnation replacement is silent and does not recover',
    outcome === 'silent'
    && app.refillCatchEffects.length === 0
    && app.switchToasts.length === 0
    && !app.refillFollowOns.includes('get_state')
    && planReconnectRefillFailure({
      stillOwned: false,
      error: new Error('messages response missing messages'),
      staleAbort: { kind: 'stale' },
    }) === 'silent');
  check('stale refill finalizer does not reset the replacement bootstrap timeout bookkeeping',
    replacementInFlight === true
    && replacementProgress === 99
    && app.refillInFlight === true
    && app.lastCutProgressAt === 99
    && reconnectRefillFinallyApplies(staleSlot, app.refillSlot) === false
    && reconnectRefillFinallyApplies(app.refillStartedSlot, app.refillSlot) === true);
}

// Bootstrap origin CAN fast-switch when all gates pass (current socket
// generation, warm ready cache, resident overlay + view slot all agree).
{
  const app = new AppSwitchHarness();
  seedReady(app, B, 6);
  const switched = app.request(targetB, 'bootstrap');
  check('bootstrap origin takes the fast path when gates pass',
    await switched === true && sameRuntime(app.display.displayed, B)
      && app.sends.length === 0 && app.fastSwitches === 1);
}

// Bootstrap stale generation still negotiates: after a reconnect the socket
// generation gate invalidates the ready stamp, fail-closed.
{
  const app = new AppSwitchHarness();
  seedReady(app, B, 6);
  app.socketGen += 1;
  const response = app.queueResponse();
  const switched = app.request(targetB, 'bootstrap');
  check('bootstrap stale generation negotiates',
    app.sends.length === 1 && app.fastSwitches === 0);
  response.resolve(handoff(B, 6));
  await flush();
  app.acceptCut(cut(B, 6));
  check('bootstrap negotiated switch still completes through its own cut',
    await switched === true && sameRuntime(app.display.displayed, B));
}

// G3: a same-catalog pending transaction COALESCES instead of being
// false-settled by a fast supersede, even once the target becomes ready.
{
  const app = new AppSwitchHarness();
  const response = app.queueResponse();
  const first = app.request(targetB);
  seedReady(app, B, 6);
  const second = app.request(targetB);
  check('same-catalog pending coalesces onto the in-flight transaction',
    app.createdTransactions === 1 && app.sends.length === 1 && app.fastSwitches === 0
    && app.display.pending?.generation === 1);
  response.resolve(handoff(B, 6));
  await flush();
  app.acceptCut(cut(B, 6));
  check('coalesced callers both resolve true with one send and one subscription',
    await first === true && await second === true
    && app.resolverCalls.get(1)?.join(',') === 'true'
    && app.subscriptions.length === 1);
}

// G4: close_session invalidates ready/resident BEFORE the unload frame, so a
// click on the retired session negotiates (one RPC) instead of fast-binding.
{
  const app = new AppSwitchHarness();
  seedReady(app, B, 6);
  await app.request(targetB);
  app.closeSession(B.sessionId);
  app.queueResponse().reject(new Error('negotiated path taken'));
  const backToB = { ...targetB, catalogRuntimeIncarnation: null };
  await app.request(backToB);
  await flush();
  check('closed session never fast-binds its retired identity',
    app.ready.get(B.sessionId) === undefined && app.resident[B.sessionId] === undefined
    && app.sends.length === 1 && app.fastSwitches === 1);
}

// G5: a stale superseded cut for an already fast-bound identity is ignored;
// it must not replace the live transcript or resubscribe at zero.
{
  const app = new AppSwitchHarness();
  seedReady(app, B, 12);
  await app.request(targetB);
  const sendsBefore = app.sends.length;
  app.acceptCut(cut(B, 12));
  check('stale non-pending cut for a fast-bound identity is ignored',
    app.sends.length === sendsBefore && app.subscriptions.length === 0
    && app.ready.get(B.sessionId)?.cursor === 12
    && sameRuntime(app.display.displayed, B));

  // Review P1: the LIVE event cursor decides, not the ready stamp. Ordinary
  // background events advance the exact runtime cursor past the ready
  // stamp; an intermediate cut between ready.cursor and the live cursor is
  // STILL ignored (watermark <= liveCursor), never clobbering the transcript.
  {
    const app = new AppSwitchHarness();
    seedReady(app, B, 12);
    await app.request(targetB);
    app.backgroundEvent(B, 13);
    app.backgroundEvent(B, 14);
    app.backgroundEvent(B, 15);
    app.acceptCut(cut(B, 14));
    check('intermediate cut between ready stamp and live cursor is ignored',
      app.ready.get(B.sessionId)?.cursor === 12
      && app.cursors.get(runtimeKey(B)) === 15
      && app.subscriptions.length === 0);
    // A cut ABOVE the live cursor is fresh data: applied, mark-ready re-stamps.
    app.acceptCut(cut(B, 16));
    check('fresh cut above the live cursor applies and restamps ready',
      app.ready.get(B.sessionId)?.cursor === 16
      && app.ready.get(B.sessionId)?.socketGeneration === app.socketGen
      && app.cursors.get(runtimeKey(B)) === 16);
  }

  // A PRIOR-generation ready entry never suppresses: after reconnect, a
  // lower-or-equal cut for the same identity is APPLIED and re-stamps the
  // current socket generation.
  {
    const app = new AppSwitchHarness();
    seedReady(app, B, 12);
    app.close();
    app.acceptCut(cut(B, 10));
    check('prior-generation ready never suppresses a lower-or-equal cut',
      app.ready.get(B.sessionId)?.cursor === 10
      && app.ready.get(B.sessionId)?.socketGeneration === app.socketGen
      && app.cursors.get(runtimeKey(B)) === 10);
  }
}

// ---------------------------------------------------------------------------
// O(1) anchor freshness under scrollbar jumps: a first→last jump is ONE
// scroll event and must re-resolve the anchor through the production probe
// with the SAME constant number of DOM operations as a one-pixel nudge —
// never a sibling walk or children scan whose cost scales with length — and
// the jumped state is exactly what the next fast save reads (production
// integration, not a zero-save-only mirror).
{
  const targetA: SessionSwitchTarget = {
    path: 'source.jsonl',
    catalogSource: 'rpi',
    catalogSessionId: A.sessionId,
    catalogStatus: 'native',
    catalogRuntimeIncarnation: A.runtimeIncarnation,
  };
  const app = new AppSwitchHarness();
  seedReady(app, B, 7);
  seedReady(app, A, 7);
  const jumpIds = Array.from({ length: JUMP_ITEM_COUNT }, (_, index) => `j${index + 1}`);
  app.mountTranscript(A.sessionId, jumpIds);
  app.saveDisplayedViewMirror(A, true);
  const opsBeforeJump = { reads: app.geometryReads, hits: app.hitTests };
  const jumpScrollTop = (JUMP_ITEM_COUNT - 4) * TRANSCRIPT_ITEM_HEIGHT;
  app.noteScroll(A.sessionId, jumpScrollTop);
  const jumpReads = app.geometryReads - opsBeforeJump.reads;
  const jumpHits = app.hitTests - opsBeforeJump.hits;
  check('first→last jump costs the same constant operations as a nudge',
    jumpHits === 3 && jumpReads === 1,
    JSON.stringify({ jumpHits, jumpReads, items: JUMP_ITEM_COUNT }));
  check('first→last jump resolves the first visible item exactly',
    app.liveViewport?.viewport.anchorItemId === `j${JUMP_ITEM_COUNT - 3}`
      && app.liveViewport.viewport.anchorOffsetPx === 0
      && app.liveViewport.viewport.follow === false,
    JSON.stringify(app.liveViewport));
  // The jumped state is what the next fast save reads (production
  // integration, not a zero-save-only mirror).
  await app.request(targetB);
  check('fast save after the jump persists the jumped-to anchor',
    app.viewScrolls.get(runtimeKey(A))?.anchorItemId === `j${JUMP_ITEM_COUNT - 3}`);
  await flush();
  await app.request(targetA);
  check('fast return after the jump restores the jumped scroll position',
    app.scrollTops.get(A.sessionId) === jumpScrollTop
      && app.liveViewport?.viewport.anchorItemId === `j${JUMP_ITEM_COUNT - 3}`);
  // A second jump — straight back to the top — pays the SAME constant
  // operation count: O(1) per event, not O(distance).
  const opsBeforeReturnJump = { reads: app.geometryReads, hits: app.hitTests };
  app.noteScroll(A.sessionId, 0);
  check('last→first jump also pays the same constant operations',
    app.geometryReads - opsBeforeReturnJump.reads === 1 && app.hitTests - opsBeforeReturnJump.hits === 3);
  check('second jump resolves the first item exactly',
    app.liveViewport?.viewport.anchorItemId === 'j1' && app.liveViewport.viewport.anchorOffsetPx === 0);
}

// ---------------------------------------------------------------------------
// O(1) live transcript viewport (App fast switch): the save reads the
// incrementally maintained anchor/follow/fold state through the REAL
// transcriptView/scrollPin modules against the fake transcript DOM — the
// LATEST user state survives a fast A→B→A roundtrip, and the fast save pays
// zero DOM geometry reads.
{
  const targetA: SessionSwitchTarget = {
    path: 'source.jsonl',
    catalogSource: 'rpi',
    catalogSessionId: A.sessionId,
    catalogStatus: 'native',
    catalogRuntimeIncarnation: A.runtimeIncarnation,
  };
  const app = new AppSwitchHarness();
  seedReady(app, B, 7);
  // A stays exact-ready on this socket generation from its boot cut, so both
  // legs of the roundtrip take the frontend-only fast path.
  seedReady(app, A, 7);
  app.mountTranscript(A.sessionId, ['m1', 'm2', 'm3', 'm4', 'm5', 'm6', 'm7', 'm8']);
  // Boot-time seed: the initial save scans once and seeds the live viewport,
  // mirroring App's activation-time live-viewport initialization.
  app.saveDisplayedViewMirror(A, true);
  // The user scrolls A away from the bottom and toggles folds; every
  // mutation lands in the live viewport through the production note paths.
  app.noteScroll(A.sessionId, 210);
  app.toggleFold(A.sessionId, 1, 0, true);
  app.noteScroll(A.sessionId, 310);
  app.toggleFold(A.sessionId, 5, 0, true);
  const live = app.liveViewport?.viewport;
  check('scroll notes keep the live anchor/follow current',
    live?.anchorItemId === 'm4' && live.follow === false
      && live.anchorOffsetPx === -10 && live.fallbackScrollTop === 310,
    JSON.stringify(app.liveViewport));
  check('fold toggles add exactly one live key per open fold',
    live?.openFoldKeys.size === 2
      && live?.openFoldKeys.has('m2:0:tool-card__raw')
      && live?.openFoldKeys.has('m6:0:tool-card__raw'),
    JSON.stringify([...(live?.openFoldKeys ?? [])]));


  // A small-scroll roundtrip still restores the LATEST user state through
  // the fast path with zero save-time DOM reads.
  app.noteScroll(A.sessionId, 210);
  app.toggleFold(A.sessionId, 1, 0, true);
  app.noteScroll(A.sessionId, 310);
  app.toggleFold(A.sessionId, 5, 0, true);
  const readsBeforeSave = app.geometryReads;
  await app.request(targetB);
  check('fast switch save performs zero anchor geometry reads',
    app.geometryReads === readsBeforeSave, JSON.stringify({ readsBeforeSave }));
  check('fast switch activated B without subscribing', sameRuntime(app.active, B)
    && app.sends.length === 0 && app.subscriptions.length === 0);
  await flush();
  await app.request(targetA);
  check('fast return restores the latest scroll position',
    app.scrollTops.get(A.sessionId) === 310,
    `scrollTop=${app.scrollTops.get(A.sessionId)}`);
  check('fast return restores the latest anchor and fold state',
    app.liveViewport?.viewport.anchorItemId === 'm4'
      && app.liveViewport.viewport.follow === false
      && app.liveViewport.viewport.openFoldKeys.has('m2:0:tool-card__raw')
      && app.liveViewport.viewport.openFoldKeys.has('m6:0:tool-card__raw'));
  const foldsAfter = (app.transcripts.get(A.sessionId)?.items ?? []).map((child) => child.folds[0].open);
  check('restored folds re-open exactly the toggled items in the fake DOM',
    foldsAfter.join(',') === 'false,true,false,false,false,true,false,false',
    JSON.stringify(foldsAfter));
  // Remaining 60 after fold growth is a scroll-away under PIN_TOLERANCE_PX=1.
  app.noteScroll(A.sessionId, 500);
  check('post-restore scroll re-derives the anchor from the restored state',
    app.liveViewport?.viewport.follow === false
      && app.liveViewport.viewport.anchorItemId === 'm6'
      && app.liveViewport.viewport.anchorOffsetPx === 0
      && app.scrollTops.get(A.sessionId) === 500);
}

// Lifecycle cut view isolation: the source view (draft + attachments) is
// saved BEFORE the cut; a New/Clone target composer starts EMPTY; a Fork's
// editorText rides ONLY in the child; and a fast roundtrip restores the
// preserved source view exactly.
{
  const app = new AppSwitchHarness();
  app.composerDraft = 'source draft must survive';
  app.attachments = [
    { id: 'a1', name: 'one.png', size: 2, mimeType: 'image/png', kind: 'image', dataBase64: 'AA==' },
    { id: 'a2', name: 'b.png', size: 2, mimeType: 'image/png', kind: 'image', dataBase64: 'AQ==' },
  ];
  // New: the child composer/attachments start EMPTY; the source view slot
  // keeps the draft + attachments exactly.
  await app.lifecycleResult(lifecycleData({ sessionId: 'new-child', runtimeIncarnation: 'new-child-inc' }));
  await flush();
  check('New cut empties the child composer', app.composerDraft === '');
  check('New cut empties the child attachments', app.attachments.length === 0);
  const sourceSlot = app.viewScrolls.get(runtimeKey(A));
  check('source draft preserved in its own slot',
    sourceSlot?.draft === 'source draft must survive' && sourceSlot?.attachments.length === 2,
    JSON.stringify(sourceSlot));
  check('fresh target slot has no scroll or fold state',
    app.viewScrolls.get(runtimeKey({ sessionId: 'new-child', runtimeIncarnation: 'new-child-inc' }))
      ?.anchorItemId === null
      && (app.viewScrolls.get(runtimeKey({ sessionId: 'new-child', runtimeIncarnation: 'new-child-inc' }))
        ?.openFoldKeys.size ?? -1) === 0);

  // Fork from the new-child session: the editorText rides ONLY in the child;
  // the new-child draft stays in its own slot.
  app.composerDraft = 'newB draft must not leak';
  await app.lifecycleResult(lifecycleData(
    { sessionId: 'fork-child', runtimeIncarnation: 'fork-child-inc' },
    { editorText: 'PONG fork draft exact' },
  ));
  await flush();
  check('fork child composer is exactly the editorText',
    app.composerDraft === 'PONG fork draft exact');
  check('fork child carries no attachments',
    app.attachments.length === 0);
  const newChildSlot = app.viewScrolls.get(runtimeKey({ sessionId: 'new-child', runtimeIncarnation: 'new-child-inc' }));
  check('new-child draft preserved in its slot across the fork cut',
    newChildSlot?.draft === 'newB draft must not leak' && newChildSlot?.attachments.length === 0,
    JSON.stringify(newChildSlot));

  // Fast roundtrip back to the new-child restores its EXACT saved view.
  const newChild: RuntimeIdentity = { sessionId: 'new-child', runtimeIncarnation: 'new-child-inc' };
  seedReady(app, newChild, 3);
  app.viewSlots.set(runtimeKey(newChild), 'new-child-view');
  await app.request({
    path: 'new-child.jsonl',
    catalogSource: 'rpi',
    catalogSessionId: 'new-child',
    catalogStatus: 'native',
    catalogRuntimeIncarnation: 'new-child-inc',
  });
  check('fast switch back restores the preserved draft and attachment state',
    app.composerDraft === 'newB draft must not leak' && app.attachments.length === 0,
    JSON.stringify({ draft: app.composerDraft, attachments: app.attachments.length }));
}

// ---------------------------------------------------------------------------
// O(1) anchor x×y probe grid: a narrow RIGHT-ALIGNED user bubble overlapping
// the viewport top must win the anchor over the wider LATER item that only
// the center column can hit — the first y row with a stamped hit decides,
// and its earliest valid child wins. Still a constant grid per event.
{
  const app = new AppSwitchHarness();
  seedReady(app, A, 7);
  // u2 is a narrow right-aligned user bubble (100px at the content box's
  // right edge); every other child is full width. The production probe
  // columns (content rect x ∈ [20, 780] → probes 44 / 400 / 756) make the
  // center column structurally blind to u2 — the pre-x-probe probe missed
  // it entirely and anchored on the LATER center item instead.
  app.mountTranscript(A.sessionId, ['u1', 'u2', 'u3', 'u4'], ['u2']);
  app.saveDisplayedViewMirror(A, true);
  const bubbleScrollTop = 2 * TRANSCRIPT_ITEM_HEIGHT - 35;
  app.noteScroll(A.sessionId, bubbleScrollTop);
  const live = app.liveViewport?.viewport;
  check('narrow right-aligned bubble wins over the later center item',
    live?.anchorItemId === 'u2' && live.anchorOffsetPx === -65,
    JSON.stringify(app.liveViewport));
  // The bubble anchor is exactly what a negotiated full scan derives.
  app.saveDisplayedViewMirror(A, true);
  check('scan anchor agrees with the probe anchor for the bubble viewport',
    app.viewScrolls.get(runtimeKey(A))?.anchorItemId === 'u2');
  // Bounded ops: the deciding row pays exactly 3 columns × 1 hit-test each
  // and one rect read for the single distinct candidate.
  check('bubble anchor resolution stays on the constant probe grid',
    app.hitTests === 3,
    String(app.hitTests));
}

// ---------------------------------------------------------------------------
// Follow-mode restore order: saved folds re-open BEFORE forcePin, so the
// restored view pins to the POST-fold bottom — the re-opened folds grow
// scrollHeight, and the final scrollTop must equal that new bottom.
{
  const app = new AppSwitchHarness();
  seedReady(app, B, 7);
  seedReady(app, A, 7);
  app.mountTranscript(A.sessionId, ['p1', 'p2', 'p3', 'p4', 'p5', 'p6', 'p7', 'p8']);
  // The user sits at the bottom with two folds open, then saves that state.
  app.saveDisplayedViewMirror(A, true);
  app.toggleFold(A.sessionId, 1, 0, true);
  app.toggleFold(A.sessionId, 4, 0, true);
  const scroll = app.transcripts.get(A.sessionId)!.scroll;
  scroll.scrollTop = scroll.scrollHeight; // pinned pre-fold bottom (clamped)
  app.noteScroll(A.sessionId, scroll.scrollTop);
  app.saveDisplayedViewMirror(A, false);
  const preFoldBottom = 8 * TRANSCRIPT_ITEM_HEIGHT - TRANSCRIPT_VIEWPORT_HEIGHT;
  // Simulate the switch-away DOM: the displayed transcript's folds close
  // again (the saved slot already captured the open keys).
  app.toggleFold(A.sessionId, 1, 0, false);
  app.toggleFold(A.sessionId, 4, 0, false);
  scroll.scrollTop = 0;
  // Restore: folds re-open first, THEN the follow pin lands on the new bottom.
  app.restoreDisplayedViewMirror(A);
  const postFoldHeight = app.transcripts.get(A.sessionId)!.height();
  check('follow restore re-opens exactly the saved folds before pinning',
    app.transcripts.get(A.sessionId)!.items.map((child) => child.folds[0].open).join(',')
      === 'false,true,false,false,true,false,false,false',
    JSON.stringify(app.transcripts.get(A.sessionId)!.items.map((child) => child.folds[0].open)));
  check('follow restore pins to the bottom AFTER folds grow scrollHeight',
    postFoldHeight === 8 * TRANSCRIPT_ITEM_HEIGHT + 2 * FOLD_OPEN_HEIGHT_PX
      && app.scrollTops.get(A.sessionId) === postFoldHeight - TRANSCRIPT_VIEWPORT_HEIGHT,
    `scrollTop=${app.scrollTops.get(A.sessionId)} bottom=${postFoldHeight - TRANSCRIPT_VIEWPORT_HEIGHT}`);
  check('pinning before the fold re-open would have stopped short of the new bottom',
    postFoldHeight - TRANSCRIPT_VIEWPORT_HEIGHT === preFoldBottom + 2 * FOLD_OPEN_HEIGHT_PX
      && preFoldBottom !== postFoldHeight - TRANSCRIPT_VIEWPORT_HEIGHT);
}

// ---------------------------------------------------------------------------
// Production stamp coverage: EVERY transcript item root and fold stamps
// through the ONE exported production API (transcriptItemStamp /
// transcriptFoldStamp in ./transcriptView) — no raw data-item-id /
// data-fold-key JSX survives anywhere in App.tsx. The exported components
// render through the API (registry exact + unique), and the App.tsx source
// scan locks every inline App-body callsite onto the API, so this block
// fails if App omits the stamping API or reverts any callsite.
{
  const appSource = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../src/App.tsx'), 'utf8');
  check('App stamps no item root outside the production API', !appSource.includes('data-item-id='));
  check('App stamps no fold outside the production API',
    !appSource.includes('data-fold-key=') && !appSource.includes('foldKey('));
  check('every App item-root callsite routes through transcriptItemStamp',
    appSource.split('transcriptItemStamp(').length - 1 === 10,
    String(appSource.split('transcriptItemStamp(').length - 1));
  check('every App fold callsite routes through transcriptFoldStamp',
    appSource.split('transcriptFoldStamp(').length - 1 === 7,
    String(appSource.split('transcriptFoldStamp(').length - 1));

  resetTranscriptStamps();
  const streaming = renderToStaticMarkup(createElement(StreamingAssistant, { sid: 'sess', id: 'live-1', workspaceCwd: '/w' }));
  const final = renderToStaticMarkup(createElement(FinalAssistant, {
    blocks: [{ type: 'text', text: 'done' }],
    sessionId: 'sess',
    workspaceCwd: '/w',
    itemId: 'final-1',
  }));
  const bash = renderToStaticMarkup(createElement(BashCard, { command: 'ls', output: 'file', status: 'done', itemId: 'bash-1' }));
  const tool = renderToStaticMarkup(createElement(ToolCard, {
    item: { kind: 'toolCard', id: 'tool-1', toolCallId: 'call-1', toolName: 'grep', args: { pattern: 'x' }, status: 'done', result: 'match' },
  }));
  check('StreamingAssistant renders the API-stamped item root and thinking fold',
    streaming.includes('data-item-id="live-1"') && streaming.includes('data-fold-key="live-1:0:thinking"'),
    streaming);
  check('FinalAssistant renders the API-stamped item root',
    final.includes('data-item-id="final-1"'), final);
  const thinkingFinal = renderToStaticMarkup(createElement(FinalAssistant, {
    blocks: [
      { type: 'thinking', thinking: 'chain of thought here' },
      { type: 'text', text: 'visible answer' },
    ],
    sessionId: 'sess',
    workspaceCwd: '/w',
  }));
  check('FinalAssistant wraps thinking in details.thinking quote container',
    thinkingFinal.includes('class="thinking"')
      && thinkingFinal.includes('class="thinking__summary"')
      && thinkingFinal.includes('class="thinking__body"')
      && thinkingFinal.includes('<details')
      && thinkingFinal.includes('chain of thought here')
      && thinkingFinal.includes('visible answer')
      && thinkingFinal.includes('Thinking'),
    thinkingFinal);
  check('FinalAssistant thinking is not bare assistant text',
    thinkingFinal.includes('thinking__body')
      && !thinkingFinal.includes('class="assistant-text">chain of thought here'),
    thinkingFinal);
  const reversedFinal = renderToStaticMarkup(createElement(FinalAssistant, {
    blocks: [
      { type: 'text', text: 'visible answer' },
      { type: 'thinking', thinking: 'chain of thought here' },
    ],
    sessionId: 'sess',
    workspaceCwd: '/w',
  }));
  check('FinalAssistant [text, thinking] paints thinking before answer',
    reversedFinal.indexOf('class="thinking"') < reversedFinal.indexOf('visible answer')
      && reversedFinal.indexOf('chain of thought here') < reversedFinal.indexOf('visible answer'),
    reversedFinal);
  check('BashCard renders the API-stamped item root',
    bash.includes('data-item-id="bash-1"'), bash);
  check('ToolCard renders the exact tool-card__raw fold key',
    tool.includes('data-fold-key="tool-1:0:tool-card__raw"'), tool);
  const expected = new Set([
    'data-item-id=live-1',
    'data-fold-key=live-1:0:thinking',
    'data-item-id=final-1',
    'data-item-id=bash-1',
    'data-fold-key=tool-1:0:tool-card__raw',
  ]);
  check('production registry holds exactly the unique stamped item/fold keys',
    stampedTranscriptKeys().size === expected.size
      && [...expected].every((key) => stampedTranscriptKeys().has(key)),
    JSON.stringify([...stampedTranscriptKeys()]));
}

// Bounded demand window: O(CHUNK) DOM per surface at every step, O(1) math.
{
  const total = 1500;
  const tail = projectTranscriptWindow(total, tailProjectionStart(total));
  check('cold tail projection is bounded to the chunk, not the full list',
    tail.end - tail.start === TRANSCRIPT_PROJECTION_CHUNK
      && tail.end === total
      && !tail.complete,
    JSON.stringify(tail));
  check('cold tail projection hides the older prefix',
    tail.start === total - TRANSCRIPT_PROJECTION_CHUNK && tail.hiddenBefore === tail.start);

  const full = projectTranscriptWindow(total, 0);
  check('a start of zero covering the whole list is complete with zero hidden items',
    total <= TRANSCRIPT_PROJECTION_CHUNK ? (full.complete && full.end === total) : !full.complete);
  const small = projectTranscriptWindow(5, 0);
  check('a list smaller than the chunk projects complete (no padding)',
    small.complete && small.start === 0 && small.end === 5);
  const degenerate = projectTranscriptWindow(0, 7);
  check('an empty list projects an empty complete window',
    degenerate.complete && degenerate.start === 0 && degenerate.end === 0);
  const drift = projectTranscriptWindow(total, total + 999);
  check('a stored start past the data clamps instead of growing the DOM',
    drift.start === total && drift.end === total);

  let start = tailProjectionStart(total);
  const seenSizes: number[] = [];
  const seenStarts: number[] = [];
  while (start > 0) {
    start = shiftProjectionWindowOlder(total, start);
    const window = projectTranscriptWindow(total, start);
    seenSizes.push(window.end - window.start);
    seenStarts.push(start);
  }
  check('older shifts keep the window at the chunk bound and land exactly on the head',
    seenSizes.every((size) => size === TRANSCRIPT_PROJECTION_CHUNK)
      && seenStarts[seenStarts.length - 1] === 0,
    JSON.stringify(seenStarts));
  check('older shifts cover every index from the head to the tail start',
    Array.from({ length: tailProjectionStart(total) }, (_, index) => index)
      .every((index) => seenStarts.some((candidate, order) => {
        const window = projectTranscriptWindow(total, candidate);
        const previousStart = order === 0 ? tailProjectionStart(total) : seenStarts[order - 1];
        return index >= candidate && index < Math.max(window.end, previousStart);
      })));
  let back = 0;
  const newerStarts: number[] = [];
  while (projectTranscriptWindow(total, back).end < total) {
    back = shiftProjectionWindowNewer(total, back);
    const window = projectTranscriptWindow(total, back);
    seenSizes.push(window.end - window.start);
    newerStarts.push(back);
  }
  check('newer shifts walk back to the exact tail within the bound',
    projectTranscriptWindow(total, back).end === total
      && seenSizes.every((size) => size <= TRANSCRIPT_PROJECTION_CHUNK && size > 0),
    JSON.stringify(newerStarts));
  check('shifts at the edges are no-ops',
    shiftProjectionWindowOlder(total, 0) === 0
      && shiftProjectionWindowNewer(total, tailProjectionStart(total)) === tailProjectionStart(total));
}

// Exact anchor: every demand shift keeps the node App measures rendered in
// BOTH windows — the older commit measures children[prepended] (the old
// window's head item), the newer pre-commit measurement reads
// children[next - start] (the new window's head item) — so scroll
// compensation never sees a missing anchor at either edge.
{
  const total = 1500;
  let start = tailProjectionStart(total);
  let olderAnchorRetained = true;
  while (start > 0) {
    const next = shiftProjectionWindowOlder(total, start);
    olderAnchorRetained = olderAnchorRetained
      && projectTranscriptWindow(total, next).end > start
      && start - next < projectTranscriptWindow(total, next).end - next;
    start = next;
  }
  check('every older shift retains the old window head item the commit measures',
    olderAnchorRetained && start === 0);
  let back = 0;
  let newerAnchorRetained = true;
  while (projectTranscriptWindow(total, back).end < total) {
    const next = shiftProjectionWindowNewer(total, back);
    newerAnchorRetained = newerAnchorRetained
      && next < projectTranscriptWindow(total, back).end
      && next - back < TRANSCRIPT_PROJECTION_CHUNK;
    back = next;
  }
  check('every newer shift retains the new window head item the pre-commit measure reads',
    newerAnchorRetained && projectTranscriptWindow(total, back).end === total);
  const parkedTail = total - 1;
  const regrown = shiftProjectionWindowOlder(total, parkedTail);
  check('a one-item parked window regrows with its sole item retained',
    projectTranscriptWindow(total, regrown).end > parkedTail
      && parkedTail - regrown < projectTranscriptWindow(total, regrown).end - regrown,
    String(regrown));
}

{
  const total = 1500;
  const mid = projectionStartForAnchor(total, 800);
  check('a mid anchor is centered inside one bounded chunk',
    mid === 800 - (TRANSCRIPT_PROJECTION_CHUNK >> 1)
      && projectTranscriptWindow(total, mid).end - mid === TRANSCRIPT_PROJECTION_CHUNK,
    String(mid));
  const head = projectionStartForAnchor(total, 10);
  check('a near-head anchor clamps at the head window',
    head === 0 && projectTranscriptWindow(total, 0).start === 0);
  const nearTail = projectionStartForAnchor(total, total - 1);
  check('a near-tail anchor covers through the tail',
    projectTranscriptWindow(total, nearTail).end === total);
  check('a missing anchor index fail-closes to the tail chunk',
    projectionStartForAnchor(total, Number.NaN) === tailProjectionStart(total));
}
{
  const total = 1500;
  // Parked anchor BELOW the tail chunk: no single contiguous chunk-sized
  // window spans the saved reading position AND the session tail, so the
  // retained projection switches to bounded DISJOINT slices — the anchor
  // neighborhood plus the tail — that together never exceed one chunk of
  // mounted items and are disjoint by construction at this distance.
  const slices = projectRetainedSlices(total, 800);
  check('a below-tail parked anchor slice contains the anchor and stays half-chunk bounded',
    slices.anchor.start <= 800 && 800 < slices.anchor.end
      && slices.anchor.end - slices.anchor.start === TRANSCRIPT_PROJECTION_CHUNK >> 1
      && slices.anchor.start === 800 - (TRANSCRIPT_PROJECTION_CHUNK >> 2),
    JSON.stringify(slices.anchor));
  check('the retained tail slice reaches the exact session tail and stays half-chunk bounded',
    slices.tail.start === total - (TRANSCRIPT_PROJECTION_CHUNK >> 1)
      && slices.tail.end === total && slices.tail.start - slices.anchor.end >= 0
      && slices.tail.end - slices.tail.start === TRANSCRIPT_PROJECTION_CHUNK >> 1,
    JSON.stringify(slices.tail));
  check('both slices stay disjoint and within the single chunk DOM bound',
    slices.anchor.end <= slices.tail.start
      && (slices.anchor.end - slices.anchor.start) + (slices.tail.end - slices.tail.start) <= TRANSCRIPT_PROJECTION_CHUNK);
  const headSlices = projectRetainedSlices(total, 0);
  check('a head-anchored parked view clamps the anchor slice at the head window',
    headSlices.anchor.start === 0 && headSlices.anchor.end === TRANSCRIPT_PROJECTION_CHUNK >> 1
      && headSlices.tail.start === total - (TRANSCRIPT_PROJECTION_CHUNK >> 1));
  const tiny = projectRetainedSlices(150, 80);
  check('a small list keeps the anchor covered and reaches the tail within the chunk bound',
    tiny.anchor.start <= 80 && 80 < tiny.anchor.end
      && tiny.anchor.end <= tiny.tail.start && tiny.tail.end === 150
      && (tiny.anchor.end - tiny.anchor.start) + (tiny.tail.end - tiny.tail.start) <= TRANSCRIPT_PROJECTION_CHUNK,
    JSON.stringify(tiny));
}

/* Parked plan: a below-tail anchor yields disjoint anchor+tail slices
 * ALWAYS — the planner takes no stored window at all, so a pre-existing
 * anchor-centered or canonical-head stored window can never suppress the
 * tail ownership marker; a tail-chunk anchor rides the single canonical
 * tail window. The demand walk's ownership of the window is enforced by the
 * walk latch, not by the planner. */
{
  const total = 1500;
  const belowTail = planParkedWindow(total, 800);
  check('a below-tail parked anchor plans disjoint slices containing both anchor and tail unconditionally',
    belowTail.kind === 'slices'
      && belowTail.anchor.start <= 800 && 800 < belowTail.anchor.end
      && belowTail.tail.end === total
      && belowTail.anchor.end <= belowTail.tail.start
      && (belowTail.anchor.end - belowTail.anchor.start) + (belowTail.tail.end - belowTail.tail.start) <= TRANSCRIPT_PROJECTION_CHUNK,
    JSON.stringify(belowTail));
  const headParked = planParkedWindow(total, 5);
  check('a head-anchored parked view plans anchor+tail slices even though a prior canonical head window would have covered it',
    headParked.kind === 'slices' && headParked.anchor.start === 0 && headParked.tail.end === total,
    JSON.stringify(headParked));
  const tailChunk = planParkedWindow(total, 1300);
  check('a tail-chunk parked anchor plans the single canonical tail window',
    tailChunk.kind === 'tail' && tailChunk.start === total - TRANSCRIPT_PROJECTION_CHUNK
      && tailChunk.start <= 1300 && projectTranscriptWindow(total, tailChunk.start).end === total,
    JSON.stringify(tailChunk));
}
{
  const total = 1500;
  check('a following surface re-anchors to the tail chunk as data grows',
    normalizeTranscriptWindow(total + 1, tailProjectionStart(total), true) === tailProjectionStart(total + 1));
  check('a parked surface keeps its start while data grows',
    normalizeTranscriptWindow(total + 1, 500, false) === 500);
  check('a parked surface clamps into shrunken data instead of emptying the window',
    normalizeTranscriptWindow(600, 900, false) === 599);
  check('an empty list normalizes to zero',
    normalizeTranscriptWindow(0, 500, true) === 0 && normalizeTranscriptWindow(0, 500, false) === 0);
}


// lifecycleReadyGate: an install-cursor EMPTY commit cannot ready a later cut.
{
  const displayed: RuntimeIdentity = { sessionId: 'sess-b', runtimeIncarnation: 'inc-2' };
  const source: RuntimeIdentity = { sessionId: 'sess-a', runtimeIncarnation: 'inc-1' };
  const ready: ReadyRuntime = { identity: displayed, cursor: 1500, socketGeneration: 3 };
  const base = {
    displayed,
    commandTarget: displayed,
    pending: null,
    ready,
    socketGeneration: 3,
    displayedKey: runtimeKey(displayed),
    currentItemsVersion: 0,
  };
  check('an empty install-cursor commit gates ready exactly while the data matches',
    lifecycleReadyGate({ ...base, committedItemsVersion: 0 }));
  const cut = { ...base, currentItemsVersion: 1 };
  check('that empty commit can NEVER satisfy a 1500-message cut that bumped the version',
    lifecycleReadyGate({ ...cut, committedItemsVersion: 0 }) === false
      && lifecycleReadyGate({ ...cut, committedItemsVersion: null }) === false);
  check('ready re-arms only after the surface commits the current items version',
    lifecycleReadyGate({ ...cut, committedItemsVersion: 1 })
      && lifecycleReadyGate({ ...cut, committedItemsVersion: 2 }) === false);
  check('a stale wrong command target fails closed',
    lifecycleReadyGate({ ...cut, committedItemsVersion: 1, commandTarget: { sessionId: 'sess-a', runtimeIncarnation: 'inc-1' } }) === false);
  check('a non-empty pending phase fails closed',
    lifecycleReadyGate({
      ...cut,
      committedItemsVersion: 1,
      pending: {
        generation: 4,
        source: { sessionId: 'sess-a', runtimeIncarnation: 'inc-1' },
        targetRequest: { path: '/p', catalogSource: 'rpi', catalogSessionId: 'sess-b', catalogStatus: 'native', catalogRuntimeIncarnation: null },
        targetIdentity: displayed,
        responseCursor: null,
        phase: 'awaiting-authoritative-cut',
        warmDisplayed: false,
      },
    }) === false);
  check('a stale ready socket generation fails closed',
    lifecycleReadyGate({ ...cut, committedItemsVersion: 1, socketGeneration: 4 }) === false);
  check('a commit from a different surface key fails closed',
    lifecycleReadyGate({ ...cut, committedItemsVersion: 1, displayedKey: '7:sess-a:5:inc-1' }) === false);
}

{
  const rect = (): TranscriptAnchorRect => ({ top: 0, bottom: 0 });
  const folds: Array<{
    open: boolean;
    parentElement: null;
    getBoundingClientRect: typeof rect;
    getAttribute: (name: string) => string | null;
  }> = [
    { open: true, parentElement: null, getBoundingClientRect: rect, getAttribute: (name) => (name === TRANSCRIPT_FOLD_ATTRIBUTE ? 'a:0:x' : null) },
    { open: false, parentElement: null, getBoundingClientRect: rect, getAttribute: (name) => (name === TRANSCRIPT_FOLD_ATTRIBUTE ? 'b:0:y' : null) },
    { open: false, parentElement: null, getBoundingClientRect: rect, getAttribute: (name) => (name === TRANSCRIPT_FOLD_ATTRIBUTE ? 'c:0:z' : null) },
  ];
  const content = {
    children: folds,
    stampedFolds: () => folds,
  };
  const saved = new Set(['b:0:y', 'c:0:z', 'ghost:0:missing']);
  check('first applyOpenFoldKeys pass opens exactly the saved keys (2 new)',
    applyOpenFoldKeys(content, saved) === 2
      && folds[0].open === false && folds[1].open === true && folds[2].open === true);
  check('second applyOpenFoldKeys pass is idempotent (0 new)',
    applyOpenFoldKeys(content, saved) === 0
      && folds[0].open === false && folds[1].open === true && folds[2].open === true);
  check('applyOpenFoldKeys with no content is a no-op',
    applyOpenFoldKeys(null, saved) === 0);
  check('applyOpenFoldKeys with an empty key set closes everything',
    applyOpenFoldKeys(content, new Set()) === 0 && folds.every((fold) => !fold.open));
}


{
  const ITEM_HEIGHT = 100;
  const FOLD_GROWTH = 30;
  const folds = [{
    open: false,
    getAttribute: (name: string) => (name === TRANSCRIPT_FOLD_ATTRIBUTE ? 'p:0:x' : null),
  }];
  const children = [
    { getBoundingClientRect: () => ({ top: 0 }) },
    { getBoundingClientRect: () => ({ top: ITEM_HEIGHT + (folds[0].open ? FOLD_GROWTH : 0) }) },
    { getBoundingClientRect: () => ({ top: 2 * ITEM_HEIGHT + (folds[0].open ? FOLD_GROWTH : 0) }) },
  ];
  const scroll = { scrollTop: 0 };
  const content = { children, stampedFolds: () => folds };
  const closed = compensatePrependedStage({ scroll, content, prepended: 2, openFoldKeys: new Set<string>() });
  check('no saved fold keys measure the closed layout delta',
    closed.openedFolds === 0 && closed.deltaPx === 2 * ITEM_HEIGHT, JSON.stringify(closed));
  const opened = compensatePrependedStage({ scroll, content, prepended: 2, openFoldKeys: new Set(['p:0:x']) });
  check('a newly opened fold above the kept anchor is INSIDE the measured delta',
    opened.openedFolds === 1 && opened.deltaPx === 2 * ITEM_HEIGHT + FOLD_GROWTH && folds[0].open,
    JSON.stringify(opened));
  check('non-prepend stages are no-ops',
    compensatePrependedStage({ scroll, content, prepended: 0, openFoldKeys: new Set(['p:0:x']) }).deltaPx === 0);
}

{
  const attrs = new Map<string, string>();
  const surface = {
    hidden: false,
    setAttribute(name: string, value: string) { attrs.set(name, value); },
    removeAttribute(name: string) { attrs.delete(name); },
  } as unknown as HTMLElement;
  applySurfaceVisibility(surface, false);
  check('inactive surface is hidden, inert, and aria-hidden',
    surface.hidden === true && attrs.get('inert') === '' && attrs.get('aria-hidden') === 'true');
  applySurfaceVisibility(surface, true);
  check('active surface drops the full hidden/inert/aria-hidden trio',
    surface.hidden === false && !attrs.has('inert') && attrs.get('aria-hidden') === 'false');
}
// ---------------------------------------------------------------------------
// P0 empty-transcript regression: a COLD bootstrap (the page's first authority
// bind) must call activateRuntime — the sole displayedRuntimeKey writer —
// BEFORE it subscribes or fetches messages, otherwise every mounted surface
// stays hidden/inert and the first successful send lands while #transcript is
// an empty canvas until a reload. The source locks pin the bind order; the
// SSR assertions drive the REAL TranscriptSurface through REAL messagesToItems
// output, so a surface whose user/assistant nodes are absent fails here.
{
  const appSource = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../src/App.tsx'), 'utf8');
  const coldBindAt = appSource.indexOf('if (coldBind || displayedNeedsRebind)');
  const activateAt = appSource.indexOf('activateRuntime(primaryIdentity, true);', coldBindAt);
  check('cold bootstrap binds the displayed surface via activateRuntime before subscribing or fetching messages',
    coldBindAt >= 0
      && activateAt > coldBindAt
      && activateAt < appSource.indexOf('const catchup = planReconnectDisplayedCatchup', coldBindAt)
      && activateAt < appSource.indexOf("sendCommand({ type: 'get_messages', sessionId: refillSessionId }, undefined, refillOpts)"),
    String({ coldBindAt, activateAt }));
  check('cold bootstrap installs displayState before activateRuntime when still unbound',
    appSource.includes('} else if (displayStateRef.current === null) {')
      && appSource.includes('displayed: primaryIdentity')
      && appSource.includes('commandTarget: primaryIdentity'),
    appSource.slice(coldBindAt, activateAt + 80));
  check('activateRuntime is the sole displayedRuntimeKey writer',
    appSource.split('displayedRuntimeKeyRef.current =').length - 1 === 1
      && appSource.split('setDisplayedRuntimeKey(').length - 1 === 1
      && appSource.includes('displayedRuntimeKeyRef.current = key;\n    setDisplayedRuntimeKey(key);'));
  check('each retained surface renders active only from the displayed runtime key',
    appSource.includes('active={rk === displayedRuntimeKey}'));
  const catchupCallAt = appSource.indexOf('const catchup = planReconnectDisplayedCatchup');
  const refillGetAt = appSource.indexOf("sendCommand({ type: 'get_messages', sessionId: refillSessionId }, undefined, refillOpts)", catchupCallAt);
  const refillBody = catchupCallAt >= 0 && refillGetAt > catchupCallAt
    ? appSource.slice(catchupCallAt, refillGetAt)
    : '';
  const restoreAt = appSource.indexOf('const restore = withBootstrapRetry', refillGetAt);
  const refillSlice = refillGetAt >= 0 && restoreAt > refillGetAt
    ? appSource.slice(refillGetAt, restoreAt)
    : (refillGetAt >= 0 ? appSource.slice(refillGetAt, refillGetAt + 9000) : '');
  check('reconnect catchup subscribes displayed at the stored cursor, exposes last-good, then refills',
    catchupCallAt >= 0
      && refillBody.includes('for (const identity of catchup.subscribe)')
      && refillBody.includes('reconnectSubscribeCursor')
      && refillBody.includes('subscribeRuntime(identity, cursor)')
      && refillBody.includes('refillInFlightRef.current = refillSessionId')
      && appSource.includes('return afterSwitch.then(() => refillWork())')
      && !appSource.includes('Promise.all([refill, restore])')
      && refillSlice.includes('planReconnectTurn')
      && refillSlice.includes('markStreamingFor(refillSessionId, latest.isStreaming === true)')
      && refillSlice.includes('recoveryState')
      && !appSource.slice(catchupCallAt, refillGetAt + 4000).includes("nextId('a')"),
    refillBody.slice(0, 400));
  const commandsAt = appSource.indexOf("type: 'get_commands'");
  const readyOnAt = appSource.indexOf("setConnState('on')", commandsAt);
  const refillJoinAt = appSource.indexOf('return afterSwitch.then(() => refillWork())');
  check('reconnect overlay stays up until get_commands then setConnState on',
    commandsAt > refillJoinAt
      && readyOnAt > commandsAt
      && !refillBody.includes("setConnState('on')"));
  check('reconnect refill RPCs stay allowed after restore enters switch-pending',
    appSource.includes('const refillOpts = { allowDuringSwitch: true as const }')
      && refillSlice.includes('undefined, refillOpts')
      && refillSlice.includes('if (!applyState(latestState, refillSessionId, false)) throw STALE_ABORT')
      && refillSlice.includes('if (!applyState(recoveryState, refillSessionId, false)) throw STALE_ABORT')
      && commandRejectedBySwitchPending(true, { allowDuringSwitch: true }) === false
      && commandRejectedBySwitchPending(true) === true);
  check('reconnect refill captures identity+socket and terminates the handler on ownership reject',
    refillBody.includes('let refillHold: ReconnectRefillHold')
      && refillBody.includes('adoptReconnectRefillHold')
      && refillBody.includes('reconnectRefillStillOwned')
      && refillSlice.includes('requireRefillOwned()')
      && refillSlice.includes('planReconnectRefillStep')
      && refillSlice.includes('throw STALE_ABORT')
      && !/if \(!alive\(\)\) throw STALE_ABORT/.test(refillSlice));
  const refillCatchAt = refillSlice.indexOf('.catch((error) => {');
  const refillFinallyAt = refillSlice.indexOf('.finally(() => {');
  const refillCatchBody = refillCatchAt >= 0 && refillFinallyAt > refillCatchAt
    ? refillSlice.slice(refillCatchAt, refillFinallyAt)
    : '';
  const refillFinallyBody = refillFinallyAt >= 0 ? refillSlice.slice(refillFinallyAt) : '';
  const ownedAt = refillCatchBody.indexOf('requireRefillOwned()');
  const toastAt = refillCatchBody.indexOf('applyReconnectRefillCatch');
  const recoverAt = refillCatchBody.indexOf("type: 'get_state'");
  check('refill catch requires ownership before toast or recovery get_state',
    ownedAt >= 0 && toastAt > ownedAt && recoverAt > toastAt,
    refillCatchBody.slice(0, 400));
  check('refill recovery retries then fail-closes instead of swallowing',
    refillCatchBody.includes('withBootstrapRetry')
      && refillCatchBody.includes('waitReconnectRetry')
      && !refillCatchBody.includes('if (stateError === STALE_ABORT) throw stateError;'));
  const restoreSlice = restoreAt >= 0
    ? appSource.slice(restoreAt, appSource.indexOf('return restore.then((restoreResult) => {', restoreAt))
    : '';
  check('session_list restore retries then fail-closes instead of returning null',
    restoreSlice.includes('withBootstrapRetry')
      && restoreSlice.includes('waitReconnectRetry')
      && !restoreSlice.includes('if (error === STALE_ABORT) throw error;'),
    restoreSlice.slice(0, 200));
  check('refill finally clears in-flight only when this slot still owns the refill',
    refillBody.includes('const refillSlot = refillSlotGenerationRef.current + 1')
      && refillFinallyBody.includes('reconnectRefillFinallyApplies(refillSlot, refillSlotGenerationRef.current)')
      && refillFinallyBody.includes('refillInFlightRef.current = null')
      && refillFinallyBody.includes('lastCutProgressAtRef.current = Date.now()'));
  check('warm replay tail advances the live cursor before confirmation',
    appSource.includes('applySwitchActions(hold.preBindActions)')
      && appSource.includes('acceptedCutLiveCursor(cut.events, cut.watermark)')
      && appSource.includes("case 'replay-events':")
      && appSource.includes('cursorByRuntimeKeyRef.current.set(runtimeKey(action.identity), replayCursor)'));
  const abortAt = appSource.indexOf('const abortActiveRun = useCallback');
  const abortSlice = abortAt >= 0 ? appSource.slice(abortAt, abortAt + 2500) : '';
  check('Stop abort re-reads authoritative isStreaming so a missed turn_terminal cannot leave Stop red',
    abortSlice.includes("type: 'abort'")
      && abortSlice.includes("type: 'get_state'")
      && abortSlice.includes('applyState(state, sid, false)'),
    abortSlice.slice(0, 400));
  const abortStay = planSessionSnapshotApply({
    shouldActivate: false,
    wireIncarnation: A.runtimeIncarnation,
    residentIncarnation: A.runtimeIncarnation,
    displayedSessionId: B.sessionId,
    targetSessionId: A.sessionId,
  });
  check('Stop A then switch B keeps the UI on B when A state resolves',
    abortStay.apply === true && abortStay.activate === false);
  check('Stop tap during a running turn sends abort and is not gated on the streaming ref',
    abortSlice.includes('applyComposerAbortTap')
      && abortSlice.includes('allowDuringSwitch: true')
      && abortSlice.includes('markStreamingFor(sid, false)')
      && abortSlice.indexOf('markStreamingFor(sid, false)') < abortSlice.indexOf("type: 'abort'")
      && appSource.includes('shouldHoldOptimisticStop')
      && abortSlice.includes('stop failed')
      && !abortSlice.includes('.catch(() => {')
      && !abortSlice.includes('streamingBySessionIdRef.current[sid]'),
    abortSlice.slice(0, 700));
  const sendBtnAt = appSource.indexOf('id="send-btn"');
  const sendBtnSlice = sendBtnAt >= 0 ? appSource.slice(sendBtnAt, sendBtnAt + 1800) : '';
  check('Stop pointerdown fires abort and click after Stop does not submit',
    sendBtnSlice.includes('onPointerDown')
      && sendBtnSlice.includes('abortActiveRun')
      && sendBtnSlice.includes('composerActionClickKind')
      && sendBtnSlice.includes('stopArmed')
      && !sendBtnSlice.includes('? abortActiveRun')
      && abortSlice.includes('abortGestureClickRef.current = false'),
    sendBtnSlice.slice(0, 400));
  const idleTap = applyComposerAbortTap('');
  const runningTap = applyComposerAbortTap('sess-bash');
  const runningPlan = planComposerAbort('sess-bash');
  check('Stop tap with a session id sends abort and flips footer to stopped before RPC',
    idleTap.command === null
      && idleTap.footerStopped === false
      && runningTap.footerStopped === true
      && runningTap.command?.type === 'abort'
      && runningTap.command?.sessionId === 'sess-bash'
      && runningPlan.kind === 'abort'
      && runningPlan.kind === 'abort'
      && runningPlan.streaming === false
      && runningPlan.footerStopped === true
      && planComposerAbort(null).kind === 'idle'
      && shouldHoldOptimisticStop(true, true) === true
      && shouldHoldOptimisticStop(true, false) === false
      && shouldHoldOptimisticStop(false, true) === false
      && abortIdleSnapshotToasts(true, false, true) === true
      && abortIdleSnapshotToasts(true, false, false) === false
      && abortIdleSnapshotToasts(true, true, true) === false
      && abortIdleSnapshotToasts(false, false, true) === false
      && appSource.includes('abortIdleSnapshotToasts')
      && appSource.includes("toast('run aborted')")
      && composerActionClickKind({ stopArmed: false, abortGestureClick: true }) === 'ignore'
      && composerActionClickKind({ stopArmed: true, abortGestureClick: true }) === 'ignore'
      && composerActionClickKind({ stopArmed: true, abortGestureClick: false }) === 'abort'
      && composerActionClickKind({ stopArmed: false, abortGestureClick: false }) === 'submit'
      && composerAbortChrome(true).dataAborting === 'true'
      && composerAbortChrome(false).dataAborting === undefined
      && dropSessionAborting({ 'sess-bash': true }, 'sess-bash')['sess-bash'] !== true);

  check('submit-while-streaming plans steer from streamingBySessionId',
    planComposerSubmitKind({ streaming: true, requested: 'prompt' }) === 'steer'
      && planComposerSubmitKind({ streaming: false, requested: 'prompt' }) === 'prompt');
  {
    const kinds: string[] = [];
    const streaming = await dispatchComposerSubmit({
      kind: planComposerSubmitKind({ streaming: true, requested: 'prompt' }),
      send: async (kind) => {
        kinds.push(kind);
        return { ok: true };
      },
      onConvertToSteer: () => { throw new Error('streaming submit must not convert'); },
    });
    check('submit-while-streaming routes steer',
      streaming.kind === 'steer' && streaming.converted === false && kinds.join(',') === 'steer');
  }
  {
    const kinds: string[] = [];
    const same: string[] = [];
    let converted = 0;
    const raced = await dispatchComposerSubmit({
      kind: 'prompt',
      send: async (kind) => {
        kinds.push(kind);
        if (kind === 'prompt') {
          throw Object.assign(new Error('session is already processing; choose steer or followUp'), {
            rpcCode: SESSION_PROCESSING_CODE,
          });
        }
        same.push('keep this');
        return { queued: true };
      },
      onConvertToSteer: () => { converted += 1; },
    });
    check('typed prompt-failure auto-converts to steer once',
      raced.kind === 'steer' && raced.converted === true
        && converted === 1
        && kinds.join(',') === 'prompt,steer'
        && same.join(',') === 'keep this'
        && planPromptBusySteerRetry({
          submittedKind: 'prompt',
          rpcCode: SESSION_PROCESSING_CODE,
          convertedOnce: true,
        }) === 'none'
        && rpcFailureToastSuppressed({ commandType: 'prompt', rpcCode: SESSION_PROCESSING_CODE }) === true);
  }
  check('App submit uses streamingBySessionIdRef and converts prompt-busy to steer',
    appSource.includes('planComposerSubmitKind')
      && appSource.includes('streamingBySessionIdRef.current[targetSid] === true')
      && appSource.includes('dispatchComposerSubmit')
      && appSource.includes('onConvertToSteer')
      && appSource.includes('rpcFailureToastSuppressed')
      && appSource.includes("submitKind === 'steer' ? { type: 'steer', message }")
      && appSource.includes('setSteeringBySessionId'));
  {
    const enterAt = appSource.indexOf('const submitComposerEnter = useCallback');
    const enterSlice = enterAt >= 0 ? appSource.slice(enterAt, enterAt + 180) : '';
    check(
      'App Enter always requests prompt so a stale streaming React flag cannot force idle steer',
      enterSlice.includes("submit('prompt')")
        && !enterSlice.includes("submit('steer')")
        && !enterSlice.includes('activeStreaming'),
      enterSlice,
    );
  }
  check('App queue_update writes the /web steering tray',
    appSource.includes("case 'queue_update'")
      && appSource.includes('steeringPreviewsFromMessages(frame.steering)')
      && appSource.includes('applyQueueUpdatePreviews')
      && appSource.includes('applyStateQueuedSteering')
      && appSource.includes('d.isStreaming === true'));
  check('live sequence gap resubscribes then one auxiliary get_state; idle snapshots request settled refill',
    planLiveSequenceGap({ cursor: 4, sequence: 6 }) === 'gap'
      && planLiveGapCatchUp({ recoveryInFlight: false }) === 'start'
      && planLiveGapCatchUp({ recoveryInFlight: true }) === 'coalesce'
      && planStreamCatchUpToast({ toastOwned: false, transport: false, busyCapacity: true }) === 'silent'
      && planStreamStallReconcile({ snapshotStreaming: false, liveHasStreamingAssistant: true }) === 'refill-settled'
      && planSettledStreamRefillRequest({
        snapshotStreaming: false,
        liveHasStreamingAssistant: true,
        reconnectRefillSessionId: 'alpha',
        targetSessionId: 'beta',
      }) === 'request'
      && planSettledStreamRefillRequest({
        snapshotStreaming: false,
        liveHasStreamingAssistant: true,
        reconnectRefillSessionId: 'alpha',
        targetSessionId: 'alpha',
      }) === 'idle'
      && appSource.includes('planLiveSequenceGap')
      && appSource.includes('planLiveGapCatchUp')
      && appSource.includes('gapCatchUpInFlightRef')
      && appSource.includes('gapCatchUpSlotRef')
      && appSource.includes('reconnectRefillStillOwned')
      && appSource.includes('auxiliary: true')
      && appSource.includes("type: 'get_state'")
      && appSource.includes('planSettledStreamRefillRequest')
      && appSource.includes('requestSettledStreamRefill')
      && appSource.includes("type: 'get_messages'")
      && !appSource.includes('planConsumedSteeringTranscriptInsert')
      && !appSource.includes('insertConsumedSteeringTexts')
      && !appSource.includes('refillInFlightRef.current !== true')
      && !appSource.includes('refillInFlightRef.current === true'));
  {
    const sid = 'switch-stall';
    const working = applyActivityStripEvent(emptyActivityStripState(sid), { type: 'turn_started', turnId: 'turn-stall' }, sid);
    let stalled = false;
    stalled = reduceStreamStall(stalled, { type: 'watchdog_probe_failed' });
    const silent = activityStripView(working, 'on', stalled);
    stalled = reduceStreamStall(stalled, { type: 'socket_drop', streaming: true });
    const dropped = activityStripView(working, 'reconnecting', stalled);
    const reconnectingOn = activityStripView(working, 'on', stalled);
    stalled = reduceStreamStall(stalled, { type: 'refill_finished' });
    const settled = applyActivityStripEvent(working, { type: 'turn_terminal', turnId: 'turn-stall', status: 'succeeded' }, sid);
    const done = activityStripView(settled, 'on', stalled);
    check('disconnect pulse reconnect refill clears the activity strip',
      silent?.state === 'reconnecting'
      && dropped?.state === 'reconnecting'
      && reconnectingOn?.state === 'reconnecting'
      && planStreamStallChrome(true) === 'pulse'
      && planStreamStallChrome(stalled) === 'hidden'
      && done === null
      && appSource.includes("io.noteStreamStall(sid, { type: 'watchdog_probe_alive' })")
      && appSource.includes("io.noteStreamStall(sid, { type: 'watchdog_probe_failed' })")
      && !appSource.includes("type: 'watchdog_silence'")
      && appSource.includes('noteStreamingSocketDrop')
      && appSource.includes("type: 'refill_finished'")
      && appSource.includes("className={activeActivityStripView.state === 'reconnecting' ? 'chat-slow-pulse' : undefined}"));
  }

  // -------------------------------------------------------------------------
  // Composer slash stamp after a WARM RECONNECT.
  //
  // The reconnect bootstrap clears the active ref (App.tsx:4827) and then
  // binds it to the listener PRIMARY via applyState(state) (:4842 -> :3681
  // `sessionIdRef.current = target`, :3683 `setSessionId(target)`), while
  // display authority — and its `commandTarget` — stays on the session the
  // user was looking at (display is only rebound for coldBind ||
  // displayedNeedsRebind). Slash dispatch stamps an EXPLICIT
  // `command.sessionId`, and an explicit id WINS over the active session in
  // ./goal routeCommandSession, so the stamp must name the DISPLAYED command
  // target: stamping the re-bound ref routes /code-review into the primary's
  // workspace, where git answers `fatal: Needed a single revision` and the
  // user sees `invalid source revision; expected a commit, branch, or tag`.
  // A typed prompt carries no sessionId and therefore always worked.
  {
    const primary: RuntimeIdentity = { sessionId: 'listener-primary-A', runtimeIncarnation: 'inc-primary' };
    const displayedB: RuntimeIdentity = { sessionId: 'displayed-B', runtimeIncarnation: 'inc-b' };
    const app = new AppSwitchHarness();
    app.display = { displayed: displayedB, commandTarget: displayedB, pending: null, nextGeneration: 2 };
    app.active = displayedB;
    app.resident[displayedB.sessionId] = displayedB.runtimeIncarnation;
    // The warm reconnect: the id-less bootstrap get_state is answered with
    // the listener primary, and the catchup refills the DISPLAYED session.
    app.discoverPrimary(primary, false);
    check('warm reconnect keeps display authority (and its command target) on the displayed session',
      sameRuntime(app.display?.displayed ?? null, displayedB)
        && sameRuntime(app.display?.commandTarget ?? null, displayedB),
      JSON.stringify(app.display));
    check('warm reconnect refills the displayed session, not the listener primary',
      app.messageRefills.join(',') === displayedB.sessionId);
    // Exactly what the bootstrap's applyState leaves in the active ref.
    const activeSessionRef = primary.sessionId;
    check('an explicit stamp of the re-bound active ref routes the command to the listener primary',
      routeCommandSession({ sessionId: activeSessionRef }, null) === primary.sessionId);
    check('the composer slash stamp targets the DISPLAYED session after a warm reconnect',
      composerCommandSessionId(app.display?.commandTarget ?? null, activeSessionRef) === displayedB.sessionId,
      composerCommandSessionId(app.display?.commandTarget ?? null, activeSessionRef));
    check('the composer command session falls back to the active ref only without display authority',
      composerCommandSessionId(null, activeSessionRef) === primary.sessionId
        && composerCommandSessionId({ sessionId: '' }, activeSessionRef) === primary.sessionId
        && composerCommandSessionId({ sessionId: 7 }, activeSessionRef) === primary.sessionId
        && composerCommandSessionId(undefined, null) === '');
    check('warm reconnect idle abort toast follows the displayed session, not the rebound active ref',
      abortIdleSnapshotToasts(true, false, displayedSessionMatches(displayedB.sessionId, app.display?.displayed.sessionId)) === true
        && abortIdleSnapshotToasts(true, false, displayedSessionMatches(displayedB.sessionId, activeSessionRef)) === false
        && abortIdleSnapshotToasts(true, false, displayedSessionMatches(primary.sessionId, app.display?.displayed.sessionId)) === false
        && displayedSessionMatches(displayedB.sessionId, '') === false
        && displayedSessionMatches(displayedB.sessionId, null) === false);
    const idleToastAt = appSource.indexOf('const toastAborted = abortIdleSnapshotToasts');
    const idleToastSlice = idleToastAt >= 0 ? appSource.slice(idleToastAt, idleToastAt + 420) : '';
    const terminalToastAt = appSource.indexOf("case 'turn_terminal':");
    const terminalToastSlice = terminalToastAt >= 0 ? appSource.slice(terminalToastAt, terminalToastAt + 900) : '';
    check('abort toasts use display authority on the idle snapshot and on turn_terminal',
      idleToastSlice.includes('displayedSessionMatches(target, displayStateRef.current?.displayed.sessionId)')
        && !idleToastSlice.includes('sessionIdRef.current')
        && terminalToastSlice.includes('displayedSessionMatches(sid, displayStateRef.current?.displayed.sessionId)')
        && !terminalToastSlice.includes("if (active) toast('run aborted')"));
    // Wiring lock: both slash stamp sites resolve through that helper — the
    // extension `/run` dispatch/summary and the `/code-review` probe with its
    // mid-flight switch guard. Every read of the active ref in those slices is
    // an argument to the helper, so no site stamps the clobberable ref alone.
    const composerAt = appSource.indexOf('const parsed = parseSupportedCommand(text);');
    const runAt = appSource.indexOf("type: 'run',", composerAt);
    const runSlice = runAt >= 0 ? appSource.slice(runAt, runAt + 1400) : '';
    const stamped = 'composerCommandSessionId(displayStateRef.current?.commandTarget, sessionIdRef.current)';
    const runRefReads = runSlice.match(/sessionIdRef\.current/g)?.length ?? 0;
    const runStamps = runSlice.split(stamped).length - 1;
    check('the extension /run stamp and its summary bubbles use the displayed command session',
      runSlice.includes(`const runSid = ${stamped};`)
        && runSlice.includes('if (runSid) command.sessionId = runSid;')
        && runSlice.split('pushItemFor(runSid').length - 1 === 2
        && runStamps === 1
        && runRefReads === 1,
      runSlice.slice(0, 600));
    // The mounted panel addresses its own code_review_* commands, so it is
    // mounted under the DISPLAYED authority — never a retained global owner.
    // A real cross-session display transition (judged on the DISPLAYED
    // identity the activation replaces, not the active ref, which a warm
    // reconnect re-binds to the listener primary) closes the panel and drops
    // the revisions, so A → B → A remounts the panel under A with no
    // B-owned or B-revisioned state, and a warm reconnect that keeps the
    // displayed session keeps this panel.
    const mountAt = appSource.indexOf("key={`code-review:${lifecycleView.displayedSessionId || sessionId || ''}`}");
    const activateAt = appSource.indexOf('const activateRuntime = useCallback');
    const activateHead = activateAt >= 0
      ? appSource.slice(activateAt, appSource.indexOf('if (shouldStopRealtimeOnSessionSwitch(realtimeOwnerRef.current, previous, identity.sessionId))', activateAt))
      : '';
    check('the review panel is mounted under the displayed authority, never a retained owner',
      mountAt >= 0
        && appSource.includes('sessionId={lifecycleView.displayedSessionId || sessionId}')
        && !appSource.includes('codeReviewOwnerSessionId'),
      String(mountAt));
    check('the cross-session reset reads the displaced DISPLAYED identity, not the active ref',
      activateHead.includes('const previousDisplayed = activatedRuntimeRef.current;')
        && activateHead.includes('if (previousDisplayed && previousDisplayed.sessionId !== identity.sessionId) {')
        && activateHead.includes("setActivePanel((panel) => (panel === 'code-review' ? '' : panel));")
        && activateHead.includes('setCodeReviewOpenArgs({});'),
      activateHead.slice(0, 700));
    // The reconnect preservation seam: capture the ACTUAL mounted review owner
    // (the display authority, which a prior reconnect can diverge from the
    // active ref) and decide preservation against the post-bootstrap DISPLAYED
    // authority — a reconnect that still displays the owner keeps the panel.
    check('the reconnect preservation captures and compares the displayed owner',
      appSource.includes('const reviewOwnerAtConnect = displayStateRef.current?.displayed.sessionId ?? sessionIdRef.current;')
        && appSource.includes("reconnectReviewOwnerRef.current = activePanelRef.current === 'code-review'\n      ? reviewOwnerAtConnect")
        && appSource.includes('if (reviewOwner && (displayStateRef.current?.displayed.sessionId ?? null) !== reviewOwner) {')
        && !appSource.includes('if (reviewOwner && sessionIdRef.current !== reviewOwner) {'),
      String(appSource.indexOf('const reviewOwnerAtConnect')));
    check('the slash open still records its revisions for the panel it mounts',
      appSource.includes('setCodeReviewOpenArgs({ ...openArgs, alreadyOpen: true });'));
  }

  const surfaceProps = {
    rk: runtimeKey(A),
    sessionId: A.sessionId,
    active: true,
    connState: 'on',
    workspaceCwd: '/w',
    itemsVersion: 1,
    getViewport: () => null,
    scrollRef: { current: null },
    pinIfPinned: () => {},
    onContentEl: () => {},
    onCommit: () => {},
    getAnchorIndex: () => -1,
  };
  resetTranscriptStamps();
  const items = messagesToItems([
    { role: 'user', content: [{ type: 'text', text: 'hello user' }], timestamp: 1 },
    { role: 'assistant', content: [{ type: 'text', text: 'hi assistant' }], timestamp: 2 },
  ]);
  const sent = renderToStaticMarkup(createElement(TranscriptSurface, {
    ...surfaceProps,
    items,
    initialStart: tailProjectionStart(items.length),
  }));
  check('a successful send renders the user message and assistant text in the active surface without a reload',
    sent.includes('msg msg--user') && sent.includes('hello user') && sent.includes('hi assistant')
      && !sent.includes('Send a prompt to start the session.'),
    sent);
  check('the sent surface stamps the user and assistant item roots through the production API',
    items.length === 2
      && stampedTranscriptKeys().has(`data-item-id=${items[0].id}`)
      && stampedTranscriptKeys().has(`data-item-id=${items[1].id}`),
    JSON.stringify([...stampedTranscriptKeys()]));
  resetTranscriptStamps();
  const empty = renderToStaticMarkup(createElement(TranscriptSurface, {
    ...surfaceProps,
    items: [],
    initialStart: 0,
  }));
  check('absent transcript nodes flip the surface to the empty-hint (the content assertion fails without them)',
    empty.includes('Send a prompt to start the session.') && !empty.includes('msg msg--user'),
    empty);
  resetTranscriptStamps();
  const longHistory = Array.from({ length: 301 }, (_, index) => ({
    id: `long-${index}`,
    message: {
      role: index % 2 === 0 ? 'user' : 'assistant',
      content: [{ type: 'text', text: `long turn ${index}` }],
      timestamp: index,
    },
  }));
  const startedLong = beginSessionSwitch({
    displayed: A,
    commandTarget: A,
    pending: null,
    nextGeneration: 1,
  }, targetB);
  const afterLongHandoff = acceptSwitchHandoff(
    startedLong.state,
    startedLong.generation,
    parseSwitchHandoff(handoff(B, 0), targetB),
    undefined,
  );
  const longCut = parseSubscriptionCut({
    type: 'subscription_snapshot',
    sessionId: B.sessionId,
    runtimeIncarnation: B.runtimeIncarnation,
    watermark: 1,
    reason: 'cold-full',
    state: { sessionId: B.sessionId },
    messages: longHistory,
    events: [],
  });
  const longPlan = acceptAuthoritativeCut(afterLongHandoff.state, startedLong.generation, longCut);
  const applyLong = longPlan.actions.find((action) => action.type === 'apply-messages');
  check('cold switch to a 301-message session applies the full messages array',
    longPlan.settlesPromise === true
      && applyLong?.type === 'apply-messages'
      && applyLong.messages.length === 301,
    JSON.stringify({ settles: longPlan.settlesPromise, type: applyLong?.type, n: applyLong && applyLong.type === 'apply-messages' ? applyLong.messages.length : null }));
  const longItems = mergeAuthoritativeItems(
    messagesToItems(applyLong && applyLong.type === 'apply-messages' ? applyLong.messages : []),
    [],
    false,
  );
  check('invalidate-then-refill of 301 messages keeps durable rows',
    longItems.length >= 301 && longItems.some((item) => item.kind === 'user' && item.text.includes('long turn 0')),
    JSON.stringify({ n: longItems.length, kinds: longItems.slice(0, 3).map((item) => item.kind) }));
  const lastGoodLong = messagesToItems(longHistory);
  const emptyRefill = mergeAuthoritativeItems([], lastGoodLong, false);
  check('empty get_messages refill after last-good does not wipe a 301-message transcript',
    emptyRefill.length >= 301 && emptyRefill.some((item) => item.kind === 'user' && item.text.includes('long turn 0')),
    JSON.stringify({ n: emptyRefill.length }));
  {
    const app = new AppSwitchHarness();
    app.items[B.sessionId] = 'audit last-good';
    app.resident[B.sessionId] = B.runtimeIncarnation;
    app.invalidateRuntime(B);
    check('runtime invalidate keeps last-good items for the same session',
      app.items[B.sessionId] === 'audit last-good'
      && app.items[A.sessionId] === 'source transcript text');
  }
  const longRendered = renderToStaticMarkup(createElement(TranscriptSurface, {
    ...surfaceProps,
    rk: runtimeKey(B),
    sessionId: B.sessionId,
    items: longItems,
    initialStart: tailProjectionStart(longItems.length),
  }));
  check('switch to a 301-message fixture session renders a non-empty transcript',
    longItems.length > 0
      && !longRendered.includes('Send a prompt to start the session.')
      && longRendered.includes('long turn 300'),
    longRendered.slice(0, 500));
  resetTranscriptStamps();
  const phoneHistory = Array.from({ length: 295 }, (_, index) => ({
    id: `audit-${index}`,
    message: {
      role: index % 2 === 0 ? 'user' : 'assistant',
      content: [{ type: 'text', text: `audit turn ${index}` }],
      timestamp: index,
    },
  }));
  const startedAudit = beginSessionSwitch({
    displayed: A,
    commandTarget: A,
    pending: null,
    nextGeneration: 1,
  }, targetB);
  const afterAuditHandoff = acceptSwitchHandoff(
    startedAudit.state,
    startedAudit.generation,
    parseSwitchHandoff(handoff(B, 0), targetB),
    undefined,
  );
  const auditCut = parseSubscriptionCut({
    type: 'session_resync',
    sessionId: B.sessionId,
    runtimeIncarnation: B.runtimeIncarnation,
    watermark: 16918,
    reason: 'resync',
    state: { sessionId: B.sessionId },
    messages: phoneHistory,
    events: [
      { type: 'message_update', sequence: 16606, sessionId: B.sessionId, runtimeIncarnation: B.runtimeIncarnation },
      { type: 'agent_settled', sequence: 16918, sessionId: B.sessionId, runtimeIncarnation: B.runtimeIncarnation },
    ],
  });
  const auditPlan = acceptAuthoritativeCut(afterAuditHandoff.state, startedAudit.generation, auditCut);
  const applyAudit = auditPlan.actions.find((action) => action.type === 'apply-messages');
  check('phone audit session_resync with a high-watermark event tail applies 295 messages and binds',
    auditPlan.settlesPromise === true
      && sameRuntime(auditPlan.state.displayed, B)
      && applyAudit?.type === 'apply-messages'
      && applyAudit.messages.length === 295
      && !auditPlan.actions.some((action) => action.type === 'resubscribe-zero' || action.type === 'fail-pending'),
    JSON.stringify({
      settles: auditPlan.settlesPromise,
      displayed: auditPlan.state.displayed.sessionId,
      n: applyAudit && applyAudit.type === 'apply-messages' ? applyAudit.messages.length : null,
      actions: auditPlan.actions.map((action) => action.type),
    }));
  const auditItems = mergeAuthoritativeItems(
    messagesToItems(applyAudit && applyAudit.type === 'apply-messages' ? applyAudit.messages : []),
    [],
    false,
  );
  const auditRendered = renderToStaticMarkup(createElement(TranscriptSurface, {
    ...surfaceProps,
    rk: runtimeKey(B),
    sessionId: B.sessionId,
    items: auditItems,
    initialStart: tailProjectionStart(auditItems.length),
  }));
  check('gapped audit resync paints transcript rows and not the empty-session hint',
    auditItems.length >= 295
      && !auditRendered.includes('Send a prompt to start the session.')
      && auditRendered.includes('audit turn 294'),
    auditRendered.slice(0, 500));
  resetTranscriptStamps();
  const handoffDoc = '<handoff-context>\nSession: alpha\nGoal: ship it\n</handoff-context>';
  const handoffItems = messagesToItems([
    { role: 'custom', customType: 'handoff', data: { text: handoffDoc }, timestamp: 1 },
  ]);
  check('the handoff record maps to a handoff-divider item carrying the doc',
    handoffItems.length === 1 && handoffItems[0].kind === 'handoff-divider' && handoffItems[0].text === handoffDoc,
    JSON.stringify(handoffItems));
  const renderedHandoff = renderToStaticMarkup(createElement(TranscriptSurface, {
    ...surfaceProps,
    items: handoffItems,
    initialStart: tailProjectionStart(handoffItems.length),
  }));
  check('the divider DOM exists in the surface with the data.text content',
    renderedHandoff.includes('msg--handoff-divider') && renderedHandoff.includes('Session handoff')
      && renderedHandoff.includes('&lt;handoff-context&gt;') && renderedHandoff.includes('ship it'),
    renderedHandoff.slice(0, 400));
  check('the divider item root is stamped through the production API',
    stampedTranscriptKeys().has(`data-item-id=${handoffItems[0].id}`),
    JSON.stringify([...stampedTranscriptKeys()]));
}

{
  const app = new AppSwitchHarness();
  const response = app.queueResponse();
  const switched = app.request(targetB);
  response.resolve(handoff(B, 0));
  await flush();
  const subscribed = app.subscriptions.length;
  const phoneHistory = Array.from({ length: 295 }, (_, index) => ({
    id: `audit-${index}`,
    message: {
      role: index % 2 === 0 ? 'user' : 'assistant',
      content: [{ type: 'text', text: `audit turn ${index}` }],
      timestamp: index,
    },
  }));
  app.acceptCut({
    type: 'session_resync',
    sessionId: B.sessionId,
    runtimeIncarnation: B.runtimeIncarnation,
    watermark: 16918,
    reason: 'resync',
    state: { sessionId: B.sessionId },
    messages: phoneHistory,
    events: [
      { type: 'message_update', sequence: 16606, sessionId: B.sessionId, runtimeIncarnation: B.runtimeIncarnation },
      { type: 'agent_settled', sequence: 16918, sessionId: B.sessionId, runtimeIncarnation: B.runtimeIncarnation },
    ],
  });
  const ok = await switched;
  check('user switch to a nonempty catalog row settles on a gapped session_resync without resubscribe-zero',
    ok === true
      && app.switchPending === false
      && sameRuntime(app.display!.displayed, B)
      && app.display?.pending === null
      && app.subscriptions.length === subscribed,
    JSON.stringify({
      ok,
      displayed: app.display?.displayed.sessionId,
      pending: app.display?.pending,
      extraSubs: app.subscriptions.slice(subscribed),
    }));
}

/* ---- code-review fullscreen/focus mode source pins (P2 review batch) ---- */
{
  const crSource = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../src/panels/CodeReviewPanel.tsx'), 'utf8');
  const cssSource = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../src/styles.css'), 'utf8');
  check('code-review panel header has a fullscreen/focus toggle (⛶)',
    crSource.includes('code-review-focus-toggle') && crSource.includes('⛶'));
  check('focus mode CSS hides the file tree, the diff display, and the resizer',
    cssSource.includes('.code-review--focus-mode .code-review__files,')
      && cssSource.includes('.code-review--focus-mode .code-review__diff,')
      && cssSource.includes('.code-review--focus-mode .code-review__thread-resizer {'));
  check('focus mode CSS targets the real dock class (.code-review__comments) for the conversation',
    cssSource.includes('.code-review--focus-mode .code-review__comments'));
  check('focus mode is full-bleed: the dock caps are dropped (width 100%, max-width none)',
    /\.code-review--focus-mode \.code-review__comments \{[^}]*width: 100%;/.test(cssSource)
      && !cssSource.includes('width: min(680px, 92%)'));
  check('focus mode forces the dock visible on narrow viewports regardless of tab',
    /@media \(max-width: 900px\)[\s\S]*?\.code-review--focus-mode \.code-review__comments \{[\s\S]*?display: flex !important;/.test(cssSource));
  check('focus toggle shows active state styling (is-active)',
    cssSource.includes('.code-review__focus-toggle.is-active'));
  check('Esc exits focus mode before closing the panel (handlePanelKeyDown Escape branch)',
    crSource.includes('if (focusMode) {\n        setFocusMode(false);'));
}

// Subsecond contract source locks: retained mounted surfaces + lifecycle marker;
// E2E hard-gates MAX_SWITCH_MS with trusted clicks and a one-rAF sampler.
{
  const appSource = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../src/App.tsx'), 'utf8');
  check('App renders each retained surface through its own FIXED demand window (no per-rAF growth)',
    appSource.includes('<SurfaceItems items={projectedItems}')
      && appSource.includes('normalizeTranscriptWindow(items.length, windowStart, follow)')
      && !appSource.includes('limit + TRANSCRIPT_PROJECTION_CHUNK')
      && !appSource.includes('activeItems.map((item) => {'));
  check('App retains mounted surfaces per runtime and flips only visibility',
    appSource.includes('const TranscriptSurface = memo(function TranscriptSurface(')
      && appSource.includes('applySurfaceVisibility(surface, active)'));
  check('App compensates prepends with fold keys applied BEFORE the kept-anchor measure',
    appSource.includes('compensatePrependedStage({'));
  check('App publishes the explicit lifecycle marker with state vs DOM projection counts',
    appSource.includes('id="session-lifecycle"')
      && appSource.includes('data-authoritative-ready=')
      && appSource.includes('data-state-messages=')
      && appSource.includes('data-projected-dom='));
  check('App activation sizes windows from the O(1) id->index map, never findIndex',
    appSource.includes('itemIndexBySessionIdRef.current.get(identity.sessionId)?.get(')
      && !appSource.includes('findIndex((item) => item.id ==='));
  check('App renders a below-tail parked anchor as bounded disjoint retained slices covering anchor and tail',
    appSource.includes('planParkedWindow(items.length, anchorIndex)')
      && appSource.includes('items.slice(parkedSlices.anchor.start, parkedSlices.anchor.end)')
      && appSource.includes('items.slice(parkedSlices.tail.start, parkedSlices.tail.end)'));
  check('a parked surface without an anchor fail-closes to the tail ownership window',
    appSource.includes('!walkOwned && !follow')
      && appSource.includes("{ kind: 'tail' as const, start: tailProjectionStart(items.length) }")
      && appSource.includes('forcePin={forcePin}')
      && appSource.includes('restored.follow || restored.anchorItemId === null'));
  check('the walk latch is render-producing state gated on the exact NOT-owned polarity, set by helper commits, and cleared on activation',
    appSource.includes('const [walkOwned, setWalkOwned] = useState(false)')
      && appSource.includes('!walkOwned && !follow')
      && !/!!walkOwned/.test(appSource)
      && appSource.includes('planDemandProjectionDecision({')
      && appSource.includes('if (planned.commit)')
      && appSource.includes('setWalkOwned(true)')
      && appSource.includes('setWalkOwned(false);')
      && appSource.includes('}, [active, forcePin, getViewport, rk])')
      && !appSource.includes('shiftProjectionWindowOlder')
      && !appSource.includes('shiftProjectionWindowNewer')
      && !appSource.includes('if (mutated) setWalkOwned(true);'));
  check('App publishes the lifecycle at every switch commit (fast, begin, handoff, cut, rollback, session-handoff cut)',
    appSource.split('publishLifecycle(').length - 1 === 12,
    String(appSource.split('publishLifecycle(').length - 1));
  check('App ready bit is the exact versioned gate: commit version must equal the current items version',
    appSource.includes('lifecycleReadyGate({')
      && appSource.includes('surfaceCommittedVersionRef.current.set(rk, itemsVersion)'));
  const e2eSource = readFileSync(
    join(dirname(fileURLToPath(import.meta.url)), '../../../../E2E.d/web/switch_catalog_identity_test.mjs'),
    'utf8',
  );
  check('E2E gates every complete switch duration under MAX_SWITCH_MS = 1000',
    e2eSource.includes('const MAX_SWITCH_MS = 1000;')
      && e2eSource.includes('if (ms >= MAX_SWITCH_MS) {'));
  check('E2E clicks are trusted Playwright locator clicks with a pre-installed page recorder',
    e2eSource.includes('page.click(') && e2eSource.includes('__rpiSwitchTiming')
      && !e2eSource.includes('row.click();'));
  check('E2E requires exact-ready completion at the first animation frame',
    e2eSource.includes('complete(timing.firstFrame)') && e2eSource.includes("paint = complete(timing.sync) ? 'sync' : 'next-frame'"));
  check('E2E hard-gates the cold click legs (desktop AND mobile) and asserts full state vs bounded DOM',
    e2eSource.includes("assertSubsecond('first cold user click B→A', coldTiming)")
      && e2eSource.includes('stateMessages === SEEDED_MESSAGES')
      && e2eSource.includes('projectedDom <= TRANSCRIPT_WINDOW_MAX'));
}

  /* Sidebar active-session marker: App must pass activeSessionId to
   * SessionSidebar — a dropped prop silently kills the current-row
   * marker (optional prop, tsc cannot catch it). */
  {
    const sidebarSource = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../src/App.tsx'), 'utf8');
    check('App passes activeSessionId to SessionSidebar (current-row marker lifeline)',
      sidebarSource.includes('activeSessionId={sessionId}'));
  }


{
  const inside = { contains: (node: { id?: string } | null) => node?.id === 'in' } as unknown as { contains(node: Node | null): boolean };
  const outsideNode = { id: 'out' } as unknown as Node;
  const insideNode = { id: 'in' } as unknown as Node;
  check('null selection does not hold projection', liveTextSelectionHoldsProjection(null) === false);
  check('collapsed caret does not hold projection', liveTextSelectionHoldsProjection({ isCollapsed: true, rangeCount: 1 }) === false);
  check('empty rangeCount does not hold projection', liveTextSelectionHoldsProjection({ isCollapsed: false, rangeCount: 0 }) === false);
  check('live range outside the surface does not hold projection',
    liveTextSelectionHoldsProjection({ isCollapsed: false, rangeCount: 1, anchorNode: outsideNode }, inside) === false);
  check('live range inside the surface holds projection',
    liveTextSelectionHoldsProjection({ isCollapsed: false, rangeCount: 1, anchorNode: insideNode }, inside) === true);
  check('covering range with both endpoints outside the surface still holds',
    liveTextSelectionHoldsProjection({ isCollapsed: false, rangeCount: 1, anchorNode: outsideNode, focusNode: outsideNode, intersectsRoot: true }, inside) === true);
  check('non-covering range with both endpoints outside the surface does not hold',
    liveTextSelectionHoldsProjection({ isCollapsed: false, rangeCount: 1, anchorNode: outsideNode, focusNode: outsideNode }, inside) === false);
  check('unmounted surface root does not hold projection',
    liveTextSelectionHoldsProjection({ isCollapsed: false, rangeCount: 1 }, null) === false);
  const total = TRANSCRIPT_PROJECTION_CHUNK * 4;
  const mid = projectionStartForAnchor(total, TRANSCRIPT_PROJECTION_CHUNK);
  const older = shiftProjectionWindowOlder(total, mid);
  const newer = shiftProjectionWindowNewer(total, mid);
  const base = {
    total,
    start: mid,
    follow: false,
    atOlderEdge: true,
    atNewerEdge: false,
    inSliceMode: false,
    sliceAnchor: -1,
    selection: null as const,
    root: inside,
    gestureActive: false,
  };
  check('decide shifts older at the edge without a live surface selection',
    planDemandProjectionDecision(base).hold === false
      && planDemandProjectionDecision(base).nextStart === older);
  check('decide holds an in-surface live range even at the older edge',
    (() => {
      const planned = planDemandProjectionDecision({
        ...base,
        selection: { isCollapsed: false, rangeCount: 1, anchorNode: insideNode },
      });
      return planned.hold === true && planned.nextStart === mid;
    })());
  check('decide does not hold a composer selection at the older edge',
    (() => {
      const planned = planDemandProjectionDecision({
        ...base,
        selection: { isCollapsed: false, rangeCount: 1, anchorNode: outsideNode },
      });
      return planned.hold === false && planned.nextStart === older;
    })());
  check('mouse gesture holds the window before the Range exists',
    planDemandProjectionDecision({ ...base, gestureActive: true }).nextStart === mid);
  check('collapse wakeup at an already-at-edge scroll shifts without another scroll',
    (() => {
      const held = planDemandProjectionDecision({
        ...base,
        selection: { isCollapsed: false, rangeCount: 1, anchorNode: insideNode },
      });
      const after = planDemandProjectionDecision({
        ...base,
        selection: { isCollapsed: true, rangeCount: 1, anchorNode: insideNode },
      });
      return held.nextStart === mid && after.hold === false && after.nextStart === older;
    })());
  check('newer-edge decision matches shiftProjectionWindowNewer',
    planDemandProjectionDecision({ ...base, atOlderEdge: false, atNewerEdge: true }).nextStart === newer
      && planDemandProjectionDecision({ ...base, atOlderEdge: false, atNewerEdge: true }).kind === 'shift'
      && planDemandProjectionDecision({ ...base, atOlderEdge: false, atNewerEdge: true }).commit === true);
  const staleFollow = planDemandProjectionDecision({
    ...base,
    start: mid,
    follow: true,
    atOlderEdge: true,
  });
  const tail = tailProjectionStart(total);
  check('normalize-first does not demand-shift using pre-resync edge geometry',
    staleFollow.kind === 'normalize'
      && staleFollow.commit === true
      && staleFollow.nextStart === tail
      && staleFollow.nextStart !== shiftProjectionWindowOlder(total, mid));
  check('following the tail does not older-shift from an unpinned older edge',
    (() => {
      const planned = planDemandProjectionDecision({
        ...base,
        start: tail,
        follow: true,
        atOlderEdge: true,
      });
      return planned.kind === 'rest' && planned.commit === false && planned.nextStart === tail;
    })());
  const alreadyHead = planDemandProjectionDecision({
    ...base,
    start: 0,
    follow: false,
    atOlderEdge: true,
  });
  check('already-normalized parked head at older edge is a rest, not a fake shift',
    alreadyHead.kind === 'rest' && alreadyHead.commit === false && alreadyHead.nextStart === 0);
  const sliceSameStart = projectionStartForAnchor(total, 80);
  const sliceCollapse = planDemandProjectionDecision({
    ...base,
    start: sliceSameStart,
    inSliceMode: true,
    sliceAnchor: 80,
    atOlderEdge: false,
  });
  check('slice collapse commits even when nextStart equals the stored start',
    sliceCollapse.kind === 'sliceCollapse'
      && sliceCollapse.commit === true
      && sliceCollapse.nextStart === sliceSameStart);
  const slices = { kind: 'slices' as const, id: 1 };
  const otherSlices = { kind: 'slices' as const, id: 2 };
  const contiguous = { kind: 'slices' as const, id: 3 };
  let committed: typeof slices | typeof otherSlices | typeof contiguous | null = null;
  committed = commitRenderPlanIfUnheld(false, committed, slices);
  check('unheld paint commits the painted plan', committed === slices);
  check('first hold with a prior commit keeps that commit, not the speculative plan',
    resolveHeldRenderPlan(true, committed, otherSlices) === slices);
  committed = commitRenderPlanIfUnheld(true, committed, otherSlices);
  check('held paint does not write the speculative plan as committed', committed === slices);
  check('hold does not switch an existing disjoint plan to null',
    resolveHeldRenderPlan(true, committed, null) === slices);
  check('releasing the hold renders the latest speculative plan',
    resolveHeldRenderPlan(false, committed, contiguous) === contiguous);
  committed = commitRenderPlanIfUnheld(false, committed, contiguous);
  check('unheld paint after release commits the new plan', committed === contiguous);
  check('first hold with no committed paint falls through to speculative',
    resolveHeldRenderPlan(true, null, slices) === slices);
  check('hold skips follow-tail normalize so a growing session keeps the committed start',
    projectionStartForRender(total + TRANSCRIPT_PROJECTION_CHUNK, mid, true, true) === mid
      && projectionStartForRender(total + TRANSCRIPT_PROJECTION_CHUNK, mid, true, false)
        === tailProjectionStart(total + TRANSCRIPT_PROJECTION_CHUNK));
}




console.log(`\nsessionSwitchApp.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  for (const failure of failures) console.error(`  FAIL ${failure}`);
  process.exit(1);
}
process.exit(0);
