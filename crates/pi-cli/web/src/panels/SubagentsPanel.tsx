// Subagents panel — live mirror of the TUI job cards + hub surface.
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
//   agent_history -> { agentId, text }  (bounded redacted child transcript)
//
// Live updates arrive as orchestration events over the same WS:
//   job_updated / agent_updated / message_delivered
// (ApplicationEvent::Orchestration, serde tag "type", snake_case).

import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import { safeText } from '../redact';
import {
  elapsedMs,
  formatDuration,
  SUBAGENT_FILTER_LABEL,
  subagentJobBucket,
  type JobStatus,
  type JobWire,
  type SubagentJobFilter,
} from '../jobCard';
import type { EventFrame } from '../types';

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

/** Filter order for the segmented control (roving-tab order). */
const JOB_FILTERS: SubagentJobFilter[] = ['running', 'completed'];

/** Lines requested from the agent_history RPC (core clamps to 1..=200). */
const HISTORY_LINES = 80;
/** Modal transcript poll cadence while the job is queued/running. */
const POLL_MS = 2000;

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
  // A real initial clock keeps the running-details elapsed label visible
  // before the one-second ticker fires.
  const [now, setNow] = useState(() => Date.now());
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [toast, setToast] = useState('');
  // List filter: live work (queued/running) by default; settled history opt-in.
  const [jobFilter, setJobFilter] = useState<SubagentJobFilter>('running');
  // Spawn form
  const [spawnAgent, setSpawnAgent] = useState('');
  const [spawnTask, setSpawnTask] = useState('');
  // Per-job interactions: hub message body + selected job for output view.
  const [messageDrafts, setMessageDrafts] = useState<Record<string, string>>({});
  const [outputJobId, setOutputJobId] = useState<string | null>(null);
  const outputJobsRef = useRef<Record<string, JobWire>>({});
  // Details modal: job under inspection + bounded child transcript state.
  const [detailsJobId, setDetailsJobId] = useState<string | null>(null);
  const [historyText, setHistoryText] = useState('');
  const [historyError, setHistoryError] = useState('');
  const detailsTriggerRef = useRef<HTMLElement | null>(null);
  const detailsCloseRef = useRef<HTMLButtonElement | null>(null);
  const detailsRef = useRef<HTMLDivElement | null>(null);
  const fetchHistoryRef = useRef<() => void>(() => {});

  const order = useMemo(() => {
    return Object.values(jobs)
      .sort((a, b) => a.job.createdAt - b.job.createdAt || a.job.id.localeCompare(b.job.id))
      .map((view) => view.job.id);
  }, [jobs]);

  // Filter only affects the visible list — the jobs map and the header
  // aggregate stay global so no job state is ever hidden from the panel.
  const filteredOrder = useMemo(() => {
    return order.filter((id) => subagentJobBucket(jobs[id].job.status) === jobFilter);
  }, [order, jobs, jobFilter]);

  const handleFilterKeyDown = (e: KeyboardEvent<HTMLButtonElement>) => {
    const index = JOB_FILTERS.indexOf(jobFilter);
    let next: SubagentJobFilter | null = null;
    switch (e.key) {
      case 'ArrowRight':
      case 'ArrowDown':
        next = JOB_FILTERS[(index + 1) % JOB_FILTERS.length];
        break;
      case 'ArrowLeft':
      case 'ArrowUp':
        next = JOB_FILTERS[(index - 1 + JOB_FILTERS.length) % JOB_FILTERS.length];
        break;
      case 'Home':
        next = JOB_FILTERS[0];
        break;
      case 'End':
        next = JOB_FILTERS[JOB_FILTERS.length - 1];
        break;
    }
    if (next === null) return;
    e.preventDefault();
    setJobFilter(next);
    document.getElementById(`subagents-filter-${next}`)?.focus();
  };

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

  // --- Details modal: bounded child transcript, polled while live. ---
  const detailsJob = detailsJobId ? jobs[detailsJobId] : null;
  const detailsLive =
    !!detailsJob && (detailsJob.job.status === 'queued' || detailsJob.job.status === 'running');
  const detailsElapsedMs = detailsJob ? elapsedMs(detailsJob.job, now) : null;
  const detailsElapsedLabel = detailsElapsedMs != null ? formatDuration(detailsElapsedMs) : '';

  const openDetails = (jobId: string) => {
    detailsTriggerRef.current = document.activeElement as HTMLElement | null;
    setHistoryText('');
    setHistoryError('');
    setDetailsJobId(jobId);
  };

  const closeDetails = () => {
    setDetailsJobId(null);
    const trigger = detailsTriggerRef.current;
    detailsTriggerRef.current = null;
    if (trigger && typeof trigger.focus === 'function') trigger.focus();
  };

  useEffect(() => {
    if (!detailsJobId || !detailsJob) return;
    const agentId = detailsJob.job.agentId;
    let cancelled = false;
    let timer: number | null = null;

    const fetchHistory = () => {
      sendCommand({ type: 'agent_history', agentId, lines: HISTORY_LINES })
        .then((data) => {
          if (cancelled) return;
          const d = data as { text?: unknown };
          setHistoryText(typeof d.text === 'string' ? d.text : '');
          setHistoryError('');
        })
        .catch((err: Error) => {
          if (cancelled) return;
          setHistoryError(err.message || 'agent_history failed');
        });
    };
    fetchHistoryRef.current = fetchHistory;
    fetchHistory();

    // Poll every 2s while queued/running; refetch once per relevant live event.
    if (detailsLive) timer = window.setInterval(fetchHistory, POLL_MS);
    const unsubscribe = subscribeEvents((frame) => {
      const relevant =
        (frame.type === 'job_updated' &&
          (frame.job as { id?: string } | undefined)?.id === detailsJobId) ||
        (frame.type === 'agent_updated' &&
          (frame.agent as { id?: string } | undefined)?.id === agentId) ||
        (frame.type === 'message_delivered' &&
          (frame.message as { from?: string } | undefined)?.from === agentId);
      if (relevant) fetchHistory();
    });

    return () => {
      cancelled = true;
      if (timer !== null) window.clearInterval(timer);
      unsubscribe();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [detailsJobId, detailsJob?.job.agentId, detailsLive, sendCommand, subscribeEvents]);

  // Initial focus: land on the Close button when the modal opens.
  useEffect(() => {
    if (detailsJobId) detailsCloseRef.current?.focus();
  }, [detailsJobId]);

  const handleDetailsKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      closeDetails();
      return;
    }
    if (e.key !== 'Tab' || !detailsRef.current) return;
    const focusables = detailsRef.current.querySelectorAll<HTMLElement>(
      'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])',
    );
    if (focusables.length === 0) return;
    const first = focusables[0];
    const last = focusables[focusables.length - 1];
    if (e.shiftKey && document.activeElement === first) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault();
      first.focus();
    }
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
          aria-label="Close subagents panel"
          onClick={onClose}
        >
          ×
        </button>
      </div>

      {!enabled && (
        <div className="subagents-panel__empty">
          Orchestration not enabled. Run <code>/settings set --project orchestration.tasks true</code>{' '}
          to enable subagents (spawn / hub features are unavailable until it is).
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

          <div
            className="subagents-panel__filter"
            role="tablist"
            aria-label="Filter subagent jobs by status"
          >
            {JOB_FILTERS.map((filter) => (
              <button
                key={filter}
                type="button"
                id={`subagents-filter-${filter}`}
                role="tab"
                aria-selected={jobFilter === filter}
                aria-pressed={jobFilter === filter}
                aria-controls="subagents-job-list"
                data-filter={filter}
                className={`subagents-panel__filter-btn${jobFilter === filter ? ' is-active' : ''}`}
                tabIndex={jobFilter === filter ? 0 : -1}
                onClick={() => setJobFilter(filter)}
                onKeyDown={handleFilterKeyDown}
              >
                {SUBAGENT_FILTER_LABEL[filter]}
              </button>
            ))}
          </div>

          <div className="subagents-panel__list" id="subagents-job-list">
            {filteredOrder.length === 0 && (
              <div className="subagents-panel__empty" data-filter-empty>
                {order.length === 0
                  ? 'No subagent jobs yet. Spawn one above, or ask the main agent to delegate (job_updated events refresh this panel live).'
                  : jobFilter === 'running'
                    ? 'No active subagent jobs right now (queued/running). Settled jobs are listed under Completed.'
                    : 'No settled subagent jobs yet (completed/failed/cancelled). Active jobs are listed under Running.'}
              </div>
            )}
            {filteredOrder.map((id) => {
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
                  onClick={() => openDetails(job.id)}
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
                  <div className="subagent-job__actions" onClick={(e) => e.stopPropagation()}>
                    <button
                      type="button"
                      className="subagent-job__action"
                      data-details-trigger
                      onClick={() => openDetails(job.id)}
                      title="Open job details and live child transcript"
                    >
                      Details
                    </button>
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
                    <div
                      className="subagent-job__output"
                      data-output-view
                      onClick={(e) => e.stopPropagation()}
                    >
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
                  <div className="subagent-job__message" onClick={(e) => e.stopPropagation()}>
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
      {detailsJob && (
        <div className="subagent-details-backdrop" onClick={closeDetails}>
          <div
            ref={detailsRef}
            className="subagent-details"
            role="dialog"
            aria-modal="true"
            aria-label="Subagent job details"
            data-details-dialog
            data-job-id={detailsJob.job.id}
            onClick={(e) => e.stopPropagation()}
            onKeyDown={handleDetailsKeyDown}
          >
            <div className="subagent-details__head">
              <span className="subagent-details__title" data-details-title>
                {safeText(agentLabel(agents, detailsJob.job.agentId))} ({safeText(detailsJob.job.agent)})
              </span>
              <div className="subagent-details__head-actions">
                <button
                  type="button"
                  className="subagent-details__action"
                  data-details-refresh
                  onClick={() => fetchHistoryRef.current?.()}
                  title="Refetch the child transcript now"
                >
                  Refresh
                </button>
                <button
                  ref={detailsCloseRef}
                  type="button"
                  className="subagent-details__action"
                  data-details-close
                  onClick={closeDetails}
                  title="Close details (Esc)"
                  aria-label="Close job details"
                >
                  Close
                </button>
              </div>
            </div>
            <div className="subagent-details__meta" data-details-status>
              <span className={`subagent-job__status subagent-job__status--${detailsJob.job.status}`}>
                {STATUS_LABEL[detailsJob.job.status]}
              </span>
              <span data-details-elapsed>{detailsElapsedLabel}</span>
            </div>
            <div className="subagent-details__section">
              <div className="subagent-details__label">Task</div>
              <div className="subagent-details__description" data-details-description>
                {safeText(detailsJob.job.description || '(no task description)')}
              </div>
            </div>
            <div className="subagent-details__section">
              <div className="subagent-details__label">Latest activity</div>
              <div className="subagent-details__activity" data-details-activity>
                {safeText(detailsJob.activity || STATUS_LABEL[detailsJob.job.status])}
              </div>
            </div>
            <div className="subagent-details__section subagent-details__section--transcript">
              <div className="subagent-details__label">
                Recent transcript
                {detailsLive && (
                  <span className="subagent-details__live" data-details-live>
                    live
                  </span>
                )}
              </div>
              {historyError !== '' && (
                <div className="subagent-details__error" data-details-error>
                  {safeText(historyError)}
                </div>
              )}
              <pre className="subagent-details__history" data-details-history>
                {historyText !== ''
                  ? safeText(historyText)
                  : historyError !== ''
                    ? '(transcript unavailable)'
                    : '(no transcript yet — waiting for child activity)'}
              </pre>
            </div>
          </div>
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
