// Session sidebar (D92) — persistent left rail listing saved sessions from the
// `session_list` RPC, with New / Fork / Clone entries and click-to-switch
// (switch_session). Collapses to a drawer on narrow screens (CSS).
// Every catalog-derived string passes through safeText().

import { useCallback, useEffect, useRef, useState } from 'react';
import { safeText } from '../redact';
import type { SessionRowWire } from './SessionPanel';

interface SessionSidebarProps {
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>;
  /** Consume a lifecycle RPC result atomically: `{sessionId,state,messages}`
   *  from switch_session/new_session (MultiSessionRuntimeManager contract), or
   *  fall back to get_state when no snapshot is present. */
  onLifecycleResult: (result: unknown) => Promise<unknown>;
  activeSessionId?: string | null;
  /** Per-session unread counts (background-session events). */
  unreadBySessionId?: Record<string, number>;
  /** Feature nav (panel toggles) rendered at the top of the rail/drawer. */
  featureNav?: React.ReactNode;
  /** Reopen the desktop rail from its collapsed strip. */
  onReopenRail: () => void;
  /** close_session for a row (non-destructive; busy closes surface refusal). */
  onCloseSession: (sessionId: string) => void;
  /** Open the full Session panel (detail + rename + fork/clone). */
  onOpenManage: () => void;
  onSwitchComplete: () => void;
}

export function SessionSidebar({
  sendCommand,
  onLifecycleResult,
  activeSessionId,
  unreadBySessionId,
  featureNav,
  onReopenRail,
  onCloseSession,
  onOpenManage,
  onSwitchComplete,
}: SessionSidebarProps) {
  const [sessions, setSessions] = useState<SessionRowWire[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const mountedRef = useRef(true);

  const load = useCallback(async () => {
    const data = await sendCommand({ type: 'session_list' });
    if (!mountedRef.current) return;
    // Own RPC contract: session_list -> { sessions: RpcSessionListRow[] }.
    const list = data as { sessions?: SessionRowWire[] };
    setSessions(Array.isArray(list?.sessions) ? list.sessions : []);
    setError('');
  }, [sendCommand]);

  useEffect(() => {
    mountedRef.current = true;
    load().catch((err: Error) => {
      if (mountedRef.current) setError(`load failed: ${safeText(err.message)}`);
    });
    // Re-list whenever the active session changes (new/switch/rename) and
    // poll lightly so sessions that appear mid-run (e.g. the first persist
    // after the first assistant turn) show up without a manual refresh.
    const poll = window.setInterval(() => {
      load().catch(() => {});
    }, 8000);
    return () => {
      mountedRef.current = false;
      window.clearInterval(poll);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeSessionId]);

  const run = useCallback(
    async (action: () => Promise<unknown>) => {
      if (busy) return;
      setBusy(true);
      setError('');
      try {
        const result = await action();
        // Atomic snapshot consumption: switch/new resolve with the TARGET
        // session's {sessionId,state,messages}; re-querying here with the
        // source sessionId would snap the view back.
        await onLifecycleResult(result);
        await load();
      } catch (err) {
        setError(safeText((err as Error).message));
      } finally {
        if (mountedRef.current) setBusy(false);
      }
    },
    [busy, load, onLifecycleResult]
  );

  return (
    <aside id="session-sidebar" className="session-sidebar" aria-label="Sessions">
      <header className="session-sidebar__head">
        <span className="session-sidebar__title">Sessions</span>
        {/* Desktop collapsed-rail reopen control (visible only in the
            collapsed strip via CSS). */}
        <button
          id="rail-reopen-btn"
          type="button"
          title="Expand the session sidebar"
          onClick={onReopenRail}
        >
          »
        </button>
        <button
          id="sidebar-new-session-btn"
          type="button"
          title="Start a fresh session (new_session)"
          onClick={() =>
            run(() => sendCommand({ type: 'new_session' })).then(onSwitchComplete).catch(() => {})
          }
          disabled={busy}
        >
          New
        </button>
      </header>

      {error !== '' && <div className="session-sidebar__error">{safeText(error)}</div>}

      {featureNav && <nav className="session-sidebar__nav">{featureNav}</nav>}

      <ul className="session-sidebar__list">
        {sessions.length === 0 && !error && (
          <li className="session-sidebar__empty">No saved sessions.</li>
        )}
        {sessions.map((row) => {
          const isActive = row.sessionId === activeSessionId;
          return (
            <li key={row.sessionId} className={isActive ? 'session-sidebar__row session-sidebar__row--active' : 'session-sidebar__row'}>
              <div className="session-sidebar__row-main">
              <button
                type="button"
                className="session-sidebar__switch"
                data-session-id={row.sessionId}
                title={`Switch to ${row.name ? safeText(row.name) : safeText(row.sessionId)}`}
                disabled={isActive || busy}
                onClick={() =>
                  run(() => sendCommand({ type: 'switch_session', sessionPath: row.path }))
                    .then(onSwitchComplete)
                    .catch(() => {})
                }
              >
                <span className="session-sidebar__name">
                  {row.name ? safeText(row.name) : safeText(row.sessionId)}
                </span>
                <span className="session-sidebar__meta">
                  {row.displayTime ? safeText(row.displayTime) : ''}
                  {row.messageCount != null ? ` · ${row.messageCount} msgs` : ''}
                  {row.loaded === true ? ' · live' : ''}
                </span>
                {(unreadBySessionId?.[row.sessionId] ?? 0) > 0 && (
                  <span
                    className="session-sidebar__unread"
                    data-unread-for={row.sessionId}
                    data-unread={unreadBySessionId?.[row.sessionId]}
                  >
                    {unreadBySessionId?.[row.sessionId]}
                  </span>
                )}
                {row.loaded === true && (
                  <span
                    className="session-sidebar__loaded"
                    data-loaded-for={row.sessionId}
                    title="This session is loaded and running"
                  >
                    live
                  </span>
                )}
                {row.summary ? (
                  <span className="session-sidebar__summary">{safeText(row.summary)}</span>
                ) : null}
              </button>
              {/* close_session: non-primary, non-active rows only; busy closes
                  surface the refusal via the App toast. */}
              {!isActive && row.source !== 'primary' && (
                <button
                  id={`session-row-close-btn-${row.sessionId}`}
                  type="button"
                  className="session-sidebar__close"
                  data-session-id={row.sessionId}
                  title={`Close session ${row.name ? safeText(row.name) : safeText(row.sessionId)}`}
                  disabled={false}
                  onClick={() => onCloseSession(row.sessionId)}
                >
                  ✕
                </button>
              )}
              </div>
            </li>
          );
        })}
      </ul>

      <footer className="session-sidebar__foot">
        <button
          id="sidebar-manage-btn"
          type="button"
          title="Open the session panel (info, rename, fork, clone)"
          onClick={onOpenManage}
        >
          Manage session…
        </button>
      </footer>
    </aside>
  );
}
