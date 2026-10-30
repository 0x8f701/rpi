// Persistent session sidebar listing saved sessions from the control plane.
// Rows are grouped provider -> real project / Other / Agent sessions; native
// rows expose Copy context, export, fork, clone, rename, move, and delete.
// The saved-row Fork from picker pages automatically in a bounded scroller.
// Every catalog-derived string passes through safeText().

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { safeText } from '../redact';
import { isTransportError } from '../transportError';
import { copyTextToClipboard } from '../CopyButton';
import { PathAutocomplete, type PathCompleteRequest, type PathCompleteResponse } from '../PathAutocomplete';
import type { CatalogSource, CatalogStatus, RequestSessionSwitch, SessionSwitchTarget } from '../sessionSwitch';
import { catalogRuntimeSessionId } from '../sessionPreference';
import {
  isLiveSessionLoad,
  parseForkMessagesPage,
  mergeForkMessagePages,
  refreshSessionList,
  sendPathCompleteRequest,
  sessionRowKey,
  type ForkMessageWire,
  type SessionRowWire,
} from './SessionPanel';


/**
 * One node of the sidebar session tree: `provider -> subgroup -> sessions`.
 * Every row lands under one of five provider groups (rpi, Pi, Codex, Grok,
 * OMP), then under exactly one secondary subgroup: a real cwd project,
 * stable `Other` for structural-noise projects, or stable `Agent sessions`
 * for backend-classified agent/bootstrap rows.
 */
interface SessionGroupNode {
  key: string;
  label: string;
  provider: string;
  /** Display kind: `provider` for top-level source groups and `project` for
   * every secondary subgroup. `subgroupKind` distinguishes the three stable
   * secondary classifications without exposing them as provider-direct rows. */
  kind: 'provider' | 'project';
  subgroupKind?: 'project' | 'other' | 'agent';
  sessions: SessionRowWire[];
  children: SessionGroupNode[];
}

export type SessionTreeItem =
  | { kind: 'group'; node: SessionGroupNode; depth: number; sessionCount: number }
  | { kind: 'row'; row: SessionRowWire; depth: number };

/** Canonical provider display label for a catalog row, or `null` when the
 *  wire `source` is not one of the five allowed providers (rpi, Pi, Codex,
 *  Grok, OMP). Native aliases map to rpi, while Pi remains a distinct
 *  immutable import source. Grok and grok/hyper both map to Grok. Unknown,
 *  empty, or dirty sources return null — the row is filtered from the
 *  sidebar rather than silently misgrouped. This mapping is a membership
 *  allowlist only; sibling group order is first-seen in the reconciled catalog. */
export function providerOf(row: SessionRowWire): string | null {
  const source = row.source?.trim().toLowerCase() || '';
  if (source === 'rpi' || source === 'native' || source === 'primary') return 'rpi';
  if (source === 'pi') return 'Pi';
  if (source === 'codex') return 'Codex';
  if (source === 'grok' || source === 'grok/hyper') return 'Grok';
  if (source === 'omp') return 'OMP';
  return null;
}

export function shouldToastSessionActionError(error: unknown): boolean {
  if (!error || typeof error !== 'object' || !("toastOwned" in error)) return true;
  return error.toastOwned !== true;
}

/** Directory names that are structural noise, not real project labels. Such
 *  rows remain visible/searchable under the stable `Other` subgroup; they
 *  are never rendered directly beneath a provider. */
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

/** Human-readable session title: explicit name > summary excerpt.
 *  Never echoes the project/cwd basename — the subgroup already names the
 *  project, so unnamed empty-summary rows stay "Untitled session". */
export function sessionTitle(row: SessionRowWire): string {
  const name = row.name?.trim();
  if (name) return safeText(name);
  const summary = row.summary?.trim();
  if (summary) return safeText(summary.length > 60 ? `${summary.slice(0, 57)}…` : summary);
  return 'Untitled session';
}

/** Compact sidebar meta: displayTime and `<N> msgs`, with a separator only when both exist. */
export function sessionRowMeta(row: Pick<SessionRowWire, 'displayTime' | 'messageCount'>): string {
  const time = row.displayTime ? safeText(row.displayTime) : '';
  const msgs = row.messageCount != null ? `${row.messageCount} msgs` : '';
  if (time && msgs) return `${time} · ${msgs}`;
  return time || msgs;
}

const OTHER_SUBGROUP_LABEL = 'Other';
const AGENT_SUBGROUP_LABEL = 'Agent sessions';

interface SessionSubgroup {
  key: string;
  label: string;
  kind: 'project' | 'other' | 'agent';
}

/** Stable secondary classification for one provider row. Backend agent flags
 *  take precedence over cwd shape; structural project names share `Other`;
 *  every other authoritative cwd basename remains a real project label
 *  (including a literal `Projects`). */
function subgroupOf(row: SessionRowWire, provider: string): SessionSubgroup {
  const providerKey = `provider::${provider}`;
  if (row.agentSession === true || row.bootstrapOnly === true) {
    return { key: `${providerKey}::special::agent-sessions`, label: AGENT_SUBGROUP_LABEL, kind: 'agent' };
  }
  const project = projectNameOf(row);
  if (isNoiseProjectName(project)) {
    return { key: `${providerKey}::special::other`, label: OTHER_SUBGROUP_LABEL, kind: 'other' };
  }
  return { key: `${providerKey}::project::${project}`, label: project, kind: 'project' };
}

/** Build the `provider -> subgroup -> sessions` tree as a count/identity
 * index for headers and collapse keys. Roots, subgroups, and rows stay in
 * first-seen reconciled-catalog order; every row has an explicit subgroup. */
export function groupSessions(rows: SessionRowWire[]): SessionGroupNode[] {
  const roots: SessionGroupNode[] = [];
  const rootIndex = new Map<string, SessionGroupNode>();
  const subgroupIndex = new Map<string, SessionGroupNode>();
  for (const row of rows) {
    const provider = providerOf(row);
    if (!provider) continue;
    const providerKey = `provider::${provider}`;
    let providerNode = rootIndex.get(providerKey);
    if (!providerNode) {
      providerNode = {
        key: providerKey,
        label: provider,
        provider,
        kind: 'provider',
        sessions: [],
        children: [],
      };
      rootIndex.set(providerKey, providerNode);
      roots.push(providerNode);
    }
    const subgroup = subgroupOf(row, provider);
    let subgroupNode = subgroupIndex.get(subgroup.key);
    if (!subgroupNode) {
      subgroupNode = {
        key: subgroup.key,
        label: subgroup.label,
        provider,
        kind: 'project',
        subgroupKind: subgroup.kind,
        sessions: [],
        children: [],
      };
      subgroupIndex.set(subgroup.key, subgroupNode);
      providerNode.children.push(subgroupNode);
    }
    subgroupNode.sessions.push(row);
  }
  return roots;
}

function indexGroupNodes(
  nodes: SessionGroupNode[],
  into: Map<string, SessionGroupNode> = new Map(),
): Map<string, SessionGroupNode> {
  for (const node of nodes) {
    into.set(node.key, node);
    indexGroupNodes(node.children, into);
  }
  return into;
}

/** Resolve the active sidebar identity from `activeSessionId`. Duplicate
 *  `sessionId` values (foreign + imported native) are distinct
 *  `sessionRowKey`s; a non-`already_imported` match wins over an alias for
 *  the same runtime, and a loaded overlay wins within that class. */
export function resolveActiveSessionRow(
  rows: SessionRowWire[],
  activeSessionId?: string | null,
): SessionRowWire | undefined {
  if (!activeSessionId) return undefined;
  let loadedDirect: SessionRowWire | undefined;
  let loadedImported: SessionRowWire | undefined;
  let direct: SessionRowWire | undefined;
  let imported: SessionRowWire | undefined;
  for (const row of rows) {
    if (row.sessionId !== activeSessionId && catalogRuntimeSessionId(row) !== activeSessionId) continue;
    if (row.status === 'already_imported') {
      if (row.loaded === true) loadedImported ??= row;
      else imported ??= row;
      continue;
    }
    if (row.loaded === true) loadedDirect ??= row;
    else direct ??= row;
  }
  return loadedDirect ?? loadedImported ?? direct ?? imported;
}


/** True when a row matches the (already lower-cased) search query by session
 * title, subgroup/project, provider label, wire source, or session id. */
export function matchSessionSearch(row: SessionRowWire, q: string): boolean {
  if (!q) return true;
  const provider = providerOf(row);
  const title = sessionTitle(row).toLowerCase();
  const project = projectNameOf(row).toLowerCase();
  const subgroup = provider ? subgroupOf(row, provider).label.toLowerCase() : '';
  const source = (row.source || '').toLowerCase();
  const sid = row.sessionId.toLowerCase();
  return title.includes(q) || project.includes(q) || subgroup.includes(q)
    || (provider ?? '').toLowerCase().includes(q) || source.includes(q) || sid.includes(q);
}

/** Rows the sidebar shows for the current search query. Temporary-workspace
 * and backend-classified bootstrap-only rows are hidden by default but never
 * removed from the catalog: search restores matches, and active/loaded rows
 * are always retained. Bootstrap rows are classified under `Agent sessions`
 * when restored. */
export function visibleSidebarSessions(
  knownSessions: SessionRowWire[],
  searchQuery: string,
  activeSessionId?: string | null,
): SessionRowWire[] {
  const activeRow = resolveActiveSessionRow(knownSessions, activeSessionId);
  const activeKey = activeRow ? sessionRowKey(activeRow) : null;
  const q = searchQuery.trim().toLowerCase();
  if (!q) {
    return knownSessions.filter((row) =>
      (row.temporary !== true && row.bootstrapOnly !== true)
      || row.loaded === true
      || (activeKey !== null && sessionRowKey(row) === activeKey));
  }
  const matched = knownSessions.filter((row) => matchSessionSearch(row, q));
  if (activeRow && activeKey && !matched.some((row) => sessionRowKey(row) === activeKey)) {
    return [...matched, activeRow];
  }
  return matched;
}

/** Merge the App-validated lifecycle row overlay into the rendered catalog.
 *
 *  The overlay is a SEPARATE channel — it never enters the private catalog
 *  state — so a failed or delayed `session_list` always retains it (the
 *  catalog array is returned untouched by reference on any error), and it
 *  is absorbed only when a catalog response contains the EXACT
 *  `source::sessionId::path` identity (same rule reconcileSessionRowOrder
 *  keys rows by). The overlay row appends at the tail, matching
 *  reconcileSessionRowOrder's genuinely-new-row semantics, so absorption
 *  never moves the row. No duplicate is possible: once the catalog carries
 *  the identity the overlay contributes nothing.
 */
export function mergeLifecycleOverlayRow(
  catalog: SessionRowWire[],
  overlay: SessionRowWire | null | undefined,
): SessionRowWire[] {
  if (!overlay) return catalog;
  const overlayKey = sessionRowKey(overlay);
  if (catalog.some((row) => sessionRowKey(row) === overlayKey)) return catalog;
  return [...catalog, overlay];
}

/** Stable catalog identity for order reconciliation. Native source aliases
 *  (`rpi` / `native` / `primary` / empty) and `grok` / `grok/hyper` share a
 *  provider family, so a loaded overlay or persist that only retags the
 *  wire `source` cannot drop a retained row and re-append it in server
 *  order. Distinct providers (foreign vs native) stay distinct keys. */
function sessionReconcileKey(row: SessionRowWire): string {
  const source = providerOf(row) ?? (row.source?.trim() || 'rpi');
  return `${source}::${row.sessionId}::${row.path}`;
}


/** Reconcile `session_list` refresh results into one authoritative row order.
 *
 * The backend sorts newest-first, and selecting a persisted session can
 * advance its modification time; blindly replacing the array would make the
 * clicked row jump. Rows match by `sessionReconcileKey` — provider family +
 * `sessionId` + `path` — so a retained identity keeps its index no matter
 * how the server re-sorts or how its metadata (mtime, activity, name,
 * loaded overlay, unread) or native source alias (`rpi`/`native`/`primary`)
 * changes. Removed identities disappear; genuinely new identities append
 * in incoming catalog order. Repeated loads (StrictMode double effects,
 * reconnect re-lists) are idempotent: the same catalog reconciles to the
 * same order and rows. */
export function reconcileSessionRowOrder(
  previous: SessionRowWire[],
  incoming: SessionRowWire[],
): SessionRowWire[] {
  const nextByKey = new Map(incoming.map((row) => [sessionReconcileKey(row), row]));
  const reconciled: SessionRowWire[] = [];
  for (const row of previous) {
    const key = sessionReconcileKey(row);
    const updated = nextByKey.get(key);
    if (!updated) continue;
    reconciled.push(updated);
    nextByKey.delete(key);
  }
  for (const row of incoming) {
    if (nextByKey.delete(sessionReconcileKey(row))) reconciled.push(row);
  }
  return reconciled;
}

function subtreeSessionCount(node: SessionGroupNode): number {
  let count = node.sessions.length;
  for (const child of node.children) count += subtreeSessionCount(child);
  return count;
}

/** Migrate explicit collapse/expand intent when an authoritative refresh
 * reclassifies a retained row from a path-derived project to a cwd-derived
 * project. Identity is provider + sessionId: the stable catalog owner that
 * survives authoritative path/cwd correction. Ambiguous duplicate owners do
 * not migrate. Stable Other/Agent keys need no migration. A transferred key
 * is removed once its old subgroup disappears (no incoming row classifies
 * under it) and retained while a sibling row still holds the subgroup, so
 * repeated A→B→A drift never resurrects a stale intent. */
