export type JobStatus = 'queued' | 'running' | 'completed' | 'failed' | 'cancelled';
export type SubagentJobFilter = 'running' | 'completed';

export const SUBAGENT_FILTER_LABEL: Record<SubagentJobFilter, string> = {
  running: 'Running',
  completed: 'Completed',
};

export function subagentJobBucket(status: JobStatus): SubagentJobFilter {
  return status === 'queued' || status === 'running' ? 'running' : 'completed';
}

export interface JobWire {
  id: string;
  agentId: string;
  agent: string;
  parentId: string;
  description?: string | null;
  status: JobStatus;
  createdAt: number;
  startedAt?: number | null;
  finishedAt?: number | null;
}

const JOB_STATUSES: Record<string, JobStatus> = {
  queued: 'queued',
  running: 'running',
  completed: 'completed',
  failed: 'failed',
  cancelled: 'cancelled',
};

function asRecord(value: unknown): Record<string, unknown> | null {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null;
  return value as Record<string, unknown>;
}

function stringField(value: unknown): string {
  return typeof value === 'string' ? value : '';
}

function optionalNumber(value: unknown): number | null {
  if (typeof value !== 'number' || !Number.isFinite(value)) return null;
  return value;
}

export function formatDuration(milliseconds: number): string {
  if (milliseconds < 1000) return `${milliseconds}ms`;
  const seconds = Math.floor(milliseconds / 1000);
  if (seconds < 60) return `${seconds}s`;
  return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
}

export function elapsedMs(job: JobWire, now: number): number | null {
  if (job.status === 'queued' || job.status === 'running') {
    if (now === 0) return null;
    const start = job.startedAt ?? job.createdAt;
    return Math.max(0, now - start);
  }
  if (job.finishedAt != null && job.startedAt != null) {
    return Math.max(0, job.finishedAt - job.startedAt);
  }
  return null;
}

export function parseJobWire(raw: unknown): JobWire | null {
  const rec = asRecord(raw);
  if (!rec) return null;
  const id = stringField(rec.id);
  if (!id) return null;
  const statusRaw = stringField(rec.status);
  const status = JOB_STATUSES[statusRaw];
  if (!status) return null;
  const createdAt = optionalNumber(rec.createdAt);
  if (createdAt === null) return null;
  return {
    id,
    agentId: stringField(rec.agentId),
    agent: stringField(rec.agent),
    parentId: stringField(rec.parentId),
    description: typeof rec.description === 'string' ? rec.description : null,
    status,
    createdAt,
    startedAt: optionalNumber(rec.startedAt),
    finishedAt: optionalNumber(rec.finishedAt),
  };
}

export function parseJobList(data: unknown): { enabled: boolean; jobs: JobWire[] } | null {
  const rec = asRecord(data);
  if (!rec || !Array.isArray(rec.jobs)) return null;
  const jobs: JobWire[] = [];
  for (const row of rec.jobs) {
    const job = parseJobWire(row);
    if (job) jobs.push(job);
  }
  return { enabled: rec.enabled === true, jobs };
}

export function applyJobUpdated(jobs: JobWire[], frame: unknown): JobWire[] {
  const rec = asRecord(frame);
  if (!rec || rec.type !== 'job_updated') return jobs;
  const next = parseJobWire(rec.job ?? rec);
  if (!next) return jobs;
  const index = jobs.findIndex((job) => job.id === next.id);
  if (index < 0) return [...jobs, next];
  const copy = jobs.slice();
  copy[index] = next;
  return copy;
}
