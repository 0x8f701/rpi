// Durable child sessions section of the unified Todo page — master-detail
// over frozen todo_child_* RPCs. List: todo_child_list. Detail: ONLY
// todo_child_detail keyed by parentSessionId + childSessionId. Late mismatched
// identities are ignored; ordinary Todo DAG state remains separate.
import { useCallback, useEffect, useRef, useState } from 'react';
import { safeText } from '../redact';
import {
  TodoChildDetail,
  type TodoChildDetailActivity,
  type TodoChildDetailAgent,
  type TodoChildDetailIrcMessage,
  type TodoChildDetailJob,
  type TodoChildDetailReceipt,
  type TodoChildDetailSnapshot,
  type TodoChildDetailTask,
  type TodoChildDetailTranscriptEntry,
  type TodoChildLifecycleStatus,
  type TodoChildParentProvenance,
  type TodoChildReceiptStatus,
  type TodoChildTaskStatus,
  type TodoChildWorkspaceIdentity,
} from '../components/TodoChildDetail';

export type TodoChildStatus = TodoChildLifecycleStatus;

export interface TodoChildManifestEntry {
  childSessionId: string;
  label?: string | null;
  createdAt: number;
  status: TodoChildStatus;
  parentEntryId?: string | null;
  parentRevision: number;
  kind: 'todo';
}

export interface TodoChildHubReceipt {
  to: string;
  outcome: string;
  messageId?: string;
  error?: string;
}

export interface TodoChildPanelProps {
  parentSessionId: string;
  sendCommand: (command: Record<string, unknown>, bubbleId?: string) => Promise<unknown>;
}

export interface TodoChildPanelOperationError {
  message: string;
  source: 'transport' | 'operation';
}

export function isTodoChildTransportOperationError(message: string): boolean {
  return message === 'not connected'
    || message === 'connection replaced'
    || message === 'host switched'
    || message.startsWith('send failed:')
    || message.startsWith('connection closed')
    || message.startsWith('connection failed')
    || message.startsWith('connection unresponsive');
}

export function todoChildPanelOperationError(
  error: unknown,
  fallback: string,
): TodoChildPanelOperationError {
  const candidate = error && typeof error === 'object'
    ? error as { message?: unknown; rpc?: unknown; transport?: unknown }
    : null;
  const message = typeof candidate?.message === 'string' && candidate.message
    ? candidate.message
    : fallback;
  return {
    message,
    source: candidate?.rpc !== true
      && (candidate?.transport === true || isTodoChildTransportOperationError(message))
      ? 'transport'
      : 'operation',
  };
}

export function retainTodoChildOperationErrorAfterRecovery(
  error: TodoChildPanelOperationError | null,
): TodoChildPanelOperationError | null {
  return error?.source === 'transport' ? null : error;
}

const LIST_POLL_MS = 3000;
const DETAIL_POLL_MS = 1500;

const LIFECYCLE_STATUS: Record<TodoChildStatus, true> = {
  created: true,
  idle: true,
  running: true,
  paused: true,
  stopped: true,
  failed: true,
};

const TASK_STATUS: Record<TodoChildTaskStatus, true> = {
  pending: true,
  running: true,
  completed: true,
  failed: true,
  interrupted: true,
};
const RECEIPT_STATUS: Record<TodoChildReceiptStatus, true> = {
  queued: true,
  running: true,
  completed: true,
  failed: true,
};
const AGENT_STATUS = new Set(['queued', 'running', 'idle', 'parked', 'aborted']);
const JOB_STATUS = new Set(['queued', 'running', 'completed', 'failed', 'cancelled']);
const HUB_OUTCOME = new Set(['queued', 'woken', 'revived', 'processed', 'failed']);

const STATUS_LABEL: Record<TodoChildStatus, string> = {
  created: 'created',
  idle: 'idle',
  running: 'running',
  paused: 'paused',
  stopped: 'stopped',
  failed: 'failed',
};

const STATUS_MARKER: Record<TodoChildStatus, string> = {
  created: '\u25CB',
  idle: '\u25CB',
  running: '\u25CF',
  paused: '\u25D0',
  stopped: '\u2715',
  failed: '!',
};

