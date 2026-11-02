// Chat goal panel — remote-runner goal management for the /chat console.
//
// This panel is the single owner of its goal state: it issues a bare goal_get
// itself and validates the payload with parseChatGoalState, then owns the
// selected-session journal via goal_journal. Both refresh on mount, on
// goal_updated / goal_usage_charged (the web App treats both as live goal
// events), and on an authoritative subscription cut (runners.ts stamps
// `authoritative` on subscription_snapshot/session_resync BEFORE the cursor
// advances) so an unsolicited resync can never leave a silently stale
// goal/journal — no polling loop. Cuts landing before the mount refresh
// settles are gated off: the initial goal_get + goal_journal already cover
// them, avoiding duplicate initial ownership requests. Mutations go through
// sendCommand, then the panel-local goal_get/journal refresh. runners.ts
// stamps the sessionId on the frame; the wire shapes mirror
// web/src/goal.ts + slashDispatch.ts:647-658, mutation-only (no `activate`
// field), matching panels/GoalPanel.tsx.
//
// Reuses the wire types from panels/GoalPanel.tsx and mirrors its create-form
// parsing / lifecycle gating / journal rendering, adapted to the chat card
// (chat-panel wrapper, chat-field/chat-button controls) with bounded
// rendering: journal rows capped at MAX_JOURNAL_ROWS, long objectives
// truncated.
//
// Pure helpers are exported for the node regression test
// (scripts/chatGoalPanel.test.ts).

import { FormEvent, useCallback, useEffect, useRef, useState } from 'react';
import { safeText } from '../redact';
import type { RunnerCommand } from './protocol';
import type { GoalEventWire, GoalLifecycle, GoalStateWire, GoalWire } from '../panels/GoalPanel';
// Shared strict goal_get parser lives in team.ts (one validation home for
// the panel snapshot and the team aggregation).
import { parseChatGoalState } from './team';

export const MAX_JOURNAL_ROWS = 50;
export const MAX_OBJECTIVE_CHARS = 200;
export const MAX_JOURNAL_OBJECTIVE_CHARS = 120;

export type GoalAction = 'pause' | 'resume' | 'complete' | 'drop';

/** goal_* wire shapes for the remote runner. sessionId is stamped by
 *  runners.ts sendCommand, so get/journal are bare (unlike /web's goal.ts
 *  variants, which carry an explicit sessionId for cross-session routing). */
export type GoalCommand =
  | { type: 'goal_create'; objective: string; tokenBudget?: number }
  | { type: 'goal_pause' }
  | { type: 'goal_resume' }
  | { type: 'goal_complete' }
  | { type: 'goal_drop' }
  | { type: 'goal_get' }
  | { type: 'goal_journal' };

const LIFECYCLE_LABEL: Record<GoalLifecycle, string> = {
  active: 'active',
  paused: 'paused',
  completed: 'completed',
  dropped: 'dropped',
};

/** Status → lifecycle-action gating matrix. A completed/dropped (or absent)
 *  goal exposes no lifecycle actions; active exposes pause/complete/drop;
 *  paused exposes resume/complete/drop. */
export function goalActionsFor(lifecycle: GoalLifecycle | null): GoalAction[] {
  switch (lifecycle) {
    case 'active':
      return ['pause', 'complete', 'drop'];
    case 'paused':
      return ['resume', 'complete', 'drop'];
    default:
      return [];
  }
}

/** goal_create wire shape — objective + optional tokenBudget, mutation-only
 *  (no `activate`), matching panels/GoalPanel.tsx and slashDispatch goalWire. */
export function goalCreateCommand(objective: string, tokenBudget?: number): GoalCommand {
  return tokenBudget === undefined
    ? { type: 'goal_create', objective }
    : { type: 'goal_create', objective, tokenBudget };
}

/** Lifecycle mutation wire shapes (goal_pause / goal_resume / goal_complete /
 *  goal_drop) — bare commands, matching slashDispatch.ts:647-658. */
export function goalCommandFor(action: GoalAction): GoalCommand {
  switch (action) {
    case 'pause':
      return { type: 'goal_pause' };
    case 'resume':
      return { type: 'goal_resume' };
    case 'complete':
      return { type: 'goal_complete' };
    case 'drop':
      return { type: 'goal_drop' };
  }
}

export function chatGoalGetCommand(): GoalCommand {
  return { type: 'goal_get' };
}

export function chatGoalJournalCommand(): GoalCommand {
  return { type: 'goal_journal' };
}

/** Selected-view refresh trigger. goal_updated / goal_usage_charged replays
 *  always re-fetch the panel-owned goal_get + goal_journal — the web App
 *  treats both as live goal events (App.tsx goal_usage_charged case). An
 *  authoritative subscription cut (runners.ts-stamped `authoritative`;
 *  every session_resync is one) refreshes too — an unsolicited resync must
 *  never silently stale the goal/journal — but only after the mount refresh
 *  settled, so the panel's initial ownership requests are never duplicated
 *  by the very cut that unblocked them. Non-authoritative reconnect
 *  snapshots only replay events; they don't. */
