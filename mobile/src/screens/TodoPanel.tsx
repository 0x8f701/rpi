import React, { useCallback, useEffect, useRef, useState } from 'react';
import { ScrollView, Text, TextInput, TouchableOpacity, View } from 'react-native';
import {
  parseHubReceiptList,
  parseTodoChildCreateId,
  parseTodoChildDetail,
  parseTodoChildManifest,
  parseTodoChildStatus,
  type HubReceiptWire,
  type RunnerCommand,
  type TodoChildDetailData,
  type TodoChildManifestEntryData,
  type TodoChildStatusData,
  type TodoChildStatusWire,
} from '../protocol';
import { redactSecrets } from '../redact';
import type { RunnerClient } from '../runnerClient';
import { AppBar, styles, type Palette } from '../ui';

type TodoStatus = 'pending' | 'in_progress' | 'completed' | 'abandoned';
type TodoCommand = Extract<RunnerCommand, { type: 'todo_op' }> & (
  | { op: 'append'; phase: string; items: string[] }
  | { op: 'done' | 'start'; task: string }
  | { op: 'add_dependency' | 'remove_dependency'; task: string; dependsOn: string[] }
);
interface TodoTask {
  id: string;
  content: string;
  status: TodoStatus;
  dependsOn: string[];
  ready: boolean;
  waitingOn: { taskId: string; content: string; status: TodoStatus }[];
  agent?: string;
}
interface TodoPhase { name: string; tasks: TodoTask[] }

const LIST_POLL_MS = 3000;
const DETAIL_POLL_MS = 1500;
const TASK_LIMIT = 12;
const AGENT_LIMIT = 8;
const JOB_LIMIT = 8;
const ACTIVITY_LIMIT = 8;
const IRC_LIMIT = 8;
const TRANSCRIPT_LIMIT = 12;
const RECEIPT_LIMIT = 8;

function asObject(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown> : null;
}
function status(value: unknown): value is TodoStatus {
  return value === 'pending' || value === 'in_progress' || value === 'completed' || value === 'abandoned';
}
function parsePhases(value: unknown): TodoPhase[] {
  if (!Array.isArray(value)) throw new Error('Invalid Todo phases');
  const ids = new Set<string>();
  const names = new Set<string>();
  return value.map((rawPhase) => {
    const phase = asObject(rawPhase);
    if (!phase || typeof phase.name !== 'string' || !Array.isArray(phase.tasks) || names.has(phase.name)) {
      throw new Error('Invalid Todo phase');
    }
    names.add(phase.name);
    return { name: phase.name, tasks: phase.tasks.map((rawTask): TodoTask => {
      const task = asObject(rawTask);
      if (!task || typeof task.id !== 'string' || !task.id || ids.has(task.id)
        || typeof task.content !== 'string' || !status(task.status)
        || !Array.isArray(task.dependsOn) || !task.dependsOn.every((id): id is string => typeof id === 'string')
        || typeof task.ready !== 'boolean' || !Array.isArray(task.waitingOn)
        || (task.agent !== undefined && task.agent !== null && typeof task.agent !== 'string')) {
        throw new Error('Invalid Todo task');
      }
      ids.add(task.id);
      const waitingOn = task.waitingOn.map((raw) => {
        const dependency = asObject(raw);
        if (!dependency || typeof dependency.taskId !== 'string' || typeof dependency.content !== 'string' || !status(dependency.status)) {
          throw new Error('Invalid Todo dependency');
        }
        return { taskId: dependency.taskId, content: dependency.content, status: dependency.status };
      });
      return { id: task.id, content: task.content, status: task.status, dependsOn: task.dependsOn,
        ready: task.ready, waitingOn, agent: typeof task.agent === 'string' ? task.agent : undefined };
    }) };
  });
}

function todoChildCapabilities(value: TodoChildStatusWire | null): {
  canPause: boolean;
  canResume: boolean;
  canStop: boolean;
  canCancel: boolean;
  canArchive: boolean;
  canSend: boolean;
  canAddTask: boolean;
  canHubSend: boolean;
} {
  const stopped = value === 'stopped';
  const active = value === 'created' || value === 'idle' || value === 'running';
  return {
    canPause: active,
    canResume: value === 'paused' || value === 'failed',
    canStop: value != null && !stopped,
    canCancel: value != null && !stopped,
    canArchive: stopped,
    canSend: value != null && !stopped,
    canAddTask: value != null && !stopped,
    canHubSend: active,
  };
}

function selectedOrFirstChildId(
  data: unknown,
  parentSessionId: string,
  entries: readonly TodoChildManifestEntryData[],
): string | null {
  const row = parseTodoChildStatus(data);
  if (!row || row.parentSessionId !== parentSessionId) return null;
  if (entries.some((entry) => entry.childSessionId === row.childSessionId)) return row.childSessionId;
  return entries[0]?.childSessionId ?? null;
}

