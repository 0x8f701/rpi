// Goal panel — current goal (objective, lifecycle, token budget +
// usage, pins), lifecycle actions, and the goal journal replay view.
//
// Wire shapes mirror pi-coding (serde camelCase / snake_case lifecycles) —
// see crates/pi-coding/src/goal.rs. Mutations run over the goal_* RPC family
// (goal_create / goal_pin / goal_unpin / goal_pause / goal_resume /
// goal_complete / goal_drop); the panel refreshes through the `goal_updated`
// / `goal_usage_charged` event stream, and `goal_get` + `goal_journal` for
// the authoritative snapshot and history.
//
// EVERY model-derived string passes through safeText() before display.

import { FormEvent, useState } from 'react';
import { safeText } from '../redact';

export type GoalLifecycle = 'active' | 'paused' | 'completed' | 'dropped';

export interface GoalUsageWire {
  tokensUsed: number;
  activeTimeSeconds: number;
}

export interface GoalWire {
  id: string;
  originGoalId?: string | null;
  objective: string;
  tokenBudget?: number | null;
  pins: string[];
  lifecycle: GoalLifecycle;
  pauseReason?: string | null;
  createdAt: string;
  updatedAt: string;
  usage: GoalUsageWire;
}

export interface GoalStateWire {
  current?: GoalWire | null;
  revision: number;
}

export interface GoalEventKindWire {
  type: string;
  reason?: string;
  pins?: string[];
  delta?: { tokens?: number; activeTimeSeconds?: number };
  [key: string]: unknown;
}

export interface GoalEventWire {
  revision: number;
  timestamp: string;
  kind: GoalEventKindWire;
  goal: GoalWire;
}

interface GoalPanelProps {
  state: GoalStateWire | null;
  journal: GoalEventWire[];
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>;
  /** Refetch goal_get + goal_journal after a mutation settles. */
  onChanged: () => void;
  onClose: () => void;
}

export const MAX_GOAL_PINS = 8;
export const MAX_GOAL_PIN_CHARS = 200;

const LIFECYCLE_LABEL: Record<GoalLifecycle, string> = {
  active: 'active',
  paused: 'paused',
  completed: 'completed',
  dropped: 'dropped',
};

function formatKind(kind: GoalEventKindWire | undefined): string {
  if (!kind || typeof kind !== 'object' || typeof kind.type !== 'string') {
    return 'updated';
  }
  switch (kind.type) {
    case 'created':
      return 'created';
    case 'fork_cloned':
      return 'fork cloned';
    case 'paused':
      return `paused${kind.reason ? ` (${kind.reason})` : ''}`;
    case 'resumed':
      return 'resumed';
    case 'completed':
      return 'completed';
    case 'dropped':
      return 'dropped';
    case 'usage_updated':
      return `usage +${typeof kind.delta?.tokens === 'number' ? kind.delta.tokens : 0} tokens`;
    case 'pins_updated':
      return `pins updated (${Array.isArray(kind.pins) ? kind.pins.length : 0} pins)`;
    default:
      return kind.type;
  }
}

