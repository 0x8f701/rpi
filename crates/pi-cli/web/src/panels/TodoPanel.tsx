// Todo DAG panel — phases → tasks with status bullets, dependency
// edges, readiness, per-task detail, and mutations via the `todo_op` RPC
// (flattened pi_coding::TodoOp: {"type":"todo_op","op":"append",...}).
//
// Wire shapes mirror pi-coding (serde camelCase) — see crates/pi-coding/src/todo.rs.
// EVERY model-derived string passes through redactSecrets() before display.

import { useMemo, useState } from 'react';
import { safeText } from '../redact';

export type TodoStatus = 'pending' | 'in_progress' | 'completed' | 'abandoned';

export interface TodoBlockedReason {
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
  blockedBy?: TodoBlockedReason[];
  agent?: string | null;
}

export interface TodoPhase {
  name: string;
  tasks: TodoTask[];
}

/** Flattened `pi_coding::TodoOp` wire shape (serde tag = "op"). */
export interface TodoOpPayload {
  op: string;
  [key: string]: unknown;
}

interface TodoPanelProps {
  phases: TodoPhase[];
  onOp: (op: TodoOpPayload) => void;
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
  blocked: number;
  completed: number;
}

function countTasks(phases: TodoPhase[]): TaskCounts {
  const counts: TaskCounts = { total: 0, open: 0, active: 0, blocked: 0, completed: 0 };
  for (const phase of phases) {
    for (const task of phase.tasks) {
      counts.total += 1;
      if (task.status === 'completed') counts.completed += 1;
      if (isOpen(task.status)) {
        counts.open += 1;
        if (task.status === 'in_progress') counts.active += 1;
        if (task.blockedBy && task.blockedBy.length > 0) counts.blocked += 1;
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

export function TodoPanel({ phases, onOp, onClose }: TodoPanelProps) {
  const [addPhase, setAddPhase] = useState('');
  const [addContent, setAddContent] = useState('');
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [editingDeps, setEditingDeps] = useState(false);
  const [depChoice, setDepChoice] = useState('');

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
    setSelectedId(id);
    setEditingDeps(false);
    setDepChoice('');
  };

  return (
    <aside id="todo-panel" className="todo-panel" aria-label="Todo DAG panel">
      <div className="todo-panel__head">
        <span className="todo-panel__title">Todos</span>
        <span className="todo-panel__counts" id="todo-counts" title="open · active · blocked · completed">
          <span className="todo-panel__counts-item">{counts.open} open</span> ·{' '}
          <span className="todo-panel__counts-item">{counts.active} active</span> ·{' '}
          <span className="todo-panel__counts-item">{counts.blocked} blocked</span> ·{' '}
          <span className="todo-panel__counts-item">{counts.completed} done</span>
        </span>
        <button id="todo-close-btn" type="button" className="todo-panel__close" title="Close todos panel" onClick={onClose}>
          ×
        </button>
      </div>

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
                  const blocked = isOpen(task.status) && (task.blockedBy?.length ?? 0) > 0;
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
                      {blocked && (
                        <span className="todo-task__blocked" title="blocked by unfinished dependencies">
                          ⛔
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
            <button type="button" className="todo-panel__close" title="Clear selection" onClick={() => setSelectedId(null)}>
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

          {selected.task.blockedBy && selected.task.blockedBy.length > 0 && (
            <div className="todo-detail__section">
              <div className="todo-detail__section-head">
                <span>blocked by</span>
              </div>
              <ul className="todo-detail__deps">
                {selected.task.blockedBy.map((b) => (
                  <li key={b.taskId}>
                    <span className="todo-detail__dep-label" title={safeText(b.taskId)}>
                      {safeText(b.content)}
                    </span>
                    <span className={`todo-detail__dep-status todo-detail__status--${b.status}`}>
                      {STATUS_LABEL[b.status]}
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
    </aside>
  );
}
