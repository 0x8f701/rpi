// Unified Todo page — ordinary DAG tasks (phases → tasks via `todo_op`) and
// embedded durable child sessions. Wire shapes mirror pi-coding (serde
// camelCase) — see crates/pi-coding/src/todo.rs.
// EVERY model-derived string passes through redactSecrets() before display.

import { useEffect, useMemo, useState } from 'react';
import { TodoChildPanel } from './TodoChildPanel';
import { safeText } from '../redact';
import {
  normalizeTodoNavigation,
  type TodoExecutionStatus,
  type TodoFailureReason,
  type TodoNavigationState,
  type TodoPhase,
  type TodoStatus,
} from './todoPanelState';

export type {
  TodoWaitingOn,
  TodoNavigationState,
  TodoPhase,
  TodoSessionSnapshot,
  TodoStatus,
  TodoTask,
} from './todoPanelState';

/** Flattened `pi_coding::TodoOp` wire shape (serde tag = "op"). */
export interface TodoOpPayload {
  op: string;
  [key: string]: unknown;
}

export interface TodoPanelProps {
  phases: TodoPhase[];
  executionStatus: TodoExecutionStatus;
  failureReason: TodoFailureReason | null;
  navigation?: TodoNavigationState;
  onNavigationChange?: (navigation: TodoNavigationState) => void;
  onOp: (op: TodoOpPayload) => void;
  parentSessionId: string;
  sendCommand: (command: Record<string, unknown>, bubbleId?: string) => Promise<unknown>;
  onClose: () => void;
}

const STATUS_LABEL: Record<TodoStatus, string> = {
  pending: 'pending',
  in_progress: 'in_progress',
  completed: 'completed',
  abandoned: 'abandoned',
};

function statusBullet(status: TodoStatus): string {
  switch (status) {
    case 'in_progress':
      return '\u25CF'; // ●
    case 'completed':
      return '\u2713'; // ✓
    case 'abandoned':
      return '\u2013'; // –
    default:
      return '\u25CB'; // ○
  }
}

function isOpen(status: TodoStatus): boolean {
  return status === 'pending' || status === 'in_progress';
}

interface TaskCounts {
  total: number;
  open: number;
  active: number;
  waiting: number;
  completed: number;
}

function countTasks(phases: TodoPhase[]): TaskCounts {
  const counts: TaskCounts = { total: 0, open: 0, active: 0, waiting: 0, completed: 0 };
  for (const phase of phases) {
    for (const task of phase.tasks) {
      counts.total += 1;
      if (task.status === 'completed') counts.completed += 1;
      if (isOpen(task.status)) {
        counts.open += 1;
        if (task.status === 'in_progress') counts.active += 1;
        if (task.waitingOn && task.waitingOn.length > 0) counts.waiting += 1;
      }
    }
  }
  return counts;
}

/** Resolve a task id to its content across all phases (for dependency labels). */
function taskContentById(phases: TodoPhase[], id: string): string {
  for (const phase of phases) {
    const task = phase.tasks.find((t) => t.id === id);
    if (task) return task.content;
  }
  return id;
}