export function migrateSidebarCollapseIntent(
  previousRows: SessionRowWire[],
  incomingRows: SessionRowWire[],
  intent: ReadonlyMap<string, boolean>,
): Map<string, boolean> {
  if (intent.size === 0 || previousRows.length === 0 || incomingRows.length === 0) return new Map(intent);
  const incomingByOwner = new Map<string, SessionRowWire[]>();
  const incomingSubgroupKeys = new Set<string>();
  for (const row of incomingRows) {
    const provider = providerOf(row);
    if (!provider) continue;
    const key = `${provider}::${row.sessionId}`;
    const matches = incomingByOwner.get(key) ?? [];
    matches.push(row);
    incomingByOwner.set(key, matches);
    incomingSubgroupKeys.add(subgroupOf(row, provider).key);
  }
  const next = new Map(intent);
  for (const previous of previousRows) {
    const provider = providerOf(previous);
    if (!provider) continue;
    const candidates = incomingByOwner.get(`${provider}::${previous.sessionId}`) ?? [];
    if (candidates.length !== 1) continue;
    const oldKey = subgroupOf(previous, provider).key;
    const newKey = subgroupOf(candidates[0], provider).key;
    if (oldKey === newKey || !intent.has(oldKey)) continue;
    const oldIntent = intent.get(oldKey) === true;
    if (!next.has(newKey) || oldIntent) next.set(newKey, oldIntent);
    // The transferred key is obsolete ONLY when the old subgroup disappeared:
    // no incoming row classifies under it any more, so keeping it would let a
    // stale collapsed/expanded value resurrect on a later A-B-A drift back.
    // When a sibling row still holds the old subgroup, the intent stays.
    if (!incomingSubgroupKeys.has(oldKey)) next.delete(oldKey);
  }
  return next;
}

/** Resolve effective collapsed keys. One rule for every node: an explicit
 *  `true` intent collapses it — including groups that hold the active or a
 *  loaded session. Agent sessions subgroups default collapsed and open
 *  on explicit `false`; an active search opens matching Agent subgroups for
 *  visibility. A provider collapse remains absolute above its subgroups. */
export function collapsedSidebarGroupKeys(
  rows: SessionRowWire[],
  intent: ReadonlyMap<string, boolean>,
  searchActive = false,
): Set<string> {
  const collapsed = new Set<string>();
  const groups = groupSessions(rows);
  for (const providerNode of groups) {
    if (intent.get(providerNode.key) === true) collapsed.add(providerNode.key);
    for (const subgroup of providerNode.children) {
      if (subgroup.subgroupKind !== 'agent') {
        if (intent.get(subgroup.key) === true) collapsed.add(subgroup.key);
        continue;
      }
      if (intent.get(subgroup.key) === true) {
        collapsed.add(subgroup.key);
      } else if (searchActive && intent.get(subgroup.key) !== true) {
        collapsed.delete(subgroup.key);
      } else if (intent.get(subgroup.key) !== false) {
        collapsed.add(subgroup.key);
      }
    }
  }
  return collapsed;
}

/** Flatten catalog rows into the exact `<li>` sequence the sidebar renders.
 * Group headers emit on first occurrence while rows retain reconciled order.
 * A collapsed group hides every descendant, including the active session. */
export function flattenTree(
  rows: SessionRowWire[],
  collapsed: Set<string>,
  out: SessionTreeItem[],
  groups: SessionGroupNode[] = groupSessions(rows),
): void {
  const byKey = indexGroupNodes(groups);
  const emitted = new Set<string>();
  for (const row of rows) {
    const provider = providerOf(row);
    if (!provider) continue;
    const providerKey = `provider::${provider}`;
    const providerNode = byKey.get(providerKey);
    if (!providerNode) continue;
    if (!emitted.has(providerKey)) {
      out.push({ kind: 'group', node: providerNode, depth: 0, sessionCount: subtreeSessionCount(providerNode) });
      emitted.add(providerKey);
    }
    if (collapsed.has(providerKey)) continue;
    const subgroup = subgroupOf(row, provider);
    const subgroupNode = byKey.get(subgroup.key);
    if (subgroupNode && !emitted.has(subgroup.key)) {
      out.push({ kind: 'group', node: subgroupNode, depth: 1, sessionCount: subtreeSessionCount(subgroupNode) });
      emitted.add(subgroup.key);
    }
    if (collapsed.has(subgroup.key)) continue;
    out.push({ kind: 'row', row, depth: subgroupNode ? 2 : 1 });
  }
}


export const SESSION_MENU_LONG_PRESS_MS = 500;
export const SESSION_ARCHIVE_UNDO_MS = 5000;
export const DELETE_SWITCH_AWAY_TITLE = 'Switch away from this session first';
/** Title for an ENABLED Move session… item: the action relocates the saved
 *  session file under another project directory and rewrites the moved
 *  session's recorded cwd to the target. When the row IS the active
 *  session, the backend rebinds that same active runtime to the
 *  destination (idle only — a busy turn is refused fail-closed); it never
 *  auto-switches sessions. */
export const MOVE_SESSION_TITLE =
  'Move this saved session under another project directory; the moved session’s cwd becomes the target (the active session stays active)';

const MENU_ESTIMATE_WIDTH = 220;
const MENU_ESTIMATE_HEIGHT = 288;


export type SessionMenuItemId =
  | 'copy-context'
  | 'export-html'
  | 'fork-latest'
  | 'fork-from'
  | 'clone'
  | 'rename'
  | 'move'
  | 'delete'
  | 'delete-confirm'
  | 'delete-cancel';

export interface SessionMenuItem {
  id: SessionMenuItemId;
  label: string;
  disabled?: boolean;
  danger?: boolean;
  title?: string;
}

/** Exact canonical catalog source tokens: the strict set the backend switch
 *  protocol and the Web control plane share. Historic wire aliases
 *  (`native`, `primary`, `grok/hyper`) are NOT canonical — see below. */
const CANONICAL_CATALOG_SOURCES: Readonly<Record<string, CatalogSource | undefined>> = {
  rpi: 'rpi',
  pi: 'pi',
  omp: 'omp',
  codex: 'codex',
  grok: 'grok',
};

/** Canonical catalog identity of one wire row. BOTH fields must validate
 *  against the EXACT RAW canonical tokens: `source` must be byte-for-byte one
 *  of the five canonical providers (`rpi`, `pi`, `omp`, `codex`, `grok`) and
 *  `status` must be the exact token `native` or `foreign`. No trim, no case
 *  folding: a dirty source (`RPI`, ` pi `) is a different token and is
 *  MALFORMED. Legacy wire aliases (`native`, `primary`, `grok/hyper`),
 *  empty/unknown/dirty sources, and missing or unknown statuses are
 *  MALFORMED — no implicit native default, no alias mapping: a malformed row
 *  carries no native/menu/switch authority. */
export function catalogIdentityOf(
  row: Pick<SessionRowWire, 'source' | 'status'>,
): { source: CatalogSource; status: CatalogStatus } | null {
  const canonical = CANONICAL_CATALOG_SOURCES[row.source ?? ''];
  if (canonical === undefined) return null;
  if (row.status !== 'native' && row.status !== 'foreign') return null;
  return { source: canonical, status: row.status };
}

/** Only the exact native rpi row may expose in-place mutation actions. */
export function isNativeSession(row: Pick<SessionRowWire, 'source' | 'status'>): boolean {
  return row.source === 'rpi' && row.status === 'native';
}


/** Mutation eligibility for the native-only Delete menu action: only the
 *  ACTIVE session is disabled (explicit switch-away guidance). A non-active
 *  row is eligible whether loaded (idle secondary runtime) or unloaded — a
 *  busy loaded row is refused by the backend with one bounded error and no
 *  catalog mutation. Move is NOT gated here: moving the active row is
 *  enabled and the backend rebinds the idle active runtime's cwd. */
export function isSessionActionDisabled(isActive: boolean): boolean {
  return isActive;
}
/** Only a byte-exact canonical catalog row owns a context menu. */
export function hasSessionContextMenu(row: Pick<SessionRowWire, 'source' | 'status'>): boolean {
  return catalogIdentityOf(row) !== null;
}

export function contextMenuItems(opts: {
  native: boolean;
  deleteDisabled: boolean;
  confirmingDelete?: boolean;
  exportDisabled?: boolean;
  copyContextDisabled?: boolean;
  moveDisabled?: boolean;
}): SessionMenuItem[] {
  if (opts.confirmingDelete) {
    // The CONFIRM button honors deleteDisabled too (P2): a pending switch
    // gates confirmDelete's runRefresh, so the control must not stay enabled.
    return [
      { id: 'delete-confirm', label: 'Confirm delete', danger: true, disabled: opts.deleteDisabled === true },
      { id: 'delete-cancel', label: 'Cancel' },
    ];
  }
  const exportItem: SessionMenuItem = {
    id: 'export-html',
    label: 'Export HTML',
    disabled: opts.exportDisabled === true,
  };
  if (!opts.native) return [exportItem];
  return [
    {
      id: 'copy-context',
      label: 'Copy context',
      disabled: opts.copyContextDisabled === true,
    },
    exportItem,
    { id: 'fork-latest', label: 'Fork latest' },
    { id: 'fork-from', label: 'Fork from…' },
    { id: 'clone', label: 'Clone' },
    { id: 'rename', label: 'Rename' },
    {
      id: 'move',
      label: 'Move session…',
      title: MOVE_SESSION_TITLE,
      disabled: opts.moveDisabled === true,
    },
    {
      id: 'delete',
      label: 'Delete',
      danger: true,
      disabled: opts.deleteDisabled,
      title: opts.deleteDisabled ? DELETE_SWITCH_AWAY_TITLE : undefined,
    },
  ];
}

/** Flip then clamp a fixed-position menu into the viewport. */
export function flipMenuPosition(
  x: number,
  y: number,
  width: number,
  height: number,
  viewportWidth: number,
  viewportHeight: number,
  margin = 8,
): { left: number; top: number } {
  let left = x;
  let top = y;
  if (left + width > viewportWidth - margin) left = x - width;
  if (top + height > viewportHeight - margin) top = y - height;
  const maxLeft = Math.max(margin, viewportWidth - width - margin);
  const maxTop = Math.max(margin, viewportHeight - height - margin);
  left = Math.min(Math.max(left, margin), maxLeft);
  top = Math.min(Math.max(top, margin), maxTop);
  return { left, top };
}

export function nextMenuFocus(
  current: number,
  direction: 1 | -1,
  count: number,
  disabled?: readonly boolean[],
): number {
  if (count <= 0) return -1;
  const start = current < 0
    ? (direction === 1 ? 0 : count - 1)
    : (current + direction + count) % count;
  for (let i = 0; i < count; i += 1) {
    const idx = (start + i * direction + count * 2) % count;
    if (!disabled?.[idx]) return idx;
  }
  return current < 0 ? 0 : current;
}

export function firstEnabledMenuFocus(disabled: readonly boolean[]): number {
  const idx = disabled.findIndex((d) => !d);
  return idx < 0 ? 0 : idx;
}

export function shouldOpenLongPress(
  elapsedMs: number,
  thresholdMs = SESSION_MENU_LONG_PRESS_MS,
): boolean {
  return elapsedMs >= thresholdMs;
}

export function forkToastMessage(name: string): string {
  return `Forked from ${name}`;
}

export function sessionMoveToast(): string {
  return 'Session moved';
}

/** Submit one bounded `session_move` wire command for the captured row path.
 *  The manually entered target directory is trimmed; an empty or non-absolute
 *  target — relative, drive-relative (`C:foo`, `C:`), share-less
 *  (`\\server`), or a malformed strict-UNC double — is rejected locally
 *  before any send so the wire never sees a blank or ambiguous move — the
 *  picker demands an explicit absolute target directory in any absolute
 *  shape the platform understands (POSIX, Windows drive-root, strict UNC).
 *  The raw trimmed target is preserved on the wire. Errors bubble to the
 *  caller's bounded error channel and never echo local paths. */
export async function moveSession(opts: {
  sessionPath: string;
  targetCwd: string;
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>;
}): Promise<unknown> {
  const targetCwd = opts.targetCwd.trim();
  if (!targetCwd) throw new Error('target directory is required');
  if (normalizeLexicalAbsoluteCwd(targetCwd) === null) {
    throw new Error('target directory must be an absolute path');
  }
  return opts.sendCommand({
    type: 'session_move',
    sessionPath: opts.sessionPath,
    targetCwd,
  });
}

/** Parsed destination half of a successful `session_move` response: the
 *  bounded move outcome plus the `session_list`-shaped destination row the
 *  backend scanned from the native sessions store. The parsed destination
 *  is the regroup contract: BOTH the top-level and row sessionId must equal
 *  the menu-captured row's sessionId EXACTLY as captured (raw strings, never
 *  trimmed), the outcome `from` must equal the menu-captured source row
 *  path EXACTLY as captured (the caller-bound proof the response is about
 *  THIS move, not some other session), the row must be a CANONICAL native
 *  row (source field EXACTLY 'rpi' and status field EXACTLY 'native' —
 *  validated by the shared catalogIdentityOf authority: no trim, no case
 *  folding, no legacy aliases, no empty-source default), its cwd must equal
 *  the explicitly requested target directory lexically and
 *  platform-neutrally: both sides normalize through the shared absolute
 *  normalizer (POSIX, Windows drive-root, strict UNC; trimmed,
 *  trailing-separator-insensitive, `.`/`..` collapsed string-only — never
 *  canonicalized — the ONLY normalized comparison here; id/from/to/path are
 *  compared RAW), the row path must be a nonempty raw string, and the
 *  outcome's `to` must equal the row's path exactly as received. The row's
 *  cwd is echoed UNTOUCHED as `cwdRaw` — the trim and lexical normalization
 *  exist only inside the local comparison, never on the echoed value.
 *  `moved` is
 *  the backend's truthful no-op marker: false means the session already
 *  resided in the target directory's session store (idempotent retry), true
 *  means the file actually moved (or an identical retry was replayed) — BOTH
 *  are successful outcomes, never a failure. Then — and only then — the caller
 *  can trust the row to appear under the target project after the single
 *  catalog refresh. */
export interface SessionMoveDestination {
  moved: boolean;
  sessionId: string;
  from: string;
  to: string;
  source: string;
  /** The destination row's cwd EXACTLY as received on the wire — never
   *  trimmed or normalized. Trim and lexical normalization exist only
   *  inside the local cwd/target comparison. */
  cwdRaw: string;
  path: string;
}