function clock(ms: number): string {
  if (!ms) return '';
  const d = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

function asRecord(value: unknown): Record<string, unknown> | null {
  return value && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

function asString(value: unknown): string | null {
  return typeof value === 'string' && value.trim() ? value : null;
}

function asOptionalString(value: unknown): string | null | undefined {
  if (value == null) return value === null ? null : undefined;
  return typeof value === 'string' ? value : undefined;
}

function asNumber(value: unknown): number | null {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 ? value : null;
}

function parseStatus(value: unknown): TodoChildStatus | null {
  return typeof value === 'string' && LIFECYCLE_STATUS[value as TodoChildStatus]
    ? (value as TodoChildStatus)
    : null;
}

function parseTaskStatus(value: unknown): TodoChildTaskStatus | null {
  return typeof value === 'string' && TASK_STATUS[value as TodoChildTaskStatus]
    ? (value as TodoChildTaskStatus)
    : null;
}
function parseReceiptStatus(value: unknown): TodoChildReceiptStatus | null {
  return typeof value === 'string' && RECEIPT_STATUS[value as TodoChildReceiptStatus]
    ? (value as TodoChildReceiptStatus)
    : null;
}

function parseDependsOn(value: unknown): string[] | null {
  if (!Array.isArray(value)) return null;
  const dependencies: string[] = [];
  for (const item of value) {
    if (typeof item !== 'string' || !item.trim()) return null;
    dependencies.push(item);
  }
  return dependencies;
}

export function parseManifestEntry(value: unknown): TodoChildManifestEntry | null {
  const row = asRecord(value);
  if (!row) return null;
  const childSessionId = asString(row.childSessionId);
  const createdAt = asNumber(row.createdAt);
  const status = parseStatus(row.status);
  const parentRevision = asNumber(row.parentRevision);
  if (
    !childSessionId ||
    createdAt == null ||
    !status ||
    parentRevision == null ||
    !(row.label == null || typeof row.label === 'string') ||
    !(row.parentEntryId == null || typeof row.parentEntryId === 'string')
  ) return null;
  if (row.kind !== 'todo') return null;
  return {
    childSessionId,
    label: asOptionalString(row.label),
    createdAt,
    status,
    parentEntryId: asOptionalString(row.parentEntryId),
    parentRevision,
    kind: 'todo',
  };
}
export function parseTodoChildManifest(value: unknown): TodoChildManifestEntry[] | null {
  if (!Array.isArray(value)) return null;
  const entries: TodoChildManifestEntry[] = [];
  const identities = new Set<string>();
  for (const item of value) {
    const entry = parseManifestEntry(item);
    if (!entry || identities.has(entry.childSessionId)) return null;
    identities.add(entry.childSessionId);
    entries.push(entry);
  }
  return entries;
}
export function parseTodoChildSelectedId(
  value: unknown,
  parentSessionId: string,
  entries: readonly TodoChildManifestEntry[],
): string | null {
  const row = asRecord(value);
  if (!row || asString(row.parentSessionId) !== parentSessionId) return null;
  const childSessionId = asString(row.childSessionId);
  return childSessionId && entries.some((entry) => entry.childSessionId === childSessionId)
    ? childSessionId
    : null;
}

export function selectedOrFirstTodoChildId(
  value: unknown,
  parentSessionId: string,
  entries: readonly TodoChildManifestEntry[],
): string | null {
  const row = asRecord(value);
  if (!row || asString(row.parentSessionId) !== parentSessionId) return null;
  return parseTodoChildSelectedId(row, parentSessionId, entries)
    ?? entries[0]?.childSessionId
    ?? null;
}

function parseParent(value: unknown): TodoChildParentProvenance | null {
  const row = asRecord(value);
  if (!row) return null;
  const sessionId = asString(row.sessionId);
  const revision = asNumber(row.revision);
  if (!sessionId || revision == null || !(row.entryId == null || typeof row.entryId === 'string')) return null;
  return { sessionId, entryId: asOptionalString(row.entryId), revision };
}

function parseWorkspace(value: unknown): TodoChildWorkspaceIdentity | null {
  const row = asRecord(value);
  if (!row) return null;
  const workspace = asString(row.workspace);
  const cwd = asString(row.cwd);
  if (!workspace || !cwd) return null;
  return { workspace, cwd };
}

function parseDetailTask(value: unknown): TodoChildDetailTask | null {
  const row = asRecord(value);
  if (!row) return null;
  const id = asString(row.id);
  const content = typeof row.content === 'string' ? row.content : null;
  const status = parseTaskStatus(row.status);
  const createdAt = asNumber(row.createdAt);
  const dependsOn = parseDependsOn(row.dependsOn);
  if (
    !id ||
    content == null ||
    !status ||
    createdAt == null ||
    !dependsOn ||
    !(row.agent == null || typeof row.agent === 'string') ||
    !(row.role == null || typeof row.role === 'string') ||
    !(row.summary == null || typeof row.summary === 'string')
  ) return null;
  return {
    id,
    content,
    dependsOn,
    status,
    agent: asOptionalString(row.agent),
    role: asOptionalString(row.role),
    createdAt,
    summary: asOptionalString(row.summary),
  };
}
function parseReceipt(value: unknown): TodoChildDetailReceipt | null {
  const row = asRecord(value);
  if (!row) return null;
  const id = asString(row.id);
  const parentSessionId = asString(row.parentSessionId);
  const childSessionId = asString(row.childSessionId);
  const messageId = asString(row.messageId);
  const acceptedAt = asNumber(row.acceptedAt);
  const status = parseReceiptStatus(row.status);
  if (
    !id ||
    !parentSessionId ||
    !childSessionId ||
    !messageId ||
    acceptedAt == null ||
    !status ||
    !(row.summary == null || typeof row.summary === 'string')
  ) return null;
  return {
    id,
    parentSessionId,
    childSessionId,
    messageId,
    acceptedAt,
    status,
    summary: asOptionalString(row.summary),
  };
}

function parseAgent(value: unknown): TodoChildDetailAgent | null {
  const row = asRecord(value);
  if (!row) return null;
  const id = asString(row.id);
  const displayName = typeof row.displayName === 'string' ? row.displayName : null;
  const status = typeof row.status === 'string' && AGENT_STATUS.has(row.status) ? row.status : null;
  if (
    !id ||
    displayName == null ||
    status == null ||
    !(row.role == null || typeof row.role === 'string') ||
    !(row.taskId == null || typeof row.taskId === 'string')
  ) return null;
  return {
    id,
    displayName,
    role: asOptionalString(row.role),
    status,
    taskId: asOptionalString(row.taskId),
  };
}

function parseJob(value: unknown): TodoChildDetailJob | null {
  const row = asRecord(value);
  if (!row) return null;
  const jobId = asString(row.jobId);
  const displayName = typeof row.displayName === 'string' ? row.displayName : null;
  const status = typeof row.status === 'string' && JOB_STATUS.has(row.status) ? row.status : null;
  if (
    !jobId ||
    displayName == null ||
    status == null ||
    !(row.role == null || typeof row.role === 'string') ||
    !(row.taskId == null || typeof row.taskId === 'string') ||
    !(row.summary == null || typeof row.summary === 'string')
  ) return null;
  return {
    jobId,
    displayName,
    role: asOptionalString(row.role),
    status,
    taskId: asOptionalString(row.taskId),
    summary: asOptionalString(row.summary),
  };
}

function parseActivity(value: unknown): TodoChildDetailActivity | null {
  const row = asRecord(value);
  if (!row) return null;
  const atMs = asNumber(row.atMs);
  const kind = row.kind === 'task' || row.kind === 'irc' ? row.kind : null;
  const text = typeof row.text === 'string' ? row.text : null;
  if (atMs == null || kind == null || text == null || !(row.actor == null || typeof row.actor === 'string')) return null;
  return { atMs, actor: asOptionalString(row.actor), kind, text };
}

function parseIrc(value: unknown): TodoChildDetailIrcMessage | null {
  const row = asRecord(value);
  if (!row) return null;
  const messageId = asString(row.messageId);
  const from = typeof row.from === 'string' ? row.from : null;
  const to = typeof row.to === 'string' ? row.to : null;
  const text = typeof row.text === 'string' ? row.text : null;
  const atMs = asNumber(row.atMs);
  if (
    !messageId ||
    from == null ||
    to == null ||
    text == null ||
    atMs == null ||
    !(row.replyTo == null || typeof row.replyTo === 'string')
  ) return null;
  return { messageId, from, to, text, atMs, replyTo: asOptionalString(row.replyTo) };
}

function parseTranscript(value: unknown): TodoChildDetailTranscriptEntry | null {
  const row = asRecord(value);
  if (!row) return null;
  const id = asString(row.id);
  const role = typeof row.role === 'string' ? row.role : null;
  const text = typeof row.text === 'string' ? row.text : null;
  if (
    !id ||
    role == null ||
    text == null ||
    !(row.actor == null || typeof row.actor === 'string') ||
    !(row.atMs == null || asNumber(row.atMs) != null) ||
    typeof row.interrupted !== 'boolean'
  ) return null;
  return {
    id,
    role,
    actor: asOptionalString(row.actor),
    text,
    atMs: asNumber(row.atMs),
    interrupted: row.interrupted,
  };
}

function parseTypedList<T>(value: unknown, parse: (item: unknown) => T | null): T[] | null {
  if (!Array.isArray(value)) return null;
  const items: T[] = [];
  for (const item of value) {
    const parsed = parse(item);
    if (!parsed) return null;
    items.push(parsed);
  }
  return items;
}
function hasUniqueValues<T>(items: readonly T[], identity: (item: T) => string): boolean {
  const seen = new Set<string>();
  for (const item of items) {
    const key = identity(item);
    if (seen.has(key)) return false;
    seen.add(key);
  }
  return true;
}


export function parseTodoChildDetailSnapshot(value: unknown): TodoChildDetailSnapshot | null {
  const row = asRecord(value);
  if (!row) return null;
  const parentSessionId = asString(row.parentSessionId);
  const childSessionId = asString(row.childSessionId);
  const todoId = asString(row.todoId);
  const parent = parseParent(row.parent);
  const workspace = parseWorkspace(row.workspace);
  const status = parseStatus(row.status);
  const isRunning = typeof row.isRunning === 'boolean' ? row.isRunning : null;
  const revision = asNumber(row.revision);
  const createdAt = asNumber(row.createdAt);
  const transcriptEntries = asNumber(row.transcriptEntries);
  const tasks = parseTypedList(row.tasks, parseDetailTask);
  const receipts = parseTypedList(row.receipts, parseReceipt);
  const agents = parseTypedList(row.agents, parseAgent);
  const jobs = parseTypedList(row.jobs, parseJob);
  const activity = parseTypedList(row.activity, parseActivity);
  const irc = parseTypedList(row.irc, parseIrc);
  const transcript = parseTypedList(row.transcript, parseTranscript);
  if (
    !parentSessionId ||
    !childSessionId ||
    !todoId ||
    !parent ||
    !workspace ||
    !status ||
    isRunning == null ||
    revision == null ||
    createdAt == null ||
    transcriptEntries == null ||
    !tasks ||
    !receipts ||
    !agents ||
    !jobs ||
    !activity ||
    !irc ||
    !transcript ||
    !(row.label == null || typeof row.label === 'string') ||
    !(row.lastError == null || typeof row.lastError === 'string')
  ) return null;
  const supervisor = row.supervisor === undefined || row.supervisor === null
    ? null
    : parseAgent(row.supervisor);
  if (row.supervisor !== undefined && row.supervisor !== null && !supervisor) return null;
  if (isRunning !== (status === 'running') || transcriptEntries < transcript.length) return null;
  if (parent.sessionId !== parentSessionId || todoId !== childSessionId) return null;
  if (receipts.some((receipt) =>
    receipt.parentSessionId !== parentSessionId || receipt.childSessionId !== childSessionId
  )) return null;
  if (
    !hasUniqueValues(tasks, (task) => task.id) ||
    !hasUniqueValues(receipts, (receipt) => receipt.id) ||
    !hasUniqueValues(agents, (agent) => agent.id) ||
    !hasUniqueValues(jobs, (job) => job.jobId) ||
    !hasUniqueValues(irc, (message) => message.messageId) ||
    !hasUniqueValues(transcript, (entry) => entry.id) ||
    (supervisor != null && agents.some((agent) => agent.id === supervisor.id))
  ) return null;
  return {
    parentSessionId,
    childSessionId,
    todoId,
    parent,
    workspace,
    status,
    isRunning,
    revision,
    label: asOptionalString(row.label),
    createdAt,
    lastError: asOptionalString(row.lastError),
    transcriptEntries,
    tasks,
    receipts,
    supervisor,
    agents,
    jobs,
    activity,
    irc,
    transcript,
  };
}

export type TodoChildLifecycleSnapshot = Pick<
  TodoChildDetailSnapshot,
  | 'parentSessionId'
  | 'childSessionId'
  | 'status'
  | 'isRunning'
  | 'transcriptEntries'
  | 'receipts'
  | 'revision'
  | 'tasks'
  | 'lastError'
  | 'label'
  | 'createdAt'
>;

export function parseTodoChildLifecycleSnapshot(value: unknown): TodoChildLifecycleSnapshot | null {
  const row = asRecord(value);
  if (!row) return null;
  const parentSessionId = asString(row.parentSessionId);
  const childSessionId = asString(row.childSessionId);
  const status = parseStatus(row.status);
  const isRunning = typeof row.isRunning === 'boolean' ? row.isRunning : null;
  const transcriptEntries = asNumber(row.transcriptEntries);
  const receipts = parseTypedList(row.receipts, parseReceipt);
  const revision = asNumber(row.revision);
  const tasks = parseTypedList(row.tasks, parseDetailTask);
  const createdAt = asNumber(row.createdAt);
  if (
    !parentSessionId
    || !childSessionId
    || !status
    || isRunning == null
    || transcriptEntries == null
    || !receipts
    || revision == null
    || !tasks
    || createdAt == null
    || !(row.lastError == null || typeof row.lastError === 'string')
    || !(row.label == null || typeof row.label === 'string')
  ) return null;
  if (
    isRunning !== (status === 'running')
    || receipts.some((receipt) => receipt.parentSessionId !== parentSessionId || receipt.childSessionId !== childSessionId)
    || !hasUniqueValues(tasks, (task) => task.id)
    || !hasUniqueValues(receipts, (receipt) => receipt.id)
  ) return null;
  return {
    parentSessionId,
    childSessionId,
    status,
    isRunning,
    transcriptEntries,
    receipts,
    revision,
    tasks,
    lastError: asOptionalString(row.lastError),
    label: asOptionalString(row.label),
    createdAt,
  };
}

export function projectTodoChildLifecycleSnapshot(
  current: TodoChildDetailSnapshot,
  lifecycle: TodoChildLifecycleSnapshot,
  parentSessionId: string,
  childSessionId: string,
): TodoChildDetailSnapshot | null {
  if (
    !matchesTodoChildSelection(current, parentSessionId, childSessionId)
    || lifecycle.parentSessionId !== parentSessionId
    || lifecycle.childSessionId !== childSessionId
    || lifecycle.revision < current.revision
    || lifecycle.transcriptEntries < current.transcriptEntries
  ) return null;
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

function parseCreateChildId(value: unknown, parentSessionId: string): string | null {
  const row = asRecord(value);
  const identity = asRecord(row?.identity);
  const snapshot = asRecord(row?.snapshot);
  const identityParent = asString(identity?.parentSessionId);
  const identityChild = asString(identity?.childSessionId);
  const snapshotParent = asString(snapshot?.parentSessionId);
  const snapshotChild = asString(snapshot?.childSessionId);
  if (
    !identityParent ||
    !identityChild ||
    !snapshotParent ||
    !snapshotChild ||
    identityParent !== parentSessionId ||
    snapshotParent !== parentSessionId ||
    snapshotChild !== identityChild
  ) return null;
  return identityChild;
}

function parseHubReceipts(value: unknown): TodoChildHubReceipt[] | null {
  if (!Array.isArray(value)) return null;
  const receipts: TodoChildHubReceipt[] = [];
  for (const item of value) {
    const row = asRecord(item);
    const to = asString(row?.to);
    const outcome = typeof row?.outcome === 'string' && HUB_OUTCOME.has(row.outcome) ? row.outcome : null;
    const messageId = asString(row?.messageId);
    const error = asString(row?.error);
    if (
      !to ||
      outcome == null ||
      !(row?.messageId == null || typeof row.messageId === 'string') ||
      !(row?.error == null || typeof row.error === 'string')
    ) return null;
    receipts.push({
      to,
      outcome,
      ...(messageId ? { messageId } : {}),
      ...(error ? { error } : {}),
    });
  }
  return receipts;
}
export function matchesTodoChildSelection(
  snapshot: TodoChildDetailSnapshot,
  parentSessionId: string,
  childSessionId: string,
): boolean {
  return snapshot.parentSessionId === parentSessionId
    && snapshot.childSessionId === childSessionId
    && snapshot.parent.sessionId === parentSessionId
    && snapshot.todoId === childSessionId;
}

export interface TodoChildCapabilities {
  canPause: boolean;
  canResume: boolean;
  canStop: boolean;
  canCancel: boolean;
  canArchive: boolean;
  canSend: boolean;
  canAddTask: boolean;
  canHubSend: boolean;
}

export function todoChildCapabilities(status: TodoChildStatus | null): TodoChildCapabilities {
  const stopped = status === 'stopped';
  const active = status === 'created' || status === 'idle' || status === 'running';
  return {
    canPause: active,
    canResume: status === 'paused' || status === 'failed',
    canStop: status != null && !stopped,
    canCancel: status != null && !stopped,
    canArchive: stopped,
    canSend: status != null && !stopped,
    canAddTask: status != null && !stopped,
    canHubSend: active,
  };
}

export function buildTodoChildCommand(
  parentSessionId: string,
  type: string,
  extra?: Record<string, unknown>,
): Record<string, unknown> {
  return { ...extra, type, parentSessionId };
}

export function admitsTodoChildDetailResponse({
  snapshot,
  parentSessionId,
  childSessionId,
  requestSequence,
  currentRequestSequence,
  selectionSequence,
  currentSelectionSequence,
  currentRevision,
}: {
  snapshot: TodoChildDetailSnapshot;
  parentSessionId: string;
  childSessionId: string;
  requestSequence: number;
  currentRequestSequence: number;
  selectionSequence: number;
  currentSelectionSequence: number;
  currentRevision: number | null;
}): boolean {
  return requestSequence === currentRequestSequence
    && selectionSequence === currentSelectionSequence
    && matchesTodoChildSelection(snapshot, parentSessionId, childSessionId)
    && (currentRevision == null || snapshot.revision >= currentRevision);
}

export function TodoChildPanel({ parentSessionId, sendCommand }: TodoChildPanelProps) {
  const [entries, setEntries] = useState<TodoChildManifestEntry[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [detail, setDetail] = useState<TodoChildDetailSnapshot | null>(null);
  const detailRef = useRef<TodoChildDetailSnapshot | null>(null);
  detailRef.current = detail;
  const [listError, setListError] = useState<string | null>(null);
  const [detailError, setDetailError] = useState<string | null>(null);
  const [busy, setBusy] = useState<Record<string, true>>({});
  const [createLabel, setCreateLabel] = useState('');
  const [creating, setCreating] = useState(false);
  const creatingRef = useRef(false);
  const [createError, setCreateError] = useState<TodoChildPanelOperationError | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [sendBody, setSendBody] = useState('');
  const [sending, setSending] = useState(false);
  const sendingRef = useRef(false);
  const [sendError, setSendError] = useState<TodoChildPanelOperationError | null>(null);
  const [taskContent, setTaskContent] = useState('');
  const [taskDependsOn, setTaskDependsOn] = useState('');
  const [taskAgent, setTaskAgent] = useState('');
  const [taskRole, setTaskRole] = useState('');
  const [addingTask, setAddingTask] = useState(false);
  const addingTaskRef = useRef(false);
  const [taskError, setTaskError] = useState<TodoChildPanelOperationError | null>(null);
  const [hubTo, setHubTo] = useState('');
  const [hubBody, setHubBody] = useState('');
  const [hubReplyTo, setHubReplyTo] = useState('');
  const [hubSending, setHubSending] = useState(false);
  const hubSendingRef = useRef(false);
  const [hubError, setHubError] = useState<TodoChildPanelOperationError | null>(null);
  const [hubReceipts, setHubReceipts] = useState<TodoChildHubReceipt[]>([]);

  const selectedRef = useRef<string | null>(null);
  selectedRef.current = selectedId;
  const parentRef = useRef(parentSessionId);
  parentRef.current = parentSessionId;
  const mountedRef = useRef(true);
  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);
  const listSeqRef = useRef(0);
  const detailSeqRef = useRef(0);
  const selectionSeqRef = useRef(0);

  const clearRecoveredTransportOperationErrors = useCallback(() => {
    setCreateError(retainTodoChildOperationErrorAfterRecovery);
    setSendError(retainTodoChildOperationErrorAfterRecovery);
    setTaskError(retainTodoChildOperationErrorAfterRecovery);
    setHubError(retainTodoChildOperationErrorAfterRecovery);
  }, []);

  const command = useCallback(
    (type: string, extra?: Record<string, unknown>): Record<string, unknown> =>
      buildTodoChildCommand(parentRef.current, type, extra),
    [],
  );
  // The service already owns the selected child per parent. On mount, show
  // the first manifest row immediately, then replace it only with a current,
  // exact parent+listed-child todo_child_status response. A stale/unlisted
  // successful response deliberately keeps the first row.
  const restoreSelectedChild = useCallback(
    (entries: TodoChildManifestEntry[]) => {
      const fallback = entries[0]?.childSessionId ?? null;
      if (!fallback) return;
      const owner = parentRef.current;
      const statusSelectionSequence = selectionSeqRef.current;
      selectedRef.current = fallback;
      setSelectedId(fallback);
      sendCommand(command('todo_child_status'))
        .then((data) => {
          if (!mountedRef.current || parentRef.current !== owner) return;
          if (selectionSeqRef.current !== statusSelectionSequence || selectedRef.current !== fallback) return;
          const selected = selectedOrFirstTodoChildId(data, owner, entries);
          if (!selected) {
            setListError('invalid todo_child_status response');
            return;
          }
          setListError(null);
          if (selected === fallback) return;
          selectionSeqRef.current += 1;
          detailSeqRef.current += 1;
          selectedRef.current = selected;
          detailRef.current = null;
          setDetail(null);
          setSelectedId(selected);
        })
        .catch((err: Error) => {
          if (!mountedRef.current || parentRef.current !== owner) return;
          if (selectionSeqRef.current !== statusSelectionSequence || selectedRef.current !== fallback) return;
          setListError(err.message || 'todo_child_status failed');
        });
    },
    [command, sendCommand],
  );

  const refreshList = useCallback(
    (preserveSelection = true) => {
      const owner = parentRef.current;
      if (!owner) return;
      const seq = ++listSeqRef.current;
      setRefreshing(true);
      sendCommand(command('todo_child_list'))
        .then((data) => {
          if (seq !== listSeqRef.current || !mountedRef.current || parentRef.current !== owner) return;
          const next = parseTodoChildManifest(data);
          if (!next) {
            setListError('invalid todo_child_list response');
            return;
          }
          clearRecoveredTransportOperationErrors();
          setEntries(next);
          if (preserveSelection && selectedRef.current) {
            const stillThere = next.some((entry) => entry.childSessionId === selectedRef.current);
            if (!stillThere) {
              selectionSeqRef.current += 1;
              detailSeqRef.current += 1;
              const fallback = next[0]?.childSessionId ?? null;
              selectedRef.current = fallback;
              setSelectedId(fallback);
              detailRef.current = null;
              setDetail(null);
            }
          } else if (!selectedRef.current) {
            restoreSelectedChild(next);
          }
          setListError(null);
        })
        .catch((err: Error) => {
          if (seq !== listSeqRef.current || !mountedRef.current || parentRef.current !== owner) return;
          setListError(err.message || 'todo_child_list failed');
        })
        .finally(() => {
          if (seq === listSeqRef.current && mountedRef.current) setRefreshing(false);
        });
    },
    [clearRecoveredTransportOperationErrors, command, restoreSelectedChild, sendCommand],
  );

  const refreshDetail = useCallback(
    (childSessionId: string) => {
      const owner = parentRef.current;
      if (!owner || !childSessionId) return;
      setDetailError(null);
      const seq = ++detailSeqRef.current;
      const selectionSequence = selectionSeqRef.current;
      sendCommand(command('todo_child_detail', { childSessionId }))
        .then((data) => {
          if (!mountedRef.current) return;
          if (parentRef.current !== owner || selectedRef.current !== childSessionId) return;
          const snapshot = parseTodoChildDetailSnapshot(data);
          const previous = detailRef.current;
          if (!snapshot || !admitsTodoChildDetailResponse({
            snapshot,
            parentSessionId: owner,
            childSessionId,
            requestSequence: seq,
            currentRequestSequence: detailSeqRef.current,
            selectionSequence,
            currentSelectionSequence: selectionSeqRef.current,
            currentRevision: previous && matchesTodoChildSelection(previous, owner, childSessionId)
              ? previous.revision
              : null,
          })) {
            if (seq === detailSeqRef.current && selectionSequence === selectionSeqRef.current) {
              setDetailError('invalid todo_child_detail response');
            }
            return;
          }
          clearRecoveredTransportOperationErrors();
          detailRef.current = snapshot;
          setDetail(snapshot);
          setEntries((prev) =>
            prev.map((entry) =>
              entry.childSessionId === childSessionId
                ? {
                    ...entry,
                    status: snapshot.status,
                    parentEntryId: snapshot.parent.entryId ?? entry.parentEntryId,
                    parentRevision: snapshot.parent.revision,
                  }
                : entry,
            ),
          );
          setDetailError(null);
        })
        .catch((err: Error) => {
          if (seq !== detailSeqRef.current || !mountedRef.current) return;
          if (parentRef.current !== owner || selectedRef.current !== childSessionId) return;
          setDetailError(err.message || 'todo_child_detail failed');
        });
    },
    [clearRecoveredTransportOperationErrors, command, sendCommand],
  );

  const selectChild = useCallback(
    (childSessionId: string, force = false) => {
      const owner = parentRef.current;
      if (!owner || !childSessionId) return;
      if (!force && childSessionId === selectedRef.current) return;
      selectionSeqRef.current += 1;
      detailSeqRef.current += 1;
      const selectionSequence = selectionSeqRef.current;
      selectedRef.current = childSessionId;
      setSelectedId(childSessionId);
      detailRef.current = null;
      setDetail(null);
      setDetailError(null);
      setSendError(null);
      setTaskError(null);
      setHubError(null);
      setHubReceipts([]);
      setSendBody('');
      setTaskContent('');
      setTaskDependsOn('');
      setTaskAgent('');
      setTaskRole('');
      setHubTo('');
      setHubBody('');
      setHubReplyTo('');
      sendCommand(command('todo_child_select', { childSessionId }))
        .then(() => {
          if (!mountedRef.current || parentRef.current !== owner || selectedRef.current !== childSessionId) return;
          refreshList(true);
        })
        .catch((err: Error) => {
          if (!mountedRef.current || parentRef.current !== owner || selectedRef.current !== childSessionId) return;
          if (selectionSeqRef.current !== selectionSequence) return;
          setDetailError(err.message || 'todo_child_select failed');
        });
    },
    [command, refreshList, sendCommand],
  );
  useEffect(() => {
    selectionSeqRef.current += 1;
    listSeqRef.current += 1;
    detailSeqRef.current += 1;
    setEntries([]);
    selectedRef.current = null;
    setSelectedId(null);
    detailRef.current = null;
    setDetail(null);
    setListError(null);
    setDetailError(null);
    setHubReceipts([]);
    setBusy({});
    setCreateLabel('');
    setCreateError(null);
    setSendBody('');
    setSendError(null);
    setTaskContent('');
    setTaskDependsOn('');
    setTaskAgent('');
    setTaskRole('');
    setTaskError(null);
    setHubTo('');
    setHubBody('');
    setHubReplyTo('');
    setHubError(null);
    refreshList(false);
  }, [parentSessionId, refreshList]);

  useEffect(() => {
    if (!selectedId) {
      detailRef.current = null;
      setDetail(null);
      return;
    }
    refreshDetail(selectedId);
  }, [selectedId, refreshDetail]);

  useEffect(() => {
    if (!selectedId) return;
    const selected = entries.find((entry) => entry.childSessionId === selectedId);
    if (selected?.status === 'stopped') return;
    const timer = window.setInterval(() => {
      const id = selectedRef.current;
      if (id) refreshDetail(id);
    }, DETAIL_POLL_MS);
    return () => window.clearInterval(timer);
  }, [selectedId, entries, refreshDetail]);

  useEffect(() => {
    const timer = window.setInterval(() => refreshList(true), LIST_POLL_MS);
    return () => window.clearInterval(timer);
  }, [refreshList]);

  const runAction = useCallback(
    (type: string, childSessionId: string) => {
      const owner = parentRef.current;
      if (!owner) return;
      setBusy((prev) => ({ ...prev, [childSessionId]: true }));
      sendCommand(command(type, { childSessionId }))
        .then((data) => {
          if (!mountedRef.current || parentRef.current !== owner) return;
          if (type === 'todo_child_archive') {
            setEntries((prev) => {
              const next = prev.filter((entry) => entry.childSessionId !== childSessionId);
              if (selectedRef.current === childSessionId) {
                const fallback = next[0]?.childSessionId ?? null;
                selectedRef.current = fallback;
                setSelectedId(fallback);
                detailRef.current = null;
                setDetail(null);
              }
              return next;
            });
            refreshList(true);
            return;
          }
          const lifecycle = parseTodoChildLifecycleSnapshot(data);
          if (!lifecycle
            || lifecycle.parentSessionId !== owner
            || lifecycle.childSessionId !== childSessionId
          ) {
            if (selectedRef.current === childSessionId) {
              setDetailError(`invalid ${type} response`);
            }
            refreshList(true);
            return;
          }
          if (selectedRef.current === childSessionId) {
            detailSeqRef.current += 1;
          }
          setEntries((prev) => prev.map((entry) =>
            entry.childSessionId === childSessionId
              ? { ...entry, status: lifecycle.status }
              : entry,
          ));
          if (selectedRef.current === childSessionId) {
            const current = detailRef.current;
            const projected = current
              ? projectTodoChildLifecycleSnapshot(current, lifecycle, owner, childSessionId)
              : null;
            if (projected) {
              detailRef.current = projected;
              setDetail(projected);
              setDetailError(null);
            } else {
              refreshDetail(childSessionId);
            }
          }
          refreshList(true);
        })
        .catch((err: Error) => {
          if (!mountedRef.current || parentRef.current !== owner) return;
          if (selectedRef.current === childSessionId) {
            setDetailError(err.message || `${type} failed`);
          }
          refreshList(true);
        })
        .finally(() => {
          if (!mountedRef.current) return;
          setBusy((prev) => {
            const next = { ...prev };
            delete next[childSessionId];
            return next;
          });
        });
    },
    [command, refreshDetail, refreshList, sendCommand],
  );

  const createChild = useCallback(() => {
    const owner = parentRef.current;
    if (!owner || creatingRef.current) return;
    const label = createLabel.trim();
    creatingRef.current = true;
    setCreating(true);
    setCreateError(null);
    const extra: Record<string, unknown> = {};
    if (label) extra.label = label;
    sendCommand(command('todo_child_create', extra))
      .then((data) => {
        if (!mountedRef.current || parentRef.current !== owner) return;
        const childSessionId = parseCreateChildId(data, owner);
        if (!childSessionId) {
          setCreateError(todoChildPanelOperationError(null, 'invalid todo_child_create response'));
          return;
        }
        setCreateLabel((current) => current.trim() === label ? '' : current);
        selectChild(childSessionId, true);
      })
      .catch((err: Error) => {
        if (!mountedRef.current || parentRef.current !== owner) return;
        setCreateError(todoChildPanelOperationError(err, 'todo_child_create failed'));
      })
      .finally(() => {
        creatingRef.current = false;
        if (mountedRef.current) setCreating(false);
      });
  }, [command, createLabel, selectChild, sendCommand]);

  const sendMessage = useCallback(() => {
    const owner = parentRef.current;
    const childSessionId = selectedRef.current;
    const message = sendBody.trim();
    if (!owner || !childSessionId || !message || sendingRef.current) return;
    sendingRef.current = true;
    setSending(true);
    setSendError(null);
    sendCommand(command('todo_child_send', { childSessionId, message }))
      .then(() => {
        if (!mountedRef.current || parentRef.current !== owner || selectedRef.current !== childSessionId) return;
        setSendBody((current) => current.trim() === message ? '' : current);
        refreshDetail(childSessionId);
      })
      .catch((err: Error) => {
        if (!mountedRef.current || parentRef.current !== owner || selectedRef.current !== childSessionId) return;
        setSendError(todoChildPanelOperationError(err, 'todo_child_send failed'));
      })
      .finally(() => {
        sendingRef.current = false;
        if (mountedRef.current) setSending(false);
      });
  }, [command, refreshDetail, sendBody, sendCommand]);

  const addTask = useCallback(() => {
    const owner = parentRef.current;
    const childSessionId = selectedRef.current;
    const content = taskContent.trim();
    if (!owner || !childSessionId || !content || addingTaskRef.current) return;
    const rawDependsOn = taskDependsOn;
    const rawAgent = taskAgent;
    const rawRole = taskRole;
    const dependsOn = rawDependsOn
      .split(/[\s,]+/)
      .map((item) => item.trim())
      .filter(Boolean);
    addingTaskRef.current = true;
    setAddingTask(true);
    setTaskError(null);
    const extra: Record<string, unknown> = { childSessionId, content };
    if (dependsOn.length) extra.dependsOn = dependsOn;
    const agent = rawAgent.trim();
    const role = rawRole.trim();
    if (agent) extra.agent = agent;
    if (role) extra.role = role;
    if (detail && selectedRef.current === childSessionId) extra.expectedRevision = detail.revision;
    sendCommand(command('todo_child_add_task', extra))
      .then(() => {
        if (!mountedRef.current || parentRef.current !== owner || selectedRef.current !== childSessionId) return;
        setTaskContent((current) => current.trim() === content ? '' : current);
        setTaskDependsOn((current) => current === rawDependsOn ? '' : current);
        setTaskAgent((current) => current === rawAgent ? '' : current);
        setTaskRole((current) => current === rawRole ? '' : current);
        refreshDetail(childSessionId);
      })
      .catch((err: Error) => {
        if (!mountedRef.current || parentRef.current !== owner || selectedRef.current !== childSessionId) return;
        setTaskError(todoChildPanelOperationError(err, 'todo_child_add_task failed'));
      })
      .finally(() => {
        addingTaskRef.current = false;
        if (mountedRef.current) setAddingTask(false);
      });
  }, [command, detail, refreshDetail, sendCommand, taskAgent, taskContent, taskDependsOn, taskRole]);

  const sendHub = useCallback(() => {
    const owner = parentRef.current;
    const childSessionId = selectedRef.current;
    const to = hubTo.trim();
    const body = hubBody.trim();
    const replyTo = hubReplyTo.trim();
    if (!owner || !childSessionId || !to || !body || hubSendingRef.current) return;
    hubSendingRef.current = true;
    setHubSending(true);
    setHubError(null);
    sendCommand(command('todo_child_hub_send', { childSessionId, to, body, ...(replyTo ? { replyTo } : {}) }))
      .then((data) => {
        if (!mountedRef.current || parentRef.current !== owner || selectedRef.current !== childSessionId) return;
        const receipts = parseHubReceipts(data);
        if (!receipts) {
          setHubError(todoChildPanelOperationError(null, 'invalid todo_child_hub_send response'));
          return;
        }
        const failed = receipts.filter((receipt) => receipt.outcome === 'failed');
        setHubReceipts(receipts);
        if (failed.length === 0) {
          setHubBody((current) => current.trim() === body ? '' : current);
          setHubReplyTo((current) => current.trim() === replyTo ? '' : current);
          setHubError(null);
        } else {
          setHubError(todoChildPanelOperationError(
            null,
            failed.map((receipt) => receipt.error || `delivery to ${receipt.to} failed`).join('; '),
          ));
        }
        refreshDetail(childSessionId);
      })
      .catch((err: Error) => {
        if (!mountedRef.current || parentRef.current !== owner || selectedRef.current !== childSessionId) return;
        setHubError(todoChildPanelOperationError(err, 'todo_child_hub_send failed'));
      })
      .finally(() => {
        hubSendingRef.current = false;
        if (mountedRef.current) setHubSending(false);
      });
  }, [command, hubBody, hubReplyTo, hubTo, refreshDetail, sendCommand]);

  const selected = entries.find((entry) => entry.childSessionId === selectedId) || null;
  const selectedBusy = selected ? Boolean(busy[selected.childSessionId]) : false;
  const selectedStatus = detail?.status ?? selected?.status ?? null;
  const capabilities = todoChildCapabilities(selectedStatus);
  const hubRecipients = detail
    ? [
        ...(detail.supervisor?.displayName ? [detail.supervisor.displayName] : []),
        ...detail.agents.map((agent) => agent.displayName),
      ].filter((name, index, all) => name.trim() && all.indexOf(name) === index)
    : [];

  return (
    <section className="todo-child-section" aria-labelledby="todo-child-heading">
      <div className="todo-child-section__head">
        <h3 id="todo-child-heading" className="todo-child-section__title">Child sessions</h3>
        <span className="todo-child-section__counts" id="todo-child-counts" title="todo children listed">
          {entries.length} child{entries.length === 1 ? '' : 'ren'}
        </span>
      </div>
      <div className="todo-child-section__body">
        <section className="todo-child-list" id="todo-child-list" tabIndex={-1} aria-label="Todo child list">
          <div className="todo-child-create">
            <input
              id="todo-child-create-label"
              className="todo-child-create__label"
              placeholder="label (optional)"
              value={createLabel}
              onChange={(e) => setCreateLabel(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') {
                  e.preventDefault();
                  createChild();
                }
              }}
              spellCheck={false}
            />
            <button id="todo-child-create-btn" type="button" disabled={creating} onClick={createChild}>
              {creating ? 'Creating…' : 'Create'}
            </button>
          </div>
          {createError && <div className="todo-child-section__error">{safeText(createError.message)}</div>}
          {listError && <div className="todo-child-section__error">{safeText(listError)}</div>}

          <div className="todo-child-list__rows">
            {entries.length === 0 && !refreshing && (
              <div className="todo-child-list__empty">
                No Todo children yet. Create one above; it is an autonomous session, not the ordinary Todo DAG.
              </div>
            )}
            {entries.map((entry) => {
              const active = entry.childSessionId === selectedId;
              const title = entry.label?.trim() || entry.childSessionId;
              return (
                <div
                  key={entry.childSessionId}
                  id={`todo-child-row-${entry.childSessionId}`}
                  className={`todo-child-row${active ? ' is-selected' : ''}`}
                  data-child-session-id={entry.childSessionId}
                  data-status={entry.status}
                  onClick={() => selectChild(entry.childSessionId)}
                >
                  <div className="todo-child-row__line">
                    <span
                      className={`todo-child-row__marker todo-child-row__marker--${entry.status}`}
                      aria-label={STATUS_LABEL[entry.status]}
                      title={STATUS_LABEL[entry.status]}
                    >
                      {STATUS_MARKER[entry.status]}
                    </span>
                    <span className="todo-child-row__name" title={safeText(title)}>
                      {safeText(title)}
                    </span>
                    <span className="todo-child-row__revision" title="parent revision">
                      rev {entry.parentRevision}
                    </span>
                  </div>
                  <div className="todo-child-row__meta">
                    <span className={`todo-child-row__status todo-child-row__status--${entry.status}`}>
                      {STATUS_LABEL[entry.status]}
                    </span>
                    {entry.createdAt ? (
                      <span className="todo-child-row__created" title="created">
                        {clock(entry.createdAt)}
                      </span>
                    ) : null}
                    {entry.parentEntryId ? (
                      <span className="todo-child-row__parent" title="parent entry">
                        {safeText(entry.parentEntryId)}
                      </span>
                    ) : null}
                  </div>
                  <div className="todo-child-row__id" title={safeText(entry.childSessionId)}>
                    {safeText(entry.childSessionId)}
                  </div>
                </div>
              );
            })}
          </div>
        </section>

        <section className="todo-child-detail-pane" id="todo-child-detail" aria-label="Todo child detail">
          {!selected ? (
            <div className="todo-child-detail-pane__empty">Select a Todo child to inspect and control it.</div>
          ) : (
            <>
              <div className="todo-child-detail-pane__head">
                <span
                  className="todo-child-detail-pane__name"
                  title={safeText(selected.label?.trim() || selected.childSessionId)}
                >
                  {safeText(selected.label?.trim() || selected.childSessionId)}
                </span>
                {selectedStatus ? (
                  <span className={`todo-child-detail-pane__status todo-child-row__status--${selectedStatus}`}>
                    {STATUS_LABEL[selectedStatus]}
                  </span>
                ) : null}
              </div>
              <div className="todo-child-detail-pane__meta">
                <span title="child session id">{safeText(selected.childSessionId)}</span>
                <span title="parent session id">{safeText(parentSessionId)}</span>
                <span title="parent revision">parent rev {selected.parentRevision}</span>
                {selected.parentEntryId ? (
                  <span title="parent entry">{safeText(selected.parentEntryId)}</span>
                ) : null}
              </div>

              <div className="todo-child-actions">
                <button
                  id="todo-child-pause-btn"
                  type="button"
                  disabled={selectedBusy || !capabilities.canPause}
                  title="Pause this Todo child (todo_child_pause)"
                  onClick={() => runAction('todo_child_pause', selected.childSessionId)}
                >
                  Pause
                </button>
                <button
                  id="todo-child-resume-btn"
                  type="button"
                  disabled={selectedBusy || !capabilities.canResume}
                  title="Resume this Todo child (todo_child_resume)"
                  onClick={() => runAction('todo_child_resume', selected.childSessionId)}
                >
                  Resume
                </button>
                <button
                  id="todo-child-stop-btn"
                  type="button"
                  disabled={selectedBusy || !capabilities.canStop}
                  title="Stop this Todo child (todo_child_stop)"
                  onClick={() => runAction('todo_child_stop', selected.childSessionId)}
                >
                  Stop
                </button>
                <button
                  id="todo-child-cancel-btn"
                  type="button"
                  disabled={selectedBusy || !capabilities.canCancel}
                  title="Cancel in-flight work (todo_child_cancel)"
                  onClick={() => runAction('todo_child_cancel', selected.childSessionId)}
                >
                  Cancel
                </button>
                <button
                  id="todo-child-archive-btn"
                  type="button"
                  disabled={selectedBusy || !capabilities.canArchive}
                  title="Archive this Todo child (todo_child_archive)"
                  onClick={() => runAction('todo_child_archive', selected.childSessionId)}
                >
                  Archive
                </button>
              </div>

              {detailError && <div className="todo-child-section__error">{safeText(detailError)}</div>}

              <div className="todo-child-send">
                <input
                  id="todo-child-send-body"
                  className="todo-child-send__body"
                  placeholder="send message to child"
                  value={sendBody}
                  disabled={!capabilities.canSend || sending}
                  onChange={(e) => setSendBody(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') {
                      e.preventDefault();
                      sendMessage();
                    }
                  }}
                  spellCheck={false}
                />
                <button
                  id="todo-child-send-btn"
                  type="button"
                  disabled={!capabilities.canSend || sending || !sendBody.trim()}
                  title="Send a parent message into the child (todo_child_send)"
                  onClick={sendMessage}
                >
                  {sending ? 'Sending…' : 'Send'}
                </button>
              </div>
              {sendError && <div className="todo-child-section__error">{safeText(sendError.message)}</div>}

              <div className="todo-child-add-task">
                <input
                  id="todo-child-task-content"
                  className="todo-child-add-task__content"
                  placeholder="add task"
                  value={taskContent}
                  disabled={!capabilities.canAddTask || addingTask}
                  onChange={(e) => setTaskContent(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') {
                      e.preventDefault();
                      addTask();
                    }
                  }}
                  spellCheck={false}
                />
                <input
                  id="todo-child-task-depends"
                  className="todo-child-add-task__depends"
                  placeholder="depends on (ids)"
                  value={taskDependsOn}
                  disabled={!capabilities.canAddTask || addingTask}
                  onChange={(e) => setTaskDependsOn(e.target.value)}
                  spellCheck={false}
                />
                <input
                  id="todo-child-task-agent"
                  className="todo-child-add-task__agent"
                  placeholder="agent (optional)"
                  value={taskAgent}
                  disabled={!capabilities.canAddTask || addingTask}
                  onChange={(e) => setTaskAgent(e.target.value)}
                  spellCheck={false}
                />
                <input
                  id="todo-child-task-role"
                  className="todo-child-add-task__role"
                  placeholder="role (optional)"
                  value={taskRole}
                  disabled={!capabilities.canAddTask || addingTask}
                  onChange={(e) => setTaskRole(e.target.value)}
                  spellCheck={false}
                />
                <button
                  id="todo-child-add-task-btn"
                  type="button"
                  disabled={!capabilities.canAddTask || addingTask || !taskContent.trim()}
                  title="Append a typed task at the current revision (todo_child_add_task)"
                  onClick={addTask}
                >
                  {addingTask ? 'Adding…' : 'Add task'}
                </button>
              </div>
              {taskError && <div className="todo-child-section__error">{safeText(taskError.message)}</div>}

              <div className="todo-child-hub">
                <select
                  id="todo-child-hub-to"
                  className="todo-child-hub__to"
                  value={hubTo}
                  disabled={!capabilities.canHubSend || hubSending}
                  onChange={(e) => setHubTo(e.target.value)}
                >
                  <option value="">recipient…</option>
                  {hubRecipients.map((name) => (
                    <option key={name} value={name}>
                      {safeText(name)}
                    </option>
                  ))}
                </select>
                <input
                  id="todo-child-hub-body"
                  className="todo-child-hub__body"
                  placeholder="hub message"
                  value={hubBody}
                  disabled={!capabilities.canHubSend || hubSending}
                  onChange={(e) => setHubBody(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') {
                      e.preventDefault();
                      sendHub();
                    }
                  }}
                  spellCheck={false}
                />
                <input
                  id="todo-child-hub-reply-to"
                  className="todo-child-hub__reply-to"
                  placeholder="reply to message id (optional)"
                  value={hubReplyTo}
                  disabled={!capabilities.canHubSend || hubSending}
                  onChange={(e) => setHubReplyTo(e.target.value)}
                  spellCheck={false}
                />
                <button
                  id="todo-child-hub-send-btn"
                  type="button"
                  disabled={!capabilities.canHubSend || hubSending || !hubTo.trim() || !hubBody.trim()}
                  title="Send an IRC hub message inside the child (todo_child_hub_send)"
                  onClick={sendHub}
                >
                  {hubSending ? 'Sending…' : 'Hub'}
                </button>
              </div>
              {hubError && <div className="todo-child-section__error">{safeText(hubError.message)}</div>}
              {hubReceipts.length > 0 && (
                <div className="todo-child-hub__receipts" aria-label="Hub receipts">
                  {hubReceipts.map((receipt, index) => (
                    <div
                      key={receipt.messageId || `${receipt.to}-${index}`}
                      className="todo-child-hub__receipt"
                      data-outcome={receipt.outcome}
                    >
                      {safeText(receipt.to)} · {safeText(receipt.outcome)}
                      {receipt.messageId ? ` · ${safeText(receipt.messageId)}` : ''}
                      {receipt.error ? ` · ${safeText(receipt.error)}` : ''}
                    </div>
                  ))}
                </div>
              )}

              {detail ? (
                <TodoChildDetail snapshot={detail} />
              ) : (
                <div className="todo-child-detail-pane__empty">loading todo_child_detail…</div>
              )}
            </>
          )}
        </section>
      </div>
    </section>
  );
}
