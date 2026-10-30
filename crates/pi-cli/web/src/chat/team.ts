// Project-channel controller for the /chat console.
//
// ChatMappingStore remains the durable Project/mapping authority and
// RunnerRegistry remains the only transport authority. This store owns the
// active exact-session projection, Project-channel selection, bounded
// Broadcast state, and the single runner-event callback used by mapped chat
// sessions.

import {
  applyAuthoritativeOrchestrationSnapshot,
  applyOrchestrationEvent,
  beginOrchestrationRefresh,
  failOrchestrationRefresh,
  initialOrchestrationProjection,
  type JobListWire,
  type OrchestrationProjection,
} from '../orchestrationProjection';
import { isAuthoritativeSubscriptionCut } from '../protocol';
import { safeText } from '../redact';
import type { EventFrame, RpcResponse } from '../types';
import type { StorageLike } from '../hostToken';
import type { GoalLifecycle, GoalStateWire, GoalWire } from '../panels/GoalPanel';
import type { JobStatus, JobWire } from '../jobCard';
import { runnerEventListenerCount, subscribeRunnerEvents } from './events';
import { CHAT_INVITE_EMPTY } from './chatEmpty';
import { resolveChannelRecipients, runnerMentionToken, type ChannelMentionMember } from './chatMentions';
import {
  assistantPlainText,
  boundChannelMessages,
  channelLogErrorMessage,
  composeWakeBrief,
  loadChannelLogs,
  saveChannelLogs,
  type ChannelMessage,
} from './chatChannelLog';
import {
  applySessionEvent,
  promptTurnId,
  type SessionTranscriptState,
} from './SessionView';
import type {
  ActiveMapping,
  ProjectRecord,
  ProjectRunnerMapping,
  RunnerCommandClient,
} from './mapping';
import {
  RunnerCommandError,
  runnerDisplayName,
  runnerMentionName,
  type RunnerConnectionState,
  type RunnerView,
} from './runners';

export const MAX_PROJECTION_ERROR_CHARS = 240;
/** Bounded recent job rows rendered under one runner activity row. */
export const MAX_RUNNER_ACTIVITY_ROWS = 8;
export const MAX_BROADCAST_RUNS_PER_PROJECT = 20;
/** Retained terminal attempts per response card; nonterminal attempts are never dropped. */
export const MAX_BROADCAST_ATTEMPTS_PER_CARD = 5;
export const MAX_PENDING_BROADCAST_EVENTS = 256;
export const MAX_PENDING_BROADCAST_BYTES = 256 * 1024;
export {
  MAX_CHANNEL_MESSAGES,
  type ChannelMessage,
} from './chatChannelLog';

const DEFAULT_MAX_CONCURRENT_READS = 4;
const GOAL_PAYLOAD_MALFORMED = 'goal_get payload was malformed; keeping the last known goal.';
const STREAM_RESTORE_ERROR = 'The live stream could not be fully restored. View the runner conversation for the durable result.';
const CANCEL_OWNERSHIP_ERROR = 'The runner is no longer processing this Broadcast turn.';
const PROMPT_TURN_MISMATCH = 'The prompt response did not match the streamed turn.';
const HANDLED_WITHOUT_STREAM = 'Completed without a streamed response.';

export type RunnerSessionKey = string;
export type ContextPane = 'files' | 'jobs';
export type MobilePane = 'projects' | 'conversation' | 'context';
export type ChannelTarget =
  | { kind: 'runner'; mappingId: string }
  | { kind: 'broadcast' };

export type BroadcastAttemptStatus =
  | 'dispatching'
  | 'streaming'
  | 'cancelling'
  | 'succeeded'
  | 'failed'
  | 'cancelled'
  | 'interrupted';

export type BroadcastFailureKind =
  | 'precondition'
  | 'remote'
  | 'transport_unknown'
  | 'protocol'
  | 'stream_gap'
  | null;

export interface BroadcastAttempt {
  id: string;
  number: number;
  promptCommandId: string;
  abortCommandId: string | null;
  abortSendCount: number;
  startedAt: number;
  endedAt: number | null;
  startSequence: number;
  turnId: string | null;
  modelLabel: string | null;
  status: BroadcastAttemptStatus;
  failureKind: BroadcastFailureKind;
  error: string;
  cancelRequested: boolean;
  connectionLost: boolean;
  streamIncomplete: boolean;
  transcript: SessionTranscriptState;
}

export interface BroadcastResponseCard {
  mappingId: string;
  runnerId: string;
  sessionId: string;
  sessionPath: string;
  runnerLabel: string;
  attempts: BroadcastAttempt[];
  currentAttemptId: string;
}

export interface BroadcastRun {
  id: string;
  projectId: string;
  prompt: string;
  createdAt: number;
  isHandoff: boolean;
  targetMappingIds: string[];
  responses: BroadcastResponseCard[];
}

export interface ProjectChannelState {
  projectId: string;
  label: string;
  target: ChannelTarget | null;
  contextPane: ContextPane;
  goalOwnerMappingId: string | null;
  fileOwnerMappingId: string | null;
  activeBroadcastRunId: string | null;
  broadcastRuns: BroadcastRun[];
  channelMessages: ChannelMessage[];
}

export interface TeamJobReference {
  runnerId: string;
  sessionId: string;
  jobId: string;
  groupId?: string | null;
}

export interface TeamSessionState {
  key: RunnerSessionKey;
  mappingId: string;
  projectId: string;
  runnerId: string;
  sessionId: string;
  sessionPath: string;
  /** Display-only mirror from RunnerView; never a routing key. */
  codename: string | null;
  connection: RunnerConnectionState;
  lastSequence: number;
  activeTurnId: string | null;
  isStreaming: boolean;
  /** null until the first job_list proves the orchestration state. */
  orchestrationEnabled: boolean | null;
  bootstrapped: boolean;
  refreshError: string;
  goalError: string;
  goal: GoalStateWire | null;
  orchestration: OrchestrationProjection;
}

export interface TeamSnapshot {
  sessions: Readonly<Record<RunnerSessionKey, TeamSessionState>>;
  channels: Readonly<Record<string, ProjectChannelState>>;
  selectedProjectId: string;
  mobilePane: MobilePane;
  projectionError: string;
  channelLogError: string;
}

export interface ProjectAggregate {
  projectId: string;
  runnerSessions: TeamSessionState[];
  goalCounts: Record<'none' | GoalLifecycle, number>;
  jobCounts: Record<JobStatus, number>;
  disconnectedCount: number;
  orchestrationDisabledCount: number;
  activeJobCount: number;
  failedJobCount: number;
}

export interface ActiveProjectMember {
  mapping: ActiveMapping;
  runner: RunnerView;
  session: TeamSessionState;
}

export interface TeamStoreOptions {
  runners: RunnerCommandClient;
  storage?: StorageLike | null;
  pageAuthority?: string;
  maxConcurrentReads?: number;
  now?: () => number;
  randomBytes?: (length: number) => Uint8Array;
}

export type TeamListener = (snapshot: TeamSnapshot) => void;

interface BroadcastAttemptRuntime {
  projectId: string;
  runId: string;
  mappingId: string;
  runnerId: string;
  sessionId: string;
  attemptId: string;
  bufferedEvents: EventFrame[];
  bufferedBytes: number;
}

interface AttemptReference {
  projectId: string;
  runId: string;
  mappingId: string;
  attemptId: string;
}

const GOAL_REFRESH_EVENTS = new Set(['goal_updated', 'goal_usage_charged']);
const ORCHESTRATION_EVENT_TYPES = new Set(['job_updated', 'agent_updated', 'message_delivered']);
const CONTEXT_PANES = new Set<ContextPane>(['files', 'jobs']);
const MOBILE_PANES = new Set<MobilePane>(['projects', 'conversation', 'context']);
const NONTERMINAL_ATTEMPT_STATUSES = new Set<BroadcastAttemptStatus>([
  'dispatching',
  'streaming',
  'cancelling',
]);

/** Collision-safe identity: JSON.stringify([runnerId, sessionId]). */
export function runnerSessionKey(runnerId: string, sessionId: string): RunnerSessionKey {
  const runner = runnerId.trim();
  const session = sessionId.trim();
  if (!runner || !session) throw new Error('Runner and session identity are required.');
  return JSON.stringify([runner, session]);
}

/** Collision-safe job identity: JSON.stringify([runnerId, sessionId, jobId]). */
export function teamJobKey(reference: Pick<TeamJobReference, 'runnerId' | 'sessionId' | 'jobId'>): string {
  const runner = reference.runnerId.trim();
  const session = reference.sessionId.trim();
  const job = reference.jobId.trim();
  if (!runner || !session || !job) throw new Error('Runner, session, and job identity are required.');
  return JSON.stringify([runner, session, job]);
}

function broadcastTurnKey(runnerId: string, sessionId: string, turnId: string): string {
  return JSON.stringify([runnerId, sessionId, turnId]);
}

function boundedError(error: unknown): string {
  const text = safeText(error instanceof Error ? error.message : String(error));
  return text.length <= MAX_PROJECTION_ERROR_CHARS
    ? text
    : `${text.slice(0, MAX_PROJECTION_ERROR_CHARS - 1)}…`;
}

