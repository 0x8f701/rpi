// Workflow panel — master-detail mirror of the TUI workflow page.
//
// Wire shapes mirror pi-cli workflow_rpc.rs (serde camelCase) and the
// WorkflowPanelSnapshot projection in workflow_panel.rs. Commands reuse the
// existing workflow_* RPC surface; `workflow_detail` returns the live panel
// projection (supervisor state, planning activity feed, active tasks, workers
// with per-agent activity). EVERY model-derived string passes through
// safeText() before display.

import { useCallback, useEffect, useRef, useState } from 'react';
import { safeText } from '../redact';
import type { EventFrame } from '../types';

/* ------------------------------------------------------------------ *
 * Wire types (camelCase — see crates/pi-cli/src/workflow_rpc.rs and
 * crates/pi-cli/src/workflow_panel.rs)
 * ------------------------------------------------------------------ */

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
  text: string;
  atMs?: number;
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

export interface WorkflowPanelSnapshot {
  id: string;
  generation: number;
  name: string;
  objective: string;
  status: WorkflowStatus;
  supervisor?: WorkflowActorSnapshot | null;
  subagents: WorkflowActorSnapshot[];
  activeTasks: WorkflowActiveTaskSnapshot[];
  worktree?: WorkflowWorktreeSnapshot | null;
  integration?: WorkflowIntegrationSnapshot | null;
  planningActivity?: WorkflowActivitySnapshot[];
  planningStartedAtMs?: number | null;
  recentIrc?: WorkflowIrcSnapshot[];
}

interface WorkflowEventPayload extends EventFrame {
  workflowId?: string;
  generation?: number;
  status?: WorkflowStatus;
  snapshot?: WorkflowWireSnapshot;
}

/* ------------------------------------------------------------------ *
 * Workflow event fan-out: App dispatches workflow_* WS events here so the
 * mounted panel can refresh without prop-drilling. Module-level registry,
 * matching the liveNodes/streamBuf hot-path pattern in App.tsx.
 * ------------------------------------------------------------------ */

type WorkflowEventListener = (frame: WorkflowEventPayload) => void;

const workflowEventListeners = new Set<WorkflowEventListener>();

export function dispatchWorkflowEvents(frame: WorkflowEventPayload): void {
  for (const listener of workflowEventListeners) {
    try {
      listener(frame);
    } catch {
      /* a listener must never break the event dispatch */
    }
  }
}

function subscribeWorkflowEvents(listener: WorkflowEventListener): () => void {
  workflowEventListeners.add(listener);
  return () => {
    workflowEventListeners.delete(listener);
  };
}

/* ------------------------------------------------------------------ *
 * Status helpers
 * ------------------------------------------------------------------ */

const TERMINAL_STATUSES: ReadonlySet<WorkflowStatus> = new Set([
  'completed',
  'failed',
  'cancelled',
  'conflicted',
]);

function isTerminal(status: WorkflowStatus): boolean {
  return TERMINAL_STATUSES.has(status);
}

const STATUS_LABEL: Record<WorkflowStatus, string> = {
  queued: 'queued',
  planning: 'planning',
  running: 'running',
  paused: 'paused',
  integrating: 'integrating',
  completed: 'completed',
  failed: 'failed',
  cancelled: 'cancelled',
  conflicted: 'conflicted',
};

const STATUS_MARKER: Record<WorkflowStatus, string> = {
  queued: '\u25CB', // ○
  planning: '\u25D0', // ◐
  running: '\u25CF', // ●
  paused: '\u2161', // Ⅱ
  integrating: '\u21C4', // ⇄
  completed: '\u2713', // ✓
  failed: '!',
  cancelled: '\u00D7', // ×
  conflicted: '!',
};

/** Mirror of pi-coding's ensure_allowed (manager.rs) so buttons match the
 * server's lifecycle rules exactly. */
function canPause(status: WorkflowStatus): boolean {
  return status === 'queued' || status === 'planning' || status === 'running';
}
function canResume(status: WorkflowStatus): boolean {
  return status === 'paused' || status === 'planning' || status === 'running';
}
function canCancel(status: WorkflowStatus): boolean {
  return !isTerminal(status);
}
function canIntegrate(status: WorkflowStatus): boolean {
  return status === 'completed' || status === 'paused' || status === 'conflicted';
}
// Remove is always available: the manager cancels non-terminal workflows
// before removal, so the button only tracks in-flight action state.