function admitsChildDetail(args: {
  snapshot: TodoChildDetailData;
  parentSessionId: string;
  childSessionId: string;
  requestSequence: number;
  currentRequestSequence: number;
  selectionSequence: number;
  currentSelectionSequence: number;
  currentRevision: number | null;
}): boolean {
  return args.requestSequence === args.currentRequestSequence
    && args.selectionSequence === args.currentSelectionSequence
    && args.snapshot.parentSessionId === args.parentSessionId
    && args.snapshot.childSessionId === args.childSessionId
    && args.snapshot.parent.sessionId === args.parentSessionId
    && args.snapshot.todoId === args.childSessionId
    && (args.currentRevision == null || args.snapshot.revision >= args.currentRevision);
}

function projectChildLifecycle(
  current: TodoChildDetailData,
  lifecycle: TodoChildStatusData,
  parentSessionId: string,
  childSessionId: string,
): TodoChildDetailData | null {
  if (current.parentSessionId !== parentSessionId || current.childSessionId !== childSessionId) return null;
  if (lifecycle.parentSessionId !== parentSessionId || lifecycle.childSessionId !== childSessionId) return null;
  if (lifecycle.revision < current.revision || lifecycle.transcriptEntries < current.transcriptEntries) return null;
  return {
    ...current,
    status: lifecycle.status,
    isRunning: lifecycle.isRunning,
    transcriptEntries: lifecycle.transcriptEntries,
    receipts: lifecycle.receipts,
    revision: lifecycle.revision,
    tasks: lifecycle.tasks,
    lastError: lifecycle.lastError,
    label: lifecycle.label,
    createdAt: lifecycle.createdAt,
  };
}

function childTitle(entry: { label: string | null; childSessionId: string }): string {
  const label = entry.label?.trim();
  return label ? label : entry.childSessionId;
}