function defaultRandomBytes(length: number): Uint8Array {
  const bytes = new Uint8Array(length);
  const cryptoObject = (globalThis as { crypto?: { getRandomValues?: (value: Uint8Array) => Uint8Array } }).crypto;
  if (cryptoObject?.getRandomValues) return cryptoObject.getRandomValues(bytes);
  for (let index = 0; index < bytes.length; index += 1) {
    bytes[index] = Math.floor(Math.random() * 256);
  }
  return bytes;
}

function namespaceFromBytes(bytes: Uint8Array): string {
  let value = '';
  for (const byte of bytes) value += byte.toString(36).padStart(2, '0').slice(-2);
  return value;
}

function isAttemptRunning(attempt: BroadcastAttempt | undefined): boolean {
  return attempt !== undefined && NONTERMINAL_ATTEMPT_STATUSES.has(attempt.status);
}

function currentAttempt(card: BroadcastResponseCard): BroadcastAttempt | undefined {
  return card.attempts.find((attempt) => attempt.id === card.currentAttemptId) ?? card.attempts.at(-1);
}

function boundedAttempts(attempts: readonly BroadcastAttempt[]): BroadcastAttempt[] {
  const terminalIds = attempts
    .filter((attempt) => !isAttemptRunning(attempt))
    .slice(-MAX_BROADCAST_ATTEMPTS_PER_CARD)
    .map((attempt) => attempt.id);
  const retainedTerminalIds = new Set(terminalIds);
  return attempts.filter((attempt) => isAttemptRunning(attempt) || retainedTerminalIds.has(attempt.id));
}

function eventTurnId(frame: EventFrame): string {
  const direct = typeof frame.turnId === 'string' ? frame.turnId.trim() : '';
  if (direct) return direct;
  const snake = typeof frame.turn_id === 'string' ? frame.turn_id.trim() : '';
  return snake;
}

function frameSequence(frame: EventFrame): number | null {
  return typeof frame.sequence === 'number' && Number.isFinite(frame.sequence)
    ? frame.sequence
    : null;
}

function frameBytes(frame: EventFrame): number {
  try {
    return new TextEncoder().encode(JSON.stringify(frame)).byteLength;
  } catch {
    return MAX_PENDING_BROADCAST_BYTES + 1;
  }
}

function readActiveTurnId(frame: Record<string, unknown>): string | null {
  const direct = typeof frame.activeTurnId === 'string' ? frame.activeTurnId.trim() : '';
  if (direct) return direct;
  const state = frame.state;
  if (state && typeof state === 'object' && !Array.isArray(state)) {
    const stateRecord = state as Record<string, unknown>;
    const nested = typeof stateRecord.activeTurnId === 'string' ? stateRecord.activeTurnId.trim() : '';
    if (nested) return nested;
  }
  const lifecycle = frame.lifecycle;
  if (lifecycle && typeof lifecycle === 'object' && !Array.isArray(lifecycle)) {
    const lifecycleRecord = lifecycle as Record<string, unknown>;
    const camel = typeof lifecycleRecord.activeTurnId === 'string' ? lifecycleRecord.activeTurnId.trim() : '';
    if (camel) return camel;
    const snake = typeof lifecycleRecord.active_turn_id === 'string' ? lifecycleRecord.active_turn_id.trim() : '';
    if (snake) return snake;
  }
  return null;
}

function readSnapshotStreaming(frame: Record<string, unknown>, activeTurnId: string | null): boolean {
  const state = frame.state;
  if (state && typeof state === 'object' && !Array.isArray(state)) {
    const isStreaming = (state as Record<string, unknown>).isStreaming;
    if (typeof isStreaming === 'boolean') return isStreaming;
  }
  return activeTurnId !== null;
}

function modelLabel(value: unknown): string | null {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null;
  const model = value as Record<string, unknown>;
  for (const candidate of [model.name, model.id]) {
    if (typeof candidate === 'string' && candidate.trim()) return candidate.trim();
  }
  return null;
}

function responseModelLabel(data: unknown): string | null {
  if (!data || typeof data !== 'object' || Array.isArray(data)) return null;
  return modelLabel((data as Record<string, unknown>).model);
}

function activeMappingsForProject(
  projectId: string,
  mappings: readonly ProjectRunnerMapping[],
): ActiveMapping[] {
  return mappings
    .filter((mapping): mapping is ActiveMapping => mapping.projectId === projectId && mapping.state === 'active')
    .sort((left, right) => left.createdAt - right.createdAt || left.id.localeCompare(right.id));
}

function normalizeTarget(
  target: ChannelTarget | null | undefined,
  activeMappings: readonly ActiveMapping[],
): ChannelTarget | null {
  if (activeMappings.length === 0) return null;
  if (target?.kind === 'broadcast') return target;
  if (target?.kind === 'runner' && activeMappings.some((mapping) => mapping.id === target.mappingId)) {
    return target;
  }
  return { kind: 'broadcast' };
}

const GOAL_LIFECYCLE_VALUES = new Set<GoalLifecycle>(['active', 'paused', 'completed', 'dropped']);

function isGoalWire(value: unknown): value is GoalWire {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return false;
  const wire = value as Partial<GoalWire>;
  return typeof wire.objective === 'string'
    && typeof wire.id === 'string'
    && typeof wire.lifecycle === 'string'
    && GOAL_LIFECYCLE_VALUES.has(wire.lifecycle as GoalLifecycle)
    && Array.isArray(wire.pins);
}

/** Strict goal_get payload gate. In particular, `current: {}` is rejected. */
export function parseChatGoalState(data: unknown): GoalStateWire | null {
  if (data === null || typeof data !== 'object' || Array.isArray(data)) return null;
  if (!('revision' in data) || typeof (data as { revision?: unknown }).revision !== 'number') return null;
  const revision = (data as { revision: number }).revision;
  if (!Number.isFinite(revision)) return null;
  if (!('current' in data)) return { revision, current: null };
  const current = (data as { current: unknown }).current;
  if (current === null) return { revision, current: null };
  if (!isGoalWire(current)) return null;
  return { revision, current };
}

/** Pure per-project aggregation over canonical exact-session states. */
export function deriveProjectAggregate(
  projectId: string,
  sessions: Readonly<Record<string, TeamSessionState>>,
): ProjectAggregate {
  const runnerSessions = Object.values(sessions).filter((state) => state.projectId === projectId);
  const goalCounts: Record<'none' | GoalLifecycle, number> = {
    none: 0,
    active: 0,
    paused: 0,
    completed: 0,
    dropped: 0,
  };
  const jobCounts: Record<JobStatus, number> = {
    queued: 0,
    running: 0,
    completed: 0,
    failed: 0,
    cancelled: 0,
  };
  let disconnectedCount = 0;
  let orchestrationDisabledCount = 0;
  for (const state of runnerSessions) {
    if (state.connection !== 'connected') disconnectedCount += 1;
    if (state.orchestrationEnabled === false) orchestrationDisabledCount += 1;
    goalCounts[state.goal?.current?.lifecycle ?? 'none'] += 1;
    for (const view of Object.values(state.orchestration.jobs)) jobCounts[view.job.status] += 1;
  }
  return {
    projectId,
    runnerSessions,
    goalCounts,
    jobCounts,
    disconnectedCount,
    orchestrationDisabledCount,
    activeJobCount: jobCounts.queued + jobCounts.running,
    failedJobCount: jobCounts.failed,
  };
}

function recency(job: JobWire): number {
  return job.startedAt ?? job.createdAt;
}

/** Live jobs first, then settled history, bounded per runner. */
export function runnerActivityJobs(state: TeamSessionState): JobWire[] {
  const live: JobWire[] = [];
  const settled: JobWire[] = [];
  for (const view of Object.values(state.orchestration.jobs)) {
    (view.job.status === 'queued' || view.job.status === 'running' ? live : settled).push(view.job);
  }
  live.sort((left, right) => recency(right) - recency(left));
  settled.sort((left, right) => recency(right) - recency(left));
  return [...live, ...settled].slice(0, MAX_RUNNER_ACTIVITY_ROWS);
}

export function deriveBroadcastRunStatus(
  run: BroadcastRun,
): 'running' | 'completed' | 'partial' | 'failed' | 'cancelled' {
  const attempts = run.responses.map(currentAttempt).filter((attempt): attempt is BroadcastAttempt => attempt !== undefined);
  if (attempts.some(isAttemptRunning)) return 'running';
  if (attempts.length > 0 && attempts.every((attempt) => attempt.status === 'succeeded')) return 'completed';
  if (attempts.length > 0 && attempts.every((attempt) => attempt.status === 'cancelled')) return 'cancelled';
  const succeeded = attempts.some((attempt) => attempt.status === 'succeeded');
  const failed = attempts.some((attempt) => attempt.status === 'failed' || attempt.status === 'interrupted');
  if (!succeeded && failed) return 'failed';
  return 'partial';
}

