import { safeText } from '../redact';
import { boundText } from '../transcript';
import type { EventFrame } from '../types';
import { projectTodoPhases, type TodoPhase } from './todoPanelState';

export type WorkflowStatus =
  | 'queued'
  | 'planning'
  | 'running'
  | 'paused'
  | 'integrating'
  | 'completed'
  | 'failed'
  | 'cancelled'
  | 'conflicted';

export interface WorkflowWireSnapshot {
  workflowId: string;
  name: string;
  objective: string;
  status: WorkflowStatus;
  generation: number;
  worktree?: string | null;
  branch?: string | null;
  supervisorAgentId?: string | null;
  supervisorJobId?: string | null;
  failure?: string | null;
  integration?: string | null;
}

export interface WorkflowAgentActivitySnapshot {
  atMs: number;
  kind: 'task' | 'irc';
  text: string;
}

export interface WorkflowActorSnapshot {
  name: string;
  role?: string;
  status: string;
  task?: string | null;
  taskId?: string | null;
  activity?: WorkflowAgentActivitySnapshot[];
}

export interface WorkflowActiveTaskSnapshot {
  taskId?: string | null;
  summary: string;
}

export interface WorkflowIrcSnapshot {
  sender: string;
  to?: string;
  text: string;
  atMs?: number;
  messageId?: string;
  replyTo?: string | null;
}

export interface WorkflowActivitySnapshot {
  atMs: number;
  kind: 'thinking' | 'text' | 'tool' | 'irc';
  text: string;
}

export interface WorkflowWorktreeSnapshot {
  label: string;
  branch: string;
}

export interface WorkflowIntegrationSnapshot {
  step: 'pending' | 'integrating' | 'applied' | 'conflicted';
  summary: string;
  filesChanged?: number;
  insertions?: number;
  deletions?: number;
  conflicts?: string[];
}

export interface WorkflowJobSnapshot {
  displayName: string;
  jobId: string;
  role?: string;
  status: 'queued' | 'running' | 'completed' | 'failed' | 'cancelled';
  taskSummary?: string | null;
  todoTaskId?: string | null;
  createdAtMs: number;
  startedAtMs?: number | null;
  finishedAtMs?: number | null;
  softBudgetExhausted?: boolean;
}

export interface WorkflowPanelSnapshot {
  id: string;
  generation: number;
  name: string;
  objective: string;
  status: WorkflowStatus;
  failure?: string | null;
  todo?: TodoPhase[];
  supervisor?: WorkflowActorSnapshot | null;
  subagents: WorkflowActorSnapshot[];
  jobs?: WorkflowJobSnapshot[];
  activeTasks: WorkflowActiveTaskSnapshot[];
  worktree?: WorkflowWorktreeSnapshot | null;
  integration?: WorkflowIntegrationSnapshot | null;
  planningActivity?: WorkflowActivitySnapshot[];
  planningStartedAtMs?: number | null;
  recentIrc?: WorkflowIrcSnapshot[];
}

export interface WorkflowEventPayload extends EventFrame {
  sessionId?: string;
  workflowId?: string;
  generation?: number;
  status?: WorkflowStatus;
  snapshot?: WorkflowWireSnapshot;
}

export interface WorkflowIdentity {
  sessionId: string;
  workflowId: string;
  generation: number;
}

export interface WorkflowListRequest {
  sessionId: string;
  requestId: number;
  eventRevision: number;
}

export interface WorkflowDetailRequest {
  identity: WorkflowIdentity;
  requestId: number;
}

export interface WorkflowEventApplication {
  workflows: WorkflowWireSnapshot[];
  workflowId: string;
  generation: number;
  removed: boolean;
}