export function TodoPanel(props: {
  palette: Palette;
  client: RunnerClient;
  sessionId: string;
  onClose: () => void;
  onError: (message: string) => void;
}): React.JSX.Element {
  const { palette, client, sessionId } = props;
  const [phases, setPhases] = useState<TodoPhase[]>([]);
  const [taskDraft, setTaskDraft] = useState('');
  const [phaseDraft, setPhaseDraft] = useState('');
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const owner = useRef({ client, sessionId, active: true, operation: false, refresh: 0 });
  if (owner.current.client !== client || owner.current.sessionId !== sessionId) {
    owner.current.active = false;
    owner.current = { client, sessionId, active: true, operation: false, refresh: 0 };
  }
  const errorHandler = useRef(props.onError);
  errorHandler.current = props.onError;
  const report = (context: string, error: unknown) => {
    errorHandler.current(redactSecrets(`${context}: ${error instanceof Error ? error.message : String(error)}`));
  };

  const refresh = useCallback(async () => {
    const current = owner.current;
    const request = ++current.refresh;
    try {
      const response = await client.command({ type: 'get_state' }, sessionId);
      if (!current.active || current !== owner.current || request !== current.refresh) return;
      if (!response.success) throw new Error(response.error || 'Request refused');
      const data = asObject(response.data);
      const next = parsePhases(data?.todoPhases);
      setPhases(next);
      setSelectedId((id) => next.some((phase) => phase.tasks.some((task) => task.id === id)) ? id : null);
    } catch (error) {
      if (current.active && current === owner.current && request === current.refresh) report('Refresh Todo', error);
    }
  }, [client, sessionId]);

  useEffect(() => {
    const current = owner.current;
    current.active = true;
    setPhases([]);
    setTaskDraft('');
    setPhaseDraft('');
    setSelectedId(null);
    setBusy(false);
    void refresh();
    const off = client.subscribeFrames((frame) => {
      const record = asObject(frame);
      if (!record) return;
      if (record.type !== 'todo_updated') return;
      if (record.sessionId !== sessionId) return;
      void refresh();
    });
    return () => { current.active = false; off(); };
  }, [client, sessionId, refresh]);

  const run = async (command: TodoCommand, clearDraft?: string) => {
    const current = owner.current;
    if (!current.active || current.operation) return;
    current.operation = true;
    ++current.refresh;
    setBusy(true);
    try {
      const response = await client.command(command, sessionId);
      if (!current.active || current !== owner.current) return;
      if (!response.success) throw new Error(response.error || 'Request refused');
      if (clearDraft !== undefined) setTaskDraft((draft) => draft === clearDraft ? '' : draft);
      await refresh();
    } catch (error) {
      if (current.active && current === owner.current) report(`Todo ${command.op}`, error);
    } finally {
      current.operation = false;
      if (current.active && current === owner.current) setBusy(false);
    }
  };

  const tasks = phases.flatMap((phase) => phase.tasks);
  const selected = tasks.find((task) => task.id === selectedId);
  const selectedPhase = phases.find((phase) => phase.tasks.some((task) => task.id === selectedId));
  return (
    <View style={[styles.screen, { backgroundColor: palette.bg }]}>
      <AppBar palette={palette} title="Todo" onBack={props.onClose} />
      <ScrollView contentContainerStyle={styles.form}>
        <TouchableOpacity disabled={busy} accessibilityLabel="Refresh Todo" onPress={() => void refresh()}>
          <Text style={{ color: palette.accent }}>Refresh</Text>
        </TouchableOpacity>
        {phases.length === 0 && <Text style={[styles.emptyText, { color: palette.muted }]}>No tasks.</Text>}
        {phases.map((phase) => (
          <View key={phase.name}>
            <Text style={[styles.formSection, { color: palette.muted }]}>{redactSecrets(phase.name) || 'Tasks'}</Text>
            <Text style={{ color: palette.muted }}>
              {`${phase.tasks.filter((task) => task.status === 'pending' || task.status === 'in_progress').length} open · ${phase.tasks.filter((task) => task.status === 'in_progress').length} active · ${phase.tasks.filter((task) => (task.status === 'pending' || task.status === 'in_progress') && task.waitingOn.length > 0).length} waiting · ${phase.tasks.filter((task) => task.status === 'completed').length} done · ${phase.tasks.filter((task) => task.status === 'abandoned').length} abandoned`}
            </Text>
            {phase.tasks.map((task) => {
              const closed = task.status === 'completed' || task.status === 'abandoned';
              const label = closed ? 'Reopen' : 'Done';
              return (
                <View key={task.id} style={[styles.listRow, { backgroundColor: palette.surface, borderBottomColor: palette.border }]}>
                  <TouchableOpacity style={styles.rowBody} accessibilityLabel={`Details ${redactSecrets(task.content)}`} onPress={() => setSelectedId(task.id)}>
                    <Text style={[styles.rowTitle, { color: palette.fg }]}>{redactSecrets(task.content)}</Text>
                    <Text style={[styles.rowSub, { color: palette.muted }]}>{task.status}</Text>
                  </TouchableOpacity>
                  <TouchableOpacity disabled={busy} accessibilityLabel={`${label} ${redactSecrets(task.content)}`} onPress={() => void run({ type: 'todo_op', op: closed ? 'start' : 'done', task: task.id })}>
                    <Text style={{ color: palette.accent }}>{label}</Text>
                  </TouchableOpacity>
                </View>
              );
            })}
          </View>
        ))}
        {selected && (
          <View>
            <Text style={[styles.formSection, { color: palette.fg }]}>Task details</Text>
            <Text style={{ color: palette.fg }}>{redactSecrets(selected.content)}</Text>
            <Text style={{ color: palette.muted }}>{`ID: ${redactSecrets(selected.id)} · Phase: ${redactSecrets(selectedPhase?.name || '')} · Status: ${selected.status} · Ready: ${selected.ready ? 'yes' : 'no'}`}</Text>
            {selected.agent && <Text style={{ color: palette.muted }}>{`Agent: ${redactSecrets(selected.agent)}`}</Text>}
            <Text style={{ color: palette.fg }}>Dependencies</Text>
            {selected.dependsOn.length === 0 && <Text style={{ color: palette.muted }}>No dependencies.</Text>}
            {selected.dependsOn.map((id) => (
              <View key={id}>
                <Text style={{ color: palette.fg }}>{redactSecrets(tasks.find((task) => task.id === id)?.content || id)}</Text>
                <TouchableOpacity disabled={busy} accessibilityLabel={`Unlink ${redactSecrets(id)}`} onPress={() => void run({ type: 'todo_op', op: 'remove_dependency', task: selected.id, dependsOn: [id] })}>
                  <Text style={{ color: palette.accent }}>Unlink</Text>
                </TouchableOpacity>
              </View>
            ))}
            {tasks.filter((task) => task.id !== selected.id && !selected.dependsOn.includes(task.id)).map((task) => (
              <TouchableOpacity key={task.id} disabled={busy} accessibilityLabel={`Link ${redactSecrets(task.content)}`} onPress={() => void run({ type: 'todo_op', op: 'add_dependency', task: selected.id, dependsOn: [task.id] })}>
                <Text style={{ color: palette.accent }}>{`Link ${redactSecrets(task.content)}`}</Text>
              </TouchableOpacity>
            ))}
            {selected.waitingOn.map((dependency) => <Text key={dependency.taskId} style={{ color: palette.muted }}>{`Waiting on: ${redactSecrets(dependency.content)} (${dependency.status})`}</Text>)}
            <TouchableOpacity accessibilityLabel="Clear task selection" onPress={() => setSelectedId(null)}><Text style={{ color: palette.accent }}>Close details</Text></TouchableOpacity>
          </View>
        )}
        <TextInput style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg }]} value={phaseDraft} onChangeText={setPhaseDraft} placeholder="Phase" accessibilityLabel="Phase" placeholderTextColor={palette.muted} />
        <TextInput style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg }]} value={taskDraft} onChangeText={setTaskDraft} placeholder="New task" accessibilityLabel="New task" placeholderTextColor={palette.muted} />
        <TouchableOpacity
          accessibilityLabel="Add task"
          style={[styles.primaryButton, { backgroundColor: palette.accent }, (!taskDraft.trim() || !phaseDraft.trim() || busy) && styles.faded]}
          disabled={!taskDraft.trim() || !phaseDraft.trim() || busy}
          onPress={() => void run({ type: 'todo_op', op: 'append', phase: phaseDraft.trim(), items: [taskDraft.trim()] }, taskDraft)}>
          <Text style={[styles.primaryButtonText, { color: palette.bg }]}>Add</Text>
        </TouchableOpacity>
        <ChildSessions key={sessionId} palette={palette} client={client} parentSessionId={sessionId} onError={props.onError} />
      </ScrollView>
    </View>
  );
}