function clock(ms: number): string {
  if (!ms) return '';
  const d = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

/* ------------------------------------------------------------------ *
 * Panel
 * ------------------------------------------------------------------ */

export interface WorkflowPanelProps {
  sendCommand: (command: Record<string, unknown>, bubbleId?: string) => Promise<unknown>;
  onClose: () => void;
}

const DETAIL_POLL_MS = 1500;
const ACTIVITY_FEED_LIMIT = 6;

export function WorkflowPanel({ sendCommand, onClose }: WorkflowPanelProps) {
  const [workflows, setWorkflows] = useState<WorkflowWireSnapshot[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [detail, setDetail] = useState<WorkflowPanelSnapshot | null>(null);
  const [detailError, setDetailError] = useState<string | null>(null);
  const [listError, setListError] = useState<string | null>(null);
  const [busy, setBusy] = useState<Set<string>>(new Set());
  const [createName, setCreateName] = useState('');
  const [createObjective, setCreateObjective] = useState('');
  const [creating, setCreating] = useState(false);
  const [createError, setCreateError] = useState<string | null>(null);
  const [expandedAgents, setExpandedAgents] = useState<Set<string>>(new Set());
  const [refreshing, setRefreshing] = useState(false);

  const selectedRef = useRef<string | null>(null);
  selectedRef.current = selectedId;
  const mountedRef = useRef(true);
  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);
  // Reject stale workflow_detail responses: only the newest request wins.
  const detailSeqRef = useRef(0);

  /* ---------------- list ---------------- */

  const refreshList = useCallback(
    (preserveSelection = true) => {
      setRefreshing(true);
      sendCommand({ type: 'workflow_list' })
        .then((data) => {
          const list = (data as { workflows?: WorkflowWireSnapshot[] }).workflows || [];
          setWorkflows(() => {
            const next = list.filter((w) => w && w.workflowId && w.status);
            if (preserveSelection && selectedRef.current) {
              const stillThere = next.some((w) => w.workflowId === selectedRef.current);
              if (stillThere) return next;
              setSelectedId(next[0]?.workflowId ?? null);
              return next;
            }
            if (!selectedRef.current) setSelectedId(next[0]?.workflowId ?? null);
            return next;
          });
          setListError(null);
        })
        .catch((err: Error) => {
          setListError(err.message);
        })
        .finally(() => setRefreshing(false));
    },
    [sendCommand]
  );

  /* ---------------- detail ---------------- */

  const refreshDetail = useCallback(
    (workflowId: string) => {
      setDetailError(null);
      const seq = ++detailSeqRef.current;
      sendCommand({ type: 'workflow_detail', workflowId })
        .then((data) => {
          if (seq !== detailSeqRef.current || !mountedRef.current) return;
          const panel = data as WorkflowPanelSnapshot;
          if (panel && panel.id) setDetail(panel);
        })
        .catch((err: Error) => {
          if (seq !== detailSeqRef.current || !mountedRef.current) return;
          setDetailError(err.message);
        });
    },
    [sendCommand]
  );

  const selectWorkflow = useCallback((workflowId: string) => {
    setSelectedId(workflowId);
  }, []);

  /* ---------------- live events ---------------- */

  useEffect(() => {
    return subscribeWorkflowEvents((frame) => {
      const workflowId = frame.workflowId || '';
      if (!workflowId) return;
      setWorkflows((prev) => {
        if (frame.type === 'workflow_removed') {
          const next = prev.filter((w) => w.workflowId !== workflowId);
          if (selectedRef.current === workflowId) {
            setSelectedId(next[0]?.workflowId ?? null);
          }
          return next;
        }
        if (frame.type === 'workflow_status_changed' && frame.status) {
          const found = prev.some((w) => w.workflowId === workflowId);
          if (!found) return prev;
          return prev.map((w) =>
            w.workflowId === workflowId
              ? {
                  ...w,
                  status: frame.status as WorkflowStatus,
                  name: typeof frame.name === 'string' ? (frame.name as string) : w.name,
                }
              : w
          );
        }
        if (frame.type === 'workflow_updated' && frame.snapshot) {
          const snap = frame.snapshot;
          const exists = prev.some((w) => w.workflowId === workflowId);
          const updated = exists
            ? prev.map((w) => (w.workflowId === workflowId ? snap : w))
            : [snap, ...prev];
          return updated;
        }
        return prev;
      });
      // A workflow that changed status or updated matters for the open
      // detail too: refresh it whenever the selected workflow is involved.
      if (selectedRef.current === workflowId) {
        refreshDetail(workflowId);
      }
    });
  }, [refreshDetail]);

  /* ---------------- initial fetch + live poll ---------------- */

  useEffect(() => {
    refreshList(false);
  }, [refreshList]);

  // Any selection change (click, create, event-driven reselect) fetches the
  // authoritative detail projection.
  useEffect(() => {
    if (!selectedId) return;
    refreshDetail(selectedId);
  }, [selectedId, refreshDetail]);

  useEffect(() => {
    if (!selectedId) return;
    const selected = workflows.find((w) => w.workflowId === selectedId);
    const live = selected && !isTerminal(selected.status);
    if (!live) return;
    const timer = window.setInterval(() => {
      refreshDetail(selectedRef.current || '');
    }, DETAIL_POLL_MS);
    return () => window.clearInterval(timer);
  }, [selectedId, workflows, refreshDetail]);

  /* ---------------- actions ---------------- */

  const runAction = useCallback(
    (command: string, workflowId: string) => {
      setBusy((prev) => new Set(prev).add(workflowId));
      sendCommand({ type: command, workflowId })
        .then(() => {
          refreshList(true);
          refreshDetail(workflowId);
        })
        .catch((err: Error & { rpc?: boolean }) => {
          if (err.rpc) {
            // The App already toasts RPC failures; surface the state too.
            setDetailError(err.message);
          }
          refreshList(true);
        })
        .finally(() => {
          setBusy((prev) => {
            const next = new Set(prev);
            next.delete(workflowId);
            return next;
          });
        });
    },
    [refreshDetail, refreshList, sendCommand]
  );

  const createWorkflow = useCallback(() => {
    const name = createName.trim();
    const objective = createObjective.trim();
    if (!name || !objective || creating) return;
    setCreating(true);
    setCreateError(null);
    sendCommand({ type: 'workflow_create', name, objective })
      .then((data) => {
        const snap = data as WorkflowWireSnapshot;
        setCreateName('');
        setCreateObjective('');
        if (snap && snap.workflowId) {
          refreshList(false);
          selectWorkflow(snap.workflowId);
        } else {
          refreshList(false);
        }
      })
      .catch((err: Error & { rpc?: boolean }) => {
        if (err.rpc) setCreateError(err.message);
      })
      .finally(() => setCreating(false));
  }, [createName, createObjective, creating, refreshList, selectWorkflow, sendCommand]);

  /* ---------------- derived ---------------- */

  const selected = workflows.find((w) => w.workflowId === selectedId) || null;
  const selectedBusy = selected ? busy.has(selected.workflowId) : false;
  const isSelectedLive = selected ? !isTerminal(selected.status) : false;

  const toggleAgent = useCallback((name: string) => {
    setExpandedAgents((prev) => {
      const next = new Set(prev);
      if (next.has(name)) next.delete(name);
      else next.add(name);
      return next;
    });
  }, []);

  const planningFeed = detail?.planningActivity || [];
  const planningFeedLines = planningFeed.slice(-ACTIVITY_FEED_LIMIT);

  return (
    <aside id="workflow-panel" className="workflow-panel" aria-label="Workflow panel">
      <div className="workflow-panel__head">
        <span className="workflow-panel__title">Workflows</span>
        <span className="workflow-panel__counts" id="workflow-counts" title="workflows listed">
          {workflows.length} workflow{workflows.length === 1 ? '' : 's'}
        </span>
        <button
          id="workflow-close-btn"
          type="button"
          className="workflow-panel__close"
          title="Close workflow panel"
          onClick={onClose}
        >
          ×
        </button>
      </div>

      <div className="workflow-panel__body">
        {/* ---------------- list pane ---------------- */}
        <section className="workflow-list" id="workflow-list" aria-label="Workflow list">
          <div className="workflow-create">
            <input
              id="workflow-create-name"
              className="workflow-create__name"
              placeholder="name"
              value={createName}
              onChange={(e) => setCreateName(e.target.value)}
              spellCheck={false}
            />
            <input
              id="workflow-create-objective"
              className="workflow-create__objective"
              placeholder="objective"
              value={createObjective}
              onChange={(e) => setCreateObjective(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') {
                  e.preventDefault();
                  createWorkflow();
                }
              }}
              spellCheck={false}
            />
            <button
              id="workflow-create-btn"
              type="button"
              disabled={creating || !createName.trim() || !createObjective.trim()}
              onClick={createWorkflow}
            >
              {creating ? 'Creating…' : 'Create'}
            </button>
          </div>
          {createError && <div className="workflow-panel__error">{safeText(createError)}</div>}
          {listError && <div className="workflow-panel__error">{safeText(listError)}</div>}

          <div className="workflow-list__rows">
            {workflows.length === 0 && !refreshing && (
              <div className="workflow-list__empty">
                No workflows yet. Create one above with a name + objective.
              </div>
            )}
            {workflows.map((w) => {
              const active = w.workflowId === selectedId;
              return (
                <div
                  key={w.workflowId}
                  id={`workflow-row-${w.workflowId}`}
                  className={`workflow-row${active ? ' is-selected' : ''}`}
                  data-workflow-id={w.workflowId}
                  data-status={w.status}
                  onClick={() => selectWorkflow(w.workflowId)}
                >
                  <div className="workflow-row__line">
                    <span
                      className={`workflow-row__marker workflow-row__marker--${w.status}`}
                      aria-label={STATUS_LABEL[w.status]}
                      title={STATUS_LABEL[w.status]}
                    >
                      {STATUS_MARKER[w.status]}
                    </span>
                    <span className="workflow-row__name" title={safeText(w.name)}>
                      {safeText(w.name)}
                    </span>
                    <span className="workflow-row__generation" title="generation">
                      gen {w.generation}
                    </span>
                  </div>
                  <div className="workflow-row__meta">
                    <span className={`workflow-row__status workflow-row__status--${w.status}`}>
                      {STATUS_LABEL[w.status]}
                    </span>
                    {w.worktree && (
                      <span className="workflow-row__worktree" title="worktree (redacted label)">
                        {safeText(w.worktree)}
                      </span>
                    )}
                  </div>
                  <div className="workflow-row__objective" title={safeText(w.objective)}>
                    {safeText(w.objective)}
                  </div>
                </div>
              );
            })}
          </div>
        </section>

        {/* ---------------- detail pane ---------------- */}
        <section className="workflow-detail" id="workflow-detail" aria-label="Workflow detail">
          {!selected ? (
            <div className="workflow-detail__empty">Select a workflow to inspect it.</div>
          ) : (
            <>
              <div className="workflow-detail__head">
                <span className="workflow-detail__name" title={safeText(selected.name)}>
                  {safeText(selected.name)}
                </span>
                <span className={`workflow-detail__status workflow-row__status--${selected.status}`}>
                  {STATUS_LABEL[selected.status]}
                </span>
              </div>
              <div className="workflow-detail__objective" title={safeText(selected.objective)}>
                {safeText(selected.objective)}
              </div>
              <div className="workflow-detail__meta">
                <span title="workflow id">{safeText(selected.workflowId)}</span>
                <span title="generation">gen {selected.generation}</span>
                {selected.branch && <span title="branch">{safeText(selected.branch)}</span>}
                {selected.worktree && <span title="worktree (redacted label)">{safeText(selected.worktree)}</span>}
              </div>
              {selected.failure && (
                <div className="workflow-detail__failure" title={safeText(selected.failure)}>
                  failure: {safeText(selected.failure)}
                </div>
              )}
              {selected.integration && (
                <div className="workflow-detail__integration" title={safeText(selected.integration)}>
                  integration: {safeText(selected.integration)}
                </div>
              )}

              <div className="workflow-actions">
                <button
                  id="workflow-pause-btn"
                  type="button"
                  disabled={selectedBusy || !canPause(selected.status)}
                  title="Pause this workflow (workflow_pause)"
                  onClick={() => runAction('workflow_pause', selected.workflowId)}
                >
                  Pause
                </button>
                <button
                  id="workflow-resume-btn"
                  type="button"
                  disabled={selectedBusy || !canResume(selected.status)}
                  title="Resume this workflow (workflow_resume)"
                  onClick={() => runAction('workflow_resume', selected.workflowId)}
                >
                  Resume
                </button>
                <button
                  id="workflow-cancel-btn"
                  type="button"
                  disabled={selectedBusy || !canCancel(selected.status)}
                  title="Cancel this workflow (workflow_cancel)"
                  onClick={() => runAction('workflow_cancel', selected.workflowId)}
                >
                  Cancel
                </button>
                <button
                  id="workflow-integrate-btn"
                  type="button"
                  disabled={selectedBusy || !canIntegrate(selected.status)}
                  title="Integrate the worktree back to the source branch (workflow_integrate)"
                  onClick={() => runAction('workflow_integrate', selected.workflowId)}
                >
                  Integrate
                </button>
                <button
                  id="workflow-remove-btn"
                  type="button"
                  disabled={selectedBusy}
                  title="Remove this workflow (workflow_remove; cancels first when not terminal)"
                  onClick={() => runAction('workflow_remove', selected.workflowId)}
                >
                  Remove
                </button>
              </div>

              {detailError && <div className="workflow-panel__error">{safeText(detailError)}</div>}

              {/* supervisor + planning activity feed */}
              <section className="workflow-section" aria-label="Supervisor">
                <h4 className="workflow-section__title">Supervisor</h4>
                {detail?.supervisor ? (
                  <ActorRow
                    actor={detail.supervisor}
                    expanded={expandedAgents.has(detail.supervisor.name)}
                    onToggle={toggleAgent}
                  />
                ) : (
                  <div className="workflow-section__hint">
                    {isSelectedLive ? 'starting supervisor…' : 'no supervisor attached'}
                  </div>
                )}
              </section>

              <section className="workflow-section" aria-label="Planning activity feed">
                <h4 className="workflow-section__title">
                  Planning activity
                  {detail?.planningStartedAtMs ? (
                    <span className="workflow-section__meta" title="planning phase started at">
                      started {clock(detail.planningStartedAtMs)}
                    </span>
                  ) : null}
                </h4>
                {planningFeed.length === 0 ? (
                  <div className="workflow-section__hint">
                    {isSelectedLive ? 'planning feed will stream here…' : 'no planning activity yet'}
                  </div>
                ) : (
                  <ul className="workflow-feed" id="workflow-detail-activity">
                    {planningFeedLines.map((entry, index) => (
                      <li
                        key={`${entry.atMs}-${index}`}
                        className={`workflow-feed__item workflow-feed__item--${entry.kind}`}
                        title={safeText(entry.text)}
                      >
                        <span className="workflow-feed__clock">{clock(entry.atMs)}</span>
                        <span className="workflow-feed__kind">{entry.kind}</span>
                        <span className="workflow-feed__text">{safeText(entry.text)}</span>
                      </li>
                    ))}
                  </ul>
                )}
              </section>

              {/* active tasks */}
              <section className="workflow-section" aria-label="Active tasks">
                <h4 className="workflow-section__title">Active tasks</h4>
                {!detail || detail.activeTasks.length === 0 ? (
                  <div className="workflow-section__hint">no active tasks</div>
                ) : (
                  <ul className="workflow-tasks" id="workflow-detail-tasks">
                    {detail.activeTasks.map((task, index) => (
                      <li key={task.taskId || `${task.summary}-${index}`} className="workflow-task">
                        <span className="workflow-task__bullet">●</span>
                        <span className="workflow-task__summary" title={safeText(task.summary)}>
                          {safeText(task.summary)}
                        </span>
                      </li>
                    ))}
                  </ul>
                )}
              </section>

              {/* workers strip */}
              <section className="workflow-section" aria-label="Workers">
                <h4 className="workflow-section__title">
                  Workers
                  <span className="workflow-section__meta">{detail?.subagents.length ?? 0} active</span>
                </h4>
                {!detail || detail.subagents.length === 0 ? (
                  <div className="workflow-section__hint">no workers yet</div>
                ) : (
                  <ul className="workflow-workers" id="workflow-detail-workers">
                    {detail.subagents.map((agent) => (
                      <li key={agent.name} className="workflow-worker" data-worker={agent.name}>
                        <ActorRow
                          actor={agent}
                          expanded={expandedAgents.has(agent.name)}
                          onToggle={toggleAgent}
                        />
                      </li>
                    ))}
                  </ul>
                )}
              </section>

              {/* recent IRC */}
              {detail && detail.recentIrc && detail.recentIrc.length > 0 && (
                <section className="workflow-section" aria-label="Recent IRC">
                  <h4 className="workflow-section__title">Recent IRC</h4>
                  <ul className="workflow-irc">
                    {detail.recentIrc.slice(-6).map((msg, index) => (
                      <li key={`${msg.sender}-${index}`} className="workflow-irc__item">
                        <span className="workflow-irc__sender">{safeText(msg.sender)}</span>
                        <span className="workflow-irc__text" title={safeText(msg.text)}>
                          {safeText(msg.text)}
                        </span>
                      </li>
                    ))}
                  </ul>
                </section>
              )}

              {/* integration step */}
              {detail?.integration && detail.integration.step !== 'pending' && (
                <section className="workflow-section" aria-label="Integration">
                  <h4 className="workflow-section__title">Integration</h4>
                  <div className="workflow-integration">
                    <span className={`workflow-integration__step workflow-integration__step--${detail.integration.step}`}>
                      {detail.integration.step}
                    </span>
                    <span>{safeText(detail.integration.summary)}</span>
                    {detail.integration.conflicts && detail.integration.conflicts.length > 0 && (
                      <span className="workflow-integration__conflicts" title="conflicting files">
                        conflicts: {detail.integration.conflicts.length}
                      </span>
                    )}
                  </div>
                </section>
              )}
            </>
          )}
        </section>
      </div>
    </aside>
  );
}