export function activeProjectMembers(
  projectId: string,
  mappings: readonly ProjectRunnerMapping[],
  runners: readonly RunnerView[],
  sessions: Readonly<Record<string, TeamSessionState>>,
): ActiveProjectMember[] {
  const runnerById = new Map(runners.map((runner) => [runner.id, runner]));
  const members: ActiveProjectMember[] = [];
  for (const mapping of activeMappingsForProject(projectId, mappings)) {
    const runner = runnerById.get(mapping.runnerId);
    if (!runner) continue;
    let key: string;
    try {
      key = runnerSessionKey(mapping.runnerId, mapping.sessionId);
    } catch {
      continue;
    }
    const session = sessions[key];
    if (!session || session.mappingId !== mapping.id) continue;
    members.push({ mapping, runner, session });
  }
  return members;
}

export class TeamStore {
  private readonly runners: RunnerCommandClient;
  private readonly storage: StorageLike | null;
  private readonly pageAuthority: string;
  private readonly hydratedLogs: Record<string, ChannelMessage[]>;
  private readonly maxConcurrentReads: number;
  private readonly now: () => number;
  private readonly namespace: string;
  private readonly listeners = new Set<TeamListener>();
  private readonly unsubscribes = new Map<string, () => void>();
  private readonly sessions = new Map<string, TeamSessionState>();
  private readonly channels = new Map<string, ProjectChannelState>();
  private readonly sessionListeners = new Map<string, Set<(frame: unknown) => void>>();
  private readonly broadcastRuntimes = new Map<string, BroadcastAttemptRuntime>();
  private readonly turnAttempts = new Map<string, string>();
  private readonly abortRetryPending = new Set<string>();
  private readonly abortInFlight = new Set<string>();
  private readonly dirty = new Set<string>();
  private readonly inFlight = new Set<string>();
  private readonly followUp = new Set<string>();
  private readonly goalRequests = new Map<string, number>();
  private projectIds = new Set<string>();
  private projectOrder: string[] = [];
  private mappingById = new Map<string, ProjectRunnerMapping>();
  private runnerById = new Map<string, RunnerView>();
  private selectedProjectId = '';
  private mobilePane: MobilePane = 'projects';
  private projectionError = '';
  private channelLogError = '';
  private sequence = 0;
  private identitySequence = 0;
  private cached: TeamSnapshot | null = null;
  private disposed = false;

  constructor(options: TeamStoreOptions) {
    this.runners = options.runners;
    this.storage = options.storage ?? null;
    this.pageAuthority = (options.pageAuthority ?? '').trim();
    const loaded = loadChannelLogs(this.storage, this.pageAuthority);
    this.hydratedLogs = loaded.logs;
    this.channelLogError = loaded.ok ? '' : channelLogErrorMessage(loaded.error);
    this.maxConcurrentReads = Math.max(1, options.maxConcurrentReads ?? DEFAULT_MAX_CONCURRENT_READS);
    this.now = options.now ?? Date.now;
    this.namespace = namespaceFromBytes((options.randomBytes ?? defaultRandomBytes)(6));
  }

  subscribe(listener: TeamListener): () => void {
    this.listeners.add(listener);
    listener(this.snapshot());
    let subscribed = true;
    return () => {
      if (!subscribed) return;
      subscribed = false;
      this.listeners.delete(listener);
    };
  }

  subscribeSession(
    runnerId: string,
    sessionId: string,
    listener: (frame: unknown) => void,
  ): () => void {
    const key = runnerSessionKey(runnerId, sessionId);
    if (!this.sessions.has(key)) throw new Error('The selected Project session is unavailable.');
    let listeners = this.sessionListeners.get(key);
    if (!listeners) {
      listeners = new Set();
      this.sessionListeners.set(key, listeners);
    }
    listeners.add(listener);
    let subscribed = true;
    return () => {
      if (!subscribed) return;
      subscribed = false;
      const current = this.sessionListeners.get(key);
      if (!current) return;
      current.delete(listener);
      if (current.size === 0) this.sessionListeners.delete(key);
    };
  }

  snapshot(): TeamSnapshot {
    if (!this.cached) {
      const sessions: Record<string, TeamSessionState> = {};
      for (const [key, state] of this.sessions) sessions[key] = state;
      const channels: Record<string, ProjectChannelState> = {};
      for (const [projectId, channel] of this.channels) channels[projectId] = channel;
      this.cached = {
        sessions,
        channels,
        selectedProjectId: this.selectedProjectId,
        mobilePane: this.mobilePane,
        projectionError: this.projectionError,
        channelLogError: this.channelLogError,
      };
    }
    return this.cached;
  }

  selectProject(projectId: string): void {
    if (!this.projectIds.has(projectId) || this.selectedProjectId === projectId) return;
    this.selectedProjectId = projectId;
    this.invalidateAndEmit();
  }

  selectTarget(projectId: string, target: ChannelTarget): void {
    const channel = this.requireChannel(projectId);
    const activeMappings = this.activeMappings(projectId);
    if (target.kind === 'runner') {
      if (!activeMappings.some((mapping) => mapping.id === target.mappingId)) {
        throw new Error('The selected runner is not active in this Project.');
      }
      this.channels.set(projectId, {
        ...channel,
        target: { kind: 'runner', mappingId: target.mappingId },
        goalOwnerMappingId: target.mappingId,
        fileOwnerMappingId: target.mappingId,
      });
    } else if (target.kind === 'broadcast') {
      if (activeMappings.length === 0) throw new Error('Add a runner before selecting Broadcast.');
      this.channels.set(projectId, { ...channel, target: { kind: 'broadcast' } });
    } else {
      throw new Error('Unknown Project channel target.');
    }
    this.invalidateAndEmit();
  }

  setContextPane(projectId: string, pane: ContextPane): void {
    if (!CONTEXT_PANES.has(pane)) throw new Error('Unknown Project context pane.');
    const channel = this.requireChannel(projectId);
    if (channel.contextPane === pane) return;
    this.channels.set(projectId, { ...channel, contextPane: pane });
    this.invalidateAndEmit();
  }

  setGoalOwner(projectId: string, mappingId: string): void {
    this.setOwner(projectId, mappingId, 'goalOwnerMappingId');
  }

  setFileOwner(projectId: string, mappingId: string): void {
    this.setOwner(projectId, mappingId, 'fileOwnerMappingId');
  }

  setMobilePane(pane: MobilePane): void {
    if (!MOBILE_PANES.has(pane)) throw new Error('Unknown mobile Project pane.');
    if (this.mobilePane === pane) return;
    this.mobilePane = pane;
    this.invalidateAndEmit();
  }