function ChildSessions(props: {
  palette: Palette;
  client: RunnerClient;
  parentSessionId: string;
  onError: (message: string) => void;
}): React.JSX.Element {
  const { palette, client, parentSessionId } = props;
  const [entries, setEntries] = useState<TodoChildManifestEntryData[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [detail, setDetail] = useState<TodoChildDetailData | null>(null);
  const [createLabel, setCreateLabel] = useState('');
  const [sendBody, setSendBody] = useState('');
  const [taskContent, setTaskContent] = useState('');
  const [taskDependsOn, setTaskDependsOn] = useState('');
  const [taskAgent, setTaskAgent] = useState('');
  const [taskRole, setTaskRole] = useState('');
  const [hubTo, setHubTo] = useState('');
  const [hubBody, setHubBody] = useState('');
  const [hubReplyTo, setHubReplyTo] = useState('');
  const [hubReceipts, setHubReceipts] = useState<HubReceiptWire[]>([]);
  const [creating, setCreating] = useState(false);
  const [sending, setSending] = useState(false);
  const [addingTask, setAddingTask] = useState(false);
  const [hubSending, setHubSending] = useState(false);
  const [actionBusy, setActionBusy] = useState(false);
  const owner = useRef({
    active: true,
    list: 0,
    detailSeq: 0,
    selection: 0,
    selectedId: null as string | null,
    snapshot: null as TodoChildDetailData | null,
    createHeld: false,
    sendHeld: false,
    taskHeld: false,
    hubHeld: false,
    actionHeld: false,
  });
  const errorHandler = useRef(props.onError);
  errorHandler.current = props.onError;
  const report = (context: string, error: unknown) => {
    errorHandler.current(redactSecrets(`${context}: ${error instanceof Error ? error.message : String(error)}`));
  };

  const rpc = useCallback(async (command: RunnerCommand) => {
    const response = await client.command(command, parentSessionId);
    if (!response.success) throw new Error(response.error || 'Request refused');
    return response.data;
  }, [client, parentSessionId]);

  const refreshList = useCallback(async (preserveSelection: boolean) => {
    const current = owner.current;
    const seq = ++current.list;
    try {
      const data = await rpc({ type: 'todo_child_list', parentSessionId });
      if (!current.active || seq !== current.list) return;
      const next = parseTodoChildManifest(data);
      if (!next) throw new Error('invalid todo_child_list response');
      setEntries(next);
      if (preserveSelection && current.selectedId) {
        if (!next.some((entry) => entry.childSessionId === current.selectedId)) {
          current.selection += 1;
          current.detailSeq += 1;
          const fallback = next[0]?.childSessionId ?? null;
          current.selectedId = fallback;
          current.snapshot = null;
          setSelectedId(fallback);
          setDetail(null);
        }
        return;
      }
      if (current.selectedId || next.length === 0) return;
      const fallback = next[0]?.childSessionId ?? null;
      if (!fallback) return;
      const selection = current.selection;
      current.selectedId = fallback;
      setSelectedId(fallback);
      try {
        const status = await rpc({ type: 'todo_child_status', parentSessionId });
        if (!current.active || current.selection !== selection || current.selectedId !== fallback) return;
        const selected = selectedOrFirstChildId(status, parentSessionId, next);
        if (!selected) throw new Error('invalid todo_child_status response');
        if (selected === fallback) return;
        current.selection += 1;
        current.detailSeq += 1;
        current.selectedId = selected;
        current.snapshot = null;
        setSelectedId(selected);
        setDetail(null);
      } catch (error) {
        if (current.active && current.selection === selection && current.selectedId === fallback) {
          report('Refresh child status', error);
        }
      }
    } catch (error) {
      if (current.active && seq === current.list) report('Refresh child list', error);
    }
  }, [parentSessionId, rpc]);

  const refreshDetail = useCallback(async (childSessionId: string) => {
    const current = owner.current;
    const seq = ++current.detailSeq;
    const selection = current.selection;
    try {
      const data = await rpc({ type: 'todo_child_detail', parentSessionId, childSessionId });
      if (!current.active || current.selectedId !== childSessionId) return;
      const snapshot = parseTodoChildDetail(data);
      const previous = current.snapshot;
      if (!snapshot || !admitsChildDetail({
        snapshot,
        parentSessionId,
        childSessionId,
        requestSequence: seq,
        currentRequestSequence: current.detailSeq,
        selectionSequence: selection,
        currentSelectionSequence: current.selection,
        currentRevision: previous && previous.parentSessionId === parentSessionId && previous.childSessionId === childSessionId
          ? previous.revision
          : null,
      })) {
        if (seq === current.detailSeq && selection === current.selection) throw new Error('invalid todo_child_detail response');
        return;
      }
      current.snapshot = snapshot;
      setDetail(snapshot);
      setEntries((prev) => {
        let changed = false;
        const next = prev.map((entry) => {
          if (entry.childSessionId !== childSessionId) return entry;
          if (entry.status === snapshot.status
            && entry.parentEntryId === snapshot.parent.entryId
            && entry.parentRevision === snapshot.parent.revision) {
            return entry;
          }
          changed = true;
          return { ...entry, status: snapshot.status, parentEntryId: snapshot.parent.entryId, parentRevision: snapshot.parent.revision };
        });
        return changed ? next : prev;
      });
    } catch (error) {
      if (current.active && seq === current.detailSeq && current.selectedId === childSessionId) {
        report('Refresh child detail', error);
      }
    }
  }, [parentSessionId, rpc]);

  const selectChild = useCallback(async (childSessionId: string, force = false) => {
    const current = owner.current;
    if (!force && childSessionId === current.selectedId) return;
    current.selection += 1;
    current.detailSeq += 1;
    current.selectedId = childSessionId;
    current.snapshot = null;
    setSelectedId(childSessionId);
    setDetail(null);
    setHubReceipts([]);
    setSendBody('');
    setTaskContent('');
    setTaskDependsOn('');
    setTaskAgent('');
    setTaskRole('');
    setHubTo('');
    setHubBody('');
    setHubReplyTo('');
    try {
      await rpc({ type: 'todo_child_select', parentSessionId, childSessionId });
      if (!current.active || current.selectedId !== childSessionId) return;
      await refreshList(true);
    } catch (error) {
      if (current.active && current.selectedId === childSessionId) report('Select child', error);
    }
  }, [parentSessionId, refreshList, rpc]);

  useEffect(() => {
    const current = owner.current;
    current.active = true;
    void refreshList(false);
    const off = client.subscribeFrames((frame) => {
      const record = asObject(frame);
      if (!record) return;
      if (record.type !== 'todo_updated' && record.type !== 'todo_child_routed') return;
      if (record.sessionId !== parentSessionId) return;
      void refreshList(true);
      if (current.selectedId) void refreshDetail(current.selectedId);
    });
    const listTimer = setInterval(() => { void refreshList(true); }, LIST_POLL_MS);
    return () => {
      current.active = false;
      off();
      clearInterval(listTimer);
    };
  }, [client, parentSessionId, refreshDetail, refreshList]);

  useEffect(() => {
    if (!selectedId) {
      owner.current.snapshot = null;
      setDetail(null);
      return;
    }
    void refreshDetail(selectedId);
  }, [refreshDetail, selectedId]);

  useEffect(() => {
    if (!selectedId) return;
    const selected = entries.find((entry) => entry.childSessionId === selectedId);
    if (selected?.status === 'stopped') return;
    const timer = setInterval(() => {
      const id = owner.current.selectedId;
      if (id) void refreshDetail(id);
    }, DETAIL_POLL_MS);
    return () => clearInterval(timer);
  }, [entries, refreshDetail, selectedId]);

  const createChild = async () => {
    const current = owner.current;
    if (!current.active || current.createHeld) return;
    const label = createLabel.trim();
    current.createHeld = true;
    setCreating(true);
    try {
      const extra: { type: 'todo_child_create'; label?: string } = { type: 'todo_child_create' };
      if (label) extra.label = label;
      const childSessionId = parseTodoChildCreateId(await rpc(extra), parentSessionId);
      if (!childSessionId) throw new Error('invalid todo_child_create response');
      setCreateLabel('');
      await selectChild(childSessionId, true);
    } catch (error) {
      if (current.active) report('Create child', error);
    } finally {
      current.createHeld = false;
      if (current.active) setCreating(false);
    }
  };

  const sendMessage = async () => {
    const current = owner.current;
    const childSessionId = current.selectedId;
    const message = sendBody.trim();
    if (!current.active || !childSessionId || !message || current.sendHeld) return;
    current.sendHeld = true;
    setSending(true);
    try {
      await rpc({ type: 'todo_child_send', parentSessionId, childSessionId, message });
      if (!current.active || current.selectedId !== childSessionId) return;
      setSendBody('');
      await refreshDetail(childSessionId);
    } catch (error) {
      if (current.active && current.selectedId === childSessionId) report('Send child', error);
    } finally {
      current.sendHeld = false;
      if (current.active) setSending(false);
    }
  };

  const addChildTask = async () => {
    const current = owner.current;
    const childSessionId = current.selectedId;
    const content = taskContent.trim();
    if (!current.active || !childSessionId || !content || current.taskHeld) return;
    const dependsOn = taskDependsOn.split(/[\s,]+/).map((item) => item.trim()).filter(Boolean);
    current.taskHeld = true;
    setAddingTask(true);
    try {
      const command: Extract<RunnerCommand, { type: 'todo_child_add_task' }> = {
        type: 'todo_child_add_task',
        parentSessionId,
        childSessionId,
        content,
      };
      if (dependsOn.length) command.dependsOn = dependsOn;
      const agent = taskAgent.trim();
      const role = taskRole.trim();
      if (agent) command.agent = agent;
      if (role) command.role = role;
      const held = current.snapshot;
      if (held && held.childSessionId === childSessionId) command.expectedRevision = held.revision;
      await rpc(command);
      if (!current.active || current.selectedId !== childSessionId) return;
      setTaskContent('');
      setTaskDependsOn('');
      setTaskAgent('');
      setTaskRole('');
      await refreshDetail(childSessionId);
    } catch (error) {
      if (current.active && current.selectedId === childSessionId) report('Add child task', error);
    } finally {
      current.taskHeld = false;
      if (current.active) setAddingTask(false);
    }
  };

  const sendHub = async () => {
    const current = owner.current;
    const childSessionId = current.selectedId;
    const to = hubTo.trim();
    const body = hubBody.trim();
    const replyTo = hubReplyTo.trim();
    if (!current.active || !childSessionId || !to || !body || current.hubHeld) return;
    current.hubHeld = true;
    setHubSending(true);
    try {
      const receipts = parseHubReceiptList(await rpc({
        type: 'todo_child_hub_send',
        parentSessionId,
        childSessionId,
        to,
        body,
        ...(replyTo ? { replyTo } : {}),
      }));
      if (!current.active || current.selectedId !== childSessionId) return;
      if (!receipts) throw new Error('invalid todo_child_hub_send response');
      setHubReceipts(receipts);
      const failed = receipts.filter((receipt) => receipt.outcome === 'failed');
      if (failed.length === 0) {
        setHubBody('');
        setHubReplyTo('');
      } else {
        report('Hub child', failed.map((receipt) => receipt.error || `delivery to ${receipt.to} failed`).join('; '));
      }
      await refreshDetail(childSessionId);
    } catch (error) {
      if (current.active && current.selectedId !== childSessionId) return;
      if (current.active) report('Hub child', error);
    } finally {
      current.hubHeld = false;
      if (current.active) setHubSending(false);
    }
  };

  const runAction = async (type: 'todo_child_pause' | 'todo_child_resume' | 'todo_child_stop' | 'todo_child_cancel' | 'todo_child_archive') => {
    const current = owner.current;
    const childSessionId = current.selectedId;
    if (!current.active || !childSessionId || current.actionHeld) return;
    current.actionHeld = true;
    setActionBusy(true);
    try {
      const data = await rpc({ type, parentSessionId, childSessionId });
      if (!current.active) return;
      if (type === 'todo_child_archive') {
        const next = entries.filter((entry) => entry.childSessionId !== childSessionId);
        const fallback = next[0]?.childSessionId ?? null;
        if (current.selectedId === childSessionId) {
          current.selectedId = fallback;
          current.snapshot = null;
          setSelectedId(fallback);
          setDetail(null);
        }
        setEntries(next);
        await refreshList(true);
        return;
      }
      const lifecycle = parseTodoChildStatus(data);
      if (!lifecycle || lifecycle.parentSessionId !== parentSessionId || lifecycle.childSessionId !== childSessionId) {
        throw new Error(`invalid ${type} response`);
      }
      setEntries((prev) => prev.map((entry) => entry.childSessionId === childSessionId ? { ...entry, status: lifecycle.status } : entry));
      if (current.selectedId === childSessionId) {
        current.detailSeq += 1;
        const projected = current.snapshot
          ? projectChildLifecycle(current.snapshot, lifecycle, parentSessionId, childSessionId)
          : null;
        if (projected) {
          current.snapshot = projected;
          setDetail(projected);
        } else {
          await refreshDetail(childSessionId);
        }
      }
      await refreshList(true);
    } catch (error) {
      if (current.active && current.selectedId === childSessionId) report(`Child ${type}`, error);
      if (current.active) await refreshList(true);
    } finally {
      current.actionHeld = false;
      if (current.active) setActionBusy(false);
    }
  };

  const selected = entries.find((entry) => entry.childSessionId === selectedId) || null;
  const selectedStatus = detail?.status ?? selected?.status ?? null;
  const capabilities = todoChildCapabilities(selectedStatus);
  const hubRecipients = detail
    ? [
        ...(detail.supervisor?.displayName ? [detail.supervisor.displayName] : []),
        ...detail.agents.map((agent) => agent.displayName),
      ].filter((name, index, all) => name.trim() && all.indexOf(name) === index)
    : [];

  return (
    <View>
      <Text style={[styles.formSection, { color: palette.fg }]}>Child sessions</Text>
      <Text style={{ color: palette.muted }}>{`${entries.length} ${entries.length === 1 ? 'child' : 'children'}`}</Text>
      <TextInput
        style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg }]}
        value={createLabel}
        onChangeText={setCreateLabel}
        placeholder="label (optional)"
        accessibilityLabel="Child label"
        placeholderTextColor={palette.muted}
      />
      <TouchableOpacity
        accessibilityLabel="Create child"
        style={[styles.primaryButton, { backgroundColor: palette.accent }, creating && styles.faded]}
        disabled={creating}
        onPress={() => void createChild()}>
        <Text style={[styles.primaryButtonText, { color: palette.bg }]}>{creating ? 'Creating…' : 'Create'}</Text>
      </TouchableOpacity>
      {entries.length === 0 && <Text style={[styles.emptyText, { color: palette.muted }]}>No Todo children.</Text>}
      {entries.map((entry) => (
        <TouchableOpacity
          key={entry.childSessionId}
          accessibilityLabel={`Select child ${redactSecrets(childTitle(entry))}`}
          style={[styles.listRow, { backgroundColor: palette.surface, borderBottomColor: palette.border }]}
          onPress={() => void selectChild(entry.childSessionId)}>
          <View style={styles.rowBody}>
            <Text style={[styles.rowTitle, { color: palette.fg }]}>{redactSecrets(childTitle(entry))}</Text>
            <Text style={[styles.rowSub, { color: palette.muted }]}>{`${entry.status} · rev ${entry.parentRevision}`}</Text>
            <Text style={[styles.rowSub, { color: palette.muted }]}>{redactSecrets(entry.childSessionId)}</Text>
          </View>
        </TouchableOpacity>
      ))}
      {!selected && <Text style={{ color: palette.muted }}>Select a Todo child to inspect and control it.</Text>}
      {selected && (
        <View>
          <Text style={[styles.formSection, { color: palette.fg }]}>{redactSecrets(childTitle(selected))}</Text>
          <Text style={{ color: palette.muted }}>{`${selectedStatus ?? selected.status} · ${redactSecrets(selected.childSessionId)} · parent rev ${selected.parentRevision}`}</Text>
          <View style={{ flexDirection: 'row', flexWrap: 'wrap', gap: 12 }}>
            <Action label="Pause child" text="Pause" disabled={actionBusy || !capabilities.canPause} palette={palette} onPress={() => void runAction('todo_child_pause')} />
            <Action label="Resume child" text="Resume" disabled={actionBusy || !capabilities.canResume} palette={palette} onPress={() => void runAction('todo_child_resume')} />
            <Action label="Stop child" text="Stop" disabled={actionBusy || !capabilities.canStop} palette={palette} onPress={() => void runAction('todo_child_stop')} />
            <Action label="Cancel child" text="Cancel" disabled={actionBusy || !capabilities.canCancel} palette={palette} onPress={() => void runAction('todo_child_cancel')} />
            <Action label="Archive child" text="Archive" disabled={actionBusy || !capabilities.canArchive} palette={palette} onPress={() => void runAction('todo_child_archive')} />
          </View>
          <TextInput
            style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg }]}
            value={sendBody}
            onChangeText={setSendBody}
            placeholder="send message to child"
            accessibilityLabel="Child message"
            editable={capabilities.canSend && !sending}
            placeholderTextColor={palette.muted}
          />
          <TouchableOpacity
            accessibilityLabel="Send child"
            disabled={!capabilities.canSend || sending || !sendBody.trim()}
            style={[styles.primaryButton, { backgroundColor: palette.accent }, (!capabilities.canSend || sending || !sendBody.trim()) && styles.faded]}
            onPress={() => void sendMessage()}>
            <Text style={[styles.primaryButtonText, { color: palette.bg }]}>{sending ? 'Sending…' : 'Send'}</Text>
          </TouchableOpacity>
          <TextInput style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg }]} value={taskContent} onChangeText={setTaskContent} placeholder="add task" accessibilityLabel="Child task" editable={capabilities.canAddTask && !addingTask} placeholderTextColor={palette.muted} />
          <TextInput style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg }]} value={taskDependsOn} onChangeText={setTaskDependsOn} placeholder="depends on (ids)" accessibilityLabel="Child depends" editable={capabilities.canAddTask && !addingTask} placeholderTextColor={palette.muted} />
          <TextInput style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg }]} value={taskAgent} onChangeText={setTaskAgent} placeholder="agent (optional)" accessibilityLabel="Child agent" editable={capabilities.canAddTask && !addingTask} placeholderTextColor={palette.muted} />
          <TextInput style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg }]} value={taskRole} onChangeText={setTaskRole} placeholder="role (optional)" accessibilityLabel="Child role" editable={capabilities.canAddTask && !addingTask} placeholderTextColor={palette.muted} />
          <TouchableOpacity
            accessibilityLabel="Add child task"
            disabled={!capabilities.canAddTask || addingTask || !taskContent.trim()}
            style={[styles.primaryButton, { backgroundColor: palette.accent }, (!capabilities.canAddTask || addingTask || !taskContent.trim()) && styles.faded]}
            onPress={() => void addChildTask()}>
            <Text style={[styles.primaryButtonText, { color: palette.bg }]}>{addingTask ? 'Adding…' : 'Add task'}</Text>
          </TouchableOpacity>
          {hubRecipients.map((name) => (
            <TouchableOpacity key={name} accessibilityLabel={`Hub recipient ${redactSecrets(name)}`} disabled={!capabilities.canHubSend || hubSending} onPress={() => setHubTo(name)}>
              <Text style={{ color: palette.accent }}>{redactSecrets(name)}</Text>
            </TouchableOpacity>
          ))}
          <TextInput style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg }]} value={hubTo} onChangeText={setHubTo} placeholder="recipient…" accessibilityLabel="Hub recipient" editable={capabilities.canHubSend && !hubSending} placeholderTextColor={palette.muted} />
          <TextInput style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg }]} value={hubBody} onChangeText={setHubBody} placeholder="hub message" accessibilityLabel="Hub message" editable={capabilities.canHubSend && !hubSending} placeholderTextColor={palette.muted} />
          <TextInput style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg }]} value={hubReplyTo} onChangeText={setHubReplyTo} placeholder="reply to message id (optional)" accessibilityLabel="Hub reply" editable={capabilities.canHubSend && !hubSending} placeholderTextColor={palette.muted} />
          <TouchableOpacity
            accessibilityLabel="Send hub"
            disabled={!capabilities.canHubSend || hubSending || !hubTo.trim() || !hubBody.trim()}
            style={[styles.primaryButton, { backgroundColor: palette.accent }, (!capabilities.canHubSend || hubSending || !hubTo.trim() || !hubBody.trim()) && styles.faded]}
            onPress={() => void sendHub()}>
            <Text style={[styles.primaryButtonText, { color: palette.bg }]}>{hubSending ? 'Sending…' : 'Hub'}</Text>
          </TouchableOpacity>
          {hubReceipts.map((receipt, index) => (
            <Text key={receipt.messageId || `${receipt.to}-${index}`} style={{ color: palette.muted }}>
              {`${redactSecrets(receipt.to)} · ${redactSecrets(receipt.outcome)}${receipt.messageId ? ` · ${redactSecrets(receipt.messageId)}` : ''}${receipt.error ? ` · ${redactSecrets(receipt.error)}` : ''}`}
            </Text>
          ))}
          {detail ? <ChildDetail snapshot={detail} palette={palette} /> : <Text style={{ color: palette.muted }}>loading todo_child_detail…</Text>}
        </View>
      )}
    </View>
  );
}

