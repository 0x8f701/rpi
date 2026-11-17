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
//   job_output   -> { job, assignment: {status:"available",text} |
//                                          {status:"unavailable",reason} }
//   agent_history -> { agentId, messages: [{ id?, message }], truncation }
//                    (bounded, redacted structured child transcript)
//
// Live updates arrive as orchestration events over the same WS:
//   job_updated / agent_updated / message_delivered
// (ApplicationEvent::Orchestration, serde tag "type", snake_case).

import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent, type ReactNode } from 'react';
import { safeText } from '../redact';
import { messagesToItems, type Item } from '../transcript';
import { MarkdownBody } from '../MarkdownBody';
import { dispatchMarkdownPathClick } from '../markdown';
import {
  elapsedMs,
  formatDuration,
  isLiveJobStatus,
  SUBAGENT_FILTER_LABEL,
  subagentJobBucket,
  type JobStatus,
  type JobWire,
  type SubagentJobFilter,
} from '../jobCard';
import type { EventFrame } from '../types';
import { isTransportError } from '../transportError';
import {
  applyAuthoritativeOrchestrationSnapshot,
  applyOrchestrationEvent,
  beginOrchestrationRefresh,
  failOrchestrationRefresh,
  initialOrchestrationProjection,
  type AgentWire,
  type CatalogAgentWire,
  type JobView,
  type MailboxMessageWire,
  type OrchestrationProjection,
} from '../orchestrationProjection';


export type { AgentWire, MailboxMessageWire } from '../orchestrationProjection';

/** Why the core dropped rows from the head of the child transcript. */
export type AgentHistoryTruncationReason = 'none' | 'limit' | 'bytes' | 'limit_and_bytes';

export interface AgentHistoryTruncation {
  omittedRows: number;
  reason: AgentHistoryTruncationReason;
}

interface SubagentsPanelProps {
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>;
  /** App-owned subscription: returns an unsubscribe function. */
  subscribeEvents: (handler: (frame: EventFrame) => void) => () => void;
  connectionEpoch: number;
  initialPage?: 'jobs' | 'irc';
  onClose: () => void;
  /** Renders restored transcript items with the App's shared SurfaceItems
   *  (ToolCard / thinking / Markdown), injected to avoid a panel → App import
   *  cycle. The panel owns the wire; the App owns the renderer. */
  renderHistory: (items: Item[]) => ReactNode;
}

const STATUS_LABEL: Record<JobStatus, string> = {
  queued: 'queued',
  running: 'running',
  cancelling: 'Cancelling',
  completed: 'completed',
  failed: 'failed',
  cancelled: 'cancelled',
};

/** Filter order for the segmented control (roving-tab order). */
const JOB_FILTERS: SubagentJobFilter[] = ['running', 'completed'];

/** Projected message rows requested from `agent_history`: the wire's hard cap
 *  (1..=200, default 80). The response payload is bounded separately by the
 *  core's byte cap and reports what it dropped in `truncation`. */
const HISTORY_LIMIT = 200;
/** Modal transcript poll cadence while the job is queued/running. */
const POLL_MS = 2000;

const TRUNCATION_REASONS: Readonly<Record<string, true>> = {
  none: true,
  limit: true,
  bytes: true,
  limit_and_bytes: true,
};