/** Patch the local catalog with a validated move destination: the row
 *  matching the destination's sessionId AND source path (`from`) is replaced
 *  in place with the destination identity (cwd = the echoed raw target,
 *  path = the moved file), so the sidebar regroups under the target project
 *  WITHOUT waiting for a refetch — the immediate post-move session_list can
 *  race the backend's catalog rescan and return pre-move rows. Rows that do
 *  not match are untouched; a catalog without the moved row is returned
 *  unchanged (the refetch/poll owns it). */
export function applySessionMoveToRows(
  rows: SessionRowWire[],
  destination: SessionMoveDestination,
): SessionRowWire[] {
  let patched = false;
  const next = rows.map((row) => {
    if (row.sessionId !== destination.sessionId || row.path !== destination.from) return row;
    patched = true;
    return { ...row, cwd: destination.cwdRaw, path: destination.path };
  });
  return patched ? next : rows;
}

/** Caller-bound expectations for a move response, taken from the
 *  menu-captured row and the typed picker — never from the wire. All three
 *  are RAW EXACT captures: never trimmed. */
export interface SessionMoveExpectations {
  expectedSessionId: string;
  expectedSessionPath: string;
  expectedTargetCwd: string;
}

/** Platform-neutral lexical absolute cwd normalizer: trim, drop trailing
 *  separators, and collapse `.`/`..` segments as pure string arithmetic —
 *  POSIX `/…`, Windows drive-root `C:\…`/`C:/…` (both separators, mixed
 *  freely), and STRICT UNC `\\server\share\…` (exactly two leading
 *  separators, nonempty server and share, no doubled or trailing structural
 *  separator — the same strictness as isAbsoluteProjectCwd in
 *  chat/mapping.ts). NEVER a filesystem canonicalization (no realpath, no
 *  symlink resolution, no existence check). Returns the normalized value —
 *  POSIX `/a/b`, drive `X:/a/b` with an uppercased drive letter and forward
 *  separators, UNC `//server/share/a/b` — or null when the path is empty,
 *  relative, drive-relative (`C:foo`, `C:`), share-less (`\\server`), a
 *  malformed strict-UNC double (triple-leading, doubled or trailing
 *  structural separator), or its `..` segments escape above the root/share;
 *  a null on either side of the comparison is a rejection. */
function normalizeLexicalAbsoluteCwd(path: string): string | null {
  const trimmed = path.trim();
  const popInto = (segments: string[], segment: string): boolean => {
    if (segment === '..') {
      if (segments.length === 0) return false;
      segments.pop();
    } else {
      segments.push(segment);
    }
    return true;
  };
  // POSIX: one leading `/`; interior/trailing doubles collapse (the long
  // standing lexical tolerance), and `..` past the root rejects.
  if (trimmed.startsWith('/')) {
    const segments: string[] = [];
    for (const segment of trimmed.split('/')) {
      if (segment === '' || segment === '.') continue;
      if (!popInto(segments, segment)) return null;
    }
    return `/${segments.join('/')}`;
  }
  // Windows drive-root: the drive letter must be rooted (`C:\`, `C:/`);
  // drive-relative shapes (`C:foo`, bare `C:`) never normalize.
  const drive = /^([A-Za-z]):/.exec(trimmed);
  if (drive) {
    const rest = trimmed.slice(2);
    if (!/^[\\/]/.test(rest)) return null;
    const segments: string[] = [];
    for (const segment of rest.split(/[\\/]/)) {
      if (segment === '' || segment === '.') continue;
      if (!popInto(segments, segment)) return null;
    }
    return `${drive[1].toUpperCase()}:/` + segments.join('/');
  }
  // STRICT UNC: exactly two leading separators, nonempty server and share,
  // and no doubled (empty) or trailing structural separator anywhere; `..`
  // is refused above the share root.
  if (trimmed.startsWith('\\\\')) {
    const segments = trimmed.slice(2).split(/[\\/]/);
    if (segments.length < 2 || segments.some((segment) => segment === '')) return null;
    const [server, share, ...tail] = segments;
    if (server === '.' || server === '..' || share === '.' || share === '..') return null;
    const path: string[] = [];
    for (const segment of tail) {
      if (segment === '.') continue;
      if (!popInto(path, segment)) return null;
    }
    return `//${server}/${share}` + (path.length === 0 ? '' : `/${path.join('/')}`);
  }
  return null;
}

export function parseSessionMoveResponse(result: unknown, expected: SessionMoveExpectations): SessionMoveDestination {
  // Every captured expectation is RAW EXACT authority: a nonempty string
  // exactly as captured from the menu row — never trimmed. A padded capture
  // can only match a wire value padded the exact same way.
  const expectedSessionId = typeof expected?.expectedSessionId === 'string' ? expected.expectedSessionId : '';
  const expectedSessionPath = typeof expected?.expectedSessionPath === 'string' ? expected.expectedSessionPath : '';
  if (expectedSessionId === '' || expectedSessionPath === '') throw new Error('session move response is invalid');
  if (!result || typeof result !== 'object') throw new Error('session move response is invalid');
  const record = result as Record<string, unknown>;
  // The backend's truthful outcome marker: true = the file moved (or an
  // identical retry was replayed), false = the session already resided in
  // the target directory's session store (idempotent no-op). BOTH are
  // successful — a successful backend no-op is never reported as a
  // failure — and both carry the same validated destination tuple.
  const moved = record.moved === true ? true : record.moved === false ? false : null;
  if (moved === null) throw new Error('session move response is invalid');
  const session = record.session;
  if (!session || typeof session !== 'object') throw new Error('session move response is invalid');
  const row = session as Record<string, unknown>;
  const sessionId = typeof row.sessionId === 'string' ? row.sessionId : '';
  const path = typeof row.path === 'string' ? row.path : '';
  const cwdRaw = typeof row.cwd === 'string' ? row.cwd : '';
  const cwdTrimmed = cwdRaw.trim();
  if (sessionId === '' || path === '' || cwdTrimmed === '') throw new Error('session move response is invalid');
  // All three id/path sites must bind to the menu-captured row EXACTLY as
  // captured — the top-level outcome sessionId AND the destination row's
  // sessionId AND the outcome `from` against the captured source row path,
  // each compared as raw strings with no trim. The `from` capture is the
  // caller-bound proof the response is about THIS move of THIS row.
  const outcomeSessionId = typeof record.sessionId === 'string' ? record.sessionId : '';
  const from = typeof record.from === 'string' ? record.from : '';
  if (outcomeSessionId !== expectedSessionId || sessionId !== expectedSessionId) {
    throw new Error('session move response is invalid');
  }
  if (from === '' || from !== expectedSessionPath) {
    throw new Error('session move response is invalid');
  }
  // The destination row must be a CANONICAL native row, validated by the
  // shared catalogIdentityOf authority: the source field must be the EXACT
  // raw token 'rpi' and the status field the EXACT token 'native' — no
  // trim, no case folding, no legacy aliases ('native', 'primary'), no
  // empty-source default, and no implicit native fallback. A dirty or
  // malformed row carries no move authority.
  const identity = catalogIdentityOf({
    source: typeof row.source === 'string' ? row.source : undefined,
    status: typeof row.status === 'string' ? row.status : undefined,
  });
  if (identity?.source !== 'rpi' || identity.status !== 'native') {
    throw new Error('session move response is invalid');
  }
  // The row cwd must equal the explicit target lexically, platform-neutrally:
  // both sides normalize through the shared lexical absolute normalizer
  // (POSIX, Windows drive-root with both separators, strict UNC; trim,
  // trailing separators, and `.`/`..` collapsed as pure string arithmetic on
  // both sides) — never a filesystem canonicalization, and a relative,
  // drive-relative, share-less, malformed-double, or root-escaping side
  // rejects. A canonical guess here could silently accept a moved-elsewhere
  // row. The trim and normalization are LOCAL to this comparison only; the
  // raw row cwd is echoed untouched as `cwdRaw`.
  const rowCwdLexical = normalizeLexicalAbsoluteCwd(cwdTrimmed);
  const targetCwdLexical = normalizeLexicalAbsoluteCwd(expected.expectedTargetCwd);
  if (rowCwdLexical === null || targetCwdLexical === null || rowCwdLexical !== targetCwdLexical) {
    throw new Error('session move response is invalid');
  }
  // The moved file (`to`) must be the row path EXACTLY as received — raw
  // string equality, no trim on either side.
  const to = typeof record.to === 'string' ? record.to : '';
  if (to !== path) throw new Error('session move response is invalid');
  return {
    moved,
    sessionId,
    from,
    to,
    source: identity.source,
    cwdRaw,
    path,
  };
}

/** Validated success payload of `session_delete`: `{ archived: true,
 *  sessionId, undoToken }` where the returned sessionId EXACTLY matches the
 *  menu-captured row and the undo token is nonempty (a usable 5s undo).
 *  A malformed success is a caller-visible failure (exactly one bounded
 *  error toast; no archived toast, no menu close). */
export interface SessionDeleteOutcome {
  sessionId: string;
  undoToken: string;
}

export function parseSessionDeleteResponse(result: unknown, expectedSessionId: string): SessionDeleteOutcome {
  const expectedId = expectedSessionId.trim();
  if (!expectedId) throw new Error('session delete response is invalid');
  if (!result || typeof result !== 'object') throw new Error('session delete response is invalid');
  const record = result as Record<string, unknown>;
  if (record.archived !== true) throw new Error('session delete response is invalid');
  const sessionId = typeof record.sessionId === 'string' ? record.sessionId.trim() : '';
  const undoToken = typeof record.undoToken === 'string' ? record.undoToken.trim() : '';
  if (!sessionId || sessionId !== expectedId || !undoToken) {
    throw new Error('session delete response is invalid');
  }
  return { sessionId, undoToken };
}


export const SESSION_COPY_CONTEXT_FORMAT = 'pi-session-context/1';
export const SESSION_COPY_CONTEXT_MAX_BYTES = 65_536;

/** Validate a canonical bounded `session_copy_context` RPC payload before any
 * clipboard write. Extra response object keys are ignored; text headers are
 * fixed at the first three lines and may not recur later. */
export function parseSessionCopyContextResponse(response: unknown): { text: string; truncated: boolean } {
  if (!response || typeof response !== 'object'
    || !('text' in response) || !('format' in response) || !('truncated' in response)) {
    throw new Error('session copy context response is invalid');
  }
  const text = response.text;
  const format = response.format;
  const truncated = response.truncated;
  if (typeof text !== 'string' || format !== SESSION_COPY_CONTEXT_FORMAT || typeof truncated !== 'boolean'
    || new TextEncoder().encode(text).byteLength > SESSION_COPY_CONTEXT_MAX_BYTES) {
    throw new Error('session copy context response is invalid');
  }
  const lines = text.split(/\r?\n/);
  const expectedTruncated = truncated ? 'yes' : 'no';
  if (lines[0] !== '# Pi session context'
    || lines[1] !== `Format: ${SESSION_COPY_CONTEXT_FORMAT}`
    || lines[2] !== `Truncated: ${expectedTruncated}`
    || lines.slice(3).some((line) => line.startsWith('Format:') || line.startsWith('Truncated:'))) {
    throw new Error('session copy context response is invalid');
  }
  return { text, truncated };
}

export const FORK_SCROLL_LOAD_THRESHOLD_PX = 48;

/** Scroll-only paging trigger for the saved-session fork picker. */
export function shouldLoadOlderForkFromScroll(
  scrollTop: number,
  clientHeight: number,
  scrollHeight: number,
  hasNextCursor: boolean,
  loading: boolean,
  failed: boolean,
): boolean {
  return hasNextCursor && !loading && !failed
    && scrollHeight - (scrollTop + clientHeight) <= FORK_SCROLL_LOAD_THRESHOLD_PX;
}

/** Keyboard-paging trigger for the saved-session fork picker: moving the
 *  roving focus onto the LAST candidate requests the next older page through
 *  the same single-flight and latched-failure gates as scroll paging. */
export function shouldLoadOlderForkFromFocus(
  focus: number,
  count: number,
  hasNextCursor: boolean,
  loading: boolean,
  failed: boolean,
): boolean {
  return count > 0 && focus === count - 1 && hasNextCursor && !loading && !failed;
}

/** Bounded section/message caps for the pi-style handoff brief. */
export const HANDOFF_BRIEF_MAX_EXCHANGES = 10;
export const HANDOFF_BRIEF_MESSAGE_CLAMP = 400;
export const HANDOFF_BRIEF_MAX_CHARS = 16_000;

/** Latest COMPACTION summary text from a session's transcript items
 *  ({kind:'summary', label:'Compaction summary', text} — transcript.ts).
 *  Branch summaries never qualify; an empty-text summary is skipped; the
 *  LAST matching item wins (newest compaction). Null when the session has
 *  none (the caller falls back to the handoff brief with guidance). */
export function latestCompactionSummary(items: ReadonlyArray<unknown>): string | null {
  let latest: string | null = null;
  for (const item of items) {
    if (!item || typeof item !== 'object') continue;
    const record = item as Record<string, unknown>;
    if (record.kind !== 'summary' || record.label !== 'Compaction summary') continue;
    if (typeof record.text !== 'string' || record.text === '') continue;
    latest = record.text;
  }
  return latest;
}

/** Clamp a string to at most `max` UTF-16 code units WITHOUT splitting a
 *  surrogate pair: a clamped half-pair would corrupt the copied text. */
function clampHandoffText(text: string, max: number): string {
  if (text.length <= max) return text;
  let end = max;
  while (end > 0 && text.charCodeAt(end - 1) >= 0xd800 && text.charCodeAt(end - 1) <= 0xdbff) end -= 1;
  return `${text.slice(0, end)}…`;
}

/** Section body of `section` inside the validated backend copy-context
 *  artifact: the lines between `## {section}` and the next `## ` heading. */
function handoffSection(artifactText: string, section: string): string {
  const start = artifactText.indexOf(`## ${section}\n`);
  if (start < 0) return '';
  const bodyStart = start + `## ${section}\n`.length;
  const nextHeading = artifactText.indexOf('\n## ', bodyStart);
  return artifactText.slice(bodyStart, nextHeading < 0 ? artifactText.length : nextHeading).trim();
}

