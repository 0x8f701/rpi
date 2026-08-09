// Session panel (D92) — current session info plus the session lifecycle
// actions: new session, switch (from the unified resume catalog), fork (from
// forkable user messages), clone, and rename.
//
// Wire shapes mirror pi-cli/src/modes/rpc.rs (serde camelCase):
//   get_state -> RpcSessionState (now includes cwd)
//   get_session_stats -> SessionStats
//   session_list -> { sessions: RpcSessionListRow[] }
//   get_fork_messages -> { messages: [{ entryId, text }] }
//   new_session / switch_session / fork / clone / set_session_name
// EVERY string derived from the model/catalog passes through safeText().

import { useCallback, useEffect, useRef, useState } from 'react';
import { safeText } from '../redact';

export interface SessionInfo {
  model?: { id?: string; name?: string; provider?: string } | null;
  thinkingLevel?: string;
  sessionFile?: string | null;
  sessionId?: string | null;
  sessionName?: string | null;
  cwd?: string;
  messageCount?: number;
  pendingMessageCount?: number;
  isStreaming?: boolean;
}

export interface SessionTokenStatsWire {
  input?: number;
  output?: number;
  cacheRead?: number;
  cacheWrite?: number;
  total?: number;
}

export interface SessionStatsWire {
  sessionFile?: string | null;
  sessionId?: string | null;
  userMessages?: number;
  assistantMessages?: number;
  toolCalls?: number;
  toolResults?: number;
  totalMessages?: number;
  tokens?: SessionTokenStatsWire;
  cost?: number;
  contextUsage?: { tokens?: number | null; contextWindow?: number; percent?: number | null } | null;
}

export interface SessionRowWire {
  source?: string;
  sessionId: string;
  name?: string | null;
  cwd?: string;
  displayTime?: string;
  modifiedEpoch?: number;
  summary?: string;
  path: string;
  size?: number;
  messageCount?: number | null;
  status?: string;
  /** Present (true) when the multi-session manager currently has this
   *  session loaded (session_list `loaded` overlay). */
  loaded?: boolean;
}

interface ForkMessageWire {
  entryId: string;
  text: string;
}

interface SessionPanelProps {
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>;
  /** Re-fetch get_state into the app shell (header session name etc.). */
  refreshState: () => Promise<unknown>;
  /** Consume a lifecycle RPC result atomically: `{sessionId,state,messages}`
   *  from switch_session/new_session/fork/clone (MultiSessionRuntimeManager
   *  contract), or fall back to get_state when no snapshot is present. */
  onLifecycleResult: (result: unknown) => Promise<unknown>;
  onClose: () => void;
}

interface SessionListWire {
  sessions: SessionRowWire[];
}

interface ForkMessagesWire {
  messages: ForkMessageWire[];
}

function formatTokens(stats: SessionStatsWire | null): string {
  if (!stats || !stats.tokens) return '—';
  const t = stats.tokens;
  const parts: string[] = [];
  if (typeof t.input === 'number') parts.push(`${t.input} in`);
  if (typeof t.output === 'number') parts.push(`${t.output} out`);
  if (typeof t.cacheRead === 'number' && t.cacheRead > 0) parts.push(`${t.cacheRead} cache-read`);
  if (typeof t.cacheWrite === 'number' && t.cacheWrite > 0) parts.push(`${t.cacheWrite} cache-write`);
  return parts.length > 0 ? parts.join(' · ') : '—';
}

