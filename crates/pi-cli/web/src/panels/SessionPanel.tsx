// Session panel — current session info plus the session lifecycle
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

/** Max visible path length in the Session panel (mobile-safe bound). */
export const SESSION_PATH_DISPLAY_MAX = 72;

/**
 * Backend-authoritative path field from get_state (`cwd` / `sessionFile`).
 * Missing/empty/non-string -> Unavailable (never guess). Present values are
 * redacted via safeText and mid-truncated for mobile/narrow layouts; full
 * value stays available for the title/tooltip.
 */
export function formatSessionPath(value: unknown): { text: string; title: string; available: boolean } {
  if (typeof value !== 'string') {
    return { text: 'Unavailable', title: '', available: false };
  }
  const trimmed = value.trim();
  if (trimmed === '') {
    return { text: 'Unavailable', title: '', available: false };
  }
  const safe = safeText(trimmed);
  if (safe.length <= SESSION_PATH_DISPLAY_MAX) {
    return { text: safe, title: safe, available: true };
  }
  // Keep head + tail so both project root and file name remain readable.
  const head = Math.max(20, Math.floor(SESSION_PATH_DISPLAY_MAX * 0.45));
  const tail = SESSION_PATH_DISPLAY_MAX - head - 1;
  const text = `${safe.slice(0, head)}…${safe.slice(-tail)}`;
  return { text, title: safe, available: true };
}

/** Parent directory of a session file path (Unix or Windows separators). */
export function sessionDirectoryOf(sessionFile: unknown): string | null {
  if (typeof sessionFile !== 'string') return null;
  const trimmed = sessionFile.trim();
  if (trimmed === '') return null;
  const cleaned = trimmed.replace(/[\\/]+$/, '');
  const idx = Math.max(cleaned.lastIndexOf('/'), cleaned.lastIndexOf('\\'));
  if (idx <= 0) return null;
  return cleaned.slice(0, idx);
}

/** Project/workspace label: last segment of cwd, or null when unavailable. */
export function projectLabelOf(cwd: unknown): string | null {
  if (typeof cwd !== 'string') return null;
  const trimmed = cwd.trim();
  if (trimmed === '') return null;
  const cleaned = trimmed.replace(/[\\/]+$/, '');
  if (cleaned === '') return null;
  const idx = Math.max(cleaned.lastIndexOf('/'), cleaned.lastIndexOf('\\'));
  const base = (idx >= 0 ? cleaned.slice(idx + 1) : cleaned).trim();
  return base || null;
}

/**
 * Concise New-session ownership/storage hint from get_state fields only.
 * Never invents paths — missing fields become "Unavailable".
 */
export function newSessionLocationHint(cwd: unknown, sessionFile: unknown): string {
  const project = projectLabelOf(cwd);
  const cwdDisp = formatSessionPath(cwd);
  const fileDisp = formatSessionPath(sessionFile);
  const dirRaw = sessionDirectoryOf(sessionFile);
  const dirDisp = dirRaw ? formatSessionPath(dirRaw) : { text: 'Unavailable', title: '', available: false };
  const projectPart = project ? safeText(project) : 'Unavailable';
  const cwdPart = cwdDisp.available ? cwdDisp.text : 'Unavailable';
  const storePart = dirDisp.available
    ? dirDisp.text
    : fileDisp.available
      ? fileDisp.text
      : 'Unavailable';
  return `New session inherits project ${projectPart} (cwd ${cwdPart}) and stores under ${storePart}.`;
}

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
  /** True when the row is an unnamed, tiny, native rpi session recorded under
   *  the OS temp root (historical test-harness shape). The sidebar hides
   *  these by default but keeps them searchable; loaded/active rows always
   *  stay visible. Recoverable view signal — never a deletion. */
  temporary?: boolean;
}

export function sessionRowKey(
  row: Pick<SessionRowWire, 'source' | 'sessionId' | 'path'>,
): string {
  return `${row.source || 'pi'}::${row.sessionId}::${row.path}`;
}

export function isLoadedCurrentSession(
  row: Pick<SessionRowWire, 'loaded' | 'sessionId'>,
  activeSessionId: string | null | undefined,
  hasLoadedActiveRow = false,
): boolean {
  if (row.sessionId !== activeSessionId) return false;
  return row.loaded === true || !hasLoadedActiveRow;
}

interface ForkMessageWire {
  entryId: string;
  text: string;
}