  /** Adopt current durable Project/mapping inputs and Registry runner views. */
  reconcileInputs(
    projects: readonly ProjectRecord[],
    mappings: readonly ProjectRunnerMapping[],
    runners: readonly RunnerView[],
  ): void {
    if (this.disposed) return;
    const orderedProjects = [...projects].sort((left, right) => left.createdAt - right.createdAt || left.id.localeCompare(right.id));
    const projectIds = new Set(orderedProjects.map((project) => project.id));
    const runnerById = new Map(runners.map((runner) => [runner.id, runner]));
    const mappingById = new Map(mappings.map((mapping) => [mapping.id, mapping]));
    const previousSessions = new Map(this.sessions);
    const nextSessions = new Map<string, TeamSessionState>();
    const errors: string[] = [];
    const initialReads: string[] = [];

    for (const mapping of mappings) {
      if (mapping.state !== 'active' || !projectIds.has(mapping.projectId)) continue;
      const runner = runnerById.get(mapping.runnerId);
      if (!runner) continue;
      let key: string;
      try {
        key = runnerSessionKey(mapping.runnerId, mapping.sessionId);
      } catch {
        errors.push(`Mapping ${mapping.id} has a blank runner or session identity; excluded from the team view.`);
        continue;
      }
      if (nextSessions.has(key)) {
        errors.push('Runner session is mapped to more than one project; the extra mapping is excluded from the team view.');
        continue;
      }
      const previous = previousSessions.get(key);
      const preserve = previous?.mappingId === mapping.id && previous.projectId === mapping.projectId;
      const state: TeamSessionState = {
        key,
        mappingId: mapping.id,
        projectId: mapping.projectId,
        runnerId: mapping.runnerId,
        sessionId: mapping.sessionId,
        sessionPath: mapping.sessionPath,
        codename: runner.codename,
        connection: runner.state,
        lastSequence: preserve ? previous.lastSequence : 0,
        activeTurnId: preserve ? previous.activeTurnId : null,
        isStreaming: preserve ? previous.isStreaming : false,
        orchestrationEnabled: preserve ? previous.orchestrationEnabled : null,
        bootstrapped: preserve ? previous.bootstrapped : false,
        refreshError: preserve ? previous.refreshError : '',
        goalError: preserve ? previous.goalError : '',
        goal: preserve ? previous.goal : null,
        orchestration: preserve ? previous.orchestration : initialOrchestrationProjection(),
      };
      nextSessions.set(key, state);
      if (state.connection === 'connected' && (!preserve || previous.connection !== 'connected')) initialReads.push(key);
    }

    for (const key of previousSessions.keys()) {
      if (!nextSessions.has(key)) this.releaseSession(key);
    }
    this.sessions.clear();
    for (const [key, state] of nextSessions) this.sessions.set(key, state);

    const nextChannels = new Map<string, ProjectChannelState>();
    for (const project of orderedProjects) {
      const activeMappings = activeMappingsForProject(project.id, mappings);
      const previous = this.channels.get(project.id);
      const activeIds = new Set(activeMappings.map((mapping) => mapping.id));
      const firstMappingId = activeMappings[0]?.id ?? null;
      const broadcastRuns = previous?.broadcastRuns.slice(-MAX_BROADCAST_RUNS_PER_PROJECT) ?? [];
      const activeBroadcast = broadcastRuns.find((run) => run.id === previous?.activeBroadcastRunId);
      nextChannels.set(project.id, {
        projectId: project.id,
        label: project.label,
        target: normalizeTarget(previous?.target, activeMappings),
        contextPane: previous && CONTEXT_PANES.has(previous.contextPane) ? previous.contextPane : 'files',
        goalOwnerMappingId: previous?.goalOwnerMappingId && activeIds.has(previous.goalOwnerMappingId)
          ? previous.goalOwnerMappingId
          : firstMappingId,
        fileOwnerMappingId: previous?.fileOwnerMappingId && activeIds.has(previous.fileOwnerMappingId)
          ? previous.fileOwnerMappingId
          : firstMappingId,
        activeBroadcastRunId: activeBroadcast && deriveBroadcastRunStatus(activeBroadcast) === 'running'
          ? activeBroadcast.id
          : [...broadcastRuns].reverse().find((run) => deriveBroadcastRunStatus(run) === 'running')?.id ?? null,
        broadcastRuns,
        channelMessages: boundChannelMessages(previous?.channelMessages ?? this.hydratedLogs[project.id] ?? []),
      });
    }
    this.channels.clear();
    for (const [projectId, channel] of nextChannels) this.channels.set(projectId, channel);

    const oldOrder = this.projectOrder;
    if (!projectIds.has(this.selectedProjectId)) {
      if (orderedProjects.length === 0) {
        this.selectedProjectId = '';
      } else if (this.selectedProjectId) {
        const removedIndex = oldOrder.indexOf(this.selectedProjectId);
        const replacementIndex = removedIndex < 0 ? 0 : Math.min(removedIndex, orderedProjects.length - 1);
        this.selectedProjectId = orderedProjects[replacementIndex].id;
      } else {
        this.selectedProjectId = orderedProjects[0].id;
      }
    }

    this.projectIds = projectIds;
    this.projectOrder = orderedProjects.map((project) => project.id);
    this.mappingById = mappingById;
    this.runnerById = runnerById;
    this.projectionError = errors.length > 0 ? boundedError(errors[0]) : '';
    this.reconcileBroadcastAttempts();
    this.cached = null;
    this.syncSubscriptions(nextSessions);
    this.emit();
    for (const key of initialReads) this.queueRead(key);
  }

  async sendBroadcast(
    projectId: string,
    draft: string,
    targetMappingIds?: readonly string[],
  ): Promise<BroadcastRun> {
    const channel = this.requireChannel(projectId);
    const prompt = draft.trim();
    if (!prompt) throw new Error('Broadcast message is required.');
    if (channel.broadcastRuns.some((run) => deriveBroadcastRunStatus(run) === 'running')) {
      throw new Error('A Broadcast is still running.');
    }
    const active = this.activeMappings(projectId);
    if (active.length === 0) throw new Error(CHAT_INVITE_EMPTY);
    const requested = targetMappingIds === undefined
      ? active
      : active.filter((mapping) => targetMappingIds.includes(mapping.id));
    if (targetMappingIds !== undefined) {
      if (targetMappingIds.length === 0) throw new Error('Broadcast recipients are required.');
      const activeIds = new Set(active.map((mapping) => mapping.id));
      if (targetMappingIds.some((mappingId) => !activeIds.has(mappingId))) {
        throw new Error('A Broadcast recipient is not active in this Project.');
      }
    }
    this.appendChannelMessage(projectId, {
      id: this.nextIdentity('msg'),
      at: this.now(),
      kind: 'human',
      mappingId: null,
      author: 'You',
      text: prompt,
    });
    return this.wakeMappings(projectId, prompt, requested, false);
  }

  private async wakeMappings(
    projectId: string,
    prompt: string,
    targets: readonly ActiveMapping[],
    isHandoff: boolean,
  ): Promise<BroadcastRun> {
    const channel = this.requireChannel(projectId);
    if (channel.broadcastRuns.some((run) => deriveBroadcastRunStatus(run) === 'running')) {
      throw new Error('A Broadcast is still running.');
    }
    const runId = this.nextIdentity('run');
    const createdAt = this.now();
    const responses = targets.map((mapping): BroadcastResponseCard => {
      const attempt = this.createAttempt(projectId, runId, mapping, 1, this.nextIdentity('prompt'));
      return {
        mappingId: mapping.id,
        runnerId: mapping.runnerId,
        sessionId: mapping.sessionId,
        sessionPath: mapping.sessionPath,
        runnerLabel: this.runnerById.get(mapping.runnerId)
          ? runnerDisplayName(this.runnerById.get(mapping.runnerId)!)
          : 'Runner',
        attempts: [attempt],
        currentAttemptId: attempt.id,
      };
    });
    const run: BroadcastRun = {
      id: runId,
      projectId,
      prompt,
      createdAt,
      isHandoff,
      targetMappingIds: targets.map((mapping) => mapping.id),
      responses,
    };
    const runs = [...channel.broadcastRuns, run].slice(-MAX_BROADCAST_RUNS_PER_PROJECT);
    const retainedRunIds = new Set(runs.map((candidate) => candidate.id));
    for (const previous of channel.broadcastRuns) {
      if (!retainedRunIds.has(previous.id)) this.releaseRunRuntimes(previous);
    }
    this.channels.set(projectId, {
      ...channel,
      activeBroadcastRunId: runId,
      broadcastRuns: runs,
    });
    this.invalidateAndEmit();

    const dispatches = responses.map((card) => {
      const runner = this.runnerById.get(card.runnerId);
      if (!runner || runner.state !== 'connected') {
        this.failAttempt(
          { projectId, runId, mappingId: card.mappingId, attemptId: card.currentAttemptId },
          'precondition',
          'Runner is not connected.',
        );
        return Promise.resolve();
      }
      return this.dispatchPrompt({
        projectId,
        runId,
        mappingId: card.mappingId,
        attemptId: card.currentAttemptId,
      });
    });
    await Promise.allSettled(dispatches);
    return this.findRun(projectId, runId) ?? run;
  }

  async cancelBroadcastTarget(projectId: string, runId: string, mappingId: string): Promise<void> {
    const reference = this.currentAttemptReference(projectId, runId, mappingId);
    const attempt = this.findAttempt(reference);
    if (!attempt || (attempt.status !== 'dispatching' && attempt.status !== 'streaming')) return;
    if (attempt.cancelRequested) return;
    this.updateAttempt(reference, (current) => ({ ...current, cancelRequested: true }));
    this.invalidateAndEmit();
    const current = this.findAttempt(reference);
    if (current?.turnId) await this.requestAbort(reference);
  }

  async cancelBroadcast(projectId: string, runId: string): Promise<void> {
    const run = this.findRun(projectId, runId);
    if (!run) throw new Error('Broadcast run was not found.');
    const cancellations: Promise<void>[] = [];
    for (const card of run.responses) {
      const attempt = currentAttempt(card);
      if (!attempt || (attempt.status !== 'dispatching' && attempt.status !== 'streaming')) continue;
      cancellations.push(this.cancelBroadcastTarget(projectId, runId, card.mappingId));
    }
    await Promise.allSettled(cancellations);
  }

  async retryBroadcastTarget(projectId: string, runId: string, mappingId: string): Promise<void> {
    const channel = this.requireChannel(projectId);
    if (channel.broadcastRuns.some((run) => run.id !== runId && deriveBroadcastRunStatus(run) === 'running')) {
      throw new Error('A Broadcast is still running.');
    }
    const run = this.findRun(projectId, runId);
    const card = run?.responses.find((response) => response.mappingId === mappingId);
    const previous = card ? currentAttempt(card) : undefined;
    if (!run || !card || !previous) throw new Error('Broadcast response was not found.');
    if (previous.status !== 'failed' && previous.status !== 'cancelled' && previous.status !== 'interrupted') {
      throw new Error('Only a settled unsuccessful response can be retried.');
    }
    const mapping = this.mappingById.get(mappingId);
    if (!mapping || mapping.state !== 'active' || mapping.projectId !== projectId
      || mapping.runnerId !== card.runnerId || mapping.sessionId !== card.sessionId) {
      throw new Error('The Broadcast runner is no longer active in this Project.');
    }
    const runner = this.runnerById.get(card.runnerId);
    if (!runner || runner.state !== 'connected') throw new Error('Runner is not connected.');

    const reuseUnknownCommand = previous.failureKind === 'transport_unknown' && previous.turnId === null;
    const attempt = this.createAttempt(
      projectId,
      runId,
      mapping,
      previous.number + 1,
      reuseUnknownCommand ? previous.promptCommandId : this.nextIdentity('prompt'),
    );
    this.replaceCard(projectId, runId, mappingId, (current) => ({
      ...current,
      attempts: boundedAttempts([...current.attempts, attempt]),
      currentAttemptId: attempt.id,
    }));
    this.setActiveRun(projectId, runId);
    this.invalidateAndEmit();
    await this.dispatchPrompt({ projectId, runId, mappingId, attemptId: attempt.id });
  }