/** Build the pi-style transferable handoff brief (`/context` parity) from the
 *  catalog row and the validated backend copy-context artifact: session
 *  name, working directory, provider, the model section, and the LAST
 *  `HANDOFF_BRIEF_MAX_EXCHANGES` user/assistant exchanges with every message
 *  clamped to `HANDOFF_BRIEF_MESSAGE_CLAMP` characters. Bounded overall —
 *  another session can consume it quickly without paging a 64KB transcript. */
export function buildSessionHandoffBrief(opts: {
  name: string;
  cwd: string;
  provider: string;
  contextText: string;
}): string {
  const modelBody = handoffSection(opts.contextText, 'Model');
  // The conversation lives in the trailing `## Model context` section, the
  // serialized exchange blocks joined by blank lines.
  const contextStart = opts.contextText.indexOf('## Model context\n');
  const contextRaw = contextStart < 0
    ? ''
    : opts.contextText.slice(contextStart + '## Model context\n'.length).trim();
  const nextSteps = contextRaw.indexOf('\n\n## Next steps\n');
  const contextBody = (nextSteps >= 0 ? contextRaw.slice(0, nextSteps) : contextRaw).trim();
  const blocks = contextBody === '' ? [] : contextBody.split('\n\n');
  const exchanges: string[][] = [];
  for (const block of blocks) {
    if (block.startsWith('[User]: ') || exchanges.length === 0) exchanges.push([block]);
    else exchanges[exchanges.length - 1].push(block);
  }
  const recent = exchanges
    .slice(-HANDOFF_BRIEF_MAX_EXCHANGES)
    .map((exchange) => exchange.map((block) => clampHandoffText(block, HANDOFF_BRIEF_MESSAGE_CLAMP)).join('\n\n'));
  const header = `# Handoff brief\nSession: ${opts.name}\nWorking directory: ${opts.cwd}\nProvider: ${opts.provider}`;
  const modelPart = modelBody === '' ? '' : `\n\n## Model\n${modelBody}`;
  const exchangesPart = recent.length === 0
    ? '\n\n## Recent exchanges\n(none)'
    : `\n\n## Recent exchanges\n${recent.join('\n\n')}`;
  return clampHandoffText(`${header}${modelPart}${exchangesPart}`, HANDOFF_BRIEF_MAX_CHARS);
}
/** Send `session_copy_context` for the captured row path and validate the
 *  response (no clipboard write — callers compose their own copy artifact). */
export async function fetchSessionCopyContext(opts: {
  sessionPath: string;
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>;
}): Promise<{ text: string; truncated: boolean }> {
  const response = await opts.sendCommand({
    type: 'session_copy_context',
    sessionPath: opts.sessionPath,
  });
  return parseSessionCopyContextResponse(response);
}

/** Send `session_copy_context` for the captured row path, validate the
 *  response, and write `text` through the injected clipboard function. */
export async function copySessionContext(opts: {
  sessionPath: string;
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>;
  copyText: (text: string) => Promise<boolean>;
}): Promise<{ text: string; truncated: boolean; toast: string }> {
  const parsed = await fetchSessionCopyContext(opts);
  const copied = await opts.copyText(parsed.text);
  if (!copied) throw new Error('Failed to copy context');
  return {
    text: parsed.text,

    truncated: parsed.truncated,
    toast: parsed.truncated ? 'Copied context (truncated)' : 'Copied context',
  };
}

type MenuMode = 'root' | 'fork-from' | 'rename' | 'move' | 'delete-confirm';

interface SidebarMenuState {
  row: SessionRowWire;
  x: number;
  y: number;
  /** Bumped on every `openMenu`. The focus/position layout effects key on
   *  this open generation (plus other primitives) instead of the menu object
   *  itself: rename/move typing replaces the state object on every
   *  keystroke, and an object-identity dependency would re-focus + re-select
   *  the whole controlled rename value after every character. */
  openGen: number;
  mode: MenuMode;
  focus: number;
  forkMessages: ForkMessageWire[];
  forkNextCursor?: string;
  /** Latched failed older-page cursor: set exactly when the request for the
   *  current `forkNextCursor` fails with a business error, rendered as the
   *  bounded status + focusable Retry row, and cleared by success, Escape,
   *  a fresh open, menu close, and Retry itself. The latch is RENDERED
   *  state, not a bare ref: the status text carries the exact failed cursor
   *  and the Retry row joins the keyboard roving focus as index
   *  `forkMessages.length`. */
  forkFailedCursor?: string;
  renameValue: string;
  /** Target working directory typed into the Move session… input. */
  moveValue: string;
}

export function ownsSavedForkPageResponse(
  current: SidebarMenuState | null,
  rowKey: string,
  mode: 'root' | 'fork-from',
  cursor: string | undefined,
  expectedRequest: number,
  currentRequest: number,
  expectedMount: number,
  currentMount: number,
): current is SidebarMenuState {
  return current?.mode === mode &&
    sessionRowKey(current.row) === rowKey &&
    current.forkNextCursor === cursor &&
    expectedRequest === currentRequest &&
    expectedMount === currentMount;
}

interface SessionSidebarProps {
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>;
  /** Dedicated catalog sender: App wraps sendCommand with
   *  `{auxiliary: true}` for every `session_list` load, so the catalog RPC
   *  bypasses the switch-pending gate and the App RPC layer never toasts
   *  its failures. Normal user actions keep the plain `sendCommand`. */
  sendCatalogCommand: (command: Record<string, unknown>) => Promise<unknown>;
  downloadSessionExport: (downloadId: string, fileName: string) => Promise<void>;
  waitForReady: () => Promise<void>;
  /** Consume a lifecycle RPC result atomically from fork/clone. Switch does not use this path. */
  onLifecycleResult: (result: unknown) => Promise<unknown>;
  onSwitchSession: RequestSessionSwitch;
  activeSessionId: string | null;
  unreadBySessionId?: Record<string, number>;
  streamingBySessionId?: Record<string, boolean>;
  featureNav?: React.ReactNode;
  onCollapse: () => void;
  onReopenRail: () => void;
  onOpenManage: () => void;
  onSwitchComplete: () => void;
  onToast?: (
    message: string,
    error?: boolean,
    opts?: { action?: { label: string; onAction: () => void }; timeoutMs?: number },
  ) => void;
  /** App-owned catalog revision: App bumps it once per successful lifecycle
   *  fork/new/clone cut; a change triggers exactly one silent background
   *  catalog load (rows are never reset/cleared and failures never render). */
  catalogRefreshRevision?: number;
  /** App-owned switch-pending gate: true from the moment a session switch
   *  begins until it settles. Catalog loads never START while it is set;
   *  every load intent raised during the window coalesces into one
   *  deferred refresh that is flushed exactly once when the gate clears. */
  switchPending: boolean;
  /** App-owned ONE-row lifecycle overlay: the validated authoritative
   *  `session` row from the most recent successful new/fork/clone cut.
   *  Merged synchronously into the rendered catalog BEFORE any catalog
   *  request so the active child row paints in the same lifecycle cut; a
   *  failed or delayed catalog retains it, and it is absorbed only when a
   *  catalog response contains the exact `source::sessionId::path` row
   *  (never duplicated, never guessed client-side). */
  lifecycleRowOverlay?: SessionRowWire | null;
  /** App-owned overlay clear, invoked once the reconciled catalog contains
   *  the exact overlay identity (the catalog row now owns the display). */
  onLifecycleRowAbsorbed?: () => void;
  /** App-side cwd mirror write (DEFECT A fix): after a session_move cutover
   *  the authoritative destination cwd lands in App's sessionCwdBySessionId
   *  IMMEDIATELY — closing the render-time drift window that left markdown
   *  path probes computing relative paths against a stale cwd. */
  onSessionCwdChanged?: (sessionId: string, cwd: string) => void;
  /** Fired exactly once after a VALIDATED `session_delete` success (the
   *  parsed outcome already proved archived=true and the exact menu-captured
   *  sessionId); App uses it to remove that session's file-explorer slot so
   *  the committed-slot diff retires the session's conversations. */
  onSessionDeleted?: (sessionId: string) => void;
  /** Latest compaction summary text for a loaded session's transcript items,
   *  or null when the session has none / is not loaded (Copy context then
   *  triggers the on-demand compact seam below). */
  getCompactionSummary?: (sessionId: string) => string | null;
  /** On-demand compact seam (Copy context ALWAYS effective): runs the /compact
   *  wire path for the row's session with handoff instructions and resolves
   *  with the FRESH compaction summary. Rejects with the bounded admission
   *  reason when compaction is refused (streaming/compacting). */
  requestSessionCompactSummary?: (sessionId: string) => Promise<string>;
  /** Copy-context load gate: true when the session has a registered runtime
   *  (on-demand compact is valid); false → the path-based session_copy_context
   *  fallback (the manager-level handler that works for every saved row). */
  isSessionLoaded?: (sessionId: string) => boolean;
}

/** Validated switch target for a canonically identified catalog row, or
 *  `null` for a malformed row: an unmappable/empty source or a missing/
 *  unknown status grants NO switch authority — never an implicit native
 *  `rpi` default. Only the five byte-exact source tokens (`rpi`, `pi`, `omp`,
 *  `codex`, `grok`) validate — aliases `native`/`primary`/`grok/hyper` are
 *  malformed — and the status must be the exact wire token.
 *  ALREADY-IMPORTED rows (status `already_imported`, user decision: foreign
 *  rows stay visible and clicking opens the native copy) resolve to the
 *  NATIVE session using the backend-authored `nativePath` and
 *  `nativeSessionId`. The imported file name is a storage label and is never
 *  treated as runtime identity. */
export function buildSwitchTarget(
  row: Pick<SessionRowWire, 'path' | 'sessionId' | 'runtimeIncarnation' | 'source' | 'status'> & Partial<Pick<SessionRowWire, 'nativePath' | 'nativeSessionId'>>,
): SessionSwitchTarget | null {
  if (row.status === 'already_imported') {
    const nativePath = row.nativePath ?? '';
    const nativeId = row.nativeSessionId ?? '';
    if (nativePath === '' || nativeId === '') return null;
    return {
      path: nativePath,
      catalogSource: 'rpi',
      catalogSessionId: nativeId,
      catalogStatus: 'native',
      catalogRuntimeIncarnation: row.runtimeIncarnation ?? null,
    };
  }
  const identity = catalogIdentityOf(row);
  if (!identity) return null;
  return {
    path: row.path,
    catalogSource: identity.source,
    catalogSessionId: row.sessionId,
    catalogStatus: identity.status,
    catalogRuntimeIncarnation: row.runtimeIncarnation ?? null,
  };
}

/** Bootstrap/reconnect restore: a catalog row that already passed
 *  `selectSessionFromCatalog` is a restorable identity even when the
 *  overlay omitted source/status. Prefer the canonical tuple; otherwise
 *  treat the row as the native runtime named by sessionId+path. */
export function buildRestoreSwitchTarget(
  row: Pick<SessionRowWire, 'path' | 'sessionId' | 'runtimeIncarnation' | 'source' | 'status'> & Partial<Pick<SessionRowWire, 'nativePath' | 'nativeSessionId'>>,
): SessionSwitchTarget | null {
  const canonical = buildSwitchTarget(row);
  if (canonical) return canonical;
  if (row.status === 'already_imported') return null;
  if (row.path === '' || row.sessionId === '') return null;
  return {
    path: row.path,
    catalogSource: 'rpi',
    catalogSessionId: row.sessionId,
    catalogStatus: 'native',
    catalogRuntimeIncarnation: row.runtimeIncarnation ?? null,
  };
}


