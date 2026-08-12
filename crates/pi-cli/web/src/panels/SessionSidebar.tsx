// Persistent session sidebar listing saved sessions from the control plane
// `session_list` RPC, with New / Fork / Clone entries and click-to-switch
// (switch_session). Collapses to a drawer on narrow screens (CSS).
// Every catalog-derived string passes through safeText().

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { safeText } from '../redact';
import { isLoadedCurrentSession, sessionRowKey, type SessionRowWire } from './SessionPanel';

/**
 * One node of the sidebar session tree: `provider -> project? -> sessions`.
 * Every row lands under one of four provider groups (rpi, Codex, Grok, OMP);
 * project/cwd basename is a secondary subgroup. Sessions with noise project
 * names (tmp, dot-tmp prefixes, bare UUIDs, or source) are listed directly under the provider
 * without a distinct subgroup. A bounded search input filters by title,
 * project, provider, source, or session id; the active session stays visible.
 */
interface SessionGroupNode {
  key: string;
  label: string;
  /** Display kind: `provider` for top-level source groups, `project` for
   *  secondary project/cwd groups nested under a provider. */
  kind: 'provider' | 'project';
  sessions: SessionRowWire[];
  children: SessionGroupNode[];
}

type SessionTreeItem =
  | { kind: 'group'; node: SessionGroupNode; depth: number; sessionCount: number }
  | { kind: 'row'; row: SessionRowWire; depth: number };

/** The four provider labels allowed as top-level sidebar groups. The order
 *  is fixed so the sidebar layout is stable regardless of catalog row order. */
const PROVIDER_ORDER = ['rpi', 'Codex', 'Grok', 'OMP'] as const;

/** Canonical provider display label for a catalog row, or `null` when the
 *  wire `source` is not one of the four allowed providers (rpi, Codex, Grok,
 *  OMP). Native aliases (pi, native, primary) map to rpi; grok and grok/hyper
 *  both map to Grok. Unknown, empty, or dirty sources return null — the row
 *  is filtered from the sidebar rather than silently misgrouped. */
export function providerOf(row: SessionRowWire): string | null {
  const source = row.source?.trim().toLowerCase() || '';
  if (source === 'pi' || source === 'native' || source === 'primary') return 'rpi';
  if (source === 'codex') return 'Codex';
  if (source === 'grok' || source === 'grok/hyper') return 'Grok';
  if (source === 'omp') return 'OMP';
  return null;
}

/** Directory names that are structural noise, not real project labels. A
 *  session whose project name matches one of these is still listed (and
 *  searchable) under its provider, but does not get a distinct project
 *  subgroup — it appears directly under the provider node. */
const NOISE_PROJECT_NAMES: Record<string, true> = { tmp: true, temp: true, source: true };

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/** True when a project basename is a tmp directory, a bare UUID, or another
 *  structural label that should not become a visible sidebar group. */