  /** Live event intake — exactly one registration per mapped runner. */
  applyRunnerEvent(runnerId: string, frame: unknown): void {
    if (this.disposed || !frame || typeof frame !== 'object' || Array.isArray(frame)) return;
    const wire = frame as EventFrame;
    const sessionId = typeof wire.sessionId === 'string' ? wire.sessionId.trim() : '';
    const type = typeof wire.type === 'string' ? wire.type : '';
    if (!sessionId || !type) return;
    let key: string;
    try {
      key = runnerSessionKey(runnerId, sessionId);
    } catch {
      return;
    }
    const before = this.sessions.get(key);
    if (!before) return;

    const authoritativeCut = isAuthoritativeSubscriptionCut(frame);
    const sequence = frameSequence(wire);
    if (!authoritativeCut && sequence !== null && sequence <= before.lastSequence) return;

    let changed = false;
    const record = frame as Record<string, unknown>;
    const watermark = typeof record.watermark === 'number' && Number.isFinite(record.watermark)
      ? record.watermark
      : null;
    let activeTurnId = before.activeTurnId;
    let isStreaming = before.isStreaming;
    if (authoritativeCut) {
      activeTurnId = readActiveTurnId(record);
      isStreaming = readSnapshotStreaming(record, activeTurnId);
    } else if (type === 'turn_started') {
      activeTurnId = eventTurnId(wire) || null;
      isStreaming = activeTurnId !== null;
    } else if (type === 'turn_terminal') {
      const terminalTurnId = eventTurnId(wire);
      if (terminalTurnId && terminalTurnId === activeTurnId) {
        activeTurnId = null;
        isStreaming = false;
      }
    } else if (type === 'session_resync_required') {
      const lifecycleTurn = readActiveTurnId(record);
      if (lifecycleTurn) {
        activeTurnId = lifecycleTurn;
        isStreaming = true;
      }
    }
    const lastSequence = Math.max(before.lastSequence, sequence ?? 0, watermark ?? 0);
    if (lastSequence !== before.lastSequence || activeTurnId !== before.activeTurnId || isStreaming !== before.isStreaming) {
      this.patch(key, (current) => ({ ...current, lastSequence, activeTurnId, isStreaming }));
      changed = true;
    }

    if (authoritativeCut) {
      changed = this.reconcileBroadcastCut(runnerId, sessionId, activeTurnId) || changed;
    } else {
      changed = this.routeBroadcastEvent(runnerId, sessionId, wire) || changed;
    }

    if (GOAL_REFRESH_EVENTS.has(type)) this.readGoal(key, ++this.sequence);
    if (authoritativeCut) {
      this.queueRead(key);
    } else if (ORCHESTRATION_EVENT_TYPES.has(type)) {
      const current = this.sessions.get(key)!;
      const projection = applyOrchestrationEvent(current.orchestration, wire);
      if (projection !== current.orchestration) {
        const repair = projection.needsRefresh;
        this.patch(key, (state) => ({ ...state, orchestration: projection }));
        changed = true;
        if (repair) this.queueRead(key);
      }
    }

    const listeners = this.sessionListeners.get(key);
    if (listeners) for (const listener of [...listeners]) listener(frame);
    if (changed) this.invalidateAndEmit();
    this.maybeContinueRequestedCancels(runnerId, sessionId);
  }

  /** Goal-only refresh: never issues job_list. */
  refreshGoal(key: string): void {
    this.readGoal(key, ++this.sequence);
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    for (const unsubscribe of this.unsubscribes.values()) unsubscribe();
    this.unsubscribes.clear();
    this.dirty.clear();
    this.inFlight.clear();
    this.followUp.clear();
    this.goalRequests.clear();
    this.sessions.clear();
    this.channels.clear();
    this.sessionListeners.clear();
    this.broadcastRuntimes.clear();
    this.turnAttempts.clear();
    this.abortRetryPending.clear();
    this.abortInFlight.clear();
    this.projectIds.clear();
    this.mappingById.clear();
    this.runnerById.clear();
    this.selectedProjectId = '';
    this.cached = null;
    this.emit();
  }

  private activeMappings(projectId: string): ActiveMapping[] {
    return activeMappingsForProject(projectId, [...this.mappingById.values()]);
  }

  private requireChannel(projectId: string): ProjectChannelState {
    const channel = this.channels.get(projectId);
    if (!channel) throw new Error('Project channel was not found.');
    return channel;
  }

  private setOwner(
    projectId: string,
    mappingId: string,
    field: 'goalOwnerMappingId' | 'fileOwnerMappingId',
  ): void {
    const channel = this.requireChannel(projectId);
    if (!this.activeMappings(projectId).some((mapping) => mapping.id === mappingId)) {
      throw new Error('The selected owner is not active in this Project.');
    }
    if (channel[field] === mappingId) return;
    this.channels.set(projectId, { ...channel, [field]: mappingId });
    this.invalidateAndEmit();
  }

  private nextIdentity(kind: string): string {
    this.identitySequence += 1;
    return `team-${this.namespace}-${kind}-${this.identitySequence}`;
  }

  private createAttempt(
    projectId: string,
    runId: string,
    mapping: ActiveMapping,
    number: number,
    promptCommandId: string,
  ): BroadcastAttempt {
    const attempt: BroadcastAttempt = {
      id: this.nextIdentity('attempt'),
      number,
      promptCommandId,
      abortCommandId: null,
      abortSendCount: 0,
      startedAt: this.now(),
      endedAt: null,
      startSequence: this.sessions.get(runnerSessionKey(mapping.runnerId, mapping.sessionId))?.lastSequence ?? 0,
      turnId: null,
      modelLabel: null,
      status: 'dispatching',
      failureKind: null,
      error: '',
      cancelRequested: false,
      connectionLost: false,
      streamIncomplete: false,
      transcript: { items: [], activeAssistantId: null, pendingUsers: [] },
    };
    this.broadcastRuntimes.set(attempt.id, {
      projectId,
      runId,
      mappingId: mapping.id,
      runnerId: mapping.runnerId,
      sessionId: mapping.sessionId,
      attemptId: attempt.id,
      bufferedEvents: [],
      bufferedBytes: 0,
    });
    return attempt;
  }

  private async dispatchPrompt(reference: AttemptReference): Promise<void> {
    const run = this.findRun(reference.projectId, reference.runId);
    const card = run?.responses.find((response) => response.mappingId === reference.mappingId);
    const attempt = this.findAttempt(reference);
    if (!run || !card || !attempt || attempt.status !== 'dispatching') return;
    try {
      const response = await this.runners.command(
        card.runnerId,
        { type: 'prompt', message: this.wakeBriefFor(reference.projectId, card.mappingId) },
        { commandId: attempt.promptCommandId, sourceSessionId: card.sessionId },
      );
      const current = this.findAttempt(reference);
      if (!current || current.status !== 'dispatching') return;
      const acceptedTurnId = (response.turnId ?? promptTurnId(response.data) ?? '').trim();
      if (!acceptedTurnId) {
        this.finishAttempt(reference, 'succeeded', null, HANDLED_WITHOUT_STREAM);
        return;
      }
      if (!this.bindAttemptToTurn(reference, acceptedTurnId, responseModelLabel(response.data), false)) return;
      const bound = this.findAttempt(reference);
      if (bound?.cancelRequested) void this.requestAbort(reference);
    } catch (error) {
      const current = this.findAttempt(reference);
      if (!current || current.status !== 'dispatching') return;
      const kind = this.runnerErrorKind(error);
      if (kind === 'transport' && this.recoverBufferedTurn(reference)) return;
      this.failAttempt(
        reference,
        kind === 'remote' ? 'remote' : kind === 'protocol' ? 'protocol' : 'transport_unknown',
        error,
      );
    }
  }

  private runnerErrorKind(error: unknown): 'remote' | 'transport' | 'protocol' {
    if (error instanceof RunnerCommandError) return error.kind;
    if (error && typeof error === 'object') {
      const kind = (error as { kind?: unknown }).kind;
      if (kind === 'remote' || kind === 'transport' || kind === 'protocol') return kind;
    }
    return 'transport';
  }

  private recoverBufferedTurn(reference: AttemptReference): boolean {
    const runtime = this.broadcastRuntimes.get(reference.attemptId);
    if (!runtime) return false;
    const startedTurns = new Set(
      runtime.bufferedEvents
        .filter((frame) => frame.type === 'turn_started')
        .map(eventTurnId)
        .filter(Boolean),
    );
    if (startedTurns.size !== 1) return false;
    const [turnId] = [...startedTurns];
    const started = runtime.bufferedEvents.find((frame) => frame.type === 'turn_started' && eventTurnId(frame) === turnId);
    return this.bindAttemptToTurn(reference, turnId, modelLabel(started?.model), true);
  }