export function TodoPanel({
  phases,
  executionStatus,
  failureReason,
  navigation,
  onNavigationChange,
  onOp,
  parentSessionId,
  sendCommand,
  onClose,
}: TodoPanelProps) {
  const [addPhase, setAddPhase] = useState('');
  const [addContent, setAddContent] = useState('');
  const [localNavigation, setLocalNavigation] = useState<TodoNavigationState>({ selectedTaskId: null });
  const activeNavigation = navigation ?? localNavigation;
  const selectedId = activeNavigation.selectedTaskId;
  const [editingDeps, setEditingDeps] = useState(false);
  const [depChoice, setDepChoice] = useState('');

  const updateNavigation = (next: TodoNavigationState) => {
    if (navigation === undefined) setLocalNavigation(next);
    onNavigationChange?.(next);
  };

  useEffect(() => {
    if (!activeNavigation.selectedTaskId) return;
    const exists = phases.some((phase) => phase.tasks.some((task) => task.id === activeNavigation.selectedTaskId));
    if (exists) return;
    const next = normalizeTodoNavigation(activeNavigation, phases);
    if (navigation === undefined) setLocalNavigation(next);
    onNavigationChange?.(next);
  }, [activeNavigation, navigation, onNavigationChange, phases]);


  const counts = useMemo(() => countTasks(phases), [phases]);

  // Locate the selected task; clear the selection if it disappeared.
  const selected = useMemo(() => {
    if (!selectedId) return null;
    for (const phase of phases) {
      const task = phase.tasks.find((t) => t.id === selectedId);
      if (task) return { phase: phase.name, task };
    }
    return null;
  }, [phases, selectedId]);

  const otherTasks = useMemo(() => {
    const out: Array<{ id: string; content: string }> = [];
    for (const phase of phases) {
      for (const task of phase.tasks) {
        if (task.id !== selectedId) out.push({ id: task.id, content: task.content });
      }
    }
    return out;
  }, [phases, selectedId]);

  const add = () => {
    const phase = addPhase.trim();
    const content = addContent.trim();
    if (!phase || !content) return;
    onOp({ op: 'append', phase, items: [content] });
    setAddContent('');
  };

  const complete = (id: string) => onOp({ op: 'done', task: id });
  const reopen = (id: string) => onOp({ op: 'start', task: id });
  const linkDependency = () => {
    if (!selectedId || !depChoice) return;
    onOp({ op: 'add_dependency', task: selectedId, dependsOn: [depChoice] });
    setDepChoice('');
  };
  const unlinkDependency = (depId: string) => {
    if (!selectedId) return;
    onOp({ op: 'remove_dependency', task: selectedId, dependsOn: [depId] });
  };

  const selectTask = (id: string) => {
    updateNavigation({ selectedTaskId: id });
    setEditingDeps(false);
    setDepChoice('');
  };

  return (
    <aside id="todo-panel" className="todo-panel" aria-label="Todo panel">
      <div className="todo-panel__head">
        <span className="todo-panel__title">Todo</span>
        <span className="todo-panel__counts" id="todo-counts" title="open · active · waiting · completed">
          <span className="todo-panel__counts-item">{counts.open} open</span> ·{' '}
          <span className="todo-panel__counts-item">{counts.active} active</span> ·{' '}
          <span className="todo-panel__counts-item">{counts.waiting} waiting</span> ·{' '}
          <span className="todo-panel__counts-item">{counts.completed} done</span>
        </span>
        <button id="todo-close-btn" type="button" className="todo-panel__close" title="Close Todo module" aria-label="Close Todo module" onClick={onClose}>
          ×
        </button>
      </div>
      {executionStatus === 'failed' && failureReason && (
        <div className="todo-panel__failure" role="alert">
          <strong>Executor failed</strong>
          <span>{safeText(failureReason.message)}</span>
        </div>
      )}
      <section className="todo-panel__tasks" aria-labelledby="todo-tasks-heading">
        <h3 id="todo-tasks-heading" className="todo-panel__section-heading">Tasks</h3>
      <div className="todo-panel__add">
        <input
          id="todo-add-phase"
          className="todo-add__phase"
          list="todo-phase-list"
          placeholder="phase (new or existing)"
          value={addPhase}
          onChange={(e) => setAddPhase(e.target.value)}
          spellCheck={false}
        />
        <datalist id="todo-phase-list">
          {phases.map((phase) => (
            <option key={phase.name} value={phase.name} />
          ))}
        </datalist>
        <input
          id="todo-add-content"
          className="todo-add__content"
          placeholder="task content"
          value={addContent}
          onChange={(e) => setAddContent(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter') {
              e.preventDefault();
              add();
            }
          }}
          spellCheck={false}
        />
        <button id="todo-add-btn" type="button" onClick={add} disabled={!addPhase.trim() || !addContent.trim()}>
          Add
        </button>
      </div>


      <div className="todo-panel__phases">
        {phases.length === 0 && (
          <div className="todo-panel__empty">
            No tasks yet. Add a phase + task above, or ask the agent to plan
            (todo_updated events refresh this panel live).
          </div>
        )}
        {phases.map((phase) => {
          const phaseOpen = phase.tasks.filter((t) => isOpen(t.status)).length;
          const phaseDone = phase.tasks.filter((t) => t.status === 'completed').length;
          return (
            <section className="todo-phase" data-phase={phase.name} key={phase.name}>
              <h4 className="todo-phase__name" title={safeText(phase.name)}>
                {safeText(phase.name)}
                <span className="todo-phase__counts">
                  {phaseOpen} open · {phaseDone} done
                </span>
              </h4>
              <ul className="todo-phase__tasks">
                {phase.tasks.map((task) => {
                  const waiting = isOpen(task.status) && (task.waitingOn?.length ?? 0) > 0;
                  const isSelected = task.id === selectedId;
                  return (
                    <li
                      key={task.id}
                      className={`todo-task${isSelected ? ' is-selected' : ''}`}
                      data-task-id={task.id}
                      onClick={() => selectTask(task.id)}
                    >
                      <span
                        className={`todo-task__bullet todo-task__bullet--${task.status}`}
                        aria-label={STATUS_LABEL[task.status]}
                        title={STATUS_LABEL[task.status]}
                      >
                        {statusBullet(task.status)}
                      </span>
                      <span className="todo-task__content" title={safeText(task.content)}>
                        {safeText(task.content)}
                      </span>
                      {waiting && (
                        <span className="todo-task__waiting" title="waiting on unfinished dependencies">
                          dependencies
                        </span>
                      )}
                      {task.status === 'completed' || task.status === 'abandoned' ? (
                        <button
                          type="button"
                          className="todo-task__action"
                          data-action="reopen"
                          title="Reopen (start) this task"
                          onClick={(e) => {
                            e.stopPropagation();
                            reopen(task.id);
                          }}
                        >
                          ↺
                        </button>
                      ) : (
                        <button
                          type="button"
                          className="todo-task__action"
                          data-action="complete"
                          title="Complete this task"
                          onClick={(e) => {
                            e.stopPropagation();
                            complete(task.id);
                          }}
                        >
                          ✓
                        </button>
                      )}
                    </li>
                  );
                })}
              </ul>
            </section>
          );
        })}
      </div>

      {selected && (
        <div className="todo-detail" id="todo-detail" data-task-id={selected.task.id}>
          <div className="todo-detail__head">
            <span className="todo-detail__title">Task detail</span>
            <button type="button" className="todo-panel__close" title="Clear selection" onClick={() => updateNavigation({ selectedTaskId: null })}>
              ×
            </button>
          </div>
          <div className="todo-detail__content" title={safeText(selected.task.content)}>
            {safeText(selected.task.content)}
          </div>
          <dl className="todo-detail__meta">
            <dt>id</dt>
            <dd className="todo-detail__id">{safeText(selected.task.id)}</dd>
            <dt>phase</dt>
            <dd>{safeText(selected.phase)}</dd>
            <dt>status</dt>
            <dd className={`todo-detail__status todo-detail__status--${selected.task.status}`}>
              {STATUS_LABEL[selected.task.status]}
            </dd>
            <dt>ready</dt>
            <dd>{selected.task.ready ? 'yes' : 'no'}</dd>
            {selected.task.agent && (
              <>
                <dt>agent</dt>
                <dd>{safeText(selected.task.agent)}</dd>
              </>
            )}
          </dl>

          <div className="todo-detail__section">
            <div className="todo-detail__section-head">
              <span>depends on</span>
              <button
                type="button"
                className="todo-detail__toggle"
                onClick={() => setEditingDeps((v) => !v)}
                aria-expanded={editingDeps}
              >
                {editingDeps ? 'done editing' : 'edit'}
              </button>
            </div>
            {selected.task.dependsOn && selected.task.dependsOn.length > 0 ? (
              <ul className="todo-detail__deps">
                {selected.task.dependsOn.map((depId) => (
                  <li key={depId}>
                    <span className="todo-detail__dep-label" title={safeText(depId)}>
                      {safeText(taskContentById(phases, depId))}
                    </span>
                    {editingDeps && (
                      <button
                        type="button"
                        className="todo-detail__unlink"
                        data-dep-id={depId}
                        title={`Unlink ${safeText(depId)}`}
                        onClick={() => unlinkDependency(depId)}
                      >
                        unlink
                      </button>
                    )}
                  </li>
                ))}
              </ul>
            ) : (
              <div className="todo-detail__muted">no dependencies</div>
            )}
            {editingDeps && otherTasks.length > 0 && (
              <div className="todo-detail__dep-add">
                <select
                  id="todo-dep-select"
                  value={depChoice}
                  onChange={(e) => setDepChoice(e.target.value)}
                  aria-label="add dependency"
                >
                  <option value="">add dependency…</option>
                  {otherTasks.map((t) => (
                    <option key={t.id} value={t.id}>
                      {safeText(t.content)}
                    </option>
                  ))}
                </select>
                <button type="button" onClick={linkDependency} disabled={!depChoice}>
                  Link
                </button>
              </div>
            )}
          </div>

          {selected.task.waitingOn && selected.task.waitingOn.length > 0 && (
            <div className="todo-detail__section">
              <div className="todo-detail__section-head">
                <span>waiting on</span>
              </div>
              <ul className="todo-detail__deps">
                {selected.task.waitingOn.map((dependency) => (
                  <li key={dependency.taskId}>
                    <span className="todo-detail__dep-label" title={safeText(dependency.taskId)}>
                      {safeText(dependency.content)}
                    </span>
                    <span className={`todo-detail__dep-status todo-detail__status--${dependency.status}`}>
                      {STATUS_LABEL[dependency.status]}
                    </span>
                  </li>
                ))}
              </ul>
            </div>
          )}

          <div className="todo-detail__actions">
            {selected.task.status === 'completed' || selected.task.status === 'abandoned' ? (
              <button type="button" id="todo-detail-reopen" onClick={() => reopen(selected.task.id)}>
                Reopen
              </button>
            ) : (
              <button type="button" id="todo-detail-complete" onClick={() => complete(selected.task.id)}>
                Complete
              </button>
            )}
          </div>
        </div>
      )}
      </section>
      <TodoChildPanel parentSessionId={parentSessionId} sendCommand={sendCommand} />
    </aside>
  );
}