function Action(props: { label: string; text: string; disabled: boolean; palette: Palette; onPress: () => void }): React.JSX.Element {
  return (
    <TouchableOpacity accessibilityLabel={props.label} disabled={props.disabled} style={props.disabled ? styles.faded : undefined} onPress={props.onPress}>
      <Text style={{ color: props.palette.accent }}>{props.text}</Text>
    </TouchableOpacity>
  );
}

function ChildDetail(props: { snapshot: TodoChildDetailData; palette: Palette }): React.JSX.Element {
  const { snapshot, palette } = props;
  const muted = { color: palette.muted };
  const fg = { color: palette.fg };
  return (
    <View>
      <Text style={fg}>{`Todo ${redactSecrets(snapshot.todoId)} · rev ${snapshot.revision}${snapshot.isRunning ? ' · running' : ''}`}</Text>
      <Text style={muted}>{`Workspace ${redactSecrets(snapshot.workspace.workspace)} · ${redactSecrets(snapshot.workspace.cwd)}`}</Text>
      {snapshot.lastError ? <Text style={{ color: palette.error }}>{redactSecrets(snapshot.lastError)}</Text> : null}
      {snapshot.supervisor ? <Text style={muted}>{`Supervisor ${redactSecrets(snapshot.supervisor.displayName)} · ${snapshot.supervisor.status}`}</Text> : null}
      {snapshot.tasks.slice(0, TASK_LIMIT).map((task) => (
        <Text key={task.id} style={fg}>{`${task.status} ${redactSecrets(task.content)}${task.agent ? ` · ${redactSecrets(task.agent)}` : ''}`}</Text>
      ))}
      {snapshot.receipts.slice(0, RECEIPT_LIMIT).map((receipt) => (
        <Text key={receipt.id} style={muted}>{`Receipt ${receipt.status}${receipt.summary ? ` · ${redactSecrets(receipt.summary)}` : ''}`}</Text>
      ))}
      {snapshot.agents.slice(0, AGENT_LIMIT).map((agent) => (
        <Text key={agent.id} style={muted}>{`Agent ${redactSecrets(agent.displayName)} · ${agent.status}`}</Text>
      ))}
      {snapshot.jobs.slice(0, JOB_LIMIT).map((job) => (
        <Text key={job.jobId} style={muted}>{`Job ${redactSecrets(job.displayName)} · ${job.status}`}</Text>
      ))}
      {snapshot.activity.slice(0, ACTIVITY_LIMIT).map((row, index) => (
        <Text key={`${row.atMs}-${index}`} style={muted}>{redactSecrets(row.text)}</Text>
      ))}
      {snapshot.irc.slice(0, IRC_LIMIT).map((row) => (
        <Text key={row.messageId} style={muted}>{`${redactSecrets(row.from)} → ${redactSecrets(row.to)} · ${redactSecrets(row.text)}`}</Text>
      ))}
      {snapshot.transcript.slice(0, TRANSCRIPT_LIMIT).map((row) => (
        <Text key={row.id} style={fg}>{redactSecrets(row.text)}</Text>
      ))}
    </View>
  );
}