  private bindAttemptToTurn(
    reference: AttemptReference,
    turnId: string,
    acceptedModelLabel: string | null,
    connectionLost: boolean,
  ): boolean {
    const runtime = this.broadcastRuntimes.get(reference.attemptId);
    const attempt = this.findAttempt(reference);
    if (!runtime || !attempt || attempt.status !== 'dispatching') return false;
    const matching = runtime.bufferedEvents.filter((frame) => eventTurnId(frame) === turnId);
    const hasMatchingStart = matching.some((frame) => frame.type === 'turn_started');
    const hasForeignStart = runtime.bufferedEvents.some((frame) => frame.type === 'turn_started' && eventTurnId(frame) !== turnId);
    if (hasForeignStart && !hasMatchingStart) {
      this.failAttempt(reference, 'protocol', PROMPT_TURN_MISMATCH);
      return false;
    }
    const started = matching.find((frame) => frame.type === 'turn_started');
    this.updateAttempt(reference, (current) => ({
      ...current,
      turnId,
      modelLabel: acceptedModelLabel ?? modelLabel(started?.model),
      status: 'streaming',
      connectionLost,
    }));
    this.turnAttempts.set(broadcastTurnKey(runtime.runnerId, runtime.sessionId, turnId), reference.attemptId);
    runtime.bufferedEvents = [];
    runtime.bufferedBytes = 0;
    this.invalidateAndEmit();
    for (const frame of matching) this.routeAcceptedEvent(reference, frame);
    return true;
  }

  private routeBroadcastEvent(runnerId: string, sessionId: string, frame: EventFrame): boolean {
    const turnId = eventTurnId(frame);
    let changed = false;
    if (turnId) {
      const acceptedAttemptId = this.turnAttempts.get(broadcastTurnKey(runnerId, sessionId, turnId));
      if (acceptedAttemptId) {
        const runtime = this.broadcastRuntimes.get(acceptedAttemptId);
        if (runtime) {
          changed = this.routeAcceptedEvent({
            projectId: runtime.projectId,
            runId: runtime.runId,
            mappingId: runtime.mappingId,
            attemptId: runtime.attemptId,
          }, frame) || changed;
        }
      }
    }
    const sequence = frameSequence(frame);
    for (const runtime of this.broadcastRuntimes.values()) {
      if (runtime.runnerId !== runnerId || runtime.sessionId !== sessionId) continue;
      const attempt = this.findAttempt(runtime);
      if (!attempt || attempt.status !== 'dispatching' || attempt.turnId !== null || !turnId) continue;
      if (sequence !== null && sequence <= attempt.startSequence) continue;
      changed = this.bufferDispatchEvent(runtime, frame) || changed;
    }
    return changed;
  }

  private bufferDispatchEvent(runtime: BroadcastAttemptRuntime, frame: EventFrame): boolean {
    const bytes = frameBytes(frame);
    let incomplete = false;
    if (bytes > MAX_PENDING_BROADCAST_BYTES) {
      incomplete = true;
    } else {
      runtime.bufferedEvents.push(frame);
      runtime.bufferedBytes += bytes;
      while (runtime.bufferedEvents.length > MAX_PENDING_BROADCAST_EVENTS
        || runtime.bufferedBytes > MAX_PENDING_BROADCAST_BYTES) {
        const removed = runtime.bufferedEvents.shift();
        if (!removed) break;
        runtime.bufferedBytes = Math.max(0, runtime.bufferedBytes - frameBytes(removed));
        incomplete = true;
      }
    }
    if (!incomplete) return false;
    return this.updateAttempt(runtime, (attempt) => attempt.streamIncomplete
      ? attempt
      : { ...attempt, streamIncomplete: true });
  }

  private routeAcceptedEvent(reference: AttemptReference, frame: EventFrame): boolean {
    const attempt = this.findAttempt(reference);
    if (!attempt || !attempt.turnId || eventTurnId(frame) !== attempt.turnId || !isAttemptRunning(attempt)) return false;
    let changed = false;
    if (frame.type === 'turn_started') {
      changed = this.updateAttempt(reference, (current) => ({
        ...current,
        status: current.status === 'cancelling' ? 'cancelling' : 'streaming',
        modelLabel: current.modelLabel ?? modelLabel(frame.model),
        connectionLost: false,
      })) || changed;
    }
    const latest = this.findAttempt(reference);
    if (latest) {
      const transcript = applySessionEvent(latest.transcript, frame);
      if (transcript !== latest.transcript) {
        changed = this.updateAttempt(reference, (current) => ({ ...current, transcript })) || changed;
      }
    }
    if (frame.type === 'turn_terminal') {
      const status = typeof frame.status === 'string' ? frame.status : '';
      if (status === 'succeeded') {
        this.finishAttempt(reference, 'succeeded', null, '');
      } else if (status === 'aborted') {
        this.finishAttempt(reference, 'cancelled', null, '');
      } else {
        const message = typeof frame.message === 'string' && frame.message.trim()
          ? frame.message
          : 'The runner reported that the Broadcast turn failed.';
        this.finishAttempt(reference, 'failed', 'remote', message);
      }
      return true;
    }
    return changed;
  }

  private reconcileBroadcastCut(runnerId: string, sessionId: string, activeTurnId: string | null): boolean {
    let changed = false;
    for (const runtime of [...this.broadcastRuntimes.values()]) {
      if (runtime.runnerId !== runnerId || runtime.sessionId !== sessionId) continue;
      const reference: AttemptReference = runtime;
      const attempt = this.findAttempt(reference);
      if (!attempt || !isAttemptRunning(attempt) || !attempt.turnId) continue;
      if (attempt.turnId === activeTurnId) {
        changed = this.updateAttempt(reference, (current) => ({
          ...current,
          streamIncomplete: true,
          connectionLost: false,
        })) || changed;
      } else {
        this.finishAttempt(reference, 'interrupted', 'stream_gap', STREAM_RESTORE_ERROR);
        changed = true;
      }
    }
    return changed;
  }

  private maybeContinueRequestedCancels(runnerId: string, sessionId: string): void {
    const session = this.sessions.get(runnerSessionKey(runnerId, sessionId));
    if (!session) return;
    for (const runtime of this.broadcastRuntimes.values()) {
      if (runtime.runnerId !== runnerId || runtime.sessionId !== sessionId) continue;
      const reference: AttemptReference = runtime;
      const attempt = this.findAttempt(reference);
      if (!attempt?.cancelRequested || !attempt.turnId || !isAttemptRunning(attempt)) continue;
      if (session.activeTurnId === attempt.turnId) {
        void this.requestAbort(reference);
      } else if (session.activeTurnId !== null) {
        this.finishAttempt(reference, 'interrupted', 'protocol', CANCEL_OWNERSHIP_ERROR);
        this.invalidateAndEmit();
      }
    }
  }

  private async requestAbort(reference: AttemptReference): Promise<void> {
    const attempt = this.findAttempt(reference);
    const runtime = this.broadcastRuntimes.get(reference.attemptId);
    if (!attempt || !runtime || !attempt.cancelRequested || !attempt.turnId || !isAttemptRunning(attempt)) return;
    if (this.abortInFlight.has(reference.attemptId)) return;
    const session = this.sessions.get(runnerSessionKey(runtime.runnerId, runtime.sessionId));
    if (!session) {
      this.finishAttempt(reference, 'interrupted', 'protocol', CANCEL_OWNERSHIP_ERROR);
      this.invalidateAndEmit();
      return;
    }
    if (session.activeTurnId === null) return;
    if (session.activeTurnId !== attempt.turnId) {
      this.finishAttempt(reference, 'interrupted', 'protocol', CANCEL_OWNERSHIP_ERROR);
      this.invalidateAndEmit();
      return;
    }
    if (attempt.abortSendCount >= 2) {
      this.finishAttempt(reference, 'interrupted', 'transport_unknown', 'Cancellation could not be confirmed after reconnecting.');
      this.invalidateAndEmit();
      return;
    }

    const abortCommandId = attempt.abortCommandId ?? this.nextIdentity('abort');
    this.updateAttempt(reference, (current) => ({
      ...current,
      abortCommandId,
      abortSendCount: current.abortSendCount + 1,
    }));
    this.abortRetryPending.delete(reference.attemptId);
    this.abortInFlight.add(reference.attemptId);
    this.invalidateAndEmit();
    try {
      await this.runners.command(
        runtime.runnerId,
        { type: 'abort' },
        {
          commandId: abortCommandId,
          sourceSessionId: runtime.sessionId,
          beforeSend: () => {
            const current = this.findAttempt(reference);
            const currentSession = this.sessions.get(runnerSessionKey(runtime.runnerId, runtime.sessionId));
            if (!current || !isAttemptRunning(current) || current.turnId !== currentSession?.activeTurnId) {
              throw new RunnerCommandError(CANCEL_OWNERSHIP_ERROR, 'protocol');
            }
          },
        },
      );
      const current = this.findAttempt(reference);
      if (current && isAttemptRunning(current)) {
        this.updateAttempt(reference, (row) => ({ ...row, status: 'cancelling', connectionLost: false }));
        this.invalidateAndEmit();
      }
    } catch (error) {
      const current = this.findAttempt(reference);
      if (!current || !isAttemptRunning(current)) return;
      const kind = this.runnerErrorKind(error);
      if (kind === 'transport' && current.abortSendCount < 2) {
        this.abortRetryPending.add(reference.attemptId);
        this.updateAttempt(reference, (row) => ({ ...row, connectionLost: true }));
      } else {
        this.finishAttempt(
          reference,
          'interrupted',
          kind === 'remote' ? 'remote' : kind === 'protocol' ? 'protocol' : 'transport_unknown',
          error,
        );
      }
      this.invalidateAndEmit();
    } finally {
      this.abortInFlight.delete(reference.attemptId);
    }
  }

