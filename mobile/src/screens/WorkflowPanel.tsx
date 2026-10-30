// Workflow panel — native master/detail mirror of the Web workflow panel
// (crates/pi-cli/web/src/panels/WorkflowPanel.tsx). Wire shapes come from the
// existing fail-closed protocol readers; no endpoint is invented. Action
// availability comes from the TYPED status (pi-coding `ensure_allowed`), never
// from prose. Every model-derived string is redacted and bounded.

import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { ScrollView, Text, TextInput, TouchableOpacity, View } from 'react-native';
import Icon from 'react-native-vector-icons/MaterialIcons';
import { formatDuration } from '../jobCard';
import {
  parseWorkflowDetail,
  parseWorkflowHubSendResult,
  parseWorkflowList,
  parseWorkflowSnapshotRow,
  type HubReceiptWire,
  type RunnerCommand,
  type WorkflowActorWire,
  type WorkflowDetailData,
  type WorkflowSnapshotData,
  type WorkflowStatusWire,
} from '../protocol';
import { redactSecrets } from '../redact';
import type { RunnerClient } from '../runnerClient';
import { AppBar, styles, type Palette } from '../ui';

const DETAIL_POLL_MS = 1500;
const PLANNING_FEED_LIMIT = 6;
const IRC_FEED_LIMIT = 6;
const ACTOR_ACTIVITY_LIMIT = 5;
const ERROR_LIMIT = 800;
const TEXT_LIMIT = 240;

