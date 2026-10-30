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
import {
  WorkflowPanelAuthority,
  projectWorkflowListSnapshot,
  workflowActorNavigationKey,
  workflowActionError,
  workflowCommand,
  workflowIdentity,
  type WorkflowDetailRequest,
  type WorkflowActorSnapshot,
  type WorkflowEventPayload,
  type WorkflowIdentity,
  type WorkflowPanelSnapshot,
  type WorkflowStatus,
  type WorkflowWireSnapshot,
} from './workflowPanelState';

export type {
  WorkflowActiveTaskSnapshot,
  WorkflowActivitySnapshot,
  WorkflowActorSnapshot,
  WorkflowAgentActivitySnapshot,
  WorkflowIntegrationSnapshot,
  WorkflowIrcSnapshot,
  WorkflowJobSnapshot,
  WorkflowPanelSnapshot,
  WorkflowStatus,
  WorkflowWireSnapshot,
  WorkflowWorktreeSnapshot,
  WorkflowEventPayload,
} from './workflowPanelState';


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

/** Detail-pane failure row: the LIVE panel snapshot's failure is authoritative
 * while a runtime is resident (the supervisor's message may not have landed on
 * the durable record yet); the durable list snapshot is the fallback. Mirrors
 * WorkflowRuntimeDetail::from_live in detail.rs. */
export function WorkflowDetailFailure({
  selected,
  detail,
}: {
  selected: WorkflowWireSnapshot;
  detail: WorkflowPanelSnapshot | null;
}) {
  const failure = detail?.failure ?? selected.failure;
  if (!failure) return null;
  return (
    <div className="workflow-detail__failure" title={safeText(failure)}>
      failure: {safeText(failure)}
    </div>
  );
}

/* ------------------------------------------------------------------ *
 * Panel
 * ------------------------------------------------------------------ */

export interface IrcReceipt {
  to: string;
  outcome: 'queued' | 'woken' | 'revived' | 'processed' | 'failed' | string;
  requested?: string;
  messageId?: string;
  error?: string;
}

export interface WorkflowPanelProps {
  sessionId: string;
  sendCommand: (command: Record<string, unknown>, bubbleId?: string) => Promise<unknown>;
  subscribeEvents: (handler: (frame: WorkflowEventPayload) => void) => () => void;
  onClose: () => void;
}

const DETAIL_POLL_MS = 1500;
const ACTIVITY_FEED_LIMIT = 6;