  private failAttempt(reference: AttemptReference, kind: Exclude<BroadcastFailureKind, 'stream_gap' | null>, error: unknown): void {
    const status: BroadcastAttemptStatus = kind === 'transport_unknown' ? 'interrupted' : 'failed';
    this.finishAttempt(reference, status, kind, error);
  }

  private finishAttempt(
    reference: AttemptReference,
    status: Extract<BroadcastAttemptStatus, 'succeeded' | 'failed' | 'cancelled' | 'interrupted'>,
    failureKind: BroadcastFailureKind,
    error: unknown,
  ): void {
    const attempt = this.findAttempt(reference);
    if (!attempt || !isAttemptRunning(attempt)) return;
    this.updateAttempt(reference, (current) => ({
      ...current,
      status,
      failureKind,
      error: error === '' ? '' : boundedError(error),
      endedAt: this.now(),
      connectionLost: false,
    }));
    this.releaseAttemptRuntime(reference.attemptId, attempt.turnId);
    this.normalizeActiveRun(reference.projectId);
    if (status === 'succeeded') this.recordRunnerReply(reference);
    this.cached = null;
    this.emit();
    if (status === 'succeeded') this.maybeHandoff(reference.projectId, reference.runId);
  }

  private appendChannelMessage(projectId: string, message: ChannelMessage): void {
    const channel = this.channels.get(projectId);
    if (!channel) return;
    this.channels.set(projectId, {
      ...channel,
      channelMessages: boundChannelMessages([...channel.channelMessages, message]),
    });
    this.cached = null;
    this.persistChannelLogs();
  }

  private persistChannelLogs(): void {
    const logs: Record<string, ChannelMessage[]> = {};
    for (const [projectId, channel] of this.channels) {
      if (channel.channelMessages.length === 0) continue;
      logs[projectId] = channel.channelMessages;
    }
    const result = saveChannelLogs(this.storage, this.pageAuthority, logs);
    if (result.ok) {
      if (this.channelLogError === '') return;
      this.channelLogError = '';
      this.cached = null;
      return;
    }
    this.channelLogError = channelLogErrorMessage(result.error);
    this.cached = null;
  }

  private wakeBriefFor(projectId: string, mappingId: string): string {
    const channel = this.requireChannel(projectId);
    const mapping = this.mappingById.get(mappingId);
    const peers = this.activeMappings(projectId)
      .map((row) => this.runnerById.get(row.runnerId))
      .filter((runner): runner is RunnerView => runner !== undefined);
    const runner = mapping ? this.runnerById.get(mapping.runnerId) : undefined;
    const selfMention = runnerMentionToken(runner ? runnerMentionName(runner, peers) : 'Runner');
    const teammateMentions: string[] = [];
    const seen = new Set<string>();
    seen.add(selfMention.toLowerCase());
    for (const row of this.activeMappings(projectId)) {
      if (row.id === mappingId) continue;
      const other = this.runnerById.get(row.runnerId);
      if (!other) continue;
      const mention = runnerMentionToken(runnerMentionName(other, peers));
      const key = mention.toLowerCase();
      if (seen.has(key)) continue;
      seen.add(key);
      teammateMentions.push(mention);
    }
    return composeWakeBrief({
      channelLabel: channel.label,
      selfMention,
      teammateMentions,
      messages: channel.channelMessages,
    });
  }

  private mentionMembers(projectId: string): ChannelMentionMember[] {
    const members: ChannelMentionMember[] = [];
    const peers = this.activeMappings(projectId)
      .map((row) => this.runnerById.get(row.runnerId))
      .filter((runner): runner is RunnerView => runner !== undefined);
    for (const mapping of this.activeMappings(projectId)) {
      const runner = this.runnerById.get(mapping.runnerId);
      if (!runner) continue;
      members.push({
        mappingId: mapping.id,
        mention: runnerMentionToken(runnerMentionName(runner, peers)),
      });
    }
    return members;
  }

  private recordRunnerReply(reference: AttemptReference): void {
    const run = this.findRun(reference.projectId, reference.runId);
    const card = run?.responses.find((response) => response.mappingId === reference.mappingId);
    const attempt = card?.attempts.find((row) => row.id === reference.attemptId);
    if (!run || !card || !attempt) return;
    const text = assistantPlainText(attempt.transcript.items);
    if (!text) return;
    this.appendChannelMessage(reference.projectId, {
      id: this.nextIdentity('msg'),
      at: this.now(),
      kind: 'runner',
      mappingId: card.mappingId,
      author: runnerMentionToken(card.runnerLabel),
      text,
    });
  }

  private maybeHandoff(projectId: string, runId: string): void {
    const run = this.findRun(projectId, runId);
    if (!run || run.isHandoff || deriveBroadcastRunStatus(run) === 'running') return;
    const channel = this.channels.get(projectId);
    if (!channel) return;
    const runIndex = channel.broadcastRuns.findIndex((row) => row.id === runId);
    if (runIndex >= 0 && runIndex < channel.broadcastRuns.length - 1) return;
    if (channel.broadcastRuns.some((row) => row.id !== runId && deriveBroadcastRunStatus(row) === 'running')) return;
    const members = this.mentionMembers(projectId);
    const exclude = new Set(run.targetMappingIds);
    const wanted: string[] = [];
    const seen = new Set<string>();
    for (const card of run.responses) {
      const attempt = card.attempts.find((row) => row.id === card.currentAttemptId);
      if (!attempt || attempt.status !== 'succeeded') continue;
      const text = assistantPlainText(attempt.transcript.items);
      if (!text) continue;
      const plan = resolveChannelRecipients(text, members);
      if (plan.kind !== 'mentioned') continue;
      for (const mappingId of plan.mappingIds) {
        if (exclude.has(mappingId) || seen.has(mappingId)) continue;
        seen.add(mappingId);
        wanted.push(mappingId);
      }
    }
    if (wanted.length === 0) return;
    const active = this.activeMappings(projectId);
    const targets = active.filter((mapping) => wanted.includes(mapping.id));
    if (targets.length === 0) return;
    void this.wakeMappings(projectId, run.prompt, targets, true);
  }

  private updateAttempt(
    reference: AttemptReference,
    update: (attempt: BroadcastAttempt) => BroadcastAttempt,
  ): boolean {
    let changed = false;
    this.replaceCard(reference.projectId, reference.runId, reference.mappingId, (card) => {
      if (card.currentAttemptId !== reference.attemptId) return card;
      const attempts = card.attempts.map((attempt) => {
        if (attempt.id !== reference.attemptId) return attempt;
        const next = update(attempt);
        if (next !== attempt) changed = true;
        return next;
      });
      if (!changed) return card;
      return { ...card, attempts: boundedAttempts(attempts) };
    });
    return changed;
  }

  private replaceCard(
    projectId: string,
    runId: string,
    mappingId: string,
    update: (card: BroadcastResponseCard) => BroadcastResponseCard,
  ): void {
    const channel = this.channels.get(projectId);
    if (!channel) return;
    let changed = false;
    const broadcastRuns = channel.broadcastRuns.map((run) => {
      if (run.id !== runId) return run;
      const responses = run.responses.map((card) => {
        if (card.mappingId !== mappingId) return card;
        const next = update(card);
        if (next !== card) changed = true;
        return next;
      });
      return changed ? { ...run, responses } : run;
    });
    if (!changed) return;
    this.channels.set(projectId, { ...channel, broadcastRuns });
    this.cached = null;
  }

  private findRun(projectId: string, runId: string): BroadcastRun | undefined {
    return this.channels.get(projectId)?.broadcastRuns.find((run) => run.id === runId);
  }

  private findAttempt(reference: AttemptReference): BroadcastAttempt | undefined {
    const card = this.findRun(reference.projectId, reference.runId)?.responses
      .find((response) => response.mappingId === reference.mappingId);
    if (!card || card.currentAttemptId !== reference.attemptId) return undefined;
    return card.attempts.find((attempt) => attempt.id === reference.attemptId);
  }