interface SessionPanelProps {
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>;
  /** Bounded wait for the current socket to reach OPEN. The mount auto-load
   *  awaits this before `sendCommand` so a mount-before-WebSocket-OPEN surfaces
   *  no persistent `load failed: not connected`; active lifecycle/rename
   *  actions bypass it (via `load({waitForReady:false})`) and fail fast. */
  waitForReady: () => Promise<void>;
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

export function SessionPanel({ sendCommand, waitForReady, refreshState, onLifecycleResult, onClose }: SessionPanelProps) {
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

  /** Re-fetch state/stats/catalog/fork messages. Auto callers (mount effect)
   *  pass `waitForReady: true` (default) so a mount-before-WebSocket-OPEN
   *  waits bounded for the socket to open instead of surfacing a persistent
   *  `load failed: not connected`; a gate timeout or a transient post-gate
   *  `not connected` race is swallowed silently (a re-open re-mounts the panel
   *  or the user refreshes) — never a permanent error. Active lifecycle/rename
   *  flows pass `waitForReady: false` so a disconnect FAILS FAST on the action. */
  const load = useCallback(async (opts?: { waitForReady?: boolean }): Promise<void> => {
    const wait = opts?.waitForReady !== false;
    if (wait) {
      try {
        await waitForReady();
      } catch {
        return; // gate timeout during disconnect — silent; re-open re-mounts
      }
    }
    try {
      const [stateData, statsData, listData, forkData] = await Promise.all([
        sendCommand({ type: 'get_state' }),
        sendCommand({ type: 'get_session_stats' }),
        sendCommand({ type: 'session_list', scope: 'all_projects' }),
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
    } catch (err) {
      // A post-gate `not connected` is a transient race (socket dropped between
      // gate-resolve and send); never a persistent `load failed: not
      // connected`. Other errors surface.
      if (wait && (err as Error).message === 'not connected') return;
      throw err;
    }
  }, [sendCommand, waitForReady, forkEntry]);

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
        // Fail-fast refresh: the action used sendCommand directly, so a
        // disconnect here is a transient post-action race — swallow
        // `not connected` (no misleading "${label} failed" after success);
        // other refresh errors surface.
        await load({ waitForReady: false }).catch((err: Error) => {
          if ((err as Error).message === 'not connected') return;
          throw err;
        });
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
        // Fail-fast refresh (see run): swallow a transient post-action
        // `not connected`; other refresh errors surface.
        await load({ waitForReady: false }).catch((err: Error) => {
          if ((err as Error).message === 'not connected') return;
          throw err;
        });
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
  const hasLoadedCurrentRow = sessions.some(
    (row) => row.loaded === true && row.sessionId === currentId,
  );
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
        <button id="session-close-btn" type="button" className="panel-close" onClick={onClose} title="Close panel" aria-label="Close session panel">
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
            <dt>Project</dt>
            <dd data-testid="session-project-value">
              {(() => {
                const label = projectLabelOf(info?.cwd);
                return label ? safeText(label) : 'Unavailable';
              })()}
            </dd>
            <dt>Working directory</dt>
            <dd
              data-testid="session-cwd-value"
              className="session-info__path"
              title={formatSessionPath(info?.cwd).title}
            >
              {formatSessionPath(info?.cwd).text}
            </dd>
            <dt>Session file</dt>
            <dd
              data-testid="session-file-value"
              className="session-info__path"
              title={formatSessionPath(info?.sessionFile).title}
            >
              {formatSessionPath(info?.sessionFile).text}
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

          <p
            className="session-info__new-hint"
            data-testid="session-new-location-hint"
            title={(() => {
              const cwdFull = formatSessionPath(info?.cwd);
              const fileFull = formatSessionPath(info?.sessionFile);
              const dirRaw = sessionDirectoryOf(info?.sessionFile);
              const dirFull = dirRaw ? formatSessionPath(dirRaw) : null;
              const parts: string[] = [];
              if (cwdFull.available) parts.push(`cwd: ${cwdFull.title}`);
              if (dirFull?.available) parts.push(`session dir: ${dirFull.title}`);
              else if (fileFull.available) parts.push(`session file: ${fileFull.title}`);
              return parts.join(' · ');
            })()}
          >
            {newSessionLocationHint(info?.cwd, info?.sessionFile)}
          </p>

          <div className="session-actions">
            <button
              id="session-new-btn"
              type="button"
              onClick={onNew}
              disabled={busy}
              title="Start a fresh session in this project/cwd (new_session)"
            >
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
          <h3 className="panel__subhead">Saved sessions (all projects)</h3>
          {sessions.length === 0 && <div className="panel__empty">No saved sessions found.</div>}
          <ul className="session-list__rows">
            {sessions.map((row) => {
              const isCurrent = isLoadedCurrentSession(row, currentId, hasLoadedCurrentRow);
              return (
                <li key={sessionRowKey(row)} className={`session-row${isCurrent ? ' session-row--current' : ''}`}>
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
