// Job-card elapsed projection mirroring crates/pi-coding job_card_adapter.
// Kept React-free so the running-details first-render contract is directly
// testable: callers pass a real clock instead of waiting for the panel ticker.

export type JobStatus = 'queued' | 'running' | 'completed' | 'failed' | 'cancelled';

/** Subagents panel list filter: live work (queued/running) vs settled history. */
export type SubagentJobFilter = 'running' | 'completed';

export const SUBAGENT_FILTER_LABEL: Record<SubagentJobFilter, string> = {
  running: 'Running',
  completed: 'Completed',
};

/**
 * Bucket a job status into the panel filter it belongs to. queued+running are
 * "in flight" (a just-spawned job must be visible under Running), everything
 * else is settled history under Completed.
 */
export function subagentJobBucket(status: JobStatus): SubagentJobFilter {
  return status === 'queued' || status === 'running' ? 'running' : 'completed';
}



export interface JobResultWire {
  index: number;
  id: string;
  agent: string;
  status: string;
  output: string;
  usage: { promptTokens?: number; completionTokens?: number; totalTokens?: number };
  softBudgetExhausted?: boolean;
  error?: string | null;
  structuredOutput?: unknown;
  artifactRef: string;
  historyRef: string;
  artifactUri: string;
}

export interface JobWire {
  id: string;
  agentId: string;
  agent: string;
  parentId: string;
  description?: string | null;
  todoTaskId?: string | null;
  workflowId?: string | null;
  workflowGeneration?: number | null;
  status: JobStatus;
  createdAt: number;
  startedAt?: number | null;
  finishedAt?: number | null;
  result?: JobResultWire | null;
  softBudgetExhausted?: boolean;
}

export function formatDuration(milliseconds: number): string {
  if (milliseconds < 1000) return `${milliseconds}ms`;
  const seconds = Math.floor(milliseconds / 1000);
  if (seconds < 60) return `${seconds}s`;
  return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
}

/** Mirror of job_card_adapter::elapsed_ms: live wall-clock while active. */
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
