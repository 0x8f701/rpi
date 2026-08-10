// Session sidebar (D92) — persistent left rail listing saved sessions from the
// `session_list` RPC, with New / Fork / Clone entries and click-to-switch
// (switch_session). Collapses to a drawer on narrow screens (CSS).
// Every catalog-derived string passes through safeText().

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { safeText } from '../redact';
import type { SessionRowWire } from './SessionPanel';

/**
 * One node of the sidebar session tree: `agent? -> project -> sessions`.
 * Agent nodes exist only when the wire row carries an `agent`/`model` field
 * (none today — kept defensive); otherwise the roots are project nodes.
 */
interface SessionGroupNode {
  key: string;
  label: string;
  sessions: SessionRowWire[];
  children: SessionGroupNode[];
}

type SessionTreeItem =
  | { kind: 'group'; node: SessionGroupNode; depth: number; sessionCount: number }
  | { kind: 'row'; row: SessionRowWire; depth: number };

/** Agent name for a row. SessionRowWire has no agent/model field today, but
 *  accept them defensively so a future backend addition surfaces as a
 *  top-level group instead of silently flattening into the project tree. */
function agentNameOf(row: SessionRowWire): string | null {
  const extended = row as SessionRowWire & { agent?: unknown; model?: unknown };
  const agent = typeof extended.agent === 'string' ? extended.agent : '';
  const model = typeof extended.model === 'string' ? extended.model : '';
  const name = (agent || model).trim();
  return name.length > 0 ? name : null;
}

/** Last path segment of a real (unescaped) cwd — the exact project name. */
function basenameOf(cwd: string): string {
  const cleaned = cwd.replace(/[\\/]+$/, '');
  const idx = Math.max(cleaned.lastIndexOf('/'), cleaned.lastIndexOf('\\'));
  return (idx >= 0 ? cleaned.slice(idx + 1) : cleaned).trim() || 'Unknown project';
}

/** Directory names that commonly hold projects; used to recover the project
 *  tail from a lossy encoded path. */
const PROJECT_ROOT_SEGMENTS: Record<string, true> = {
  projects: true,
  project: true,
  repos: true,
  repo: true,
  repositories: true,
  src: true,
  source: true,
  work: true,
  workspace: true,
  workspaces: true,
  code: true,
  dev: true,
  development: true,
  apps: true,
  app: true,
};

/** Project name derived from a session *file* path (used when `cwd` is
 *  absent). Native sessions live at `<root>/--<encoded-cwd>--/<file>.jsonl`,
 *  where the encoded cwd is the absolute path with the leading '/' removed
 *  and `/ \ :` replaced by '-' (encode_cwd_safe_path in pi-coding). That
 *  encoding is lossy for names containing '-', so the encoded directory is
 *  split on '-' and the tail after a known project-parent segment is kept:
 *  `--home-cj-Projects-parth-generic-v1--` -> `parth-generic-v1`. */
function projectFromSessionPath(path: string): string {
  const trimmed = path.trim();
  if (!trimmed) return 'Unknown project';
  const segment = trimmed.match(/(?:^|[/\\])(--[^/\\]*--)[/\\]/)?.[1];
  if (segment) {
    const encoded = segment.slice(2, -2);
    const parts = encoded.split('-').filter((part) => part.length > 0);
    if (parts.length > 1) {
      for (let i = parts.length - 1; i >= 1; i -= 1) {
        if (PROJECT_ROOT_SEGMENTS[parts[i].toLowerCase()]) {
          const tail = parts.slice(i + 1).join('-');
          if (tail) return tail;
        }
      }
    }
    return parts[parts.length - 1] || 'Unknown project';
  }
  // Plain file path (foreign sessions): fall back to the basename.
  const cleaned = trimmed.replace(/[\\/]+$/, '');
  const idx = Math.max(cleaned.lastIndexOf('/'), cleaned.lastIndexOf('\\'));
  const base = idx >= 0 ? cleaned.slice(idx + 1) : cleaned;
  return base.replace(/\.[^.]+$/, '') || 'Unknown project';
}

/** Project directory for a row: the real `cwd` basename when available,
 *  otherwise derived from the session file path. */
function projectNameOf(row: SessionRowWire): string {
  const cwd = row.cwd?.trim();
  if (cwd) return basenameOf(cwd);
  return projectFromSessionPath(row.path);
}

/** Human-readable session title: explicit name > summary excerpt > project name.
 *  Never shows a raw UUID — falls back to the project name so the sidebar
 *  reads like ChatGPT's session list. */
function sessionTitle(row: SessionRowWire): string {
  const name = row.name?.trim();
  if (name) return safeText(name);
  const summary = row.summary?.trim();
  if (summary) return safeText(summary.length > 60 ? `${summary.slice(0, 57)}…` : summary);
  return projectNameOf(row);
}