export function goalFrameRefreshesJournal(frame: unknown, bootstrapped = true): boolean {
  if (!frame || typeof frame !== 'object' || !('type' in frame)) return false;
  if (frame.type === 'goal_updated' || frame.type === 'goal_usage_charged') return true;
  const type = frame.type;
  if (type === 'session_resync') return bootstrapped;
  if (type === 'subscription_snapshot') {
    return bootstrapped && 'authoritative' in frame && frame.authoritative === true;
  }
  return false;
}

/** Successful mutation: existing sendCommand, then the panel-local goal_get
 *  refresh, then the selected-session journal refresh. Never job_list. */
export async function completeGoalMutation(
  sendCommand: (cmd: RunnerCommand) => Promise<unknown>,
  command: GoalCommand,
  refreshGoal: () => void | Promise<void>,
  refreshJournal: () => void | Promise<void>,
): Promise<void> {
  await sendCommand(command);
  await refreshGoal();
  await refreshJournal();
}

export async function fetchChatGoalState(
  sendCommand: (cmd: RunnerCommand) => Promise<unknown>,
): Promise<GoalStateWire> {
  const data = await sendCommand(chatGoalGetCommand());
  const snapshot = parseChatGoalState(data);
  if (snapshot === null) throw new Error('invalid goal_get response');
  return snapshot;
}

export async function fetchChatGoalJournal(
  sendCommand: (cmd: RunnerCommand) => Promise<unknown>,
): Promise<GoalEventWire[]> {
  const data = await sendCommand(chatGoalJournalCommand());
  if (!Array.isArray(data)) throw new Error('invalid goal_journal response');
  return data as GoalEventWire[];
}

export type ChatGoalJournalFetch =
  | { kind: 'current'; entries: GoalEventWire[] }
  | { kind: 'superseded' };

export async function fetchCurrentChatGoalJournal(
  sendCommand: (cmd: RunnerCommand) => Promise<unknown>,
  seq: number,
  currentSeq: () => number,
): Promise<ChatGoalJournalFetch> {
  try {
    const entries = await fetchChatGoalJournal(sendCommand);
    if (seq !== currentSeq()) return { kind: 'superseded' };
    return { kind: 'current', entries };
  } catch (error) {
    if (seq !== currentSeq()) return { kind: 'superseded' };
    throw error;
  }
}

/** Bounded journal rendering: keep only the most recent `max` entries (the
 *  backend replays oldest-first, so the tail is the latest activity). */
export function boundJournal(entries: GoalEventWire[], max: number): GoalEventWire[] {
  return entries.length > max ? entries.slice(entries.length - max) : entries;
}

/** Long-text truncation for bounded rendering. */
export function truncateText(text: string, max: number): string {
  if (text.length <= max) return text;
  return `${text.slice(0, Math.max(0, max - 1))}\u2026`;
}

