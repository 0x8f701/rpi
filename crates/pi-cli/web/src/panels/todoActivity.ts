import { safeText } from '../redact';
import { boundText } from '../transcript';
import type { JobStatus } from '../jobCard';

export type ActivityPanelPage = 'recent' | 'settled';
export type ActivityKind = 'job' | 'message';

export interface ActivityEntry {
  id: string;
  sourceId: string;
  actorId?: string;
  sessionId: string;
  sequence: number;
  kind: ActivityKind;
  actor: string;
  summary: string;
  status: JobStatus | 'message';
  atMs: number;
}

export interface ActivityNavigationState {
  page: ActivityPanelPage;
}

export function activityEntryMatchesPage(entry: ActivityEntry, page: ActivityPanelPage): boolean {
  const isSettled = entry.status === 'completed' || entry.status === 'failed' || entry.status === 'cancelled';
  return page === 'recent' ? !isSettled : isSettled;
}

export interface ActivitySessionSnapshot {
  sessionId: string;
  entries: ActivityEntry[];
  sequence: number;
  actors?: Record<string, string>;
}

export interface ActivityEventFrame {
  type: 'job_updated' | 'agent_updated' | 'message_delivered';
  sessionId?: unknown;
  sequence?: unknown;
  job?: unknown;
  agent?: unknown;
  message?: unknown;
}

export const ACTIVITY_LIMIT = 200;
const JOB_STATUSES: ReadonlySet<string> = new Set(['queued', 'running', 'completed', 'failed', 'cancelled']);

function record(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

function text(value: unknown, limit: number): string {
  return boundText(safeText(value), limit);
}

function unsigned(value: unknown): number | null {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 ? value : null;
}

function jobEntry(sessionId: string, sequence: number, value: unknown, actors: Record<string, string>): ActivityEntry | null {
  const source = record(value);
  const status = source?.status;
  if (
    !source ||
    typeof source.id !== 'string' ||
    source.id === '' ||
    typeof status !== 'string' ||
    !JOB_STATUSES.has(status)
  ) return null;
  const result = record(source.result);
  const actorId = typeof source.agentId === 'string' && source.agentId !== '' ? source.agentId : undefined;
  const actor = (actorId ? actors[actorId] : '') || text(source.agent, 160) || 'subagent';
  const description = text(source.description, 600);
  const output = text(result?.output, 600);
  const isSettled = status === 'completed' || status === 'failed' || status === 'cancelled';
  const summary = (isSettled ? output || description : description || output) || status;
  const atMs = unsigned(source.finishedAt) ?? unsigned(source.startedAt) ?? unsigned(source.createdAt) ?? sequence;
  return {
    id: `${sessionId}:${sequence}:job:${source.id}`,
    sourceId: source.id,
    actorId,
    sessionId,
    sequence,
    kind: 'job',
    actor,
    summary,
    status: status as JobStatus,
    atMs,
  };
}

function messageEntry(sessionId: string, sequence: number, value: unknown): ActivityEntry | null {
  const source = record(value);
  if (!source || typeof source.id !== 'string' || source.id === '' || typeof source.body !== 'string') return null;
  const from = text(source.from, 160) || 'subagent';
  const to = text(source.to, 160);
  return {
    id: `${sessionId}:${sequence}:message:${source.id}`,
    sourceId: source.id,
    sessionId,
    sequence,
    kind: 'message',
    actor: to ? `${from} → ${to}` : from,
    summary: text(source.body, 600),
    status: 'message',
    atMs: unsigned(source.timestamp) ?? sequence,
  };
}

export function activityNavigationKey(sessionId: string): string {
  return JSON.stringify(['ordinary-activity', sessionId]);
}

export function applyActivityEvent(
  snapshot: ActivitySessionSnapshot,
  frame: ActivityEventFrame,
): ActivitySessionSnapshot {
  if (frame.sessionId !== snapshot.sessionId) return snapshot;
  const sequence = typeof frame.sequence === 'number' && Number.isSafeInteger(frame.sequence)
    ? frame.sequence
    : 0;
  if (sequence <= snapshot.sequence) return snapshot;
  const actors = { ...(snapshot.actors ?? {}) };
  if (frame.type === 'agent_updated') {
    const agent = record(frame.agent);
    if (agent && typeof agent.id === 'string' && agent.id !== '') {
      const displayName = text(agent.displayName, 160);
      if (displayName) actors[agent.id] = displayName;
      const entries = snapshot.entries.map((candidate) =>
        candidate.actorId === agent.id && displayName ? { ...candidate, actor: displayName } : candidate,
      );
      return { sessionId: snapshot.sessionId, entries, sequence, actors };
    }
    return { ...snapshot, sequence, actors };
  }
  const entry = frame.type === 'job_updated'
    ? jobEntry(snapshot.sessionId, sequence, frame.job, actors)
    : messageEntry(snapshot.sessionId, sequence, frame.message);
  if (!entry) return { ...snapshot, sequence, actors };
  const withoutPrevious = entry.kind === 'job'
    ? snapshot.entries.filter((candidate) => candidate.kind !== 'job' || candidate.sourceId !== entry.sourceId)
    : snapshot.entries;
  const entries = [...withoutPrevious, entry].sort(
    (left, right) => left.atMs - right.atMs || left.sequence - right.sequence,
  );
  return {
    sessionId: snapshot.sessionId,
    entries: entries.length > ACTIVITY_LIMIT ? entries.slice(entries.length - ACTIVITY_LIMIT) : entries,
    sequence,
    actors,
  };
}