const STATUS_LABEL: Record<WorkflowStatusWire, string> = {
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

const STATUS_MARKER: Record<WorkflowStatusWire, string> = {
  queued: '\u25CB',
  planning: '\u25D0',
  running: '\u25CF',
  paused: '\u2161',
  integrating: '\u21C4',
  completed: '\u2713',
  failed: '!',
  cancelled: '\u00D7',
  conflicted: '!',
};

type WorkflowAction = 'pause' | 'resume' | 'cancel' | 'integrate' | 'remove';
type WorkflowLifecycleCommand = Extract<RunnerCommand, {
  type: 'workflow_pause' | 'workflow_resume' | 'workflow_cancel' | 'workflow_integrate' | 'workflow_remove';
}>;

const ACTIONS: WorkflowAction[] = ['pause', 'resume', 'cancel', 'integrate', 'remove'];

const ACTION_LABEL: Record<WorkflowAction, string> = {
  pause: 'Pause',
  resume: 'Resume',
  cancel: 'Cancel',
  integrate: 'Integrate',
  remove: 'Remove',
};

const ACTION_COMMAND: Record<WorkflowAction, WorkflowLifecycleCommand['type']> = {
  pause: 'workflow_pause',
  resume: 'workflow_resume',
  cancel: 'workflow_cancel',
  integrate: 'workflow_integrate',
  remove: 'workflow_remove',
};

const ACTION_ICON: Record<WorkflowAction, string> = {
  pause: 'pause',
  resume: 'play-arrow',
  cancel: 'close',
  integrate: 'call-merge',
  remove: 'delete',
};

interface WorkflowIdentity {
  sessionId: string;
  workflowId: string;
  generation: number;
}

function safe(text: string, limit: number): string {
  return redactSecrets(text).slice(0, limit);
}

function isTerminal(status: WorkflowStatusWire): boolean {
  return status === 'completed' || status === 'failed' || status === 'cancelled' || status === 'conflicted';
}

/** Mirrors pi-coding `ensure_allowed` (workflow/manager.rs:1295-1318). Remove
 *  stays available: the manager cancels a non-terminal workflow first. */
function lifecycleAllowed(status: WorkflowStatusWire): Record<WorkflowAction, boolean> {
  return {
    pause: status === 'queued' || status === 'planning' || status === 'running',
    resume: status === 'paused' || status === 'planning' || status === 'running',
    cancel: !isTerminal(status),
    integrate: status === 'completed' || status === 'paused' || status === 'conflicted'
      || status === 'failed' || status === 'integrating',
    remove: true,
  };
}

function clock(ms: number): string {
  if (!ms) return '';
  const date = new Date(ms);
  const pad = (value: number) => String(value).padStart(2, '0');
  return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

/** Receipt tone verbatim from the backend contract: the pending set reads as in
 *  flight, `processed` is the backend's own classification, else a failure. */
function receiptColor(outcome: string, palette: Palette): string {
  if (outcome === 'queued' || outcome === 'woken' || outcome === 'revived') return palette.muted;
  return outcome === 'processed' ? palette.ok : palette.error;
}

function actorKey(identity: WorkflowIdentity, name: string): string {
  return `${identity.sessionId}:${identity.workflowId}:${identity.generation}:${name}`;
}

function ActorRow(props: {
  actor: WorkflowActorWire;
  expanded: boolean;
  palette: Palette;
  onToggle: () => void;
}): React.JSX.Element {
  const { actor, palette } = props;
  const feed = actor.activity.slice(-ACTOR_ACTIVITY_LIMIT);
  const earlier = actor.activity.length - feed.length;
  const foldable = actor.activity.length > 0;
  return (
    <View>
      <TouchableOpacity
        disabled={!foldable}
        onPress={props.onToggle}
        accessibilityLabel={`Toggle activity ${safe(actor.name, 160)}`}>
        <Text style={[styles.rowSub, { color: palette.fg }]} numberOfLines={2}>
          {`${foldable ? (props.expanded ? '\u25BE ' : '\u25B8 ') : ''}${safe(actor.name, 160)} \u00B7 ${safe(actor.status, 80)}${actor.role ? ` \u00B7 ${safe(actor.role, 160)}` : ''}`}
        </Text>
        {actor.task ? (
          <Text style={[styles.rowSub, { color: palette.muted }]} numberOfLines={2}>{safe(actor.task, 600)}</Text>
        ) : null}
      </TouchableOpacity>
      {props.expanded && feed.length > 0 ? (
        <View style={{ paddingLeft: 12 }}>
          {earlier > 0 ? (
            <Text style={[styles.cardMeta, { color: palette.muted }]}>{`\u2026 ${earlier} earlier`}</Text>
          ) : null}
          {feed.map((entry, index) => (
            <Text
              key={`${entry.atMs}-${index}`}
              style={[styles.cardMeta, { color: palette.muted }]}
              numberOfLines={2}>
              {`${clock(entry.atMs)} ${safe(entry.kind, 80)} ${safe(entry.text, TEXT_LIMIT)}`}
            </Text>
          ))}
        </View>
      ) : null}
    </View>
  );
}

export function WorkflowPanel(props: {
  palette: Palette;
  client: RunnerClient;
  sessionId: string;
  onClose: () => void;
  onError: (message: string) => void;
}): React.JSX.Element {
  const { palette } = props;
  const [workflows, setWorkflows] = useState<WorkflowSnapshotData[] | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [detail, setDetail] = useState<WorkflowDetailData | null>(null);
  const [nameDraft, setNameDraft] = useState('');
  const [objectiveDraft, setObjectiveDraft] = useState('');
  const [busy, setBusy] = useState(false);
  const [pendingRemove, setPendingRemove] = useState(false);
  const [expandedActors, setExpandedActors] = useState<ReadonlySet<string>>(new Set());
  const [recipient, setRecipient] = useState('');
  const [messageDraft, setMessageDraft] = useState('');
  const [sending, setSending] = useState(false);
  const [receipts, setReceipts] = useState<HubReceiptWire[] | null>(null);
  const listRequestRef = useRef(0);
  const detailRequestRef = useRef(0);
  const detailInFlightRef = useRef<number | null>(null);
  const createRequestRef = useRef(0);
  const ircRequestRef = useRef(0);
  const identityRef = useRef<WorkflowIdentity | null>(null);
  const selectedRef = useRef<string | null>(null);
  const busyRef = useRef(false);
  const sendingRef = useRef(false);
  const removedRef = useRef<Set<string>>(new Set());
  const mountedRef = useRef(true);
  const onErrorRef = useRef(props.onError);
  selectedRef.current = selectedId;
  onErrorRef.current = props.onError;

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const report = useCallback((context: string, error: unknown) => {
    const message = error instanceof Error ? error.message : String(error);
    onErrorRef.current(safe(`${context}: ${message}`, ERROR_LIMIT));
  }, []);

  const refreshList = useCallback(async (): Promise<void> => {
    const requestId = ++listRequestRef.current;
    try {
      const response = await props.client.command({ type: 'workflow_list' }, props.sessionId);
      if (!response.success) throw new Error(response.error || 'workflow_list failed');
      const rows = parseWorkflowList(response.data);
      if (!rows) throw new Error('workflow_list response is invalid');
      if (requestId !== listRequestRef.current || !mountedRef.current) return;
      // A removed id is never listed again: a response that raced the removal
      // cannot resurrect the row.
      const next = rows.filter((row) => !removedRef.current.has(row.workflowId));
      setWorkflows(next);
      const current = selectedRef.current;
      if (current === null || !next.some((row) => row.workflowId === current)) {
        setSelectedId(next[0]?.workflowId ?? null);
      }
    } catch (error) {
      if (requestId !== listRequestRef.current || !mountedRef.current) return;
      report('workflow list', error);
    }
  }, [props.client, props.sessionId, report]);

  const refreshDetail = useCallback(async (identity: WorkflowIdentity): Promise<void> => {
    if (busyRef.current || removedRef.current.has(identity.workflowId)) return;
    const requestId = ++detailRequestRef.current;
    detailInFlightRef.current = requestId;
    try {
      const response = await props.client.command(
        { type: 'workflow_detail', workflowId: identity.workflowId },
        props.sessionId,
      );
      if (!response.success) throw new Error(response.error || 'workflow_detail failed');
      const parsed = parseWorkflowDetail(response.data);
      if (!parsed) throw new Error('workflow_detail response is invalid');
      // A late response may only replace the detail of the selection it was
      // requested for.
      const selected = identityRef.current;
      if (requestId !== detailRequestRef.current || !mountedRef.current) return;
      if (!selected || selected.workflowId !== identity.workflowId) return;
      if (selected.sessionId !== identity.sessionId || selected.generation !== identity.generation) return;
      if (parsed.id !== identity.workflowId || parsed.generation < identity.generation) return;
      setDetail(parsed);
    } catch (error) {
      // A failed refresh preserves the last-good detail: nothing is cleared.
      if (requestId !== detailRequestRef.current || !mountedRef.current) return;
      report('workflow detail', error);
    } finally {
      if (detailInFlightRef.current === requestId) detailInFlightRef.current = null;
    }
  }, [props.client, props.sessionId, report]);

  const selectWorkflow = useCallback((workflowId: string) => {
    if (selectedRef.current === workflowId) return;
    detailRequestRef.current += 1;
    detailInFlightRef.current = null;
    ircRequestRef.current += 1;
    sendingRef.current = false;
    setSending(false);
    setReceipts(null);
    setRecipient('');
    setMessageDraft('');
    setExpandedActors(new Set());
    setPendingRemove(false);
    setDetail(null);
    setSelectedId(workflowId);
  }, []);

  const createWorkflow = useCallback(async (): Promise<void> => {
    const name = nameDraft.trim();
    const objective = objectiveDraft.trim();
    if (!name || !objective || busyRef.current) return;
    const requestId = ++createRequestRef.current;
    const selectionAtStart = selectedRef.current;
    busyRef.current = true;
    setBusy(true);
    try {
      const response = await props.client.command({ type: 'workflow_create', name, objective }, props.sessionId);
      if (!response.success) throw new Error(response.error || 'workflow_create failed');
      const created = parseWorkflowSnapshotRow(response.data);
      if (!created) throw new Error('workflow_create response is invalid');
      if (requestId !== createRequestRef.current || !mountedRef.current) return;
      setNameDraft('');
      setObjectiveDraft('');
      await refreshList();
      if (selectedRef.current === selectionAtStart) selectWorkflow(created.workflowId);
    } catch (error) {
      if (requestId !== createRequestRef.current || !mountedRef.current) return;
      report('workflow create', error);
    } finally {
      if (requestId === createRequestRef.current) {
        busyRef.current = false;
        setBusy(false);
      }
    }
    if (requestId === createRequestRef.current && mountedRef.current) {
      const identity = identityRef.current;
      if (identity !== null) void refreshDetail(identity);
    }
  }, [nameDraft, objectiveDraft, props.client, props.sessionId, refreshDetail, refreshList, report, selectWorkflow]);

  const runAction = useCallback(async (action: WorkflowAction, identity: WorkflowIdentity): Promise<void> => {
    if (busyRef.current) return;
    busyRef.current = true;
    setBusy(true);
    // Any in-flight detail response is stale from here on, and no poll may
    // restart detail until the action settles.
    detailRequestRef.current += 1;
    detailInFlightRef.current = null;
    try {
      const command = ACTION_COMMAND[action];
      const response = await props.client.command({ type: command, workflowId: identity.workflowId }, props.sessionId);
      if (!response.success) throw new Error(response.error || `${command} failed`);
      if (action === 'remove') {
        removedRef.current.add(identity.workflowId);
        ircRequestRef.current += 1;
        sendingRef.current = false;
        setSending(false);
        setReceipts(null);
        setRecipient('');
        setMessageDraft('');
        setPendingRemove(false);
        if (selectedRef.current === identity.workflowId) setDetail(null);
      }
      await refreshList();
    } catch (error) {
      // Exactly one bounded report for the CURRENT action: a rejection never
      // flips the rendered status — the list refresh below owns the state.
      const selected = identityRef.current;
      const current = selected !== null
        && selected.sessionId === identity.sessionId
        && selected.workflowId === identity.workflowId
        && selected.generation === identity.generation
        && !removedRef.current.has(identity.workflowId);
      if (current && mountedRef.current) report(`workflow ${action}`, error);
      await refreshList();
    } finally {
      busyRef.current = false;
      setBusy(false);
    }
    if (mountedRef.current && !removedRef.current.has(identity.workflowId)) {
      const selected = identityRef.current;
      if (selected !== null && selected.workflowId === identity.workflowId) void refreshDetail(selected);
    }
  }, [props.client, props.sessionId, refreshDetail, refreshList, report]);

  const sendMessage = useCallback(async (identity: WorkflowIdentity, to: string): Promise<void> => {
    const body = messageDraft.trim();
    if (!body || !to || sendingRef.current) return;
    const requestId = ++ircRequestRef.current;
    sendingRef.current = true;
    setSending(true);
    try {
      const response = await props.client.command(
        {
          type: 'workflow_hub_send',
          workflowId: identity.workflowId,
          generation: identity.generation,
          recipient: to,
          body,
        },
        props.sessionId,
      );
      if (!response.success) throw new Error(response.error || 'workflow_hub_send failed');
      const result = parseWorkflowHubSendResult(response.data);
      if (!result) throw new Error('workflow_hub_send response is invalid');
      // Receipts belong to the recipient that was asked for.
      if (result.recipient !== to) throw new Error('workflow_hub_send response recipient mismatch');
      const selected = identityRef.current;
      if (requestId !== ircRequestRef.current || !mountedRef.current) return;
      if (!selected || selected.workflowId !== identity.workflowId) return;
      if (selected.generation !== identity.generation) return;
      setReceipts(result.receipts);
      setMessageDraft('');
    } catch (error) {
      if (requestId !== ircRequestRef.current || !mountedRef.current) return;
      report('workflow message', error);
    } finally {
      if (requestId === ircRequestRef.current) {
        sendingRef.current = false;
        setSending(false);
      }
    }
  }, [messageDraft, props.client, props.sessionId, report]);

  useEffect(() => {
    listRequestRef.current += 1;
    detailRequestRef.current += 1;
    detailInFlightRef.current = null;
    identityRef.current = null;
    selectedRef.current = null;
    removedRef.current = new Set();
    setWorkflows(null);
    setSelectedId(null);
    setDetail(null);
    setNameDraft('');
    setObjectiveDraft('');
    setPendingRemove(false);
    setExpandedActors(new Set());
    setRecipient('');
    setMessageDraft('');
    setReceipts(null);
  }, [props.sessionId]);

  useEffect(() => {
    void refreshList();
  }, [refreshList]);

  useEffect(() => {
    const off = props.client.subscribeFrames((frame) => {
      if (!frame || typeof frame !== 'object') return;
      const record = frame as Record<string, unknown>;
      if (record.sessionId !== props.sessionId) return;
      if (record.type === 'workflow_removed') {
        const workflowId = record.workflowId;
        if (typeof workflowId !== 'string') return;
        removedRef.current.add(workflowId);
        if (selectedRef.current === workflowId) {
          detailRequestRef.current += 1;
          detailInFlightRef.current = null;
          setDetail(null);
        }
        setWorkflows((current) => {
          if (current === null) return current;
          const next = current.filter((row) => row.workflowId !== workflowId);
          if (selectedRef.current === workflowId) setSelectedId(next[0]?.workflowId ?? null);
          return next;
        });
        return;
      }
      if (record.type === 'workflow_updated') {
        const row = parseWorkflowSnapshotRow(record.snapshot);
        if (!row || removedRef.current.has(row.workflowId)) return;
        setWorkflows((current) => {
          if (current === null) return current;
          const index = current.findIndex((existing) => existing.workflowId === row.workflowId);
          if (index === -1) return [...current, row];
          return current.map((existing, position) => (position === index ? row : existing));
        });
        return;
      }
      if (record.type === 'workflow_status_changed') {
        const workflowId = record.workflowId;
        const status = record.status;
        if (typeof workflowId !== 'string' || typeof status !== 'string') return;
        if (!(status in STATUS_LABEL)) return;
        setWorkflows((current) => (current === null
          ? current
          : current.map((existing) => (existing.workflowId === workflowId
            ? { ...existing, status: status as WorkflowStatusWire }
            : existing))));
      }
    });
    return off;
  }, [props.client, props.sessionId]);

  useEffect(() => {
    const selected = workflows?.find((row) => row.workflowId === selectedId) ?? null;
    const identity: WorkflowIdentity | null = selected === null
      ? null
      : { sessionId: props.sessionId, workflowId: selected.workflowId, generation: selected.generation };
    const prior = identityRef.current;
    identityRef.current = identity;
    if (identity === null) {
      detailRequestRef.current += 1;
      detailInFlightRef.current = null;
      setDetail(null);
      return;
    }
    if (prior !== null && prior.workflowId !== identity.workflowId) {
      detailRequestRef.current += 1;
      detailInFlightRef.current = null;
      setDetail(null);
    }
    if (removedRef.current.has(identity.workflowId) || busyRef.current) return;
    void refreshDetail(identity);
  }, [props.sessionId, refreshDetail, selectedId, workflows]);

  useEffect(() => {
    const selected = workflows?.find((row) => row.workflowId === selectedId);
    if (!selected || isTerminal(selected.status)) return;
    const timer = setInterval(() => {
      const identity = identityRef.current;
      if (!identity || busyRef.current || removedRef.current.has(identity.workflowId)) return;
      if (detailInFlightRef.current !== null) return;
      void refreshDetail(identity);
    }, DETAIL_POLL_MS);
    return () => clearInterval(timer);
  }, [refreshDetail, selectedId, workflows]);

  const selected = workflows?.find((row) => row.workflowId === selectedId) ?? null;
  const selectedIdentity: WorkflowIdentity | null = selected === null
    ? null
    : { sessionId: props.sessionId, workflowId: selected.workflowId, generation: selected.generation };
  const visibleDetail = detail !== null && selectedIdentity !== null
    && detail.id === selectedIdentity.workflowId
    && detail.generation >= selectedIdentity.generation
    ? detail
    : null;
  const actors = useMemo(() => {
    if (visibleDetail === null) return [];
    const all = visibleDetail.supervisor === null
      ? visibleDetail.subagents
      : [visibleDetail.supervisor, ...visibleDetail.subagents];
    const seen = new Set<string>();
    return all.filter((actor) => {
      if (actor.name === '' || seen.has(actor.name)) return false;
      seen.add(actor.name);
      return true;
    });
  }, [visibleDetail]);
  const planningFeed = (visibleDetail?.planningActivity ?? []).slice(-PLANNING_FEED_LIMIT);
  const recentIrc = (visibleDetail?.recentIrc ?? []).slice(-IRC_FEED_LIMIT);
  const failure = visibleDetail?.failure ?? selected?.failure ?? null;
  const allowed = selected === null ? null : lifecycleAllowed(selected.status);
  const createDisabled = busy || nameDraft.trim() === '' || objectiveDraft.trim() === '';
  const worktree = visibleDetail?.worktree.label || selected?.worktree || '';
  const branch = visibleDetail?.worktree.branch || selected?.branch || '';
  const toggleActor = useCallback((key: string) => {
    setExpandedActors((current) => {
      const next = new Set(current);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  }, []);

  const inputStyle = [styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg }];
  const rowStyle = [styles.listRow, { backgroundColor: palette.surface, borderBottomColor: palette.border }];
  const actionStyle = { backgroundColor: palette.accent, flexGrow: 1, marginTop: 0 };
  const muted: { color: string } = { color: palette.muted };

  return (
    <View style={[styles.screen, { backgroundColor: palette.bg }]}>
      <AppBar
        palette={palette}
        title="Workflows"
        onBack={props.onClose}
        actions={[{ icon: 'refresh', label: 'Refresh workflows', onPress: () => void refreshList(), disabled: busy }]}
      />
      <ScrollView contentContainerStyle={styles.form}>
        <Text style={[styles.formSection, muted]}>New workflow</Text>
        <TextInput
          style={inputStyle}
          value={nameDraft}
          onChangeText={setNameDraft}
          placeholder="name"
          placeholderTextColor={palette.muted}
          accessibilityLabel="Workflow name"
          editable={!busy}
        />
        <TextInput
          style={inputStyle}
          value={objectiveDraft}
          onChangeText={setObjectiveDraft}
          placeholder="objective"
          placeholderTextColor={palette.muted}
          accessibilityLabel="Workflow objective"
          editable={!busy}
        />
        <TouchableOpacity
          style={[styles.primaryButton, { backgroundColor: palette.accent }, createDisabled && styles.faded]}
          disabled={createDisabled}
          onPress={() => void createWorkflow()}
          accessibilityLabel="Create workflow">
          <Icon name="add" size={18} color={palette.bg} />
          <Text style={[styles.primaryButtonText, { color: palette.bg }]}>{busy ? 'Working' : 'Create'}</Text>
        </TouchableOpacity>

        <Text style={[styles.formSection, muted]}>
          {workflows === null ? 'Workflows' : `Workflows \u00B7 ${workflows.length}`}
        </Text>
        {workflows === null ? <Text style={[styles.emptyText, muted]}>--</Text> : null}
        {workflows?.length === 0 ? (
          <Text style={[styles.emptyText, muted]}>No workflows yet. Create one above with a name + objective.</Text>
        ) : null}
        {workflows?.map((row) => (
          <TouchableOpacity
            key={row.workflowId}
            style={rowStyle}
            onPress={() => selectWorkflow(row.workflowId)}
            accessibilityLabel={`Select workflow ${safe(row.name, 160)}`}>
            <View style={styles.rowBody}>
              <Text style={[styles.rowTitle, { color: palette.fg }]} numberOfLines={1}>
                {`${STATUS_MARKER[row.status]} ${safe(row.name, 160)}`}
              </Text>
              <Text
                style={[styles.rowSub, { color: row.workflowId === selectedId ? palette.accent : palette.muted }]}
                numberOfLines={1}>
                {`${STATUS_LABEL[row.status]} \u00B7 gen ${row.generation}${row.worktree ? ` \u00B7 ${safe(row.worktree, TEXT_LIMIT)}` : ''}`}
              </Text>
              <Text style={[styles.rowSub, muted]} numberOfLines={2}>{safe(row.objective, ERROR_LIMIT)}</Text>
              {row.failure ? (
                <Text style={[styles.rowSub, { color: palette.error }]} numberOfLines={2}>
                  {`failure: ${safe(row.failure, ERROR_LIMIT)}`}
                </Text>
              ) : null}
            </View>
          </TouchableOpacity>
        ))}

        {selected === null || selectedIdentity === null || allowed === null ? (
          <Text style={[styles.emptyText, muted]}>Select a workflow to inspect it.</Text>
        ) : (
          <>
            <Text style={[styles.formSection, { color: palette.fg }]} numberOfLines={1}>{safe(selected.name, 160)}</Text>
            <Text style={muted} numberOfLines={1}>
              {`${STATUS_LABEL[selected.status]} \u00B7 gen ${selected.generation} \u00B7 ${safe(selected.workflowId, TEXT_LIMIT)}`}
            </Text>
            <Text style={{ color: palette.fg }}>{safe(selected.objective, ERROR_LIMIT)}</Text>
            {branch ? <Text style={muted} numberOfLines={1}>{`branch ${safe(branch, TEXT_LIMIT)}`}</Text> : null}
            {worktree ? <Text style={muted} numberOfLines={1}>{`worktree ${safe(worktree, TEXT_LIMIT)}`}</Text> : null}
            {failure ? (
              <Text style={{ color: palette.error }} numberOfLines={3}>{`failure: ${safe(failure, ERROR_LIMIT)}`}</Text>
            ) : null}

            <View style={{ flexDirection: 'row', flexWrap: 'wrap', gap: 8 }}>
              {ACTIONS.map((action) => {
                const confirm = action === 'remove' && pendingRemove;
                const label = confirm ? `${ACTION_LABEL[action]}?` : ACTION_LABEL[action];
                const name = safe(selected.name, 160);
                return (
                  <TouchableOpacity
                    key={action}
                    style={[
                      styles.primaryButton,
                      action === 'remove' ? { ...actionStyle, backgroundColor: palette.error } : actionStyle,
                      (busy || !allowed[action]) && styles.faded,
                    ]}
                    disabled={busy || !allowed[action]}
                    onPress={() => {
                      if (action === 'remove' && !pendingRemove) {
                        setPendingRemove(true);
                        return;
                      }
                      void runAction(action, selectedIdentity);
                    }}
                    accessibilityLabel={confirm
                      ? `Confirm ${ACTION_LABEL[action].toLowerCase()} ${name}`
                      : `${ACTION_LABEL[action]} ${name}`}>
                    <Icon name={ACTION_ICON[action]} size={18} color={palette.bg} />
                    <Text style={[styles.primaryButtonText, { color: palette.bg }]}>{label}</Text>
                  </TouchableOpacity>
                );
              })}
            </View>

            <Text style={[styles.formSection, muted]}>Supervisor</Text>
            {visibleDetail?.supervisor ? (
              <ActorRow
                actor={visibleDetail.supervisor}
                expanded={expandedActors.has(actorKey(selectedIdentity, visibleDetail.supervisor.name))}
                palette={palette}
                onToggle={() => toggleActor(actorKey(selectedIdentity, visibleDetail.supervisor?.name ?? ''))}
              />
            ) : (
              <Text style={muted}>
                {isTerminal(selected.status) ? 'no supervisor attached' : 'starting supervisor\u2026'}
              </Text>
            )}

            <Text style={[styles.formSection, muted]}>
              {visibleDetail?.planningStartedAtMs
                ? `Planning activity \u00B7 started ${clock(visibleDetail.planningStartedAtMs)}`
                : 'Planning activity'}
            </Text>
            {planningFeed.length === 0 ? (
              <Text style={muted}>
                {isTerminal(selected.status) ? 'no planning activity yet' : 'planning feed will stream here\u2026'}
              </Text>
            ) : (
              planningFeed.map((entry, index) => (
                <Text key={`${entry.atMs}-${index}`} style={[styles.cardMeta, muted]} numberOfLines={2}>
                  {`${clock(entry.atMs)} ${safe(entry.kind, 80)} ${safe(entry.text, TEXT_LIMIT)}`}
                </Text>
              ))
            )}

            <Text style={[styles.formSection, muted]}>Active tasks</Text>
            {visibleDetail === null || visibleDetail.activeTasks.length === 0 ? (
              <Text style={muted}>no active tasks</Text>
            ) : (
              visibleDetail.activeTasks.map((task, index) => (
                <Text key={task.taskId ?? `${task.summary}-${index}`} style={{ color: palette.fg }} numberOfLines={2}>
                  {`\u25CF ${safe(task.summary, 600)}`}
                </Text>
              ))
            )}

            <Text style={[styles.formSection, muted]}>{`Workers \u00B7 ${visibleDetail?.subagents.length ?? 0}`}</Text>
            {visibleDetail === null || visibleDetail.subagents.length === 0 ? (
              <Text style={muted}>no workers yet</Text>
            ) : (
              visibleDetail.subagents.map((actor) => (
                <ActorRow
                  key={actor.name}
                  actor={actor}
                  expanded={expandedActors.has(actorKey(selectedIdentity, actor.name))}
                  palette={palette}
                  onToggle={() => toggleActor(actorKey(selectedIdentity, actor.name))}
                />
              ))
            )}

            <Text style={[styles.formSection, muted]}>
              {visibleDetail === null ? 'Jobs' : `Jobs \u00B7 ${visibleDetail.jobs.length}`}
            </Text>
            {visibleDetail === null || visibleDetail.jobs.length === 0 ? (
              <Text style={muted}>no worker jobs</Text>
            ) : (
              visibleDetail.jobs.map((job) => {
                const duration = job.startedAtMs === null || job.finishedAtMs === null
                  ? null
                  : formatDuration(job.finishedAtMs - job.startedAtMs);
                return (
                  <Text key={job.jobId} style={{ color: palette.fg }} numberOfLines={2}>
                    {`${safe(job.displayName, 160)} \u00B7 ${safe(job.role ?? '', 160)} \u00B7 ${job.status}${duration === null ? '' : ` \u00B7 ${duration}`}${job.softBudgetExhausted ? ' \u00B7 soft budget' : ''}`}
                  </Text>
                );
              })
            )}

            <Text style={[styles.formSection, muted]}>Recent IRC</Text>
            {recentIrc.length === 0 ? (
              <Text style={muted}>No recent messages</Text>
            ) : (
              recentIrc.map((message, index) => (
                <Text
                  key={message.messageId ?? `${message.sender}-${index}`}
                  style={{ color: palette.fg }}
                  numberOfLines={3}>
                  {`${safe(message.sender, 160)}${message.to === null ? '' : ` \u2192 ${safe(message.to, 160)}`} ${safe(message.text, 600)}`}
                </Text>
              ))
            )}

            <Text style={[styles.formSection, muted]}>Send message</Text>
            {actors.length === 0 ? (
              <Text style={muted}>No agents to message yet.</Text>
            ) : (
              <View style={{ flexDirection: 'row', flexWrap: 'wrap', gap: 8 }}>
                {actors.map((actor) => (
                  <TouchableOpacity
                    key={actor.name}
                    style={[
                      styles.primaryButton,
                      {
                        backgroundColor: recipient === actor.name ? palette.accent : palette.surface,
                        borderWidth: 1,
                        borderColor: palette.border,
                        marginTop: 0,
                        paddingHorizontal: 12,
                        flexGrow: 0,
                      },
                    ]}
                    onPress={() => setRecipient(actor.name)}
                    accessibilityLabel={`Message ${safe(actor.name, 160)}`}>
                    <Text style={[styles.primaryButtonText, { color: recipient === actor.name ? palette.bg : palette.fg }]}>
                      {safe(actor.name, 160)}
                    </Text>
                  </TouchableOpacity>
                ))}
              </View>
            )}
            <TextInput
              style={inputStyle}
              value={messageDraft}
              onChangeText={setMessageDraft}
              placeholder="message to send"
              placeholderTextColor={palette.muted}
              accessibilityLabel="Workflow message body"
              editable={!sending}
            />
            <TouchableOpacity
              style={[
                styles.primaryButton,
                { backgroundColor: palette.accent },
                (sending || messageDraft.trim() === '' || recipient === '') && styles.faded,
              ]}
              disabled={sending || messageDraft.trim() === '' || recipient === ''}
              onPress={() => void sendMessage(selectedIdentity, recipient)}
              accessibilityLabel="Send workflow message">
              <Icon name="send" size={18} color={palette.bg} />
              <Text style={[styles.primaryButtonText, { color: palette.bg }]}>{sending ? 'Sending' : 'Send'}</Text>
            </TouchableOpacity>

            {receipts === null ? null : (
              <>
                <Text style={[styles.formSection, muted]}>Receipts</Text>
                {receipts.map((receipt, index) => (
                  <Text
                    key={`${receipt.to}-${index}`}
                    style={{ color: receiptColor(receipt.outcome, palette) }}
                    numberOfLines={3}>
                    {`${safe(receipt.to, 160)} ${safe(receipt.outcome, 80)}${receipt.messageId === null ? '' : ` message ${safe(receipt.messageId, 160)}`}${receipt.error === null ? '' : ` ${safe(receipt.error, ERROR_LIMIT)}`}`}
                  </Text>
                ))}
              </>
            )}

            <Text style={[styles.formSection, muted]}>Integration</Text>
            {visibleDetail !== null && visibleDetail.integration.step !== 'pending' ? (
              <>
                <Text style={{ color: visibleDetail.integration.step === 'conflicted' ? palette.error : palette.fg }}>
                  {`${visibleDetail.integration.step} ${safe(visibleDetail.integration.summary, ERROR_LIMIT)}`}
                </Text>
                {visibleDetail.integration.filesChanged > 0
                  || visibleDetail.integration.insertions > 0
                  || visibleDetail.integration.deletions > 0 ? (
                  <Text style={muted}>
                    {`${visibleDetail.integration.filesChanged} files \u00B7 +${visibleDetail.integration.insertions} \u2212${visibleDetail.integration.deletions}`}
                  </Text>
                ) : null}
                {visibleDetail.integration.conflicts.slice(0, 3).map((conflict) => (
                  <Text key={conflict} style={{ color: palette.error }} numberOfLines={2}>
                    {`! ${safe(conflict, TEXT_LIMIT)}`}
                  </Text>
                ))}
              </>
            ) : (
              <Text style={muted} numberOfLines={2}>
                {selected.integration === null ? 'not started' : safe(selected.integration, ERROR_LIMIT)}
              </Text>
            )}
          </>
        )}
        <View style={styles.bottomPad} />
      </ScrollView>
    </View>
  );
}

