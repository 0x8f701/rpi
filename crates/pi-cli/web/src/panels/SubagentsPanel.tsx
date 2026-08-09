// Subagents panel (D93) — live mirror of the TUI job cards + hub surface.
//
// Wire shapes mirror pi-coding (serde camelCase) — see
// crates/pi-coding/src/orchestration/{jobs,runtime}.rs. Every model-derived
// string passes through safeText() before display; nothing is injected as raw
// HTML.
//
// RPC surface (crates/pi-cli/src/modes/rpc.rs):
//   job_list     -> { enabled, jobs, agents, messages, catalog }
//   task_spawn   -> { spawns }  (args mirror the `task` tool wire shape)
//   hub_send     -> { receipts }
//   job_cancel   -> { cancelled }
//   job_output   -> { job }
//
// Live updates arrive as orchestration events over the same WS:
//   job_updated / agent_updated / message_delivered
// (ApplicationEvent::Orchestration, serde tag "type", snake_case).

import { useEffect, useMemo, useRef, useState } from 'react';
import { safeText } from '../redact';
import type { EventFrame } from '../types';

export type JobStatus = 'queued' | 'running' | 'completed' | 'failed' | 'cancelled';

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

interface CatalogAgentWire {
  name: string;
  description: string;
}

interface SubagentsPanelProps {
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>;
  /** App-owned subscription: returns an unsubscribe function. */
  subscribeEvents: (handler: (frame: EventFrame) => void) => () => void;
  onClose: () => void;
}

interface JobView {
  job: JobWire;
  /** Latest child -> Main IRC body attached as the live activity one-liner. */
  activity: string | null;
  activityAt: number | null;
}

const STATUS_LABEL: Record<JobStatus, string> = {
  queued: 'queued',
  running: 'running',
  completed: 'completed',
  failed: 'failed',
  cancelled: 'cancelled',
};

function formatDuration(milliseconds: number): string {
  if (milliseconds < 1000) return `${milliseconds}ms`;
  const seconds = Math.floor(milliseconds / 1000);
  if (seconds < 60) return `${seconds}s`;
  return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
}