const WORKFLOW_STATUSES: ReadonlySet<string> = new Set([
  'queued',
  'planning',
  'running',
  'paused',
  'integrating',
  'completed',
  'failed',
  'cancelled',
  'conflicted',
]);
const JOB_STATUSES: ReadonlySet<string> = new Set(['queued', 'running', 'completed', 'failed', 'cancelled']);
const ACTOR_LIMIT = 64;
const JOB_LIMIT = 128;
const ACTIVE_TASK_LIMIT = 128;
const RECENT_IRC_LIMIT = 50;
const ACTIVITY_LIMIT = 12;
const CONFLICT_LIMIT = 50;

function record(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

function text(value: unknown, limit: number): string {
  return boundText(safeText(value), limit);
}

function optionalText(value: unknown, limit: number): string | null {
  return typeof value === 'string' && value !== '' ? text(value, limit) : null;
}

function unsigned(value: unknown): number | null {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 ? value : null;
}

function workflowStatus(value: unknown): WorkflowStatus | null {
  return typeof value === 'string' && WORKFLOW_STATUSES.has(value) ? (value as WorkflowStatus) : null;
}

function isTerminalStatus(status: WorkflowStatus): boolean {
  return status === 'completed' || status === 'failed' || status === 'cancelled' || status === 'conflicted';
}

export function projectWorkflowListSnapshot(value: unknown): WorkflowWireSnapshot | null {
  const source = record(value);
  if (!source || typeof source.workflowId !== 'string' || source.workflowId === '') return null;
  const status = workflowStatus(source.status);
  const generation = unsigned(source.generation);
  if (!status || generation === null) return null;
  return {
    workflowId: source.workflowId,
    name: text(source.name, 160),
    objective: text(source.objective, 800),
    status,
    generation,
    worktree: optionalText(source.worktree, 240),
    branch: optionalText(source.branch, 240),
    supervisorAgentId: optionalText(source.supervisorAgentId, 160),
    supervisorJobId: optionalText(source.supervisorJobId, 160),
    failure: optionalText(source.failure, 800),
    integration: optionalText(source.integration, 800),
  };
}

function actorSnapshot(value: unknown): WorkflowActorSnapshot | null {
  const source = record(value);
  if (!source || typeof source.name !== 'string' || source.name === '') return null;
  const activity = Array.isArray(source.activity)
    ? source.activity.slice(-ACTIVITY_LIMIT).flatMap((candidate) => {
        const entry = record(candidate);
        if (!entry) return [];
        const atMs = unsigned(entry.atMs);
        const kind = entry.kind;
        if (atMs === null || (kind !== 'task' && kind !== 'irc')) return [];
        return [{ atMs, kind, text: text(entry.text, 240) } as WorkflowAgentActivitySnapshot];
      })
    : [];
  return {
    name: text(source.name, 160),
    role: optionalText(source.role, 160) ?? undefined,
    status: text(source.status, 80),
    task: optionalText(source.task, 600),
    taskId: typeof source.taskId === 'string' && source.taskId !== '' ? source.taskId : null,
    activity,
  };
}

function integrationSnapshot(value: unknown): WorkflowIntegrationSnapshot | null {
  const source = record(value);
  if (!source) return null;
  const step = source.step;
  if (step !== 'pending' && step !== 'integrating' && step !== 'applied' && step !== 'conflicted') return null;
  return {
    step,
    summary: text(source.summary, 800),
    filesChanged: unsigned(source.filesChanged) ?? undefined,
    insertions: unsigned(source.insertions) ?? undefined,
    deletions: unsigned(source.deletions) ?? undefined,
    conflicts: Array.isArray(source.conflicts)
      ? source.conflicts.slice(0, CONFLICT_LIMIT).map((conflict) => text(conflict, 240))
      : [],
  };
}

function jobSnapshot(value: unknown): WorkflowJobSnapshot | null {
  const source = record(value);
  if (!source) return null;
  const createdAtMs = unsigned(source.createdAtMs);
  const status = source.status;
  if (
    typeof source.jobId !== 'string' ||
    source.jobId === '' ||
    typeof status !== 'string' ||
    !JOB_STATUSES.has(status) ||
    createdAtMs === null
  ) return null;
  return {
    displayName: text(source.displayName, 160),
    jobId: source.jobId,
    role: optionalText(source.role, 160) ?? undefined,
    status: status as WorkflowJobSnapshot['status'],
    taskSummary: optionalText(source.taskSummary, 600),
    todoTaskId: typeof source.todoTaskId === 'string' && source.todoTaskId !== '' ? source.todoTaskId : null,
    createdAtMs,
    startedAtMs: unsigned(source.startedAtMs),
    finishedAtMs: unsigned(source.finishedAtMs),
    softBudgetExhausted: source.softBudgetExhausted === true,
  };
}

export function projectWorkflowDetail(value: unknown): WorkflowPanelSnapshot | null {
  const source = record(value);
  if (!source) return null;
  const id = source.id;
  const generation = unsigned(source.generation);
  const status = workflowStatus(source.status);
  if (typeof id !== 'string' || id === '' || generation === null || !status) return null;
  const supervisor = source.supervisor == null ? null : actorSnapshot(source.supervisor);
  const subagents = Array.isArray(source.subagents)
    ? source.subagents.slice(0, ACTOR_LIMIT).flatMap((candidate) => {
        const actor = actorSnapshot(candidate);
        return actor ? [actor] : [];
      })
    : [];
  const jobs = Array.isArray(source?.jobs)
    ? source.jobs.slice(0, JOB_LIMIT).flatMap((candidate) => {
        const job = jobSnapshot(candidate);
        return job ? [job] : [];
      })
    : [];
  const activeTasks = Array.isArray(source?.activeTasks)
    ? source.activeTasks.slice(0, ACTIVE_TASK_LIMIT).flatMap((candidate) => {
        const task = record(candidate);
        if (!task || typeof task.summary !== 'string' || task.summary === '') return [];
        return [{
          taskId: typeof task.taskId === 'string' && task.taskId !== '' ? task.taskId : null,
          summary: text(task.summary, 600),
        } as WorkflowActiveTaskSnapshot];
      })
    : [];
  const planningActivity = Array.isArray(source?.planningActivity)
    ? source.planningActivity.slice(-ACTIVITY_LIMIT).flatMap((candidate) => {
        const entry = record(candidate);
        if (!entry) return [];
        const atMs = unsigned(entry.atMs);
        const kind = entry.kind;
        if (atMs === null || (kind !== 'thinking' && kind !== 'text' && kind !== 'tool' && kind !== 'irc')) return [];
        return [{ atMs, kind, text: text(entry.text, 240) } as WorkflowActivitySnapshot];
      })
    : [];
  const recentIrc = Array.isArray(source?.recentIrc)
    ? source.recentIrc.slice(-RECENT_IRC_LIMIT).flatMap((candidate) => {
        const message = record(candidate);
        if (!message || typeof message.sender !== 'string' || typeof message.text !== 'string') return [];
        return [{
          sender: text(message.sender, 160),
          to: optionalText(message.to, 160) ?? undefined,
          text: text(message.text, 600),
          atMs: unsigned(message.atMs) ?? undefined,
          messageId: typeof message.messageId === 'string' && message.messageId !== '' ? message.messageId : undefined,
          replyTo: typeof message.replyTo === 'string' && message.replyTo !== '' ? message.replyTo : null,
        } as WorkflowIrcSnapshot];
      })
    : [];
  const worktree = record(source.worktree);
  return {
    id,
    generation,
    name: text(source.name, 160),
    objective: text(source.objective, 800),
    status,
    failure: optionalText(source.failure, 800),
    supervisor,
    subagents,
    jobs,
    activeTasks,
    worktree: worktree ? { label: text(worktree.label, 240), branch: text(worktree.branch, 240) } : null,
    todo: projectTodoPhases(source.todo),
    integration: source.integration == null ? null : integrationSnapshot(source.integration),
    planningActivity,
    planningStartedAtMs: unsigned(source.planningStartedAtMs),
    recentIrc,
  };
}

export function workflowIdentity(
  sessionId: string,
  workflow: Pick<WorkflowWireSnapshot, 'workflowId' | 'generation'>,
): WorkflowIdentity {
  return { sessionId, workflowId: workflow.workflowId, generation: workflow.generation };
}

/** Bound a wire error for the panel's single error row — the same 800-char
 *  budget as every other model-derived string in this panel. */
export function workflowActionError(err: unknown): string {
  return boundText(safeText(err instanceof Error ? err.message : String(err)), 800);
}

export function workflowCommand(sessionId: string, command: Record<string, unknown>): Record<string, unknown> {
  return { ...command, sessionId };
}

export function workflowActorNavigationKey(identity: WorkflowIdentity, actorName: string): string {
  return JSON.stringify([identity.sessionId, identity.workflowId, identity.generation, actorName]);
}


export class WorkflowPanelAuthority {
  readonly sessionId: string;
  private listRequestId = 0;
  private detailRequestId = 0;
  /** Workflows with an admitted lifecycle action (cancel/remove/…). While a
   *  workflow is busy, no poll/effect may restart its detail stream. */
  private readonly actionBusyByWorkflow = new Set<string>();
  private eventRevision = 0;
  private lastEventSequence = 0;
  private lastUnsequencedEventKey = '';
  private readonly generationByWorkflow = new Map<string, number>();
  private readonly tombstoneByWorkflow = new Map<string, number>();
  private readonly revisionByWorkflow = new Map<string, number>();

  constructor(sessionId: string) {
    this.sessionId = sessionId;
  }

  beginList(): WorkflowListRequest {
    return { sessionId: this.sessionId, requestId: ++this.listRequestId, eventRevision: this.eventRevision };
  }
  isCurrentList(request: WorkflowListRequest): boolean {
    return request.sessionId === this.sessionId && request.requestId === this.listRequestId;
  }


  acceptList(
    request: WorkflowListRequest,
    previous: WorkflowWireSnapshot[],
    value: unknown,
  ): WorkflowWireSnapshot[] | null {
    if (request.sessionId !== this.sessionId || request.requestId !== this.listRequestId) return null;
    const payload = record(value);
    const values = Array.isArray(payload?.workflows) ? payload.workflows : [];
    const responseById = new Map<string, WorkflowWireSnapshot>();
    const responseOrder: string[] = [];
    for (const candidate of values) {
      const snapshot = projectWorkflowListSnapshot(candidate);
      if (!snapshot) continue;
      const current = responseById.get(snapshot.workflowId);
      if (!current) responseOrder.push(snapshot.workflowId);
      if (!current || snapshot.generation >= current.generation) responseById.set(snapshot.workflowId, snapshot);
    }
    const previousById = new Map(previous.map((snapshot) => [snapshot.workflowId, snapshot]));
    const next: WorkflowWireSnapshot[] = [];
    const used = new Set<string>();
    for (const workflowId of responseOrder) {
      const changedAfterRequest = (this.revisionByWorkflow.get(workflowId) ?? 0) > request.eventRevision;
      const response = responseById.get(workflowId);
      const previousSnapshot = previousById.get(workflowId);
      const candidate = changedAfterRequest ? previousSnapshot : response;
      if (
        previousSnapshot &&
        response &&
        isTerminalStatus(previousSnapshot.status) &&
        previousSnapshot.status !== response.status
      ) continue;
      if (!candidate || !this.acceptsGeneration(workflowId, candidate.generation)) continue;
      next.push(candidate);
      used.add(workflowId);
      this.rememberGeneration(workflowId, candidate.generation);
    }
    for (const candidate of previous) {
      if (used.has(candidate.workflowId)) continue;
      const changedAfterRequest = (this.revisionByWorkflow.get(candidate.workflowId) ?? 0) > request.eventRevision;
      if (!changedAfterRequest || !this.acceptsGeneration(candidate.workflowId, candidate.generation)) continue;
      next.push(candidate);
      this.rememberGeneration(candidate.workflowId, candidate.generation);
    }
    return next;
  }

  /** SINGLE PHYSICAL detail RPC on the wire. Invalidation (cancel/select/
   *  action) NEVER frees this slot — it only makes the open request's
   *  response stale; the slot frees exclusively when the physical promise
   *  settles. */
  private detailInFlight: { key: string; requestId: number } | null = null;
  /** Latest desired exact identity recorded while the physical slot is open
   *  for a different identity; launched at most once on physical settle. */
  private desiredDetail: WorkflowIdentity | null = null;

  /** Detail admission. An identical sessionId+workflowId+generation already
   *  on the wire coalesces (returns null); a different identity records the
   *  one latest desired identity and waits for the physical settle. A removed
   *  or action-gated workflow is never admitted. */
  beginDetail(identity: WorkflowIdentity): WorkflowDetailRequest | null {
    if (this.isRemoved(identity.workflowId) || this.isActionBusy(identity.workflowId)) return null;
    const key = JSON.stringify([identity.sessionId, identity.workflowId, identity.generation]);
    if (this.detailInFlight) {
      // A LIVE physical for the same exact identity coalesces. A STALE
      // physical (its response was invalidated by cancel/action) or a
      // different identity records the one latest desired identity — the
      // held response will be discarded, so a fresh one is wanted.
      if (this.detailInFlight.key === key && this.detailInFlight.requestId === this.detailRequestId) return null;
      this.desiredDetail = identity;
      return null;
    }
    const requestId = ++this.detailRequestId;
    this.detailInFlight = { key, requestId };
    return { identity, requestId };
  }
  /** Physical settle (the promise resolved or rejected — any outcome):
   *  release the on-wire slot and return the latest desired identity to
   *  launch (at most one), or null. Invalidation never reaches here — only
   *  the owner of the physical request settles it. */
  settleDetail(request: WorkflowDetailRequest): WorkflowIdentity | null {
    if (!this.detailInFlight || this.detailInFlight.requestId !== request.requestId) return null;
    this.detailInFlight = null;
    const desired = this.desiredDetail;
    this.desiredDetail = null;
    if (desired && !this.isRemoved(desired.workflowId) && !this.isActionBusy(desired.workflowId)) return desired;
    return null;
  }

  /** Acceptance invalidation only: the open physical request's response
   *  becomes stale and the recorded desired identity is voided. The physical
   *  slot stays held until its own promise settles. */
  cancelDetail(): void {
    this.detailRequestId += 1;
    this.desiredDetail = null;
  }
  isCurrentDetail(request: WorkflowDetailRequest): boolean {
    return request.identity.sessionId === this.sessionId && request.requestId === this.detailRequestId;
  }

  /** Lifecycle-action admission: synchronously invalidates the detail
   *  authority (any in-flight detail response becomes stale) and marks the
   *  workflow busy so no poll/effect restarts detail until settlement. */
  beginAction(workflowId: string): void {
    this.actionBusyByWorkflow.add(workflowId);
    this.cancelDetail();
  }
  endAction(workflowId: string): void {
    this.actionBusyByWorkflow.delete(workflowId);
  }
  isActionBusy(workflowId: string): boolean {
    return this.actionBusyByWorkflow.has(workflowId);
  }

  /** Remove is terminal: the identity can never be detailed or listed again.
   *  An absolute tombstone rejects every future generation (ids are never
   *  reused), so stale list snapshots and events cannot resurrect it. */
  tombstoneWorkflow(workflowId: string): void {
    this.tombstoneByWorkflow.set(workflowId, Number.MAX_SAFE_INTEGER);
  }
  /** True once an authoritative removal (explicit success or a
   *  workflow_removed event) landed for this id. */
  isRemoved(workflowId: string): boolean {
    return this.tombstoneByWorkflow.has(workflowId);
  }

  acceptDetail(
    request: WorkflowDetailRequest,
    selected: WorkflowIdentity | null,
    value: unknown,
  ): WorkflowPanelSnapshot | null {
    if (
      request.requestId !== this.detailRequestId ||
      request.identity.sessionId !== this.sessionId ||
      this.isRemoved(request.identity.workflowId) ||
      !selected ||
      selected.sessionId !== this.sessionId ||
      selected.workflowId !== request.identity.workflowId
    ) return null;
    const detail = projectWorkflowDetail(value);
    if (!detail || detail.id !== request.identity.workflowId) return null;
    const minimumGeneration = Math.max(request.identity.generation, selected.generation);
    if (detail.generation < minimumGeneration || !this.acceptsGeneration(detail.id, detail.generation)) return null;
    this.rememberGeneration(detail.id, detail.generation);
    return detail;
  }

  applyEvent(frame: WorkflowEventPayload, previous: WorkflowWireSnapshot[]): WorkflowEventApplication | null {
    if (frame.sessionId !== this.sessionId) return null;
    const sequence = typeof frame.sequence === 'number' && Number.isSafeInteger(frame.sequence)
      ? frame.sequence
      : null;
    if (sequence !== null && sequence <= this.lastEventSequence) return null;
    const workflowId = typeof frame.workflowId === 'string' ? frame.workflowId : '';
    const generation = unsigned(frame.generation);
    if (!workflowId || generation === null || !this.acceptsGeneration(workflowId, generation)) return null;
    const unsequencedKey = sequence === null
      ? JSON.stringify([frame.type, workflowId, generation, frame.status ?? null, frame.snapshot ?? null])
      : '';
    if (unsequencedKey !== '' && unsequencedKey === this.lastUnsequencedEventKey) return null;
    let workflows = previous;
    let removed = false;
    switch (frame.type) {
      case 'workflow_removed':
        // Absolute tombstone: workflow ids are never reused, so NO later
        // generation may resurrect the workflow from a stale list snapshot
        // or a late event.
        this.tombstoneWorkflow(workflowId);
        workflows = previous.filter((snapshot) => snapshot.workflowId !== workflowId);
        removed = true;
        break;
      case 'workflow_status_changed': {
        const status = workflowStatus(frame.status);
        if (!status) return null;
        const current = previous.find((snapshot) => snapshot.workflowId === workflowId);
        if (current && isTerminalStatus(current.status) && current.status !== status) return null;
        workflows = previous.map((snapshot) => snapshot.workflowId === workflowId
          ? {
              ...snapshot,
              generation,
              status,
              name: typeof frame.name === 'string' ? text(frame.name, 160) : snapshot.name,
            }
          : snapshot);
        break;
      }
      case 'workflow_updated': {
        const snapshot = projectWorkflowListSnapshot(frame.snapshot);
        if (!snapshot || snapshot.workflowId !== workflowId || snapshot.generation !== generation) return null;
        const current = previous.find((candidate) => candidate.workflowId === workflowId);
        if (current && isTerminalStatus(current.status) && current.status !== snapshot.status) return null;
        const exists = Boolean(current);
        workflows = exists
          ? previous.map((candidate) => candidate.workflowId === workflowId ? snapshot : candidate)
          : [snapshot, ...previous];
        break;
      }
      default:
        return null;
    }
    this.eventRevision += 1;
    this.revisionByWorkflow.set(workflowId, this.eventRevision);
    this.rememberGeneration(workflowId, generation);
    if (sequence !== null) this.lastEventSequence = sequence;
    else this.lastUnsequencedEventKey = unsequencedKey;
    return { workflows, workflowId, generation, removed };
  }

  private acceptsGeneration(workflowId: string, generation: number): boolean {
    const tombstone = this.tombstoneByWorkflow.get(workflowId);
    if (tombstone !== undefined && generation <= tombstone) return false;
    const known = this.generationByWorkflow.get(workflowId);
    return known === undefined || generation >= known;
  }

  private rememberGeneration(workflowId: string, generation: number): void {
    const known = this.generationByWorkflow.get(workflowId) ?? 0;
    if (generation > known) this.generationByWorkflow.set(workflowId, generation);
  }
}