/** Build the `agent? -> project -> sessions` tree in catalog order (the Rust
 *  side already sorts newest-first), grouping rows by agent then project. */
function groupSessions(rows: SessionRowWire[]): SessionGroupNode[] {
  const roots: SessionGroupNode[] = [];
  const rootIndex = new Map<string, SessionGroupNode>();
  const projectIndex = new Map<string, SessionGroupNode>();
  for (const row of rows) {
    const agent = agentNameOf(row);
    const project = projectNameOf(row);
    const fullKey = agent ? `agent::${agent}::project::${project}` : `project::${project}`;
    let projectNode = projectIndex.get(fullKey);
    if (!projectNode) {
      projectNode = { key: fullKey, label: project, sessions: [], children: [] };
      projectIndex.set(fullKey, projectNode);
      if (agent) {
        const agentKey = `agent::${agent}`;
        let agentNode = rootIndex.get(agentKey);
        if (!agentNode) {
          agentNode = { key: agentKey, label: agent, sessions: [], children: [] };
          rootIndex.set(agentKey, agentNode);
          roots.push(agentNode);
        }
        agentNode.children.push(projectNode);
      } else {
        roots.push(projectNode);
      }
    }
    projectNode.sessions.push(row);
  }
  return roots;
}

function subtreeSessionCount(node: SessionGroupNode): number {
  let count = node.sessions.length;
  for (const child of node.children) count += subtreeSessionCount(child);
  return count;
}

/** Flatten the group tree into an ordered list of headers and session rows,
 *  skipping collapsed subtrees entirely. */
function flattenTree(
  nodes: SessionGroupNode[],
  collapsed: Set<string>,
  depth: number,
  out: SessionTreeItem[]
): void {
  for (const node of nodes) {
    out.push({ kind: 'group', node, depth, sessionCount: subtreeSessionCount(node) });
    if (collapsed.has(node.key)) continue;
    flattenTree(node.children, collapsed, depth + 1, out);
    for (const row of node.sessions) {
      out.push({ kind: 'row', row, depth: depth + 1 });
    }
  }
}

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
  /** Group keys (agent::<agent> / project::<project>) whose subtree is
   *  collapsed in the tree view. */
  const [collapsedGroups, setCollapsedGroups] = useState<Set<string>>(() => new Set());

  const groups = useMemo(() => groupSessions(sessions), [sessions]);
  const treeItems = useMemo(() => {
    const out: SessionTreeItem[] = [];
    flattenTree(groups, collapsedGroups, 0, out);
    return out;
  }, [groups, collapsedGroups]);

  const toggleGroup = useCallback((key: string) => {
    setCollapsedGroups((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  }, []);

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
        {treeItems.map((item) => {
          if (item.kind === 'group') {
            const collapsed = collapsedGroups.has(item.node.key);
            return (
              <li key={item.node.key} className="session-sidebar__group">
                <button
                  type="button"
                  className="session-sidebar__group-head"
                  aria-expanded={!collapsed}
                  style={{ paddingLeft: 4 + item.depth * 14 }}
                  title={`${collapsed ? 'Expand' : 'Collapse'} ${safeText(item.node.label)}`}
                  onClick={() => toggleGroup(item.node.key)}
                >
                  <span className="session-sidebar__group-caret" aria-hidden="true">
                    {collapsed ? '▸' : '▾'}
                  </span>
                  <span className="session-sidebar__group-label">{safeText(item.node.label)}</span>
                  <span className="session-sidebar__group-count">{item.sessionCount}</span>
                </button>
              </li>
            );
          }
          const row = item.row;
          const isActive = row.sessionId === activeSessionId;
          return (
            <li
              key={row.sessionId}
              className={isActive ? 'session-sidebar__row session-sidebar__row--active' : 'session-sidebar__row'}
              style={item.depth > 0 ? { paddingLeft: item.depth * 14 } : undefined}
            >
              <div className="session-sidebar__row-main">
              <button
                type="button"
                className="session-sidebar__switch"
                data-session-id={row.sessionId}
                title={`Switch to ${sessionTitle(row)}`}
                disabled={isActive || busy}
                onClick={() =>
                  run(() => sendCommand({ type: 'switch_session', sessionPath: row.path }))
                    .then(onSwitchComplete)
                    .catch(() => {})
                }
              >
                <span className="session-sidebar__name">
                  {sessionTitle(row)}
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
                  title={`Close session ${sessionTitle(row)}`}
                  type="button"
                  className="session-sidebar__close"
                  data-session-id={row.sessionId}
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