export function SessionPanel({ sendCommand, refreshState, onLifecycleResult, onClose }: SessionPanelProps) {
  const [info, setInfo] = useState<SessionInfo | null>(null);
  const [stats, setStats] = useState<SessionStatsWire | null>(null);
  const [sessions, setSessions] = useState<SessionRowWire[]>([]);
  const [forkMessages, setForkMessages] = useState<ForkMessageWire[]>([]);
  const [forkEntry, setForkEntry] = useState('');
  const [rename, setRename] = useState('');
  const [renaming, setRenaming] = useState(false);
  const renameInputRef = useRef<HTMLInputElement>(null);
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState('');
  const mountedRef = useRef(true);

  const load = useCallback(async () => {
    const [stateData, statsData, listData, forkData] = await Promise.all([
      sendCommand({ type: 'get_state' }),
      sendCommand({ type: 'get_session_stats' }),
      sendCommand({ type: 'session_list' }),
      sendCommand({ type: 'get_fork_messages' }),
    ]);
    if (!mountedRef.current) return;
    // These wire shapes are the rpi control plane's own RPC contract
    // (RpcSessionState / SessionStats / session_list / get_fork_messages).
    const info = stateData as SessionInfo;
    const stats = statsData as SessionStatsWire | null;
    const list = listData as SessionListWire;
    const forkDataWire = forkData as ForkMessagesWire;
    setInfo(info || null);
    setStats(stats);
    setSessions(Array.isArray(list?.sessions) ? list.sessions : []);
    const forkMessagesWire = Array.isArray(forkDataWire?.messages) ? forkDataWire.messages : [];
    setForkMessages(forkMessagesWire);
    if (forkMessagesWire.length > 0 && !forkMessagesWire.some((f) => f.entryId === forkEntry)) {
      setForkEntry(forkMessagesWire[0].entryId);
    }
  }, [sendCommand, forkEntry]);

  useEffect(() => {
    mountedRef.current = true;
    load().catch((err: Error) => {
      if (mountedRef.current) setStatus(`load failed: ${safeText(err.message)}`);
    });
    return () => {
      mountedRef.current = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const run = useCallback(
    async (action: () => Promise<unknown>, label: string) => {
      if (busy) return;
      setBusy(true);
      setStatus('');
      try {
        await action();
        await refreshState();
        await load();
        setStatus(`${label} ok`);
      } catch (err) {
        setStatus(`${label} failed: ${safeText((err as Error).message)}`);
      } finally {
        if (mountedRef.current) setBusy(false);
      }
    },
    [busy, load, refreshState]
  );

  /** Lifecycle actions (new/clone/fork/switch) resolve with the TARGET
   *  session's `{sessionId,state,messages}` snapshot: consume it ATOMICALLY
   *  through onLifecycleResult (which switches the active view + transcript)
   *  instead of refreshState — re-querying get_state with the SOURCE
   *  sessionId would snap the view back. load() then re-reads the panel for
   *  the newly active session. */
  const runLifecycle = useCallback(
    async (action: () => Promise<unknown>, label: string) => {
      if (busy) return;
      setBusy(true);
      setStatus('');
      try {
        const result = await action();
        await onLifecycleResult(result);
        await load();
        setStatus(`${label} ok`);
      } catch (err) {
        setStatus(`${label} failed: ${safeText((err as Error).message)}`);
      } finally {
        if (mountedRef.current) setBusy(false);
      }
    },
    [busy, load, onLifecycleResult]
  );

  const onNew = () => runLifecycle(() => sendCommand({ type: 'new_session' }), 'new session');
  const onClone = () => runLifecycle(() => sendCommand({ type: 'clone' }), 'clone');
  const onFork = () => {
    if (!forkEntry) {
      setStatus('no forkable message selected');
      return;
    }
    runLifecycle(() => sendCommand({ type: 'fork', entryId: forkEntry }), 'fork');
  };
  const onSwitch = (path: string) =>
    runLifecycle(() => sendCommand({ type: 'switch_session', sessionPath: path }), 'switch');
  const onRename = () => {
    // Read the live DOM value (not the state) so a freshly typed name is
    // committed even if the controlled-input state update hasn't rendered.
    const name = (renameInputRef.current?.value ?? rename).trim();
    if (!name) {
      setStatus('session name cannot be empty');
      return;
    }
    setRenaming(true);
    run(() => sendCommand({ type: 'set_session_name', name }), 'rename').finally(() => {
      if (mountedRef.current) setRenaming(false);
    });
  };

  const currentId = info?.sessionId || stats?.sessionId || null;
  const tokens = formatTokens(stats);
  const contextPct =
    stats?.contextUsage?.percent != null ? `${Math.round(stats.contextUsage.percent)}%` : '—';

  return (
    <section id="session-panel" className="panel" aria-label="Session panel">
      <header className="panel__head">
        <span className="panel__title">Session</span>
        <span className="panel__hint">
          lifecycle: new · switch · fork · clone · rename
        </span>
        <button id="session-refresh-btn" type="button" onClick={() => load().catch(() => {})} disabled={busy}>
          Refresh
        </button>
        <button id="session-close-btn" type="button" onClick={onClose} title="Close panel">
          ✕
        </button>
      </header>

      {status !== '' && (
        <div className={`panel__status${status.includes('failed') ? ' panel__status--error' : ''}`}>
          {safeText(status)}
        </div>
      )}

      <div className="panel__body">
        <div className="session-info">
          <h3 className="panel__subhead">Current session</h3>
          <dl className="session-info__grid">
            <dt>Name</dt>
            <dd data-testid="session-name-value">{info?.sessionName ? safeText(info.sessionName) : '—'}</dd>
            <dt>Session id</dt>
            <dd>{info?.sessionId ? safeText(info.sessionId) : '—'}</dd>
            <dt>Model</dt>
            <dd>
              {info?.model?.id
                ? safeText(`${info.model.provider || '?'}/${info.model.id}`)
                : '—'}
            </dd>
            <dt>Thinking</dt>
            <dd>{info?.thinkingLevel ? safeText(info.thinkingLevel) : '—'}</dd>
            <dt>Working directory</dt>
            <dd title={info?.cwd ? safeText(info.cwd) : ''}>
              {info?.cwd ? safeText(info.cwd) : '—'}
            </dd>
            <dt>Session file</dt>
            <dd title={info?.sessionFile ? safeText(info.sessionFile) : ''}>
              {info?.sessionFile ? safeText(info.sessionFile) : '—'}
            </dd>
            <dt>Messages</dt>
            <dd>
              {typeof info?.messageCount === 'number' ? info.messageCount : '—'}
              {typeof info?.pendingMessageCount === 'number' && info.pendingMessageCount > 0
                ? ` (${info.pendingMessageCount} pending)`
                : ''}
            </dd>
            <dt>Tokens</dt>
            <dd>{safeText(tokens)}</dd>
            <dt>Context</dt>
            <dd>{safeText(contextPct)}</dd>
          </dl>

          <div className="session-actions">
            <button id="session-new-btn" type="button" onClick={onNew} disabled={busy} title="Start a fresh session (new_session)">
              New session
            </button>
            <button id="session-clone-btn" type="button" onClick={onClone} disabled={busy} title="Clone the current branch (clone)">
              Clone
            </button>
            <span className="session-fork">
              <select
                id="session-fork-select"
                value={forkEntry}
                disabled={forkMessages.length === 0 || busy}
                onChange={(e) => setForkEntry(e.target.value)}
                title="Fork from a previous user message"
              >
                {forkMessages.length === 0 && <option value="">no forkable messages</option>}
                {forkMessages.map((f) => (
                  <option key={f.entryId} value={f.entryId}>
                    {safeText(f.text.length > 60 ? `${f.text.slice(0, 60)}…` : f.text)}
                  </option>
                ))}
              </select>
              <button
                id="session-fork-btn"
                type="button"
                onClick={onFork}
                disabled={forkMessages.length === 0 || busy}
              >
                Fork
              </button>
            </span>
            <span className="session-rename">
              <input
                id="session-rename-input"
                ref={renameInputRef}
                type="text"
                value={rename}
                placeholder="Rename this session…"
                onChange={(e) => setRename(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter') onRename();
                }}
                disabled={busy}
              />
              <button id="session-rename-btn" type="button" onClick={onRename} disabled={busy || renaming}>
                Rename
              </button>
            </span>
          </div>
        </div>

        <div className="session-list">
          <h3 className="panel__subhead">Saved sessions (this working directory)</h3>
          {sessions.length === 0 && <div className="panel__empty">No saved sessions found.</div>}
          <ul className="session-list__rows">
            {sessions.map((row) => {
              const isCurrent = row.sessionId === currentId;
              return (
                <li key={row.sessionId} className={`session-row${isCurrent ? ' session-row--current' : ''}`}>
                  <div className="session-row__main">
                    <span className="session-row__name">
                      {row.name ? safeText(row.name) : safeText(row.sessionId)}
                      {isCurrent ? ' (current)' : ''}
                    </span>
                    <span className="session-row__meta">
                      {row.displayTime ? safeText(row.displayTime) : ''}
                      {row.messageCount != null ? ` · ${row.messageCount} msgs` : ''}
                      {row.source ? ` · ${safeText(row.source)}` : ''}
                    </span>
                    {row.summary ? (
                      <span className="session-row__summary">{safeText(row.summary)}</span>
                    ) : null}
                    <span className="session-row__cwd" title={row.cwd ? safeText(row.cwd) : ''}>
                      {row.cwd ? safeText(row.cwd) : ''}
                    </span>
                  </div>
                  {!isCurrent && (
                    <button
                      type="button"
                      className="session-row__switch"
                      onClick={() => onSwitch(row.path)}
                      disabled={busy}
                      title="Resume this saved session (switch_session)"
                    >
                      Switch
                    </button>
                  )}
                </li>
              );
            })}
          </ul>
        </div>
      </div>
    </section>
  );
}