export function WorkflowPanel({ sessionId, sendCommand, subscribeEvents, onClose }: WorkflowPanelProps) {
  const [workflows, setWorkflows] = useState<WorkflowWireSnapshot[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [detailSnapshot, setDetail] = useState<WorkflowPanelSnapshot | null>(null);
  const [detailError, setDetailError] = useState<string | null>(null);
  const [listError, setListError] = useState<string | null>(null);
  const [busy, setBusy] = useState<Set<string>>(new Set());
  const [createName, setCreateName] = useState('');
  const [createObjective, setCreateObjective] = useState('');
  const [creating, setCreating] = useState(false);
  const creatingRef = useRef(false);
  const [createError, setCreateError] = useState<string | null>(null);
  const [expandedAgents, setExpandedAgents] = useState<Set<string>>(new Set());
  const [refreshing, setRefreshing] = useState(false);
  // IRC send composer: recipient + body + last receipts. Receipt outcomes are
  // shown verbatim (queued/woken/revived/processed/failed) — "processed" is
  // the backend's delivery classification and is never reworded as success.
  const [ircRecipient, setIrcRecipient] = useState('');
  const [ircBody, setIrcBody] = useState('');
  const [ircSending, setIrcSending] = useState(false);
  const ircSendingRef = useRef(false);
  const [ircReceipts, setIrcReceipts] = useState<IrcReceipt[] | null>(null);
  const [ircError, setIrcError] = useState<string | null>(null);
  const createRequestRef = useRef(0);
  const ircRequestRef = useRef(0);
  const authorityRef = useRef(new WorkflowPanelAuthority(sessionId));
  // Synchronous mirror of `workflows` so event/list settlement can compute the
  // next list OUTSIDE a state updater — removed-workflow handling carries side
  // effects (tombstone, selection clear, list refresh) that must never run
  // inside an updater.
  const workflowsRef = useRef<WorkflowWireSnapshot[]>([]);

  const selectedRef = useRef<string | null>(null);
  selectedRef.current = selectedId;
  const mountedRef = useRef(true);
  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);
  const selectedIdentityRef = useRef<WorkflowIdentity | null>(null);

  /* ---------------- list ---------------- */

  const refreshList = useCallback(
    (preserveSelection = true): Promise<void> => {
      const request = authorityRef.current.beginList();
      setRefreshing(true);
      // The returned promise settles when THIS list response settles (either
      // outcome). Lifecycle actions chain on it so their authority gate can
      // outlive the action RPC and hold until the list is authoritative.
      return sendCommand(workflowCommand(sessionId, { type: 'workflow_list' }))
        .then((data) => {
          if (!mountedRef.current || !authorityRef.current.isCurrentList(request)) return;
          const next = authorityRef.current.acceptList(request, workflowsRef.current, data);
          if (!next) return;
          workflowsRef.current = next;
          setWorkflows(next);
          if (preserveSelection && selectedRef.current) {
            const stillThere = next.some((workflow) => workflow.workflowId === selectedRef.current);
            if (!stillThere) setSelectedId(next[0]?.workflowId ?? null);
          } else if (!selectedRef.current) {
            setSelectedId(next[0]?.workflowId ?? null);
          }
          setListError(null);
        })
        .catch((err: Error) => {
          if (mountedRef.current && authorityRef.current.isCurrentList(request)) setListError(err.message);
        })
        .finally(() => {
          if (mountedRef.current && authorityRef.current.isCurrentList(request)) setRefreshing(false);
        });
    },
    [sendCommand, sessionId]
  );

  /* ---------------- detail ---------------- */

  const refreshDetail: (identity: WorkflowIdentity) => void = useCallback(
    (identity: WorkflowIdentity) => {
      // Physical single-flight admission: an identical
      // sessionId+workflowId+generation request already on the wire
      // coalesces (returns null); a different identity records the one
      // latest desired identity and waits for the physical settle — the
      // concurrency limit can never be reached by polling.
      const request = authorityRef.current.beginDetail(identity);
      if (!request) return;
      setDetailError(null);
      const send = (current: WorkflowDetailRequest): void => {
        sendCommand(workflowCommand(sessionId, { type: 'workflow_detail', workflowId: current.identity.workflowId }))
          .then((data) => {
            if (!mountedRef.current) return;
            const panel = authorityRef.current.acceptDetail(current, selectedIdentityRef.current, data);
            if (panel) setDetail(panel);
          })
          .catch((err: Error) => {
            if (!mountedRef.current) return;
            // Superseded/cancelled requests AND removed-workflow failures are
            // discarded silently: a "workflow was not found" arriving after an
            // authoritative remove is a stale response, never an error.
            if (!authorityRef.current.isCurrentDetail(current) || authorityRef.current.isRemoved(current.identity.workflowId)) return;
            setDetailError(workflowActionError(err));
          })
          .finally(() => {
            if (!mountedRef.current) return;
            // Physical settle: release the on-wire slot, then launch AT MOST
            // ONE latest desired identity — only if it is still exactly the
            // selected identity and still exists in the accepted list. The
            // recursion re-admits through every gate (removed/action-busy).
            const desired = authorityRef.current.settleDetail(current);
            if (!desired) return;
            const selected = selectedIdentityRef.current;
            if (
              !selected ||
              selected.sessionId !== desired.sessionId ||
              selected.workflowId !== desired.workflowId ||
              selected.generation !== desired.generation
            ) return;
            if (!workflowsRef.current.some((workflow) => workflow.workflowId === desired.workflowId)) return;
            refreshDetail(desired);
          });
      };
      send(request);
    },
    [sendCommand, sessionId]
  );

  const selectWorkflow = useCallback((workflowId: string) => {
    authorityRef.current.cancelDetail();
    ircRequestRef.current += 1;
    ircSendingRef.current = false;
    setDetail(null);
    setDetailError(null);
    setExpandedAgents(new Set());
    setIrcSending(false);
    setIrcReceipts(null);
    setIrcError(null);
    setSelectedId(workflowId);
  }, []);

  /* ---------------- irc send ---------------- */

  const sendIrc = useCallback(
    (workflowId: string, generation: number) => {
      const body = ircBody.trim();
      const recipient = ircRecipient.trim();
      if (!body || !recipient || ircSendingRef.current) return;
      ircSendingRef.current = true;
      setIrcSending(true);
      setIrcError(null);
      const requestId = ++ircRequestRef.current;
      const identity = workflowIdentity(sessionId, { workflowId, generation });
      const isCurrent = () => {
        const selected = selectedIdentityRef.current;
        return mountedRef.current &&
          requestId === ircRequestRef.current &&
          selected?.sessionId === identity.sessionId &&
          selected.workflowId === identity.workflowId &&
          selected.generation === identity.generation;
      };
      sendCommand(workflowCommand(sessionId, {
        type: 'workflow_hub_send', workflowId, generation, recipient, body,
      }))
        .then((data) => {
          if (!isCurrent()) return;
          const payload = data as { receipts?: IrcReceipt[] };
          setIrcReceipts(payload.receipts ?? []);
          setIrcBody((current) => current.trim() === body ? '' : current);
        })
        .catch((err: Error) => {
          if (isCurrent()) setIrcError(err.message);
        })
        .finally(() => {
          if (isCurrent()) {
            ircSendingRef.current = false;
            setIrcSending(false);
          }
        });
    },
    [ircBody, ircRecipient, sendCommand, sessionId]
  );

  /* ---------------- live events ---------------- */

  useEffect(() => {
    return subscribeEvents((frame) => {
      const application = authorityRef.current.applyEvent(frame, workflowsRef.current);
      if (!application) return;
      workflowsRef.current = application.workflows;
      setWorkflows(application.workflows);
      if (selectedRef.current === application.workflowId) {
        setDetail(null);
        authorityRef.current.cancelDetail();
        if (application.removed) {
          // Authoritative removal event: applyEvent already tombstoned the
          // identity. Clear selection + detail and refresh the LIST only —
          // the removed id is never detailed again.
          setSelectedId(application.workflows[0]?.workflowId ?? null);
          ircRequestRef.current += 1;
          ircSendingRef.current = false;
          setIrcSending(false);
          refreshList(true);
        }
      }
    });
  }, [refreshList, subscribeEvents]);

  /* ---------------- initial fetch + live poll ---------------- */

  useEffect(() => {
    // refreshList returns its settlement promise (for action gating); the
    // mount effect must not return it (React forbids non-cleanup returns).
    void refreshList(false);
  }, [refreshList]);

  useEffect(() => {
    const selected = workflows.find((workflow) => workflow.workflowId === selectedId) ?? null;
    const identity = selected ? workflowIdentity(sessionId, selected) : null;
    selectedIdentityRef.current = identity;
    if (!identity) {
      authorityRef.current.cancelDetail();
      setDetail(null);
      return;
    }
    // A removed identity is never detailed; while a lifecycle action is
    // admitted for this workflow, NO effect (initial/list/event) may restart
    // its detail stream — the settlement path owns exactly one refresh.
    if (authorityRef.current.isRemoved(identity.workflowId) || authorityRef.current.isActionBusy(identity.workflowId)) return;
    refreshDetail(identity);
  }, [selectedId, workflows, refreshDetail, sessionId]);

  useEffect(() => {
    const selected = workflows.find((workflow) => workflow.workflowId === selectedId);
    const live = selected && !isTerminal(selected.status);
    if (!live) return;
    const timer = window.setInterval(() => {
      const identity = selectedIdentityRef.current;
      if (!identity) return;
      // Same authority gates as the selection effect: removed identities and
      // in-flight actions suppress the poll; single-flight coalesces ticks
      // that race an open request.
      if (authorityRef.current.isRemoved(identity.workflowId) || authorityRef.current.isActionBusy(identity.workflowId)) return;
      refreshDetail(identity);
    }, DETAIL_POLL_MS);
    return () => window.clearInterval(timer);
  }, [selectedId, workflows, refreshDetail]);

  /* ---------------- actions ---------------- */

  const runAction = useCallback(
    (command: string, identity: WorkflowIdentity) => {
      // Admission: synchronously invalidate the detail authority for this
      // workflow (any in-flight detail becomes stale), clear the stale detail
      // error, and mark the row busy. Until settlement, no poll/effect may
      // restart detail — zero further detail RPCs for this workflow.
      authorityRef.current.beginAction(identity.workflowId);
      setBusy((previous) => new Set(previous).add(identity.workflowId));
      setDetailError(null);
      sendCommand(workflowCommand(sessionId, { type: command, workflowId: identity.workflowId }))
        .then(() => {
          if (command === 'workflow_remove') {
            // Remove is a terminal tombstone: the identity can never be
            // detailed again. Clear the selection's detail and refresh the
            // LIST only — never detail the removed id.
            authorityRef.current.tombstoneWorkflow(identity.workflowId);
            if (selectedRef.current === identity.workflowId) {
              setDetail(null);
              authorityRef.current.cancelDetail();
            }
            // Chain on the list settlement so the gate below outlives the
            // action RPC: no poll may detail between the action response and
            // the authoritative list.
            return refreshList(true);
          }
          // Cancel/pause/resume/integrate: refresh the list ONCE. When the
          // list authority accepts it and the workflow still exists and is
          // selected, the selection effect issues exactly one detail refresh
          // (single-flight coalesces anything racing it).
          return refreshList(true);
        })
        .catch((err: Error & { rpc?: boolean }) => {
          // EVERY current action rejection — transport, timeout, or business
          // — surfaces exactly once as a bounded panel error row (the global
          // RPC layer stays silent for workflow_*). Suppression is
          // identity-based only: a tombstoned workflow (authoritative remove)
          // or a different current selection never renders an error. Never an
          // error-message regex.
          const selected = selectedIdentityRef.current;
          if (
            mountedRef.current &&
            selected !== null &&
            selected.sessionId === identity.sessionId &&
            selected.workflowId === identity.workflowId &&
            selected.generation === identity.generation &&
            !authorityRef.current.isRemoved(identity.workflowId)
          ) {
            setDetailError(workflowActionError(err));
          }
          return refreshList(true);
        })
        .finally(() => {
          // The gate releases only after the settlement list response settled
          // (success or failure): between the action response and the
          // authoritative list, no poll/effect may detail this workflow.
          authorityRef.current.endAction(identity.workflowId);
          setBusy((previous) => {
            const next = new Set(previous);
            next.delete(identity.workflowId);
            return next;
          });
        });
    },
    [refreshList, sendCommand, sessionId]
  );

  const createWorkflow = useCallback(() => {
    const name = createName.trim();
    const objective = createObjective.trim();
    if (!name || !objective || creatingRef.current) return;
    creatingRef.current = true;
    setCreating(true);
    setCreateError(null);
    const requestId = ++createRequestRef.current;
    const selectionAtStart = selectedRef.current;
    sendCommand(workflowCommand(sessionId, { type: 'workflow_create', name, objective }))
      .then((data) => {
        if (!mountedRef.current || requestId !== createRequestRef.current) return;
        const snapshot = projectWorkflowListSnapshot(data);
        setCreateName((current) => current.trim() === name ? '' : current);
        setCreateObjective((current) => current.trim() === objective ? '' : current);
        refreshList(false);
        if (snapshot && selectedRef.current === selectionAtStart) selectWorkflow(snapshot.workflowId);
      })
      .catch((err: Error & { rpc?: boolean }) => {
        // The create composer owns its error surface: EVERY current rejection
        // — transport, timeout, or business — renders once, bounded. A stale
        // request (superseded or unmounted) renders nothing.
        if (mountedRef.current && requestId === createRequestRef.current) {
          setCreateError(workflowActionError(err));
        }
      })
      .finally(() => {
        if (mountedRef.current && requestId === createRequestRef.current) {
          creatingRef.current = false;
          setCreating(false);
        }
      });
  }, [createName, createObjective, refreshList, selectWorkflow, sendCommand, sessionId]);

  /* ---------------- derived ---------------- */

  const selected = workflows.find((w) => w.workflowId === selectedId) || null;
  const selectedIdentity = selected ? workflowIdentity(sessionId, selected) : null;
  const detail = detailSnapshot &&
    selectedIdentity &&
    detailSnapshot.id === selectedIdentity.workflowId &&
    detailSnapshot.generation >= selectedIdentity.generation
    ? detailSnapshot
    : null;
  const selectedBusy = selected ? busy.has(selected.workflowId) : false;
  const isSelectedLive = selected ? !isTerminal(selected.status) : false;

  const toggleAgent = useCallback((name: string) => {
    const identity = selectedIdentityRef.current;
    if (!identity) return;
    const key = workflowActorNavigationKey(identity, name);
    setExpandedAgents((previous) => {
      const next = new Set(previous);
      if (next.has(key)) next.delete(key);
      else next.add(key);
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
          aria-label="Close workflow panel"
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
                  {w.failure && (
                    <div className="workflow-row__failure" title={safeText(w.failure)}>
                      failure: {safeText(w.failure)}
                    </div>
                  )}
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
              <WorkflowDetailFailure selected={selected} detail={detail} />
              {selected.integration && (
                <div className="workflow-detail__integration" title={safeText(selected.integration)}>
                  integration: {safeText(selected.integration)}
                </div>
              )}

              <div className="workflow-actions">
                <button
                  id="workflow-pause-btn"
                  type="button"
                  disabled={selectedBusy || !canPause(selected.status) || !selectedIdentity}
                  title="Pause this workflow (workflow_pause)"
                  onClick={() => selectedIdentity && runAction('workflow_pause', selectedIdentity)}
                >
                  Pause
                </button>
                <button
                  id="workflow-resume-btn"
                  type="button"
                  disabled={selectedBusy || !canResume(selected.status) || !selectedIdentity}
                  title="Resume this workflow (workflow_resume)"
                  onClick={() => selectedIdentity && runAction('workflow_resume', selectedIdentity)}
                >
                  Resume
                </button>
                <button
                  id="workflow-cancel-btn"
                  type="button"
                  disabled={selectedBusy || !canCancel(selected.status) || !selectedIdentity}
                  title="Cancel this workflow (workflow_cancel)"
                  onClick={() => selectedIdentity && runAction('workflow_cancel', selectedIdentity)}
                >
                  Cancel
                </button>
                <button
                  id="workflow-integrate-btn"
                  type="button"
                  disabled={selectedBusy || !canIntegrate(selected.status) || !selectedIdentity}
                  title="Integrate the worktree back to the source branch (workflow_integrate)"
                  onClick={() => selectedIdentity && runAction('workflow_integrate', selectedIdentity)}
                >
                  Integrate
                </button>
                <button
                  id="workflow-remove-btn"
                  type="button"
                  disabled={selectedBusy || !selectedIdentity}
                  title="Remove this workflow (workflow_remove; cancels first when not terminal)"
                  onClick={() => selectedIdentity && runAction('workflow_remove', selectedIdentity)}
                >
                  Remove
                </button>
              </div>

              {detailError && <div className="workflow-panel__error">{safeText(detailError)}</div>}

              {/* supervisor + planning activity feed */}
              <section className="workflow-section" aria-label="Supervisor">
                <h4 className="workflow-section__title">Supervisor</h4>
                {detail?.supervisor && selectedIdentity ? (
                  <ActorRow
                    actor={detail.supervisor}
                    expanded={expandedAgents.has(workflowActorNavigationKey(selectedIdentity, detail.supervisor.name))}
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
                          expanded={Boolean(selectedIdentity && expandedAgents.has(workflowActorNavigationKey(selectedIdentity, agent.name)))}
                          onToggle={toggleAgent}
                        />
                      </li>
                    ))}
                  </ul>
                )}
              </section>

              {/* recent IRC */}
              {detail && (
                <section className="workflow-section" aria-label="Recent IRC">
                  <h4 className="workflow-section__title">Recent IRC</h4>
                  {detail.recentIrc && detail.recentIrc.length > 0 && (
                    <ul className="workflow-irc">
                      {detail.recentIrc.slice(-6).map((msg, index) => (
                        <li key={`${msg.sender}-${index}`} className="workflow-irc__item">
                          <span className="workflow-irc__sender">{safeText(msg.sender)}</span>
                          <span className="workflow-irc__time">
                            {new Date(msg.atMs || Date.now()).toLocaleTimeString()}
                          </span>
                          <span className="workflow-irc__text" title={safeText(msg.text)}>
                            {safeText(msg.text)}
                          </span>
                        </li>
                      ))}
                    </ul>
                  )}
                  {/* hub send composer: recipient from the workflow's agents */}
                  <form
                    className="workflow-irc__composer"
                    aria-label="Send workflow message"
                    onSubmit={(event) => {
                      event.preventDefault();
                      if (detail.id && detail.generation > 0) {
                        sendIrc(detail.id, detail.generation);
                      }
                    }}
                  >
                    <select
                      className="workflow-irc__recipient"
                      aria-label="Recipient"
                      value={ircRecipient}
                      onChange={(event) => setIrcRecipient(event.target.value)}
                      disabled={ircSending}
                    >
                      <option value="">recipient…</option>
                      {[detail.supervisor, ...detail.subagents]
                        .filter((agent): agent is NonNullable<typeof agent> => Boolean(agent))
                        .map((agent) => (
                          <option key={agent.name} value={agent.name}>
                            {safeText(agent.name)}
                            {agent.role ? ` (${safeText(agent.role)})` : ''}
                          </option>
                        ))}
                    </select>
                    <input
                      className="workflow-irc__body"
                      aria-label="Message body"
                      value={ircBody}
                      placeholder="message to send…"
                      onChange={(event) => setIrcBody(event.target.value)}
                      disabled={ircSending}
                    />
                    <button
                      type="submit"
                      className="workflow-irc__send"
                      disabled={ircSending || !ircBody.trim() || !ircRecipient.trim()}
                    >
                      {ircSending ? 'sending…' : 'send'}
                    </button>
                  </form>
                  {ircError && (
                    <p className="workflow-irc__error" role="alert">{safeText(ircError)}</p>
                  )}
                  {ircReceipts && (
                    <ul className="workflow-irc__receipts" aria-label="Delivery receipts">
                      {ircReceipts.map((receipt, index) => {
                        const presentation = receiptPresentation(receipt);
                        return (
                          <li
                            key={`${receipt.to}-${index}`}
                            className={`workflow-irc__receipt workflow-irc__receipt--${presentation.tone}`}
                          >
                            <span className="workflow-irc__receipt-to">{safeText(presentation.target)}</span>
                            <span className={`workflow-irc__receipt-outcome workflow-irc__receipt-outcome--${presentation.tone}`}>
                              {presentation.outcome}
                            </span>
                            {presentation.messageId && (
                              <span className="workflow-irc__receipt-message" title={safeText(presentation.messageId)}>
                                message {safeText(presentation.messageId)}
                              </span>
                            )}
                            {presentation.error && (
                              <span className="workflow-irc__receipt-error" title={safeText(presentation.error)}>
                                {safeText(presentation.error)}
                              </span>
                            )}
                          </li>
                        );
                      })}
                    </ul>
                  )}
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
 * Receipt presentation: outcome verbatim from the backend contract. The
 * pending set (queued/woken/revived) reads as "in flight", processed is the
 * backend's own delivery classification (never reworded), failed carries the
 * error text.
 * ------------------------------------------------------------------ */

export type ReceiptTone = 'pending' | 'processed' | 'failed';

export function receiptTone(outcome: string): ReceiptTone {
  switch (outcome) {
    case 'queued':
    case 'woken':
    case 'revived':
      return 'pending';
    case 'processed':
      return 'processed';
    default:
      return 'failed';
  }
}
export interface ReceiptPresentation {
  target: string;
  tone: ReceiptTone;
  outcome: string;
  error: string;
  messageId: string;
}

export function receiptPresentation(receipt: IrcReceipt): ReceiptPresentation {
  return {
    target: receipt.requested || receipt.to,
    tone: receiptTone(receipt.outcome),
    outcome: receipt.outcome,
    error: receipt.error || '',
    messageId: receipt.messageId || '',
  };
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