  private currentAttemptReference(projectId: string, runId: string, mappingId: string): AttemptReference {
    const card = this.findRun(projectId, runId)?.responses.find((response) => response.mappingId === mappingId);
    if (!card) throw new Error('Broadcast response was not found.');
    return { projectId, runId, mappingId, attemptId: card.currentAttemptId };
  }

  private setActiveRun(projectId: string, runId: string): void {
    const channel = this.requireChannel(projectId);
    this.channels.set(projectId, { ...channel, activeBroadcastRunId: runId });
    this.cached = null;
  }

  private normalizeActiveRun(projectId: string): void {
    const channel = this.channels.get(projectId);
    if (!channel) return;
    const current = channel.broadcastRuns.find((run) => run.id === channel.activeBroadcastRunId);
    if (current && deriveBroadcastRunStatus(current) === 'running') return;
    const activeBroadcastRunId = [...channel.broadcastRuns].reverse()
      .find((run) => deriveBroadcastRunStatus(run) === 'running')?.id ?? null;
    if (activeBroadcastRunId === channel.activeBroadcastRunId) return;
    this.channels.set(projectId, { ...channel, activeBroadcastRunId });
    this.cached = null;
  }

  private releaseAttemptRuntime(attemptId: string, turnId: string | null): void {
    const runtime = this.broadcastRuntimes.get(attemptId);
    if (runtime && turnId) this.turnAttempts.delete(broadcastTurnKey(runtime.runnerId, runtime.sessionId, turnId));
    this.broadcastRuntimes.delete(attemptId);
    this.abortRetryPending.delete(attemptId);
    this.abortInFlight.delete(attemptId);
  }

  private releaseRunRuntimes(run: BroadcastRun): void {
    for (const card of run.responses) {
      for (const attempt of card.attempts) this.releaseAttemptRuntime(attempt.id, attempt.turnId);
    }
  }

  private reconcileBroadcastAttempts(): void {
    const retryAfterReconnect: AttemptReference[] = [];
    for (const channel of this.channels.values()) {
      for (const run of channel.broadcastRuns) {
        for (const card of run.responses) {
          const attempt = currentAttempt(card);
          if (!attempt || !isAttemptRunning(attempt)) continue;
          const reference: AttemptReference = {
            projectId: channel.projectId,
            runId: run.id,
            mappingId: card.mappingId,
            attemptId: attempt.id,
          };
          const mapping = this.mappingById.get(card.mappingId);
          if (!mapping || mapping.state !== 'active' || mapping.projectId !== channel.projectId
            || mapping.runnerId !== card.runnerId || mapping.sessionId !== card.sessionId) {
            this.finishAttempt(reference, 'interrupted', 'precondition', 'The runner was removed from this Project.');
            continue;
          }
          const connected = this.runnerById.get(card.runnerId)?.state === 'connected';
          this.updateAttempt(reference, (current) => current.connectionLost === !connected
            ? current
            : { ...current, connectionLost: !connected });
          if (connected && this.abortRetryPending.has(attempt.id)) retryAfterReconnect.push(reference);
        }
      }
      this.normalizeActiveRun(channel.projectId);
    }
    for (const reference of retryAfterReconnect) void this.requestAbort(reference);
  }

  private readGoal(key: string, request: number): void {
    if (this.disposed) return;
    const state = this.sessions.get(key);
    if (!state || state.connection !== 'connected') return;
    this.goalRequests.set(key, request);
    this.runners.command(state.runnerId, { type: 'goal_get' }, { sourceSessionId: state.sessionId })
      .then((response) => {
        if (this.disposed || this.goalRequests.get(key) !== request || !this.sessions.has(key)) return;
        this.applyGoalResponse(key, response.data);
        this.emit();
      })
      .catch((error: unknown) => {
        if (this.disposed || this.goalRequests.get(key) !== request || !this.sessions.has(key)) return;
        this.patch(key, (current) => ({ ...current, goalError: boundedError(error) }));
        this.emit();
      });
  }

  private applyGoalResponse(key: string, data: unknown): void {
    const snapshot = parseChatGoalState(data);
    if (snapshot === null) {
      this.patch(key, (current) => ({ ...current, goalError: GOAL_PAYLOAD_MALFORMED }));
      return;
    }
    this.patch(key, (current) => {
      if (current.goal !== null && snapshot.revision < current.goal.revision) return current;
      return { ...current, goal: snapshot, goalError: '' };
    });
  }

  private queueRead(key: string): void {
    if (this.disposed) return;
    const state = this.sessions.get(key);
    if (!state || state.connection !== 'connected') return;
    if (this.inFlight.has(key)) {
      this.followUp.add(key);
      return;
    }
    this.dirty.add(key);
    this.pumpReads();
  }

  private patch(key: string, update: (state: TeamSessionState) => TeamSessionState): void {
    const state = this.sessions.get(key);
    if (!state) return;
    const next = update(state);
    if (next === state) return;
    this.sessions.set(key, next);
    this.cached = null;
  }

  private pumpReads(): void {
    while (this.dirty.size > 0 && this.inFlight.size < this.maxConcurrentReads) {
      let picked: string | null = null;
      for (const key of this.dirty) {
        const state = this.sessions.get(key);
        if (!state) {
          this.dirty.delete(key);
          continue;
        }
        if (state.connection === 'connected') {
          picked = key;
          break;
        }
      }
      if (picked === null) return;
      this.dirty.delete(picked);
      this.inFlight.add(picked);
      this.runRead(picked);
    }
  }

  private runRead(key: string): void {
    const state = this.sessions.get(key);
    if (!state) {
      this.inFlight.delete(key);
      return;
    }
    const jobRequest = ++this.sequence;
    this.goalRequests.set(key, ++this.sequence);
    this.patch(key, (current) => ({
      ...current,
      bootstrapped: false,
      orchestrationEnabled: null,
      refreshError: '',
      orchestration: beginOrchestrationRefresh(current.orchestration, jobRequest),
    }));
    this.emit();
    void Promise.allSettled([
      this.runners.command(state.runnerId, { type: 'goal_get' }, { sourceSessionId: state.sessionId }),
      this.runners.command(state.runnerId, { type: 'job_list' }, { sourceSessionId: state.sessionId }),
    ]).then(([goal, jobs]) => this.settleRead(key, jobRequest, goal, jobs));
  }

  private settleRead(
    key: string,
    jobRequest: number,
    goal: PromiseSettledResult<RpcResponse>,
    jobs: PromiseSettledResult<RpcResponse>,
  ): void {
    if (this.disposed) return;
    const current = this.sessions.get(key);
    if (!current) {
      this.inFlight.delete(key);
      this.followUp.delete(key);
      this.pumpReads();
      return;
    }
    if (goal.status === 'fulfilled') this.applyGoalResponse(key, goal.value.data);
    else this.patch(key, (state) => ({ ...state, goalError: boundedError(goal.reason) }));
    if (jobs.status === 'fulfilled') {
      const data = (jobs.value.data || {}) as Partial<JobListWire>;
      this.patch(key, (latest) => ({
        ...latest,
        bootstrapped: true,
        orchestrationEnabled: data.enabled === true,
        orchestration: applyAuthoritativeOrchestrationSnapshot(latest.orchestration, jobRequest, data),
      }));
    } else {
      this.patch(key, (latest) => ({
        ...latest,
        refreshError: boundedError(jobs.reason),
        orchestration: failOrchestrationRefresh(latest.orchestration, jobRequest),
      }));
    }
    const settled = this.sessions.get(key);
    const needsRepair = this.followUp.delete(key)
      || (settled !== undefined
        && settled.orchestration.requestId === null
        && settled.orchestration.needsRefresh);
    this.emit();
    this.inFlight.delete(key);
    if (needsRepair) this.queueRead(key);
    this.pumpReads();
  }

  private syncSubscriptions(states: ReadonlyMap<string, TeamSessionState>): void {
    const required = new Set<string>();
    for (const state of states.values()) required.add(state.runnerId);
    for (const [runnerId, unsubscribe] of [...this.unsubscribes]) {
      if (required.has(runnerId)) continue;
      unsubscribe();
      this.unsubscribes.delete(runnerId);
    }
    for (const runnerId of required) {
      if (this.unsubscribes.has(runnerId)) continue;
      const before = runnerEventListenerCount(runnerId);
      const unsubscribe = subscribeRunnerEvents(runnerId, (frame) => this.applyRunnerEvent(runnerId, frame));
      if (runnerEventListenerCount(runnerId) !== before + 1) {
        unsubscribe();
        this.projectionError = boundedError(`Live event listener cap reached for runner ${runnerId}; team rows keep last known state.`);
        this.cached = null;
        continue;
      }
      this.unsubscribes.set(runnerId, unsubscribe);
    }
  }

  private releaseSession(key: string): void {
    this.sessions.delete(key);
    this.sessionListeners.delete(key);
    this.inFlight.delete(key);
    this.dirty.delete(key);
    this.followUp.delete(key);
    this.goalRequests.delete(key);
    this.cached = null;
  }

  private invalidateAndEmit(): void {
    this.cached = null;
    this.emit();
  }

  private emit(): void {
    const snapshot = this.snapshot();
    for (const listener of [...this.listeners]) listener(snapshot);
  }
}
