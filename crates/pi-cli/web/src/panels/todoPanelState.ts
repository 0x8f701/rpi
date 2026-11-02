import { safeText } from '../redact';
import { boundText } from '../transcript';

export type TodoStatus = 'pending' | 'in_progress' | 'completed' | 'abandoned';

export interface TodoWaitingOn {
  taskId: string;
  content: string;
  status: TodoStatus;
}

export interface TodoTask {
  id: string;
  content: string;
  status: TodoStatus;
  dependsOn?: string[];
  ready?: boolean;
  waitingOn?: TodoWaitingOn[];
  agent?: string | null;
}

export interface TodoPhase {
  name: string;
  tasks: TodoTask[];
}

export interface TodoNavigationState {
  selectedTaskId: string | null;
}

export interface TodoSessionSnapshot {
  sessionId: string;
  phases: TodoPhase[];
  sequence: number;
  executionStatus: TodoExecutionStatus;
  failureReason: TodoFailureReason | null;
}

export type TodoExecutionStatus = 'dormant' | 'active' | 'settled' | 'failed';

export interface TodoFailureReason {
  kind: 'no_progress' | 'spawn' | 'reconcile';
  message: string;
}

export interface TodoEventFrame {
  type: 'todo_updated' | 'todo_reminder';
  sessionId?: unknown;
  sequence?: unknown;
  phases?: unknown;
  executionStatus?: unknown;
  failureReason?: unknown;
}

const TODO_STATUSES: ReadonlySet<string> = new Set([
  'pending',
  'in_progress',
  'completed',
  'abandoned',
]);
const TODO_PHASE_LIMIT = 128;
const TODO_TASK_LIMIT = 2_000;
const TODO_DEPENDENCY_LIMIT = 512;

function record(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

function text(value: unknown, limit: number): string {
  return boundText(safeText(value), limit);
}

function status(value: unknown): TodoStatus | null {
  return typeof value === 'string' && TODO_STATUSES.has(value)
    ? (value as TodoStatus)
    : null;
}

function waitingOn(value: unknown): TodoWaitingOn | null {
  const source = record(value);
  const reasonStatus = status(source?.status);
  if (!source || typeof source.taskId !== 'string' || source.taskId === '' || !reasonStatus) return null;
  return { taskId: source.taskId, content: text(source.content, 600), status: reasonStatus };
}

function executionStatus(value: unknown): TodoExecutionStatus {
  return value === 'active' || value === 'settled' || value === 'failed' ? value : 'dormant';
}

function failureReason(value: unknown): TodoFailureReason | null {
  const source = record(value);
  if (!source || !['no_progress', 'spawn', 'reconcile'].includes(String(source.kind)) || typeof source.message !== 'string') return null;
  return { kind: source.kind as TodoFailureReason['kind'], message: text(source.message, 1_200) };
}

function task(value: unknown): TodoTask | null {
  const source = record(value);
  const taskStatus = status(source?.status);
  if (!source || typeof source.id !== 'string' || source.id === '' || !taskStatus) return null;
  return {
    id: source.id,
    content: text(source.content, 600),
    status: taskStatus,
    dependsOn: Array.isArray(source.dependsOn)
      ? source.dependsOn
          .slice(0, TODO_DEPENDENCY_LIMIT)
          .filter((dependency): dependency is string => typeof dependency === 'string' && dependency !== '')
      : [],
    ready: source.ready === true,
    waitingOn: Array.isArray(source.waitingOn)
      ? source.waitingOn.slice(0, TODO_DEPENDENCY_LIMIT).flatMap((candidate) => {
          const dependency = waitingOn(candidate);
          return dependency ? [dependency] : [];
        })
      : [],
    agent: typeof source.agent === 'string' && source.agent !== '' ? text(source.agent, 160) : null,
  };
}

export function projectTodoPhases(value: unknown): TodoPhase[] {
  if (!Array.isArray(value)) return [];
  let taskCount = 0;
  const phases: TodoPhase[] = [];
  for (const candidate of value.slice(0, TODO_PHASE_LIMIT)) {
    const source = record(candidate);
    if (!source || typeof source.name !== 'string') continue;
    const tasks: TodoTask[] = [];
    if (Array.isArray(source.tasks)) {
      for (const taskCandidate of source.tasks) {
        if (taskCount >= TODO_TASK_LIMIT) break;
        const projected = task(taskCandidate);
        if (!projected) continue;
        tasks.push(projected);
        taskCount += 1;
      }
    }
    phases.push({ name: text(source.name, 160), tasks });
    if (taskCount >= TODO_TASK_LIMIT) break;
  }
  return phases;
}

export function todoNavigationKey(sessionId: string): string {
  return JSON.stringify(['ordinary-todo', sessionId]);
}

export function normalizeTodoNavigation(
  navigation: TodoNavigationState,
  phases: TodoPhase[],
): TodoNavigationState {
  if (!navigation.selectedTaskId) return navigation;
  const exists = phases.some((phase) => phase.tasks.some((candidate) => candidate.id === navigation.selectedTaskId));
  return exists ? navigation : { selectedTaskId: null };
}

export function applyTodoEvent(
  snapshot: TodoSessionSnapshot,
  frame: TodoEventFrame,
): TodoSessionSnapshot {
  if (frame.sessionId !== snapshot.sessionId) return snapshot;
  const sequence = typeof frame.sequence === 'number' && Number.isSafeInteger(frame.sequence)
    ? frame.sequence
    : 0;
  if (sequence <= snapshot.sequence) return snapshot;
  return {
    sessionId: snapshot.sessionId,
    phases: Array.isArray(frame.phases) ? projectTodoPhases(frame.phases) : snapshot.phases,
    sequence,
    executionStatus: executionStatus(frame.executionStatus),
    failureReason: failureReason(frame.failureReason),
  };
}