export function SessionSidebar({
  sendCommand,
  downloadSessionExport,
  waitForReady,
  onLifecycleResult,
  onSwitchSession,
  activeSessionId,
  unreadBySessionId,
  featureNav,
  onCollapse,
  onReopenRail,
  onOpenManage,
  onSwitchComplete,
  onToast,
  sendCatalogCommand,
  catalogRefreshRevision,
  switchPending,
  lifecycleRowOverlay,
  onLifecycleRowAbsorbed,
  onSessionCwdChanged,
  onSessionDeleted,
  getCompactionSummary,
  requestSessionCompactSummary,
  isSessionLoaded,
  streamingBySessionId,
}: SessionSidebarProps) {
  const [sessions, setSessions] = useState<SessionRowWire[]>([]);
  // Rendered rows = private catalog rows + the lifecycle overlay merged
  // SYNCHRONOUSLY during render — strictly BEFORE any catalog request (the
  // mount effect, poll, and revision loads all reconcile only the private
  // catalog). The active child row therefore paints in the same lifecycle
  // cut that set it, and a failed/delayed `session_list` retains it until
  // the exact identity appears (absorption) — see mergeLifecycleOverlayRow.
  const mergedSessions = useMemo(
    () => mergeLifecycleOverlayRow(sessions, lifecycleRowOverlay),
    [sessions, lifecycleRowOverlay],
  );
  const activeCatalogRow = resolveActiveSessionRow(mergedSessions, activeSessionId);
  const activeRowKey = activeCatalogRow ? sessionRowKey(activeCatalogRow) : null;


  const [busy, setBusy] = useState(false);
  const busyRef = useRef(false);
  const [exporting, setExporting] = useState(false);
  const exportingRef = useRef(false);
  const [copying, setCopying] = useState(false);
  const copyingRef = useRef(false);
  const [switchingRowKey, setSwitchingRowKey] = useState<string | null>(null);
  const switchingRef = useRef<string | null>(null);
  const mountedRef = useRef(true);
  const mountedGenRef = useRef(0);
  const requestGenRef = useRef(0);
  const sessionsRef = useRef(sessions);
  sessionsRef.current = sessions;
  const loadRef = useRef<(opts?: { waitForReady?: boolean; userInitiated?: boolean }) => Promise<void>>(async () => {});
  /** Synchronous mirror of the `switchPending` prop for callbacks (timers,
   *  clicks, effects): catalog intents may arrive between renders. */
  const switchPendingRef = useRef(switchPending);
  switchPendingRef.current = switchPending;
  /** Authoritative deferred-catalog intent. Any number of mount/poll/
   *  revision/action refresh intents raised while `switchPending` coalesce
   *  into this one flag; the flush effect below sends exactly one catalog
   *  load when the gate clears. Effects only request/flush it — the flag is
   *  the single authority, never mirrored into state. */
  const deferredCatalogRef = useRef(false);
  /** Explicit group intent. `true` means collapsed; `false` records an
   * explicit expansion of default-collapsed Agent sessions. Missing keys use
   * the subgroup default. */
  const collapseIntentRef = useRef<Map<string, boolean>>(new Map());
  const [collapseIntent, setCollapseIntent] = useState<Map<string, boolean>>(() => collapseIntentRef.current);
  /** Bounded client-side search query. Empty restores the default view. */
  const [searchQuery, setSearchQuery] = useState('');
  const [menu, setMenu] = useState<SidebarMenuState | null>(null);
  const [menuPos, setMenuPos] = useState({ left: 0, top: 0 });
  const menuRef = useRef<HTMLDivElement | null>(null);
  const forkScrollRef = useRef<HTMLDivElement | null>(null);
  const renameInputRef = useRef<HTMLInputElement | null>(null);
  const restoreFocusRef = useRef<HTMLElement | null>(null);
  const suppressClickRef = useRef(false);
  const menuStateRef = useRef<SidebarMenuState | null>(menu);
  const forkMenuRequestRef = useRef(0);
  const forkMenuLoadingOlderRef = useRef(false);
  const menuOpenGenRef = useRef(0);
  menuStateRef.current = menu;
  const longPressRef = useRef<{ pointerId: number; timer: number; x: number; y: number } | null>(null);
  const sidebarRootRef = useRef<HTMLElement | null>(null);

  /** Sessions whose wire `source` maps to one of the five allowed providers.
   *  Rows with an unrecognised source are filtered out entirely — they never
   *  appear in the sidebar and never create a top-level group. */
  const knownSessions = useMemo(
    () => mergedSessions.filter((row) => providerOf(row) !== null),
    [mergedSessions]
  );

  /** Rows matching the bounded client-side search. Temporary and bootstrap
   * rows stay searchable; active/loaded identities are forced visible. */
  const visibleSessions = useMemo(
    () => visibleSidebarSessions(knownSessions, searchQuery, activeSessionId),
    [knownSessions, searchQuery, activeSessionId]
  );

  const groups = useMemo(() => groupSessions(visibleSessions), [visibleSessions]);
  const collapsedGroups = useMemo(() => collapsedSidebarGroupKeys(
    visibleSessions,
    collapseIntent,
    searchQuery.trim() !== '',
  ), [visibleSessions, collapseIntent, searchQuery]);
  const treeItems = useMemo(() => {
    const out: SessionTreeItem[] = [];
    flattenTree(visibleSessions, collapsedGroups, out, groups);
    return out;
  }, [visibleSessions, groups, collapsedGroups]);


  const toggleGroup = useCallback((key: string) => {
    setCollapseIntent((previous) => {
      const next = new Map(previous);
      next.set(key, !collapsedSidebarGroupKeys(
        visibleSessions,
        previous,
        searchQuery.trim() !== '',
      ).has(key));
      collapseIntentRef.current = next;
      return next;
    });
  }, [searchQuery, visibleSessions]);

  /** Re-fetch the session catalog. Every load flows through the dedicated
   *  auxiliary catalog sender (`sendCatalogCommand`), so the App RPC layer
   *  never toasts a catalog failure and a pending switch never rejects the
   *  send at the gate. Auto/background callers (mount effect, poll,
   *  catalogRefreshRevision bump) pass `waitForReady: true` (default) so
   *  raw WebSocket OPEN waits for authoritative session binding. A gate
   *  timeout or transient post-gate disconnect is swallowed; the next poll
   *  retries. Active actions pass `waitForReady: false` and fail fast on
   *  the action itself. `load` NEVER throws: failed background loads retain
   *  the last successful catalog and retry silently; the first empty-catalog
   *  failure and explicit user refresh failures use the centralized toast. */
  /** Catalog-loading UI state.
   *  - `catalogRefreshing`: a session_list is in flight (any load). With an
   *    EMPTY catalog it renders skeleton rows (after the 150ms delayed-show);
   *    with rows painted it renders the thin top progress bar + spinning
   *    Refresh. Clears on settle (success or failure).
   *  - `catalogLoadShown`: the 150ms delayed-show latch — fast catalogs
   *    (<150ms) never see the skeleton.
   *  - `firstLoadFailed`: the FIRST load failed terminally → empty state.
   *    Refresh failures surface via the centralized toast; last-good rows
   *    are kept. Both clear on the next successful load. */
  const [catalogRefreshing, setCatalogRefreshing] = useState(false);
  const [catalogLoadShown, setCatalogLoadShown] = useState(false);
  const [firstLoadFailed, setFirstLoadFailed] = useState(false);
  const firstLoadFailedRef = useRef(false);
  const catalogLoadShowTimerRef = useRef<number | null>(null);
  const load = useCallback(async (opts?: { waitForReady?: boolean; userInitiated?: boolean }): Promise<void> => {
    const wait = opts?.waitForReady !== false;
    const mountedGen = mountedGenRef.current;
    const emptyCatalog = sessionsRef.current.length === 0;
    const showRefreshing = emptyCatalog || opts?.userInitiated === true;
    if (showRefreshing) setCatalogRefreshing(true);
    if (wait) {
      try {
        await waitForReady();
      } catch {
        if (showRefreshing) {
          setCatalogLoadShown(false);
          setCatalogRefreshing(false);
        }
        return; // gate timeout during disconnect — silent; next poll retries
      }
    }
    if (mountedGen !== mountedGenRef.current) {
      if (showRefreshing) {
        setCatalogLoadShown(false);
        setCatalogRefreshing(false);
      }
      return;
    }
    if (switchPendingRef.current) {
      deferredCatalogRef.current = true;
      if (showRefreshing) {
        setCatalogLoadShown(false);
        setCatalogRefreshing(false);
      }
      return;
    }
    deferredCatalogRef.current = false;
    const requestGen = ++requestGenRef.current;
    const previousRows = sessionsRef.current;
    const emptyAfterGate = previousRows.length === 0;
    const listResult = await refreshSessionList(sendCatalogCommand, previousRows);
    // Only the LIVE load clears the spinner/hint — a stale settle must never
    // clear a newer load's pending state.
    if (!isLiveSessionLoad(requestGen, requestGenRef.current, mountedGen, mountedGenRef.current)) {
      if (requestGenRef.current === requestGen) setCatalogRefreshing(false);
      return;
    }
    // Reset the 150ms one-way latch so every load re-arms the gate.
    setCatalogLoadShown(false);
    setCatalogRefreshing(false);
    if (listResult.error) {
      // Background 8s polls stay silent (spam); user refresh or the first
      // failed empty-catalog load toasts — latched, never repeated per poll.
      const firstFailure = emptyAfterGate && !firstLoadFailedRef.current;
      if (opts?.userInitiated === true || firstFailure) onToast?.('Failed to load sessions', true);
      firstLoadFailedRef.current = firstLoadFailedRef.current || emptyAfterGate;
      if (emptyAfterGate) setFirstLoadFailed(true);
      return;
    }
    firstLoadFailedRef.current = false;
    setFirstLoadFailed(false);
    // Functional updater: a collapse/expand click that lands between the
    // session_list request and this commit must survive — migrating from a
    // ref snapshot here would silently discard the click's intent.
    setCollapseIntent((current) => {
      const nextCollapseIntent = migrateSidebarCollapseIntent(previousRows, listResult.sessions, current);
      collapseIntentRef.current = nextCollapseIntent;
      return nextCollapseIntent;
    });
    setSessions((current) => reconcileSessionRowOrder(current, listResult.sessions));
  }, [sendCatalogCommand, waitForReady, onToast]);

  /** Overlay absorption: when a reconciled catalog response carries the
   *  EXACT overlay identity, the catalog row now owns the display and App's
   *  overlay is cleared — never duplicated. A FAILED load returns above
   *  before any reconcile, so the overlay survives failed/delayed catalogs
   *  and the next poll/revision retries. Replaced overlays (a newer
   *  lifecycle cut) are keyed from the current prop each render, so the
   *  check always tests the LIVE overlay identity. */
  const onLifecycleRowAbsorbedRef = useRef(onLifecycleRowAbsorbed);
  onLifecycleRowAbsorbedRef.current = onLifecycleRowAbsorbed;
  const lifecycleRowOverlayKey = lifecycleRowOverlay ? sessionRowKey(lifecycleRowOverlay) : null;
  const lifecycleRowOverlayKeyRef = useRef<string | null>(lifecycleRowOverlayKey);
  lifecycleRowOverlayKeyRef.current = lifecycleRowOverlayKey;
  useEffect(() => {
    const overlayKey = lifecycleRowOverlayKeyRef.current;
    if (!overlayKey) return;
    if (!sessionsRef.current.some((row) => sessionRowKey(row) === overlayKey)) return;
    onLifecycleRowAbsorbedRef.current?.();
  }, [sessions, lifecycleRowOverlayKey]);
  /** The single catalog-load entry point. While a switch is pending the
   *  load never starts: the intent is recorded once (N intents coalesce)
   *  and the flush effect sends exactly one load when the gate clears. An
   *  immediately started load also clears any stale deferred intent, so an
   *  intent landing in the same commit as the gate clearing can never
   *  double-send after the flush. */
  const requestCatalogLoad = useCallback((opts?: { waitForReady?: boolean; userInitiated?: boolean }): Promise<void> => {
    if (switchPendingRef.current) {
      deferredCatalogRef.current = true; // coalesce: one deferred refresh
      return Promise.resolve();
    }
    deferredCatalogRef.current = false;
    return load(opts);
  }, [load]);
  loadRef.current = requestCatalogLoad;

  /** 150ms delayed-show gate: the catalog skeleton only appears if the load
   *  is STILL in flight after 150ms AND the catalog is still empty — fast
   *  catalogs never see the skeleton (perceived-latency practice). */
  useEffect(() => {
    if (!catalogRefreshing || sessionsRef.current.length > 0) {
      if (catalogLoadShowTimerRef.current !== null) {
        window.clearTimeout(catalogLoadShowTimerRef.current);
        catalogLoadShowTimerRef.current = null;
      }
      return;
    }
    catalogLoadShowTimerRef.current = window.setTimeout(() => {
      catalogLoadShowTimerRef.current = null;
      if (catalogRefreshing && sessionsRef.current.length === 0) setCatalogLoadShown(true);
    }, 150);
    return () => {
      if (catalogLoadShowTimerRef.current !== null) {
        window.clearTimeout(catalogLoadShowTimerRef.current);
        catalogLoadShowTimerRef.current = null;
      }
    };
  }, [catalogRefreshing]);
  useEffect(() => {
    const instanceGen = ++mountedGenRef.current;
    mountedRef.current = true;
    // First load is user-initiated (skeleton + bar allowed if slow); the
    // poll is background — no bar flash every 8s. loadRef keeps the interval
    // on the latest catalog/load without putting `load` in the effect deps.
    // The effect is STABLE across the active session id: an active-id change
    // never triggers its own immediate load — every negotiated switch
    // refreshes via runSwitch's explicit load, every lifecycle cut
    // (new/fork/clone) via App's catalogRefreshRevision bump, and the poll
    // is the background fallback. A per-id effect here would double-fetch
    // each lifecycle cut (two authorities racing).
    loadRef.current({ waitForReady: true, userInitiated: true }).catch(() => {});
    const poll = window.setInterval(() => {
      loadRef.current().catch(() => {}); // background: silent on any failure
    }, 8000);
    return () => {
      mountedRef.current = false;
      if (mountedGenRef.current === instanceGen) mountedGenRef.current += 1;
      window.clearInterval(poll);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /** App-owned catalog revision (bumped once per successful lifecycle
   *  fork/new/clone cut): a change triggers exactly ONE background
   *  `session_list`. It never resets or clears rows, never surfaces an
   *  error, and a failed refresh keeps the last catalog (the 8s poll
   *  retries). The first-seen value skips: the mount effect owns the initial
   *  load, so a remount with an already-advanced revision does not
   *  double-fetch. */
  const catalogRevisionRef = useRef(catalogRefreshRevision);
  useEffect(() => {
    if (catalogRevisionRef.current === catalogRefreshRevision) return;
    catalogRevisionRef.current = catalogRefreshRevision;
    loadRef.current().catch(() => {}); // background: silent on any failure
  }, [catalogRefreshRevision]);

  /** Flush the deferred catalog intent exactly once when a pending switch
   *  settles. The flag is re-checked through the render-mirrored ref: if a
   *  NEW switch began between the intent and this flush, the intent stays
   *  deferred for the next settle. `requestCatalogLoad` re-checks the same
   *  gate, so a revision intent landing in this same commit cannot
   *  double-send behind the flush. */
  useEffect(() => {
    if (switchPending || !deferredCatalogRef.current) return;
    if (switchPendingRef.current) return; // a newer switch began before the send
    deferredCatalogRef.current = false;
    void loadRef.current().catch(() => {}); // background: silent on any failure
  }, [switchPending]);

  const run = useCallback(
    async (action: () => Promise<unknown>, switchingKey?: string): Promise<boolean> => {
      // Refuse while a switch is pending: lifecycle commands (fork/new) must
      // never race the negotiated switch, and latest-click-wins stays
      // exclusively on runSwitch.
      if (busyRef.current || switchingRef.current !== null) return false;
      if (switchingKey) {
        switchingRef.current = switchingKey;
        setSwitchingRowKey(switchingKey);
      }
      busyRef.current = true;
      setBusy(true);
      try {
        const result = await action();
        await onLifecycleResult(result);
        // NO awaited catalog refresh here: the post-lifecycle session_list is
        // owned by App's catalogRefreshRevision bump (exactly ONE background
        // load per lifecycle cut). Awaiting another load here would race a
        // second/third fetch against it. A lifecycle failure still toasts
        // below; a post-success refresh failure keeps the rows and the 8s
        // poll retries.
        return true;
      } catch (err) {
        const commandError = err as Error;
        const message = safeText(commandError.message);
        if (shouldToastSessionActionError(commandError)) onToast?.(message, true);
        return false;
      } finally {
        busyRef.current = false;
        // NEVER clear a marker this action does not own: a run() without a
        // switchingKey has no marker authority at all.
        if (switchingRef.current === switchingKey) {
          switchingRef.current = null;
          if (mountedRef.current) setSwitchingRowKey(null);
        }
        if (mountedRef.current) setBusy(false);
      }
    },
    [onLifecycleResult, onToast]
  );

  const runSwitch = useCallback(
    async (row: SessionRowWire, switchingKey: string): Promise<boolean> => {
      if (busyRef.current) return false;
      // A malformed catalog row (unmappable/empty source, non-exact status,
      // or already_imported missing nativePath/nativeSessionId) carries no
      // switch authority. The click must not be silent: one error toast,
      // displayed/commandTarget unchanged.
      const target = buildSwitchTarget(row);
      if (!target) {
        onToast?.('cannot switch this session: catalog identity is invalid', true);
        return false;
      }
      // Latest click wins: a click while another switch is pending simply
      // re-targets (App's generation replace settles the old transaction
      // false); the pending switch never blocks the sidebar.
      switchingRef.current = switchingKey;
      // Defer paint so same-turn fast switches stay within the first frame.
      const paintRaf = window.requestAnimationFrame(() => {
        if (mountedRef.current && switchingRef.current === switchingKey) {
          setSwitchingRowKey(switchingKey);
        }
      });
      try {
        const ok = await onSwitchSession(target, { origin: 'user' });
        if (ok) {
          void requestCatalogLoad({ waitForReady: false });
          try {
            const nativeSwitch = [...document.querySelectorAll('.session-sidebar__switch')]
              .find((el) => el instanceof HTMLElement && el.dataset.sessionId === target.catalogSessionId);
            nativeSwitch?.scrollIntoView({ block: 'nearest' });
          } catch {
            // Catalog refresh already dispatched; missing native row is non-fatal.
          }
        }
        return ok;
      } catch (err) {
        // An unexpected switch error is never silent: exactly one bounded
        // toast — but never a duplicate of App's user-origin switch-failure
        // toast (shouldToastSessionActionError respects toastOwned, and the
        // protocol-normal switch-pending rejection stays silent).
        const commandError = err as Error;
        if (shouldToastSessionActionError(commandError)) onToast?.(safeText(commandError.message), true);
        return false;
      } finally {
        // Cancel before paint when the switch settles in the same turn.
        window.cancelAnimationFrame(paintRaf);
        // Owner guard: a superseded (replaced) switch must never clear the
        // newer row's pending marker.
        if (switchingRef.current === switchingKey) {
          switchingRef.current = null;
          if (mountedRef.current) setSwitchingRowKey(null);
        }
      }
    },
    [onSwitchSession, requestCatalogLoad, onToast]
  );

  /** Delete/restore/rename: refresh the catalog only — do not activate. The
   *  action's own failure is exactly one bottom-right toast (unless App
   *  already owns the toast); the post-success catalog refresh is
   *  background-owned and can never turn the action into a failure or
   *  render anything — a failed refresh keeps the rows and retries.
   *
   *  `backgroundCatalog: true` (used by confirmDelete) launches the SAME
   *  single silent catalog load WITHOUT awaiting it: busy is released as
   *  soon as the action settles so the bounded Undo toast (and its 5s
   *  window) is not delayed by the refresh. The load stays exactly one
   *  request through requestCatalogLoad's single-entry gate; failures stay
   *  silent and the 8s poll retries. */
  const runRefresh = useCallback(
    async <T,>(action: () => Promise<T>, opts?: { backgroundCatalog?: boolean }): Promise<T | false> => {
      // Refuse while a switch is pending: move/delete/refresh commands must
      // never race the negotiated switch.
      if (busyRef.current || switchingRef.current !== null) return false;
      busyRef.current = true;
      setBusy(true);
      let actionCompleted = false;
      try {
        const result = await action();
        actionCompleted = true;
        if (opts?.backgroundCatalog === true) {
          void requestCatalogLoad({ waitForReady: false }).catch(() => {});
        } else {
          await requestCatalogLoad({ waitForReady: false, userInitiated: true });
        }
        return result;
      } catch (err) {
        if (!actionCompleted) {
          const commandError = err as Error;
          if (shouldToastSessionActionError(commandError)) {
            onToast?.(safeText(commandError.message), true);
          }
        }
        return false;
      } finally {
        busyRef.current = false;
        if (mountedRef.current) setBusy(false);
      }
    },
    [onToast, requestCatalogLoad]
  );

  const closeMenu = useCallback(() => {
    forkMenuRequestRef.current += 1;
    forkMenuLoadingOlderRef.current = false;
    menuStateRef.current = null;
    setMenu(null);
    const restore = restoreFocusRef.current;
    restoreFocusRef.current = null;
    window.setTimeout(() => restore?.focus(), 0);
  }, []);

  const openMenu = useCallback((row: SessionRowWire, x: number, y: number, restore: HTMLElement | null): boolean => {
    const isActive = activeRowKey !== null && sessionRowKey(row) === activeRowKey;
    const items = contextMenuItems({
      native: isNativeSession(row),
      deleteDisabled: isSessionActionDisabled(isActive),
      exportDisabled: exporting,
      copyContextDisabled: copying,
      moveDisabled: streamingBySessionId?.[row.sessionId] === true,
    });
    if (items.length === 0) return false;
    forkMenuRequestRef.current += 1;
    menuOpenGenRef.current += 1;
    forkMenuLoadingOlderRef.current = false;
    restoreFocusRef.current = restore;
    setMenuPos(flipMenuPosition(
      x, y, MENU_ESTIMATE_WIDTH, MENU_ESTIMATE_HEIGHT,
      window.innerWidth, window.innerHeight,
    ));
    setMenu({
      row,
      x,
      y,
      openGen: menuOpenGenRef.current,
      mode: 'root',
      focus: firstEnabledMenuFocus(items.map((item) => item.disabled === true)),
      forkMessages: [],
      forkNextCursor: undefined,
      renameValue: row.name?.trim() || sessionTitle(row),
      moveValue: '',
    });
    return true;
  }, [activeRowKey, copying, exporting, streamingBySessionId]);
  useLayoutEffect(() => {
    if (!menu || !menuRef.current) return;
    const rect = menuRef.current.getBoundingClientRect();
    setMenuPos(flipMenuPosition(
      menu.x, menu.y, rect.width, rect.height,
      window.innerWidth, window.innerHeight,
    ));
    // Primitive deps: re-clamp only when the menu's rendered geometry can
    // change — a fresh open, a moved anchor, a mode body swap, a fork-list
    // length change, or Move-input suggestions appearing. Rename/move typing
    // replaces the state object but not the geometry, so it must not re-run.
  }, [menu?.openGen, menu?.x, menu?.y, menu?.mode, menu?.forkMessages.length, menu?.moveValue]);

  useLayoutEffect(() => {
    if (!menu) return;
    if (menu.mode === 'rename') {
      renameInputRef.current?.focus({ preventScroll: true });
      renameInputRef.current?.select();
      return;
    }
    if (menu.mode === 'move') {
      menuRef.current?.querySelector<HTMLInputElement>('#session-move-cwd-input')
        ?.focus({ preventScroll: true });
      return;
    }
    const items = menuRef.current?.querySelectorAll<HTMLElement>('[role="menuitem"]');
    const focused = items?.[menu.focus];
    focused?.focus({ preventScroll: true });
    // Fork-from roving focus keeps the bounded scroller on the focused
    // candidate (including the latched-failure Retry row): keyboard paging
    // must never leave the focused item outside the visible window.
    if (menu.mode === 'fork-from' && focused) {
      focused.scrollIntoView({ block: 'nearest' });
    }
    // Primitive deps: focus/select must run exactly once per menu open
    // (openGen), per mode transition (rename/move entry selects the full
    // title), and per root-mode focus move — never per renameValue
    // keystroke, which replaces the menu object only.
  }, [menu?.openGen, menu?.mode, menu?.focus]);

  useEffect(() => {
    if (!menu) return;
    const onDown = (event: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(event.target as Node)) closeMenu();
    };
    const onWheel = (event: WheelEvent) => {
      if (menu.mode === 'fork-from' && forkScrollRef.current?.contains(event.target as Node)) return;
      closeMenu();
    };
    const onTouchMove = (event: TouchEvent) => {
      if (menu.mode === 'fork-from' && forkScrollRef.current?.contains(event.target as Node)) return;
      closeMenu();
    };
    const onWinBlur = () => closeMenu();
    document.addEventListener('mousedown', onDown);
    window.addEventListener('wheel', onWheel, true);
    window.addEventListener('touchmove', onTouchMove, true);
    window.addEventListener('blur', onWinBlur);
    return () => {
      document.removeEventListener('mousedown', onDown);
      window.removeEventListener('wheel', onWheel, true);
      window.removeEventListener('touchmove', onTouchMove, true);
      window.removeEventListener('blur', onWinBlur);
    };
  }, [menu, closeMenu]);


  const activateLifecycle = useCallback(async (action: () => Promise<unknown>, toastName?: string) => {
    const ok = await run(action);
    closeMenu();
    if (ok) {
      if (toastName) onToast?.(forkToastMessage(toastName));
      onSwitchComplete();
    }
  }, [closeMenu, onSwitchComplete, onToast, run]);

  /** Submit the Move session… form: one bounded `session_move` wire command
   *  carrying the captured row path and the manually entered absolute target
   *  directory, then close, refetch the catalog exactly once, and toast.
   *  The empty picker is a disabled submit: a blank input returns before any
   *  wire send. The response's destination row is parsed and validated
   *  (sessionId AND source path AND `from` bound to the captured row, native
   *  row, cwd lexically equal to the explicit target, path == moved file)
   *  before success and returned to the caller with its truthful `moved`
   *  marker (false = the session already resided in the target store — an
   *  idempotent backend no-op that is STILL a success, never a failure);
   *  errors surface through the bounded runRefresh channel (exactly one
   *  toast) and never echo local paths. Move never auto-switches: a
   *  non-active row relocates its saved file only, and the ACTIVE row is
   *  moved server-side with a cutover that rebinds that same active
   *  runtime's cwd to the target (idle only — a busy turn is refused
   *  fail-closed). */
  const commitMove = useCallback(async (value?: string): Promise<SessionMoveDestination | false> => {
    if (!menu || menu.mode !== 'move') return false;
    if (streamingBySessionId?.[menu.row.sessionId] === true) return false;
    const targetCwd = (value ?? menu.moveValue).trim();
    if (targetCwd === '') return false;
    const result = await runRefresh(async () => parseSessionMoveResponse(await moveSession({
      sessionPath: menu.row.path,
      targetCwd,
      sendCommand,
    }), {
      expectedSessionId: menu.row.sessionId,
      expectedSessionPath: menu.row.path,
      expectedTargetCwd: targetCwd,
    }));
    if (result === false) return false;
    // DEFECT A: the authoritative destination cwd lands in App's mirror
    // IMMEDIATELY (before any render/refetch) so markdown path probes never
    // compute a relative path against a stale cwd.
    onSessionCwdChanged?.(result.sessionId, result.cwdRaw);
    // Regroup WITHOUT waiting for a refetch: the parsed destination row is
    // caller-bound and validated, so patch the local catalog in place — the
    // refetch inside runRefresh may have raced the backend's post-move
    // rescan and returned pre-move (stale-grouped) rows.
    setSessions((current) => applySessionMoveToRows(current, result));
    closeMenu();
    onToast?.(sessionMoveToast());
    return result;
  }, [closeMenu, menu, onSessionCwdChanged, onToast, runRefresh, sendCommand, streamingBySessionId]);

  /** Confirm delete: one bounded `session_delete`, then the response is
   *  parsed and validated (archived=true, nonempty sessionId AND undoToken)
   *  BEFORE the archived toast. A malformed success surfaces exactly one
   *  bounded error toast through runRefresh; the archived toast is skipped
   *  and the confirm menu stays open. A valid outcome closes the menu and
   *  shows the 'Session archived' toast IMMEDIATELY (the 5s Undo window
   *  starts at once) while the catalog refresh runs as exactly one silent
   *  background load. The undo token is always present, so Undo restores
   *  exactly once via `session_restore`. */
  const confirmDelete = useCallback(async () => {
    if (!menu) return;
    const result = await runRefresh(async () => parseSessionDeleteResponse(
      await sendCommand({ type: 'session_delete', sessionPath: menu.row.path }),
      menu.row.sessionId,
    ), { backgroundCatalog: true });
    if (result === false) return;
    // Validated outcome (archived=true, exact menu-captured sessionId): the
    // host removes that session's file-explorer slot BEFORE the archive/undo
    // toast so the committed-slot diff retires the session's conversations
    // promptly.
    onSessionDeleted?.(result.sessionId);
    closeMenu();
    onToast?.('Session archived', false, {
      timeoutMs: SESSION_ARCHIVE_UNDO_MS,
      action: {
        label: 'Undo',
        onAction: () => {
          void runRefresh(() => sendCommand({ type: 'session_restore', undoToken: result.undoToken }));
        },
      },
    });
  }, [closeMenu, menu, onSessionDeleted, onToast, runRefresh, sendCommand]);
  /** Directory suggestions for the Move session… input: one bounded
   *  `path_complete` probe through the existing sendCommand socket, bound to
   *  the active source session when one is loaded so relative suggestions
   *  resolve against its cwd. Errors and malformed replies stay quiet (null)
   *  so completion never blocks manual absolute-path entry. */
  const sendMovePathComplete = useCallback(
    (req: PathCompleteRequest): Promise<PathCompleteResponse | null> =>
      sendPathCompleteRequest(sendCommand, activeCatalogRow?.sessionId, req),
    [activeCatalogRow?.sessionId, sendCommand],
  );
  const commitRename = useCallback(async () => {
    if (!menu) return;
    const name = (renameInputRef.current?.value ?? menu.renameValue).trim();
    if (!name) {
      onToast?.('session name cannot be empty', true);
      return;
    }
    const result = await runRefresh(() =>
      sendCommand({ type: 'set_session_name', name, sessionPath: menu.row.path })
    );
    if (result !== false) closeMenu();
  }, [closeMenu, menu, onToast, runRefresh, sendCommand]);

  const activateItem = useCallback(async (id: SessionMenuItemId) => {
    if (!menu) return;
    const row = menu.row;
    const isActive = activeRowKey !== null && sessionRowKey(row) === activeRowKey;

    switch (id) {
      case 'copy-context': {
        if (copyingRef.current) return;
        copyingRef.current = true;
        setCopying(true);
        try {
          // PRIMARY (user requirement): copy the session's LATEST compaction
          // summary VERBATIM when one exists in the transcript items.
          const runtimeId = catalogRuntimeSessionId(row);
          const summaryText = runtimeId !== '' ? getCompactionSummary?.(runtimeId) ?? null : null;
          if (summaryText !== null) {
            const copiedSummary = await copyTextToClipboard(summaryText);
            if (!copiedSummary) throw new Error('Failed to copy context');
            if (!mountedRef.current) return;
            closeMenu();
            onToast?.('Copied compact summary');
            return;
          }
          // ON-DEMAND (user requirement: Copy context is ALWAYS effective,
          // like pi's /context) — branched on whether the session is LOADED:
          // (a) LOADED + no stored summary → generate one NOW via the /compact
          // wire path and copy the FRESH summary when it settles. No silent
          // transcript-brief fallback: if compaction is refused
          // (streaming/compacting) the bounded reason is toasted and nothing
          // is copied.
          if (runtimeId !== '' && isSessionLoaded?.(runtimeId) === true) {
            const fresh = await requestSessionCompactSummary?.(runtimeId);
            if (typeof fresh === 'string' && fresh !== '') {
              const copiedFresh = await copyTextToClipboard(fresh);
              if (!copiedFresh) throw new Error('Failed to copy context');
              if (!mountedRef.current) return;
              closeMenu();
              onToast?.('Copied compact summary');
              return;
            }
            // The compact ran but produced NO summary (a session with nothing
            // to summarize — a legitimate edge, not an error): fall through
            // to the transcript brief below with a truthful toast.
          }
          // (b) UNLOADED row (no registered runtime) OR a loaded session whose
          // compact produced no summary: the PATH-BASED session_copy_context —
          // the manager-level handler that works for every saved row — plus
          // the brief built from its artifact. (Unloaded rows must never run
          // the compact RPC: it routes through the runtime registry and would
          // reject 'unknown session' — or worse, spawn a runtime for a copy.)
          const artifact = await fetchSessionCopyContext({ sessionPath: row.path, sendCommand });
          const brief = buildSessionHandoffBrief({
            name: sessionTitle(row),
            cwd: row.cwd ?? '',
            provider: providerOf(row) ?? row.source ?? '',
            contextText: artifact.text,
          });
          const copiedBrief = await copyTextToClipboard(brief);
          if (!copiedBrief) throw new Error('Failed to copy context');
          if (!mountedRef.current) return;
          closeMenu();
          onToast?.('Copied context brief');
        } catch (err) {
          if (!mountedRef.current) return;
          closeMenu();
          const commandError = err as Error;
          if (shouldToastSessionActionError(commandError)) {
            onToast?.(safeText(commandError.message), true);
          }
        } finally {
          copyingRef.current = false;
          if (mountedRef.current) setCopying(false);
        }
        return;
      }
      case 'export-html': {
        if (exportingRef.current) return;
        exportingRef.current = true;
        setExporting(true);
        try {
          const response = await sendCommand({ type: 'export_html', sessionPath: row.path });
          if (!response || typeof response !== 'object'
            || !('downloadId' in response) || typeof response.downloadId !== 'string'
            || !('fileName' in response) || typeof response.fileName !== 'string') {
            throw new Error('session export response is invalid');
          }
          await downloadSessionExport(response.downloadId, response.fileName);
          closeMenu();
          onToast?.(`Exported ${response.fileName}`);
        } catch (err) {
          closeMenu();
          const commandError = err as Error;
          if (shouldToastSessionActionError(commandError)) {
            onToast?.(safeText(commandError.message), true);
          }
        } finally {
          exportingRef.current = false;
          if (mountedRef.current) setExporting(false);
        }
        return;
      }
      case 'fork-latest':
        if (!isNativeSession(row)) return;
        await activateLifecycle(
          () => sendCommand({ type: 'session_fork_latest', sessionPath: row.path }),
          sessionTitle(row),
        );
        return;
      case 'fork-from': {
        if (!isNativeSession(row)) return;
        const rowKey = sessionRowKey(row);
        const mountedGen = mountedGenRef.current;
        const requestGen = ++forkMenuRequestRef.current;
        try {
          const data = await sendCommand({ type: 'get_fork_messages', sessionPath: row.path });
          const page = parseForkMessagesPage(data);
          if (page.messages.length === 0) {
            onToast?.('no forkable messages', true);
            return;
          }
          setMenu((current) => ownsSavedForkPageResponse(
            current, rowKey, 'root', undefined,
            requestGen, forkMenuRequestRef.current, mountedGen, mountedGenRef.current,
          ) ? {
            ...current,
            mode: 'fork-from',
            forkMessages: page.messages,
            forkNextCursor: page.nextCursor,
            focus: 0,
          } : current);
        } catch (err) {
          if (mountedRef.current && ownsSavedForkPageResponse(
            menuStateRef.current, rowKey, 'root', undefined,
            requestGen, forkMenuRequestRef.current, mountedGen, mountedGenRef.current,
          ) && !isTransportError(err)) {
            const commandError = err as Error;
            if (shouldToastSessionActionError(commandError)) {
              onToast?.(safeText(commandError.message), true);
            }
          }
        }
        return;
      }
      case 'clone':
        if (!isNativeSession(row)) return;
        await activateLifecycle(() => sendCommand({ type: 'clone', sessionPath: row.path }));
        return;
      case 'rename':
        if (!isNativeSession(row)) return;
        setMenu((current) => current ? { ...current, mode: 'rename', focus: 0 } : current);
        return;
      case 'move':
        if (!isNativeSession(row) || streamingBySessionId?.[row.sessionId] === true) return;
        // Prefill the CURRENT full path (D3): the user sees the whole working
        // directory and edits the tail instead of typing it blind.
        setMenu((current) => current ? { ...current, mode: 'move', focus: 0, moveValue: row.cwd ?? '' } : current);
        return;
      case 'delete':
        if (!isNativeSession(row) || isSessionActionDisabled(isActive)) return;
        setMenu((current) => current ? { ...current, mode: 'delete-confirm', focus: 0 } : current);
        return;
      case 'delete-confirm':
        await confirmDelete();
        return;
      case 'delete-cancel':
        setMenu((current) => current ? { ...current, mode: 'root', focus: 0 } : current);
        return;
    }
  }, [activateLifecycle, activeRowKey, closeMenu, confirmDelete, downloadSessionExport, getCompactionSummary, menu, onToast, requestSessionCompactSummary, sendCommand, streamingBySessionId]);


  const forkFromEntry = useCallback(async (entryId: string) => {
    if (!menu) return;
    const row = menu.row;
    await activateLifecycle(
      () => sendCommand({ type: 'fork', entryId, sessionPath: row.path }),
      sessionTitle(row),
    );
  }, [activateLifecycle, menu, sendCommand]);

  const loadOlderForkFrom = useCallback(async (): Promise<void> => {
    if (!menu || menu.mode !== 'fork-from' || !menu.forkNextCursor || forkMenuLoadingOlderRef.current) return;
    const row = menu.row;
    const rowKey = sessionRowKey(row);
    const cursor = menu.forkNextCursor;
    const mountedGen = mountedGenRef.current;
    const requestGen = ++forkMenuRequestRef.current;
    forkMenuLoadingOlderRef.current = true;
    try {
      const data = await sendCommand({ type: 'get_fork_messages', sessionPath: row.path, cursor });
      const page = parseForkMessagesPage(data);
      if (!mountedRef.current || !ownsSavedForkPageResponse(
        menuStateRef.current, rowKey, 'fork-from', cursor,
        requestGen, forkMenuRequestRef.current, mountedGen, mountedGenRef.current,
      )) return;
      setMenu((current) => ownsSavedForkPageResponse(
        current, rowKey, 'fork-from', cursor,
        requestGen, forkMenuRequestRef.current, mountedGen, mountedGenRef.current,
      ) ? {
        ...current,
        forkMessages: mergeForkMessagePages(current.forkMessages, page.messages),
        forkNextCursor: page.nextCursor,
        focus: current.focus,
        forkFailedCursor: undefined,
      } : current);
    } catch (err) {
      if (mountedRef.current && ownsSavedForkPageResponse(
        menuStateRef.current, rowKey, 'fork-from', cursor,
        requestGen, forkMenuRequestRef.current, mountedGen, mountedGenRef.current,
      ) && !isTransportError(err)) {
        // Rendered latch: the fork-from menu shows the bounded status + Retry
        // row for this exact cursor until a success, Escape, or Retry clears it.
        setMenu((current) => ownsSavedForkPageResponse(
          current, rowKey, 'fork-from', cursor,
          requestGen, forkMenuRequestRef.current, mountedGen, mountedGenRef.current,
        ) ? { ...current, forkFailedCursor: cursor } : current);
        const commandError = err as Error;
        if (shouldToastSessionActionError(commandError)) {
          onToast?.(safeText(commandError.message), true);
        }
      }
    } finally {
      if (requestGen === forkMenuRequestRef.current) forkMenuLoadingOlderRef.current = false;
    }
  }, [menu, sendCommand, onToast]);

  /** Retry after a latched older-page failure: clear the rendered latch, then
   *  re-run the SAME single-flight older-page load for the still-current
   *  `forkNextCursor`. Invoked by the Retry row's click AND its keyboard
   *  activation (Enter/Space in the fork-from roving focus). */
  const retryForkOlderFrom = useCallback((): void => {
    setMenu((current) => current && current.mode === 'fork-from' && current.forkFailedCursor !== undefined
      ? { ...current, forkFailedCursor: undefined }
      : current);
    void loadOlderForkFrom();
  }, [loadOlderForkFrom]);

  const onForkFromScroll = useCallback((event: React.UIEvent<HTMLDivElement>) => {
    if (!menu || menu.mode !== 'fork-from') return;
    const target = event.currentTarget;
    const remaining = target.scrollHeight - (target.scrollTop + target.clientHeight);
    // Scrolling deliberately away from the bottom clears the rendered failure
    // latch so a later return can retry; guard the state write against the
    // high-frequency scroll stream.
    if (remaining > FORK_SCROLL_LOAD_THRESHOLD_PX && menu.forkFailedCursor !== undefined) {
      setMenu((current) => current?.forkFailedCursor !== undefined && current.mode === 'fork-from'
        ? { ...current, forkFailedCursor: undefined }
        : current);
    }
    if (shouldLoadOlderForkFromScroll(
      target.scrollTop,
      target.clientHeight,
      target.scrollHeight,
      menu.forkNextCursor !== undefined,
      forkMenuLoadingOlderRef.current,
      menu.forkFailedCursor === menu.forkNextCursor,
    )) {
      void loadOlderForkFrom();
    }
  }, [loadOlderForkFrom, menu]);


  const onMenuKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    // The Move session… input owns its keys via PathAutocomplete plus the
    // wrapper handler in the portal; the root-menu handler must not also
    // treat Enter/Escape/Tab as menu navigation.
    if (!menu || menu.mode === 'rename' || menu.mode === 'move') return;
    if (menu.mode === 'fork-from') {
      // The latched failure appends ONE extra roving-focus item after the
      // candidates: the focusable Retry row at index `count`.
      const count = menu.forkMessages.length;
      const failed = menu.forkFailedCursor !== undefined;
      const navCount = failed ? count + 1 : count;
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
        event.preventDefault();
        const next = nextMenuFocus(menu.focus, event.key === 'ArrowDown' ? 1 : -1, navCount);
        setMenu((current) => current ? { ...current, focus: next } : current);
        // Reaching the LAST candidate with a pending older page triggers the
        // same single-flight paging as a near-bottom scroll.
        if (shouldLoadOlderForkFromFocus(
          next,
          count,
          menu.forkNextCursor !== undefined,
          forkMenuLoadingOlderRef.current,
          menu.forkFailedCursor === menu.forkNextCursor,
        )) {
          void loadOlderForkFrom();
        }
      } else if (event.key === 'Enter' || event.key === ' ') {
        event.preventDefault();
        if (failed && menu.focus === count) {
          retryForkOlderFrom();
        } else {
          const pick = menu.forkMessages[menu.focus];
          if (pick) void forkFromEntry(pick.entryId);
        }
      } else if (event.key === 'Escape') {
        event.preventDefault();
        forkMenuRequestRef.current += 1;
        forkMenuLoadingOlderRef.current = false;
        setMenu((current) => current ? {
          ...current,
          mode: 'root',
          focus: 0,
          forkMessages: [],
          forkNextCursor: undefined,
          forkFailedCursor: undefined,
        } : current);
      } else if (event.key === 'Tab') {
        event.preventDefault();
        closeMenu();
      }
      return;
    }
    const items = contextMenuItems({
      native: isNativeSession(menu.row),
      deleteDisabled: isSessionActionDisabled(
        activeRowKey !== null && sessionRowKey(menu.row) === activeRowKey,
      ) || switchingRef.current !== null,
      confirmingDelete: menu.mode === 'delete-confirm',
      exportDisabled: exporting,
      copyContextDisabled: copying,
      moveDisabled: streamingBySessionId?.[menu.row.sessionId] === true,
    });

    const disabled = items.map((item) => item.disabled === true);
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      const next = nextMenuFocus(menu.focus, event.key === 'ArrowDown' ? 1 : -1, items.length, disabled);
      setMenu((current) => current ? { ...current, focus: next } : current);
    } else if (event.key === 'Escape') {
      event.preventDefault();
      if (menu.mode !== 'root') {
        setMenu((current) => current ? { ...current, mode: 'root', focus: 0 } : current);
      } else {
        closeMenu();
      }
    } else if (event.key === 'Tab') {
      event.preventDefault();
      closeMenu();
    }
  };

  const clearLongPress = () => {
    const pending = longPressRef.current;
    if (pending) {
      window.clearTimeout(pending.timer);
      longPressRef.current = null;
    }
  };

  return (
    <aside id="session-sidebar" ref={sidebarRootRef} className="session-sidebar" aria-label="Sessions">
      <div className="session-sidebar__head">
        <span className="session-sidebar__title">Sessions</span>
        {/* Desktop collapsed-rail reopen control (CSS hides it ≤720px;
            the ☰ header menu owns mobile drawer open/close). */}
        <button
          id="rail-reopen-btn"
          type="button"
          title="Expand the session sidebar"
          aria-label="Expand the session sidebar"
          onClick={onReopenRail}
        >
          »
        </button>
        <div className="session-sidebar__head-actions" role="toolbar" aria-label="Session actions">
          <button
            id="sidebar-collapse-btn"
            type="button"
            className="session-sidebar__icon-btn"
            title="Collapse the session sidebar"
            aria-label="Collapse the session sidebar"
            onClick={(event) => {
              event.currentTarget.blur();
              onCollapse();
              requestAnimationFrame(() => {
                document.getElementById('rail-reopen-btn')?.focus();
              });
            }}
          >
            «
          </button>
        </div>
      </div>

      {catalogRefreshing && mergedSessions.length > 0 && (
        <div className="session-sidebar__refresh-bar" role="progressbar" aria-label="Refreshing sessions" />
      )}

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
            onClick={() => setSearchQuery('')}
          >
            ✕
          </button>
        )}
        <button
          id="session-sidebar-refresh-btn"
          type="button"
          className={`session-sidebar__refresh${catalogRefreshing ? ' session-sidebar__refresh--spinning' : ''}`}
          title="Refresh sessions"
          aria-label="Refresh sessions"
          onClick={() => { void requestCatalogLoad({ waitForReady: false, userInitiated: true }); }}
        >
          ⟳
        </button>
      </div>

      <ul className="session-sidebar__list">
        {catalogRefreshing && mergedSessions.length === 0 && catalogLoadShown && (
          <>
            {[0, 1, 2, 3, 4].map((index) => (
              <li key={index} className="session-sidebar__catalog-skeleton" aria-hidden="true">
                <div className="session-sidebar__row-main">
                  <span className="session-sidebar__catalog-skeleton-badge" />
                  <span className="session-sidebar__catalog-skeleton-line" />
                </div>
              </li>
            ))}
          </>
        )}
        {treeItems.length === 0 && !catalogRefreshing && !(firstLoadFailed && mergedSessions.length === 0) && (
          <li className="session-sidebar__empty">
            {mergedSessions.length === 0
              ? 'No saved sessions.'
              : searchQuery.trim() !== ''
                ? 'No sessions match your search.'
                : 'No sessions visible. Search to include hidden sessions.'}
          </li>
        )}
        {treeItems.map((item) => {
          if (item.kind === 'group') {
            const collapsed = collapsedGroups.has(item.node.key);
            return (
              <li
                key={item.node.key}
                className="session-sidebar__group"
                data-group-kind={item.node.kind}
                data-subgroup-kind={item.node.subgroupKind}
              >
                <button
                  type="button"
                  className="session-sidebar__group-head"
                  data-group-kind={item.node.kind}
                  data-subgroup-kind={item.node.subgroupKind}
                  data-provider={item.node.provider}
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
          const rowKey = sessionRowKey(row);
          const isActive = activeRowKey !== null && rowKey === activeRowKey;
          const isSwitching = switchingRowKey === rowKey;
          const hasMenu = hasSessionContextMenu(row);
          const menuOpen = menu != null && sessionRowKey(menu.row) === rowKey;


          return (
            <li
              key={rowKey}
              className={`${isActive ? 'session-sidebar__row session-sidebar__row--active' : 'session-sidebar__row'}${isSwitching ? ' session-sidebar__row--switching' : ''}`}
              data-switching={isSwitching ? 'true' : undefined}
              data-loaded={row.loaded === true ? 'true' : undefined}
              style={item.depth > 0 ? { paddingLeft: item.depth * 14 } : undefined}
            >
              <div className="session-sidebar__row-main">
              <button
                type="button"
                className="session-sidebar__switch"
                data-session-id={row.sessionId}
                data-session-source={row.source || 'rpi'}
                title={isActive ? `Current session: ${sessionTitle(row)}${row.loaded === true ? ' (loaded)' : ''}` : isSwitching ? `Switching to ${sessionTitle(row)}` : row.loaded === true ? `Switch to ${sessionTitle(row)} (loaded)` : `Switch to ${sessionTitle(row)}`}
                aria-label={isActive ? `Current session: ${sessionTitle(row)}${row.loaded === true ? ' (loaded)' : ''}` : isSwitching ? `Switching to ${sessionTitle(row)}` : row.loaded === true ? `Switch to ${sessionTitle(row)} (loaded)` : `Switch to ${sessionTitle(row)}`}
                aria-current={isActive ? 'true' : undefined}
                aria-busy={isSwitching ? true : undefined}
                aria-haspopup={hasMenu ? 'menu' : undefined}
                aria-expanded={hasMenu ? menuOpen : undefined}
                disabled={busy}
                onContextMenu={(event) => {
                  if (!hasMenu) return;
                  event.preventDefault();
                  openMenu(row, event.clientX, event.clientY, event.currentTarget);
                }}
                onKeyDown={(event) => {
                  if (!hasMenu) return;
                  if (event.key === 'F10' && event.shiftKey) {
                    event.preventDefault();
                    const rect = event.currentTarget.getBoundingClientRect();
                    openMenu(row, rect.left, rect.bottom, event.currentTarget);
                  }
                }}
                onPointerDown={(event) => {
                  if (!hasMenu) return;
                  if (event.pointerType !== 'touch') return;
                  clearLongPress();
                  const pointerId = event.pointerId;
                  const x = event.clientX;
                  const y = event.clientY;
                  const target = event.currentTarget;
                  longPressRef.current = {
                    pointerId,
                    x,
                    y,
                    timer: window.setTimeout(() => {
                      if (!longPressRef.current || longPressRef.current.pointerId !== pointerId) return;
                      longPressRef.current = null;
                      if (openMenu(row, x, y, target)) suppressClickRef.current = true;
                    }, SESSION_MENU_LONG_PRESS_MS),
                  };
                }}
                onPointerUp={clearLongPress}
                onPointerCancel={clearLongPress}
                onPointerMove={(event) => {
                  const pending = longPressRef.current;
                  if (!pending || event.pointerId !== pending.pointerId) return;
                  if (Math.abs(event.clientX - pending.x) > 10 || Math.abs(event.clientY - pending.y) > 10) {
                    clearLongPress();
                  }
                }}
                onClick={async (event) => {
                  if (suppressClickRef.current) {
                    event.preventDefault();
                    suppressClickRef.current = false;
                    return;
                  }
                  if (isActive) return;
                  // Fire-and-forget: a pending switch never blocks further
                  // clicks; the newest click replaces the pending target.
                  void runSwitch(row, rowKey).then((switched) => {
                    if (switched) onSwitchComplete();
                  });
                }}
              >
                <span className="session-sidebar__name">
                  {sessionTitle(row)}
                </span>
                {(isSwitching || sessionRowMeta(row)) ? (
                  <span className="session-sidebar__meta">
                    {isSwitching ? 'Switching…' : sessionRowMeta(row)}
                  </span>
                ) : null}
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
                    aria-hidden="true"
                  />
                )}
                {row.status === 'already_imported' && (
                  <span
                    className="session-sidebar__imported"
                    data-imported-for={row.sessionId}
                    title="imported into rpi — click opens the native copy"
                  >
                    imported
                  </span>
                )}
              </button>
              </div>
            </li>
          );
        })}
      </ul>

      {menu && createPortal(
        <div
          ref={menuRef}
          className={`session-sidebar__menu${menu.mode === 'move' ? ' session-sidebar__menu--move' : ''}`}
          role={menu.mode === 'move' ? 'dialog' : 'menu'}
          aria-label={menu.mode === 'move' ? 'Move session' : 'Session actions'}
          style={{ left: menuPos.left, top: menuPos.top }}
          onKeyDown={onMenuKeyDown}
          onContextMenu={(event) => event.preventDefault()}
        >
          {menu.mode === 'rename' ? (
            <input
              ref={renameInputRef}
              className="session-sidebar__menu-rename"
              type="text"
              role="menuitem"
              aria-label="Rename session"
              value={menu.renameValue}
              onChange={(event) => setMenu((current) => current ? { ...current, renameValue: event.target.value } : current)}
              onKeyDown={(event) => {
                if (event.key === 'Enter') {
                  event.preventDefault();
                  event.stopPropagation();
                  void commitRename();
                } else if (event.key === 'Escape') {
                  event.preventDefault();
                  event.stopPropagation();
                  setMenu((current) => current ? { ...current, mode: 'root', focus: 0 } : current);
                } else if (event.key === 'Tab') {
                  event.preventDefault();
                  event.stopPropagation();
                  closeMenu();
                }
              }}
            />
          ) : menu.mode === 'move' ? (
            /* PathAutocomplete owns Enter/Tab/Escape while its suggestion
               menu is open (Enter/Tab accept, Escape closes the menu); the
               wrapper below handles Escape cancel and Tab dismiss only when
               the combobox did not consume the key, and Enter submission
               flows exclusively through onEnter. */
            <span
              className="session-sidebar__menu-move"
              onKeyDown={(event) => {
                if (event.defaultPrevented) return;
                if (event.key === 'Escape') {
                  event.preventDefault();
                  event.stopPropagation();
                  closeMenu();
                } else if (event.key === 'Tab') {
                  event.preventDefault();
                  event.stopPropagation();
                  closeMenu();
                }
              }}
            >
              <PathAutocomplete
                id="session-move-cwd-input"
                value={menu.moveValue}
                onChange={(next) => setMenu((current) => current ? { ...current, moveValue: next } : current)}
                sendComplete={sendMovePathComplete}
                kind="directory"
                placeholder="Target directory (absolute)…"
                ariaLabel="Move session target directory"
                onEnter={() => { void commitMove(menuRef.current?.querySelector<HTMLInputElement>('#session-move-cwd-input')?.value); }}
              />
              <div className="session-sidebar__menu-move-actions">
                <button
                  id="session-move-cancel-btn"
                  type="button"
                  onClick={closeMenu}
                >
                  Cancel
                </button>
                <button
                  id="session-move-submit-btn"
                  type="button"
                  disabled={busy || switchingRef.current !== null || streamingBySessionId?.[menu.row.sessionId] === true || menu.moveValue.trim() === ''}
                  onClick={() => { void commitMove(menuRef.current?.querySelector<HTMLInputElement>('#session-move-cwd-input')?.value); }}
                >
                  Move
                </button>
              </div>
            </span>
          ) : menu.mode === 'fork-from' ? (
            <div
              ref={forkScrollRef}
              className="session-sidebar__fork-scroll"
              role="none"
              onScroll={onForkFromScroll}
            >
              {menu.forkMessages.map((message, index) => (
                <button
                  key={message.entryId}
                  type="button"
                  role="menuitem"
                  className={`session-sidebar__menu-item${index === menu.focus ? ' is-focused' : ''}`}
                  tabIndex={index === menu.focus ? 0 : -1}
                  onMouseEnter={() => setMenu((current) => current ? { ...current, focus: index } : current)}
                  onClick={() => void forkFromEntry(message.entryId)}
                >
                  {safeText(message.text.length > 60 ? `${message.text.slice(0, 57)}…` : message.text)}
                </button>
              ))}
              {menu.forkFailedCursor !== undefined && (
                <>
                  <div
                    className="session-sidebar__fork-status"
                    role="status"
                  >
                    {safeText(`Couldn't load older messages (cursor ${menu.forkFailedCursor})`)}
                  </div>
                  <button
                    type="button"
                    role="menuitem"
                    className={`session-sidebar__menu-item${menu.focus === menu.forkMessages.length ? ' is-focused' : ''}`}
                    tabIndex={menu.focus === menu.forkMessages.length ? 0 : -1}
                    aria-label={`Retry loading older messages (cursor ${menu.forkFailedCursor})`}
                    onMouseEnter={() => setMenu((current) => current ? { ...current, focus: current.forkMessages.length } : current)}
                    onClick={retryForkOlderFrom}
                  >
                    Retry
                  </button>
                </>
              )}
            </div>
          ) : (
            contextMenuItems({
              native: isNativeSession(menu.row),
              deleteDisabled: isSessionActionDisabled(
                activeRowKey !== null && sessionRowKey(menu.row) === activeRowKey,
              ) || switchingRef.current !== null,
              confirmingDelete: menu.mode === 'delete-confirm',
              exportDisabled: exporting,
              copyContextDisabled: copying,
              moveDisabled: streamingBySessionId?.[menu.row.sessionId] === true,
            }).map((item, index) => (

              <button
                key={item.id}
                type="button"
                role="menuitem"
                className={`session-sidebar__menu-item${item.danger ? ' session-sidebar__menu-item--danger' : ''}${index === menu.focus ? ' is-focused' : ''}`}
                tabIndex={item.disabled ? -1 : index === menu.focus ? 0 : -1}
                disabled={item.disabled}
                title={item.title}
                onMouseEnter={() => { if (!item.disabled) setMenu((current) => current ? { ...current, focus: index } : current); }}
                onClick={() => { if (!item.disabled) void activateItem(item.id); }}
              >
                {item.label}
              </button>
            ))
          )}
        </div>,
        document.body,
      )}

      <footer className="session-sidebar__foot">
        <button
          id="sidebar-manage-btn"
          type="button"
          className="session-sidebar__secondary-btn"
          title="New Session (opens session management with the cwd input focused)"
          aria-label="New Session"
          onClick={onOpenManage}
        >
          New Session
        </button>
      </footer>
    </aside>
  );
}