function formatGoalKind(kind: GoalEventWire['kind'] | undefined): string {
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

export interface ChatGoalPanelProps {
  sendCommand: (cmd: RunnerCommand) => Promise<unknown>;
  /** Session-scoped event subscription (Lane B filters frames to the owning
   *  session before handing the subscription here). */
  events: (cb: (frame: unknown) => void) => () => void;
}

export function ChatGoalPanel({ sendCommand, events }: ChatGoalPanelProps) {
  const [state, setState] = useState<GoalStateWire | null>(null);
  const [journal, setJournal] = useState<GoalEventWire[]>([]);
  const [objective, setObjective] = useState('');
  const [budget, setBudget] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const requestSeq = useRef(0);
  // Tracks whether the mount refresh settled: cuts arriving earlier are
  // already covered by those initial goal_get + goal_journal requests.
  const bootstrappedRef = useRef(false);

  const goal: GoalWire | null = state?.current ?? null;
  const lifecycle: GoalLifecycle | null = goal ? goal.lifecycle : null;
  const actions = goalActionsFor(lifecycle);
  const usage = goal?.usage;

  const refreshGoal = useCallback(async () => {
    const snapshot = await fetchChatGoalState(sendCommand);
    setState(snapshot);
  }, [sendCommand]);

  const refreshJournal = useCallback(async () => {
    const seq = ++requestSeq.current;
    const result = await fetchCurrentChatGoalJournal(sendCommand, seq, () => requestSeq.current);
    if (result.kind === 'superseded') return;
    setJournal(result.entries);
  }, [sendCommand]);

  useEffect(() => {
    let disposed = false;
    const refresh = () => {
      if (disposed) return;
      void Promise.all([refreshGoal(), refreshJournal()])
        .then(() => {
          if (disposed) return;
          bootstrappedRef.current = true;
          setError('');
        })
        .catch((err: unknown) => {
          if (disposed) return;
          setError(err instanceof Error ? err.message : 'command failed');
        });
    };
    refresh();
    const unsubscribe = events((frame) => {
      if (!goalFrameRefreshesJournal(frame, bootstrappedRef.current)) return;
      refresh();
    });
    return () => {
      disposed = true;
      unsubscribe();
    };
  }, [events, refreshGoal, refreshJournal]);

  const busyRef = useRef(false);
  const run = async (command: GoalCommand): Promise<boolean> => {
    if (busyRef.current) return false;
    busyRef.current = true;
    setBusy(true);
    setError('');
    try {
      await completeGoalMutation(sendCommand, command, refreshGoal, refreshJournal);
      return true;
    } catch (err) {
      setError(err instanceof Error ? err.message : 'command failed');
      return false;
    } finally {
      busyRef.current = false;
      setBusy(false);
    }
  };

  const onCreate = (e: FormEvent) => {
    e.preventDefault();
    const text = objective.trim();
    if (!text || busyRef.current) return;
    const rawBudget = budget;
    const parsed = rawBudget.trim() === '' ? undefined : Number(rawBudget.trim());
    const tokenBudget = parsed === undefined || Number.isNaN(parsed) ? undefined : Math.max(1, Math.floor(parsed));
    void run(goalCreateCommand(text, tokenBudget)).then((done) => {
      if (!done) return;
      setObjective((current) => current.trim() === text ? '' : current);
      setBudget((current) => current === rawBudget ? '' : current);
    });
  };

  const onAction = (action: GoalAction) => {
    if (busyRef.current) return;
    void run(goalCommandFor(action));
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
  const journalRows = boundJournal(journal, MAX_JOURNAL_ROWS);

  return (
    <section
      className="chat-panel chat-goal-panel"
      data-has-goal={goal ? 'true' : 'false'}
      data-lifecycle={lifecycle ?? 'none'}
    >
      <div className="chat-panel__head">
        <h2>Goal</h2>
        {goal && (
          <span
            className={`goal-status goal-status--${lifecycle ?? 'none'}`}
            data-lifecycle={lifecycle ?? 'none'}
          >
            {LIFECYCLE_LABEL[lifecycle ?? 'active']}
            {goal.pauseReason ? ` (${goal.pauseReason})` : ''}
          </span>
        )}
      </div>

      {error !== '' && <p className="chat-form-error" role="alert">{safeText(error)}</p>}

      {!goal && (
        <form className="goal-create" onSubmit={(e) => void onCreate(e)}>
          <label className="chat-field goal-create__objective">
            Goal
            <input
              type="text"
              placeholder="What should this agent accomplish?"
              value={objective}
              onChange={(e) => setObjective(e.target.value)}
              maxLength={65536}
              autoComplete="off"
              spellCheck={false}
            />
          </label>
          <label className="chat-field goal-create__budget">
            Token budget
            <input
              type="number"
              min={1}
              step={1}
              placeholder="optional"
              value={budget}
              onChange={(e) => setBudget(e.target.value)}
            />
          </label>
          <button
            className="chat-button chat-button--primary"
            type="submit"
            disabled={busy || objective.trim() === ''}
          >
            Create goal
          </button>
        </form>
      )}

      {goal && (
        <div className="goal-detail">
          <div className="goal-detail__objective" title={goal.objective}>
            {safeText(truncateText(goal.objective, MAX_OBJECTIVE_CHARS))}
          </div>
          <div className="goal-detail__meta">
            <span className="goal-detail__budget">{budgetLine}</span>
            <span className="goal-detail__usage">{usageLine}</span>
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
            {actions.map((action) => (
              <button
                key={action}
                className={`chat-button chat-button--compact${action === 'drop' ? ' chat-button--danger' : ''}`}
                type="button"
                disabled={busy}
                onClick={() => onAction(action)}
              >
                {action === 'pause' ? 'Pause' : action === 'resume' ? 'Resume' : action === 'complete' ? 'Complete' : 'Drop'}
              </button>
            ))}
          </div>
        </div>
      )}

      {goal && journalRows.length > 0 && (
        <div className="goal-journal">
          <div className="goal-section-head">
            <span>Journal</span>
          </div>
          <ul className="goal-journal__list">
            {journalRows.map((entry, index) => (
              <li
                key={entry.revision || index}
                className="goal-journal__entry"
                data-kind={entry.kind?.type ?? 'unknown'}
              >
                <span className="goal-journal__time">
                  {new Date(entry.timestamp).toLocaleTimeString()}
                </span>
                <span className="goal-journal__kind">{formatGoalKind(entry.kind)}</span>
                {entry.goal && entry.goal.objective && (
                  <span className="goal-journal__objective" title={entry.goal.objective}>
                    {safeText(truncateText(entry.goal.objective, MAX_JOURNAL_OBJECTIVE_CHARS))}
                  </span>
                )}
              </li>
            ))}
          </ul>
        </div>
      )}
    </section>
  );
}