export function GoalPanel({ state, journal, sendCommand, onChanged, onClose }: GoalPanelProps) {
  const [objective, setObjective] = useState('');
  const [budget, setBudget] = useState('');
  const [pinText, setPinText] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');

  const goal = state?.current ?? null;
  const lifecycle: GoalLifecycle | null = goal ? goal.lifecycle : null;
  const pins = goal?.pins ?? [];
  const usage = goal?.usage;

  const run = async (command: Record<string, unknown>) => {
    setBusy(true);
    setError('');
    try {
      await sendCommand(command);
      onChanged();
    } catch (err) {
      setError((err as Error).message || 'command failed');
    } finally {
      setBusy(false);
    }
  };

  const onCreate = (e: FormEvent) => {
    e.preventDefault();
    const text = objective.trim();
    if (!text || busy) return;
    const parsed = budget.trim() === '' ? undefined : Number(budget.trim());
    const tokenBudget = parsed === undefined || Number.isNaN(parsed) ? undefined : Math.max(1, Math.floor(parsed));
    setObjective('');
    setBudget('');
    run({ type: 'goal_create', objective: text, ...(tokenBudget !== undefined ? { tokenBudget } : {}) }).catch(() => {});
  };

  const onPin = (e: FormEvent) => {
    e.preventDefault();
    const text = pinText.trim();
    if (!text || busy || pins.length >= MAX_GOAL_PINS) return;
    setPinText('');
    run({ type: 'goal_pin', text }).catch(() => {});
  };

  const onUnpin = (index: number) => {
    if (busy) return;
    run({ type: 'goal_unpin', index }).catch(() => {});
  };

  const budgetLine = goal
    ? goal.tokenBudget != null
      ? `${goal.tokenBudget} token budget`
      : 'no token budget'
    : '';
  const usageLine = goal && usage
    ? `${usage.tokensUsed}/${goal.tokenBudget ?? '\u221E'} tokens \u00B7 ${usage.activeTimeSeconds}s active`
    : '';
  const percent =
    goal && usage && goal.tokenBudget
      ? Math.min(100, Math.round((usage.tokensUsed / goal.tokenBudget) * 100))
      : 0;

  return (
    <section
      id="goal-panel"
      className="panel goal-panel"
      data-has-goal={goal ? 'true' : 'false'}
      data-lifecycle={lifecycle ?? 'none'}
    >
      <div className="goal-panel__head">
        <span className="goal-panel__title">Goal</span>
        {goal && (
          <span
            id="goal-status"
            className={`goal-status goal-status--${lifecycle ?? 'none'}`}
            data-lifecycle={lifecycle ?? 'none'}
          >
            {LIFECYCLE_LABEL[lifecycle ?? 'active']}
            {goal.pauseReason ? ` (${goal.pauseReason})` : ''}
          </span>
        )}
        <button
          id="goal-close-btn"
          type="button"
          className="goal-panel__close"
          title="Close the Goal panel"
          onClick={onClose}
        >
          {'\u00D7'}
        </button>
      </div>

      {error !== '' && <div className="goal-panel__error">{safeText(error)}</div>}

      {!goal && (
        <form id="goal-create-form" className="goal-create" onSubmit={(e) => void onCreate(e)}>
          <input
            id="goal-objective-input"
            className="goal-create__objective"
            type="text"
            placeholder="Objective — why future turns may continue"
            value={objective}
            onChange={(e) => setObjective(e.target.value)}
            maxLength={65536}
            autoComplete="off"
            spellCheck={false}
          />
          <input
            id="goal-budget-input"
            className="goal-create__budget"
            type="number"
            min={1}
            step={1}
            placeholder="Token budget (optional)"
            value={budget}
            onChange={(e) => setBudget(e.target.value)}
          />
          <button id="goal-create-btn" type="submit" disabled={busy || objective.trim() === ''}>
            Create goal
          </button>
        </form>
      )}

      {goal && (
        <div className="goal-detail">
          <div id="goal-objective" className="goal-detail__objective">
            {safeText(goal.objective)}
          </div>
          <div className="goal-detail__meta">
            <span id="goal-budget" className="goal-detail__budget">
              {budgetLine}
            </span>
            <span id="goal-usage" className="goal-detail__usage">
              {usageLine}
            </span>
          </div>
          {goal.tokenBudget != null && usage && (
            <div className="goal-meter" aria-hidden="true">
              <div
                className={`goal-meter__fill${percent >= 100 ? ' goal-meter__fill--full' : ''}`}
                style={{ width: `${percent}%` }}
              />
            </div>
          )}
          <div className="goal-actions">
            {lifecycle === 'active' && (
              <button
                id="goal-pause-btn"
                type="button"
                disabled={busy}
                onClick={() => run({ type: 'goal_pause' }).catch(() => {})}
              >
                Pause
              </button>
            )}
            {lifecycle === 'paused' && (
              <button
                id="goal-resume-btn"
                type="button"
                disabled={busy}
                onClick={() => run({ type: 'goal_resume' }).catch(() => {})}
              >
                Resume
              </button>
            )}
            {(lifecycle === 'active' || lifecycle === 'paused') && (
              <>
                <button
                  id="goal-complete-btn"
                  type="button"
                  disabled={busy}
                  onClick={() => run({ type: 'goal_complete' }).catch(() => {})}
                >
                  Complete
                </button>
                <button
                  id="goal-drop-btn"
                  type="button"
                  disabled={busy}
                  onClick={() => run({ type: 'goal_drop' }).catch(() => {})}
                >
                  Drop
                </button>
              </>
            )}
          </div>
        </div>
      )}

      {goal && (
        <div className="goal-pins">
          <div className="goal-section-head">
            <span>
              Pins ({pins.length}/{MAX_GOAL_PINS})
            </span>
            <span className="goal-section-hint">role-model examples shown in the goal turn</span>
          </div>
          <form className="goal-pins__add" onSubmit={(e) => void onPin(e)}>
            <input
              id="goal-pin-input"
              type="text"
              placeholder={`Pin text (max ${MAX_GOAL_PIN_CHARS} chars)`}
              maxLength={MAX_GOAL_PIN_CHARS}
              value={pinText}
              onChange={(e) => setPinText(e.target.value)}
              autoComplete="off"
              spellCheck={false}
            />
            <button
              id="goal-pin-btn"
              type="submit"
              disabled={busy || pinText.trim() === '' || pins.length >= MAX_GOAL_PINS}
            >
              Pin
            </button>
          </form>
          {pins.length === 0 ? (
            <div className="goal-pins__empty">no pins</div>
          ) : (
            <ul id="goal-pins" className="goal-pins__list">
              {pins.map((pin, index) => (
                <li key={`${index}-${pin}`} className="goal-pin">
                  <span className="goal-pin__text">{safeText(pin)}</span>
                  <button
                    id={`goal-unpin-${index}`}
                    type="button"
                    className="goal-pin__unpin"
                    title="Remove this pin"
                    disabled={busy}
                    onClick={() => onUnpin(index)}
                  >
                    {'\u00D7'}
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>
      )}

      <div className="goal-journal">
        <div className="goal-section-head">
          <span>Journal</span>
          <span className="goal-section-hint">goal event replay</span>
        </div>
        {journal.length === 0 ? (
          <div className="goal-journal__empty">no goal events yet</div>
        ) : (
          <ul id="goal-journal" className="goal-journal__list">
            {journal.map((entry, index) => (
              <li key={entry.revision || index} className="goal-journal__entry" data-kind={entry.kind?.type ?? 'unknown'}>
                <span className="goal-journal__time">
                  {new Date(entry.timestamp).toLocaleTimeString()}
                </span>
                <span className="goal-journal__kind">{formatKind(entry.kind)}</span>
                {entry.goal && entry.goal.objective && (
                  <span className="goal-journal__objective">{safeText(entry.goal.objective)}</span>
                )}
              </li>
            ))}
          </ul>
        )}
      </div>
    </section>
  );
}