/** Mirror of job_card_adapter::elapsed_ms: live wall-clock while active. */
function elapsedMs(job: JobWire, now: number): number | null {
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

/** Mirror of job_card_adapter::progress_row_text. */
function progressLine(job: JobWire, view: JobView, now: number): string {
  const parts: string[] = [];
  if (view.activity && view.activity.trim() !== '') {
    parts.push(view.activity);
  }
  if (parts.length === 0) {
    parts.push(STATUS_LABEL[job.status]);
  }
  const elapsed = elapsedMs(job, now);
  if (elapsed != null) parts.push(formatDuration(elapsed));
  return parts.join(' · ');
}

/** Aggregate row mirroring job_card_adapter::aggregate_row. */
function aggregateLine(jobs: JobView[]): string {
  const counts = { running: 0, queued: 0, completed: 0, failed: 0, cancelled: 0 };
  for (const view of jobs) counts[view.job.status] += 1;
  const parts: string[] = [];
  for (const label of ['running', 'queued', 'completed', 'failed', 'cancelled'] as const) {
    if (counts[label] > 0) parts.push(`${counts[label]} ${label}`);
  }
  return `Jobs · ${parts.join(' · ')}`;
}

export function SubagentsPanel({ sendCommand, subscribeEvents, onClose }: SubagentsPanelProps) {
  const [jobs, setJobs] = useState<Record<string, JobView>>({});
  const [agents, setAgents] = useState<Record<string, AgentWire>>({});
  const [enabled, setEnabled] = useState(false);
  const [catalog, setCatalog] = useState<CatalogAgentWire[]>([]);
  const [now, setNow] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [toast, setToast] = useState('');
  // Spawn form
  const [spawnAgent, setSpawnAgent] = useState('');
  const [spawnTask, setSpawnTask] = useState('');
  // Per-job interactions: hub message body + selected job for output view.
  const [messageDrafts, setMessageDrafts] = useState<Record<string, string>>({});
  const [outputJobId, setOutputJobId] = useState<string | null>(null);
  const outputJobsRef = useRef<Record<string, JobWire>>({});

  const order = useMemo(() => {
    return Object.values(jobs)
      .sort((a, b) => a.job.createdAt - b.job.createdAt || a.job.id.localeCompare(b.job.id))
      .map((view) => view.job.id);
  }, [jobs]);

  const applyJob = (job: JobWire) => {
    setJobs((prev) => {
      const existing = prev[job.id];
      return {
        ...prev,
        [job.id]: {
          job,
          activity: existing?.activity ?? null,
          activityAt: existing?.activityAt ?? null,
        },
      };
    });
  };

  const applyAgent = (agent: AgentWire) => {
    setAgents((prev) => ({ ...prev, [agent.id]: agent }));
  };

  const recordMessage = (message: MailboxMessageWire) => {
    // Attach child -> Main IRC to the child's running job as live activity.
    setJobs((prev) => {
      const match = Object.values(prev)
        .filter((view) => view.job.agentId === message.from)
        .sort((a, b) => statusRank(b.job.status) - statusRank(a.job.status))[0];
      if (!match) return prev;
      return {
        ...prev,
        [match.job.id]: {
          ...match,
          activity: safeText(message.body),
          activityAt: message.timestamp,
        },
      };
    });
  };

  useEffect(() => {
    const unsubscribe = subscribeEvents((frame) => {
      switch (frame.type) {
        case 'job_updated':
          if (frame.job) applyJob(frame.job as unknown as JobWire);
          break;
        case 'agent_updated':
          if (frame.agent) applyAgent(frame.agent as unknown as AgentWire);
          break;
        case 'message_delivered':
          if (frame.message) recordMessage(frame.message as unknown as MailboxMessageWire);
          break;
      }
    });
    // Live elapsed ticker while the panel is open.
    const tick = window.setInterval(() => setNow(Date.now()), 1000);
    return () => {
      unsubscribe();
      window.clearInterval(tick);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [subscribeEvents]);

  useEffect(() => {
    let cancelled = false;
    sendCommand({ type: 'job_list' })
      .then((data) => {
        if (cancelled) return;
        const d = (data || {}) as {
          enabled?: boolean;
          jobs?: JobWire[];
          agents?: AgentWire[];
          catalog?: CatalogAgentWire[];
        };
        setEnabled(!!d.enabled);
        setCatalog(Array.isArray(d.catalog) ? d.catalog : []);
        const next: Record<string, JobView> = {};
        for (const job of d.jobs || []) next[job.id] = { job, activity: null, activityAt: null };
        setJobs(next);
        const agentMap: Record<string, AgentWire> = {};
        for (const agent of d.agents || []) agentMap[agent.id] = agent;
        setAgents(agentMap);
      })
      .catch(() => {
        if (!cancelled) setError('job_list failed — orchestration may be unavailable');
      });
    return () => {
      cancelled = true;
    };
  }, [sendCommand]);

  const spawn = () => {
    const task = spawnTask.trim();
    if (!task) return;
    setBusy(true);
    setError('');
    const args: Record<string, unknown> = { task };
    if (spawnAgent.trim()) args.agent = spawnAgent.trim();
    sendCommand({ type: 'task_spawn', args })
      .then((data) => {
        const spawns = (data as { spawns?: Array<{ jobId?: string; agentId?: string }> }).spawns || [];
        if (spawns.length === 0) {
          setError('task_spawn returned no jobs');
          return;
        }
        setSpawnTask('');
        setToast(`spawned ${spawns.length} subagent job(s)`);
      })
      .catch((err: Error) => setError(`task_spawn failed: ${err.message}`))
      .finally(() => setBusy(false));
  };

  const cancelJob = (jobId: string) => {
    setError('');
    sendCommand({ type: 'job_cancel', jobIds: [jobId] })
      .then(() => setToast(`cancel requested for ${jobId}`))
      .catch((err: Error) => setError(`job_cancel failed: ${err.message}`));
  };

  const sendMessage = (agentId: string, jobId: string) => {
    const body = (messageDrafts[jobId] || '').trim();
    if (!body) return;
    setError('');
    sendCommand({ type: 'hub_send', to: agentId, body })
      .then((data) => {
        const receipts = (data as { receipts?: Array<{ outcome?: string; error?: string }> }).receipts || [];
        const first = receipts[0];
        if (first && first.error) {
          setError(`hub_send failed: ${first.error}`);
        } else {
          setToast(`message delivered to ${agentId}`);
          setMessageDrafts((prev) => ({ ...prev, [jobId]: '' }));
        }
      })
      .catch((err: Error) => setError(`hub_send failed: ${err.message}`));
  };

  const viewOutput = (jobId: string) => {
    sendCommand({ type: 'job_output', jobId })
      .then((data) => {
        const job = (data as { job?: JobWire }).job;
        if (!job) {
          setError(`job_output: no job ${jobId}`);
          return;
        }
        outputJobsRef.current = { ...outputJobsRef.current, [jobId]: job };
        setOutputJobId((prev) => (prev === jobId ? null : jobId));
      })
      .catch((err: Error) => setError(`job_output failed: ${err.message}`));
  };

  return (
    <aside id="subagents-panel" className="subagents-panel" aria-label="Subagents panel">
      <div className="subagents-panel__head">
        <span className="subagents-panel__title">Subagents</span>
        <span className="subagents-panel__counts" id="subagents-counts" title="live orchestration jobs">
          {order.length === 0 ? 'no jobs' : aggregateLine(order.map((id) => jobs[id]))}
        </span>
        <button
          id="subagents-close-btn"
          type="button"
          className="subagents-panel__close"
          title="Close subagents panel"
          onClick={onClose}
        >
          ×
        </button>
      </div>

      {!enabled && (
        <div className="subagents-panel__empty">
          Orchestration is not enabled in this session (settings: orchestration.tasks).
          Subagent spawn / hub features are unavailable until it is.
        </div>
      )}

      {enabled && (
        <>
          <div className="subagents-panel__spawn">
            <div className="subagents-panel__spawn-row">
              <select
                id="subagents-agent-select"
                value={spawnAgent}
                onChange={(e) => setSpawnAgent(e.target.value)}
                title="Agent definition (default: auto-selected)"
              >
                <option value="">agent (auto)…</option>
                {catalog.map((agent) => (
                  <option key={agent.name} value={agent.name} title={safeText(agent.description)}>
                    {safeText(agent.name)}
                  </option>
                ))}
              </select>
              <input
                id="subagents-task-input"
                placeholder="task briefing (objective + acceptance)"
                value={spawnTask}
                onChange={(e) => setSpawnTask(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter') {
                    e.preventDefault();
                    spawn();
                  }
                }}
                spellCheck={false}
              />
              <button
                id="subagents-spawn-btn"
                type="button"
                onClick={spawn}
                disabled={busy || !spawnTask.trim()}
              >
                Spawn
              </button>
            </div>
          </div>

          <div className="subagents-panel__list">
            {order.length === 0 && (
              <div className="subagents-panel__empty">
                No subagent jobs yet. Spawn one above, or ask the main agent to
                delegate (job_updated events refresh this panel live).
              </div>
            )}
            {order.map((id) => {
              const view = jobs[id];
              const job = view.job;
              const result = job.result;
              const settled = job.status !== 'queued' && job.status !== 'running';
              return (
                <section
                  key={job.id}
                  className="subagent-job"
                  data-job-id={job.id}
                  data-status={job.status}
                >
                  <div className="subagent-job__head">
                    <span className="subagent-job__title" title={safeText(job.id)}>
                      {safeText(agentLabel(agents, job.agentId))} ({safeText(job.agent)})
                    </span>
                    <span className={`subagent-job__status subagent-job__status--${job.status}`}>
                      {STATUS_LABEL[job.status]}
                    </span>
                  </div>
                  {job.description && (
                    <div className="subagent-job__description" title={safeText(job.description)}>
                      {safeText(job.description)}
                    </div>
                  )}
                  {!settled && (
                    <div className="subagent-job__progress" data-progress-line>
                      {progressLine(job, view, now)}
                    </div>
                  )}
                  {settled && (
                    <div className="subagent-job__progress" data-progress-line>
                      {progressLine(job, view, now)}
                    </div>
                  )}
                  <div className="subagent-job__actions">
                    <button
                      type="button"
                      className="subagent-job__action"
                      onClick={() => viewOutput(job.id)}
                      title="Fetch the settled job's delivered output (yield payload)"
                    >
                      {outputJobId === job.id ? 'Hide output' : 'Output'}
                    </button>
                    <button
                      type="button"
                      className="subagent-job__action"
                      onClick={() => cancelJob(job.id)}
                      disabled={settled}
                      title="Cancel this subagent job"
                    >
                      Cancel
                    </button>
                  </div>
                  {outputJobId === job.id && (
                    <div className="subagent-job__output" data-output-view>
                      {result?.error ? (
                        <div className="subagent-job__error">{safeText(result.error)}</div>
                      ) : null}
                      <pre className="subagent-job__output-text">
                        {result && result.output.trim() !== ''
                          ? safeText(result.output)
                          : '(no delivered output yet)'}
                      </pre>
                      {result && (result.artifactRef || result.historyRef || result.artifactUri) && (
                        <div className="subagent-job__refs">
                          {result.artifactRef && <div>agent://{safeText(result.artifactRef)}</div>}
                          {result.historyRef && <div>history://{safeText(result.historyRef)}</div>}
                          {result.artifactUri && <div>{safeText(result.artifactUri)}</div>}
                        </div>
                      )}
                    </div>
                  )}
                  <div className="subagent-job__message">
                    <input
                      className="subagent-job__message-input"
                      placeholder={`message ${safeText(job.agentId)} (hub send)`}
                      value={messageDrafts[job.id] || ''}
                      onChange={(e) =>
                        setMessageDrafts((prev) => ({ ...prev, [job.id]: e.target.value }))
                      }
                      onKeyDown={(e) => {
                        if (e.key === 'Enter') {
                          e.preventDefault();
                          sendMessage(job.agentId, job.id);
                        }
                      }}
                      spellCheck={false}
                    />
                    <button
                      type="button"
                      className="subagent-job__action"
                      onClick={() => sendMessage(job.agentId, job.id)}
                      disabled={!(messageDrafts[job.id] || '').trim()}
                      title="Send a hub message to this subagent"
                    >
                      Send
                    </button>
                  </div>
                </section>
              );
            })}
          </div>
        </>
      )}

      {error && (
        <div className="subagents-panel__error" data-panel-error>
          {safeText(error)}
        </div>
      )}
      {toast && (
        <div className="subagents-panel__toast" data-panel-toast>
          {safeText(toast)}
        </div>
      )}
    </aside>
  );
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

/** Resolve a human-facing agent label; falls back to the stable id. */
export function agentLabel(agents: Record<string, AgentWire>, agentId: string): string {
  const agent = agents[agentId];
  const name = agent?.displayName?.trim() || '';
  return name || agentId;
}