/* ------------------------------------------------------------------ *
 * Actor row: name + status + current task, with an expandable bounded
 * activity feed (the TUI's foldable agent rows).
 * ------------------------------------------------------------------ */

function ActorRow({
  actor,
  expanded,
  onToggle,
}: {
  actor: WorkflowActorSnapshot;
  expanded: boolean;
  onToggle: (name: string) => void;
}) {
  const activity = actor.activity || [];
  const feed = activity.slice(-5);
  const earlier = activity.length - feed.length;
  const foldable = activity.length > 0;
  return (
    <div className={`workflow-actor${foldable ? ' is-foldable' : ''}`}>
      <button
        type="button"
        className="workflow-actor__row"
        title={foldable ? 'Toggle activity feed' : safeText(actor.name)}
        onClick={() => foldable && onToggle(actor.name)}
      >
        <span className="workflow-actor__name" title={safeText(actor.name)}>
          {foldable ? (expanded ? '▾ ' : '▸ ') : ''}
          {safeText(actor.name)}
        </span>
        <span className={`workflow-actor__status workflow-actor__status--${actor.status}`}>
          {actor.status}
        </span>
        {actor.task && (
          <span className="workflow-actor__task" title={safeText(actor.task)}>
            {safeText(actor.task)}
          </span>
        )}
      </button>
      {expanded && (
        <ul className="workflow-actor__activity">
          {earlier > 0 && (
            <li className="workflow-actor__earlier">… {earlier} earlier</li>
          )}
          {feed.map((entry, index) => (
            <li
              key={`${entry.atMs}-${index}`}
              className={`workflow-actor__activity-item workflow-actor__activity-item--${entry.kind}`}
              title={safeText(entry.text)}
            >
              <span className="workflow-feed__clock">{clock(entry.atMs)}</span>
              <span className="workflow-actor__activity-kind">{entry.kind}</span>
              <span>{safeText(entry.text)}</span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
