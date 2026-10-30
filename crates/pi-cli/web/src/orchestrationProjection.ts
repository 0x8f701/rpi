// Shared orchestration snapshot/event projection.
// Extracted from SubagentsPanel so /web and /chat share one request/event
// race, JobView activity attachment, and group-identity convention.
//
// `job_updated` replaces JobView.job and retains activity/activityAt.
// A unique `message_delivered` attaches safeText(body)/timestamp to the
// status-ranked JobView whose job.agentId matches message.from.
// An authoritative snapshot creates JobView rows and preserves prior
// activity/activityAt for each retained job identifier.

import { safeText } from './redact';
import type { JobStatus, JobWire } from './jobCard';
import type { EventFrame } from './types';

export const MAX_ORCHESTRATION_EVENT_BUFFER = 128;
export const MAX_ORCHESTRATION_MESSAGES = 64;

export interface AgentWire {
  id: string;
  displayName: string;
  agent: string;
  parentId?: string | null;
  status: string;
  createdAt: number;
  lastActivity: number;
  unread: number;
  artifactRef?: string | null;
  historyRef?: string | null;
}

export interface MailboxMessageWire {
  id: string;
  from: string;
  to: string;
  body: string;
  timestamp: number;
  replyTo?: string | null;
}

export interface CatalogAgentWire {
  name: string;
  description: string;
}

export interface JobListWire {
  enabled: boolean;
  groupId: string | null;
  jobs: JobWire[];
  agents: AgentWire[];
  messages: MailboxMessageWire[];
  catalog: CatalogAgentWire[];
}

export interface JobView {
  job: JobWire;
  /** Latest child -> Main IRC body attached as the live activity one-liner. */
  activity: string | null;
  activityAt: number | null;
}

export interface OrchestrationProjection {
  groupId: string | null;
  jobs: Record<string, JobView>;
  agents: Record<string, AgentWire>;
  messages: MailboxMessageWire[];
  buffered: EventFrame[];
  requestId: number | null;
  needsRefresh: boolean;
}

export function initialOrchestrationProjection(): OrchestrationProjection {
  return {
    groupId: null,
    jobs: {},
    agents: {},
    messages: [],
    buffered: [],
    requestId: null,
    needsRefresh: false,
  };
}

/** snake_case `group_id` first; camelCase `groupId` fallback; missing is mismatch. */
export function normalizedEventGroup(frame: EventFrame): string {
  return typeof frame.group_id === 'string'
    ? frame.group_id
    : typeof frame.groupId === 'string'
      ? frame.groupId
      : '';
}

function statusRank(status: JobStatus): number {
  switch (status) {
    case 'running':
      return 2;
    case 'queued':
      return 1;
    default:
      return 0;
  }
}

function bufferOrchestrationEvent(
  state: OrchestrationProjection,
  frame: EventFrame,
): OrchestrationProjection {
  const groupId = normalizedEventGroup(frame);
  if (groupId === '') return state;
  const buffered = [...state.buffered, frame];
  if (buffered.length <= MAX_ORCHESTRATION_EVENT_BUFFER) {
    return { ...state, buffered };
  }
  return {
    ...state,
    buffered: buffered.slice(-MAX_ORCHESTRATION_EVENT_BUFFER),
    needsRefresh: true,
  };
}

export function applyOrchestrationEvent(
  state: OrchestrationProjection,
  frame: EventFrame,
): OrchestrationProjection {
  const groupId = normalizedEventGroup(frame);
  if (state.groupId === null || state.requestId !== null) {
    return bufferOrchestrationEvent(state, frame);
  }
  if (groupId !== state.groupId) {
    return { ...state, needsRefresh: true };
  }
  switch (frame.type) {
    case 'job_updated': {
      const job = frame.job as unknown as JobWire | undefined;
      if (!job) return state;
      const existing = state.jobs[job.id];
      return {
        ...state,
        jobs: {
          ...state.jobs,
          [job.id]: {
            job,
            activity: existing?.activity ?? null,
            activityAt: existing?.activityAt ?? null,
          },
        },
      };
    }
    case 'agent_updated': {
      const agent = frame.agent as unknown as AgentWire | undefined;
      return agent ? { ...state, agents: { ...state.agents, [agent.id]: agent } } : state;
    }
    case 'message_delivered': {
      const message = frame.message as unknown as MailboxMessageWire | undefined;
      if (!message || state.messages.some((existing) => existing.id === message.id)) return state;
      const messages = [...state.messages, message].slice(-MAX_ORCHESTRATION_MESSAGES);
      const match = Object.values(state.jobs)
        .filter((view) => view.job.agentId === message.from)
        .sort((a, b) => statusRank(b.job.status) - statusRank(a.job.status))[0];
      if (!match) return { ...state, messages };
      return {
        ...state,
        messages,
        jobs: {
          ...state.jobs,
          [match.job.id]: {
            ...match,
            activity: safeText(message.body),
            activityAt: message.timestamp,
          },
        },
      };
    }
    default:
      return state;
  }
}

export function beginOrchestrationRefresh(
  state: OrchestrationProjection,
  requestId: number,
): OrchestrationProjection {
  return { ...state, requestId, needsRefresh: false };
}

export function failOrchestrationRefresh(
  state: OrchestrationProjection,
  requestId: number,
): OrchestrationProjection {
  if (state.requestId !== requestId) return state;
  let next: OrchestrationProjection = { ...state, requestId: null, buffered: [] };
  for (const frame of state.buffered) next = applyOrchestrationEvent(next, frame);
  return next;
}

export function applyAuthoritativeOrchestrationSnapshot(
  state: OrchestrationProjection,
  requestId: number,
  data: Partial<JobListWire>,
): OrchestrationProjection {
  if (state.requestId !== requestId) return state;
  const groupId = typeof data.groupId === 'string' ? data.groupId : '';
  const jobs: Record<string, JobView> = {};
  for (const job of data.jobs || []) {
    const existing = state.jobs[job.id];
    jobs[job.id] = {
      job,
      activity: existing?.activity ?? null,
      activityAt: existing?.activityAt ?? null,
    };
  }
  const agents: Record<string, AgentWire> = {};
  for (const agent of data.agents || []) agents[agent.id] = agent;
  let next: OrchestrationProjection = {
    groupId: groupId === '' ? null : groupId,
    jobs: groupId === '' ? {} : jobs,
    agents: groupId === '' ? {} : agents,
    messages: groupId === '' ? [] : (data.messages || []).slice(-MAX_ORCHESTRATION_MESSAGES),
    buffered: [],
    requestId: null,
    needsRefresh: state.needsRefresh,
  };
  if (groupId !== '') {
    for (const frame of state.buffered) {
      if (normalizedEventGroup(frame) !== groupId) continue;
      next = applyOrchestrationEvent(next, frame);
    }
  }
  return next;
}