export function isNoiseProjectName(name: string): boolean {
  const lower = name.trim().toLowerCase();
  if (lower === '' || lower === 'unknown project') return true;
  if (NOISE_PROJECT_NAMES[lower]) return true;
  if (lower.startsWith('.tmp')) return true;
  if (UUID_RE.test(lower)) return true;
  return false;
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
 *  where the encoded cwd is the absolute path with the leading separator
 *  removed and path separators replaced by `-`. The encoding is lossy for
 *  names containing `-`, so the tail after a known project-parent segment is
 *  kept: `--workspace-projects-parth-generic-v1--` -> `parth-generic-v1`. */
export function projectFromSessionPath(path: string): string {
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
export function projectNameOf(row: SessionRowWire): string {
  const cwd = row.cwd?.trim();
  if (cwd) return basenameOf(cwd);
  return projectFromSessionPath(row.path);
}

/** Human-readable session title: explicit name > summary excerpt > project name.
 *  Never shows a raw UUID — falls back to the project name so the sidebar
 *  reads like ChatGPT's session list. */
export function sessionTitle(row: SessionRowWire): string {
  const name = row.name?.trim();
  if (name) return safeText(name);
  const summary = row.summary?.trim();
  if (summary) return safeText(summary.length > 60 ? `${summary.slice(0, 57)}…` : summary);
  return projectNameOf(row);
}

/** Build the `provider -> project? -> sessions` tree. Every row lands under
 *  one of the four provider groups (rpi/Codex/Grok/OMP). Sessions whose
 *  project name is structural noise (tmp, dot-tmp prefixes, bare UUIDs, or source) are listed
 *  directly under the provider without a distinct project subgroup, so the
 *  default sidebar never shows those as visible groups. Provider roots are
 *  ordered by `PROVIDER_ORDER`; rows within each node retain catalog order
 *  (the Rust side already sorts newest-first). */
export function groupSessions(rows: SessionRowWire[]): SessionGroupNode[] {
  const roots: SessionGroupNode[] = [];
  const rootIndex = new Map<string, SessionGroupNode>();
  const projectIndex = new Map<string, SessionGroupNode>();
  for (const row of rows) {
    const provider = providerOf(row);
    if (!provider) continue;
    const providerKey = `provider::${provider}`;
    let providerNode = rootIndex.get(providerKey);
    if (!providerNode) {
      providerNode = { key: providerKey, label: provider, kind: 'provider', sessions: [], children: [] };
      rootIndex.set(providerKey, providerNode);
      roots.push(providerNode);
    }
    const project = projectNameOf(row);
    if (isNoiseProjectName(project)) {
      providerNode.sessions.push(row);
      continue;
    }
    const projectKey = `${providerKey}::project::${project}`;
    let projectNode = projectIndex.get(projectKey);
    if (!projectNode) {
      projectNode = { key: projectKey, label: project, kind: 'project', sessions: [], children: [] };
      projectIndex.set(projectKey, projectNode);
      providerNode.children.push(projectNode);
    }
    projectNode.sessions.push(row);
  }
  roots.sort(
    (a, b) => PROVIDER_ORDER.indexOf(a.label as (typeof PROVIDER_ORDER)[number]) -
      PROVIDER_ORDER.indexOf(b.label as (typeof PROVIDER_ORDER)[number])
  );
  return roots;
}

/** True when a row matches the (already lower-cased) search query by session
 *  title, project/cwd basename, provider label, wire source, or session id.
 *  Exported for unit tests; the component keeps the active session visible
 *  even when this returns false. */
export function matchSessionSearch(row: SessionRowWire, q: string): boolean {
  if (!q) return true;
  const title = sessionTitle(row).toLowerCase();
  const project = projectNameOf(row).toLowerCase();
  const provider = (providerOf(row) ?? '').toLowerCase();
  const source = (row.source || '').toLowerCase();
  const sid = row.sessionId.toLowerCase();
  return title.includes(q) || project.includes(q) || provider.includes(q) || source.includes(q) || sid.includes(q);
}

/** Rows the sidebar shows for the current search query.
 *
 *  Temporary-workspace rows (the `temporary` wire flag: unnamed, tiny,
 *  native rpi sessions recorded under the OS temp root — the historical
 *  test-harness shape) are hidden by DEFAULT so historical pollution does
 *  not clutter the list, but they never leave the catalog:
 *  - an empty query keeps them only when the row is loaded or is the active
 *    session (active/loaded sessions never disappear),
 *  - a non-empty query includes them like any other row, so a legal temp
 *    session can be found by title, project, provider, source, or session id.
 *  Exported for unit tests.
 */
export function visibleSidebarSessions(
  knownSessions: SessionRowWire[],
  searchQuery: string,
  activeSessionId?: string | null,
): SessionRowWire[] {
  const q = searchQuery.trim().toLowerCase();
  if (!q) {
    return knownSessions.filter(
      (row) =>
        row.temporary !== true ||
        row.loaded === true ||
        row.sessionId === activeSessionId,
    );
  }
  const matched = knownSessions.filter((row) => matchSessionSearch(row, q));
  const activeRow = knownSessions.find((r) => r.sessionId === activeSessionId);
  if (activeRow && !matched.some((r) => r.sessionId === activeSessionId)) {
    return [...matched, activeRow];
  }
  return matched;
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
  /** Bounded wait for the current socket to reach OPEN. Auto/background loads
   *  (mount + poll) await this before `sendCommand` so a mount-before-
   *  WebSocket-OPEN surfaces no persistent `load failed: not connected`;
   *  active actions (New / Switch) bypass it and fail fast. */
  waitForReady: () => Promise<void>;
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
  /** Open the full Session panel (detail + rename + fork/clone). */
  onOpenManage: () => void;
  onSwitchComplete: () => void;
}

export function SessionSidebar({
  sendCommand,
  waitForReady,
  onLifecycleResult,
  activeSessionId,
  unreadBySessionId,
  featureNav,
  onReopenRail,
  onOpenManage,
  onSwitchComplete,
}: SessionSidebarProps) {
  const [sessions, setSessions] = useState<SessionRowWire[]>([]);
  const hasLoadedActiveRow = sessions.some(
    (row) => row.loaded === true && row.sessionId === activeSessionId,
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const mountedRef = useRef(true);
  /** Group keys (`provider::<label>` / `provider::<label>::project::<name>`)
   *  whose subtree is collapsed in the tree view. */
  const [collapsedGroups, setCollapsedGroups] = useState<Set<string>>(() => new Set());
  /** Bounded client-side search query. Empty restores the full catalog list. */
  const [searchQuery, setSearchQuery] = useState('');

  /** Sessions whose wire `source` maps to one of the four allowed providers.
   *  Rows with an unrecognised source are filtered out entirely — they never
   *  appear in the sidebar and never create a top-level group. */
  const knownSessions = useMemo(
    () => sessions.filter((row) => providerOf(row) !== null),
    [sessions]
  );

  /** Rows matching the current search query (bounded client-side filtering
   *  by session title, project/cwd basename, provider label, wire source,
   *  or session id). Temporary-workspace rows are hidden by default but stay
   *  searchable, and the active session is always kept visible even when it
   *  does not match the query, so the user never loses track of the running
   *  session while filtering. */
  const visibleSessions = useMemo(
    () => visibleSidebarSessions(knownSessions, searchQuery, activeSessionId),
    [knownSessions, searchQuery, activeSessionId]
  );

  const groups = useMemo(() => groupSessions(visibleSessions), [visibleSessions]);
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

  /** Re-fetch the session catalog. Auto/background callers (mount effect,
   *  poll) pass `waitForReady: true` (default) so a mount-before-WebSocket-
   *  OPEN waits bounded for the socket to open instead of surfacing a
   *  persistent `load failed: not connected`; a gate timeout or a transient
   *  post-gate `not connected` race is swallowed silently (the next poll
   *  retries) — never a permanent error. Active actions (run) pass
   *  `waitForReady: false` so a disconnect FAILS FAST on the action itself. */
  const load = useCallback(async (opts?: { waitForReady?: boolean }): Promise<void> => {
    const wait = opts?.waitForReady !== false;
    if (wait) {
      try {
        await waitForReady();
      } catch {
        return; // gate timeout during disconnect — silent; next poll retries
      }
    }
    try {
      const data = await sendCommand({ type: 'session_list', scope: 'all_projects' });
      if (!mountedRef.current) return;
      // Own RPC contract: session_list -> { sessions: RpcSessionListRow[] }.
      const list = data as { sessions?: SessionRowWire[] };
      setSessions(Array.isArray(list?.sessions) ? list.sessions : []);
      setError('');
    } catch (err) {
      // A post-gate `not connected` is a transient race (socket dropped between
      // gate-resolve and send); the next poll/reconnect retries — never a
      // persistent `load failed: not connected`. Other errors surface.
      if (wait && (err as Error).message === 'not connected') return;
      throw err;
    }
  }, [sendCommand, waitForReady]);

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
        // Fail-fast refresh (the action already used sendCommand directly): a
        // disconnect here is a transient race after a successful action — the
        // next poll re-lists — so swallow `not connected` instead of showing a
        // misleading post-action error. Other refresh errors surface.
        await load({ waitForReady: false }).catch((err: Error) => {
          if ((err as Error).message === 'not connected') return;
          throw err;
        });
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

      <div className="session-sidebar__search-box">
        <input
          id="session-sidebar-search"
          className="session-sidebar__search"
          type="search"
          maxLength={120}
          placeholder="Search sessions…"
          value={searchQuery}
          onChange={(e) => setSearchQuery(e.target.value)}
          aria-label="Search sessions by title, project, provider, or session id"
        />
        {searchQuery !== '' && (
          <button
            id="session-sidebar-search-clear"
            type="button"
            className="session-sidebar__search-clear"
            title="Clear search"
            aria-label="Clear search"
            onClick={() => setSearchQuery('')}
          >
            ✕
          </button>
        )}
      </div>

      <ul className="session-sidebar__list">
        {knownSessions.length === 0 && !error && (
          <li className="session-sidebar__empty">
            {sessions.length === 0 ? 'No saved sessions.' : 'No sessions match your search.'}
          </li>
        )}
        {treeItems.map((item) => {
          if (item.kind === 'group') {
            const collapsed = collapsedGroups.has(item.node.key);
            return (
              <li key={item.node.key} className="session-sidebar__group" data-group-kind={item.node.kind}>
                <button
                  type="button"
                  className="session-sidebar__group-head"
                  data-group-kind={item.node.kind}
                  data-provider={item.node.kind === 'provider' ? item.node.label : undefined}
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
          const isActive = isLoadedCurrentSession(row, activeSessionId, hasLoadedActiveRow);
          return (
            <li
              key={sessionRowKey(row)}
              className={isActive ? 'session-sidebar__row session-sidebar__row--active' : 'session-sidebar__row'}
              style={item.depth > 0 ? { paddingLeft: item.depth * 14 } : undefined}
            >
              <div className="session-sidebar__row-main">
              <button
                type="button"
                className="session-sidebar__switch"
                data-session-id={row.sessionId}
                data-session-source={row.source || 'pi'}
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
                {row.loaded === true && (unreadBySessionId?.[row.sessionId] ?? 0) > 0 && (
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