/** Runtime record guard for RPC payloads (`null`/arrays rejected). */
function asWireRecord(value: unknown): Record<string, unknown> | null {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

/** Display fields `public_history_message` always emits for one block. Unknown
 *  block types fail closed; valid wire controls keep projecting. */
function isHistoryBlock(block: unknown): boolean {
  const b = asWireRecord(block);
  if (b === null || typeof b.type !== 'string') return false;
  switch (b.type) {
    case 'text':
      return typeof b.text === 'string';
    case 'thinking':
      return typeof b.thinking === 'string';
    case 'image':
      return typeof b.data === 'string' && typeof b.mimeType === 'string';
    case 'toolCall':
      return typeof b.id === 'string' && typeof b.name === 'string' && 'arguments' in b;
    default:
      return false;
  }
}

/** Closed role + display contract: user/assistant/toolResult content is a
 *  block array; bash command/output are strings; custom display is a boolean.
 *  Any other role is a malformed row, never a silently empty history. */
function isHistoryMessage(message: Record<string, unknown>): boolean {
  switch (message.role) {
    case 'user':
    case 'assistant':
    case 'toolResult':
      return Array.isArray(message.content) && message.content.every(isHistoryBlock);
    case 'bashExecution':
      return typeof message.command === 'string' && typeof message.output === 'string';
    case 'custom':
      return typeof message.display === 'boolean';
    default:
      return false;
  }
}

/** One `agent_history` row: `{ id?, message: { role, ... } }`. Mirrors the
 *  mobile `parseAgentHistoryRow` contract; any malformed row fails the whole
 *  payload so a half-valid list never replaces last-good history. */
function projectAgentHistoryRows(
  messages: unknown[],
): Array<{ id?: string; message: Record<string, unknown> }> | null {
  const rows: Array<{ id?: string; message: Record<string, unknown> }> = [];
  for (const raw of messages) {
    const row = asWireRecord(raw);
    if (row === null) return null;
    const message = asWireRecord(row.message);
    if (message === null || !isHistoryMessage(message)) {
      return null;
    }
    const id = row.id;
    rows.push(typeof id === 'string' && id !== '' ? { id, message } : { message });
  }
  return rows;
}

/** Validate one `agent_history` success payload and project its rows through
 *  the shared transcript projector. Throws on any shape violation — a
 *  mismatched `agentId` or a missing `truncation` must never render as this
 *  child's history. */
export function projectAgentHistory(data: unknown, agentId: string): {
  items: Item[];
  truncation: AgentHistoryTruncation;
} {
  const payload = asWireRecord(data);
  if (payload === null || payload.agentId !== agentId) {
    const reported = payload !== null ? payload.agentId : undefined;
    const got = typeof reported === 'string' && reported !== '' ? reported : 'nothing';
    throw new Error(`agent_history returned history for ${got}`);
  }
  const messages = payload.messages;
  if (!Array.isArray(messages)) {
    throw new Error('agent_history returned no messages array');
  }
  const rows = projectAgentHistoryRows(messages);
  if (rows === null) {
    throw new Error('agent_history returned an invalid message row');
  }
  const truncation = asWireRecord(payload.truncation);
  if (truncation === null) {
    throw new Error('agent_history returned no truncation');
  }
  const omittedRows = truncation.omittedRows;
  const reason = truncation.reason;
  if (typeof omittedRows !== 'number' || !Number.isInteger(omittedRows) || omittedRows < 0) {
    throw new Error('agent_history returned an invalid omittedRows');
  }
  if (typeof reason !== 'string' || TRUNCATION_REASONS[reason] !== true) {
    throw new Error('agent_history returned an invalid truncation reason');
  }
  if (reason === 'none' && omittedRows !== 0) {
    throw new Error('agent_history reported untruncated but omitted rows');
  }
  return {
    items: messagesToItems(rows),
    truncation: { omittedRows, reason: reason as AgentHistoryTruncationReason },
  };
}

type DetailAssignment =
  | { jobId: string; status: 'loading' }
  | { jobId: string; status: 'available'; text: string }
  | { jobId: string; status: 'unavailable'; reason: string }
  | { jobId: string; status: 'error' };
type LoadedAssignment = Extract<DetailAssignment, { status: 'available' | 'unavailable' }>;

function projectJobAssignment(data: unknown, jobId: string): LoadedAssignment {
  const payload = asWireRecord(data);
  const job = payload === null ? null : asWireRecord(payload.job);
  if (payload === null || job === null || job.id !== jobId) {
    const got = job !== null && typeof job.id === 'string' && job.id !== '' ? job.id : 'nothing';
    throw new Error(`job_output returned job ${got} for ${jobId}`);
  }
  const assignment = asWireRecord(payload.assignment);
  if (assignment === null) throw new Error('job_output returned no assignment');
  if (assignment.status === 'available') {
    if (typeof assignment.text !== 'string') {
      throw new Error('job_output available assignment has no text');
    }
    return { jobId, status: 'available', text: assignment.text };
  }
  if (assignment.status === 'unavailable') {
    if (typeof assignment.reason !== 'string' || assignment.reason === '') {
      throw new Error('job_output unavailable assignment has no reason');
    }
    return { jobId, status: 'unavailable', reason: assignment.reason };
  }
  throw new Error('job_output returned an unknown assignment status');
}

/** Human notice for dropped head rows; empty for an untruncated transcript. */
function truncationNotice(truncation: AgentHistoryTruncation): string {
  const rows = `${truncation.omittedRows} older message${truncation.omittedRows === 1 ? '' : 's'} hidden`;
  switch (truncation.reason) {
    case 'limit':
      return `${rows} (message limit)`;
    case 'bytes':
      return `${rows} (size limit)`;
    case 'limit_and_bytes':
      return `${rows} (message and size limits)`;
    default:
      return '';
  }
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
  const counts: Record<JobStatus, number> = {
    queued: 0, running: 0, cancelling: 0, completed: 0, failed: 0, cancelled: 0,
  };
  for (const view of jobs) counts[view.job.status] += 1;
  const parts: string[] = [];
  for (const label of ['running', 'cancelling', 'queued', 'completed', 'failed', 'cancelled'] as const) {
    if (counts[label] > 0) parts.push(`${counts[label]} ${label}`);
  }
  return `Jobs · ${parts.join(' · ')}`;
}


export function SubagentsPanel({ sendCommand, subscribeEvents, connectionEpoch, initialPage = 'jobs', onClose, renderHistory }: SubagentsPanelProps) {
  const [projection, setProjection] = useState<OrchestrationProjection>(initialOrchestrationProjection);
  const jobs = projection.jobs;
  const agents = projection.agents;
  const [enabled, setEnabled] = useState(false);
  const [catalog, setCatalog] = useState<CatalogAgentWire[]>([]);
  const nextRequestIdRef = useRef(0);
  // A real initial clock keeps the running-details elapsed label visible
  // before the one-second ticker fires.
  const [now, setNow] = useState(() => Date.now());
  const [busy, setBusy] = useState(false);
  const busyRef = useRef(false);
  const hubSendingRef = useRef(false);
  const [error, setError] = useState('');
  const [toast, setToast] = useState('');
  // List filter: live work (queued/running) by default; settled history opt-in.
  const [jobFilter, setJobFilter] = useState<SubagentJobFilter>('running');
  const [page, setPage] = useState<'jobs' | 'irc'>(initialPage);
  const messages = projection.messages;
  // Spawn form
  const [spawnAgent, setSpawnAgent] = useState('');
  const [spawnTask, setSpawnTask] = useState('');
  // Per-job interactions: hub message body + selected job for output view.
  const [messageDrafts, setMessageDrafts] = useState<Record<string, string>>({});
  const [outputJobId, setOutputJobId] = useState<string | null>(null);
  const outputJobsRef = useRef<Record<string, JobWire>>({});
  // Details modal: job under inspection + bounded structured child transcript.
  const [detailsJobId, setDetailsJobId] = useState<string | null>(null);
  const [historyItems, setHistoryItems] = useState<Item[]>([]);
  const [historyTruncation, setHistoryTruncation] = useState<AgentHistoryTruncation | null>(null);
  const [historyLoading, setHistoryLoading] = useState(false);
  const [historyError, setHistoryError] = useState('');
  // Authoritative full task body (independent of the history poller).
  const [assignment, setAssignment] = useState<DetailAssignment | null>(null);
  const [assignmentError, setAssignmentError] = useState<{ jobId: string; message: string } | null>(null);
  const [assignmentRevision, setAssignmentRevision] = useState(0);
  // Monotonic read sequence: a slower older snapshot never overwrites a newer
  // one; `historyAgentRef` invalidates responses for a previously opened child.
  const historySeqRef = useRef(0);
  const historyAgentRef = useRef('');
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



  useEffect(() => {
    const unsubscribe = subscribeEvents((frame) => {
      setProjection((prev) => applyOrchestrationEvent(prev, frame));
    });
    // Live elapsed ticker while the panel is open.
    const tick = window.setInterval(() => setNow(Date.now()), 1000);
    return () => {
      unsubscribe();
      window.clearInterval(tick);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [subscribeEvents]);

  const refreshJobs = useCallback((reason: 'connection' | 'repair') => {
    const requestId = ++nextRequestIdRef.current;
    setProjection((state) => beginOrchestrationRefresh(state, requestId));
    if (reason === 'connection') setError('');
    void sendCommand({ type: 'job_list' })
      .then((data) => {
        const d = (data || {}) as {
          enabled?: boolean;
          groupId?: string;
          jobs?: JobWire[];
          agents?: AgentWire[];
          catalog?: CatalogAgentWire[];
          messages?: MailboxMessageWire[];
        };
        setEnabled(!!d.enabled);
        setCatalog(Array.isArray(d.catalog) ? d.catalog : []);
        setProjection((state) => applyAuthoritativeOrchestrationSnapshot(state, requestId, {
          groupId: d.groupId,
          jobs: d.jobs,
          agents: d.agents,
          messages: d.messages,
        }));
      })
      .catch((error: unknown) => {
        setProjection((state) => failOrchestrationRefresh(state, requestId));
        if (!isTransportError(error)) setError('job_list failed — orchestration may be unavailable');
      });
  }, [sendCommand]);

  useEffect(() => {
    refreshJobs('connection');
  }, [connectionEpoch, refreshJobs]);

  useEffect(() => {
    if (projection.needsRefresh && projection.requestId === null) refreshJobs('repair');
  }, [projection.needsRefresh, projection.requestId, refreshJobs]);

  const spawn = async () => {
    const task = spawnTask.trim();
    if (!task || busyRef.current) return;
    busyRef.current = true;
    setBusy(true);
    setError('');
    const args: Record<string, unknown> = { task };
    if (spawnAgent.trim()) args.agent = spawnAgent.trim();
    try {
      const data = await sendCommand({ type: 'task_spawn', args });
      const spawns = (data as { spawns?: Array<{ jobId?: string; agentId?: string }> }).spawns || [];
      if (spawns.length === 0) {
        setError('task_spawn returned no jobs');
        return;
      }
      setSpawnTask((current) => current.trim() === task ? '' : current);
      setToast(`spawned ${spawns.length} subagent job(s)`);
    } catch (error: unknown) {
      if (!isTransportError(error)) setError(`task_spawn failed: ${(error as Error).message}`);
    } finally {
      busyRef.current = false;
      setBusy(false);
    }
  };

  const cancelJob = (jobId: string) => {
    setError('');
    sendCommand({ type: 'job_cancel', jobIds: [jobId] })
      .then(() => setToast(`cancel requested for ${jobId}`))
      .catch((error: unknown) => {
        if (!isTransportError(error)) setError(`job_cancel failed: ${(error as Error).message}`);
      });
  };

  const sendMessage = async (agentId: string, jobId: string) => {
    const body = (messageDrafts[jobId] || '').trim();
    if (!body || hubSendingRef.current) return;
    hubSendingRef.current = true;
    setError('');
    try {
      const data = await sendCommand({ type: 'hub_send', to: agentId, body });
      const receipts = (data as { receipts?: Array<{ outcome?: string; error?: string }> }).receipts || [];
      const first = receipts[0];
      if (first && first.error) {
        setError(`hub_send failed: ${first.error}`);
      } else {
        setToast(`message delivered to ${agentId}`);
        setMessageDrafts((prev) => {
          const current = prev[jobId] || '';
          if (current.trim() !== body) return prev;
          return { ...prev, [jobId]: '' };
        });
      }
    } catch (error: unknown) {
      if (!isTransportError(error)) setError(`hub_send failed: ${(error as Error).message}`);
    } finally {
      hubSendingRef.current = false;
    }
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
      .catch((error: unknown) => {
        if (!isTransportError(error)) setError(`job_output failed: ${(error as Error).message}`);
      });
  };

  // --- Details modal: bounded child transcript, polled while live. ---
  const detailsJob = detailsJobId ? jobs[detailsJobId] : null;
  const detailsLive =
    !!detailsJob && isLiveJobStatus(detailsJob.job.status);
  const detailsElapsedMs = detailsJob ? elapsedMs(detailsJob.job, now) : null;
  const detailsElapsedLabel = detailsElapsedMs != null ? formatDuration(detailsElapsedMs) : '';
  // Only the job on screen may paint: never the previous job's task body.
  const detailAssignment =
    detailsJob && assignment !== null && assignment.jobId === detailsJob.job.id
      ? assignment
      : null;

  const openDetails = (jobId: string) => {
    detailsTriggerRef.current = document.activeElement as HTMLElement | null;
    // Switching children clears the previous child immediately and invalidates
    // its in-flight reads.
    historyAgentRef.current = '';
    historySeqRef.current += 1;
    setHistoryItems([]);
    setHistoryTruncation(null);
    setHistoryError('');
    setHistoryLoading(true);
    setAssignment({ jobId, status: 'loading' });
    setAssignmentError(null);
    setDetailsJobId(jobId);
  };

  const closeDetails = () => {
    historySeqRef.current += 1;
    setAssignment(null);
    setAssignmentError(null);
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

    // A changed agent (not a live-status flip) starts from a clean transcript;
    // keeps last-good rows for the SAME agent across polls.
    if (historyAgentRef.current !== agentId) {
      historyAgentRef.current = agentId;
      historySeqRef.current += 1;
      setHistoryItems([]);
      setHistoryTruncation(null);
      setHistoryError('');
      setHistoryLoading(true);
    }

    const fetchHistory = () => {
      const seq = ++historySeqRef.current;
      sendCommand({ type: 'agent_history', agentId, limit: HISTORY_LIMIT })
        .then((data) => {
          if (cancelled || seq !== historySeqRef.current || historyAgentRef.current !== agentId) return;
          try {
            const history = projectAgentHistory(data, agentId);
            setHistoryItems(history.items);
            setHistoryTruncation(history.truncation);
            setHistoryError('');
          } catch (error: unknown) {
            setHistoryError((error as Error).message || 'agent_history returned an invalid payload');
          } finally {
            setHistoryLoading(false);
          }
        })
        .catch((error: unknown) => {
          if (cancelled || seq !== historySeqRef.current || historyAgentRef.current !== agentId) return;
          setHistoryLoading(false);
          if (isTransportError(error)) return;
          setHistoryError((error as Error).message || 'agent_history failed');
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
  }, [detailsJobId, detailsJob?.job.agentId, detailsLive, connectionEpoch, sendCommand, subscribeEvents]);

  // Full task body (independent of history); same-job last-good survives failures.
  useEffect(() => {
    if (!detailsJobId) return;
    const jobId = detailsJobId;
    let isCurrent = true;
    const report = (message: string) => {
      if (!isCurrent) return;
      setAssignmentError({ jobId, message });
      setAssignment((prev) => (
        prev !== null && prev.jobId === jobId
          && (prev.status === 'available' || prev.status === 'unavailable')
          ? prev
          : { jobId, status: 'error' }
      ));
    };
    setAssignment((prev) => (
      prev !== null && prev.jobId === jobId && prev.status !== 'error'
        ? prev
        : { jobId, status: 'loading' }
    ));
    void sendCommand({ type: 'job_output', jobId })
      .then((data) => {
        if (!isCurrent) return;
        try {
          setAssignment(projectJobAssignment(data, jobId));
          setAssignmentError((prev) => (prev !== null && prev.jobId === jobId ? null : prev));
        } catch (error: unknown) {
          report((error as Error).message || 'job_output returned an invalid payload');
        }
      })
      .catch((error: unknown) => {
        report((error as Error).message || 'job_output failed');
      });
    return () => {
      isCurrent = false;
    };
  }, [detailsJobId, connectionEpoch, assignmentRevision, sendCommand]);

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
    <aside
      id="subagents-panel"
      // The details modal is a child of this fixed panel, so the panel's own
      // stacking context (the shared drawer z-index) would cap the modal's
      // backdrop below the composer footer. The modifier class raises the
      // panel while details is open — see `.subagents-panel--details`.
      className={detailsJob ? 'subagents-panel subagents-panel--details' : 'subagents-panel'}
      aria-label="Subagents panel"
    >
      <div className="subagents-panel__head">
        <span className="subagents-panel__title">Subagents</span>
        <span className="subagents-panel__counts" id="subagents-counts" title="live orchestration jobs">
          {page === 'irc'
            ? `${messages.length} messages`
            : order.length === 0
              ? 'no jobs'
              : aggregateLine(order.map((id) => jobs[id]))}
        </span>
        <button
          id="subagents-close-btn"
          type="button"
          className="subagents-panel__close"
          title="Close subagents panel"
          aria-label="Close subagents panel"
          onClick={onClose}
        >
          {'\u00D7'}
        </button>
      </div>
      <div className="subagents-panel__body">
      <div className="subagents-panel__filter" role="tablist" aria-label="Jobs or IRC">
        <button
          type="button"
          className={`subagents-panel__filter-btn${page === 'jobs' ? ' is-active' : ''}`}
          aria-selected={page === 'jobs'}
          onClick={() => setPage('jobs')}
        >
          Jobs
        </button>
        <button
          type="button"
          className={`subagents-panel__filter-btn${page === 'irc' ? ' is-active' : ''}`}
          aria-selected={page === 'irc'}
          onClick={() => setPage('irc')}
        >
          IRC
        </button>
      </div>


      {!enabled && (
        <div className="subagents-panel__empty">
          Orchestration not enabled. Run <code>/settings set --project orchestration.tasks true</code>{' '}
          to enable subagents (spawn / hub features are unavailable until it is).
        </div>
      )}

      {enabled && page === 'irc' && (
        <div className="subagents-panel__list" id="subagents-irc-list">
          {messages.length === 0 && (
            <div className="subagents-panel__empty">No hub mailbox messages.</div>
          )}
          {messages.map((message) => (
            <section key={message.id} className="subagent-job" data-message-id={message.id}>
              <div className="subagent-job__head">
                <span className="subagent-job__title">
                  {safeText(message.from)} ➤ {safeText(message.to)}
                </span>
              </div>
              <div className="subagent-job__progress">{safeText(message.body)}</div>
              {message.replyTo ? (
                <div className="subagent-job__meta">reply to {safeText(message.replyTo)}</div>
              ) : null}
            </section>
          ))}
        </div>
      )}

      {enabled && page === 'jobs' && (
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
                    ? 'No active subagent jobs right now (queued/running/cancelling). Settled jobs are listed under Completed.'
                    : 'No settled subagent jobs yet (completed/failed/cancelled). Active jobs are listed under Running.'}
              </div>
            )}
            {filteredOrder.map((id) => {
              const view = jobs[id];
              const job = view.job;
              const result = job.result;
              const settled = !isLiveJobStatus(job.status);
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
                      onClick={(e) => {
                        // Ownerless surface: relative <a href>/md-path clicks
                        // are blocked from default navigation BEFORE the
                        // stopPropagation that shields the card toggle (the
                        // document-level dispatcher never sees this event).
                        dispatchMarkdownPathClick(e);
                        e.stopPropagation();
                      }}
                    >
                      {result?.error ? (
                        <div className="subagent-job__error">{safeText(result.error)}</div>
                      ) : null}
                      <div className="subagent-job__output-md" data-path-navigation="disabled">
                        <MarkdownBody text={result?.output ?? ''} />
                      </div>
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

      {error && !detailsJob && (
        <div className="subagents-panel__error" data-panel-error>
          {safeText(error)}
        </div>
      )}
      {toast && (
        <div className="subagents-panel__toast" data-panel-toast>
          {safeText(toast)}
        </div>
      )}
      </div>
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
                  onClick={() => {
                    setAssignmentRevision((revision) => revision + 1);
                    fetchHistoryRef.current?.();
                  }}
                  title="Refetch the full task and child transcript now"
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
            <div className="subagent-details__body">
              {(error !== '' || assignmentError !== null) && (
                <div className="subagent-details__error" data-details-panel-error>
                  {error !== '' && <div>{safeText(error)}</div>}
                  {assignmentError !== null && (
                    <div data-details-assignment-error>
                      {safeText(`job_output failed for ${assignmentError.jobId}: ${assignmentError.message}`)}
                    </div>
                  )}
                </div>
              )}
              <div className="subagent-details__meta" data-details-status>
                <span
                  className={`subagent-job__status subagent-job__status--${detailsJob.job.status}`}
                  role="status"
                >
                  {(detailsJob.job.status === 'running' || detailsJob.job.status === 'cancelling') && (
                    <span className="subagent-details__status-pulse" aria-hidden="true" />
                  )}
                  {STATUS_LABEL[detailsJob.job.status]}
                </span>
                <span data-details-elapsed>{detailsElapsedLabel}</span>
              </div>
              <div className="subagent-details__section" data-details-task-section>
                <div className="subagent-details__label">Task</div>
                <div
                  className="subagent-details__description"
                  data-details-description
                  data-assignment-status={detailAssignment?.status ?? 'loading'}
                  data-path-navigation="disabled"
                  onClick={(e) => dispatchMarkdownPathClick(e)}
                >
                  {detailAssignment?.status === 'available' ? (
                    <MarkdownBody text={detailAssignment.text} />
                  ) : detailAssignment?.status === 'unavailable' ? (
                    <span>Full task unavailable ({safeText(detailAssignment.reason)}).</span>
                  ) : detailAssignment?.status === 'error' ? (
                    <span>Full task not loaded. Refresh to retry.</span>
                  ) : (
                    <span>Loading full task…</span>
                  )}
                </div>
              </div>
              {detailsJob.activity != null && detailsJob.activity.trim() !== '' && (
                <div className="subagent-details__section" data-details-activity-section>
                  <div className="subagent-details__label">Latest activity</div>
                  <div className="subagent-details__activity" data-details-activity>
                    {safeText(detailsJob.activity)}
                  </div>
                </div>
              )}
              <div className="subagent-details__section subagent-details__section--transcript">
                <div className="subagent-details__label">Recent transcript</div>
                {historyError !== '' && (
                  <div className="subagent-details__error" data-details-error>
                    {safeText(historyError)}
                  </div>
                )}
                {historyTruncation !== null && historyTruncation.omittedRows > 0 && (
                  <div
                    className="subagent-details__truncation"
                    data-details-truncation
                    data-truncation-reason={historyTruncation.reason}
                  >
                    {truncationNotice(historyTruncation)}
                  </div>
                )}
                {historyItems.length > 0 ? (
                  <div
                    className="transcript-content subagent-details__transcript"
                    data-details-history
                    // No session owner exists for the child: path chips and
                    // relative markdown links are inert here (markdown.ts).
                    data-path-navigation="disabled"
                  >
                    {renderHistory(historyItems)}
                  </div>
                ) : historyError !== '' ? null : historyLoading ? (
                  <div className="subagent-details__empty" data-details-loading>
                    Loading transcript…
                  </div>
                ) : (
                  <div className="subagent-details__empty" data-details-empty>
                    {historyTruncation !== null && historyTruncation.omittedRows > 0
                      ? 'The child transcript exceeds the history size limit — no messages fit.'
                      : 'No transcript yet — waiting for child activity.'}
                  </div>
                )}
              </div>
            </div>
          </div>
        </div>
      )}
    </aside>
  );
}



/** Resolve a human-facing agent label; falls back to the stable id. */
export function agentLabel(agents: Record<string, AgentWire>, agentId: string): string {
  const agent = agents[agentId];
  const name = agent?.displayName?.trim() || '';
  return name || agentId;
}
