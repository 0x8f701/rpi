export type JobStatus = 'queued' | 'running' | 'cancelling' | 'completed' | 'failed' | 'cancelled';
export type SubagentJobFilter = 'running' | 'completed';

export const SUBAGENT_FILTER_LABEL: Record<SubagentJobFilter, string> = {
  running: 'Running',
  completed: 'Completed',
};

/**
 * Live execution phases. `cancelling` has committed cancellation intent but
 * the runner still owns abort/drain/cleanup, so it stays active: elapsed,
 * history polling, and the Running filter. It is not an alias of `running`.
 * Terminal statuses are history.
 */
export function isLiveJobStatus(status: JobStatus): boolean {
  return status === 'queued' || status === 'running' || status === 'cancelling';
}

/** Activity attachment rank. Cancelling outranks running; settled is zero. */
export function jobStatusRank(status: JobStatus): number {
  switch (status) {
    case 'cancelling':
      return 3;
    case 'running':
      return 2;
    case 'queued':
      return 1;
    default:
      return 0;
  }
}

/** Display label. Only `cancelling` is titled; other statuses stay wire tokens. */
export function jobStatusLabel(status: JobStatus): string {
  return status === 'cancelling' ? 'Cancelling' : status;
}

/**
 * Bucket a job status into the panel filter it belongs to. queued, running,
 * and cancelling are in flight (a just-spawned or draining job must stay
 * visible under Running); everything else is settled history under Completed.
 */
export function subagentJobBucket(status: JobStatus): SubagentJobFilter {
  return isLiveJobStatus(status) ? 'running' : 'completed';
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
  cancelling: 'cancelling',
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
  if (isLiveJobStatus(job.status)) {
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
